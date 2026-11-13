//! EventMsg 单一事件协议（ADR-005，共享内核——前后端契约）。
//!
//! 兼容性铁律（AGENTS §2 / docs §5 FF-2）：仅允许 additive 变更——
//! 新增事件类型或新增 `Option` 字段；字段命名 snake_case，与 `src/transport/types.ts` 镜像。
//! 修改本文件必须同步前端镜像并更新 `tests/event_schema.rs`。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::clock::now_ms;

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

/// 回合失败的结构化描述（审计 A0，additive）。
///
/// `code` 是稳定分类码。模型侧：`AUTH` / `QUOTA` / `RATE_LIMIT` / `TIMEOUT` / `NETWORK` /
/// `SERVER` / `ENDPOINT_UNSUPPORTED` / `BAD_REQUEST` / `EMPTY_RESPONSE` /
/// `CONTEXT_TOO_LONG` / `CONTENT_FILTERED` / `INCOMPLETE` / `UNKNOWN`；
/// 本地侧：`COMPACTION_FAILED` / `CONTEXT_EXPAND_FAILED` / `TEST_TAMPERING` /
/// `VERIFY_FAILED`。前端按它本地化，`message` 始终是可读的一句话（不得为空）。
/// `provider_code` / `provider_message` 是端点原始值（已脱敏），用于展开与对账。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TurnErrorInfo {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_message: Option<String>,
    /// 本次失败前已经重试过几次（0 = 首次请求就失败）。additive。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts: Option<u32>,
    /// 端点 `Retry-After` 原值（秒）。仅“因该值超过上限而放弃重试”时填写。additive。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_secs: Option<u64>,
}

/// 一次模型请求重试的阶段（`model_retry` 事件的 `phase`，additive）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryPhase {
    /// 即将等待：`delay_ms` 是本次要等多久。
    Scheduled,
    /// 等待结束、重新建流之前。
    Started,
    /// 次数用尽，或因 `Retry-After` 超过上限而放弃。
    Exhausted,
}

impl RetryPhase {
    /// FF-2 单一事实源（SH-078）：同 `TurnEndReason::all_variants` 的穷举约定。
    pub fn all_variants() -> &'static [Self] {
        match Self::Scheduled {
            Self::Scheduled | Self::Started | Self::Exhausted => &[Self::Scheduled, Self::Started, Self::Exhausted],
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

/// 写后验证的四态（docs/04 §1.2）。
///
/// `Inconclusive` / `Skipped` 的原因码都是**封闭枚举**：没有 `Other`、没有 catch-all。
/// 判定侧凡未命中任何具名原因，一律按 `Failed` 处理（fail-closed）——
/// 「判不出来」不许变成偷偷放过。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyState {
    Passed,
    Failed,
    Inconclusive(InconclusiveReason),
    Skipped(SkipReason),
}

/// `Inconclusive` 的封闭原因集。新增成员必须同时补 `docs/04` 与判定用例。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InconclusiveReason {
    /// 单命令或总时限内没跑完。
    Timeout,
    /// 命令根本不存在（shell 报 `not recognized` / `command not found`）。
    CommandMissing,
    /// 命令在首次置脏之前就已经红（基线命中），不是本次改动造成的。
    PreExistingRed,
    /// 环境类失败：网络、磁盘、权限、注册表等，重跑也不一定好。
    EnvFailure,
}

impl InconclusiveReason {
    /// FF-2 单一事实源（SH-078）：同 `TurnEndReason::all_variants` 的穷举约定。
    pub fn all_variants() -> &'static [Self] {
        match Self::Timeout {
            Self::Timeout | Self::CommandMissing | Self::PreExistingRed | Self::EnvFailure => {
                &[Self::Timeout, Self::CommandMissing, Self::PreExistingRed, Self::EnvFailure]
            }
        }
    }
}

/// `Skipped` 的封闭原因集。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// 本次没有源码类写入（纯文档/资源改动）。
    NoSourceWrite,
    /// 配置与探测都没给出命令。
    NoCommandConfigured,
}

impl SkipReason {
    /// FF-2 单一事实源（SH-078）：同 `TurnEndReason::all_variants` 的穷举约定。
    pub fn all_variants() -> &'static [Self] {
        match Self::NoSourceWrite {
            Self::NoSourceWrite | Self::NoCommandConfigured => {
                &[Self::NoSourceWrite, Self::NoCommandConfigured]
            }
        }
    }
}

/// `#[serde(skip_serializing_if)]` 的布尔谓词：只落有信息的键，`false` 走缺省。
fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventMsg {
    TurnStarted { turn: u32 },
    TurnCompleted {
        turn: u32,
        reason: TurnEndReason,
        /// 审计 A0：失败回合的结构化错误。additive + optional，旧消费端无感知。
        /// 有它，UI 才能说"密钥无效"而不是"回合以 failed 结束"，导出才能对账 401。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<TurnErrorInfo>,
        /// 写后验证闸门（additive）：本回合在「有源码写入但未验证」的情况下结束。
        /// 末轮不可否决时它会是 `true`，且不会伴随 `VerificationResult`——这是设计
        /// 明确披露的窗口，不是漏报。旧档案没有该字段，按 `false` 解码。
        ///
        /// `false` 时不落键：`04 §1.2` 把它写成 `unverified?: bool`（缺省即 false），
        /// 而 FF-104 要求旧事件载荷的键集合一个都不能多——无条件序列化会让
        /// `turn_completed` 凭空多出一个键，旧消费端的严格校验直接失败。
        #[serde(default, skip_serializing_if = "is_false")]
        unverified: bool,
    },
    /// 写后验证闸门（additive，docs/04 §1.2）：宿主执行项目检查命令的结果。
    ///
    /// 命令由宿主从配置/探测得到，**不受模型控制**；事件带命令原文是为了让
    /// 「免审批执行」这件事可审计——不弹窗不等于不留痕。
    VerificationResult {
        turn: u32,
        state: VerifyState,
        commands: Vec<String>,
        duration_ms: u64,
        summary: String,
        /// 最多 5 条，与自愈诊断同口径。
        diagnostics: Vec<String>,
    },
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
        /// 关联的工具调用 id（additive）：审批卡详情区按它回聊天快照取完整参数。
        /// 不从 `request_id` 里解析 `appr-` 前缀——那是约定不是契约，格式一变就无声失效，
        /// 而失效的表现是「详情又白了」。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        /// 批准后批量覆盖的范围（additive），值是范围标识不是文案：
        /// `"same-args"` = 本轮内参数完全相同的调用不再询问。
        /// `None` = 批准只对这一次生效。用户必须能在卡片上看到这句话覆盖了多远。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        batch_scope: Option<String>,
    },
    ExecApprovalResolved { request_id: String, decision: ApprovalDecision },
    /// 命令前缀白名单命中（SH-120 / ADR-007③，additive）：放行依据必须可事后追溯——
    /// 不弹窗不等于不留痕。`rule_id` 是规则原文（与 config.toml 同一份字符串）。
    /// 旧消费端遇到未知 type 走 default 分支忽略，不破坏兼容。
    AllowlistHit { tool: String, command: String, rule_id: String },
    PlanDelta { delta: String },
    TurnDiff { files: Vec<String> },
    Error { message: String },
    /// 模型请求重试（additive，A0 之后的“可见重试”）：三阶段都发，前端据此建节点。
    ///
    /// 只在**首个可见增量之前**发生（正文 / 推理增量都算可见输出）；目前已吐出的
    /// 尝试绝不会重试，所以这个事件也绝不会跟着重复正文。
    ModelRetry {
        turn: u32,
        /// 第几次重试，1 起。
        retry: u32,
        /// 本次采样允许的最大重试次数（不含首次请求）。
        max_retries: u32,
        phase: RetryPhase,
        /// `scheduled` = 将要等待的毫秒；`started` / `exhausted` 恒为 0。
        delay_ms: u64,
        /// 与 `ModelError::code()` 同一套稳定码。
        code: String,
        /// 已脱敏的一句话原因。
        message: String,
        /// 同一条重试链的稳定 id；不同采样步不复用。
        retry_id: String,
    },
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
    /// 档案降级（additive）：轨迹落盘失败，但对话继续。
    ///
    /// 为什么不是 `Error`：写档案是观测，不是主流程。磁盘满/ACL 挡一下不该让本轮
    /// 失败；但也必须让用户知道「这轮轨迹可能不完整」，并留一条可被后续诊断读到
    /// 的状态（本事件随 journal 落盘，日志侧另有 `tracing::warn!`）。
    RolloutDegraded { message: String },
    /// 系统提示词版本（additive，A0 之后的「提示词入轨迹」）。
    ///
    /// 只在**实际下发的那条路径**上、内容指纹变化时发出：同一 run 的多轮不重复。
    /// 载荷是实际下发的原文，落 journal 时由既有 `config::redact` 统一脱敏；
    /// 不要在这里预处理，否则轨迹里看到的就不是模型看到的那份。
    SystemPrompt {
        text: String,
        /// 落盘文本的指纹，前端据此判断版本是否变化。
        sha256: String,
        at_ms: u64,
        /// 记录时的轮次。
        turn: u32,
        /// 记录时的步（与 `tool/call` 等事件同一口径）。
        step: u32,
    },
    /// 上下文压缩开始（additive）。
    ///
    /// 压缩是**有损操作**：从这一刻到 [`EventMsg::CompactionEnded`] 之间，历史会被
    /// 一段摘要替换。不发这条，用户只会看到「上下文忽然变短」（甚至连这个都看不到），
    /// 无法回答「刚才发生了什么」——与被吞掉的失败是同一类问题。
    CompactionStarted {
        /// 一次压缩的唯一标识，串起 started / summarized / ended 三条。
        compaction_id: String,
        trigger: CompactionTrigger,
        /// 被替换的历史区间 `[range_start, range_end)`（回放后历史的下标）。
        range_start: u32,
        range_end: u32,
        /// 触发时的上下文规模（与预算判据同口径的字节估算）。
        estimated_tokens: u32,
        /// 触发时的 token 预算；0 = 未启用压缩，不会出现本事件。
        budget_tokens: u32,
        /// 回合序号；`None` = 回合外手动 `/compact`。
        turn: Option<u32>,
    },
    /// 摘要已生成、历史已替换：记录**丢了什么**（additive）。
    ///
    /// 只有区间与摘要，没有原文——原文仍在档案里（rollout 只追加不删）。要找回被丢的
    /// 内容，看这条事件的区间 + 档案即可；要判断「变短之后剩了什么」，看 `summary`。
    CompactionSummarized {
        compaction_id: String,
        trigger: CompactionTrigger,
        range_start: u32,
        range_end: u32,
        /// 被摘要取代的历史条目数（= `range_end - range_start`）。
        shadowed_items: u32,
        /// 被取代区间的 token 估算（与预算判据同口径）。
        shadowed_tokens: u32,
        /// 摘要全文：它替换了被丢弃的区间。
        summary: String,
    },
    /// 压缩结束（additive）：成功与失败都发。
    ///
    /// 失败也必须落到这里。只发 `Error` 的话，UI 无法把「一次失败的压缩」与
    /// 「一次普通的工具失败」分开，事后也没有时间线可对。
    CompactionEnded {
        compaction_id: String,
        ok: bool,
        /// 失败原因；成功时缺省（`skip_serializing_if` 保证不产生该键）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// 压缩后的历史规模（成功时给）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        history_tokens_after: Option<u32>,
    },
}

/// 压缩触发原因（additive）。
///
/// 刻意不预留「上下文溢出」：本仓库只有预算压力与手动两条路径，多一个永不产出的
/// 变体就是「写了没接上」（AGENTS §4.4）。真接入溢出触发时再连同消费方一起加。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    /// 回合内上下文达到预算（ADR-001）。
    Pressure,
    /// 用户主动 `/compact`（回合外）。
    Manual,
}

impl CompactionTrigger {
    /// FF-2 单一事实源（SH-078）：同 `TurnEndReason::all_variants` 的穷举约定。
    pub fn all_variants() -> &'static [Self] {
        match Self::Pressure {
            Self::Pressure | Self::Manual => &[Self::Pressure, Self::Manual],
        }
    }
}

/// 「此刻有没有人能回执一张审批卡」的三态（照 dsh 的 `unavailable` 语义）。
///
/// 判据只有结构性事实，不含任何时间假设：没有超时，也没有「等了多久算没人」。
/// 依据：`src/dsh/packages/interaction/user-approval/src/index.ts:306-317` ——
/// 审批请求走 waterfall，默认终点就是 `'unavailable'`（没有应答者 = 立刻 fail closed）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponderState {
    /// 有存活的 webview，且前端已握手：卡片可能被点击。
    Ready,
    /// 一个 webview 都没有（无头运行 / 窗口全销毁）。
    NoWebview,
    /// webview 在，但前端从未握手（页面加载失败 / 脚本崩在启动）。
    FrontendNotReady,
}

impl ResponderState {
    /// 落日志用的原因：拒绝必须可定位，不能只剩一句「被拒了」。
    pub fn reason(self) -> &'static str {
        match self {
            Self::Ready => "有可回执的前端",
            Self::NoWebview => "没有存活的 webview（无头运行或窗口已销毁）",
            Self::FrontendNotReady => "webview 存在但前端未握手",
        }
    }
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

    /// 审批卡此刻有没有可能被回执。
    ///
    /// 默认 `Ready`：测试替身与无信封出口没有前端概念，维持旧行为（等回执），
    /// 否则所有既有用例会在「没人能回」上被误判成拒绝。生产出口（TauriEventSink）
    /// 必须给出真实判定。
    fn responder(&self) -> ResponderState {
        ResponderState::Ready
    }
}

/// 前端是否已发出就绪信号（由 `frontend_ready` 命令置位，只置不清）。
///
/// 放在 events 而不是 lib：审批探测在出口层，不能让叶子模块反向依赖 crate 根。
static FRONTEND_READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 前端就绪信号的唯一写入口；返回「是否是第一次」。
pub fn mark_frontend_ready() -> bool {
    !FRONTEND_READY.swap(true, Ordering::SeqCst)
}

/// 前端是否已握手（窗口可能已隐藏到托盘，但页面还在）。
pub fn frontend_ready() -> bool {
    FRONTEND_READY.load(Ordering::SeqCst)
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

    /// 审批卡有没有可能被回执。默认 `Ready` 供测试替身维持旧行为。
    fn responder(&self) -> ResponderState {
        ResponderState::Ready
    }
}

impl IpcEmitter for tauri::AppHandle {
    fn emit(&self, event: &AgentEvent) -> Result<(), String> {
        tauri::Emitter::emit(self, EVENT_CHANNEL, event).map_err(|e| format!("事件广播失败: {e}"))
    }

    /// 结构性判据，不问「用户是否在看」：
    /// - 没有任何 webview → 卡片发出去也没人能点（无头运行 / 窗口全销毁）；
    /// - 有 webview 但前端从未握手 → 页面没跑起来，监听器不存在；
    /// - 隐藏到托盘不算不可达：窗口对象仍在，点托盘就能看见卡片。
    fn responder(&self) -> ResponderState {
        if tauri::Manager::webview_windows(self).is_empty() {
            return ResponderState::NoWebview;
        }
        if !frontend_ready() {
            return ResponderState::FrontendNotReady;
        }
        ResponderState::Ready
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
    let ts_ms = now_ms();
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

    /// 子代理与 root 共用同一个 `ipc`，所以回执能力天然一致——ADR-103 只换信封，
    /// 不换通道。子代理没有独立回执方式，这也是它们不会被本判定漏掉的依据。
    fn responder(&self) -> ResponderState {
        self.ipc.responder()
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
        let before = now_ms();
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

    /// 写后验证闸门给 `turn_completed` 加了 `unverified`：FF-104 要求旧事件的载荷
    /// 键集合一个都不能多，所以 `false` 必须不落键、`true` 才出现（04 §1.2 写的就是
    /// `unverified?: bool`，缺省即 false）。两个方向都钉住，别让 skip 被顺手改回去。
    #[test]
    fn turn_completed_only_carries_unverified_when_it_is_true() {
        let keys = |msg: EventMsg| -> std::collections::BTreeSet<String> {
            serde_json::to_value(msg).unwrap().as_object().unwrap().keys().cloned().collect()
        };
        let clean = keys(EventMsg::TurnCompleted {
            turn: 1,
            reason: TurnEndReason::Completed,
            error: None,
            unverified: false,
        });
        assert_eq!(
            clean,
            ["type", "turn", "reason"].into_iter().map(str::to_string).collect(),
            "false 时不得落键：旧消费端的严格键集合校验（FF-104）不能因 additive 字段被打红"
        );

        let dirty = keys(EventMsg::TurnCompleted {
            turn: 1,
            reason: TurnEndReason::Completed,
            error: None,
            unverified: true,
        });
        assert!(
            dirty.contains("unverified"),
            "true 必须落键，否则「改过源码但没验证」的披露窗口丢失：{dirty:?}"
        );
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

    /// 回执能力随 `ipc` 走：`scoped()` 只换信封不换通道，子代理与 root 判定必须一致；
    /// TauriEventSink 不自己拍脑袋，一律委托注入的 ipc。默认实现保持旧行为（等回执）。
    #[test]
    fn responder_state_is_delegated_and_inherited_by_scoped_sink() {
        struct UnreachableIpc;
        impl IpcEmitter for UnreachableIpc {
            fn emit(&self, _: &AgentEvent) -> Result<(), String> {
                Ok(())
            }

            fn responder(&self) -> ResponderState {
                ResponderState::NoWebview
            }
        }

        let sink = TauriEventSink::for_test(Arc::new(UnreachableIpc));
        assert_eq!(sink.responder(), ResponderState::NoWebview, "root 必须用注入的判据");
        let child = sink.scoped("01CHILD", Some("root")).expect("子代理出口");
        assert_eq!(
            child.responder(),
            ResponderState::NoWebview,
            "子代理没有独立回执通道，判定必须跟 root 一致"
        );

        // 默认实现：测试替身 / NullEventSink 维持旧行为，否则既有用例会被误判成拒绝。
        assert_eq!(NullEventSink.responder(), ResponderState::Ready);
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
