//! 会话标题自动生成（first-prompt LLM，04 §4 `[session_title]`）。
//!
//! 语义对齐上游 `dsh-session-title-first-prompt-llm`（base bundle 的开箱默认）：
//! **全新会话**（无父——分叉永不自动生成）收到首条用户消息后，用生效路由发一次
//! 辅助请求总结该消息生成标题，写进 `titles.json` 侧车；截断派生（rollout::summarize）
//! 降级为 fallback——生成失败、超时、输入超限都保持 fallback，不自动重试；生成期间
//! 用户手改（`set_title`）优先，生成结果不覆盖。
//!
//! 辅助请求**不进入会话历史**、不加主回合的 token 与延迟（fire-and-forget）；
//! 日志只记结果不记内容（首条消息是用户内容，标题请求与主请求同一条脱敏纪律）。

use std::path::Path;

use futures::StreamExt;
use tauri::Manager;

use crate::config::SessionTitleConfig;
use crate::model_client::{self, HistoryItem, ModelStreamEvent, provider::ProviderConfig, Role};

// ============================== 纯函数（提示 / 框架 / 解析） ==============================

/// 辅助请求的系统指令（对齐上游 first-prompt-llm 的 `systemPrompt`）。
pub fn system_prompt(config: &SessionTitleConfig) -> String {
    [
        "Create a concise title for an AI coding-assistant session from the supplied human messages.",
        "Return only the title on one line, **in plain text of natural language**, with no quotes, prefix, explanation, Markdown, XML, or terminal control codes. No code is allowed.",
        "Use the language of the messages.",
        format!(
            "Aim for about {} words in non-CJK languages or {} CJK characters.",
            config.target_words, config.target_cjk_characters
        )
        .as_str(),
        "If the messages give little to name, still return a short best-effort title, such as Greeting, instead of explaining.",
    ]
    .join("\n")
}

/// 清洗控制字符并归一空白（对齐上游 `cleanTitleText`）：去 OSC / CSI / 其余 ESC
/// 序列、C0/C1 控制字符、双向文本控制符，空白折叠成单空格。标题是界面元数据，
/// 终端转义序列混进来会直接污染侧栏渲染。
pub fn normalize_title_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.peek() {
                // CSI：ESC [ … 直到 0x40–0x7E 区间的终止字节
                Some('[') => {
                    chars.next();
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if ('\x40'..='\x7e').contains(&n) {
                            break;
                        }
                    }
                }
                // OSC：ESC ] … 直到 BEL 或 ST（ESC \）
                Some(']') => {
                    chars.next();
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if n == '\x07' {
                            break;
                        }
                        if n == '\x1b' {
                            chars.next();
                            break;
                        }
                    }
                }
                // 其余两字符转义序列：吞掉随后的一个字符
                _ => {
                    chars.next();
                }
            },
            c if c.is_control() && !c.is_whitespace() => {}
            '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => {}
            c => out.push(c),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 整行被同一对 `*`/`**` 包裹时剥掉标记：模型无视"单行无 Markdown"指令时的常见
/// 形态；内部还含标记的行不动（那可能是字面星号）。
fn strip_whole_line_emphasis(line: &str) -> String {
    for marker in ["**", "*"] {
        if let Some(inner) = line.strip_prefix(marker) {
            if let Some(stripped) = inner.strip_suffix(marker) {
                if !stripped.is_empty() && !stripped.contains(marker) {
                    return stripped.to_string();
                }
            }
        }
    }
    line.to_string()
}

/// 从模型输出取标题：首个非空行（模型把注释写在标题后面时只取标题），归一化后
/// 为空即拒绝——空标题会让侧栏行看起来"会话没有内容"。
pub fn title_from_output(text: &str) -> Result<String, String> {
    let line = text
        .split(['\r', '\n'])
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let title = normalize_title_text(&strip_whole_line_emphasis(line));
    if title.is_empty() {
        return Err("模型输出没有可用的标题行".into());
    }
    Ok(title)
}

/// 消息 JSON 框架：用户文本作为 JSON 字符串值进结构——文本里出现任何引号、
/// 大括号、换行都拆不掉框架（对齐上游 `frameMessages` 的防注入姿态）。
pub fn frame_messages(text: &str) -> String {
    serde_json::json!({ "messages": [{ "text": text }] }).to_string()
}

/// 资格判定（first-prompt 节奏）：无父（分叉永不自动生成，即使分叉源是空会话）
/// 且历史里还没有任何用户消息（续跑、崩溃恢复都不重新生成）。
pub fn eligible(parent_id: Option<&str>, history: &[HistoryItem]) -> bool {
    parent_id.is_none()
        && !history
            .iter()
            .any(|item| matches!(item, HistoryItem::Message { role: Role::User, .. }))
}

/// 生成期间用户改了名（`titles.json` 已有该会话条目）就不覆盖——用户手改优先。
pub fn user_took_over(dir: &Path, agent_id: &str) -> bool {
    crate::rollout::read_titles(dir).contains_key(agent_id)
}

// ============================== 辅助请求 ==============================

fn title_history(first_message: &str) -> Vec<HistoryItem> {
    vec![HistoryItem::Message {
        role: Role::User,
        text: first_message.to_string(),
        reasoning: None,
    }]
}

/// 执行一次标题生成：框架后的输入超限**拒绝**（截断会让标题基于被切开的文本编造），
/// 流式消费到收口，端到端超时掐断（迟到的成功不采纳）。返回的标题已过
/// [`title_from_output`] 归一化，非空。
async fn generate_title(
    provider: &ProviderConfig,
    api_key: &str,
    first_message: &str,
    config: &SessionTitleConfig,
) -> Result<String, String> {
    let input = frame_messages(first_message);
    if input.len() as u64 > config.max_input_bytes {
        return Err(format!(
            "标题输入 {} 字节超过上限 {}（不截断，保持 fallback）",
            input.len(),
            config.max_input_bytes
        ));
    }
    let system = system_prompt(config);
    let history = title_history(first_message);
    let mut body = if provider.is_responses() {
        model_client::build_responses_request(provider, &history, &[], &system)
    } else {
        model_client::build_chat_request(provider, &history, &[], &system)
    };
    // 输出上限是防失控的兜底；两个协议族的字段名不同，注入进各自的请求体
    if provider.is_responses() {
        body["max_output_tokens"] = serde_json::json!(config.max_output_tokens);
    } else {
        body["max_tokens"] = serde_json::json!(config.max_output_tokens);
    }
    let mut stream = model_client::stream_body(provider, api_key, body).await?;
    let mut text = String::new();
    let consumed = tokio::time::timeout(
        std::time::Duration::from_millis(config.timeout_ms),
        async {
            while let Some(event) = stream.next().await {
                match event {
                    ModelStreamEvent::OutputTextDelta { delta } => text.push_str(&delta),
                    ModelStreamEvent::Failed { message } => {
                        return Err(format!("标题流失败: {message}"))
                    }
                    ModelStreamEvent::Completed { .. } => break,
                    _ => {} // 思考增量与结构化项不进标题
                }
            }
            Ok::<(), String>(())
        },
    )
    .await;
    match consumed {
        Err(_) => Err(format!("标题生成超过 {}ms 超时（保持 fallback）", config.timeout_ms)),
        Ok(Err(e)) => Err(e),
        Ok(Ok(())) => title_from_output(&text),
    }
}

/// 入口：前置不满足就静默返回（关开关、没密钥都不是错误——fallback 派生标题一直
/// 在）；满足则派生后台任务，成功写侧车、失败留日志。绝不阻塞主回合。
pub fn spawn_generation(app: tauri::AppHandle, agent_id: String, first_message: String) {
    let state = app.state::<crate::app_state::AppState>();
    let cfg = state.config();
    if !cfg.session_title.enabled {
        return;
    }
    let active = cfg.effective_model();
    // 标题辅助请求不启用推理档：档位属于主回合的采样质量选择，辅助请求要的是
    // 快与省，带上档位只会拖慢标题生成并多烧 token。自定义请求头照带（部署头
    // 是端点级的，比如企业代理签名——少一个头就可能打不进去）。
    let provider = ProviderConfig {
        provider: active.provider.clone(),
        base_url: active.base_url.clone(),
        model: active.model.clone(),
        reasoning_effort: None,
        headers: active.headers.clone(),
    };
    let api_key = match crate::config::SecretStore::new(&state.config_root()).load_key(&active.secret_name) {
        Ok(Some(key)) => key,
        _ => return,
    };
    let title_config = cfg.session_title.clone();
    let dir = state.data_root().join(crate::rollout::ROLLOUT_DIR);
    tauri::async_runtime::spawn(async move {
        match generate_title(&provider, &api_key, &first_message, &title_config).await {
            Ok(title) => {
                if user_took_over(&dir, &agent_id) {
                    return;
                }
                if let Err(e) = crate::rollout::set_title(&dir, &agent_id, &title) {
                    tracing::warn!("标题写入失败（fallback 保持）: {e:?}");
                }
            }
            Err(e) => tracing::debug!("标题生成未完成，保持派生标题: {e}"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rollout::{read_titles, set_title, TITLES_FILE};

    fn temp_dir(name: &str) -> crate::test_support::TestTempDir {
        crate::test_support::TestTempDir::new(&format!("title-{name}"))
    }

    #[test]
    fn system_prompt_carries_targets_and_language_rule() {
        let cfg = SessionTitleConfig::default();
        let prompt = system_prompt(&cfg);
        assert!(prompt.contains("about 5 words"), "{prompt}");
        assert!(prompt.contains("10 CJK characters"), "{prompt}");
        assert!(prompt.contains("Use the language of the messages"), "{prompt}");
        assert!(prompt.contains("No code is allowed"), "{prompt}");
    }

    /// 控制字符 / 终端转义序列 / 双向控制符不得进标题；空白折叠成单空格。
    #[test]
    fn normalize_strips_escapes_and_control_chars() {
        assert_eq!(normalize_title_text("把端口\u{1b}[31m改成\u{1b}[0m 9090"), "把端口改成 9090");
        assert_eq!(normalize_title_text("\u{1b}]0;title\x07读文件"), "读文件");
        assert_eq!(normalize_title_text("a\u{202e}b\u{2066}c"), "abc");
        assert_eq!(normalize_title_text("  多   个\t空格\n混合  "), "多 个 空格 混合");
    }

    #[test]
    fn title_from_output_takes_first_nonempty_line_and_strips_emphasis() {
        assert_eq!(title_from_output("把端口改到 9090").unwrap(), "把端口改到 9090");
        assert_eq!(title_from_output("\n\n**端口调整**\n注释在后面").unwrap(), "端口调整");
        assert_eq!(title_from_output("*单星包裹*").unwrap(), "单星包裹");
        // 内部还含标记的行不剥——那可能是字面星号
        assert_eq!(title_from_output("*a*b*").unwrap(), "*a*b*");
        assert!(title_from_output("   \n  \n").is_err(), "全空输出必须拒绝而不是交空标题");
    }

    /// 用户文本作为 JSON 值进框架：内容里出现引号、大括号、换行都拆不掉结构。
    #[test]
    fn frame_messages_survives_hostile_text() {
        let hostile = "他说 \"}}\n{break} 然后走了";
        let framed = frame_messages(hostile);
        let parsed: serde_json::Value = serde_json::from_str(&framed).expect("敌意文本不得破坏 JSON 框架");
        assert_eq!(parsed["messages"][0]["text"], hostile);
    }

    #[test]
    fn eligible_only_for_fresh_nonfork_sessions() {
        let fresh: Vec<HistoryItem> = vec![];
        assert!(eligible(None, &fresh), "新会话无历史 = 首次");
        let with_user = vec![HistoryItem::Message {
            role: Role::User,
            text: "上一条".into(),
            reasoning: None,
        }];
        assert!(!eligible(None, &with_user), "已有用户消息 = 续跑，不重新生成");
        assert!(!eligible(Some("parent-1"), &fresh), "分叉永不自动生成，即使源是空会话");
        // assistant 开头的档案（理论上不存在）不算用户消息
        let assistant_only = vec![HistoryItem::Message {
            role: Role::Assistant,
            text: "开场".into(),
            reasoning: None,
        }];
        assert!(eligible(None, &assistant_only));
    }

    /// 生成期间用户手改：titles.json 有了条目就不再覆盖；没有才写。
    #[test]
    fn user_title_takes_precedence_over_generation() {
        let dir = temp_dir("guard");
        assert!(!user_took_over(&dir, "a"), "没人改名时不拦");
        // 侧车写不进没档案的会话——先放一个最小档案（首行 meta 已由 set_title 的
        // 存在性检查覆盖，这里只关心 read_titles 的守卫语义，直接写侧车文件）
        std::fs::write(dir.join(TITLES_FILE), r#"{"a": "用户起的名"}"#).unwrap();
        assert!(user_took_over(&dir, "a"), "生成结果不得覆盖用户手改");
    }

    /// 超长输入在派发前拒绝（不截断——上游语义），错误里带上限值。
    #[tokio::test]
    async fn oversized_input_rejects_before_dispatch() {
        let cfg = SessionTitleConfig { max_input_bytes: 64, ..SessionTitleConfig::default() };
        let provider = ProviderConfig {
            provider: "openai-chat".into(),
            base_url: "http://127.0.0.1:1/v1".into(),
            model: "stub".into(),
            reasoning_effort: None,
            headers: Default::default(),
        };
        let err = generate_title(&provider, "k", "这是一条很长很长很长很长的用户消息，用来超出 64 字节的框架上限……", &cfg)
            .await
            .unwrap_err();
        assert!(err.contains("64"), "{err}");
    }

    /// 端点连不上 = 生成失败，错误可日志（内容无关）；set_title 的写入路径由
    /// rollout 自己的测试覆盖，这里只锁"失败必须返回 Err 而不是交空标题"。
    #[tokio::test]
    async fn unreachable_endpoint_is_an_error_not_an_empty_title() {
        let cfg = SessionTitleConfig { timeout_ms: 300, ..SessionTitleConfig::default() };
        let provider = ProviderConfig {
            provider: "openai-chat".into(),
            base_url: "http://127.0.0.1:1/v1".into(),
            model: "stub".into(),
            reasoning_effort: None,
            headers: Default::default(),
        };
        let result = generate_title(&provider, "k", "你好", &cfg).await;
        // 契约只有一条：端点不可达必须整体失败（fallback 保持），绝不交出空标题
        assert!(result.is_err());
    }

    /// 写入走 rollout::set_title 的既有校验（长度上限等）；守卫在写入前已经把
    /// 用户手改挡掉，这里锁"成功路径真的落了盘"。
    #[tokio::test]
    async fn generated_title_lands_in_titles_sidecar() {
        let dir = temp_dir("sidecar");
        let id = "sess-title-e2e";
        std::fs::write(dir.join(format!("{id}.jsonl")), "{\"type\":\"session_meta\"}\n").unwrap();
        // 直接复用生成链路的最末步语义：守卫 + 写入
        assert!(!user_took_over(&dir, id));
        set_title(&dir, id, "桩端点生成的标题").unwrap();
        assert_eq!(read_titles(&dir).get(id).map(String::as_str), Some("桩端点生成的标题"));
    }
}
