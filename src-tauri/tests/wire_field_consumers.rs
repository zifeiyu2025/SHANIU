//! 字段消费方三态表（缝合机制第二件）。
//!
//! 后端 wire 结构新增一个字段，前端可能根本没人读——历史上 `SessionEntry.images`
//! 就只出现在两份声明中的一份，导出因此丢掉用户图片。这个用例不判断「有没有读者」
//! （那需要语义分析，硬判会产生大量误报），它只强制一件事：
//!
//! **每一个后端字段都必须在下面这张表里显式表态**，三态之一：
//! - `Read`：前端有消费者；
//! - `BackendOnly("理由")`：有意只给后端/审计用，不是漏；
//! - `Gap("待前端补读")`：确认该有人读但当前没有，已登记待办。
//!
//! 因此新增字段而不登记 → 用例红；删改字段而表里还留着 → 用例红。
//! 存量已分类的不吵——这是它不变成噪音源的前提。
//!
//! 口径：只覆盖 `EventMsg`（事件协议）、`AgentEvent`（信封）、`SessionEntry`（档案投影）
//! 三个核心 wire 结构。字段从 Rust 源头解析，不在用例里手抄——手抄的清单会和源头漂移。

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Clone, Copy)]
enum Status {
    /// 前端有消费者。
    Read,
    /// 有意只给后端/审计用；理由必须写清，否则下一个人分不清它与缺口。
    BackendOnly(&'static str),
    /// 应该有读者但当前没有；写明待办，改完后把这一格改成 `Read`。
    Gap(&'static str),
}

struct Rule {
    owner: &'static str,
    /// 结构体名，或枚举里的变体名（`AgentEvent` / `SessionEntry` 用结构体名）。
    variant: &'static str,
    /// 默认登记为 `Read` 的字段，空格分隔。
    fields: &'static str,
    /// 非 `Read` 的例外项。
    overrides: &'static [(&'static str, Status)],
}

const RULES: &[Rule] = &[
    // ── 事件信封 ────────────────────────────────────────────────────────────
    Rule {
        owner: "AgentEvent",
        variant: "AgentEvent",
        fields: "event_id session_id agent_id ts_ms msg",
        // 父子关系由 catalog（subagent_spawned）推导，信封字段只留档给日志/第三方消费者。
        overrides: &[("parent_id", Status::BackendOnly("前端从 catalog 推导父子边，信封字段留档"))],
    },
    // ── 档案投影（`load_session` 返回值） ───────────────────────────────────
    Rule {
        owner: "SessionEntry",
        variant: "Message",
        fields: "role text reasoning at_ms images",
        overrides: &[],
    },
    Rule {
        owner: "SessionEntry",
        variant: "ToolCall",
        fields: "call_id tool arguments output at_ms",
        overrides: &[],
    },
    Rule {
        owner: "SessionEntry",
        variant: "TurnEnd",
        fields: "turn reason at_ms error",
        overrides: &[],
    },
    // ── EventMsg ────────────────────────────────────────────────────────────
    Rule { owner: "EventMsg", variant: "TurnStarted", fields: "turn", overrides: &[] },
    Rule {
        owner: "EventMsg",
        variant: "TurnCompleted",
        fields: "reason error unverified",
        // 后端 turn 是采样轮次，前端一律用 journal.turn（同 model_retry/token_usage）。
        overrides: &[("turn", Status::BackendOnly("后端 turn 是采样轮次；前端用 journal.turn"))],
    },
    Rule {
        owner: "EventMsg",
        variant: "VerificationResult",
        fields: "state commands duration_ms diagnostics",
        overrides: &[
            ("turn", Status::BackendOnly("后端 turn 是采样轮次；前端用 journal.turn")),
            // 前端按四态自建文案：同一句话两侧各写一遍必然漂移，事件里的 summary 只做审计/导出。
            ("summary", Status::BackendOnly("前端自建四态文案；summary 留给审计与导出")),
        ],
    },
    Rule { owner: "EventMsg", variant: "AgentMessageDelta", fields: "delta", overrides: &[] },
    Rule { owner: "EventMsg", variant: "ReasoningDelta", fields: "delta", overrides: &[] },
    Rule { owner: "EventMsg", variant: "ToolCallBegin", fields: "call_id tool args", overrides: &[] },
    Rule {
        owner: "EventMsg",
        variant: "ToolCallOutputDelta",
        fields: "call_id channel delta",
        overrides: &[],
    },
    Rule {
        owner: "EventMsg",
        variant: "ToolCallEnd",
        fields: "call_id exit_code error result",
        overrides: &[],
    },
    Rule {
        owner: "EventMsg",
        variant: "ExecApprovalRequest",
        fields: "request_id tool args risk agent_id task_summary reason call_id batch_scope",
        overrides: &[],
    },
    Rule {
        owner: "EventMsg",
        variant: "ExecApprovalResolved",
        fields: "",
        overrides: &[
            ("request_id", Status::BackendOnly("协议占位：预期后端不产生，前端 default 分支忽略")),
            ("decision", Status::BackendOnly("协议占位：预期后端不产生，前端 default 分支忽略")),
        ],
    },
    Rule {
        owner: "EventMsg",
        variant: "AllowlistHit",
        fields: "tool",
        overrides: &[
            // 决定，不是遗漏：长命令回显会把通知刷屏。
            ("command", Status::BackendOnly("有意不回显：长命令会刷屏，放行依据留在事件日志里")),
            ("rule_id", Status::Gap("待前端补读：通知里看不出被哪条规则放行")),
        ],
    },
    Rule {
        owner: "EventMsg",
        variant: "PlanDelta",
        fields: "",
        overrides: &[("delta", Status::BackendOnly("协议占位：预期后端不产生，前端 default 分支忽略"))],
    },
    Rule {
        owner: "EventMsg",
        variant: "TurnDiff",
        fields: "",
        overrides: &[("files", Status::BackendOnly("协议占位：预期后端不产生，前端 default 分支忽略"))],
    },
    Rule { owner: "EventMsg", variant: "Error", fields: "message", overrides: &[] },
    Rule {
        owner: "EventMsg",
        variant: "ModelRetry",
        fields: "retry max_retries phase delay_ms code message retry_id",
        overrides: &[("turn", Status::BackendOnly("后端 turn 是采样轮次；前端用 journal.turn"))],
    },
    Rule {
        owner: "EventMsg",
        variant: "TokenUsage",
        fields: "input_tokens cached_input_tokens output_tokens",
        overrides: &[("turn", Status::BackendOnly("后端 turn 是采样轮次；前端用 journal.turn"))],
    },
    Rule {
        owner: "EventMsg",
        variant: "SubagentSpawned",
        fields: "child_id task name label created_at_ms",
        overrides: &[],
    },
    Rule { owner: "EventMsg", variant: "SubagentExited", fields: "child_id status summary", overrides: &[] },
    Rule {
        owner: "EventMsg",
        variant: "SubagentMessage",
        fields: "child_id sender_id text rpc_id",
        overrides: &[],
    },
    Rule { owner: "EventMsg", variant: "RolloutDegraded", fields: "message", overrides: &[] },
    Rule {
        owner: "EventMsg",
        variant: "SystemPrompt",
        fields: "text",
        overrides: &[
            ("sha256", Status::BackendOnly("后端去重（recorded_prompt）与 last_prompt_sha 校验用")),
            ("at_ms", Status::BackendOnly("审计时间戳；前端事件 id 用 journal seq")),
            ("turn", Status::BackendOnly("前端用 journal.turn/step，后端 turn 是采样轮次")),
            ("step", Status::BackendOnly("前端用 journal.turn/step，后端 step 是采样步")),
        ],
    },
    Rule {
        owner: "EventMsg",
        variant: "CompactionStarted",
        fields: "compaction_id trigger",
        overrides: &[
            ("range_start", Status::Gap("待前端补读：压缩区间是该事件唯一的增量信息")),
            ("range_end", Status::Gap("待前端补读：压缩区间是该事件唯一的增量信息")),
            ("estimated_tokens", Status::Gap("待前端补读：回答为什么现在压（压力/预算）")),
            ("budget_tokens", Status::Gap("待前端补读：回答为什么现在压（压力/预算）")),
            ("turn", Status::BackendOnly("后端 turn 是采样轮次；前端用 journal.turn")),
        ],
    },
    Rule {
        owner: "EventMsg",
        variant: "CompactionSummarized",
        fields: "compaction_id trigger shadowed_items shadowed_tokens summary",
        overrides: &[
            ("range_start", Status::Gap("待前端补读：压缩区间能回答压了哪一段")),
            ("range_end", Status::Gap("待前端补读：压缩区间能回答压了哪一段")),
        ],
    },
    Rule {
        owner: "EventMsg",
        variant: "CompactionEnded",
        fields: "compaction_id ok error",
        overrides: &[("history_tokens_after", Status::Gap("待前端补读：用户最想看压缩后是否真的变短"))],
    },
];

/// 去掉行尾注释：块内文档注释可能带花括号，会让深度计数错位。
fn strip_comments(src: &str) -> String {
    src.lines()
        .map(|line| match line.find("//") {
            Some(index) => &line[..index],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 取 `header { ... }` 的花括号内文本（`header` 形如 `pub enum EventMsg`）。
fn block_body<'a>(src: &'a str, header: &str) -> Option<&'a str> {
    let start = src.find(header)?;
    let rest = &src[start + header.len()..];
    let open = rest.find('{')?;
    let mut depth = 0usize;
    for (offset, ch) in rest[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&rest[open + 1..open + offset]);
                }
            }
            _ => {}
        }
    }
    None
}

/// 行首标识符 + 冒号 → 字段名（`turn: u32,` / `pub turn: u32,`）。
fn field_name(line: &str) -> Option<String> {
    let line = line.trim().strip_prefix("pub ").unwrap_or(line.trim());
    let end = line
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '_')
        .map(|(i, c)| i + c.len_utf8())
        .last()?;
    let name = &line[..end];
    if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    line[end..].trim_start().starts_with(':').then(|| name.to_string())
}

/// 解析枚举：返回 (变体名, 字段名清单)。
fn enum_fields(body: &str) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    let mut depth = 0i32;
    for line in body.lines() {
        let text = line.trim();
        if text.is_empty() || text.starts_with('#') {
            continue;
        }
        if depth == 0 && text.starts_with(|c: char| c.is_ascii_uppercase()) {
            let end = text
                .char_indices()
                .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '_')
                .map(|(i, c)| i + c.len_utf8())
                .last()
                .unwrap_or(0);
            out.push((text[..end].to_string(), Vec::new()));
            if let Some(open) = text.find('{') {
                collect_fields(&text[open..], &mut out.last_mut().expect("刚 push").1);
            }
        } else if depth >= 1 {
            if let (Some(field), Some(last)) = (field_name(text), out.last_mut()) {
                last.1.push(field);
            }
        }
        depth += text.matches('{').count() as i32 - text.matches('}').count() as i32;
    }
    out
}

/// 解析结构体字段（`pub struct X { ... }`）。
fn struct_fields(body: &str) -> Vec<(String, Vec<String>)> {
    let fields = body.lines().filter_map(field_name).collect::<Vec<_>>();
    vec![(String::new(), fields)]
}

/// 单行变体的内联字段：`{ turn: u32, reason: TurnEndReason }`。
fn collect_fields(inline: &str, out: &mut Vec<String>) {
    let Some(open) = inline.find('{') else { return };
    let inner = &inline[open + 1..];
    for part in inner.split(',') {
        if let Some(field) = field_name(part) {
            out.push(field);
        }
    }
}

fn read(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("读 {} 失败: {e}", path.display()))
}

/// 从 Rust 源头解析出 (owner, variant, field) 三元组。
fn backend_fields() -> Vec<(String, String, String)> {
    let events = strip_comments(&read("src/events.rs"));
    let lib = strip_comments(&read("src/lib.rs"));

    let mut out = Vec::new();
    let mut push = |owner: &str, pairs: Vec<(String, Vec<String>)>| {
        for (variant, fields) in pairs {
            for field in fields {
                // 结构体（AgentEvent）没有变体名，用结构体名占位；枚举保留真实变体名。
                let variant = if variant.is_empty() { owner.to_string() } else { variant.clone() };
                out.push((owner.to_string(), variant, field));
            }
        }
    };
    let event_msg = block_body(&events, "pub enum EventMsg").expect("events.rs 里找不到 EventMsg");
    push("EventMsg", enum_fields(event_msg));
    let envelope = block_body(&events, "pub struct AgentEvent").expect("events.rs 里找不到 AgentEvent");
    push("AgentEvent", struct_fields(envelope));
    let entry = block_body(&lib, "pub enum SessionEntry").expect("lib.rs 里找不到 SessionEntry");
    push("SessionEntry", enum_fields(entry));
    out
}

fn registry() -> BTreeMap<(String, String, String), Status> {
    let mut map = BTreeMap::new();
    for rule in RULES {
        for field in rule.fields.split_whitespace() {
            map.insert(
                (rule.owner.to_string(), rule.variant.to_string(), field.to_string()),
                Status::Read,
            );
        }
        for (field, status) in rule.overrides {
            map.insert(
                (rule.owner.to_string(), rule.variant.to_string(), field.to_string()),
                *status,
            );
        }
    }
    map
}

#[test]
fn every_backend_wire_field_is_classified() {
    let detected = backend_fields();
    // 解析器若损坏会返回空集，那样这张表就变成了永真的装饰品。
    assert!(
        detected.len() >= 100,
        "只解析出 {} 个后端字段，明显少于预期（EventMsg≈81 + AgentEvent 6 + SessionEntry 14），\
         解析器可能已经失效",
        detected.len()
    );

    let map = registry();
    let detected_keys = detected.iter().cloned().collect::<BTreeSet<_>>();
    let mut unclassified = Vec::new();
    for key in &detected_keys {
        if !map.contains_key(key) {
            unclassified.push(format!(
                "- {}.{} 的字段 `{}` 未表态",
                key.0, key.1, key.2
            ));
        }
    }
    assert!(
        unclassified.is_empty(),
        "后端 wire 结构新增了字段，但没有登记消费状态。\n\
         请在本文件 RULES 里显式表态：Read / BackendOnly(理由) / Gap(待补读)。\n\
         「前端暂时用不到」不是默认值——先判断是新漏了还是有意为之：\n{}",
        unclassified.join("\n")
    );

    let stale = map
        .keys()
        .filter(|key| !detected_keys.contains(*key))
        .map(|key| format!("- {}.{} 的字段 `{}` 已不存在", key.0, key.1, key.2))
        .collect::<Vec<_>>();
    assert!(
        stale.is_empty(),
        "登记表引用了后端已不存在的字段（改名或删除后没同步），表里的结论已经不可信：\n{}",
        stale.join("\n")
    );

    let mut gap_lines = Vec::new();
    let mut backend_only_lines = Vec::new();
    for (key, status) in &map {
        match status {
            Status::Read => {}
            Status::BackendOnly(reason) => {
                backend_only_lines.push(format!("- {}.{}.{}：{reason}", key.0, key.1, key.2));
            }
            Status::Gap(reason) => gap_lines.push(format!("- {}.{}.{}：{reason}", key.0, key.1, key.2)),
        }
    }
    println!(
        "[字段消费] {} 个后端字段全部已表态；其中 gap {} 个、backend-only {} 个",
        detected_keys.len(),
        gap_lines.len(),
        backend_only_lines.len()
    );
    println!("[gap]");
    for line in &gap_lines {
        println!("  {line}");
    }
    println!("[backend-only]");
    for line in &backend_only_lines {
        println!("  {line}");
    }
}

/// 反向对照：证明「未登记即失败」不是因为表写错了才碰巧全绿。
#[test]
fn unregistered_field_is_reported() {
    let synthetic = [
        ("EventMsg".to_string(), "AllowlistHit".to_string(), "tool".to_string()),
        ("EventMsg".to_string(), "AllowlistHit".to_string(), "brand_new".to_string()),
    ];
    let map = registry();
    let missing = synthetic
        .iter()
        .filter(|key| !map.contains_key(*key))
        .collect::<Vec<_>>();
    assert_eq!(missing.len(), 1, "新增字段没有被识别为未表态");
    assert_eq!(missing[0].2, "brand_new");
}
