// pub 供集成测试（src-tauri/tests/）作为契约面使用
pub mod agent_loop;
pub mod app_state;
pub mod config;
pub mod events;
pub mod model_client;
pub mod sandbox;
pub mod permission;
pub mod rollout;
pub mod tool_router;
pub mod tools;

use std::sync::Arc;

use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager,
};

use agent_loop::ModelBackend;
use app_state::AppState;
use config::AppConfig;

/// IPC 握手（M0 验收）：前端 invoke 携带 nonce，后端经事件通道原样回显。
#[tauri::command]
fn dev_ping(app: tauri::AppHandle, nonce: String) -> Result<(), String> {
    events::emit_pong(&app, &nonce)
}

/// 提交任务，并返回**本次使用的 agent_id**。
///
/// `agent_id` 为 None = 新会话（现场建档）；为 Some = 续写那份档案（SH-033 的
/// 「打开历史会话后继续输入」）。返回值是必需的而不是顺手加的：新会话的 id
/// 只有后端知道（`next_session_id` 在这里生成），前端拿不到它就无从决定
/// 下一次提交该续写谁——会话列表也就永远点不回自己刚建的那条。
#[tauri::command]
fn send_task(app: tauri::AppHandle, task: String, agent_id: Option<String>) -> Result<String, String> {
    if task.trim().is_empty() {
        return Err("任务为空".into());
    }
    let state = app.state::<AppState>();
    match agent_id.filter(|id| !id.trim().is_empty()) {
        Some(id) => {
            continue_session(&app, &state, id.clone(), &task)?;
            Ok(id)
        }
        None => {
            let (agent, cancel, new_id) = build_new_session(&state)?;
            let sink: Arc<dyn events::EventSink> = Arc::new(events::TauriEventSink(app.clone()));
            tauri::async_runtime::spawn(async move {
                let _ = agent.run_task(&task, sink, cancel).await;
                app.state::<AppState>().end_task();
            });
            Ok(new_id)
        }
    }
}

/// 续写一份既有档案——`resume_session` 与 `send_task(Some(id))` 的共用主体。
///
/// 恢复的三条硬要求都在这里成立：档案按 agent_id 定位、续跑**追加**到同一份档案
/// （`open_existing`，不重新建档）、已落盘条数作为写入起点（`persisted`，避免把旧历史
/// 重写一遍）。历史合法性由 `rollout::resume` 的重放规则保证；这里不再"修"历史——
/// 静默补配对会掩盖档案损坏，而档案损坏必须由上层显式处理。
fn continue_session(
    app: &tauri::AppHandle,
    state: &AppState,
    agent_id: String,
    task: &str,
) -> Result<(), String> {
    let path = state
        .data_root()
        .join(rollout::ROLLOUT_DIR)
        .join(format!("{agent_id}.jsonl"));
    let resumed = rollout::resume::resume(&path).map_err(|e| format!("恢复会话失败: {e}"))?;
    let persisted = resumed.history.len();
    let mut history = resumed.history;
    history.push(model_client::HistoryItem::Message {
        role: model_client::Role::User,
        text: task.to_string(),
        reasoning: None,
    });

    let writer = rollout::RolloutWriter::open_existing(&resumed.path)
        .map_err(|e| format!("档案续写打不开: {e}"))?;
    let (agent, cancel) = build_agent_with(state, Some(Arc::new(writer)))?;
    let sink: Arc<dyn events::EventSink> = Arc::new(events::TauriEventSink(app.clone()));
    // spawn 的 future 必须是 'static：借用的 app 只能换成 owned 句柄再 move 进去。
    let app_handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let _ = agent.run_with_history(history, persisted, sink, cancel).await;
        app_handle.state::<AppState>().end_task();
    });
    Ok(())
}

/// 重启后恢复会话并续跑新任务（SH-031）。
#[tauri::command]
fn resume_session(app: tauri::AppHandle, agent_id: String, task: String) -> Result<(), String> {
    if task.trim().is_empty() {
        return Err("任务为空".into());
    }
    let state = app.state::<AppState>();
    continue_session(&app, &state, agent_id, &task)
}

/// 会话列表（SH-033）：档案目录里的摘要，按更新时间倒序。
///
/// 返回空列表而不是错误：档案目录不存在是**首次运行的正常状态**，
/// 把它当错误会让前端在干净安装上必须写一条特判分支，而那条分支什么也修不了。
#[tauri::command]
fn list_sessions(app: tauri::AppHandle) -> Vec<rollout::SessionSummary> {
    let state = app.state::<AppState>();
    rollout::list(&state.data_root().join(rollout::ROLLOUT_DIR))
}

/// 打开一个会话所需的历史条目。
///
/// **为什么不复用事件流重放**：`EventMsg` 里没有用户消息事件——用户消息历来是前端
/// `submitUserTask` 本地生成的。若为了重放新增一个事件变体，就得动受保护的 `events/`
/// 并牵动 FF-2（EventMsg tag ↔ types.ts 的机械绑定）。而重放的语义本来就是
/// 「一次性数据加载」而不是「流」，用 command 返回值更贴切，且对协议零侵入。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEntry {
    Message {
        role: String,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
    },
    ToolCall {
        call_id: String,
        tool: String,
        arguments: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },
}

/// 档案历史 → 展示条目。
///
/// 工具输出在档案里排在**同轮全部调用之后**（"先全部 call、再全部 output"，SH-031 的
/// `nearest_safe_cut` 正是靠这个形状才敢只在配对边界截断）。展示要按调用顺序把输出带上，
/// 所以先建一遍 `call_id → output` 索引，再按原序输出，`FunctionCallOutput` 本身不再成条。
fn project_entries(items: &[rollout::RolloutItem]) -> Vec<SessionEntry> {
    let mut outputs: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for item in items {
        if let rollout::RolloutItem::ResponseItem {
            item: model_client::HistoryItem::FunctionCallOutput { call_id, output },
        } = item
        {
            outputs.insert(call_id.clone(), output.clone());
        }
    }

    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match item {
            rollout::RolloutItem::ResponseItem {
                item: model_client::HistoryItem::Message { role, text, reasoning },
            } => out.push(SessionEntry::Message {
                role: match role {
                    model_client::Role::User => "user",
                    model_client::Role::Assistant => "assistant",
                    model_client::Role::System => "system",
                }
                .to_string(),
                text: text.clone(),
                reasoning: reasoning.clone(),
            }),
            rollout::RolloutItem::ResponseItem {
                item: model_client::HistoryItem::FunctionCall { name, arguments, call_id },
            } => out.push(SessionEntry::ToolCall {
                call_id: call_id.clone(),
                tool: name.clone(),
                arguments: arguments.clone(),
                output: outputs.get(call_id).cloned(),
            }),
            // 已并进对应的 ToolCall，不单独成条。
            rollout::RolloutItem::ResponseItem {
                item: model_client::HistoryItem::FunctionCallOutput { .. },
            } => {}
            // 压缩锚点在 resume 时的语义就是"用 summary 取代那段区间"，
            // 展示也必须一致，否则用户会看到区间原文而模型看到的是摘要——两份真相。
            rollout::RolloutItem::Compacted { summary, .. } => out.push(SessionEntry::Message {
                role: "assistant".to_string(),
                text: summary.clone(),
                reasoning: None,
            }),
            // 首行 meta 不是历史；子代理引用要等 SH-056 的泳道 UI 才有展示形态。
            rollout::RolloutItem::SessionMeta { .. } | rollout::RolloutItem::SubagentRef { .. } => {}
        }
    }
    out
}

/// 读回一个会话的历史条目，供前端渲染（SH-033 的"打开会话"）。
///
/// 走 `rollout::read` 的严格口径：档案中间坏行必须报错，绝不能给前端一份"看起来完整、
/// 实际缺尾"的历史——那正是 SH-030 设计选择 2 要防的事。
#[tauri::command]
fn load_session(app: tauri::AppHandle, agent_id: String) -> Result<Vec<SessionEntry>, String> {
    let state = app.state::<AppState>();
    let path = state
        .data_root()
        .join(rollout::ROLLOUT_DIR)
        .join(format!("{agent_id}.jsonl"));
    let archive = rollout::read(&path).map_err(|e| format!("档案读不回: {e}"))?;
    Ok(project_entries(&archive.items))
}

/// 从既有会话分支出一个新档案，返回新 agent_id（SH-033 的 fork 入口）。
///
/// 截断点由 `nearest_safe_cut` 兜底：前端要 N 条，实际可能给得更少（从中间切会留下
/// 有 call 无 output 的尾巴，续跑第一次请求就 400，见 SH-082）。父档案只读。
#[tauri::command]
fn fork_session(app: tauri::AppHandle, agent_id: String, keep_items: usize) -> Result<String, String> {
    let state = app.state::<AppState>();
    let dir = state.data_root().join(rollout::ROLLOUT_DIR);
    let src = dir.join(format!("{agent_id}.jsonl"));
    let new_id = state.next_session_id();
    let forked = rollout::resume::fork(&src, &dir, &new_id, keep_items)
        .map_err(|e| format!("分支失败: {e}"))?;
    Ok(forked.new_agent_id)
}

/// 组装一次新会话所需的 AgentLoop + 取消令牌 + 它的 agent_id（现场建档）。
/// 建档失败就直接报错给前端——静默不落盘会让用户以为"能 resume"，实际不能。
fn build_new_session(
    state: &AppState,
) -> Result<(agent_loop::AgentLoop, tokio_util::sync::CancellationToken, String), String> {
    let (writer, agent_id) = new_session_writer(state)?;
    let (agent, cancel) = build_agent_with(state, Some(writer))?;
    Ok((agent, cancel, agent_id))
}

fn new_session_writer(state: &AppState) -> Result<(Arc<rollout::RolloutWriter>, String), String> {
    let cfg = state.config();
    let agent_id = state.next_session_id();
    let meta = rollout::RolloutItem::SessionMeta {
        session_id: agent_id.clone(),
        agent_id: agent_id.clone(),
        parent_id: None,
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or_default(),
        config_snapshot: rollout::ConfigSnapshot {
            provider: cfg.model.provider.clone(),
            base_url: cfg.model.base_url.clone(),
            model: cfg.model.model.clone(),
            sandbox: format!("{:?}", cfg.sandbox.mode),
            approval: format!("{:?}", cfg.approval.mode),
            workspace_root: state.workspace_root().display().to_string(),
        },
    };
    let writer = rollout::RolloutWriter::create(&state.data_root().join(rollout::ROLLOUT_DIR), &meta)
        .map(Arc::new)
        .map_err(|e| format!("会话档案建档失败: {e}"))?;
    Ok((writer, agent_id))
}

fn build_agent_with(
    state: &AppState,
    writer: Option<Arc<rollout::RolloutWriter>>,
) -> Result<(agent_loop::AgentLoop, tokio_util::sync::CancellationToken), String> {
    let cfg = state.config();
    let provider = model_client::provider::ProviderConfig::from_model_config(&cfg.model);
    let secret = config::SecretStore::new(&state.config_root());
    let api_key = secret
        .load_key("openai-responses")?
        .ok_or("未配置 API Key，请先在设置中保存")?;

    let cancel = tokio_util::sync::CancellationToken::new();
    if !state.begin_task(cancel.clone()) {
        return Err("已有任务在运行".into());
    }

    let backend: Arc<dyn ModelBackend> = Arc::new(agent_loop::ResponsesBackend { provider, api_key });
    let mut router = tool_router::ToolRouter::new();
    tools::register_builtin(&mut router, state.plan_state.clone());
    let agent = agent_loop::AgentLoop::from_config(
        backend,
        Arc::new(router),
        state.broker.clone(),
        state.plan_state.clone(),
        &cfg,
        state.workspace_root(),
        writer,
    );
    Ok((agent, cancel))
}

/// 取消当前任务。
#[tauri::command]
fn cancel_task(app: tauri::AppHandle) -> Result<(), String> {
    let _ = app.state::<AppState>().cancel_active();
    Ok(())
}

/// 审批回执（approval card → broker）。
#[tauri::command]
fn approve(app: tauri::AppHandle, request_id: String, decision: String) -> Result<(), String> {
    let decision = match decision.as_str() {
        "allow" => events::ApprovalDecision::Allow,
        "allow_always" => events::ApprovalDecision::AllowAlways,
        "deny" => events::ApprovalDecision::Deny,
        other => return Err(format!("未知裁决: {other}")),
    };
    app.state::<AppState>().broker.resolve(&request_id, decision);
    Ok(())
}

/// 保存 API Key（DPAPI 加密落盘，docs §9.1 secrets\）。
#[tauri::command]
fn store_api_key(app: tauri::AppHandle, provider: String, key: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    config::SecretStore::new(&state.config_root()).store_key(&provider, &key)
}

/// 查询是否已保存某 provider 的 Key。
#[tauri::command]
fn has_api_key(app: tauri::AppHandle, provider: String) -> Result<bool, String> {
    let state = app.state::<AppState>();
    config::SecretStore::new(&state.config_root()).load_key(&provider).map(|k| k.is_some())
}

/// 读取设置（非敏感字段 + Key 是否已配置）。
#[tauri::command]
fn get_settings(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    let state = app.state::<AppState>();
    let cfg = state.config();
    let has_key = config::SecretStore::new(&state.config_root())
        .load_key("openai-responses")
        .map(|k| k.is_some())?;
    Ok(serde_json::json!({
        "baseUrl": cfg.model.base_url,
        "model": cfg.model.model,
        "maxTurns": cfg.model.max_turns,
        "workspaceRoot": state.workspace_root().to_string_lossy(),
        "hasApiKey": has_key,
        // 安全面两档的回显。序列化直接走 serde（枚举本身是 snake_case），
        // 不在这里手写字符串——手写就会与 config.toml 里的写法漂移成两套名字。
        "sandboxMode": serde_json::to_value(cfg.sandbox.mode).map_err(|e| e.to_string())?,
        "approvalMode": serde_json::to_value(cfg.approval.mode).map_err(|e| e.to_string())?,
    }))
}

/// 报错时列给用户的合法档位名。它必须与 `config.rs` 的枚举同源——
/// `safety_mode_names_match_enums` 用穷举后的序列化结果逐字比对，加一档忘改这里会直接红。
const SANDBOX_MODE_NAMES: &str = "read_only / workspace_write / danger_full_access";
const APPROVAL_MODE_NAMES: &str = "ask / auto_read_only";

/// 保存设置：模型端点/工作区入 config.toml，API Key（可选填）DPAPI 加密落盘。
#[tauri::command]
fn save_settings(
    app: tauri::AppHandle,
    base_url: String,
    model: String,
    workspace_root: String,
    api_key: Option<String>,
    sandbox_mode: Option<String>,
    approval_mode: Option<String>,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut cfg: AppConfig = state.config();
    cfg.model.base_url = base_url.trim_end_matches('/').to_string();
    cfg.model.model = model;
    cfg.workspace.root = workspace_root;
    // 安全面两档：None = 不改，所以旧调用形态（不传这两个键）行为完全不变。
    // 非法值必须报错而不是静默保留旧值——"点了保存却没生效、界面还显示旧档位"是安全
    // 设置里最坏的一类失败：用户会以为自己已经收紧（或放宽）了，实际什么都没变。
    // 解析复用 serde 而不是手写 match，这样名字永远与 config.toml 里的写法同源。
    if let Some(mode) = sandbox_mode {
        cfg.sandbox.mode = serde_json::from_value(serde_json::Value::String(mode))
            .map_err(|_| format!("沙箱模式取值非法，可用：{}", SANDBOX_MODE_NAMES))?;
    }
    if let Some(mode) = approval_mode {
        cfg.approval.mode = serde_json::from_value(serde_json::Value::String(mode))
            .map_err(|_| format!("审批模式取值非法，可用：{}", APPROVAL_MODE_NAMES))?;
    }
    state.update_config(cfg)?;
    if let Some(key) = api_key {
        let key = key.trim().to_string();
        if !key.is_empty() {
            config::SecretStore::new(&state.config_root()).store_key("openai-responses", &key)?;
        }
    }
    Ok(())
}

/// 从托盘恢复主窗口：取消最小化 + 显示 + 聚焦。
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

pub fn run() {
    // 数据目录解析结果写入日志第一行（docs §9.3 的「解析结果写入日志第一行」即指此条）
    let dirs = config::resolve_dirs_from_env();
    // 日志初始化必须早于一切 tracing:: 事件；guard 存活期 = 进程存活期
    let _log_guard = match config::init_logging(&dirs.data_root) {
        Ok(guard) => Some(guard),
        Err(e) => {
            // 日志起不来不能拦着应用启动（fail-open 但脱敏表照常工作），兜底打印说明原因
            eprintln!("[SHANIU] 日志初始化失败（本次会话无文件日志）: {e}");
            None
        }
    };
    tracing::info!("config_root: {}", dirs.config_root.display());
    tracing::info!("data_root:   {}", dirs.data_root.display());

    let (app_config, config_path) =
        AppConfig::load_or_create(&dirs.config_root).unwrap_or_else(|e| {
            tracing::warn!("配置加载失败（fail-closed，使用临时默认配置）: {e}");
            (AppConfig::default(), dirs.config_root.join(config::CONFIG_FILE))
        });

    tauri::Builder::default()
        .manage(AppState::new(app_config, config_path, dirs.data_root.clone()))
        .invoke_handler(tauri::generate_handler![
            dev_ping,
            send_task,
            resume_session,
            list_sessions,
            load_session,
            fork_session,
            cancel_task,
            approve,
            store_api_key,
            has_api_key,
            get_settings,
            save_settings
        ])
        .on_window_event(|window, event| {
            // 关闭 = 隐藏到系统托盘（不退出）；真正退出只走托盘菜单「退出」
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .setup(|app| {
            // 系统托盘：菜单 = 显示 / 退出；左键单击图标 = 显示主窗口
            let show = MenuItem::with_id(app, "show", "显示", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let tray_menu = Menu::with_items(app, &[&show, &quit])?;
            TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().expect("缺少应用图标").clone())
                .tooltip("SHANIU")
                .menu(&tray_menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "show" => show_main_window(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main_window(tray.app_handle());
                    }
                })
                .build(app)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("SHANIU 启动失败");
}

#[cfg(test)]
mod session_projection_tests {
    use super::{project_entries, SessionEntry, APPROVAL_MODE_NAMES, SANDBOX_MODE_NAMES};
    use crate::config;
    use crate::model_client::{HistoryItem, Role};
    use crate::rollout::{ConfigSnapshot, RolloutItem};

    fn msg(role: Role, text: &str) -> RolloutItem {
        RolloutItem::ResponseItem {
            item: HistoryItem::Message { role, text: text.into(), reasoning: None },
        }
    }

    fn call(name: &str, args: &str, id: &str) -> RolloutItem {
        RolloutItem::ResponseItem {
            item: HistoryItem::FunctionCall { name: name.into(), arguments: args.into(), call_id: id.into() },
        }
    }

    fn out(id: &str, output: &str) -> RolloutItem {
        RolloutItem::ResponseItem {
            item: HistoryItem::FunctionCallOutput { call_id: id.into(), output: output.into() },
        }
    }

    #[test]
    fn tool_output_merges_into_its_call_instead_of_standing_alone() {
        // 档案形状是"先全部 call、再全部 output"，投影必须按调用顺序把输出带上
        let items = vec![
            msg(Role::User, "跑一下"),
            call("unified_exec", "{\"command\":\"ls\"}", "c1"),
            call("read_file", "{\"path\":\"a.txt\"}", "c2"),
            out("c2", "文件内容"),
            out("c1", "a.txt\nb.txt"),
        ];
        let entries = project_entries(&items);
        assert_eq!(entries.len(), 3, "两条调用 + 一条用户消息，输出不单独成条");
        match &entries[1] {
            SessionEntry::ToolCall { call_id, tool, output, .. } => {
                assert_eq!(call_id, "c1");
                assert_eq!(tool, "unified_exec");
                assert_eq!(output.as_deref(), Some("a.txt\nb.txt"), "输出按 call_id 配对，不是按到达顺序");
            }
            other => panic!("期望 ToolCall，得到 {other:?}"),
        }
        match &entries[2] {
            SessionEntry::ToolCall { call_id, output, .. } => {
                assert_eq!(call_id, "c2");
                assert_eq!(output.as_deref(), Some("文件内容"));
            }
            other => panic!("期望 ToolCall，得到 {other:?}"),
        }
    }

    #[test]
    fn unpaired_call_keeps_output_none() {
        // 取消或审批拒绝的轮次：有 call 无 output，投影不能凭空补一个成功输出
        let items = vec![call("unified_exec", "{}", "c1")];
        match &project_entries(&items)[0] {
            SessionEntry::ToolCall { output, .. } => assert_eq!(output, &None),
            other => panic!("期望 ToolCall，得到 {other:?}"),
        }
    }

    #[test]
    fn compaction_anchor_projects_as_assistant_summary() {
        // resume 时锚点的语义就是"摘要取代区间"，展示必须一致，
        // 否则用户看到区间原文而模型看到的是摘要——两份真相。
        let items = vec![
            msg(Role::User, "早期任务"),
            RolloutItem::Compacted { range_start: 0, range_end: 3, summary: "已压缩的上下文".into() },
        ];
        let entries = project_entries(&items);
        assert_eq!(entries.len(), 2);
        match &entries[1] {
            SessionEntry::Message { role, text, .. } => {
                assert_eq!(role, "assistant");
                assert_eq!(text, "已压缩的上下文");
            }
            other => panic!("期望 Message，得到 {other:?}"),
        }
    }

    #[test]
    fn meta_and_subagent_ref_do_not_become_entries() {
        let items = vec![
            RolloutItem::SessionMeta {
                session_id: "s".into(),
                agent_id: "a".into(),
                parent_id: None,
                created_at: 1,
                config_snapshot: ConfigSnapshot::default(),
            },
            RolloutItem::SubagentRef { child_id: "c".into(), rollout_path: "p".into() },
            msg(Role::Assistant, "正文"),
        ];
        let entries = project_entries(&items);
        assert_eq!(entries.len(), 1, "首行 meta 与子代理引用不是历史；SH-056 的泳道 UI 才有后者的展示形态");
        assert!(matches!(&entries[0], SessionEntry::Message { role, .. } if role == "assistant"));
    }

    #[test]
    fn reasoning_survives_projection_when_present() {
        // SH-083 的 reasoning_content 必须能在打开会话时一起回来，
        // 否则"重开旧会话"和"刚跑完"看到的上下文不一样。
        let items = vec![RolloutItem::ResponseItem {
            item: HistoryItem::Message { role: Role::Assistant, text: "答案".into(), reasoning: Some("思路".into()) },
        }];
        match &project_entries(&items)[0] {
            SessionEntry::Message { reasoning, .. } => assert_eq!(reasoning.as_deref(), Some("思路")),
            other => panic!("期望 Message，得到 {other:?}"),
        }
    }

    /// 档位清单与枚举同源（SH-040）。
    ///
    /// 这里刻意用**序列化结果**而不是手写第二份字符串来比对：如果哪天给枚举加一档
    /// 却忘了更新报错文案里的清单，用户就会照着一份不完整的清单去填，然后撞上
    /// "取值非法"——安全设置的提示本身成了误导源。
    #[test]
    fn safety_mode_names_match_enums() {
        let sandbox_all = [
            config::SandboxMode::ReadOnly,
            config::SandboxMode::WorkspaceWrite,
            config::SandboxMode::DangerFullAccess,
        ];
        let sandbox_joined = sandbox_all
            .iter()
            .map(|m| serde_json::to_value(*m).unwrap().as_str().unwrap().to_string())
            .collect::<Vec<_>>()
            .join(" / ");
        assert_eq!(sandbox_joined, SANDBOX_MODE_NAMES, "沙箱档位清单与枚举漂移了");

        let approval_all = [config::ApprovalMode::Ask, config::ApprovalMode::AutoReadOnly];
        let approval_joined = approval_all
            .iter()
            .map(|m| serde_json::to_value(*m).unwrap().as_str().unwrap().to_string())
            .collect::<Vec<_>>()
            .join(" / ");
        assert_eq!(approval_joined, APPROVAL_MODE_NAMES, "审批档位清单与枚举漂移了");
    }

    /// 两个模式必须真的能 round-trip 回自身：只比对名字的话，serde 属性一改就会
    /// 让"界面显示的值"和"存进 config.toml 的值"变成两套东西。
    #[test]
    fn safety_modes_round_trip_through_wire_names() {
        for name in SANDBOX_MODE_NAMES.split(" / ") {
            let parsed: config::SandboxMode =
                serde_json::from_value(serde_json::Value::String(name.to_string())).unwrap();
            assert_eq!(serde_json::to_value(parsed).unwrap().as_str().unwrap(), name);
        }
        for name in APPROVAL_MODE_NAMES.split(" / ") {
            let parsed: config::ApprovalMode =
                serde_json::from_value(serde_json::Value::String(name.to_string())).unwrap();
            assert_eq!(serde_json::to_value(parsed).unwrap().as_str().unwrap(), name);
        }
        // 非法值必须解析失败——save_settings 的报错分支全靠这个前提
        assert!(serde_json::from_value::<config::SandboxMode>(
            serde_json::Value::String("full_access".into())
        )
        .is_err());
    }
}
