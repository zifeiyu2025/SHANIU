//! EventMsg 单一事件协议（ADR-005，共享内核——前后端契约）。
//!
//! 兼容性铁律（AGENTS §2 / docs §5 FF-2）：仅允许 additive 变更——
//! 新增事件类型或新增 `Option` 字段；字段命名 snake_case，与 `src/transport/types.ts` 镜像。
//! 修改本文件必须同步前端镜像并更新 `tests/event_schema.rs`。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// 后端 → 前端事件流通道。
pub const EVENT_CHANNEL: &str = "shaniu://event";
/// IPC 握手回显通道（M0 验收：ping/pong）。
pub const PONG_CHANNEL: &str = "shaniu://pong";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEndReason {
    Completed,
    Cancelled,
    Failed,
    BudgetExhausted,
}

impl TurnEndReason {
    /// FF-2 单一事实源（SH-078）：变体清单的唯一权威，`tests/event_schema.rs` 据此穷举比对 TS 镜像。
    /// 无通配臂 match 保证新增变体而不更新此处时**编译失败**——模式清单与返回数组必须逐条一致。
    pub fn all_variants() -> &'static [Self] {
        match Self::Completed {
            Self::Completed | Self::Cancelled | Self::Failed | Self::BudgetExhausted => {
                &[Self::Completed, Self::Cancelled, Self::Failed, Self::BudgetExhausted]
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IoChannel {
    Stdout,
    Stderr,
}

impl IoChannel {
    /// FF-2 单一事实源（SH-078）：同 `TurnEndReason::all_variants` 的穷举约定。
    pub fn all_variants() -> &'static [Self] {
        match Self::Stdout {
            Self::Stdout | Self::Stderr => &[Self::Stdout, Self::Stderr],
        }
    }
}

/// 工具风险四级（ADR-003）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Read,
    WriteInWorkspace,
    Execute,
    Destructive,
    /// 计划评审（Plan Mode 退出确认，ADR-003）。
    PlanReview,
}

impl RiskLevel {
    /// FF-2 单一事实源（SH-078）：同 `TurnEndReason::all_variants` 的穷举约定。
    pub fn all_variants() -> &'static [Self] {
        match Self::Read {
            Self::Read | Self::WriteInWorkspace | Self::Execute | Self::Destructive | Self::PlanReview => &[
                Self::Read,
                Self::WriteInWorkspace,
                Self::Execute,
                Self::Destructive,
                Self::PlanReview,
            ],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Allow,
    Deny,
    /// 仅 execute 级有效：本会话记住选择（ADR-003），destructive 永不记忆。
    AllowAlways,
}

impl ApprovalDecision {
    /// FF-2 单一事实源（SH-078）：同 `TurnEndReason::all_variants` 的穷举约定。
    pub fn all_variants() -> &'static [Self] {
        match Self::Allow {
            Self::Allow | Self::Deny | Self::AllowAlways => &[Self::Allow, Self::Deny, Self::AllowAlways],
        }
    }
}

/// 工具调用终态（docs/04 §1.2.1）：`ToolCallEnd.result` 的取值域。
/// 存在时是**唯一**状态判据；缺失时前端才走 `exit_code`/`error` 的两步反推回退。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallResult {
    /// 工具执行成功。
    Ok,
    /// 执行失败（含超时、非零退出、工具层错误）。
    Error,
    /// 用户/系统取消（CancellationToken）。
    Cancelled,
    /// 审批拒绝。
    Denied,
}

impl ToolCallResult {
    /// FF-2 单一事实源（SH-078）：同 `TurnEndReason::all_variants` 的穷举约定。
    pub fn all_variants() -> &'static [Self] {
        match Self::Ok {
            Self::Ok | Self::Error | Self::Cancelled | Self::Denied => {
                &[Self::Ok, Self::Error, Self::Cancelled, Self::Denied]
            }
        }
    }
}

/// 子代理终态（SH-053，docs/02 §2.1：done / failed / cancelled）。
///
/// 定义在协议层而不是 `subagent_registry`：它要跨事件线到前端，而 `events` 是共享
/// 基础设施、不得反向依赖业务模块（FF-1）。注册表以 `pub use ... as ExitStatus`
/// 复用同一份定义，避免两份枚举漂移（同 `PlanState` 迁入共享内核的先例）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentExitStatus {
    Done,
    Failed,
    Cancelled,
}

impl SubagentExitStatus {
    /// FF-2 单一事实源（SH-078）：无通配臂 match = 加变体不更新清单就编译失败。
    pub fn all_variants() -> &'static [Self] {
        match Self::Done {
            Self::Done | Self::Failed | Self::Cancelled => &[Self::Done, Self::Failed, Self::Cancelled],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventMsg {
    TurnStarted { turn: u32 },
    TurnCompleted { turn: u32, reason: TurnEndReason },
    AgentMessageDelta { delta: String },
    /// additive（FF-2，docs/04 §1.2）：思考模型的推理增量。刻意**不复用**正文 delta——
    /// 混进正文会把"想的过程"当成"答的内容"，前端也无法折叠；业界（codex
    /// `ReasoningContentDelta`、ZCode `reasoning_delta`）一律单列事件。
    /// 旧前端遇到未知 type 走 default 分支忽略，不破坏兼容。
    ReasoningDelta { delta: String },
    ToolCallBegin { call_id: String, tool: String, args: serde_json::Value },
    ToolCallOutputDelta { call_id: String, channel: IoChannel, delta: String },
    ToolCallEnd {
        call_id: String,
        exit_code: Option<i32>,
        error: Option<String>,
        /// additive（FF-2，docs/04 §1.2.1）：`None` 时不产生该键，旧消费端无感知。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<ToolCallResult>,
    },
    ExecApprovalRequest {
        request_id: String,
        tool: String,
        args: serde_json::Value,
        risk: RiskLevel,
        /// 审批冒泡（ADR-103）：谁在要权限。additive（FF-2）：旧消费端无感知。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        /// 该代理的任务摘要；root 为 None。让用户在统一审批 UI 上分得清
        /// 「哪个子代理为了哪件事要权限」，而不是只有一个工具名。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_summary: Option<String>,
        /// 风险原因（SH-118⑤，additive）：用户在批什么、为什么现在问。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    ExecApprovalResolved { request_id: String, decision: ApprovalDecision },
    /// 命令前缀白名单命中（SH-120 / ADR-007③，additive）：放行依据必须可事后追溯——
    /// 不弹窗不等于不留痕。`rule_id` 是规则原文（与 config.toml 同一份字符串）。
    /// 旧消费端遇到未知 type 走 default 分支忽略，不破坏兼容。
    AllowlistHit { tool: String, command: String, rule_id: String },
    PlanDelta { delta: String },
    TurnDiff { files: Vec<String> },
    Error { message: String },
    TokenUsage {
        turn: u32,
        input_tokens: u32,
        /// 新增字段必须 optional（FF-2）：旧消费端可忽略。
        #[serde(default)]
        cached_input_tokens: Option<u32>,
        output_tokens: u32,
    },
    /// 子代理派生（SH-053，additive，docs/04 §1.2 预留位）。
    ///
    /// 字段按上游 DSH 的持久事实对齐：`name`/`label`/`created_at_ms` 供父 journal 的
    /// `subagent/catalog` v3（`packages/subagent/subagent/src/catalog.ts`），`task` 全文
    /// 供子泳道首条 user 消息与审批卡摘要。归属走 `AgentEvent` 信封（ADR-104），
    /// 载荷不重复 parent_id。上游预留的 `profile`/`allowed_tools` 不带：前者恒为
    /// worker、后者尚未实现（SH-054），恒定值与死字段不进协议。
    SubagentSpawned {
        child_id: String,
        task: String,
        name: String,
        label: String,
        created_at_ms: u64,
    },
    /// 子代理收口（SH-053，additive）。`status` + `summary` 与上游结算通知
    /// `{summary, stopReason}`（`continuation-messages.ts`）同构；ExitSummary 的
    /// `artifacts` 恒空、`token_usage` 已由子泳道自己的 `token_usage` 事件承载，
    /// 故不进载荷（也避免 events 依赖 registry 的 ExitSummary 类型）。
    SubagentExited {
        child_id: String,
        status: SubagentExitStatus,
        summary: String,
    },
    /// 向子代理注入 steering（SH-053，additive）。上游把注入落为**子会话**的
    /// `user/message`，用 `source.senderSessionId` 表达来源，故这里带 `sender_id`。
    /// docs/04 预留的 `direction: to_child/from_child` 只实现 to_child 半支：
    /// 子代理正文已按 agent_id 信封逐 delta 到达泳道，再发一份 from_child 摘要
    /// 就是第二真相源。`preview` 升级为 `text` 全文——泳道要重建完整 user 消息。
    SubagentMessage {
        child_id: String,
        sender_id: String,
        text: String,
        /// additive（FF-2）：UI 注入时带上客户端铸造的 rpc id，泳道据此把
        /// `source` 写成 `user + rpcId`，客户端撤销自己的乐观回显（上游
        /// `BrowserPromptSource`）；模型侧 steering 不带，落 `agent-message`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rpc_id: Option<String>,
    },
}

/// 事件出口抽象：agent_loop 及各模块经此广播（便于测试注入与 M1 复用）。
pub trait EventSink: Send + Sync {
    fn emit(&self, msg: &EventMsg) -> Result<(), String>;

    /// 以某个子代理身份复制一个出口（docs/02 ADR-104 的 `agent_id` 归属）。
    ///
    /// 默认返回 `None`：纯测试/无信封的出口没有代理身份概念，调用方应退回原出口。
    /// 只有真正构造 `AgentEvent` 的出口（TauriEventSink）才需要实现它。
    fn scoped(&self, _agent_id: &str, _parent_id: Option<&str>) -> Option<Arc<dyn EventSink>> {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentEvent {
    pub event_id: String,
    pub session_id: String,
    pub agent_id: String,
    pub parent_id: Option<String>,
    pub ts_ms: u64,
    pub msg: EventMsg,
}

/// 事件信封持久化缝（SH-094 批 1，依赖倒置）。`events` 是共享基础设施，不反向依赖
/// 具体日志模块；组合根把 `event_journal` 的实现注入 `TauriEventSink`，由此断开
/// `events ↔ event_journal` 环（FF-1：events 自身只能依赖 config/events）。
pub trait EventJournal: Send + Sync {
    fn append(&self, event: &AgentEvent) -> Result<(), String>;
}

static EVENT_IDS: Mutex<ulid::Generator> = Mutex::new(ulid::Generator::new());

/// WS 在线判据（SH-127 M2）：已鉴权且**至少订阅过一个会话**的 gateway 连接数。
///
/// 为什么不只数连接：半死连接（客户端崩了 / 网断了但 TCP 没超时）会让计数恒 > 0，
/// 于是 Tauri 通道被永久压住、WS 那头也没人收。所以只有真正订阅过的连接才计数，
/// 且 gateway 对这类连接有空闲超时（60s 无任何帧即断开并减计数）。
#[derive(Clone, Default)]
pub struct ViewerCount(Arc<AtomicUsize>);

impl ViewerCount {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn enter(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    pub fn leave(&self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }

    pub fn get(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }

    pub fn any(&self) -> bool {
        self.get() > 0
    }
}

/// 把事件推给 Tauri 前端（IPC 广播）。抽成 trait 只为可测：
/// 生产用 `AppHandle`，测试用计数器实现，从而能数出「每条事件实际投递几次」。
pub trait IpcEmitter: Send + Sync {
    fn emit(&self, event: &AgentEvent) -> Result<(), String>;
}

impl IpcEmitter for tauri::AppHandle {
    fn emit(&self, event: &AgentEvent) -> Result<(), String> {
        tauri::Emitter::emit(self, EVENT_CHANNEL, event).map_err(|e| format!("事件广播失败: {e}"))
    }
}

fn emit_agent_event(
    session_id: &str,
    agent_id: &str,
    parent_id: Option<&str>,
    msg: &EventMsg,
    publish: impl FnOnce(&AgentEvent) -> Result<(), String>,
) -> Result<(), String> {
    // 编号与发送共用临界区，否则并发生产者可先发出较大的 event_id。
    let mut ids = EVENT_IDS.lock().map_err(|e| format!("事件编号锁不可用: {e}"))?;
    let now = SystemTime::now();
    let ts_ms = now.duration_since(UNIX_EPOCH).map_err(|e| format!("事件时间不可用: {e}"))?.as_millis() as u64;
    let event_id = ids.generate_from_datetime(now).map_err(|e| format!("事件编号生成失败: {e}"))?;
    publish(&AgentEvent {
        event_id: event_id.to_string(),
        session_id: session_id.to_owned(),
        agent_id: agent_id.to_owned(),
        parent_id: parent_id.map(str::to_owned),
        ts_ms,
        msg: msg.clone(),
    })
}

pub struct TauriEventSink {
    ipc: Arc<dyn IpcEmitter>,
    session_id: String,
    /// 事件归属（ADR-104）：root 为 `"root"`，子代理为注册表分配的 id。
    agent_id: String,
    parent_id: Option<String>,
    /// SH-094 批 1：注入的事件信封持久化缝。None = 不落盘（测试/无目录启动）。
    journal: Option<Arc<dyn EventJournal>>,
    /// SH-094 批 2：注入的 WS gateway 广播发送端（依赖倒置——events 不引用 gateway 模块，
    /// 只持 tokio Sender）。None = 不喂 gateway。组合根从 GatewayHub 取发送端注入。
    broadcast: Option<broadcast::Sender<AgentEvent>>,
    /// SH-127 M2：WS 在线连接数。>0 时 Tauri IPC 通道是重复投递，跳过；
    /// 归零（掉线/回退）即自动恢复。None = 无法判断，按旧行为始终走 IPC。
    viewers: Option<ViewerCount>,
}

impl TauriEventSink {
    pub fn root(app: tauri::AppHandle, session_id: String) -> Self {
        Self {
            ipc: Arc::new(app),
            session_id,
            agent_id: "root".into(),
            parent_id: None,
            journal: None,
            broadcast: None,
            viewers: None,
        }
    }

    /// 子代理出口：同一会话、同一持久化/广播缝，只改事件归属。
    pub fn scoped(
        app: tauri::AppHandle,
        session_id: String,
        agent_id: impl Into<String>,
        parent_id: Option<String>,
    ) -> Self {
        Self {
            ipc: Arc::new(app),
            session_id,
            agent_id: agent_id.into(),
            parent_id,
            journal: None,
            broadcast: None,
            viewers: None,
        }
    }

    /// 注入持久化实现：信封逐条落盘；广播仍是主通道，日志失败只告警不中断回合。
    pub fn with_journal(mut self, journal: Arc<dyn EventJournal>) -> Self {
        self.journal = Some(journal);
        self
    }

    /// 注入 WS gateway 广播发送端：信封落盘后发布到 gateway hub（批 3 切前端后取代 Tauri emit）。
    pub fn with_broadcast(mut self, sender: broadcast::Sender<AgentEvent>) -> Self {
        self.broadcast = Some(sender);
        self
    }

    /// 注入 WS 在线计数：在线时不再重复投递 Tauri IPC。
    pub fn with_viewers(mut self, viewers: ViewerCount) -> Self {
        self.viewers = Some(viewers);
        self
    }
}

impl EventSink for TauriEventSink {
    fn scoped(&self, agent_id: &str, parent_id: Option<&str>) -> Option<Arc<dyn EventSink>> {
        Some(Arc::new(TauriEventSink {
            ipc: self.ipc.clone(),
            session_id: self.session_id.clone(),
            agent_id: agent_id.to_owned(),
            parent_id: parent_id.map(str::to_owned),
            journal: self.journal.clone(),
            broadcast: self.broadcast.clone(),
            viewers: self.viewers.clone(),
        }))
    }

    fn emit(&self, msg: &EventMsg) -> Result<(), String> {
        emit_agent_event(&self.session_id, &self.agent_id, self.parent_id.as_deref(), msg, |event| {
            // 先落盘再广播：收到事件的客户端按 event_id 续播时，日志里一定已有它。
            if let Some(journal) = &self.journal {
                if let Err(error) = journal.append(event) {
                    tracing::warn!("事件日志写入失败（广播继续）: {error}");
                }
            }
            // 喂 WS gateway（无接收端时 send 返回 Err，值丢弃——续播由日志兜底）。
            if let Some(sender) = &self.broadcast {
                let _ = sender.send(event.clone());
            }
            // M2：WS 有在线订阅者时，Tauri IPC 那条路没人听（前端 WS 模式不注册 Tauri
            // 监听），继续发就是同一条事件的第二次投递。跳过它，等计数归零自动恢复。
            if self.viewers.as_ref().is_some_and(ViewerCount::any) {
                tracing::debug!(event_id = %event.event_id, "WS 在线，跳过 Tauri IPC 投递");
                return Ok(());
            }
            self.ipc.emit(event)
        })
    }
}

/// 测试缝：不依赖 `AppHandle` 构造 sink，换入计数型 IPC 实现。
#[cfg(test)]
impl TauriEventSink {
    pub(crate) fn for_test(ipc: Arc<dyn IpcEmitter>) -> Self {
        Self {
            ipc,
            session_id: "test".into(),
            agent_id: "root".into(),
            parent_id: None,
            journal: None,
            broadcast: None,
            viewers: None,
        }
    }
}

/// IPC 握手：回显 nonce 证明 invoke → 后端 → 事件通道 → 前端链路连通。
pub fn emit_pong(app: &tauri::AppHandle, nonce: &str) -> Result<(), String> {
    // 显式限定 trait：本模块新增了 `IpcEmitter for AppHandle`，与 `tauri::Emitter` 同名，
    // 不写全路径会报 multiple applicable items（E0034）。
    tauri::Emitter::emit(
        app,
        PONG_CHANNEL,
        serde_json::json!({ "nonce": nonce }),
    )
    .map_err(|e| format!("pong 广播失败: {e}"))
}

/// 测试支撑：丢弃全部事件（工具/循环单测的 sink 注入件）。
#[cfg(test)]
#[derive(Debug, Default)]
pub struct NullEventSink;

#[cfg(test)]
impl EventSink for NullEventSink {
    fn emit(&self, _msg: &EventMsg) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod envelope_tests {
    use super::*;

    #[test]
    fn root_event_keeps_session_payload_and_timestamp() {
        let msg = EventMsg::ReasoningDelta { delta: "推理片段".into() };
        let before = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        emit_agent_event("session-a", "root", None, &msg, |event| {
            assert_eq!(event.session_id, "session-a");
            assert_eq!(event.agent_id, "root");
            assert_eq!(event.parent_id, None);
            assert_eq!(event.msg, msg);
            assert!(event.ts_ms >= before);
            let id = event.event_id.parse::<ulid::Ulid>().unwrap();
            assert!(id.timestamp_ms() >= event.ts_ms);
            Ok(())
        }).unwrap();
    }

    #[test]
    fn scoped_events_carry_agent_identity_without_changing_payload() {
        let msg = EventMsg::AgentMessageDelta { delta: "子代理片段".into() };
        emit_agent_event("session-a", "01AGENT", Some("root"), &msg, |event| {
            assert_eq!(event.session_id, "session-a");
            assert_eq!(event.agent_id, "01AGENT");
            assert_eq!(event.parent_id.as_deref(), Some("root"));
            // 归属只加在信封上，载荷原样保留——旧消费方（FF-104）仍能读 msg。
            assert_eq!(event.msg, msg);
            Ok(())
        })
        .unwrap();
    }

    /// M2 验收：WS 有在线订阅者时，同一条事件不再被第二次投递到 Tauri IPC；
    /// 计数归零（掉线/回退）后 IPC 自动恢复。
    #[test]
    fn ws_viewer_presence_skips_duplicate_ipc_delivery() {
        struct CountingIpc(AtomicUsize);
        impl IpcEmitter for CountingIpc {
            fn emit(&self, _: &AgentEvent) -> Result<(), String> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }
        let ipc = Arc::new(CountingIpc(AtomicUsize::new(0)));
        let viewers = ViewerCount::new();
        let sink = TauriEventSink::for_test(ipc.clone()).with_viewers(viewers.clone());
        let msg = EventMsg::TurnStarted { turn: 1 };

        for _ in 0..5 {
            sink.emit(&msg).unwrap();
        }
        assert_eq!(ipc.0.load(Ordering::SeqCst), 5, "无 WS 在线时每条事件都走 IPC");
        println!("[M2] 无在线订阅者：5 条事件 → IPC 投递 {} 次", ipc.0.load(Ordering::SeqCst));

        viewers.enter();
        for _ in 0..5 {
            sink.emit(&msg).unwrap();
        }
        assert_eq!(
            ipc.0.load(Ordering::SeqCst),
            5,
            "WS 在线时单条事件只投递一次（WS 侧），不该再走 IPC"
        );
        println!("[M2] WS 在线（再发 5 条）→ 累计 IPC 投递 {} 次（增量为 0）", ipc.0.load(Ordering::SeqCst));

        viewers.leave();
        sink.emit(&msg).unwrap();
        assert_eq!(ipc.0.load(Ordering::SeqCst), 6, "WS 计数归零后 IPC 必须恢复");
        println!("[M2] 在线计数归零（再发 1 条）→ 累计 IPC 投递 {} 次（增量 1）", ipc.0.load(Ordering::SeqCst));
    }

    #[test]
    fn ids_remain_ordered_across_sessions_and_resumed_tasks() {
        let mut ids = Vec::new();
        for session in ["first", "second", "first"] {
            emit_agent_event(session, "root", None, &EventMsg::TurnStarted { turn: 1 }, |event| {
                assert_eq!(event.session_id, session);
                ids.push(event.event_id.clone());
                Ok(())
            }).unwrap();
        }
        assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn concurrent_publish_order_matches_id_order_without_losing_events() {
        let received = Mutex::new(Vec::new());
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for worker in 0..8 {
                let received = &received;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    let session_id = format!("session-{worker}");
                    for index in 0..64 {
                        emit_agent_event(&session_id, "root", None, &EventMsg::AgentMessageDelta { delta: index.to_string() }, |event| {
                            received.lock().unwrap().push(event.clone());
                            Ok(())
                        }).unwrap();
                    }
                });
            }
        });
        let events = received.into_inner().unwrap();
        assert_eq!(events.len(), 512);
        assert!(events.windows(2).all(|pair| pair[0].event_id < pair[1].event_id));
        for worker in 0..8 {
            let session_id = format!("session-{worker}");
            let messages: Vec<_> = events.iter().filter(|event| event.session_id == session_id).map(|event| &event.msg).collect();
            assert_eq!(messages.len(), 64);
            for (index, msg) in messages.into_iter().enumerate() {
                assert_eq!(*msg, EventMsg::AgentMessageDelta { delta: index.to_string() });
            }
        }
    }

    #[test]
    fn publish_failure_propagates_without_poisoning_the_next_event() {
        let mut failed_id = String::new();
        let msg = EventMsg::Error { message: "测试错误".into() };
        let result = emit_agent_event("session-a", "root", None, &msg, |event| {
            failed_id = event.event_id.clone();
            Err("传输不可用".into())
        });
        assert_eq!(result, Err("传输不可用".into()));
        emit_agent_event("session-a", "root", None, &msg, |event| {
            assert!(event.event_id > failed_id);
            Ok(())
        }).unwrap();
    }
}
