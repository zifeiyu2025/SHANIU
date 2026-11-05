//! 健身契约测试：rollout JSONL 格式（docs/04 §3.1，SH-030）。
//!
//! 测的是**档案与运行时历史的一致性**，不是字面量快照：
//! 1) 首行必须是 `session_meta`，且带 04 §3.1 要求的全部字段；
//! 2) 其后每行都是 `response_item`，且把档案读回来再喂给模型，应与运行时 `history` 等价
//!    ——resume（SH-031）依赖的正是这条，档案与历史漂移是最难查的一类 bug；
//! 3) 一行一条 JSON、以换行结尾（逐行 flush 的直接后果）。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use shaniu_lib::agent_loop::{AgentLoop, ModelBackend, ModelStream, ToolDeclarationShim};
use shaniu_lib::config::{ApprovalMode, SandboxMode};
use shaniu_lib::events::{EventMsg, EventSink};
use shaniu_lib::model_client::{HistoryItem, ModelStreamEvent, Role, Usage};
use shaniu_lib::permission::{policy, ApprovalBroker};
use shaniu_lib::rollout::{self, ConfigSnapshot, RolloutItem, ROLLOUT_DIR};
use shaniu_lib::tool_router::{PlanState, ToolRouter};
use tokio_util::sync::CancellationToken;

static SEQ: AtomicUsize = AtomicUsize::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("shaniu-rollout-it-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 两轮脚本：第一轮文本+工具调用，第二轮纯文本收口。
/// 覆盖档案里会出现的全部载荷形态，同时让轮数成为确定值。
struct ScriptedBackend(Mutex<VecDeque<Vec<ModelStreamEvent>>>);

#[async_trait]
impl ModelBackend for ScriptedBackend {
    async fn stream(
        &self,
        _history: &[HistoryItem],
        _tools: &[ToolDeclarationShim],
        _instructions: &str,
    ) -> Result<ModelStream, String> {
        let script = self.0.lock().unwrap().pop_front().unwrap_or_default();
        Ok(Box::pin(futures::stream::iter(script)))
    }
}

/// 档案断言只看落盘文件，事件面这里只需存在（吞掉即可）。
struct NullSink;

impl EventSink for NullSink {
    fn emit(&self, _msg: &EventMsg) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn rollout_file_matches_history_after_one_task() {
    let data_root = temp_dir("one");
    let rollout_dir = data_root.join(ROLLOUT_DIR);
    let meta = RolloutItem::SessionMeta {
        session_id: "sess-it".into(),
        agent_id: "agent-it".into(),
        parent_id: None,
        created_at: 1_700_000_000_000,
        config_snapshot: ConfigSnapshot {
            provider: "openai-chat".into(),
            base_url: "https://x/v1".into(),
            model: "m".into(),
            sandbox: "WorkspaceWrite".into(),
            approval: "Ask".into(),
            workspace_root: data_root.display().to_string(),
        },
    };
    let writer = Arc::new(rollout::RolloutWriter::create(&rollout_dir, &meta).unwrap());

    let turn_one = vec![
        ModelStreamEvent::ReasoningDelta { delta: "先看一眼".into() },
        ModelStreamEvent::OutputTextDelta { delta: "我来读文件".into() },
        ModelStreamEvent::OutputItem(shaniu_lib::model_client::ResponseItem::FunctionCall {
            name: "read_file".into(),
            arguments: r#"{"path":"a.txt"}"#.into(),
            call_id: "c1".into(),
        }),
        ModelStreamEvent::Completed {
            usage: Usage { input_tokens: 5, cached_input_tokens: None, output_tokens: 3 },
        },
    ];
    let turn_two = vec![
        ModelStreamEvent::OutputTextDelta { delta: "读完了".into() },
        ModelStreamEvent::Completed {
            usage: Usage { input_tokens: 6, cached_input_tokens: None, output_tokens: 2 },
        },
    ];
    let backend = Arc::new(ScriptedBackend(Mutex::new(VecDeque::from([turn_one, turn_two]))));

    let agent = AgentLoop {
        backend,
        // 空路由：read_file 未注册 → 走"未知工具"分支，仍会留下 call + output 两条记录
        router: Arc::new(ToolRouter::new()),
        broker: Arc::new(ApprovalBroker::new()),
        plan_state: Arc::new(PlanState::new()),
        workspace_root: data_root.clone(),
        max_turns: 2,
        token_budget: 0,
        policy: policy::SessionPolicy { approval: ApprovalMode::Ask, sandbox: SandboxMode::WorkspaceWrite },
        rollout: Some(writer.clone()),
        rollout_broke: std::sync::atomic::AtomicBool::new(false),
    };
    let sink: Arc<dyn EventSink> = Arc::new(NullSink);
    let summary = agent.run_task("读 a.txt", sink, CancellationToken::new()).await;
    assert_eq!(summary.turns, 2, "脚本两轮收口（第二轮无工具调用）");

    let path = rollout_dir.join("agent-it.jsonl");
    let raw = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(raw.ends_with('\n'), "每行必须以换行结尾（逐行 flush 的可见后果）");

    // 首行契约
    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(first["type"], "session_meta");
    for field in ["session_id", "agent_id", "parent_id", "created_at", "config_snapshot"] {
        assert!(first.get(field).is_some(), "session_meta 缺 04 §3.1 要求的字段 {field}");
    }
    assert_eq!(first["config_snapshot"]["model"], "m");

    // 其余行都是 response_item，且带 type 标签
    for line in &lines[1..] {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["type"], "response_item", "档案主体只能是 response_item，收到 {v}");
        assert!(v.get("item").is_some(), "response_item 必须带 item 载荷");
    }

    // 读回来必须与写出去的一致：user 消息 + assistant(文本+reasoning 未回传则仅正文) + call + output
    let back = rollout::read(&path).unwrap();
    assert_eq!(back.meta, meta, "首行档案必须原样读回");
    assert_eq!(back.torn_tail_lines, 0, "正常收口不该有半截写");
    let items: Vec<HistoryItem> = back
        .items
        .iter()
        .map(|i| match i {
            RolloutItem::ResponseItem { item } => item.clone(),
            other => panic!("本轮不该出现 {other:?}"),
        })
        .collect();
    assert!(matches!(items.first(), Some(HistoryItem::Message { role: Role::User, text, .. }) if text == "读 a.txt"), "首条应为用户任务：{items:?}");
    // 两条助手正文都在档里（第一轮 "我来读文件"、第二轮 "读完了"）
    let assistant_texts: Vec<&str> = items
        .iter()
        .filter_map(|i| match i {
            HistoryItem::Message { role: Role::Assistant, text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(assistant_texts, vec!["我来读文件", "读完了"], "跨轮文本必须按序入档：{assistant_texts:?}");
    assert!(
        items.iter().any(|i| matches!(i, HistoryItem::FunctionCall { call_id, .. } if call_id == "c1")),
        "工具调用必须入档：{items:?}"
    );
    assert!(
        items.iter().any(|i| matches!(i, HistoryItem::FunctionCallOutput { call_id, .. } if call_id == "c1")),
        "工具输出必须入档——缺它则 resume 后的历史违反 ChatCompletions 配对规则（SH-082）"
    );

    // 档案重放成历史后，再喂给 Chat 请求构造器必须产出合法 messages（与运行时同构）
    let msgs = shaniu_lib::model_client::chat_messages("SYS", &items);
    let with_calls: Vec<_> = msgs.iter().filter(|m| m["tool_calls"].is_array()).collect();
    assert_eq!(with_calls.len(), 1, "同轮调用应合成一条 assistant 消息");

    std::fs::remove_dir_all(&data_root).ok();
}

#[tokio::test]
async fn second_task_gets_its_own_rollout_file_not_overwrite() {
    // 建档拒绝覆盖：这是不可逆数据丢失的唯一防线（agent_id 由会话序号给出，正常不会撞；
    // 撞了说明调用方错了，静默覆盖等于把上一段会话历史抹掉）
    let dir = temp_dir("dup");
    let meta = RolloutItem::SessionMeta {
        session_id: "s".into(),
        agent_id: "same".into(),
        parent_id: None,
        created_at: 1,
        config_snapshot: ConfigSnapshot::default(),
    };
    let _first = rollout::RolloutWriter::create(&dir, &meta).unwrap();
    let err = rollout::RolloutWriter::create(&dir, &meta).unwrap_err();
    assert!(matches!(err, rollout::RolloutError::AlreadyExists(_)), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}
