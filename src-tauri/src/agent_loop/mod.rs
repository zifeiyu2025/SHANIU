//! turn 主循环（docs §8 agent_loop/mod.rs，ADR-001）：
//! 请求模型（流式）→ 收集 tool calls → 执行 → 结果回填 → 再请求。
//!
//! 单一职责边界：
//! - 模型流经 `ModelBackend` 注入（真实 Responses 实现 + 测试脚本实现）；
//! - 工具执行经 `ToolRouter`（M2 起在路由层前置审批裁决）；
//! - 事件全部经 `EventSink` 广播；取消经 `CancellationToken`（drop 模型流断开连接）。
//!
//! 每个任务 = 若干 turn；turn 内无工具调用即任务完成（completed）。

pub mod anomaly;
pub mod budget;
pub mod error_codes;
pub mod instance;
pub mod prompt;
pub mod repair;
pub mod rules;
pub mod verify;

use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::clock::now_ms;
use crate::config::AppConfig;
use crate::events::{EventMsg, ToolCallResult};
use crate::model_client::{
    self, provider::ProviderConfig, HistoryItem, ModelStreamEvent, ResponseItem, Role, Usage,
};
use crate::permission::{policy, ApprovalBroker};
use crate::tool_router::{PlanState, ToolContext, ToolRouter};

pub use budget::{Exhaustion, TreeBudget, TurnBudget};
pub use instance::{AgentInstance, TokenTotals, MAX_AGENT_DEPTH};

// ============================== 模型后端抽象 ==============================

/// 模型流（转发 model_client 的别名）。
pub use crate::model_client::ModelStream;

/// 模型后端：把 (历史, 工具声明, 系统提示) 变成模型事件流。
///
/// `instructions` 每个任务只组装一次（`prompt::SessionPrompt`，docs/04 §6.1），
/// 不写入 history，避免逐轮重复累积；会话内的易变量以尾部 system 消息进 history。
#[async_trait::async_trait]
pub trait ModelBackend: Send + Sync {
    async fn stream(
        &self,
        history: &[HistoryItem],
        tools: &[ToolDeclarationShim],
        instructions: &str,
    ) -> Result<ModelStream, crate::model_client::ModelError>;
}

/// 避免 agent_loop 直接耦合 model_client 的声明类型（转发即可，无重复定义）。
pub use crate::model_client::ToolDeclaration as ToolDeclarationShim;

/// 真实后端：Responses SSE（密钥由调用方从 SecretStore 解密后注入）。
pub struct ResponsesBackend {
    pub provider: ProviderConfig,
    pub api_key: String,
}

#[async_trait::async_trait]
impl ModelBackend for ResponsesBackend {
    async fn stream(
        &self,
        history: &[HistoryItem],
        tools: &[ToolDeclarationShim],
        instructions: &str,
    ) -> Result<ModelStream, crate::model_client::ModelError> {
        let stream =
            model_client::stream_model(&self.provider, &self.api_key, history, tools, instructions)
                .await?;
        Ok(Box::pin(stream))
    }
}

// ============================== 结束前否决点（SH-121②） ==============================

/// 结束前否决点的只读上下文。
pub struct StopContext<'a> {
    /// 本轮为止的完整历史（不含本次助手文本）。
    pub history: &'a [HistoryItem],
    /// 本次助手文本（模型说要结束的那段话）。
    pub last_text: &'a str,
    /// 当前轮次（从 1 开始）。
    pub turn: u32,
    /// 计划态是否仍然开启（模型可能忘了 `exit_plan_mode`）。
    pub plan_active: bool,
}

/// 结束前否决点（对齐 codex `turn.rs:824-842` 的 stop hook 语义）。
///
/// 返回 `Some(提醒)` 表示**先别结束**：提醒作为独立 `Role::System` 条目注入历史后
/// 再跑一轮；返回 `None` 表示允许结束。
///
/// 为什么必须由宿主判定：模型文本里的「已完成」只是文本，不能终止循环
/// （SH-121① 已保证「只有派发了工具调用才继续」）。否决点是反向的同一个原则——
/// 「只有宿主说可以停，才停」。每次任务最多否决一次，避免变成无限续跑。
pub trait StopHook: Send + Sync {
    fn before_stop(&self, ctx: StopContext<'_>) -> Option<String>;
}

/// 默认否决点：计划态还开着、模型却只用文字收尾。
///
/// 这时循环继续一轮并提示提交计划——否则用户看到的是「计划态静默悬空」，
/// 下一轮对话又回到计划态，白烧一次往返。
struct PlanModeStopHook;

impl StopHook for PlanModeStopHook {
    fn before_stop(&self, ctx: StopContext<'_>) -> Option<String> {
        if !ctx.plan_active {
            return None;
        }
        Some(
            "宿主检查：计划态仍未退出，而本轮没有调用任何工具。\
             如果计划已成形，请调用 exit_plan_mode 提交；否则继续执行当前步骤，不要只用文字结束。"
                .to_string(),
        )
    }
}

/// 结束前否决额度（方案 C）：计划 1 次 + 验证 1 次，硬顶 2。
///
/// 为什么不把「防循环」交给被调用的钩子（codex 的 `stop_hook_active` 做法）：
/// 本闸门是宿主内建逻辑，不是第三方脚本；硬顶写在宿主才不会被一次配置失误
/// 变成无限续跑。
#[derive(Default)]
struct VetoLedger {
    plan_used: bool,
    verify_used: bool,
}

/// 评测消融开关（与 `SHANIU_EVAL_ABLATE_AR6` / `SHANIU_EVAL_ABLATE_REPAIR` 同口径）：
/// `SHANIU_EVAL_ABLATE_VERIFY=1` 关掉写后验证闸门。
///
/// 只有一个入口，装配期调用一次：闸门是否生效必须是**构造期常量**，不能每轮去读
/// 进程环境——同一进程里并发跑两个会话时，环境变量会把两臂的语义搅在一起。
fn apply_eval_ablation(verify: &mut crate::config::VerifyConfig) {
    if std::env::var("SHANIU_EVAL_ABLATE_VERIFY")
        .ok()
        .is_some_and(|value| value.trim() == "1")
    {
        verify.enabled = false;
    }
}

// ============================== 循环本体 ==============================

pub struct AgentLoop {
    pub backend: Arc<dyn ModelBackend>,
    pub router: Arc<ToolRouter>,
    pub broker: Arc<ApprovalBroker>,
    pub plan_state: Arc<PlanState>,
    pub workspace_root: PathBuf,
    pub max_turns: u32,
    /// 单会话 token 预算（04 §4 `[agent].token_budget`）：每轮采样前据此判定是否压缩。
    /// 0 = 不启用压缩（`compaction::plan_if_needed` 对 0 直接返回 None）。
    pub token_budget: usize,
    /// 树级预算账本（SH-055，02 ADR-103）：root 实例直接用这本账，派生出的子代理
    /// 克隆同一句柄，于是「任一代理触顶 → 全树不再发起新模型请求」由同一个账本保证。
    /// 默认 `unlimited()`：本单元只做共账接线，配置面与 `spawn_agent.budget_tokens`
    /// 的限额派生在下一单元收口。
    pub tree_budget: TreeBudget,
    /// 会话策略句柄（SH-120）：每个 turn 开头取一次快照，因此审批档的切换
    /// 从下一个 turn 起生效，而不是改一个进程级全局开关（ADR-007）。
    pub policy: Arc<policy::SessionPolicyHandle>,
    /// 会话档案写入器（SH-030）。None = 本次运行不落盘（测试与无目录启动用）。
    /// 这是 `run_with_history` 构造 root 实例时的默认值；`run_instance` 以实例自带的
    /// 写入器为准，子代理因此能各写各的档案（SH-050）。
    pub rollout: Option<Arc<crate::rollout::RolloutWriter>>,
    /// 落盘失败只播报一次：磁盘满时每条历史都报一遍会把事件流刷满，却没有任何新信息。
    /// 与其余字段一样公开——`AgentLoop` 是纯数据结构，测试与装配层都按字面量构造它。
    pub rollout_broke: std::sync::atomic::AtomicBool,
    /// 会话 id（SH-089）：后台作业的属主；装配层通过 `with_jobs` 注入。
    /// 实例没自带 `session_id` 时沿用它，root 的既有行为不变。
    pub session_id: Option<String>,
    /// 进程级作业注册表（SH-089）。
    pub jobs: Option<Arc<crate::jobs::JobRegistry>>,
    /// 结束前否决点（SH-121②）。`None` = 不否决（测试与纯后端路径的默认）。
    /// 生产装配在 [`AgentLoop::from_config`] 注入 [`PlanModeStopHook`]。
    pub stop_hook: Option<Arc<dyn StopHook>>,
    /// 写后验证闸门配置（`04 §4 [verify]`）。缺段 = 默认开启 + enforce。
    ///
    /// 该配置只影响宿主侧判定，不改变任何权限档：写入是否经过审批、是否被拒绝，
    /// 与「有没有置脏」是两件事——脏标记只认**真的落盘且内容变了**的写入。
    pub verify: crate::config::VerifyConfig,
    /// 模型请求重试策略（`04 §4 [model].max_retries`）：只决定“能不能再试、等多久”，
    /// 事件与取消由采样循环负责。测试一律 [`crate::model_client::RetryPolicy::disabled`]，
    /// 保证既有脚本后端的行为不变。
    pub retry: crate::model_client::RetryPolicy,
}

/// 档案写入的连续失败阈值：同一条目重试这么多次仍失败，视为档案不可写。
///
/// 不看单次抖动（杀毒软件短暂占用文件句柄可以自愈），只看"同一条目连续失败"——
/// 磁盘满/权限错这类持续性故障会在第一次 flush 就触顶。
const MAX_ROLLOUT_WRITE_ATTEMPTS: u32 = 3;

/// 把 `history[*recorded..]` 逐条追加进档案，成功一条推进一格。
///
/// **失败时游标停在失败条目前**（审计 H3）：已成功写入的前缀不回退，失败条目与它之后
/// 的条目也不会被误记为已落盘。原先无条件 `*recorded = history.len()` 会让这些条目
/// 永远不再写入，resume 时历史凭空少一段——档案看起来完整，实际与运行态分叉。
///
/// 同一条目连续失败达到 [`MAX_ROLLOUT_WRITE_ATTEMPTS`] → 返回硬错误，由调用方中止本轮；
/// 不再"发一次 Error 事件继续跑"。
fn append_pending<F>(
    history: &[HistoryItem],
    recorded: &mut usize,
    mut append: F,
) -> Result<(), String>
where
    F: FnMut(&crate::rollout::RolloutItem) -> Result<(), crate::rollout::RolloutError>,
{
    while *recorded < history.len() {
        let entry = crate::rollout::RolloutItem::ResponseItem {
            item: history[*recorded].clone(),
            // 落档时刻即事件发生时刻：导出/回放/搜索都按它排序（A0-4）。
            at_ms: now_ms(),
        };
        let mut last_error = None;
        for _ in 0..MAX_ROLLOUT_WRITE_ATTEMPTS {
            match append(&entry) {
                Ok(()) => {
                    last_error = None;
                    break;
                }
                Err(e) => last_error = Some(e),
            }
        }
        if let Some(e) = last_error {
            return Err(format!(
                "会话档案写入失败（同一条目连续 {MAX_ROLLOUT_WRITE_ATTEMPTS} 次），已停止本轮：{e}"
            ));
        }
        *recorded += 1;
    }
    Ok(())
}

/// 写一条压缩锚点；连续失败即返回 `Err`，由调用方**放弃本次压缩**（不改内存历史）。
///
/// 抽成接收闭包的形式是为了让失败路径可测：`RolloutWriter` 持有已打开的文件句柄，
/// 单测里没法靠改文件权限让 `append` 失败（打开着的句柄不受只读属性影响）。
///
/// 重试口径与 [`append_pending`] 一致：只滤瞬时占用，连续失败视为档案不可写。
fn append_anchor<F>(
    target: crate::compaction::Target,
    summary: &str,
    mut append: F,
) -> Result<(), String>
where
    F: FnMut(&crate::rollout::RolloutItem) -> Result<(), crate::rollout::RolloutError>,
{
    let anchor = crate::rollout::RolloutItem::Compacted {
        range_start: target.range_start,
        range_end: target.range_end,
        summary: summary.to_string(),
    };
    let mut last_error = None;
    for _ in 0..MAX_ROLLOUT_WRITE_ATTEMPTS {
        match append(&anchor) {
            Ok(()) => {
                last_error = None;
                break;
            }
            Err(error) => last_error = Some(error),
        }
    }
    match last_error {
        None => Ok(()),
        Some(error) => Err(format!(
            "会话档案写入失败（连续 {MAX_ROLLOUT_WRITE_ATTEMPTS} 次），本次压缩未生效：{error}"
        )),
    }
}

/// 把 `[target.range_start, target.range_end)` 换成一条 **user** 摘要消息。
///
/// 与 `rollout::resume` 重放 `Compacted` 锚点时的公式必须逐项一致——
/// `memory_compaction_matches_resume_replay` 用例把两边对起来，改动任何一侧都会红。
fn replace_with_summary(
    history: &mut Vec<HistoryItem>,
    target: crate::compaction::Target,
    summary: &str,
) {
    history.splice(
        target.range_start..target.range_end,
        // 摘要必须是 **user** 而不是 assistant。两个理由各自独立、都成立：
        // 1. thinking 档端点要求重放的每条 assistant 消息都带回 `reasoning_content`
        //    （SH-083），而摘要没有 reasoning——以 assistant 身份进历史就直接 400。
        //    这不是理论风险：1500 预算的真机基线里 `search-and-report` 就是这么挂的。
        // 2. 摘要不是"模型说过的话"，而是交给下一个模型的交接指令——语义上就是 user 侧输入。
        //    codex 同此：`CompactionSummary::role()` 返回 "user"。
        // 注意这并不违反 04 §6.1 不变量③：那条禁止的是把摘要塞进 **system**。
        [HistoryItem::Message { role: Role::User, text: summary.to_string(), reasoning: None }],
    );
}

/// 把任务开始时的环境快照插到**最后一条用户消息之前**。
///
/// 顺序理由：快照（时间 / Git）是"交底"，读起来应该跟着任务走；直接追加到用户消息之后
/// 会形成 `user → system` 的奇怪收尾。找不到用户消息（纯工具链历史等）时追加到末尾——
/// 位置差一点可以接受，把快照丢掉不行。
fn insert_prompt_note(history: &mut Vec<HistoryItem>, text: String) {
    let at = history
        .iter()
        .rposition(|item| matches!(item, HistoryItem::Message { role: Role::User, .. }))
        .unwrap_or(history.len());
    history.insert(at, HistoryItem::Message { role: Role::System, text, reasoning: None });
}

impl AgentLoop {
    /// 注入会话作用域与作业注册表（SH-089）；装配层在构造后调用。
    pub fn with_jobs(
        mut self,
        session_id: impl Into<String>,
        jobs: Arc<crate::jobs::JobRegistry>,
    ) -> Self {
        self.session_id = Some(session_id.into());
        self.jobs = Some(jobs);
        self
    }

    /// 接管共享的策略句柄（SH-120）：装配层需要它才能在任务运行中切换审批档，
    /// 并让切换从下一个 turn 起生效（见 [`policy::SessionPolicyHandle`]）。
    pub fn with_policy(mut self, policy: Arc<policy::SessionPolicyHandle>) -> Self {
        self.policy = policy;
        self
    }

    /// 把 `history` 里尚未落盘的尾部条目补写进档案。
    ///
    /// 返回 `Err` = 档案已不可写（连续失败触顶），调用方必须中止本轮并标 Failed——
    /// 继续跑只会让内存历史与档案进一步分叉。游标推进规则见 [`append_pending`]。
    fn flush_history(
        &self,
        rollout: &Option<Arc<crate::rollout::RolloutWriter>>,
        history: &[HistoryItem],
        recorded: &mut usize,
    ) -> Result<(), String> {
        let Some(writer) = rollout else {
            *recorded = history.len();
            return Ok(());
        };
        append_pending(history, recorded, |entry| writer.append(entry))
    }

    /// 就地压缩一段历史：发一次不带工具的总结请求 → 写 `Compacted` 锚点 → 用摘要替换区间。
    ///
    /// 三件事的顺序是硬性的：
    /// 1. **先落锚点、再改内存**。锚点的语义是「重放出的 history 里 `[start,end)` 区间被
    ///    summary 取代」，所以它必须写在那段条目**之后**——`recorded >= target.range_end`
    ///    由「每轮末尾都 flush」保证，锚点紧随其后，回放时正好接在那些条目之后生效。
    ///    顺序反了就会出现锚点指向尚未写入的条目，resume 直接报区间越界。
    /// 2. **锚点写失败 = 本次压缩不发生**。历史与游标都保持原样，返回
    ///    `ArchiveUnavailable`：调用方只让本轮不再重试，不把整个回合判失败。
    ///    反过来「内存压了、档案没压」会让重启后的历史与用户看到的结果对不上，
    ///    而档案与运行态分叉正是必须避免的那一类问题。
    ///    （旧实现把这次 append 的失败静默吞掉，注释还声称「已由 `flush_history` 播报」；
    ///    这条 append 根本不经过 `flush_history`，那句话是错的。）
    /// 3. **压缩完必须把落盘游标收到新历史末尾**。`flush_history` 按
    ///    `history[*recorded..]` 切片，而压缩让 history 变短了：不收游标就是 start > len
    ///    直接 panic，而且是只在长会话跑到第 N 轮才炸的那种 panic。
    async fn compact_in_place(
        &self,
        rollout: &Option<Arc<crate::rollout::RolloutWriter>>,
        history: &mut Vec<HistoryItem>,
        recorded: &mut usize,
        target: crate::compaction::Target,
        instructions: &str,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<CompactionStats, CompactAbort> {
        let request = crate::compaction::build_request(history, &target);
        // 总结请求不带工具声明：它不需要调工具，带上只会让端点多算一份声明开销，
        // 并给模型一个"还可以继续查"的错误暗示。
        // 建连阶段也参与取消竞争：端点不回响应头时，只竞争流上的 `next_item` 等于
        // 「停止按钮对压缩无效」，而压缩正占着应用级任务槽——那会把整个应用挂住。
        // 与主循环建连处的取消竞争同一形态（`cancel_while_opening_stream_stops_promptly`）。
        let opening = self.backend.stream(&request, &[], instructions);
        let mut stream = tokio::select! {
            _ = cancel.cancelled() => return Err(CompactAbort::Cancelled),
            result = opening => result
                .map_err(|e| CompactAbort::Failed(format!("压缩请求发起失败，本轮中止：{e}")))?,
        };
        let mut summary = String::new();
        // P0-2：压缩流也必须收到终态标记才算成功；中途 `Failed`/`Incomplete`/静默 EOF
        // 都不得拿半截摘要去替换历史。
        let mut finished = false;
        loop {
            let mut stream_pin = stream.as_mut();
            let next_item = stream_pin.next();
            tokio::select! {
                _ = cancel.cancelled() => return Err(CompactAbort::Cancelled),
                item = next_item => match item {
                    None => break,
                    Some(ModelStreamEvent::OutputTextDelta { delta }) => summary.push_str(&delta),
                    Some(ModelStreamEvent::Completed { .. }) | Some(ModelStreamEvent::Finished) => {
                        finished = true;
                    }
                    Some(ModelStreamEvent::Incomplete { reason }) => {
                        return Err(CompactAbort::Failed(
                            crate::model_client::ModelError::incomplete(&reason).human,
                        ));
                    }
                    Some(ModelStreamEvent::Failed { message, error }) => {
                        return Err(CompactAbort::Failed(
                            error.map(|error| error.human).unwrap_or(message),
                        ));
                    }
                    // 压缩请求不该产出工具调用；真产出了也只能忽略——它不是本轮的答案
                    Some(ModelStreamEvent::ReasoningDelta { .. })
                    | Some(ModelStreamEvent::OutputItem(_)) => {}
                }
            }
        }
        if !finished {
            return Err(CompactAbort::Failed(
                "压缩流在完成标记前结束，已放弃本次压缩".to_string(),
            ));
        }
        let summary = crate::compaction::extract_summary(&summary).map_err(CompactAbort::Failed)?;
        // 摘要之外先原样保留用户原文：模型漏抄一条硬约束，接手方就永远看不到。
        // 仍然是单字符串、单条 `Compacted` 锚点，档案结构不变。
        let summary =
            crate::compaction::compose_summary(history.as_slice(), &target, self.token_budget, &summary);
        // 先记账再动历史：区间被 splice 掉之后就再也算不出「丢了哪些、多少 token」。
        let shadowed_items = target.range_end - target.range_start;
        let shadowed_tokens =
            crate::compaction::history_tokens(&history[target.range_start..target.range_end]);

        if let Some(writer) = rollout {
            // 先落锚点、再改内存（顺序见上面第 1 点）。写不进去就**不提交**：
            // 此刻 history 与 recorded 都还没动，回放与运行态继续一致。
            append_anchor(target, &summary, |item| writer.append(item))
                .map_err(CompactAbort::ArchiveUnavailable)?;
        }

        replace_with_summary(history, target, &summary);
        *recorded = history.len();
        let history_tokens_after = crate::compaction::history_tokens(history);
        tracing::info!(
            compressed_items = shadowed_items,
            history_len = history.len(),
            "上下文已压缩（SH-032）"
        );
        Ok(CompactionStats {
            summary,
            shadowed_items: u32::try_from(shadowed_items).unwrap_or(u32::MAX),
            shadowed_tokens: u32::try_from(shadowed_tokens).unwrap_or(u32::MAX),
            history_tokens_after: u32::try_from(history_tokens_after).unwrap_or(u32::MAX),
        })
    }
}

/// 一次成功压缩的产出：用来填 `compaction_summarized / ended` 的载荷。
struct CompactionStats {
    summary: String,
    shadowed_items: u32,
    shadowed_tokens: u32,
    history_tokens_after: u32,
}

/// 压缩中止的三种性质。
///
/// 取消不能当失败：用户主动停止被记成「本轮失败」，就会被错误统计、错误提示，
/// 也和后端其它取消路径（`TurnEndReasonLocal::Cancelled`）不一致。
///
/// 档案不可写也不能当失败（`Failed` 会中止整个回合）：其余落盘失败走的是降级
/// （`flush_history` → `degrade_rollout`），压缩这一条若把回合判死，同一类故障
/// 在两条路径上就有了相反的后果。
enum CompactAbort {
    Cancelled,
    Failed(String),
    ArchiveUnavailable(String),
}

impl CompactAbort {
    fn message(&self) -> String {
        match self {
            Self::Cancelled => "压缩被用户取消".to_string(),
            Self::Failed(text) | Self::ArchiveUnavailable(text) => text.clone(),
        }
    }
}

impl AgentLoop {
    pub fn from_config(
        backend: Arc<dyn ModelBackend>,
        router: Arc<ToolRouter>,
        broker: Arc<ApprovalBroker>,
        plan_state: Arc<PlanState>,
        cfg: &AppConfig,
        workspace_root: PathBuf,
        rollout: Option<Arc<crate::rollout::RolloutWriter>>,
    ) -> Self {
        Self {
            backend,
            router,
            broker,
            plan_state,
            workspace_root,
            max_turns: TurnBudget::from_config(cfg).max_turns,
            // 压缩触发点（SH-132）：能力声明的 `context_window` 一旦已知，就不能再让
            // 固定预算吃到底——128K 窗口配 500K 预算等于永远不压缩，等端点溢出才报错。
            // 窗口未知时同样不能退化成「50 万才压」：过去 `window == 0` 直接返回配置预算，
            // 配置里 context_window 缺省即 0，压缩于是永不触发（2026-11-10 修复）。
            token_budget: {
                let window = cfg.effective_model().context_window as usize;
                if window == 0 && cfg.agent.token_budget != 0 {
                    tracing::warn!(
                        configured = cfg.agent.token_budget,
                        fallback_window = crate::compaction::FALLBACK_CONTEXT_WINDOW,
                        "模型未声明 context_window：压缩阈值按保守窗口推导，请在模型行补全该字段"
                    );
                }
                crate::compaction::compact_token_budget(cfg.agent.token_budget, window)
            },
            tree_budget: TreeBudget::unlimited(),
            policy: Arc::new(policy::SessionPolicyHandle::new(policy::SessionPolicy {
                approval: cfg.approval.mode,
                sandbox: cfg.sandbox.mode,
            })),
            rollout,
            rollout_broke: std::sync::atomic::AtomicBool::new(false),
            session_id: None,
            jobs: None,
            stop_hook: Some(Arc::new(PlanModeStopHook)),
            verify: {
                let mut verify = cfg.verify.clone();
                apply_eval_ablation(&mut verify);
                verify
            },
            // 手写配置把次数调大只会让一次失败拖成几分钟（500ms→8s 累加），封顶 8。
            retry: crate::model_client::RetryPolicy::new(cfg.model.max_retries.min(8)),
        }
    }

    /// 执行一个完整任务（若干 turn）。失败/取消以 TurnCompleted 事件收口，
    /// 同时返回 TurnSummary（永不 panic、错误不静默）。
    pub async fn run_task(
        &self,
        task: &str,
        sink: Arc<dyn crate::events::EventSink>,
        cancel: CancellationToken,
    ) -> TurnSummary {
        self.run_task_with_orchestrator(task, sink, cancel, None).await
    }

    /// SH-052：root 任务带编排能力启动；子代理与既有调用走 [`Self::run_task`]（None）。
    pub async fn run_task_with_orchestrator(
        &self,
        task: &str,
        sink: Arc<dyn crate::events::EventSink>,
        cancel: CancellationToken,
        orchestrator: Option<Arc<dyn crate::tool_router::orchestration::AgentOrchestrator>>,
    ) -> TurnSummary {
        let history = vec![HistoryItem::Message { role: Role::User, text: task.to_string(), reasoning: None }];
        self.run_with_history_and_orchestrator(history, 0, sink, cancel, orchestrator).await
    }

    /// 从既有历史续跑（SH-031 resume/fork）。
    ///
    /// `persisted` = 该历史里**已经落盘**的条数（resume 传 `resumed.history.len()`，
    /// 新会话传 0）。传错就会把整段旧历史再写一遍，档案里出现重复条目——这是
    /// 只有读的时候才会发现的错，所以做成显式参数而不是猜。
    ///
    /// 历史合法性由调用方负责：`rollout::resume` 重放出来的历史已按配对规则截断，
    /// 这里刻意不再"修"历史——静默补配对会掩盖档案损坏，而档案损坏必须由上层显式处理。
    pub async fn run_with_history(
        &self,
        history: Vec<HistoryItem>,
        persisted: usize,
        sink: Arc<dyn crate::events::EventSink>,
        cancel: CancellationToken,
    ) -> TurnSummary {
        self.run_with_history_and_orchestrator(history, persisted, sink, cancel, None).await
    }

    pub async fn run_with_history_and_orchestrator(
        &self,
        history: Vec<HistoryItem>,
        persisted: usize,
        sink: Arc<dyn crate::events::EventSink>,
        cancel: CancellationToken,
        orchestrator: Option<Arc<dyn crate::tool_router::orchestration::AgentOrchestrator>>,
    ) -> TurnSummary {
        let mut instance = AgentInstance::root(history, persisted, cancel);
        instance.rollout = self.rollout.clone();
        // root 用引擎的树级账本；子代理由注册表在派生时挂同一个句柄（SH-055）。
        instance.budget = self.tree_budget.clone();
        self.run_instance_with_orchestrator(&mut instance, sink, orchestrator).await
    }

    /// 在一个代理实例上执行完整任务（SH-050）。
    ///
    /// root 与子代理都走这里：`self` 是共享执行引擎（后端/路由/审批/沙箱/预算），
    /// `instance` 携带本代理独有的可变状态。执行结束后 `instance.history` 是完整历史、
    /// `instance.recorded` 是已落盘游标——子代理的 ExitSummary 与 resume 都从这里取。
    pub async fn run_instance(
        &self,
        instance: &mut AgentInstance,
        sink: Arc<dyn crate::events::EventSink>,
    ) -> TurnSummary {
        self.run_instance_with_orchestrator(instance, sink, None).await
    }

    /// SH-052：带编排能力的实例执行入口（root 注入，子代理为 None）。
    pub async fn run_instance_with_orchestrator(
        &self,
        instance: &mut AgentInstance,
        sink: Arc<dyn crate::events::EventSink>,
        orchestrator: Option<Arc<dyn crate::tool_router::orchestration::AgentOrchestrator>>,
    ) -> TurnSummary {
        // 工具上下文需要的身份快照（此刻 instance 尚未被可变解构）。
        let scope = crate::tool_router::orchestration::AgentScope {
            agent_id: instance.agent_id.clone(),
            parent_id: instance.parent_id.clone(),
            depth: instance.depth,
            cancel: instance.cancel.clone(),
            sink: sink.clone(),
            // 父侧有效工具集（SH-054）：派生时与子侧声明取交集，防止提权。
            allowed_tools: instance.allowed_tools.clone(),
        };
        let inbox = instance.inbox.clone();
        // 树级预算句柄（SH-055）：与 root/子代理共享同一本账，解构前先取出来。
        let budget = instance.budget.clone();
        // 只解构本函数要用的字段：身份/深度留给注册表与后续工具上下文（SH-051）。
        // `allowed_tools`/`task`（SH-054）是审批冒泡与权限合成的输入，执行期只读。
        let AgentInstance {
            cancel,
            history,
            recorded,
            rollout,
            session_id,
            usage: totals,
            allowed_tools,
            task,
            ..
        } = instance;
        let cancel = &*cancel;
        let history = &mut *history;
        let recorded = &mut *recorded;
        let rollout = &*rollout;
        // 每次工具门禁都要的代理身份（ADR-103）：白名单 + 审批来源。
        let gate_subject = crate::permission::GateSubject {
            agent_id: &scope.agent_id,
            task_summary: task.as_deref(),
            allowed_tools: allowed_tools.as_ref(),
            // 工具级档位逐次取（ADR-107）：MCP 工具的风险来自用户批准记录。
            risk_override: None,
        };
        // 实例没指定作业属主时沿用引擎的会话 id（root 的既有行为）。
        let session_id = session_id.clone().or_else(|| self.session_id.clone());
        // SH-124：本次任务共享一份读状态登记表——read_file 记录，写工具落盘前校验。
        let read_registry = Arc::new(crate::tool_router::ReadRegistry::new());
        // SH-126：本次任务共享一份写日志——写工具登记，自愈闭环据此判定「是否靠改测试变绿」。
        let write_journal = Arc::new(crate::tool_router::WriteJournal::new());
        // SH-072：本次任务共享一份图片预算——view_image 的张数/字节上限按会话累计。
        let image_budget = Arc::new(std::sync::Mutex::new(
            crate::tool_router::ImageBudget::default(),
        ));
        // 自上次失败以来发生过的写入（恢复窗口内），命令重新通过时结算。
        let mut pending_writes: Vec<crate::tool_router::FileWrite> = Vec::new();
        // SH-125：失败自愈状态机（每任务一份，跨 turn 累积重试次数与错误签名）。
        let mut repair =
            repair::RepairController::new(repair::DEFAULT_REPAIR_ATTEMPTS, self.workspace_root.clone());
        // 评估消融开关（与 `SHANIU_EVAL_ABLATE_AR6` 同口径）：设 1 = 关闭失败自愈闭环。
        // 没有这个开关就量不出 SH-125 的成本——「跑通带 bug 工程」这类任务的成功率、
        // 轮数与 token 差异，只有在同日同端点的两臂对照里才有意义（docs/04 §6.5）。
        let repair_enabled = !std::env::var("SHANIU_EVAL_ABLATE_REPAIR")
            .ok()
            .is_some_and(|value| value.trim() == "1");
        // SH-122：空转检测（连续相同工具 + 相同参数），跨 turn 累计连续计数。
        let mut anomaly = anomaly::AnomalyGuard::new();
        let mut reason = TurnEndReasonLocal::Completed;
        // 审计 A0：失败结局的结构化原因，随 `TurnCompleted` 一起收口。
        // 只有模型请求失败才填；落盘/压缩失败仍以 `Error` 事件说明。
        let mut turn_error: Option<crate::events::TurnErrorInfo> = None;
        let mut last_turn = 0u32;
        // 最近一次端点报回的上下文规模，作为压缩判据的观测侧（SH-032）。
        // 字节估算看不见推理 token 与工具声明开销，只有端点的 input_tokens 是真实值。
        let mut observed_input_tokens = 0usize;
        // 任务内冻结的系统提示（DSH `systemPromptUpdate: 'in-history'`）：只组装一次，
        // 易变量（时间 / Git / 权限档 / 计划态）走尾部 system 消息，保证前缀逐字节稳定。
        let mut session_prompt: Option<prompt::SessionPrompt> = None;
        // 本 run 已落档的系统提示词指纹。只记一次：提示词在 run 内冻结，多轮重复记录
        // 会让档案体积随轮数线性增长（二十轮无变化 = 二十份同样的几万 token）。
        let mut recorded_prompt: Option<String> = None;
        // 结束前否决点额度（SH-121② + 写后验证）：计划 1 次、验证 1 次，硬顶 2。
        //
        // 两个槽位分开记，是为了让「计划态还没退出」与「代码还没验证」各自只消费
        // 自己的额度：计划态否决之后，验证闸门仍然有票；反过来也成立。
        let mut veto_ledger = VetoLedger::default();
        // 写后验证的脏状态：只由「真的落盘且内容变了」的源码写入置脏。
        // 审批被拒的写入根本到不了写日志（工具不执行、不登记），因此天然不置脏。
        let mut dirty = verify::DirtyTracker::default();
        // 交互式终端里「按原样跑过验证命令并通过」的观测：终端写、闸门读。
        // 命中即可免跑重复命令（一编译几十秒，模型刚跑过就没必要再来一遍）。
        let verify_passes = Arc::new(crate::tool_router::VerifyPassLog::default());
        // 脏状态代次：每次「源码内容真的变了」就 +1。验证结果对某一代有效，
        // 同一代内不再重跑（模型连续两次只用文字收尾时，第二次是重复劳动）；
        // 模型改过代码 → 代次前进 → 必须重新验证，否则一次失败会把任务钉死在
        // `unverified`，而实际上它已经修好了。
        let mut dirty_epoch: u64 = 0;
        let mut verified_epoch: u64 = 0;
        // 首次置脏之前观测到的同命令失败 = 既有失败基线，避免「仓库本来就红」被算到本次改动头上。
        let mut baseline = verify::BaselineCache::default();
        // 评估消融开关在构造时落到配置里（与 REPAIR 同口径）：设 1 = 关闭写后验证闸门
        // （不跑、不拦、不发事件）。放在构造期而不是每轮读环境变量，测试才能用
        // `verify.enabled = false` 复现控制臂，不必并发改进程环境。
        let verify_enabled = self.verify.enabled;
        let emit = |sink: &Arc<dyn crate::events::EventSink>, msg: EventMsg| {
            let _ = sink.emit(&msg);
        };
        // 档案写入失败 = 降级，不是失败。
        //
        // 观测（轨迹/审计）不能决定主流程成败：磁盘满或 ACL 挡一下，用户要的是把话
        // 说完，而不是「这轮对话失败」再加一个看不出原因的错误。这里只记 warn + 发一条
        // 用户可见的降级通知（只发一次），本轮照常继续；游标停在失败条目上，磁盘恢复后
        // 下一轮会自然补齐。
        let degrade_rollout = |sink: &Arc<dyn crate::events::EventSink>, message: String| {
            tracing::warn!(
                target: "rollout",
                attempts = MAX_ROLLOUT_WRITE_ATTEMPTS,
                %message,
                "会话档案写入失败，本轮继续；轨迹可能不完整"
            );
            if !self.rollout_broke.swap(true, std::sync::atomic::Ordering::Relaxed) {
                emit(
                    sink,
                    EventMsg::RolloutDegraded {
                        message: format!("本轮轨迹可能不完整：{message}"),
                    },
                );
            }
        };

        // SH-091：技能目录在每次任务开始时发现一次。目录为空时系统提示不加技能段，
        // 且不向模型声明 `skill` 工具——避免无技能工作区的 token 开销与假能力。
        let skill_summaries = crate::skills::discover(&self.workspace_root);
        let skill_catalog = crate::skills::catalog_lines(&skill_summaries, 500);
        let has_model_skills = skill_summaries.iter().any(|skill| skill.model_invocable);

        'outer: for turn in 1..=self.max_turns {
            last_turn = turn;
            turn_error = None;
            // SH-120：策略快照按 turn 取。运行中切换审批档只影响后续 turn，
            // 不回改正在执行的这一轮——一轮之内裁决依据必须自洽。
            let turn_policy = self.policy.snapshot();
            // SH-118④：批量确认窗口按 turn 重置——上一轮的批准不延用到下一轮。
            self.broker.begin_turn(gate_subject.agent_id);
            emit(&sink, EventMsg::TurnStarted { turn });

            // SH-055 / FF-103：每次模型请求前先领名额（本轮的压缩请求也算一次采样）。
            // 触顶后**任何**代理都领不到新一轮——这就是「整树停止发起新模型请求」的执行点；
            // 已在途的请求不中断（暂停/询问用户由上层处理，账本只回答能不能继续）。
            if let Err(exhausted) = budget.claim_round() {
                let message = match exhausted {
                    Exhaustion::Tokens => format!(
                        "已达树级 token 预算上限（全树已用 {}），整树停止发起新模型请求",
                        budget.tree_tokens_spent()
                    ),
                    Exhaustion::Rounds => format!(
                        "已达树级轮数上限（{} 轮），整树停止发起新模型请求",
                        budget.rounds_started()
                    ),
                };
                emit(&sink, EventMsg::Error { message });
                reason = TurnEndReasonLocal::BudgetExhausted;
                break 'outer;
            }

            // SH-052：父代理经 send_agent_message 注入的 steering 消息在采样前排空，
            // 作为普通 user 消息进入本轮上下文。不新造事件类型：它本来就是用户侧输入。
            let steering: Vec<String> = {
                let mut inbox = inbox.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                std::mem::take(&mut *inbox)
            };
            for text in steering {
                history.push(HistoryItem::Message { role: Role::User, text, reasoning: None });
            }

            // 1) 采样（流式）：取消 → drop 流 = 断开连接
            // 系统提示**每任务只组装一次**（DSH in-history 方案）：DeepSeek 的缓存是前缀
            // 精确匹配，每轮把「当前时间」写回 system 开头等于每分钟把几万 token 历史作废。
            // 中途变化（权限档 / 隔离机制 / 计划态）仍必须让模型知道，改走尾部 system 消息。
            let prompt_ctx = prompt::PromptContext {
                workspace_root: &self.workspace_root,
                sandbox: turn_policy.sandbox,
                os: crate::sandbox::current_os_mode(),
                plan_mode: self.plan_state.is_active_for(session_id.as_deref()),
                skill_catalog: &skill_catalog,
                env: prompt::EnvironmentFacts::detect(&self.workspace_root),
            };
            if session_prompt.is_none() {
                let (assembled, note) = prompt::SessionPrompt::assemble(&prompt_ctx);
                // 快照只服务"这一轮的新任务"：历史末尾必须是**尚未落盘**的用户消息
                // （`run_task` 即 [user]；resume 由调用方先追加新用户消息）。
                // 续跑若尾部停在助手消息上（无新输入），插到 `recorded` 之前会把已落盘
                // 条目整体后移——游标之后多写一条旧条目、新快照反而永远写不进档案。
                let fresh_task = *recorded < history.len()
                    && matches!(history.last(), Some(HistoryItem::Message { role: Role::User, .. }));
                if let Some(note) = note.filter(|_| fresh_task) {
                    insert_prompt_note(history, note);
                }
                session_prompt = Some(assembled);
            }
            let session_prompt = session_prompt.as_mut().expect("上一行保证已组装");
            if let Some(note) = session_prompt.refresh(&prompt_ctx) {
                tracing::info!(
                    turn,
                    session_id = session_id.as_deref().unwrap_or("-"),
                    agent_id = %scope.agent_id,
                    "运行事实变化，已作为尾部 system 消息注入"
                );
                history.push(HistoryItem::Message { role: Role::System, text: note, reasoning: None });
            }
            let instructions = session_prompt.instructions().to_string();
            // 系统提示词入轨迹：记录点必须在**实际下发路径**上——`instructions` 就是紧接着
            // 要交给 model_client 的那一份。不要事后重新 `assemble()` 再存，那存的是
            // "应该发的内容"，一旦与实际下发分叉就再也查不出来。
            //
            // 指纹对**落盘文本**计算：rollout 写入前统一脱敏（M6 不变量），若对原文算 hash，
            // 含密钥的正常档案会在 resume 自洽校验里被误判为损坏。
            {
                let sha = crate::rollout::sha256_hex(&crate::config::redact(&instructions));
                let should_record = match &recorded_prompt {
                    None => true,
                    Some(last) if last == &sha => false,
                    Some(_) => {
                        tracing::error!(
                            target: "prompt",
                            turn,
                            session_id = session_id.as_deref().unwrap_or("-"),
                            "同一 run 内系统提示词发生变化——冻结策略被破坏，记录新版本"
                        );
                        true
                    }
                };
                if should_record {
                    // R2：resume 后新提示词与档案最后一条不同 → 合法漂移（换模型/换档位/
                    // 工作区变化），只告警，不得拒绝启动。
                    if let Some(writer) = rollout {
                        if let Some(previous) = crate::rollout::last_prompt_sha(writer.path()) {
                            if previous != sha {
                                tracing::warn!(
                                    target: "prompt",
                                    turn,
                                    session_id = session_id.as_deref().unwrap_or("-"),
                                    "系统提示词与档案记录不同（合法变化：换模型 / 换档位 / 工作区变化）"
                                );
                            }
                        }
                    }
                    let at_ms = now_ms();
                    // 先落档案、再发事件：档案写不进去时仍要发事件，让轨迹至少能看到
                    // 这一版提示词，同时由降级通知说明「档案可能不完整」。
                    if let Some(writer) = rollout {
                        let item = crate::rollout::RolloutItem::SystemPrompt {
                            text: instructions.clone(),
                            sha256: sha.clone(),
                            at_ms,
                            turn,
                        };
                        let mut last_error = None;
                        for _ in 0..MAX_ROLLOUT_WRITE_ATTEMPTS {
                            match writer.append(&item) {
                                Ok(()) => {
                                    last_error = None;
                                    break;
                                }
                                Err(e) => last_error = Some(e),
                            }
                        }
                        if let Some(e) = last_error {
                            degrade_rollout(
                                &sink,
                                format!(
                                    "系统提示词写入档案失败（连续 {MAX_ROLLOUT_WRITE_ATTEMPTS} 次）：{e}"
                                ),
                            );
                        }
                    }
                    emit(
                        &sink,
                        EventMsg::SystemPrompt {
                            text: instructions.clone(),
                            sha256: sha.clone(),
                            at_ms,
                            turn,
                            step: 0,
                        },
                    );
                    recorded_prompt = Some(sha);
                }
            }
            // 计划态下只向模型声明只读工具 + exit_plan_mode（ADR-003 双层防御第一层）
            let declarations = declarations_for(
                &self.router,
                self.plan_state.is_active_for(session_id.as_deref()),
                has_model_skills,
                scope.depth,
                orchestrator.is_some(),
                allowed_tools.as_ref(),
            );

            // 2.5) 采样前检查 token 预算（ADR-001：「每轮采样前检查 token 预算触发 compaction」）。
            // 位置必须在采样之前：压缩的产物要对本轮请求可见才有意义，放轮末等于压了个寂寞
            // ——那一轮的上下文照样发出去，溢出照旧。
            if let Some(target) = crate::compaction::plan_if_needed(
                observed_input_tokens,
                history,
                self.token_budget,
            ) {
                // 有损操作必须出声：先落「开始了」，再压；失败/取消都要落到 ended，
                // 否则用户只看到上下文变短，却没有任何时间线可对（compaction 可见性）。
                let compaction_id = crate::compaction::new_id();
                let started_at = crate::compaction::history_tokens(history);
                emit(
                    &sink,
                    EventMsg::CompactionStarted {
                        compaction_id: compaction_id.clone(),
                        trigger: crate::events::CompactionTrigger::Pressure,
                        range_start: u32::try_from(target.range_start).unwrap_or(u32::MAX),
                        range_end: u32::try_from(target.range_end).unwrap_or(u32::MAX),
                        estimated_tokens: u32::try_from(observed_input_tokens.max(started_at))
                            .unwrap_or(u32::MAX),
                        budget_tokens: u32::try_from(self.token_budget).unwrap_or(u32::MAX),
                        turn: Some(turn),
                    },
                );
                match self
                    .compact_in_place(rollout, history, recorded, target, &instructions, cancel)
                    .await
                {
                    Ok(stats) => {
                        emit(
                            &sink,
                            EventMsg::CompactionSummarized {
                                compaction_id: compaction_id.clone(),
                                trigger: crate::events::CompactionTrigger::Pressure,
                                range_start: u32::try_from(target.range_start).unwrap_or(u32::MAX),
                                range_end: u32::try_from(target.range_end).unwrap_or(u32::MAX),
                                shadowed_items: stats.shadowed_items,
                                shadowed_tokens: stats.shadowed_tokens,
                                summary: stats.summary,
                            },
                        );
                        emit(
                            &sink,
                            EventMsg::CompactionEnded {
                                compaction_id,
                                ok: true,
                                error: None,
                                history_tokens_after: Some(stats.history_tokens_after),
                            },
                        );
                    }
                    Err(abort) => {
                        let message = abort.message();
                        emit(
                            &sink,
                            EventMsg::CompactionEnded {
                                compaction_id,
                                ok: false,
                                error: Some(message.clone()),
                                history_tokens_after: None,
                            },
                        );
                        match abort {
                            // 取消按取消收口：不记 Error，也不算失败轮。
                            CompactAbort::Cancelled => {
                                reason = TurnEndReasonLocal::Cancelled;
                                break 'outer;
                            }
                            CompactAbort::Failed(_) => {
                                // P0-2：本地失败也要写明分类，前端才能显示「为什么失败」。
                                turn_error = Some(local_turn_error("COMPACTION_FAILED", message.clone()));
                                emit(&sink, EventMsg::Error { message });
                                reason = TurnEndReasonLocal::Failed;
                                break 'outer;
                            }
                            // 档案不可写：本次压缩没有发生（历史与游标未动），
                            // 与其余落盘失败同口径走降级——本轮继续，不判失败。
                            // 压缩检查每轮只做一次（turn 体内无内层循环），所以
                            // 这里不需要额外的「本轮不再重试」标记。
                            CompactAbort::ArchiveUnavailable(_) => {
                                degrade_rollout(&sink, message);
                            }
                        }
                    }
                }
            }

            // 2.7) 会话引用展开（SH-090 批 2）：只作用于本步请求副本，档案与展示历史不动。
            // 轮次内的后续采样末尾不是 user 消息，不会重复展开。
            let expanded = match rollout.as_ref() {
                Some(writer) => {
                    let dir = writer.path().parent().map(std::path::Path::to_path_buf);
                    let id = writer.path().file_stem().map(|stem| stem.to_string_lossy().into_owned());
                    match (dir, id) {
                        (Some(dir), Some(id)) => crate::session_reference::prepare_request(history, &dir, &id),
                        _ => Ok(None),
                    }
                }
                None => Ok(None),
            };
            let expanded = match expanded {
                Ok(value) => value,
                Err(message) => {
                    turn_error = Some(local_turn_error("CONTEXT_EXPAND_FAILED", message.clone()));
                    emit(&sink, EventMsg::Error { message });
                    reason = TurnEndReasonLocal::Failed;
                    break 'outer;
                }
            };
            // 2.8) 用户 `/name` 技能触发（SH-091）：只扫描原始历史末尾的用户消息，
            // 避免把会话引用展开注入的上下文当成用户手势。技能正文最后追加，
            // 离模型回答最近（与上游“背景在前、要执行的物料在后”一致）。
            let skill_injections = crate::skills::injections(history, &self.workspace_root);
            let mut request_history: Vec<HistoryItem> =
                expanded.unwrap_or_else(|| history.clone());
            if !skill_injections.is_empty() {
                request_history.extend(skill_injections);
            }
            // 2) 一次采样 = 建流 + 消费流，失败统一收敛成 `Option<ModelError>`。
            //    重试只发生在首个可见增量之前（见 `emitted_delta`）：已吐出的正文/思考
            //    不能重来，否则用户会看到两遍。
            let mut retries: u32 = 0;
            let mut retry_chain: Option<String> = None;
            let (assistant_text, mut reasoning_text, full_message, calls, usage) = 'sample: loop {
                // 每次尝试独占的状态：重试之间直接丢弃——它们没进历史、没执行工具。
                let mut emitted_delta = false;
                let mut assistant_text = String::new();
                let mut reasoning_text = String::new();
                let mut full_message: Option<String> = None;
                let mut calls: Vec<(String, String, String)> = Vec::new(); // (name, arguments, call_id)
                let mut usage: Option<Usage> = None;

                // 建立流本身也要能被取消（审计 H1）：端点迟迟不回响应头时，
                // 只在流建立之后检查取消等于"取消点永远到不了"。
                let opened = tokio::select! {
                    _ = cancel.cancelled() => {
                        reason = TurnEndReasonLocal::Cancelled;
                        break 'outer;
                    }
                    opened = self.backend.stream(&request_history, &declarations, &instructions) => opened,
                };

                // 是否收到终态标记：`Completed`（带 usage）或 `Finished`（仅 finish_reason）。
                // P0-1：没有终态标记就 EOF，说明回答只到一半，不能当成功收口。
                let mut finished = false;
                let failure = match opened {
                    Err(error) => Some(error),
                    Ok(mut stream) => {
                        let mut failure: Option<crate::model_client::ModelError> = None;
                        // 2.1) 消费流：转发增量、收集输出项
                        loop {
                            // 流空闲/首包上限（审计 H1）：每个分片都重置计时，超过它说明连接已僵死。
                            let idle = tokio::time::sleep(crate::model_client::STREAM_IDLE_TIMEOUT);
                            tokio::pin!(idle);
                            let mut stream_pin = stream.as_mut(); // Pin<&mut dyn S>，Next 借用其存活
                            let next_item = stream_pin.next();
                            tokio::select! {
                                _ = cancel.cancelled() => {
                                    reason = TurnEndReasonLocal::Cancelled;
                                    break 'outer;
                                }
                                _ = &mut idle => {
                                    failure = Some(crate::model_client::ModelError::timeout(format!(
                                        "模型流空闲超过 {}s 未收到数据，已中断",
                                        crate::model_client::STREAM_IDLE_TIMEOUT.as_secs()
                                    )));
                                    break;
                                }
                                item = next_item => match item {
                                    None => break,
                                    Some(ModelStreamEvent::OutputTextDelta { delta }) => {
                                        assistant_text.push_str(&delta);
                                        // 硬约束①：正文一吐就不再重试（重试会重复正文）。
                                        emitted_delta = true;
                                        emit(&sink, EventMsg::AgentMessageDelta { delta });
                                    }
                                    // 思考增量单独成事件：它不是答案内容，前端要能折叠显示，
                                    // FF-5 也要把它算进"首个可见反馈"（业界同一口径，见 latency_budget）
                                    Some(ModelStreamEvent::ReasoningDelta { delta }) => {
                                        reasoning_text.push_str(&delta);
                                        // 硬约束①：推理增量同样算可见输出——重试会让"思考过程"出现两遍。
                                        emitted_delta = true;
                                        emit(&sink, EventMsg::ReasoningDelta { delta });
                                    }
                                    Some(ModelStreamEvent::OutputItem(ResponseItem::Message { role: _, text })) => {
                                        full_message = Some(text);
                                    }
                                    Some(ModelStreamEvent::OutputItem(ResponseItem::FunctionCall { name, arguments, call_id })) => {
                                        calls.push((name, arguments, call_id));
                                    }
                                    Some(ModelStreamEvent::Completed { usage: u }) => {
                                        usage = Some(u);
                                        finished = true;
                                    }
                                    Some(ModelStreamEvent::Finished) => {
                                        finished = true;
                                    }
                                    // 端点明确表示没说完（max_tokens / 内容过滤）：终局失败，
                                    // 原因走分类而不是"未知错误"。
                                    Some(ModelStreamEvent::Incomplete { reason }) => {
                                        finished = true;
                                        failure = Some(crate::model_client::ModelError::incomplete(&reason));
                                        break;
                                    }
                                    Some(ModelStreamEvent::Failed { message, error }) => {
                                        // 没有结构化错误时也要留下痕迹：结局不得被当成"正常结束"。
                                        failure = Some(error.unwrap_or_else(|| crate::model_client::ModelError::from(message)));
                                        break;
                                    }
                                }
                            }
                        }
                        failure
                    }
                };

                // 空响应：流正常结束却什么都没有。顺序是硬性的——**先看 `calls`**：
                // 有工具调用说明这次采样已经产生副作用，重试会重复执行工具，绝不重试，
                // 因此 `calls` 非空时直接视为"有输出"。
                let failure = failure.or_else(|| {
                    (calls.is_empty() && !emitted_delta && full_message.is_none() && usage.is_none())
                        .then(crate::model_client::ModelError::empty_response)
                });
                // P0-1：EOF 前没有收到终态标记（`response.completed` / `finish_reason`），
                // 说明流被截断；已经吐出的半截内容只能丢弃，不能作为助手消息落历史。
                let failure = failure.or_else(|| {
                    (!finished).then(crate::model_client::ModelError::stream_truncated)
                });

                let Some(error) = failure else {
                    break 'sample (assistant_text, reasoning_text, full_message, calls, usage);
                };

                // 重试的硬约束：只在**尚未向用户交付任何内容**时重试。
                //
                // - 正文/推理增量已经 emit 过：重试会让用户看到重复内容；
                // - 已经拿到 usage：这次采样已按端点口径结算，重试会重复计费；
                // - 已经收集到工具调用：前端已经看到工具卡片，重试会重复执行工具。
                //
                // 三者任一命中就直接终局，不再走 `await_retry`。
                let outcome = if emitted_delta || usage.is_some() || !calls.is_empty() {
                    give_up_without_retry_after(&error)
                } else {
                    await_retry(&self.retry, &sink, cancel, turn, retries, &mut retry_chain, &error).await
                };
                match outcome {
                    RetryOutcome::Retry => {
                        retries += 1;
                        tracing::warn!(
                            turn,
                            retry = retries,
                            code = error.code(),
                            provider_message = error.provider_message.as_deref().unwrap_or("-"),
                            "模型请求失败，即将重试"
                        );
                        continue 'sample;
                    }
                    RetryOutcome::Cancelled => {
                        reason = TurnEndReasonLocal::Cancelled;
                        break 'outer;
                    }
                    RetryOutcome::GiveUp(final_error) => {
                        // 失败必须留下能自修的信息（A0）：一句人话进 Error，结构化字段进 TurnCompleted。
                        let shown = final_error
                            .provider_message
                            .clone()
                            .unwrap_or_else(|| final_error.human.clone());
                        emit(&sink, EventMsg::Error { message: shown });
                        let mut info: crate::events::TurnErrorInfo = (&final_error).into();
                        info.attempts = Some(retries);
                        turn_error = Some(info);
                        reason = TurnEndReasonLocal::Failed;
                        break 'outer;
                    }
                }
            };
            if let Some(u) = usage {
                // 实例级累计（ExitSummary 的 token_usage）。
                totals.absorb(&u);
                // 树级共账（SH-055）：父与全部后代记同一本账；cached 是 input 的子集，不重复计。
                budget.record_tokens(u64::from(u.input_tokens) + u64::from(u.output_tokens));
                observed_input_tokens = usize::try_from(u.input_tokens).unwrap_or(usize::MAX);
                // 命中率必须落 shaniu.log（TokenUsage 事件只进前端事件流与 events.jsonl，
                // 改提示词/压缩策略后要靠这行比对回归）。miss 由 hit+miss=input 推出：
                // DeepSeek 的 prompt_cache_miss_tokens 与 OpenAI 的 cached_tokens 都满足该恒等式。
                let cached = u.cached_input_tokens.unwrap_or(0);
                tracing::info!(
                    turn,
                    session_id = session_id.as_deref().unwrap_or("-"),
                    agent_id = %scope.agent_id,
                    input_tokens = u.input_tokens,
                    cached_input_tokens = cached,
                    cache_miss_tokens = u.input_tokens.saturating_sub(cached),
                    output_tokens = u.output_tokens,
                    cache_hit_permille =
                        if u.input_tokens == 0 { 0 } else { u64::from(cached) * 1000 / u64::from(u.input_tokens) },
                    "token 用量"
                );
                emit(&sink, EventMsg::TokenUsage {
                    turn,
                    input_tokens: u.input_tokens,
                    cached_input_tokens: u.cached_input_tokens,
                    output_tokens: u.output_tokens,
                });
            }

            // 3) 助手输出入历史（完整消息优先，增量累计兜底）
            let text = full_message.unwrap_or(assistant_text);
            // reasoning 只要出现过就必须挂到这条 assistant 消息上回传（SH-083）：
            // thinking 档端点缺该字段直接 400。哪怕正文为空也要留——留了才有下一轮。
            let reasoning = if reasoning_text.trim().is_empty() {
                None
            } else {
                Some(std::mem::take(&mut reasoning_text))
            };
            // 结束前否决点（SH-121② + 写后验证）：先算再入历史——`text` 之后会被 move。
            // 只在「确实要结束」且预算还够再跑一轮时询问。计划优先、验证次之，
            // 两者各最多一次（硬顶 2）；`turn + 1 < max_turns` 保证否决后还有一轮可用。
            let can_veto = turn + 1 < self.max_turns;
            let plan_veto = if calls.is_empty() && can_veto && !veto_ledger.plan_used {
                self.stop_hook.as_ref().and_then(|hook| {
                    hook.before_stop(StopContext {
                        history,
                        last_text: &text,
                        turn,
                        plan_active: self.plan_state.is_active_for(session_id.as_deref()),
                    })
                })
            } else {
                None
            };
            if plan_veto.is_some() {
                veto_ledger.plan_used = true;
            }
            if !text.trim().is_empty() || reasoning.is_some() {
                history.push(HistoryItem::Message { role: Role::Assistant, text, reasoning });
            }

            // 4) 无工具调用 → 结束前否决点放行/否决
            if calls.is_empty() {
                let mut reminder = plan_veto;
                // 计划态否决优先：这一轮不跑验证，脏状态留到下一轮。
                // 否则「计划提醒」会吃掉验证额度，形成饥饿。
                if reminder.is_none()
                    && can_veto
                    && verify_enabled
                    && dirty.is_dirty()
                    && dirty_epoch != verified_epoch
                {
                    verified_epoch = dirty_epoch;
                    // 通过计入优先：模型刚在终端里按原样跑过同一条命令并成功，
                    // 就没有必要再等一遍编译。判定顺序收在 `verify::evaluate` 一处。
                    let outcome = verify::evaluate(
                        &verify::VerifyRequest {
                            cfg: &self.verify,
                            dirty: &dirty,
                            passes: &verify_passes,
                            baseline: verify::baseline_enabled(&self.verify)
                                .then_some(&baseline),
                            workspace_root: &self.workspace_root,
                            epoch: dirty_epoch,
                            timeout: std::time::Duration::from_millis(
                                self.verify.clamped_timeout_ms(),
                            ),
                            sandbox: crate::config::SandboxMode::WorkspaceWrite,
                        },
                        cancel,
                    )
                    .await;
                    emit(
                        &sink,
                        EventMsg::VerificationResult {
                            turn,
                            state: outcome.state,
                            commands: outcome.commands.clone(),
                            duration_ms: outcome.duration_ms,
                            summary: outcome.summary.clone(),
                            diagnostics: outcome.diagnostics.clone(),
                        },
                    );
                    match outcome.state {
                        crate::events::VerifyState::Passed => {
                            if repair_enabled {
                                // 成功先清自愈状态，再结算「这次绿是不是靠改测试换来的」。
                                repair.observe_verification(true, "");
                                match repair.settle_verification(&pending_writes) {
                                    Some(repair::RepairAction::RejectedRepair(text)) => {
                                        turn_error = Some(local_turn_error("TEST_TAMPERING", text.clone()));
                                        emit(&sink, EventMsg::Error { message: text.clone() });
                                        history.push(HistoryItem::Message {
                                            role: Role::System,
                                            text,
                                            reasoning: None,
                                        });
                                        reason = TurnEndReasonLocal::Failed;
                                        break 'outer;
                                    }
                                    Some(repair::RepairAction::SuspiciousRepair(text)) => {
                                        emit(&sink, EventMsg::Error { message: text.clone() });
                                        history.push(HistoryItem::Message {
                                            role: Role::System,
                                            text,
                                            reasoning: None,
                                        });
                                    }
                                    _ => {}
                                }
                            }
                            dirty.clear();
                            pending_writes.clear();
                        }
                        crate::events::VerifyState::Failed => {
                            // 运行期泄压阀：warn 只发事件不否决，改配置即生效。
                            // 额度已用尽时同样只披露：验证照跑、结果照发，但不再拦。
                            if matches!(
                                self.verify.effective_mode(),
                                crate::config::VerifyMode::Enforce
                            ) && !veto_ledger.verify_used
                            {
                                let note = if repair_enabled {
                                    repair
                                        .observe_verification(false, &outcome.output)
                                        .map(|action| match action {
                                            repair::RepairAction::Nudge(text)
                                            | repair::RepairAction::GiveUp(text)
                                            | repair::RepairAction::RejectedRepair(text)
                                            | repair::RepairAction::SuspiciousRepair(text) => text,
                                        })
                                } else {
                                    None
                                };
                                // 这次失败之前的写入属于「修复前」，不能算到下一轮修复头上。
                                pending_writes.clear();
                                reminder = Some(verify::reminder_text(&outcome, note.as_deref()));
                                veto_ledger.verify_used = true;
                            }
                        }
                        // 判不出来/没配置命令：不清脏、不否决，由末轮 `unverified` 披露。
                        crate::events::VerifyState::Inconclusive(_)
                        | crate::events::VerifyState::Skipped(_) => {}
                    }
                }
                match reminder {
                    Some(reminder) => {
                        // 独立 System 条目：不伪装成工具结果、不冒充用户消息。
                        history.push(HistoryItem::Message {
                            role: Role::System,
                            text: reminder,
                            reasoning: None,
                        });
                        continue;
                    }
                    None => break,
                }
            }

            // 5) 本轮已到预算上限：不在这里 break。先让本轮已声明的工具照常执行，
            //    保证历史里的 FunctionCall / FunctionCallOutput 成对；执行完再收口。
            //    旧行为在这一步直接丢弃整批调用，模型最后一刻想跑的验证命令会消失，
            //    只留下一个没有输出的 FunctionCall 意图。

            // 6) 工具调用入历史 → 执行 → 输出回填
            // SH-122：先按派发顺序过一遍空转检测（同一工具 + 规范化参数连续第 4 次），
            // 提醒在工具输出之后以独立 System 条目注入，每轮最多 3 条。
            anomaly.begin_turn();
            let mut anomaly_reminders: Vec<String> = Vec::new();
            for (name, arguments, _) in &calls {
                if let Some(reminder) = anomaly.observe(name, arguments) {
                    anomaly_reminders.push(reminder);
                }
            }
            for (name, arguments, call_id) in &calls {
                history.push(HistoryItem::FunctionCall {
                    name: name.clone(),
                    arguments: arguments.clone(),
                    call_id: call_id.clone(),
                });
            }
            // 连续只读调用并行（第 4 笔）：读类工具互不依赖，串行 await 等于把磁盘与网络
            // 延迟逐条相加。批次内仍先**按序**过门禁（审批不能被并行绕过），再并发执行，
            // 最后按原调用顺序回写事件与 tool 输出——协议要求 tool 结果与 tool_calls
            // 一一对应，前端也按数组顺序消费，顺序不能由完成先后决定。
            //
            // 只并行「连续」的只读调用：中间夹一条写入/执行/计划/子代理就断开批次，
            // 它们会改状态，顺序就是语义。
            let mut batch_consumed = 0usize;
            for (index, (name, arguments, call_id)) in calls.iter().enumerate() {
                if index < batch_consumed {
                    continue;
                }
                if parallel_read_only(&self.router, name) {
                    let mut batch_end = index + 1;
                    while batch_end < calls.len()
                        && parallel_read_only(&self.router, &calls[batch_end].0)
                    {
                        batch_end += 1;
                    }
                    if batch_end - index >= 2 {
                        batch_consumed = batch_end;
                        let batch = &calls[index..batch_end];
                        // 阶段 1：按序发 Begin 并过门禁；通过的进 pending，被拒的当场回填。
                        let mut pending: Vec<(usize, serde_json::Value, ToolContext)> = Vec::new();
                        let mut resolved = vec![false; batch.len()];
                        let mut cancelled = false;
                        for (offset, (b_name, b_arguments, b_call_id)) in batch.iter().enumerate() {
                            emit(&sink, EventMsg::ToolCallBegin {
                                call_id: b_call_id.clone(),
                                tool: b_name.clone(),
                                args: serde_json::from_str(b_arguments)
                                    .unwrap_or(serde_json::Value::String(b_arguments.clone())),
                            });
                            if self.router.get(b_name).is_none() {
                                emit(&sink, EventMsg::ToolCallEnd {
                                    call_id: b_call_id.clone(),
                                    exit_code: None,
                                    error: Some(format!("未知工具: {b_name}")),
                                    result: Some(ToolCallResult::Error),
                                });
                                history.push(HistoryItem::FunctionCallOutput {
                                    call_id: b_call_id.clone(),
                                    output: format!("未知工具: {b_name}"),
                                });
                                resolved[offset] = true;
                                continue;
                            }
                            if self.plan_state.is_active_for(session_id.as_deref())
                                && !policy::plan_mode_allows(b_name)
                            {
                                emit(&sink, EventMsg::ToolCallEnd {
                                    call_id: b_call_id.clone(),
                                    exit_code: None,
                                    error: Some("计划模式仅只读工具可用".into()),
                                    result: Some(ToolCallResult::Error),
                                });
                                history.push(HistoryItem::FunctionCallOutput {
                                    call_id: b_call_id.clone(),
                                    output: "计划模式仅只读工具可用；请先调用 exit_plan_mode 提交计划"
                                        .into(),
                                });
                                resolved[offset] = true;
                                continue;
                            }
                            let risk_override = self
                                .router
                                .get(b_name)
                                .and_then(|executor| executor.spec().risk);
                            let call_subject =
                                crate::permission::GateSubject { risk_override, ..gate_subject };
                            let args_val = serde_json::from_str(b_arguments)
                                .unwrap_or(serde_json::Value::String(b_arguments.clone()));
                            match self
                                .broker
                                .gate(
                                    b_name,
                                    &args_val,
                                    b_call_id,
                                    turn_policy,
                                    call_subject,
                                    crate::permission::GateEnv { sink: &sink, cancel },
                                )
                                .await
                            {
                                crate::permission::GateOutcome::Allowed => {}
                                crate::permission::GateOutcome::Denied => {
                                    emit(&sink, EventMsg::ToolCallEnd {
                                        call_id: b_call_id.clone(),
                                        exit_code: None,
                                        error: Some("用户拒绝".into()),
                                        result: Some(ToolCallResult::Denied),
                                    });
                                    history.push(HistoryItem::FunctionCallOutput {
                                        call_id: b_call_id.clone(),
                                        output: "用户拒绝了该工具调用".into(),
                                    });
                                    resolved[offset] = true;
                                    continue;
                                }
                                crate::permission::GateOutcome::Forbidden => {
                                    emit(&sink, EventMsg::ToolCallEnd {
                                        call_id: b_call_id.clone(),
                                        exit_code: None,
                                        error: Some("策略拒绝：当前代理无权使用该工具".into()),
                                        result: Some(ToolCallResult::Denied),
                                    });
                                    history.push(HistoryItem::FunctionCallOutput {
                                        call_id: b_call_id.clone(),
                                        output: "该工具不在当前代理的有效工具集内（权限只降不升），不会通过审批获得；请改用可用工具或在结果中说明限制".into(),
                                    });
                                    resolved[offset] = true;
                                    continue;
                                }
                                crate::permission::GateOutcome::Cancelled => {
                                    emit(&sink, EventMsg::ToolCallEnd {
                                        call_id: b_call_id.clone(),
                                        exit_code: None,
                                        error: Some("已取消".into()),
                                        result: Some(ToolCallResult::Cancelled),
                                    });
                                    cancelled = true;
                                    break;
                                }
                            }
                            let tool_ctx = ToolContext::new(
                                self.workspace_root.clone(),
                                cancel.clone(),
                                b_call_id.clone(),
                                sink.clone(),
                                turn_policy.sandbox,
                            )
                            .with_jobs(session_id.clone(), self.jobs.clone())
                            .with_reads(read_registry.clone())
                            .with_writes(write_journal.clone())
                            .with_images(image_budget.clone())
                            .with_verify_passes(verify_passes.clone())
                            .with_scope(scope.clone());
                            let tool_ctx = match &orchestrator {
                                Some(orchestrator) => {
                                    tool_ctx.with_orchestration(scope.clone(), orchestrator.clone())
                                }
                                None => tool_ctx,
                            };
                            pending.push((offset, args_val, tool_ctx));
                        }
                        if cancelled {
                            reason = TurnEndReasonLocal::Cancelled;
                            break 'outer;
                        }
                        // 阶段 2：并发执行。取消令牌与执行同时竞争——用户按下停止后
                        // 立即放弃等待，迟到结果不采纳。
                        let futures = pending.iter().map(|(offset, args_val, ctx)| {
                            self.router.execute(&batch[*offset].0, args_val.clone(), ctx)
                        });
                        let joined = tokio::select! {
                            _ = cancel.cancelled() => None,
                            joined = futures::future::join_all(futures) => Some(joined),
                        };
                        // 阶段 3：按原调用顺序回写。取消时给尚未回填的调用补一条
                        // Cancelled，保证每条 FunctionCall 都有对应输出（协议完整）。
                        let mut results = joined.unwrap_or_default();
                        let mut done: Vec<Option<Result<crate::tool_router::ToolOutput, crate::tool_router::ToolError>>> =
                            (0..batch.len()).map(|_| None).collect();
                        // joined 为 None（取消）时是空表，长度必然对不上非空的 pending；
                        // 整批都被门禁拒掉时 pending 为空，两边同为 0——不算取消。
                        let executed = results.len() == pending.len();
                        if executed {
                            for ((offset, _, _), result) in pending.iter().zip(results.drain(..)) {
                                done[*offset] = Some(result);
                            }
                        }
                        let mut cancelled = !executed;
                        for (offset, (b_name, _, b_call_id)) in batch.iter().enumerate() {
                            if resolved[offset] {
                                continue;
                            }
                            match done[offset].take() {
                                Some(result) => {
                                    match record_tool_result(
                                        history,
                                        &sink,
                                        b_name,
                                        b_call_id,
                                        &result,
                                        turn_policy.sandbox,
                                    ) {
                                        Ok(_) => {}
                                        Err(reason_local) => {
                                            cancelled = true;
                                            reason = reason_local;
                                        }
                                    }
                                }
                                None => {
                                    emit(&sink, EventMsg::ToolCallEnd {
                                        call_id: b_call_id.clone(),
                                        exit_code: None,
                                        error: Some("已取消".into()),
                                        result: Some(ToolCallResult::Cancelled),
                                    });
                                    history.push(HistoryItem::FunctionCallOutput {
                                        call_id: b_call_id.clone(),
                                        output: "已取消".into(),
                                    });
                                }
                            }
                            if cancelled {
                                // 取消后剩余调用同样按序补齐，不留没有输出的 tool_call。
                                for (rest, (_, _, rest_id)) in
                                    batch.iter().enumerate().skip(offset + 1)
                                {
                                    if resolved[rest] {
                                        continue;
                                    }
                                    emit(&sink, EventMsg::ToolCallEnd {
                                        call_id: rest_id.clone(),
                                        exit_code: None,
                                        error: Some("已取消".into()),
                                        result: Some(ToolCallResult::Cancelled),
                                    });
                                    history.push(HistoryItem::FunctionCallOutput {
                                        call_id: rest_id.clone(),
                                        output: "已取消".into(),
                                    });
                                }
                                break;
                            }
                        }
                        if cancelled {
                            reason = TurnEndReasonLocal::Cancelled;
                            break 'outer;
                        }
                        continue;
                    }
                }
                // 未知工具 = 协议违规：不经门禁直接拒绝回填（模型自我纠正），不打扰人工
                if self.router.get(name).is_none() {
                    emit(&sink, EventMsg::ToolCallBegin {
                        call_id: call_id.clone(),
                        tool: name.clone(),
                        args: serde_json::from_str(arguments)
                            .unwrap_or(serde_json::Value::String(arguments.clone())),
                    });
                    emit(&sink, EventMsg::ToolCallEnd {
                        call_id: call_id.clone(),
                        exit_code: None,
                        error: Some(format!("未知工具: {name}")),
                        result: Some(ToolCallResult::Error),
                    });
                    history.push(HistoryItem::FunctionCallOutput {
                        call_id: call_id.clone(),
                        output: format!("未知工具: {name}"),
                    });
                    continue;
                }

                emit(&sink, EventMsg::ToolCallBegin {
                    call_id: call_id.clone(),
                    tool: name.clone(),
                    args: serde_json::from_str(arguments)
                        .unwrap_or(serde_json::Value::String(arguments.clone())),
                });

                // 计划态执行守卫（双层防御；声明已收窄，此处兜底）
                if self.plan_state.is_active_for(session_id.as_deref()) && !policy::plan_mode_allows(name) {
                    emit(&sink, EventMsg::ToolCallEnd {
                        call_id: call_id.clone(),
                        exit_code: None,
                        error: Some("计划模式仅只读工具可用".into()),
                        result: Some(ToolCallResult::Error),
                    });
                    history.push(HistoryItem::FunctionCallOutput {
                        call_id: call_id.clone(),
                        output: "计划模式仅只读工具可用；请先调用 exit_plan_mode 提交计划".into(),
                    });
                    continue;
                }

                // 审批门禁（ADR-003）：裁决 → 询问回执 → 放行/拒绝。
                // 风险档位优先取工具自身的（用户批准时指定，SH-063/ADR-107），
                // 内置工具没有该字段，回退 risk_of。
                let risk_override = self
                    .router
                    .get(name)
                    .and_then(|executor| executor.spec().risk);
                let call_subject = crate::permission::GateSubject { risk_override, ..gate_subject };
                let args_val =
                    serde_json::from_str(arguments).unwrap_or(serde_json::Value::String(arguments.clone()));
                match self
                    .broker
                    .gate(
                        name,
                        &args_val,
                        call_id,
                        turn_policy,
                        call_subject,
                        crate::permission::GateEnv { sink: &sink, cancel },
                    )
                    .await
                {
                    crate::permission::GateOutcome::Allowed => {}
                    crate::permission::GateOutcome::Denied => {
                        emit(&sink, EventMsg::ToolCallEnd {
                            call_id: call_id.clone(),
                            exit_code: None,
                            error: Some("用户拒绝".into()),
                            result: Some(ToolCallResult::Denied),
                        });
                        history.push(HistoryItem::FunctionCallOutput {
                            call_id: call_id.clone(),
                            output: "用户拒绝了该工具调用".into(),
                        });
                        continue;
                    }
                    crate::permission::GateOutcome::Forbidden => {
                        // 策略拒绝与用户拒绝分开回填：模型应换策略，而不是重试等审批。
                        emit(&sink, EventMsg::ToolCallEnd {
                            call_id: call_id.clone(),
                            exit_code: None,
                            error: Some("策略拒绝：当前代理无权使用该工具".into()),
                            result: Some(ToolCallResult::Denied),
                        });
                        history.push(HistoryItem::FunctionCallOutput {
                            call_id: call_id.clone(),
                            output: "该工具不在当前代理的有效工具集内（权限只降不升），不会通过审批获得；请改用可用工具或在结果中说明限制".into(),
                        });
                        continue;
                    }
                    crate::permission::GateOutcome::Cancelled => {
                        emit(&sink, EventMsg::ToolCallEnd {
                            call_id: call_id.clone(),
                            exit_code: None,
                            error: Some("已取消".into()),
                            result: Some(ToolCallResult::Cancelled),
                        });
                        reason = TurnEndReasonLocal::Cancelled;
                        break 'outer;
                    }
                }

                let tool_ctx = ToolContext::new(
                    self.workspace_root.clone(),
                    cancel.clone(),
                    call_id.clone(),
                    sink.clone(),
                    turn_policy.sandbox,
                )
                .with_jobs(session_id.clone(), self.jobs.clone())
                .with_reads(read_registry.clone())
                .with_writes(write_journal.clone())
                .with_images(image_budget.clone())
                .with_verify_passes(verify_passes.clone())
                .with_scope(scope.clone());
                // 只有 root（depth=0）拿到编排能力；子代理沿用 scope 但无 orchestrator，
                // 分发层会拒绝它调用编排工具（第二层深度门控）。
                let tool_ctx = match &orchestrator {
                    Some(orchestrator) => tool_ctx.with_orchestration(scope.clone(), orchestrator.clone()),
                    None => tool_ctx,
                };
                let mut result = self.router.execute(name, args_val.clone(), &tool_ctx).await;
                // 自报用的实际档位：提权重跑后必须跟着变，不能谎报成会话档位。
                let mut effective_sandbox = tool_ctx.sandbox;
                // on-failure 提权（SH-120 / ADR-007②）：只有**沙箱拒绝**这一类失败才值得问一次
                // 「是否以 full-access 重跑」——普通非零退出重跑结果一样，问了只是多一次打断。
                // 提权必须用户点头：不问就放宽就是静默降级（AGENTS §11.3 退化红线）。
                if let Err(crate::tool_router::ToolError::SandboxDenied(sandbox_reason)) = &result {
                    if turn_policy.approval == crate::config::ApprovalMode::OnFailure
                        && turn_policy.sandbox != crate::config::SandboxMode::DangerFullAccess
                    {
                        let escalation = self
                            .broker
                            .request_escalation(
                                name,
                                &args_val,
                                call_id,
                                sandbox_reason,
                                call_subject,
                                crate::permission::GateEnv { sink: &sink, cancel },
                            )
                            .await;
                        match escalation {
                            crate::permission::GateOutcome::Allowed => {
                                // 只在这一次调用上放宽沙箱；会话策略与后续命令不受影响
                                let retry_ctx = ToolContext {
                                    sandbox: crate::config::SandboxMode::DangerFullAccess,
                                    ..tool_ctx
                                };
                                effective_sandbox = retry_ctx.sandbox;
                                result = self.router.execute(name, args_val, &retry_ctx).await;
                            }
                            crate::permission::GateOutcome::Cancelled => {
                                emit(&sink, EventMsg::ToolCallEnd {
                                    call_id: call_id.clone(),
                                    exit_code: None,
                                    error: Some("已取消".into()),
                                    result: Some(ToolCallResult::Cancelled),
                                });
                                reason = TurnEndReasonLocal::Cancelled;
                                break 'outer;
                            }
                            // 拒绝/策略禁止：保留原始沙箱失败回填给模型
                            _ => {}
                        }
                    }
                }
                let output_text = match record_tool_result(
                    history,
                    &sink,
                    name,
                    call_id,
                    &result,
                    effective_sandbox,
                ) {
                    Ok(output_text) => output_text,
                    Err(reason_local) => {
                        reason = reason_local;
                        break 'outer;
                    }
                };
                // SH-126：把本次调用的写入搬进本地缓冲。写日志本身是 take 语义，
                // 不搬走就会被下一次写入覆盖掉一半判定依据。
                //
                // 写后验证的脏标记也只认这里：**审批被拒的写入根本不会进写日志**
                // （工具未执行、未登记），所以「拒了就置脏、逼模型去编一个命令」
                // 这条路径天然不存在。
                let writes = write_journal.take();
                dirty.mark(&writes);
                if verify::DirtyTracker::has_source_change(&writes) {
                    dirty_epoch += 1;
                    // 旧通过记录随之失效：先绿后改不算已验证。
                    verify_passes.advance();
                }
                pending_writes.extend(writes);
                // SH-125：命令失败 → 提取「文件:行」→ 下一轮以独立 System 条目提示
                // 模型读错、修复、重跑；达到上限或同一错误重复则明示放弃（不伪造成功）。
                let succeeded = match &result {
                    Ok(out) => out.success,
                    Err(_) => false,
                };
                // 写后验证的基线：只收「首次置脏之前」的同命令失败。之后再有失败
                // 就是本次改动造成的，不能拿来当「本来就红」的藉口。
                if !succeeded && !dirty.is_dirty() && verify::baseline_enabled(&self.verify) {
                    if let Some(command) = verify::tool_command(name, arguments) {
                        baseline.note_failure(&command, &output_text);
                    }
                }
                let was_recovering = repair.is_recovering();
                if let Some(action) = repair_enabled
                    .then(|| repair.observe(name, succeeded, &output_text))
                    .flatten()
                {
                    let text = match action {
                        repair::RepairAction::Nudge(text)
                        | repair::RepairAction::GiveUp(text)
                        | repair::RepairAction::RejectedRepair(text)
                        | repair::RepairAction::SuspiciousRepair(text) => text,
                    };
                    history.push(HistoryItem::Message {
                        role: Role::System,
                        text,
                        reasoning: None,
                    });
                }
                // 恢复窗口从这次失败才开始：之前发生的写入属于「修复前」的正常改动，
                // 不能算到这次修复头上（否则新建测试文件这种正当行为会被误判）。
                if repair.is_recovering() && !was_recovering {
                    pending_writes.clear();
                }
                // SH-126：命令重新通过时结算——这次「绿」是不是靠改测试换来的。
                // 只对命令类工具结算：写工具自己的成功不是「修复完成」，写日志要留到重跑时用。
                if repair_enabled && succeeded && repair::is_command_tool(name) {
                    match repair.check_test_tampering(name, &pending_writes) {
                        Some(repair::RepairAction::RejectedRepair(text)) => {
                            turn_error = Some(local_turn_error("TEST_TAMPERING", text.clone()));
                            emit(&sink, EventMsg::Error { message: text.clone() });
                            history.push(HistoryItem::Message {
                                role: Role::System,
                                text,
                                reasoning: None,
                            });
                            reason = TurnEndReasonLocal::Failed;
                            break 'outer;
                        }
                        Some(repair::RepairAction::SuspiciousRepair(text)) => {
                            emit(&sink, EventMsg::Error { message: text.clone() });
                            history.push(HistoryItem::Message {
                                role: Role::System,
                                text,
                                reasoning: None,
                            });
                        }
                        _ => {}
                    }
                    pending_writes.clear();
                }
            }
            // SH-122：空转提醒排在全部工具输出之后，作为独立上下文条目交给下一轮模型。
            for reminder in anomaly_reminders {
                history.push(HistoryItem::Message {
                    role: Role::System,
                    text: reminder,
                    reasoning: None,
                });
            }
            // 轮末落盘：崩溃只丢当前轮，之前各轮的档案已经 flush 到位。
            // 档案不可写只降级（审计 H3 的"硬错误"已按观测不绑架主流程的原则推翻）：
            // 游标停在失败条目上，磁盘恢复后下一轮会补齐；本轮继续跑完。
            if let Err(message) = self.flush_history(rollout, history, recorded) {
                degrade_rollout(&sink, message);
            }

            // 5) 本轮工具批次已执行完、调用与输出已成对：此刻才收口预算耗尽。
            //    这样「最后一轮仍在请求工具」不会再静默丢调用，也无需额外的
            //    「未执行工具」说明——它们确实执行了，历史是完整的。
            if turn == self.max_turns {
                emit(&sink, EventMsg::Error { message: format!("已达最大轮数上限（{turn}）") });
                reason = TurnEndReasonLocal::BudgetExhausted;
                break;
            }
        }

        // 统一收口：所有 break 路径共用一次「写后验证」复查。
        //
        // 循环内的闸门只在模型主动收工（本轮没有工具调用）时才跑，而且要求
        // `turn + 1 < max_turns`（否决后还得有一轮去修）。于是「把预算全花在读写上、
        // 最后一轮仍在请求工具」的路径会在 `turn == max_turns` 处直接 break：
        // 一次宿主裁决都没有，只剩 `unverified` 这个披露位——判据碰巧通过时，
        // 过程就是「写完就收工、根本没验证」。
        //
        // 这里补的仍然是同一个闸门（`verify::evaluate`），不新造判定：
        // - 只对 root：子代理与父共享工作区，各自跑编译会互相踩；
        // - 只对「源码写入且这一代没有裁决」：纯问答、只读任务、只写文档都不会置脏，
        //   计划态下写工具本来就被挡住，因此不会被逼着跑编译；
        // - 取消/已失败不收口：用户主动停下时再起一个编译进程是错的。
        if verify_enabled
            && scope.depth == 0
            && dirty.is_dirty()
            && dirty_epoch != verified_epoch
            && !cancel.is_cancelled()
            && !matches!(
                reason,
                TurnEndReasonLocal::Cancelled | TurnEndReasonLocal::Failed
            )
        {
            let outcome = verify::evaluate(
                &verify::VerifyRequest {
                    cfg: &self.verify,
                    dirty: &dirty,
                    passes: &verify_passes,
                    baseline: verify::baseline_enabled(&self.verify).then_some(&baseline),
                    workspace_root: &self.workspace_root,
                    epoch: dirty_epoch,
                    timeout: std::time::Duration::from_millis(
                        self.verify.clamped_timeout_ms(),
                    ),
                    sandbox: crate::config::SandboxMode::WorkspaceWrite,
                },
                cancel,
            )
            .await;
            emit(
                &sink,
                EventMsg::VerificationResult {
                    turn: last_turn,
                    state: outcome.state,
                    commands: outcome.commands.clone(),
                    duration_ms: outcome.duration_ms,
                    summary: outcome.summary.clone(),
                    diagnostics: outcome.diagnostics.clone(),
                },
            );
            match outcome.state {
                crate::events::VerifyState::Passed => {
                    dirty.clear();
                    pending_writes.clear();
                }
                crate::events::VerifyState::Failed => {
                    // 没有剩余轮次可以否决：不再假装这是「已完成但未验证」。
                    // 宿主真的跑过、真的失败，收口就是 `failed`，命令与诊断放进
                    // 历史与 `error` 事件，用户在下一轮/续跑时能照着跑。
                    if matches!(
                        self.verify.effective_mode(),
                        crate::config::VerifyMode::Enforce
                    ) && !cancel.is_cancelled()
                    {
                        let text = verify::reminder_text(&outcome, None);
                        turn_error = Some(local_turn_error("VERIFY_FAILED", text.clone()));
                        emit(&sink, EventMsg::Error { message: text.clone() });
                        history.push(HistoryItem::Message {
                            role: Role::System,
                            text,
                            reasoning: None,
                        });
                        reason = TurnEndReasonLocal::Failed;
                    }
                }
                // 判不出来/没配置命令：不清脏、不改收口原因，仍由 `unverified` 披露。
                crate::events::VerifyState::Inconclusive(_)
                | crate::events::VerifyState::Skipped(_) => {}
            }
        }

        // 覆盖所有 break 路径（取消/失败/预算耗尽）——它们都从这里收口
        if let Err(message) = self.flush_history(rollout, history, recorded) {
            degrade_rollout(&sink, message);
        }

        // A0-3：回合结局必须落档。导出/统计只认这一行；不写就等于把失败当成功。
        // 写失败不阻断退出（档案已处于不可写状态），但必须在日志里点名——
        // 导出侧看不到 TurnEnd 时会标 unknown，不会被当成成功。
        if let Some(writer) = rollout {
            let reason_code = match reason {
                TurnEndReasonLocal::Completed => "completed",
                TurnEndReasonLocal::Cancelled => "cancelled",
                TurnEndReasonLocal::Failed => "failed",
                TurnEndReasonLocal::BudgetExhausted => "budget_exhausted",
            };
            if let Err(error) = writer.append(&crate::rollout::RolloutItem::TurnEnd {
                turn: last_turn,
                reason: reason_code.to_string(),
                at_ms: now_ms(),
                error: turn_error.clone(),
            }) {
                tracing::warn!(
                    target: "rollout",
                    %error,
                    "回合结局写入档案失败，导出将标记为 unknown"
                );
            }
        }

        emit(&sink, EventMsg::TurnCompleted {
            turn: last_turn,
            reason: match reason {
                TurnEndReasonLocal::Completed => crate::events::TurnEndReason::Completed,
                TurnEndReasonLocal::Cancelled => crate::events::TurnEndReason::Cancelled,
                TurnEndReasonLocal::Failed => crate::events::TurnEndReason::Failed,
                TurnEndReasonLocal::BudgetExhausted => crate::events::TurnEndReason::BudgetExhausted,
            },
            error: turn_error,
            // 有源码写入、但到结束都没拿到「通过」裁决：如实标记，不假装验证过。
            // 收口复查已跑过但仍被判不出来（Skipped/Inconclusive）或 warn 模式时，
            // 这里就是唯一的诚实出口；收口复查失败则 reason 已被改为 failed。
            unverified: dirty.is_dirty(),
        });
        // 相对路径给父级用：子代理收口后由 `wait_agent` 登记回父级写日志。
        // 两边都过同一个 `canonical_key`（长名 + 去 verbatim + 盘符一致），否则
        // Windows 的 8.3 短名（`RUNNER~1`）会让 `strip_prefix` 剥不掉，脏路径
        // 留成绝对路径。工作区本身都取不到长名时（已被删除/无权限）退回原路径：
        // 此时没有可信的公共前缀，保留绝对路径至少不丢信息，且脏判定仍按后缀生效。
        let workspace_key = crate::tool_router::canonical_key(&self.workspace_root);
        let dirty_paths = dirty
            .files()
            .map(|path| {
                path.strip_prefix(&workspace_key)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        TurnSummary { turns: last_turn, reason: reason.into(), dirty_paths }
    }
}

/// 本轮向模型声明的工具集合（ADR-003 计划态只读的第一层防御）：
/// 计划态下仅保留只读工具与 `exit_plan_mode`，写入/执行工具对模型不可见。
///
/// SH-091：没有任何 model-invocable 技能时，`skill` 工具不声明。工具仍注册在
/// 路由里（执行路径与审批测试照常），但模型看不到它，也就不会对空目录发起无效调用。
fn declarations_for(
    router: &ToolRouter,
    plan_active: bool,
    has_model_skills: bool,
    depth: u32,
    has_orchestrator: bool,
    allowed_tools: Option<&crate::permission::ToolAllowlist>,
) -> Vec<crate::model_client::ToolDeclaration> {
    let mut all = router.declarations();
    if !has_model_skills {
        all.retain(|declaration| declaration.name != "skill");
    }
    // SH-054 / ADR-103 第一层：被收窄掉的工具不向模型声明。
    // 这是"模型看不到就不会调用"的防线；执行期还有 gate 的同一份白名单兜底。
    if let Some(allowlist) = allowed_tools {
        all.retain(|declaration| allowlist.contains(&declaration.name));
    }
    // ADR-101 / FF-106 第一层：子代理的 ToolSpec 里不含编排工具；
    // 没有 orchestrator 的装配（测试/子代理）也不声明，避免模型看到不可用的能力。
    if depth >= MAX_AGENT_DEPTH || !has_orchestrator {
        all.retain(|declaration| !crate::tool_router::orchestration::is_agent_tool(&declaration.name));
    }
    if !plan_active {
        return all;
    }
    all.into_iter()
        .filter(|d| policy::plan_mode_allows(&d.name))
        .collect()
}

/// 可以并行执行的只读工具：复用 `risk_of` 的 Read 档，再排除三类**会改状态**的调用。
///
/// 工具自身的 `spec().risk` 优先于 `risk_of`（SH-063/ADR-107 的用户批准覆盖），
/// 否则动态工具一旦被标成写入档仍会被当成只读并发放行。
///
/// 排除名单各自有明确理由，不是保守起见：
/// - `todo_write` 改会话内任务清单，两次调用谁先落地就是语义；
/// - `enter_plan_mode` 切换计划态，顺序即状态迁移；
/// - `wait_agent` 属于子代理面（用户裁定子代理一律串行），并发等待没有收益。
fn parallel_read_only(router: &ToolRouter, name: &str) -> bool {
    let risk = router
        .get(name)
        .and_then(|executor| executor.spec().risk)
        .unwrap_or_else(|| policy::risk_of(name));
    risk == crate::events::RiskLevel::Read
        && !matches!(name, "todo_write" | "enter_plan_mode" | "wait_agent")
}

/// 回填一次工具执行结果：`ToolCallEnd` 事件 + 给模型的 tool 输出（附沙箱说明与图片引用）。
///
/// 返回 `Err(reason)` 表示这次调用被取消，调用方必须结束整个回合；`Ok` 是写进历史前的
/// 工具输出原文（baseline/repair 判定要用的、未附沙箱说明的那份）。
///
/// 串行与只读批次共用这一份回填：两条路径的结束事件语义必须逐字一致，否则
/// 「同一工具在批次里报成功、在串行里报失败」这类分叉没有任何测试挡得住。
fn record_tool_result(
    history: &mut Vec<HistoryItem>,
    sink: &Arc<dyn crate::events::EventSink>,
    name: &str,
    call_id: &str,
    result: &Result<crate::tool_router::ToolOutput, crate::tool_router::ToolError>,
    sandbox: crate::config::SandboxMode,
) -> Result<String, TurnEndReasonLocal> {
    let output_text = match result {
        Ok(out) => {
            let _ = sink.emit(&EventMsg::ToolCallEnd {
                call_id: call_id.to_string(),
                exit_code: Some(i32::from(!out.success)),
                error: None,
                // 成功/失败是两回事：exit_code 只表达是否非零退出，result 才区分语义
                result: Some(if out.success {
                    ToolCallResult::Ok
                } else {
                    ToolCallResult::Error
                }),
            });
            out.output.clone()
        }
        Err(crate::tool_router::ToolError::Cancelled) => {
            let _ = sink.emit(&EventMsg::ToolCallEnd {
                call_id: call_id.to_string(),
                exit_code: None,
                error: Some("已取消".into()),
                result: Some(ToolCallResult::Cancelled),
            });
            return Err(TurnEndReasonLocal::Cancelled);
        }
        Err(error) => {
            let _ = sink.emit(&EventMsg::ToolCallEnd {
                call_id: call_id.to_string(),
                exit_code: None,
                error: Some(error.to_string()),
                result: Some(ToolCallResult::Error),
            });
            error.to_string()
        }
    };
    history.push(HistoryItem::FunctionCallOutput {
        call_id: call_id.to_string(),
        output: match crate::sandbox::execution_note(name, sandbox, &output_text) {
            Some(note) => format!("{output_text}\n{note}"),
            None => output_text.clone(),
        },
    });
    // SH-072：view_image 的图片以引用形式回填成一条用户消息——Chat 协议不允许
    // 图片挂在 tool 消息里，模型只能从用户消息里看到它。
    if let Ok(out) = result {
        if out.success {
            if let Some(image) = crate::tool_router::attachment(&out.output) {
                history.push(HistoryItem::ImageMessage {
                    role: crate::model_client::Role::User,
                    text: format!("[view_image] {}", image.path),
                    images: vec![image],
                });
            }
        }
    }
    Ok(output_text)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnEndReasonLocal {
    Completed,
    Cancelled,
    Failed,
    BudgetExhausted,
}

impl From<TurnEndReasonLocal> for crate::events::TurnEndReason {
    fn from(r: TurnEndReasonLocal) -> Self {
        match r {
            TurnEndReasonLocal::Completed => Self::Completed,
            TurnEndReasonLocal::Cancelled => Self::Cancelled,
            TurnEndReasonLocal::Failed => Self::Failed,
            TurnEndReasonLocal::BudgetExhausted => Self::BudgetExhausted,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSummary {
    pub turns: u32,
    pub reason: crate::events::TurnEndReason,
    /// 本任务改过、但收口时仍未通过验证的源码（相对工作区）。
    /// 子代理收口时带回父级，父级再登记进写日志，让脏状态自然传导。
    pub dirty_paths: Vec<String>,
}

// ============================== 采样重试 ==============================

/// 一次采样失败后的处置结论。
enum RetryOutcome {
    /// 已按退避等过一段、可以再发一次请求。
    Retry,
    /// 等待期间被取消：不发 `started`，直接走 `TurnCompleted(cancelled)`。
    Cancelled,
    /// 终局。错误可能被改写（`Retry-After` 超上限时换成可解释的那句话）。
    GiveUp(crate::model_client::ModelError),
}

/// 非“因 `Retry-After` 超限”的终局：清掉端点要求，避免 `TurnErrorInfo.retry_after_secs`
/// 记录一个与放弃原因无关的秒数。
fn give_up_without_retry_after(error: &crate::model_client::ModelError) -> RetryOutcome {
    let mut error = error.clone();
    error.retry_after_ms = None;
    RetryOutcome::GiveUp(error)
}

/// 本地失败（压缩/引用展开/自愈拒绝/收口验证）也要带结构化 `turn_error`。
///
/// 这些失败没有模型错误分类可用；只发 `Error` 事件的话，前端只能落进 UNKNOWN 兜底，
/// 用户看不到"为什么失败"。`code` 用稳定的本地错误码，`message` 必须是可读的一句话。
fn local_turn_error(code: &str, message: impl Into<String>) -> crate::events::TurnErrorInfo {
    crate::events::TurnErrorInfo {
        code: code.to_string(),
        message: message.into(),
        http_status: None,
        provider_code: None,
        provider_message: None,
        attempts: None,
        retry_after_secs: None,
    }
}

/// 决定这次采样失败是否重试；重试则发 `scheduled` → 可取消等待 → 发 `started`。
///
/// 调用方必须保证只在**首个可见增量之前**调用：本函数不认识流内容，`emitted_delta`
/// 的判定在采样循环里（正文与推理增量任一 emit 过即禁止重试）。
///
/// 三条硬约束的落点：
/// - 等待与取消令牌 `select!` 竞争，不是裸 `sleep`；
/// - 次数用尽 / `Retry-After` 超上限都发 `exhausted`，且后者文案带出要求秒数与上限；
/// - `max_retries == 0`（测试默认）时一次也不试，且不产生任何 `model_retry` 事件。
async fn await_retry(
    policy: &crate::model_client::RetryPolicy,
    sink: &Arc<dyn crate::events::EventSink>,
    cancel: &CancellationToken,
    turn: u32,
    retries_used: u32,
    retry_chain: &mut Option<String>,
    error: &crate::model_client::ModelError,
) -> RetryOutcome {
    use crate::events::RetryPhase;

    if policy.max_retries == 0 || !crate::model_client::RetryPolicy::is_retryable(error.kind) {
        return give_up_without_retry_after(error);
    }

    // 第几次重试，1 起；同一条重试链共用一个 id（不同采样步各自生成）。
    let retry = retries_used + 1;
    let max_retries = policy.max_retries;
    let retry_id = retry_chain.get_or_insert_with(|| ulid::Ulid::generate().to_string()).clone();
    let send = |phase: RetryPhase, delay_ms: u64, message: String| {
        let _ = sink.emit(&EventMsg::ModelRetry {
            turn,
            retry,
            max_retries,
            phase,
            delay_ms,
            code: error.code().to_string(),
            message,
            retry_id: retry_id.clone(),
        });
    };

    if retries_used >= max_retries {
        send(RetryPhase::Exhausted, 0, format!("已重试 {retries_used} 次，仍失败：{}", error.human));
        return give_up_without_retry_after(error);
    }

    let Some(delay) = policy.delay(retry, error.retry_after_ms, crate::model_client::random01()) else {
        // 端点要求等待超过上限：放弃，但**必须说清为什么**（dsh 在这里只是静默跳过，
        // 用户只知道“失败了”，不知道“服务端让等 30 秒而我们最多等 10 秒”）。
        // 秒数取响应头原值，不四舍五入；上限取 max_delay 的整秒。
        let required = error.retry_after_ms.unwrap_or(0) / 1000;
        let message =
            format!("服务端要求等待 {required} 秒，超过上限 {} 秒，已放弃重试", policy.max_delay.as_secs());
        send(RetryPhase::Exhausted, 0, message.clone());
        let mut gave_up = error.clone();
        gave_up.human = message.clone();
        gave_up.provider_message = Some(message);
        // 保留 `retry_after_ms`：`TurnErrorInfo.retry_after_secs` 要靠它带出原始秒数。
        return RetryOutcome::GiveUp(gave_up);
    };

    let delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
    send(RetryPhase::Scheduled, delay_ms, error.human.clone());
    let waited = tokio::select! {
        _ = cancel.cancelled() => false,
        _ = tokio::time::sleep(delay) => true,
    };
    if !waited {
        // 取消发生在等待中：不发 started、不发 exhausted；前端由“有 scheduled、
        // 无 started、回合已关闭”推导为 cancelled（与 dsh retry 节点一致）。
        return RetryOutcome::Cancelled;
    }
    send(RetryPhase::Started, 0, error.human.clone());
    RetryOutcome::Retry
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{EventSink, TurnEndReason};
    use crate::model_client::ToolDeclaration;
    use crate::tool_router::{ToolError, ToolExecutor, ToolOutput, ToolSpec};
    use serde_json::Value;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    // ---- 测试基建：捕获 sink + 脚本后端 + echo 工具 ----

    #[derive(Clone)]
    struct CapturingSink(Arc<Mutex<Vec<EventMsg>>>);

    impl CapturingSink {
        fn new() -> (Self, Arc<Mutex<Vec<EventMsg>>>) {
            let store = Arc::new(Mutex::new(Vec::new()));
            (Self(store.clone()), store)
        }
    }

    impl EventSink for CapturingSink {
        fn emit(&self, msg: &EventMsg) -> Result<(), String> {
            self.0.lock().unwrap().push(msg.clone());
            Ok(())
        }
    }

    struct ScriptedBackend(Mutex<VecDeque<Vec<ModelStreamEvent>>>);

    impl ScriptedBackend {
        fn new(turns: Vec<Vec<ModelStreamEvent>>) -> Self {
            Self(Mutex::new(turns.into()))
        }
    }

    #[async_trait::async_trait]
    impl ModelBackend for ScriptedBackend {
        async fn stream(
            &self,
            _history: &[HistoryItem],
            _tools: &[ToolDeclaration],
            _instructions: &str,
        ) -> Result<ModelStream, crate::model_client::ModelError> {
            let mut q = self.0.lock().unwrap();
            match q.pop_front() {
                Some(script) if !script.is_empty() => Ok(Box::pin(futures::stream::iter(script))),
                // 空脚本 = 永不产出的事件流（取消/超时类确定性测试用）
                _ => Ok(Box::pin(futures::stream::pending())),
            }
        }
    }

    /// 与 [`ScriptedBackend`] 的"空脚本"不同：这里连**流都不会建立**——
    /// 用于验证取消能在 `stream().await` 阶段生效（审计 H1）。
    struct HangingBackend;

    #[async_trait::async_trait]
    impl ModelBackend for HangingBackend {
        async fn stream(
            &self,
            _history: &[HistoryItem],
            _tools: &[ToolDeclaration],
            _instructions: &str,
        ) -> Result<ModelStream, crate::model_client::ModelError> {
            futures::future::pending().await
        }
    }

    struct EchoTool {
        spec: ToolSpec,
    }

    impl Default for EchoTool {
        fn default() -> Self {
            // 命名 read_file → 风险 read → 门禁自动放行（回显工具用于循环测试）
            Self {
                spec: ToolSpec::new("read_file", "回显", serde_json::json!({"type":"object"})),
            }
        }
    }

    #[async_trait::async_trait]
    impl ToolExecutor for EchoTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }
        async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::ok(format!("echo:{args}")))
        }
    }

    fn make_loop(
        turns: Vec<Vec<ModelStreamEvent>>,
    ) -> (
        AgentLoop,
        Arc<Mutex<Vec<EventMsg>>>,
        CancellationToken,
        Arc<ApprovalBroker>,
    ) {
        make_loop_with_backend(Arc::new(ScriptedBackend::new(turns)))
    }

    fn make_loop_with_backend(
        backend: Arc<dyn ModelBackend>,
    ) -> (
        AgentLoop,
        Arc<Mutex<Vec<EventMsg>>>,
        CancellationToken,
        Arc<ApprovalBroker>,
    ) {
        let mut router = ToolRouter::new();
        router.register(Arc::new(EchoTool::default()));
        let (_sink, store) = CapturingSink::new();
        let broker = Arc::new(ApprovalBroker::new());
        let agent = AgentLoop {
            backend,
            router: Arc::new(router),
            broker: broker.clone(),
            plan_state: Arc::new(PlanState::new()),
            workspace_root: std::env::temp_dir(),
            max_turns: 3,
            // 既有循环用例一律不启用压缩：它们断言的是轮次与工具回填行为，
            // 混进压缩会让"为什么这轮历史变短"变成隐性变量。
            token_budget: 0,
            tree_budget: TreeBudget::unlimited(),
            policy: Arc::new(policy::SessionPolicyHandle::new(policy::SessionPolicy {
                approval: crate::config::ApprovalMode::OnRequest,
                sandbox: crate::config::SandboxMode::WorkspaceWrite,
            })),
            rollout: None,
            rollout_broke: std::sync::atomic::AtomicBool::new(false),
            session_id: None,
            jobs: None,
            stop_hook: None,
            verify: Default::default(),
            retry: crate::model_client::RetryPolicy::disabled(),
        };
        (agent, store, CancellationToken::new(), broker)
    }

    fn usage_ev() -> ModelStreamEvent {
        ModelStreamEvent::Completed { usage: Usage { input_tokens: 10, cached_input_tokens: None, output_tokens: 5 } }
    }

    fn fn_call_ev() -> ModelStreamEvent {
        ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
            name: "read_file".into(),
            arguments: r#"{"x":1}"#.into(),
            call_id: "c1".into(),
        })
    }

    /// 压缩用例专用：端点报回大用量。压缩判据取 `max(观测值, 字节估算)`，
    /// 只有观测值足够大，才能把触发点精确放在"第 1 轮结束后"——而不是第 1 轮
    /// 开头（那时历史里已有用户消息 + 环境 system 两条，配小预算会立刻触发）。
    fn big_usage_ev() -> ModelStreamEvent {
        ModelStreamEvent::Completed {
            usage: Usage { input_tokens: 500_000, cached_input_tokens: None, output_tokens: 5 },
        }
    }

    // ---- 用例 ----

    #[tokio::test]
    async fn single_turn_completion_emits_expected_events() {
        let (agent, store, cancel, _broker) = make_loop(vec![
            vec![
                ModelStreamEvent::OutputTextDelta { delta: "你好".into() },
                usage_ev(),
            ],
        ]);
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.reason, TurnEndReason::Completed);
        let events = store.lock().unwrap();
        let tags: Vec<&str> = events.iter().map(|e| match e {
            EventMsg::TurnStarted { .. } => "turn_started",
            // 提示词入轨迹：首个 turn 在实际下发前记一条（每个 run 恰好一次）。
            EventMsg::SystemPrompt { .. } => "system_prompt",
            EventMsg::AgentMessageDelta { .. } => "delta",
            EventMsg::TokenUsage { .. } => "usage",
            EventMsg::TurnCompleted { .. } => "turn_completed",
            other => panic!("意外事件: {other:?}"),
        }).collect();
        assert_eq!(tags, ["turn_started", "system_prompt", "delta", "usage", "turn_completed"]);
    }

    /// 结束前否决点：始终否决的钩子也只能再要一轮（每次任务上限一次），
    /// 且提醒以 `Role::System` 注入——不冒充用户消息，也不伪装成工具结果。
    #[tokio::test]
    async fn stop_hook_vetoes_at_most_once_then_allows_completion() {
        struct AlwaysVeto {
            calls: Mutex<u32>,
        }
        impl StopHook for AlwaysVeto {
            fn before_stop(&self, ctx: StopContext<'_>) -> Option<String> {
                *self.calls.lock().unwrap() += 1;
                assert!(ctx.last_text.contains("完成"), "否决点应看到模型收尾文本");
                Some("宿主提醒：还有未完成的步骤。".into())
            }
        }

        let (mut agent, store, cancel, _broker) = make_loop(vec![
            vec![ModelStreamEvent::OutputTextDelta { delta: "完成".into() }, usage_ev()],
            vec![ModelStreamEvent::OutputTextDelta { delta: "完成".into() }, usage_ev()],
            vec![ModelStreamEvent::OutputTextDelta { delta: "完成".into() }, usage_ev()],
        ]);
        let hook = Arc::new(AlwaysVeto { calls: Mutex::new(0) });
        agent.stop_hook = Some(hook.clone() as Arc<dyn StopHook>);
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store)), cancel).await;

        // 一次否决 → 再跑一轮；第二次想否决时 `stop_vetoed` 已置位，必须放行。
        assert_eq!(summary.turns, 2);
        assert_eq!(summary.reason, TurnEndReason::Completed);
        assert_eq!(*hook.calls.lock().unwrap(), 1);
    }

    /// 没有钩子时必须一轮结束——否决点不能让既有「文本即完成」的路径多跑。
    #[tokio::test]
    async fn without_stop_hook_text_only_turn_completes_immediately() {
        let (agent, store, cancel, _broker) = make_loop(vec![
            vec![ModelStreamEvent::OutputTextDelta { delta: "完成".into() }, usage_ev()],
            vec![ModelStreamEvent::OutputTextDelta { delta: "不该跑到".into() }, usage_ev()],
        ]);
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store)), cancel).await;
        assert_eq!(summary.turns, 1);
        assert_eq!(summary.reason, TurnEndReason::Completed);
    }

    /// 默认钩子只对「计划态悬空」开口：正常收尾不得被拦。
    #[test]
    fn plan_mode_stop_hook_only_vetoes_when_plan_is_active() {
        let history: Vec<HistoryItem> = Vec::new();
        let ctx = |plan_active| StopContext {
            history: &history,
            last_text: "完成",
            turn: 1,
            plan_active,
        };
        assert!(PlanModeStopHook.before_stop(ctx(true)).is_some());
        assert!(PlanModeStopHook.before_stop(ctx(false)).is_none());
    }

    // ---- 写后验证闸门（design.md / spec.md） ----

    /// 假写入工具：按参数写文件并登记写日志（`path` / `body`，缺省 `src/lib.rs`）。
    ///
    /// 名字**必须**是 `apply_patch`：未知工具名会被 `permission::risk_of` 判成
    /// Destructive 而走审批，测试里没有审批人 → 永久挂起。写什么文件由参数决定，
    /// 因此同一个工具就能演「模型自己修好了」的回合。
    struct FakeWriter {
        spec: ToolSpec,
    }

    impl Default for FakeWriter {
        fn default() -> Self {
            Self { spec: ToolSpec::new("apply_patch", "假写入", serde_json::json!({"type":"object"})) }
        }
    }

    #[async_trait::async_trait]
    impl ToolExecutor for FakeWriter {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }
        async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
            let rel = args.get("path").and_then(|v| v.as_str()).unwrap_or("src/lib.rs");
            let body = args.get("body").and_then(|v| v.as_str()).unwrap_or_default();
            let path = ctx.workspace_root.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            let before = std::fs::read_to_string(&path).ok();
            std::fs::write(&path, body).map_err(|e| ToolError::Execution(e.to_string()))?;
            if let Some(journal) = &ctx.writes {
                journal.record(&path, before.as_deref(), Some(body));
            }
            Ok(ToolOutput::ok("written"))
        }
    }

    /// 假终端：把「按原样跑过验证命令并通过」记进共享观测（替身只做这一段）。
    ///
    /// 名字用 `read_file` 是为了走 Read 风险（Allow），避免测试里等审批——
    /// 真实的 `terminal` 走的是同一份 `ctx.verify_passes` 句柄。
    struct FakePassRecorder {
        spec: ToolSpec,
        command: String,
    }

    #[async_trait::async_trait]
    impl ToolExecutor for FakePassRecorder {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }
        async fn execute(&self, _args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
            if let Some(log) = &ctx.verify_passes {
                log.record_pass(&self.command);
            }
            Ok(ToolOutput::ok("ok"))
        }
    }

    fn script_call_with(name: &str, arguments: &str, call_id: &str) -> Vec<ModelStreamEvent> {
        vec![
            ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                name: name.into(),
                arguments: arguments.into(),
                call_id: call_id.into(),
            }),
            usage_ev(),
        ]
    }

    fn script_text(text: &str) -> Vec<ModelStreamEvent> {
        vec![ModelStreamEvent::OutputTextDelta { delta: text.into() }, usage_ev()]
    }

    fn verify_demo(
        turns: Vec<Vec<ModelStreamEvent>>,
        commands: Vec<String>,
        max_turns: u32,
        mode: crate::config::VerifyMode,
    ) -> (
        AgentLoop,
        Arc<Mutex<Vec<EventMsg>>>,
        CancellationToken,
        crate::test_support::TestTempDir,
    ) {
        let (mut agent, store, cancel, _broker) = make_loop(turns);
        let ws = crate::test_support::TestTempDir::new("verify-gate");
        let mut router = ToolRouter::new();
        router.register(Arc::new(FakeWriter::default()));
        router.register(Arc::new(FakePassRecorder {
            spec: ToolSpec::new("read_file", "假终端观测", serde_json::json!({"type":"object"})),
            command: commands.first().cloned().unwrap_or_default(),
        }));
        agent.router = Arc::new(router);
        agent.workspace_root = ws.path().to_path_buf();
        agent.max_turns = max_turns;
        agent.verify = crate::config::VerifyConfig {
            commands,
            mode,
            ..Default::default()
        };
        // 与 `AgentLoop::from_config` 走同一个消融入口：这条路径若不接，评测对照臂
        // 就只有集成用例能验，单测里量不出「关掉闸门」的差别。
        apply_eval_ablation(&mut agent.verify);
        (agent, store, cancel, ws)
    }

    fn verification_states(events: &[EventMsg]) -> Vec<crate::events::VerifyState> {
        events
            .iter()
            .filter_map(|event| match event {
                EventMsg::VerificationResult { state, .. } => Some(*state),
                _ => None,
            })
            .collect()
    }

    fn completed_unverified(events: &[EventMsg]) -> bool {
        events
            .iter()
            .find_map(|event| match event {
                EventMsg::TurnCompleted { unverified, .. } => Some(*unverified),
                _ => None,
            })
            .expect("必须有一条 turn_completed")
    }

    /// 不设 `SHANIU_EVAL_ABLATE_VERIFY=1` 时闸门必须是开的（消融是唯一关掉闸门的路径）。
    ///
    /// 反向对照（AGENTS §4.2）：`$env:SHANIU_EVAL_ABLATE_VERIFY='1'; cargo test --lib
    /// eval_ablation_switch` 必须红——红了才证明「关掉闸门」这条对照臂真由这个开关驱动，
    /// 而不是被别的路径顺手关掉。同一次带上环境变量跑闸门用例也必须红（见 verify_demo）。
    #[test]
    fn verify_gate_is_enabled_unless_eval_ablation_switch_is_set() {
        let mut verify = crate::config::VerifyConfig::default();
        apply_eval_ablation(&mut verify);
        assert!(
            verify.enabled,
            "未设 SHANIU_EVAL_ABLATE_VERIFY=1 时写后验证闸门必须默认开启"
        );
    }

    /// 失败 → 否决一次 → 模型补一步让命令通过 → 放行；全程只否决一次。
    #[tokio::test]
    async fn verification_gate_vetoes_once_then_passes() {
        // 会起沙箱进程：与建/删 AppContainer profile 的用例串行，否则
        // CreateProcessAsUserW 可能撞上刚被回收的 profile（0x80070002 → EnvFailure）。
        let _serial = crate::sandbox::appcontainer::PROFILE_TEST_MUTEX.lock().await;
        let (agent, store, cancel, _ws) = verify_demo(
            vec![
                script_call_with(
                    "apply_patch",
                    r#"{"path":"src/lib.rs","body":"pub fn f() {}"}"#,
                    "w1",
                ),
                script_text("写完了"),
                script_call_with(
                    "apply_patch",
                    r#"{"path":"src/ok.rs","body":"pub fn ok() {}"}"#,
                    "w2",
                ),
                script_text("验证通过"),
            ],
            vec![r"if exist src\ok.rs (exit 0) else (exit 1)".into()],
            6,
            crate::config::VerifyMode::Enforce,
        );
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;

        assert_eq!(summary.reason, TurnEndReason::Completed);
        let events = store.lock().unwrap();
        let states = verification_states(&events);
        assert_eq!(
            summary.turns, 4,
            "失败否决后应多跑一轮，通过后不再续跑；实际裁决序列 {states:?}"
        );
        assert_eq!(
            verification_states(&events),
            vec![
                crate::events::VerifyState::Failed,
                crate::events::VerifyState::Passed
            ]
        );
        assert!(!completed_unverified(&events), "验证通过后不得标记未验证");
    }

    /// 旧行为：末轮不可否决就整块跳过闸门，一次 `VerificationResult` 都不发，
    /// 只靠 `unverified` 披露；判据碰巧通过时，过程就是「写完就收工、根本没验证」。
    /// 新行为：末轮仍要拿到真实裁决；没有下一轮可修时，失败即如实收口为 `failed`。
    #[tokio::test]
    async fn verification_gate_last_turn_still_runs_and_fails_the_turn() {
        let (agent, store, cancel, _ws) = verify_demo(
            vec![
                script_call_with("apply_patch", r#"{"body":"pub fn f() {}"}"#, "w1"),
                script_text("写完了"),
            ],
            vec!["exit 1".into()],
            2,
            crate::config::VerifyMode::Enforce,
        );
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;

        assert_eq!(summary.reason, TurnEndReason::Failed, "无剩余轮次时不得伪装成完成");
        let events = store.lock().unwrap();
        assert_eq!(
            verification_states(&events),
            vec![crate::events::VerifyState::Failed],
            "末轮也必须向宿主拿一次裁决"
        );
        assert!(completed_unverified(&events), "失败后仍标记未验证");
        assert!(
            events.iter().any(|event| matches!(
                event,
                EventMsg::Error { message } if message.contains("exit 1")
            )),
            "必须把可执行的下一步（命令）交给用户"
        );
        // P0-2：末轮验证失败也要落 `turn_error`，不能只靠 `Error` 事件。
        let info = events
            .iter()
            .find_map(|event| match event {
                EventMsg::TurnCompleted { error: Some(info), .. } => Some(info),
                _ => None,
            })
            .expect("验证失败必须带结构化原因");
        assert_eq!(info.code, "VERIFY_FAILED");
    }

    /// 复现 `repo-feat-context-overflow-code`：32 轮全在读写，最后一轮仍在请求工具，
    /// 循环在轮次上限直接 break。旧行为这里 0 次裁决、收口 `budget_exhausted`。
    /// 新行为在统一收口补一次宿主裁决：失败即 `failed`，不再把「没验证」当完成。
    #[tokio::test]
    async fn verification_gate_runs_when_budget_exhausts_with_tool_calls() {
        let (agent, store, cancel, _ws) = verify_demo(
            vec![
                script_call_with("apply_patch", r#"{"body":"pub fn f() {}"}"#, "w1"),
                // 同内容写入：工具调用照常发生，但不推进脏代次。
                script_call_with("apply_patch", r#"{"body":"pub fn f() {}"}"#, "w2"),
            ],
            vec!["exit 1".into()],
            2,
            crate::config::VerifyMode::Enforce,
        );
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;

        assert_eq!(summary.turns, 2);
        assert_eq!(summary.reason, TurnEndReason::Failed);
        let events = store.lock().unwrap();
        assert_eq!(
            verification_states(&events),
            vec![crate::events::VerifyState::Failed],
            "预算耗尽的收口口也必须接闸门"
        );
        assert!(completed_unverified(&events));
        assert!(events.iter().any(|event| matches!(
            event,
            EventMsg::Error { message } if message.contains("exit 1")
        )));
    }

    /// 同一收口口上「跑过验证」的对照组：观察到终端通过就放行，不重复跑、不改收口。
    #[tokio::test]
    async fn verification_gate_exit_pass_clears_unverified() {
        // 写入与验证放在同一轮：写入先执行并推进脏代次，随后的终端调用记录本代通过。
        // 末轮只有文本（不可否决），逼出统一收口口，验证它认这份证据并放行。
        let (agent, store, cancel, _ws) = verify_demo(
            vec![
                vec![
                    ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                        name: "apply_patch".into(),
                        arguments: r#"{"body":"pub fn f() {}"}"#.into(),
                        call_id: "w1".into(),
                    }),
                    ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                        name: "read_file".into(),
                        arguments: "{}".into(),
                        call_id: "r1".into(),
                    }),
                    usage_ev(),
                ],
                script_text("写完了"),
            ],
            vec!["cargo test".into()],
            2,
            crate::config::VerifyMode::Enforce,
        );
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;

        assert_eq!(summary.reason, TurnEndReason::Completed, "验证通过不改收口原因");
        let events = store.lock().unwrap();
        assert_eq!(
            verification_states(&events),
            vec![crate::events::VerifyState::Passed]
        );
        assert!(!completed_unverified(&events), "验证通过后不得标记未验证");
    }

    /// A：子代理改了源码、root 自己没写。`wait_agent` 把子代理的脏路径并回父级
    /// 写日志后，父级收口必须照样被要求验证。
    /// 旧行为（`AgentOutcome` 不带路径、也不登记）下父级脏状态为空，这里会以
    /// `completed` 收口且 0 次 `VerificationResult`。
    #[tokio::test]
    async fn child_writes_are_verified_by_parent_gate() {
        use crate::tool_router::orchestration::{
            AgentOrchestrator, AgentOutcome, AgentOutcomeState, AgentScope,
        };

        struct ChildStub;

        #[async_trait::async_trait]
        impl AgentOrchestrator for ChildStub {
            async fn spawn_agent(
                &self,
                _parent: &AgentScope,
                _task: &str,
                _budget_tokens: Option<u64>,
                _profile: Option<&str>,
                _allowed_tools: Option<&[String]>,
            ) -> Result<String, String> {
                Err("本用例不派生子代理".into())
            }

            async fn resume_agent(
                &self,
                _parent: &AgentScope,
                _agent_id: &str,
                _task: Option<&str>,
            ) -> Result<String, String> {
                Err("本用例不续跑子代理".into())
            }

            async fn wait_agents(
                &self,
                agent_ids: &[String],
                _timeout: std::time::Duration,
            ) -> Vec<AgentOutcome> {
                agent_ids
                    .iter()
                    .map(|agent_id| AgentOutcome {
                        agent_id: agent_id.clone(),
                        state: AgentOutcomeState::Done,
                        summary: "完成".into(),
                        artifacts: Vec::new(),
                        token_usage: None,
                        turns: Some(2),
                        dirty_paths: vec!["src/child.rs".into()],
                    })
                    .collect()
            }

            fn send_message(&self, _agent_id: &str, _message: &str) -> Result<(), String> {
                Err("本用例不注入消息".into())
            }
        }

        let (mut agent, store, cancel, _broker) = make_loop(vec![
            script_call_with("wait_agent", r#"{"agent_ids":["a1"]}"#, "wait-1"),
            // 最后一轮只用文字收工：此刻才走统一收口闸门。
            script_text("子代理改完了"),
        ]);
        let ws = crate::test_support::TestTempDir::new("child-dirty");
        let mut router = ToolRouter::new();
        crate::tools::orchestrate::register(&mut router);
        agent.router = Arc::new(router);
        agent.workspace_root = ws.path().to_path_buf();
        agent.max_turns = 2;
        agent.verify = crate::config::VerifyConfig {
            commands: vec!["exit 1".into()],
            mode: crate::config::VerifyMode::Enforce,
            ..Default::default()
        };
        let summary = agent
            .run_task_with_orchestrator(
                "任务",
                Arc::new(CapturingSink(store.clone())),
                cancel,
                Some(Arc::new(ChildStub)),
            )
            .await;

        assert_eq!(summary.dirty_paths, vec!["src/child.rs".to_string()]);
        // 父级自己没有写入，脏文件全部来自子代理；不并回写日志就不会有这次裁决。
        assert_eq!(
            verification_states(&store.lock().unwrap()),
            vec![crate::events::VerifyState::Failed],
            "子代理改了源码，父级收工必须被要求验证"
        );
        assert!(
            completed_unverified(&store.lock().unwrap()),
            "验证失败必须如实标记未验证，不能当成已完成"
        );
    }

    /// B：最后一轮仍在请求工具。
    /// 旧行为在工具批次执行前就 `break`，历史里连 `FunctionCall` 都没有（更不会有
    /// 输出）；新行为先把本轮批次执行完，再以预算耗尽收口，保证调用/输出成对。
    #[tokio::test]
    async fn last_turn_tool_calls_execute_before_budget_exhaustion() {
        let (mut agent, store, cancel, _broker) = make_loop(vec![script_call_with(
            "read_file",
            "{}",
            "last-call",
        )]);
        agent.max_turns = 1;
        let mut instance =
            crate::agent_loop::instance::AgentInstance::root(Vec::new(), 0, cancel);
        let summary = agent
            .run_instance(&mut instance, Arc::new(CapturingSink(store)))
            .await;

        assert_eq!(summary.turns, 1);
        assert_eq!(summary.reason, TurnEndReason::BudgetExhausted, "收口原因仍是预算耗尽");
        let calls: Vec<&str> = instance
            .history
            .iter()
            .filter_map(|item| match item {
                HistoryItem::FunctionCall { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .collect();
        let outputs: Vec<&str> = instance
            .history
            .iter()
            .filter_map(|item| match item {
                HistoryItem::FunctionCallOutput { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(calls, vec!["last-call"], "最后一轮声明的调用必须入历史并执行");
        assert_eq!(outputs, calls, "每个 FunctionCall 都必须有配对的 Output");
    }

    /// `warn` 是运行期泄压阀：仍发事件，但不否决、不增加轮次。
    #[tokio::test]
    async fn verification_gate_warn_mode_reports_without_veto() {
        let (agent, store, cancel, _ws) = verify_demo(
            vec![
                script_call_with("apply_patch", r#"{"body":"pub fn f() {}"}"#, "w1"),
                script_text("写完了"),
                script_text("不该跑到"),
            ],
            vec!["exit 1".into()],
            4,
            crate::config::VerifyMode::Warn,
        );
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;

        assert_eq!(summary.turns, 2, "warn 不否决，不得多跑一轮");
        let events = store.lock().unwrap();
        assert_eq!(verification_states(&events), vec![crate::events::VerifyState::Failed]);
        assert!(completed_unverified(&events));
    }

    /// 纯文档写入不置脏：没有可验证的源码产物，不该跑编译。
    #[tokio::test]
    async fn verification_gate_ignores_documentation_writes() {
        let (agent, store, cancel, _ws) = verify_demo(
            vec![
                script_call_with("apply_patch", r#"{"path":"notes.md","body":"记录"}"#, "w1"),
                script_text("写完了"),
            ],
            vec!["exit 1".into()],
            3,
            crate::config::VerifyMode::Enforce,
        );
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;

        assert_eq!(summary.turns, 2);
        let events = store.lock().unwrap();
        assert!(verification_states(&events).is_empty());
        assert!(!completed_unverified(&events), "没有源码写入就不算未验证");
    }

    /// 饥饿序列：计划否决先吃掉自己那一票，验证闸门仍有票；两票各自只用一次。
    #[tokio::test]
    async fn verification_gate_does_not_starve_after_plan_veto() {
        // 会起沙箱进程，与建/删 profile 的用例串行（同上）。
        let _serial = crate::sandbox::appcontainer::PROFILE_TEST_MUTEX.lock().await;
        struct OnceVeto {
            calls: Mutex<u32>,
        }
        impl StopHook for OnceVeto {
            fn before_stop(&self, _ctx: StopContext<'_>) -> Option<String> {
                *self.calls.lock().unwrap() += 1;
                Some("宿主提醒：计划态还没退出。".into())
            }
        }

        let (mut agent, store, cancel, _ws) = verify_demo(
            vec![
                script_call_with("apply_patch", r#"{"body":"pub fn f() {}"}"#, "w1"),
                script_text("写完了"),
                script_text("还是没验证"),
                script_text("结束"),
            ],
            vec!["exit 1".into()],
            6,
            crate::config::VerifyMode::Enforce,
        );
        let hook = Arc::new(OnceVeto { calls: Mutex::new(0) });
        agent.stop_hook = Some(hook.clone() as Arc<dyn StopHook>);
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;

        // 轮 2 计划否决 → 轮 3 验证否决 → 轮 4 放行。
        let events = store.lock().unwrap();
        let states = verification_states(&events);
        assert_eq!(summary.turns, 4, "实际裁决序列 {states:?}");
        assert_eq!(*hook.calls.lock().unwrap(), 1, "计划槽位只能消费一次");
        assert_eq!(
            verification_states(&events),
            vec![crate::events::VerifyState::Failed],
            "验证闸门在计划否决之后仍必须执行一次，且只执行一次"
        );
    }

    /// 否决额度是硬顶：被否决一次后，模型再次写坏也只如实披露，不得再否决。
    ///
    /// 反向对照：删掉闸门里的 `!veto_ledger.verify_used`，`turns` 会从 4 变 5。
    #[tokio::test]
    async fn verification_gate_vetoes_at_most_once_then_discloses() {
        // 会起沙箱进程，与建/删 profile 的用例串行（同上）。
        let _serial = crate::sandbox::appcontainer::PROFILE_TEST_MUTEX.lock().await;
        let (agent, store, cancel, _ws) = verify_demo(
            vec![
                script_call_with(
                    "apply_patch",
                    r#"{"path":"src/lib.rs","body":"pub fn f() {}"}"#,
                    "w1",
                ),
                script_text("写完了"),
                script_call_with(
                    "apply_patch",
                    r#"{"path":"src/lib2.rs","body":"pub fn g() {}"}"#,
                    "w2",
                ),
                script_text("还是写完了"),
                // 若第二票仍能否决，这个回合会被消费掉并让 `turns` 变 5。
                script_text("结束"),
            ],
            vec!["exit 1".into()],
            6,
            crate::config::VerifyMode::Enforce,
        );
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;

        assert_eq!(summary.reason, TurnEndReason::Completed);
        let events = store.lock().unwrap();
        let states = verification_states(&events);
        assert_eq!(
            summary.turns, 4,
            "第二次失败不得再否决（硬顶每任务一票）；实际裁决序列 {states:?}"
        );
        assert_eq!(
            states,
            vec![
                crate::events::VerifyState::Failed,
                crate::events::VerifyState::Failed
            ]
        );
        assert!(completed_unverified(&events), "第二次失败必须如实标记未验证");
    }

    /// 环境故障算「判不出来」，不得否决——模型改代码也修不好。
    ///
    /// 反向对照：把 `is_env_failure` 的特征串摘空，本条会退化成 `Failed` 并否决。
    #[tokio::test]
    async fn verification_gate_does_not_veto_on_environment_failure() {
        // 会起沙箱进程，与建/删 profile 的用例串行（同上）。
        let _serial = crate::sandbox::appcontainer::PROFILE_TEST_MUTEX.lock().await;
        let (agent, store, cancel, _ws) = verify_demo(
            vec![
                script_call_with("apply_patch", r#"{"body":"pub fn f() {}"}"#, "w1"),
                script_text("写完了"),
                script_text("不该跑到"),
            ],
            vec!["echo Could not resolve host: example.invalid & exit /b 1".into()],
            4,
            crate::config::VerifyMode::Enforce,
        );
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;

        let events = store.lock().unwrap();
        assert_eq!(summary.turns, 2, "环境故障不得否决续跑");
        assert_eq!(
            verification_states(&events),
            vec![crate::events::VerifyState::Inconclusive(
                crate::events::InconclusiveReason::EnvFailure
            )]
        );
        assert!(completed_unverified(&events), "环境故障也必须如实标记未验证");
    }

    #[tokio::test]
    async fn verification_gate_counts_terminal_pass_without_rerunning() {
        let (agent, store, cancel, _ws) = verify_demo(
            vec![
                script_call_with("apply_patch", r#"{"body":"pub fn f() {}"}"#, "w1"),
                script_call_with("read_file", "{}", "r1"),
                script_text("跑过了"),
            ],
            vec!["cargo test".into()],
            6,
            crate::config::VerifyMode::Enforce,
        );
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;

        assert_eq!(summary.reason, TurnEndReason::Completed);
        assert_eq!(summary.turns, 3);
        let events = store.lock().unwrap();
        assert_eq!(
            verification_states(&events),
            vec![crate::events::VerifyState::Passed]
        );
        let (duration_ms, text) = events
            .iter()
            .find_map(|event| match event {
                EventMsg::VerificationResult { duration_ms, summary, .. } => {
                    Some((*duration_ms, summary.clone()))
                }
                _ => None,
            })
            .expect("必须发 VerificationResult");
        assert_eq!(duration_ms, 0, "通过计入不得再跑一遍命令");
        assert!(text.contains("终端"), "summary 必须说明来源：{text}");
        assert!(!completed_unverified(&events));
    }

    /// 回归（挂死）：验证命令永不返回时，回合必须到点收割，不得被挂住，也不得误否决。
    ///
    /// 时间账分两处量，别混在一起：
    /// - **闸门自己的时限**（1s 到点收割）由 `verify::tests::run_commands_harvests_at_deadline`
    ///   钉住——那条不起沙箱，断言才不随机器漂移；
    /// - **本用例**只管循环级事实：回合返回了、裁决是 `timeout`、没有被当成失败否决。
    ///   外层时限是「挂死」的兜底，不是性能断言：沙箱起一个进程本身就可能要分钟级
    ///   （AppContainer 建 profile + 工作区 ACE 链），把它写进断言等于给环境立规矩。
    #[tokio::test]
    async fn verification_gate_times_out_instead_of_hanging() {
        // 会起沙箱进程，与建/删 profile 的用例串行（同上）。
        let _serial = crate::sandbox::appcontainer::PROFILE_TEST_MUTEX.lock().await;
        let (mut agent, store, cancel, _ws) = verify_demo(
            vec![
                script_call_with("apply_patch", r#"{"body":"pub fn f() {}"}"#, "w1"),
                script_text("写完了"),
            ],
            // 纯 CPU 忙循环：不依赖网络与控制台，沙箱内也不会立刻退出。
            vec!["for /L %i in (1,1,2000000000) do @rem".into()],
            4,
            crate::config::VerifyMode::Enforce,
        );
        // 配置下限就是 1000ms（`clamped_timeout_ms`）：取最短时限，让「到点收割」这条
        // 断言有东西可测；收割时长本身由 `verify::tests` 按毫秒级钉住。
        agent.verify.timeout_ms = 1_000;
        let started = std::time::Instant::now();
        let summary = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel),
        )
        .await
        .expect("验证闸门挂住了回合（300s 未返回即失败）");
        let elapsed = started.elapsed();

        assert_eq!(summary.reason, TurnEndReason::Completed);
        assert_eq!(
            summary.turns, 2,
            "超时不算失败，不得多跑一轮（本回合耗时 {elapsed:?}）"
        );
        let events = store.lock().unwrap();
        assert_eq!(
            verification_states(&events),
            vec![crate::events::VerifyState::Inconclusive(
                crate::events::InconclusiveReason::Timeout
            )],
            "不返回的命令必须判为超时；判成 Failed 会误否决"
        );
        assert!(completed_unverified(&events), "超时不算验证通过");
    }

    #[tokio::test]
    async fn tool_call_cycle_then_complete() {
        let (agent, store, cancel, _broker) = make_loop(vec![
            vec![fn_call_ev(), usage_ev()],
            vec![ModelStreamEvent::OutputTextDelta { delta: "完成".into() }, usage_ev()],
        ]);
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.turns, 2);
        assert_eq!(summary.reason, TurnEndReason::Completed);
        let events = store.lock().unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            EventMsg::ToolCallBegin { call_id, tool, .. } if call_id == "c1" && tool == "read_file"
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            EventMsg::ToolCallEnd { call_id, exit_code: Some(0), error: None, result: Some(ToolCallResult::Ok) } if call_id == "c1"
        )));
    }

    /// 结果语义探针：同一个假工具按需返回失败/取消，用于钉住 emit 分支的 `result` 取值。
    #[derive(Debug, Clone, Copy)]
    enum ToolBehavior {
        Fail,
        Cancel,
    }

    fn make_loop_with_read_tool(
        behavior: ToolBehavior,
        turns: Vec<Vec<ModelStreamEvent>>,
    ) -> (
        AgentLoop,
        Arc<Mutex<Vec<EventMsg>>>,
        CancellationToken,
        Arc<ApprovalBroker>,
    ) {
        struct BehaviorTool {
            spec: ToolSpec,
            behavior: ToolBehavior,
        }
        #[async_trait::async_trait]
        impl ToolExecutor for BehaviorTool {
            fn spec(&self) -> &ToolSpec {
                &self.spec
            }
            async fn execute(&self, _a: Value, _c: &ToolContext) -> Result<ToolOutput, ToolError> {
                match self.behavior {
                    ToolBehavior::Fail => Ok(ToolOutput::fail("命令以非零状态退出")),
                    ToolBehavior::Cancel => Err(ToolError::Cancelled),
                }
            }
        }
        // 命名 read_file → 风险 read → 门禁自动放行，避免测试里再走审批
        let (mut agent, store, cancel, broker) = make_loop(turns);
        let mut r = ToolRouter::new();
        r.register(Arc::new(BehaviorTool {
            spec: ToolSpec::new("read_file", "假读工具", serde_json::json!({"type":"object"})),
            behavior,
        }));
        agent.router = Arc::new(r);
        (agent, store, cancel, broker)
    }

    // ---- 只读工具并行（第 4 笔） ----

    /// 慢读工具：并发证据取「同时在飞的调用峰值」，不用墙钟，避免 CI 抖动。
    struct SlowRead {
        spec: ToolSpec,
        started: Arc<std::sync::atomic::AtomicUsize>,
        finished: Arc<std::sync::atomic::AtomicUsize>,
        peak: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ToolExecutor for SlowRead {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }
        async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
            // 在飞数量 = 已启动 − 已结束；峰值落在「本次启动之后」，串行实现永远是 1。
            let now = self
                .started
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            let finished = self.finished.load(std::sync::atomic::Ordering::SeqCst);
            self.peak.fetch_max(now - finished, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            self.finished
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ToolOutput::ok("read"))
        }
    }

    fn call_ev(name: &str, arguments: &str, call_id: &str) -> ModelStreamEvent {
        ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
            name: name.into(),
            arguments: arguments.into(),
            call_id: call_id.into(),
        })
    }

    fn slow_router(
        peak: Arc<std::sync::atomic::AtomicUsize>,
    ) -> (ToolRouter, Arc<std::sync::atomic::AtomicUsize>) {
        let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut router = ToolRouter::new();
        router.register(Arc::new(SlowRead {
            spec: ToolSpec::new("read_file", "慢读", serde_json::json!({"type":"object"})),
            started: started.clone(),
            finished: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            peak,
        }));
        (router, started)
    }

    /// 连续只读调用必须并发在飞，且事件与结果按原调用顺序回写。
    #[tokio::test]
    async fn consecutive_read_only_calls_run_in_parallel_and_replay_in_order() {
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (mut agent, store, cancel, _broker) = make_loop(vec![
            vec![
                call_ev("read_file", r#"{"path":"a"}"#, "r1"),
                call_ev("read_file", r#"{"path":"b"}"#, "r2"),
                usage_ev(),
            ],
            script_text("读完了"),
        ]);
        let (router, _started) = slow_router(peak.clone());
        agent.router = Arc::new(router);
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;

        assert_eq!(summary.reason, TurnEndReason::Completed);
        assert_eq!(
            peak.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "两条只读调用必须同时在飞（串行实现这里峰值只会是 1）"
        );
        let events = store.lock().unwrap();
        let begins: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                EventMsg::ToolCallBegin { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .collect();
        let ends: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                EventMsg::ToolCallEnd { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(begins, ["r1", "r2"], "Begin 必须按原调用顺序发出");
        assert_eq!(
            ends,
            ["r1", "r2"],
            "结果必须按原调用顺序回写，而不是按完成先后（完成顺序由调度决定）"
        );
    }

    /// 写入是分隔符：它不能被并进只读批次，两侧的读各自独立。
    #[tokio::test]
    async fn write_between_reads_stays_serial() {
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (mut agent, store, cancel, _broker) = make_loop(vec![
            vec![
                call_ev("read_file", r#"{"path":"a"}"#, "r1"),
                call_ev("apply_patch", r#"{"path":"src/x.rs","body":"x"}"#, "w1"),
                call_ev("read_file", r#"{"path":"b"}"#, "r2"),
                usage_ev(),
            ],
            script_text("完成"),
        ]);
        let ws = crate::test_support::TestTempDir::new("parallel-order");
        agent.workspace_root = ws.path().to_path_buf();
        let (mut router, _started) = slow_router(peak.clone());
        router.register(Arc::new(FakeWriter::default()));
        agent.router = Arc::new(router);
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;

        assert_eq!(summary.reason, TurnEndReason::Completed);
        assert_eq!(
            peak.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "写入两侧的读必须各自串行，不能被并成一批"
        );
        let events = store.lock().unwrap();
        let order: Vec<String> = events
            .iter()
            .filter_map(|event| match event {
                EventMsg::ToolCallBegin { call_id, .. } => Some(format!("begin:{call_id}")),
                EventMsg::ToolCallEnd { call_id, .. } => Some(format!("end:{call_id}")),
                _ => None,
            })
            .collect();
        assert_eq!(
            order,
            ["begin:r1", "end:r1", "begin:w1", "end:w1", "begin:r2", "end:r2"],
            "写入必须夹在两段只读之间串行，不能被并行批次吞进去"
        );
    }

    /// 取消必须传播到正在并发的读上：不等 150ms×2 跑完，且每条调用都有输出。
    #[tokio::test]
    async fn cancelling_a_read_batch_propagates_and_reports_every_call() {
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (mut agent, store, cancel, _broker) = make_loop(vec![
            vec![
                call_ev("read_file", r#"{"path":"a"}"#, "r1"),
                call_ev("read_file", r#"{"path":"b"}"#, "r2"),
                usage_ev(),
            ],
            script_text("不该跑到"),
        ]);
        let (router, _started) = slow_router(peak);
        agent.router = Arc::new(router);
        let stopper = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            stopper.cancel();
        });
        let summary = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel),
        )
        .await
        .expect("取消必须立即传播，不能等两条读跑完");

        assert_eq!(summary.reason, TurnEndReason::Cancelled);
        let events = store.lock().unwrap();
        let cancelled: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                EventMsg::ToolCallEnd {
                    call_id,
                    result: Some(ToolCallResult::Cancelled),
                    ..
                } => Some(call_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            cancelled,
            ["r1", "r2"],
            "取消后每条 FunctionCall 都要有对应输出，协议不能留空洞"
        );
    }

    #[tokio::test]
    async fn failed_tool_emits_result_error() {
        let (agent, store, cancel, _broker) = make_loop_with_read_tool(
            ToolBehavior::Fail,
            vec![
                vec![fn_call_ev(), usage_ev()],
                vec![ModelStreamEvent::OutputTextDelta { delta: "继续".into() }, usage_ev()],
            ],
        );
        agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        let events = store.lock().unwrap();
        // 非零退出：exit_code 是证据字段，result 才是语义判据
        assert!(events.iter().any(|e| matches!(
            e,
            EventMsg::ToolCallEnd { call_id, exit_code: Some(1), result: Some(ToolCallResult::Error), .. }
                if call_id == "c1"
        )));
    }

    #[tokio::test]
    async fn cancelled_tool_emits_result_cancelled() {
        let (agent, store, cancel, _broker) = make_loop_with_read_tool(
            ToolBehavior::Cancel,
            vec![vec![fn_call_ev(), usage_ev()]],
        );
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.reason, TurnEndReason::Cancelled);
        let events = store.lock().unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            EventMsg::ToolCallEnd { call_id, exit_code: None, result: Some(ToolCallResult::Cancelled), .. }
                if call_id == "c1"
        )));
    }

    /// 压缩可见性：自动压缩必须留下 `started → summarized → ended` 三条事件。
    /// 只打日志的实现下，用户唯一能观察到的是"上下文忽然变短"——说不出为什么、
    /// 丢了哪些、丢了多少。
    #[tokio::test]
    async fn auto_compaction_emits_started_summarized_ended_in_order() {
        let (mut agent, store, cancel, _broker) = make_loop(vec![
            // 第 1 轮：一个工具调用 + 端点报回大用量 → 第 2 轮开头判定超预算
            vec![fn_call_ev(), big_usage_ev()],
            // 压缩请求（不带工具声明）：只回摘要文本
            vec![
                ModelStreamEvent::OutputTextDelta { delta: "摘要正文".into() },
                usage_ev(),
            ],
            // 压缩之后真正的第 2 轮采样
            vec![
                ModelStreamEvent::OutputTextDelta { delta: "完成".into() },
                usage_ev(),
            ],
        ]);
        // 预算 100：第 1 轮开头（估算 24 token）不触发；第 2 轮开头观测到 50 万才触发
        agent.token_budget = 100;
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        let events = store.lock().unwrap();
        assert_eq!(summary.reason, TurnEndReason::Completed, "事件流：{events:?}");

        let compaction: Vec<&EventMsg> = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    EventMsg::CompactionStarted { .. }
                        | EventMsg::CompactionSummarized { .. }
                        | EventMsg::CompactionEnded { .. }
                )
            })
            .collect();
        // 顺序是契约的一部分：ended 必须先说明成败，才轮到摘要成为"当前历史"
        assert!(
            matches!(compaction.as_slice(), [
                EventMsg::CompactionStarted { .. },
                EventMsg::CompactionSummarized { .. },
                EventMsg::CompactionEnded { .. },
            ]),
            "压缩三条事件缺失或乱序：{events:?}"
        );

        let EventMsg::CompactionStarted {
            compaction_id,
            trigger,
            range_start,
            range_end,
            estimated_tokens,
            budget_tokens,
            turn,
        } = compaction[0]
        else {
            unreachable!()
        };
        assert_eq!(*trigger, crate::events::CompactionTrigger::Pressure, "自动路径的触发原因是预算压力");
        assert_eq!(*range_start, 0, "压缩从最旧的一条起算");
        assert!(*range_end >= 1, "至少要压掉一条，且不能把全部历史压空");
        assert_eq!(*estimated_tokens, 500_000, "触发量取端点观测值，而不是偏低的字节估算");
        assert_eq!(*budget_tokens, 100);
        assert_eq!(*turn, Some(2), "压缩发生在第 2 轮采样之前");

        let EventMsg::CompactionSummarized {
            compaction_id: summarized_id,
            shadowed_items,
            shadowed_tokens,
            summary,
            ..
        } = compaction[1]
        else {
            unreachable!()
        };
        assert_eq!(summarized_id, compaction_id, "三条事件必须配成同一组");
        assert_eq!(*shadowed_items, *range_end - *range_start, "被替换的条目数要与区间一致");
        assert!(*shadowed_tokens > 0, "被丢掉的量必须可量化，不能只报条目数");
        assert_eq!(summary, "摘要正文");

        let EventMsg::CompactionEnded { ok, error, history_tokens_after, .. } = compaction[2] else {
            unreachable!()
        };
        assert!(*ok);
        assert_eq!(*error, None);
        assert!(history_tokens_after.is_some(), "成功后要给出压缩后的历史量");
        // 成功路径不许额外报错
        assert!(
            !events.iter().any(|e| matches!(e, EventMsg::Error { .. })),
            "压缩成功不应产出 error 事件：{events:?}"
        );
    }

    /// 压缩期间按停止（含"端点还没回响应头"这一段）：必须收口为**取消**，不是失败。
    ///
    /// 反向对照：把 `CompactAbort::Cancelled` 改回按失败处理，本用例立刻变红
    /// （多出 `EventMsg::Error`、回合被记成 `Failed`）。
    #[tokio::test]
    async fn cancel_during_compaction_ends_as_cancelled_not_failed() {
        struct HangingCompactionBackend {
            calls: std::sync::atomic::AtomicUsize,
            entered: Arc<tokio::sync::Notify>,
        }

        #[async_trait::async_trait]
        impl ModelBackend for HangingCompactionBackend {
            async fn stream(
                &self,
                _history: &[HistoryItem],
                _tools: &[ToolDeclaration],
                _instructions: &str,
            ) -> Result<ModelStream, crate::model_client::ModelError> {
                if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    // 第 1 轮采样：工具调用 + 大用量，让压缩在**第 2 轮开头**触发
                    return Ok(Box::pin(futures::stream::iter(vec![fn_call_ev(), big_usage_ev()])));
                }
                // 第 2 次调用 = 压缩请求：先告知测试再挂住，模拟端点不回响应头
                self.entered.notify_one();
                futures::future::pending().await
            }
        }

        let entered = Arc::new(tokio::sync::Notify::new());
        let (mut agent, store, cancel, _broker) = make_loop_with_backend(Arc::new(
            HangingCompactionBackend {
                calls: std::sync::atomic::AtomicUsize::new(0),
                entered: entered.clone(),
            },
        ));
        agent.token_budget = 100;
        let run = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel.clone());
        tokio::pin!(run);

        // 等压缩请求真的发出去了再取消：提前取消测的是别的东西（还没开始压）
        tokio::select! {
            _ = entered.notified() => {}
            _ = &mut run => panic!("压缩请求未发出就结束了：{:?}", store.lock().unwrap()),
        }
        cancel.cancel();
        let summary = tokio::time::timeout(std::time::Duration::from_secs(1), run)
            .await
            .expect("取消必须能打断压缩请求的建连；超时 = 任务槽被压缩永久占用");
        assert_eq!(summary.reason, TurnEndReason::Cancelled, "用户取消不是失败轮");

        let events = store.lock().unwrap();
        assert!(
            events.iter().any(|e| matches!(
                e,
                EventMsg::CompactionEnded { ok: false, error: Some(message), .. }
                    if message == "压缩被用户取消"
            )),
            "取消也要落到 ended，并如实写明原因：{events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(e, EventMsg::CompactionSummarized { .. })),
            "取消时没有摘要，不许伪造 summarized：{events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(e, EventMsg::Error { .. })),
            "取消不是错误，不许发 error 事件：{events:?}"
        );
    }

    #[tokio::test]
    async fn budget_exhausted_when_tools_keep_coming() {
        let (agent, store, cancel, _broker) = make_loop(vec![
            vec![fn_call_ev(), usage_ev()],
            vec![fn_call_ev(), usage_ev()],
            vec![fn_call_ev(), usage_ev()],
        ]);
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.reason, TurnEndReason::BudgetExhausted);
        assert!(store.lock().unwrap().iter().any(|e| matches!(
            e,
            EventMsg::Error { message } if message.contains("最大轮数")
        )));
    }

    #[tokio::test]
    async fn tree_budget_exhaustion_blocks_the_next_model_request() {
        // 每轮用量 15（10 输入 + 5 输出）；上限 20 → 第 2 轮结束后累计 30，
        // 第 3 轮必须在**发起采样之前**被拦下（FF-103 的执行点）。
        let (mut agent, store, cancel, _broker) = make_loop(vec![
            vec![fn_call_ev(), usage_ev()],
            vec![fn_call_ev(), usage_ev()],
            vec![ModelStreamEvent::OutputTextDelta { delta: "不该到这里".into() }, usage_ev()],
        ]);
        agent.max_turns = 5; // 排除"轮数上限"这条路径，只测树级预算
        agent.tree_budget = TreeBudget::new(Some(20), None);
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.reason, TurnEndReason::BudgetExhausted);
        assert!(store.lock().unwrap().iter().any(|e| matches!(
            e,
            EventMsg::Error { message } if message.contains("树级 token 预算")
        )));
        // 用量停在 30 = 只发生过两次采样；第 3 段脚本从未被消费。
        assert_eq!(agent.tree_budget.tokens_spent(), 30);
        assert!(store
            .lock()
            .unwrap()
            .iter()
            .all(|e| !matches!(e, EventMsg::AgentMessageDelta { delta } if delta == "不该到这里")));
    }

    #[tokio::test]
    async fn cancelled_token_yields_cancelled_completion() {
        // 空脚本 = 挂起的事件流：预取消后 select! 只有取消分支就绪（确定性）
        let (agent, store, cancel, _broker) = make_loop(vec![vec![]]);
        cancel.cancel();
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.reason, TurnEndReason::Cancelled);
        assert!(store.lock().unwrap().iter().any(|e| matches!(
            e,
            EventMsg::TurnCompleted { reason: TurnEndReason::Cancelled, .. }
        )));
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_while_opening_stream_stops_promptly() {
        // 端点连响应头都不回：修复前 `stream().await` 不参与取消竞争，任务永久卡死
        // （任务槽不释放、后续任务全被拒）。
        //
        // 时序走**虚拟时钟**而不是墙钟：CI 的 windows-latest 上 732 个用例并行，测试线程
        // 被饿住时墙钟照样走，2s 预算会假红（run 42715231949 就是这么红的）。
        // 判断强度不降——两段互为反向对照：
        //   ① 无取消信号：推 10 分钟虚拟时间也不许返回（提前返回 = 测的不是建连阶段）；
        //   ② 注入取消：必须收口为 Cancelled，且不靠时钟推进。
        // 这里敢用虚拟时钟，是因为 `HangingBackend` 是纯内存的 `pending()`，
        // 没有 socket 一类真实 I/O——否则时间会被瞬间跳完，用例变成空转。
        let (agent, store, cancel, _broker) = make_loop_with_backend(Arc::new(HangingBackend));
        let run = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel.clone());
        tokio::pin!(run);

        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(600), &mut run).await.is_err(),
            "无取消信号时必须一直挂在 stream() 上；提前返回说明断言打在了错的阶段"
        );

        cancel.cancel();
        let summary = tokio::time::timeout(std::time::Duration::from_secs(1), run)
            .await
            .expect("取消必须能打断 stream() 的等待；超时 = 任务槽被永久占用");
        assert_eq!(summary.reason, TurnEndReason::Cancelled);
    }


    /// 审计 H3：写入失败时游标必须停在失败条目前；连续失败触顶后返回硬错误，
    /// 而不是把游标推到 `history.len()` 让这些条目永久不落盘。
    #[test]
    fn archive_cursor_stops_at_the_failed_item_and_hard_fails_at_threshold() {
        let history = vec![
            HistoryItem::Message { role: Role::User, text: "a".into(), reasoning: None },
            HistoryItem::Message { role: Role::User, text: "b".into(), reasoning: None },
        ];
        let mut recorded = 0usize;
        let mut calls = 0u32;
        let err = append_pending(&history, &mut recorded, |_item| {
            calls += 1;
            // 第一条成功，第二条永久失败
            if calls == 1 {
                Ok(())
            } else {
                Err(crate::rollout::RolloutError::Io("磁盘满".into()))
            }
        })
        .unwrap_err();
        assert_eq!(recorded, 1, "游标必须停在失败条目前，不能越过它");
        assert_eq!(calls, 1 + MAX_ROLLOUT_WRITE_ATTEMPTS, "失败条目应按阈值重试");
        assert!(err.contains("连续 3 次"), "{err}");
    }

    /// 单次抖动（文件被短暂占用）重试成功后必须继续推进游标，不能把瞬时故障当永久损坏。
    #[test]
    fn archive_write_retries_transient_failure_then_advances() {
        let history =
            vec![HistoryItem::Message { role: Role::User, text: "a".into(), reasoning: None }];
        let mut recorded = 0usize;
        let mut calls = 0u32;
        append_pending(&history, &mut recorded, |_| {
            calls += 1;
            if calls == 1 {
                Err(crate::rollout::RolloutError::Io("瞬时占用".into()))
            } else {
                Ok(())
            }
        })
        .unwrap();
        assert_eq!(recorded, 1, "重试成功后游标必须前进");
        assert_eq!(calls, 2);
    }

    /// D3 反向对照：压缩锚点连续写失败必须变成 `Err`，让调用方放弃本次压缩。
    /// 改回旧写法（`let _ = writer.append(...)`）时这个返回值不存在，本用例无可断言
    /// ——「历史与游标保持原样」由调用顺序保证：`append_anchor` 在 `history.splice` 之前，
    /// 提前 return 时两者都还没动。
    #[test]
    fn anchor_write_failure_is_reported_after_retries() {
        let mut calls = 0u32;
        let error = append_anchor(
            crate::compaction::Target { range_start: 0, range_end: 2 },
            "摘要",
            |_| {
                calls += 1;
                Err(crate::rollout::RolloutError::Io("磁盘满".into()))
            },
        )
        .expect_err("连续写失败必须上抛，不能当成功");
        assert_eq!(
            calls, MAX_ROLLOUT_WRITE_ATTEMPTS,
            "必须重试满次数再放弃，避免一次瞬时占用就把压缩作废"
        );
        assert!(error.contains("本次压缩未生效"), "文案要说明压缩没有发生：{error}");
    }

    /// 压缩有两份实现：回合内的内存替换（[`replace_with_summary`]）与 `rollout::resume`
    /// 重放锚点。两者必须产出同一份历史——这正是「档案与运行态分叉」最容易复发的地方。
    /// 断言逐项相等（`Vec<HistoryItem>` 的 PartialEq），不是比长度。
    ///
    /// 反向对照：把 resume 侧摘要的 role 改成 `System`（或让区间多留一条原消息），本用例红。
    #[test]
    fn memory_compaction_matches_resume_replay() {
        use crate::rollout::resume::resume;
        use crate::rollout::{ConfigSnapshot, RolloutItem, RolloutWriter};

        let dir = crate::test_support::TestTempDir::new("compact-replay");
        let items: Vec<HistoryItem> = vec![
            HistoryItem::Message { role: Role::User, text: "u0".into(), reasoning: None },
            HistoryItem::Message {
                role: Role::Assistant,
                text: "a1".into(),
                reasoning: Some("思考 1".into()),
            },
            HistoryItem::Message { role: Role::User, text: "u2".into(), reasoning: None },
            HistoryItem::Message { role: Role::Assistant, text: "a3".into(), reasoning: None },
            HistoryItem::Message { role: Role::User, text: "u4".into(), reasoning: None },
        ];
        let meta = RolloutItem::SessionMeta {
            session_id: "sess".into(),
            agent_id: "agent".into(),
            parent_id: None,
            created_at: 0,
            config_snapshot: ConfigSnapshot::default(),
        };
        let writer = RolloutWriter::create(&dir, &meta).unwrap();
        for item in &items {
            writer
                .append(&RolloutItem::ResponseItem { item: item.clone(), at_ms: 0 })
                .unwrap();
        }
        // 两次锚点：第二次的区间基于第一次压缩**之后**的历史，步骤语义必须与运行态一致。
        let first = crate::compaction::Target { range_start: 1, range_end: 4 };
        writer
            .append(&RolloutItem::Compacted {
                range_start: first.range_start,
                range_end: first.range_end,
                summary: "第一次交接摘要".into(),
            })
            .unwrap();
        let second = crate::compaction::Target { range_start: 0, range_end: 2 };
        writer
            .append(&RolloutItem::Compacted {
                range_start: second.range_start,
                range_end: second.range_end,
                summary: "第二次交接摘要".into(),
            })
            .unwrap();
        let path = writer.path().to_path_buf();
        drop(writer);

        let resumed = resume(&path).unwrap();
        assert_eq!(resumed.applied_compactions, 2, "两条锚点都应生效");

        let mut memory = items.clone();
        replace_with_summary(&mut memory, first, "第一次交接摘要");
        replace_with_summary(&mut memory, second, "第二次交接摘要");

        assert_eq!(memory, resumed.history, "内存压缩与档案重放必须逐项相等");
        assert!(
            matches!(
                &memory[0],
                HistoryItem::Message { role: Role::User, text, .. } if text == "第二次交接摘要"
            ),
            "摘要必须以 user 身份占据区间位置：{:?}",
            memory[0]
        );
    }

    /// A0-3：失败回合的档案结局必须写 `failed`，不能默认 `completed`。
    /// 反向对照：撤掉收口处的 TurnEnd 追加，最后一行会变回 `response_item`，本用例红。
    #[tokio::test]
    async fn turn_end_is_recorded_with_real_reason() {
        let dir = std::env::temp_dir().join(format!(
            "shaniu-a0-turnend-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = crate::rollout::RolloutItem::SessionMeta {
            session_id: "session".into(),
            agent_id: "agent".into(),
            parent_id: None,
            created_at: 0,
            config_snapshot: crate::rollout::ConfigSnapshot::default(),
        };
        let writer = Arc::new(crate::rollout::RolloutWriter::create(&dir, &meta).unwrap());
        let (mut agent, store, cancel, _broker) = make_loop(vec![vec![ModelStreamEvent::Failed {
            message: "boom".into(),
            error: None,
        }]]);
        agent.rollout = Some(writer.clone());

        agent.run_task("任务", Arc::new(CapturingSink(store)), cancel).await;

        let text = std::fs::read_to_string(writer.path()).unwrap();
        let last = text.lines().rev().find(|line| !line.trim().is_empty()).unwrap();
        match serde_json::from_str::<crate::rollout::RolloutItem>(last).unwrap() {
            crate::rollout::RolloutItem::TurnEnd { turn, reason, error, .. } => {
                assert_eq!(turn, 1);
                assert_eq!(reason, "failed");
                assert_eq!(error.expect("失败结局必须带结构化原因").code, "UNKNOWN");
            }
            other => panic!("档案最后一行必须是回合结局，实际为 {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn model_failure_maps_to_failed() {
        let (agent, store, cancel, _broker) = make_loop(vec![vec![ModelStreamEvent::Failed {
            message: "boom".into(),
            error: None,
        }]]);
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.reason, TurnEndReason::Failed);
        assert!(store.lock().unwrap().iter().any(|e| matches!(
            e,
            EventMsg::Error { message } if message == "boom"
        )));
        // 反向对照：撤掉 `turn_error` 的赋值，这条必红——失败回合不得"无原因"。
        assert!(store.lock().unwrap().iter().any(|e| matches!(
            e,
            EventMsg::TurnCompleted { reason: TurnEndReason::Failed, error: Some(info), .. }
                if info.code == "UNKNOWN"
        )));
    }

    /// P0-1：流在完成标记之前安静结束，不得当成正常完成。
    ///
    /// 反向对照：旧逻辑把 `None`（EOF）直接 `break` 并按成功收口，这条会红在
    /// `TurnEndReason::Completed` 上——半截回答还会被写进历史。
    #[tokio::test]
    async fn eof_without_terminal_marker_fails_and_keeps_half_answer_out_of_history() {
        let dir = std::env::temp_dir().join(format!("shaniu-eof-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = crate::rollout::RolloutItem::SessionMeta {
            session_id: "session".into(),
            agent_id: "agent".into(),
            parent_id: None,
            created_at: 0,
            config_snapshot: crate::rollout::ConfigSnapshot::default(),
        };
        let writer = Arc::new(crate::rollout::RolloutWriter::create(&dir, &meta).unwrap());
        let (mut agent, store, cancel, _broker) = make_loop(vec![vec![
            ModelStreamEvent::OutputTextDelta { delta: "半截回答".into() },
        ]]);
        agent.rollout = Some(writer.clone());

        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;

        assert_eq!(summary.reason, TurnEndReason::Failed, "没有终态标记的 EOF 不是成功");
        let events = store.lock().unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            EventMsg::AgentMessageDelta { delta } if delta == "半截回答"
        )));
        let info = events
            .iter()
            .find_map(|event| match event {
                EventMsg::TurnCompleted { reason: TurnEndReason::Failed, error: Some(info), .. } => {
                    Some(info)
                }
                _ => None,
            })
            .expect("断流必须带结构化失败原因");
        assert_eq!(info.code, "NETWORK", "断流应归类为连接中断，而不是 UNKNOWN");
        drop(events);

        let archive = crate::rollout::read(writer.path()).unwrap();
        assert!(
            !archive.items.iter().any(|item| matches!(
                item,
                crate::rollout::RolloutItem::ResponseItem {
                    item: HistoryItem::Message { role: Role::Assistant, .. },
                    ..
                }
            )),
            "半截助手消息不得落进历史"
        );
        assert!(
            archive.items.iter().any(|item| matches!(
                item,
                crate::rollout::RolloutItem::TurnEnd { .. }
            )),
            "档案结局必须写成 failed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P0-2：压缩流中途报错时，半截摘要不得替换历史。
    ///
    /// 反向对照：旧循环把 `Failed` 丢进 `Some(_) => {}`，摘要非空就照常提交压缩锚点，
    /// 这条会红在 `TurnEndReason::Failed` 与「不得产出摘要」两条断言上。
    #[tokio::test]
    async fn failed_compaction_stream_keeps_history_and_reports_reason() {
        let dir = std::env::temp_dir().join(format!("shaniu-compact-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = crate::rollout::RolloutItem::SessionMeta {
            session_id: "session".into(),
            agent_id: "agent".into(),
            parent_id: None,
            created_at: 0,
            config_snapshot: crate::rollout::ConfigSnapshot::default(),
        };
        let writer = Arc::new(crate::rollout::RolloutWriter::create(&dir, &meta).unwrap());
        let (mut agent, store, cancel, _broker) = make_loop(vec![
            vec![fn_call_ev(), big_usage_ev()],
            // 压缩流：先吐了半截摘要，然后断流
            vec![
                ModelStreamEvent::OutputTextDelta { delta: "半截摘要".into() },
                ModelStreamEvent::Failed {
                    message: "压缩断流".into(),
                    error: Some(crate::model_client::ModelError::stream_broken("压缩断流")),
                },
            ],
            // 旧实现会继续采样；给一段正常答案，让旧行为能收口（而不是挂住）
            vec![ModelStreamEvent::OutputTextDelta { delta: "完成".into() }, usage_ev()],
        ]);
        agent.token_budget = 100;
        agent.rollout = Some(writer.clone());

        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;

        assert_eq!(summary.reason, TurnEndReason::Failed, "压缩失败应让回合失败");
        let events = store.lock().unwrap();
        assert!(
            !events.iter().any(|e| matches!(e, EventMsg::CompactionSummarized { .. })),
            "中途失败的压缩不得产出摘要"
        );
        assert!(events.iter().any(|e| matches!(e, EventMsg::CompactionEnded { ok: false, .. })));
        let info = events
            .iter()
            .find_map(|event| match event {
                EventMsg::TurnCompleted { error: Some(info), .. } => Some(info),
                _ => None,
            })
            .expect("压缩失败必须带结构化原因");
        assert_eq!(info.code, "COMPACTION_FAILED");
        drop(events);

        let archive = crate::rollout::read(writer.path()).unwrap();
        assert!(
            !archive
                .items
                .iter()
                .any(|item| matches!(item, crate::rollout::RolloutItem::Compacted { .. })),
            "压缩锚点不得写入，原历史必须保持可用"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P0-2：会话引用展开失败原先只发 `Error`，界面只能落到 UNKNOWN。
    #[tokio::test]
    async fn session_reference_expansion_failure_reports_turn_error() {
        let dir = std::env::temp_dir().join(format!("shaniu-sref-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = crate::rollout::RolloutItem::SessionMeta {
            session_id: "self".into(),
            agent_id: "self".into(),
            parent_id: None,
            created_at: 0,
            config_snapshot: crate::rollout::ConfigSnapshot::default(),
        };
        let writer = Arc::new(crate::rollout::RolloutWriter::create(&dir, &meta).unwrap());
        let (mut agent, store, cancel, _broker) = make_loop(vec![]);
        agent.rollout = Some(writer);

        let task = format!(
            "请参考 @[缺失会话]({}) 回答",
            crate::session_reference::encode_uri("missing-session")
        );
        let summary = agent
            .run_with_history(
                vec![HistoryItem::Message { role: Role::User, text: task, reasoning: None }],
                0,
                Arc::new(CapturingSink(store.clone())),
                cancel,
            )
            .await;

        assert_eq!(summary.reason, TurnEndReason::Failed);
        let events = store.lock().unwrap();
        let info = events
            .iter()
            .find_map(|event| match event {
                EventMsg::TurnCompleted { error: Some(info), .. } => Some(info),
                _ => None,
            })
            .expect("展开失败必须带结构化原因");
        assert_eq!(info.code, "CONTEXT_EXPAND_FAILED");
        assert!(!info.message.is_empty(), "原因文案不得为空");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 审计 A0：401 必须带着分类码与原始 code/message 进入 `TurnCompleted`，
    /// 否则 UI 只能显示"回合以 failed 结束"、导出也无法对账。
    #[tokio::test]
    async fn auth_failure_carries_structured_error() {
        let (agent, store, cancel, _broker) = make_loop(vec![vec![ModelStreamEvent::Failed {
            message: "Invalid API key".into(),
            error: Some(crate::model_client::ModelError::from_status(
                401,
                r#"{"error":{"code":"invalid_api_key","message":"Invalid API key"}}"#,
            )),
        }]]);
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.reason, TurnEndReason::Failed);
        let events = store.lock().unwrap();
        let info = events
            .iter()
            .find_map(|event| match event {
                EventMsg::TurnCompleted { error: Some(info), .. } => Some(info),
                _ => None,
            })
            .expect("失败回合必须带结构化错误");
        assert_eq!(info.code, "AUTH");
        assert_eq!(info.message, "密钥无效");
        assert_eq!(info.http_status, Some(401));
        assert_eq!(info.provider_code.as_deref(), Some("invalid_api_key"));
        assert_eq!(info.provider_message.as_deref(), Some("Invalid API key"));
    }

    #[tokio::test]
    async fn unknown_tool_error_backfills_and_continues() {
        let (agent, store, cancel, _broker) = make_loop(vec![
            vec![ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                name: "no_such_tool".into(),
                arguments: "{}".into(),
                call_id: "c9".into(),
            }), usage_ev()],
            vec![ModelStreamEvent::OutputTextDelta { delta: "已纠正".into() }, usage_ev()],
        ]);
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.reason, TurnEndReason::Completed);
        let events = store.lock().unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            EventMsg::ToolCallEnd { call_id, error: Some(msg), .. } if call_id == "c9" && msg.contains("未知工具")
        )));
    }

    #[tokio::test]
    async fn destructive_tool_requires_approval_and_deny_blocks_execution() {
        // wipe_disk 是注册过的假想破坏性工具（未知工具名走路由拒绝路径，见另一用例）
        let (agent, store, cancel, broker) = make_loop_with_destructive(vec![
            vec![ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                name: "wipe_disk".into(),
                arguments: "{}".into(),
                call_id: "c7".into(),
            }), usage_ev()],
            vec![ModelStreamEvent::OutputTextDelta { delta: "已纠正".into() }, usage_ev()],
        ]);
        let sink = Arc::new(CapturingSink(store.clone()));
        let run = tokio::spawn(async move { agent.run_task("任务", sink, cancel).await });
        // 等待审批请求出现后回执「拒绝」
        for _ in 0..300 {
            if broker.has_pending("appr-c7") {
                assert!(broker.resolve("appr-c7", crate::events::ApprovalDecision::Deny));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let summary = run.await.unwrap();
        assert_eq!(summary.reason, TurnEndReason::Completed);
        let events = store.lock().unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            EventMsg::ExecApprovalRequest { request_id, risk: crate::events::RiskLevel::Destructive, .. }
                if request_id == "appr-c7"
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            EventMsg::ToolCallEnd { call_id, error: Some(msg), result: Some(ToolCallResult::Denied), .. }
                if call_id == "c7" && msg.contains("用户拒绝")
        )));
    }

    /// SH-125 接线：命令工具失败后，下一轮请求里必须多出一条带「文件:行」的 System 提醒。
    #[tokio::test]
    async fn command_failure_injects_repair_reminder_with_file_line() {
        struct FailingExec {
            spec: ToolSpec,
        }
        impl Default for FailingExec {
            fn default() -> Self {
                Self {
                    spec: ToolSpec::new(
                        "unified_exec",
                        "假命令",
                        serde_json::json!({"type":"object"}),
                    ),
                }
            }
        }
        #[async_trait::async_trait]
        impl ToolExecutor for FailingExec {
            fn spec(&self) -> &ToolSpec {
                &self.spec
            }
            async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput::fail(
                    "error[E0308]: mismatched types\n --> src/lib.rs:4:18\n expected `i32`",
                ))
            }
        }

        struct RecordingBackend {
            seen: Arc<Mutex<Vec<Vec<HistoryItem>>>>,
            script: Mutex<VecDeque<Vec<ModelStreamEvent>>>,
        }
        #[async_trait::async_trait]
        impl ModelBackend for RecordingBackend {
            async fn stream(
                &self,
                history: &[HistoryItem],
                _tools: &[ToolDeclaration],
                _instructions: &str,
            ) -> Result<ModelStream, crate::model_client::ModelError> {
                self.seen.lock().unwrap().push(history.to_vec());
                let mut queue = self.script.lock().unwrap();
                match queue.pop_front() {
                    Some(script) => Ok(Box::pin(futures::stream::iter(script))),
                    None => Ok(Box::pin(futures::stream::pending())),
                }
            }
        }

        let seen = Arc::new(Mutex::new(Vec::new()));
        let backend = Arc::new(RecordingBackend {
            seen: seen.clone(),
            script: Mutex::new(VecDeque::from(vec![
                vec![
                    ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                        name: "unified_exec".into(),
                        arguments: "{}".into(),
                        call_id: "c1".into(),
                    }),
                    usage_ev(),
                ],
                vec![
                    ModelStreamEvent::OutputTextDelta { delta: "已修复".into() },
                    usage_ev(),
                ],
            ])),
        });
        let mut router = ToolRouter::new();
        router.register(Arc::new(FailingExec::default()));
        let (_sink, store) = CapturingSink::new();
        let broker = Arc::new(ApprovalBroker::new());
        let agent = AgentLoop {
            backend,
            router: Arc::new(router),
            broker: broker.clone(),
            plan_state: Arc::new(PlanState::new()),
            workspace_root: std::env::temp_dir(),
            max_turns: 3,
            token_budget: 0,
            tree_budget: TreeBudget::unlimited(),
            policy: Arc::new(policy::SessionPolicyHandle::new(policy::SessionPolicy {
                approval: crate::config::ApprovalMode::OnRequest,
                sandbox: crate::config::SandboxMode::WorkspaceWrite,
            })),
            rollout: None,
            rollout_broke: std::sync::atomic::AtomicBool::new(false),
            session_id: None,
            jobs: None,
            stop_hook: None,
            verify: Default::default(),
            retry: crate::model_client::RetryPolicy::disabled(),
        };
        let sink = Arc::new(CapturingSink(store.clone()));
        let run = tokio::spawn(async move {
            agent.run_task("修好它", sink, CancellationToken::new()).await
        });
        for _ in 0..300 {
            if broker.has_pending("appr-c1") {
                assert!(broker.resolve("appr-c1", crate::events::ApprovalDecision::AllowAlways));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let summary = run.await.unwrap();
        assert_eq!(summary.reason, TurnEndReason::Completed);

        let captured = seen.lock().unwrap();
        assert!(captured.len() >= 2, "至少有失败轮与提醒后的重试轮");
        let reminder = captured[1]
            .iter()
            .find_map(|item| match item {
                HistoryItem::Message { role: Role::System, text, .. }
                    if text.contains("自动修复提醒") =>
                {
                    Some(text.clone())
                }
                _ => None,
            })
            .expect("命令失败后必须注入修复提醒");
        assert!(reminder.contains("src/lib.rs:4:18"), "{reminder}");
    }

    /// SH-126：模型靠「删掉断言」让命令变绿时，系统必须拒绝把这次当作修复成功。
    ///
    /// 这是自愈闭环最危险的一条路径：能改文件 + 能重跑命令 = 可以把测试改成永远通过。
    /// 本用例走真实接线（写日志 → 命令重跑成功 → 结算），只把两个工具换成可脚本化的假实现。
    #[tokio::test]
    async fn weakening_test_assertions_is_rejected_as_a_repair() {
        use std::sync::atomic::{AtomicU32, Ordering};

        const ORIGINAL: &str = "#[test]\nfn adds() {\n    assert_eq!(add(2, 2), 4);\n}\n";
        const WEAKENED: &str = "#[test]\nfn adds() {\n}\n";

        struct FailingOnceExec {
            spec: ToolSpec,
            calls: AtomicU32,
        }
        #[async_trait::async_trait]
        impl ToolExecutor for FailingOnceExec {
            fn spec(&self) -> &ToolSpec {
                &self.spec
            }
            async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
                if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Ok(ToolOutput::fail(
                        "test adds ... FAILED\n\
                         thread 'adds' panicked at tests/check.rs:3:5:\n\
                         assertion `left == right` failed",
                    ))
                } else {
                    Ok(ToolOutput::ok("test result: ok. 1 passed; 0 failed"))
                }
            }
        }

        struct WeakeningPatch {
            spec: ToolSpec,
            path: PathBuf,
        }
        #[async_trait::async_trait]
        impl ToolExecutor for WeakeningPatch {
            fn spec(&self) -> &ToolSpec {
                &self.spec
            }
            async fn execute(&self, _args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
                let before = std::fs::read_to_string(&self.path).unwrap_or_default();
                std::fs::write(&self.path, WEAKENED).unwrap();
                if let Some(writes) = &ctx.writes {
                    writes.record(&self.path, Some(&before), Some(WEAKENED));
                }
                Ok(ToolOutput::ok("补丁应用成功"))
            }
        }

        /// 审批请求一到就地「总是允许」，让用例专注在结算逻辑上。
        struct AutoApproveSink {
            store: Arc<Mutex<Vec<EventMsg>>>,
            broker: Arc<ApprovalBroker>,
        }
        impl EventSink for AutoApproveSink {
            fn emit(&self, msg: &EventMsg) -> Result<(), String> {
                self.store.lock().unwrap().push(msg.clone());
                if let EventMsg::ExecApprovalRequest { request_id, .. } = msg {
                    self.broker
                        .resolve(request_id, crate::events::ApprovalDecision::AllowAlways);
                }
                Ok(())
            }
        }

        let ws = crate::test_support::TestTempDir::new("tamper");
        std::fs::create_dir_all(ws.join("tests")).unwrap();
        let test_file = ws.join("tests/check.rs");
        std::fs::write(&test_file, ORIGINAL).unwrap();

        let call = |name: &str, id: &str, args: &str| {
            vec![
                ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                    name: name.into(),
                    arguments: args.into(),
                    call_id: id.into(),
                }),
                usage_ev(),
            ]
        };
        let mut router = ToolRouter::new();
        router.register(Arc::new(FailingOnceExec {
            spec: ToolSpec::new("unified_exec", "假命令", serde_json::json!({"type":"object"})),
            calls: AtomicU32::new(0),
        }));
        router.register(Arc::new(WeakeningPatch {
            spec: ToolSpec::new("apply_patch", "假补丁", serde_json::json!({"type":"object"})),
            path: test_file.clone(),
        }));
        let broker = Arc::new(ApprovalBroker::new());
        let store = Arc::new(Mutex::new(Vec::new()));
        let agent = AgentLoop {
            backend: Arc::new(ScriptedBackend::new(vec![
                call("unified_exec", "c1", "{}"),
                call("apply_patch", "c2", "{}"),
                call("unified_exec", "c3", "{}"),
                vec![ModelStreamEvent::OutputTextDelta { delta: "已修复".into() }, usage_ev()],
            ])),
            router: Arc::new(router),
            broker: broker.clone(),
            plan_state: Arc::new(PlanState::new()),
            workspace_root: ws.to_path_buf(),
            max_turns: 5,
            token_budget: 0,
            tree_budget: TreeBudget::unlimited(),
            policy: Arc::new(policy::SessionPolicyHandle::new(policy::SessionPolicy {
                approval: crate::config::ApprovalMode::OnRequest,
                sandbox: crate::config::SandboxMode::WorkspaceWrite,
            })),
            rollout: None,
            rollout_broke: std::sync::atomic::AtomicBool::new(false),
            session_id: None,
            jobs: None,
            stop_hook: None,
            verify: Default::default(),
            retry: crate::model_client::RetryPolicy::disabled(),
        };
        let sink: Arc<dyn EventSink> = Arc::new(AutoApproveSink { store: store.clone(), broker });
        let summary = agent
            .run_task("修好它", sink, CancellationToken::new())
            .await;

        assert_eq!(
            summary.reason,
            TurnEndReason::Failed,
            "靠删断言换来的绿必须按失败处理"
        );
        let events = store.lock().unwrap();
        let rejection = events
            .iter()
            .find_map(|e| match e {
                EventMsg::Error { message } if message.contains("修复被拒绝") => Some(message.clone()),
                _ => None,
            })
            .expect("必须产出可见的拒绝提示，不能静默通过");
        assert!(rejection.contains("tests/check.rs"), "{rejection}");
        // P0-2：拒绝原因必须进 `turn_error`，否则前端只能显示 UNKNOWN。
        let info = events
            .iter()
            .find_map(|event| match event {
                EventMsg::TurnCompleted { error: Some(info), .. } => Some(info),
                _ => None,
            })
            .expect("拒绝修复必须带结构化原因");
        assert_eq!(info.code, "TEST_TAMPERING");
        // 文件确实被改了（说明判定不是靠「没发生写入」侥幸通过）
        assert_eq!(std::fs::read_to_string(&test_file).unwrap(), WEAKENED);
    }

    /// SH-122 接线：连续第 4 次相同调用后，下一轮请求里必须多出一条独立 System 提醒。
    #[tokio::test]
    async fn repeated_identical_calls_inject_anomaly_reminder() {
        struct RecordingBackend {
            seen: Arc<Mutex<Vec<Vec<HistoryItem>>>>,
            script: Mutex<VecDeque<Vec<ModelStreamEvent>>>,
        }
        #[async_trait::async_trait]
        impl ModelBackend for RecordingBackend {
            async fn stream(
                &self,
                history: &[HistoryItem],
                _tools: &[ToolDeclaration],
                _instructions: &str,
            ) -> Result<ModelStream, crate::model_client::ModelError> {
                self.seen.lock().unwrap().push(history.to_vec());
                let mut queue = self.script.lock().unwrap();
                match queue.pop_front() {
                    Some(script) => Ok(Box::pin(futures::stream::iter(script))),
                    None => Ok(Box::pin(futures::stream::pending())),
                }
            }
        }

        let repeated = || {
            vec![
                ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                    name: "read_file".into(),
                    arguments: r#"{"path":"a.txt"}"#.into(),
                    call_id: format!("c{}", ulid::Ulid::generate()),
                }),
                usage_ev(),
            ]
        };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let backend = Arc::new(RecordingBackend {
            seen: seen.clone(),
            script: Mutex::new(VecDeque::from(vec![
                repeated(),
                repeated(),
                repeated(),
                repeated(),
                vec![
                    ModelStreamEvent::OutputTextDelta { delta: "停".into() },
                    usage_ev(),
                ],
            ])),
        });
        let mut router = ToolRouter::new();
        router.register(Arc::new(EchoTool::default()));
        let (_sink, store) = CapturingSink::new();
        let agent = AgentLoop {
            backend,
            router: Arc::new(router),
            broker: Arc::new(ApprovalBroker::new()),
            plan_state: Arc::new(PlanState::new()),
            workspace_root: std::env::temp_dir(),
            max_turns: 6,
            token_budget: 0,
            tree_budget: TreeBudget::unlimited(),
            policy: Arc::new(policy::SessionPolicyHandle::new(policy::SessionPolicy {
                approval: crate::config::ApprovalMode::OnRequest,
                sandbox: crate::config::SandboxMode::WorkspaceWrite,
            })),
            rollout: None,
            rollout_broke: std::sync::atomic::AtomicBool::new(false),
            session_id: None,
            jobs: None,
            stop_hook: None,
            verify: Default::default(),
            retry: crate::model_client::RetryPolicy::disabled(),
        };
        let sink = Arc::new(CapturingSink(store.clone()));
        agent
            .run_task("随便", sink, CancellationToken::new())
            .await;

        let captured = seen.lock().unwrap();
        assert!(captured.len() >= 5, "四次相同调用 + 收尾轮");
        for history in captured.iter().take(4) {
            assert!(
                !history.iter().any(|item| matches!(
                    item,
                    HistoryItem::Message { text, .. } if text.contains("空转提醒")
                )),
                "前 3 次不应提醒"
            );
        }
        let reminder = captured[4]
            .iter()
            .find_map(|item| match item {
                HistoryItem::Message { role: Role::System, text, .. }
                    if text.contains("空转提醒") =>
                {
                    Some(text.clone())
                }
                _ => None,
            })
            .expect("第 4 次相同调用后必须注入空转提醒");
        assert!(reminder.contains("read_file"), "{reminder}");
    }

    fn make_loop_with_destructive(
        turns: Vec<Vec<ModelStreamEvent>>,
    ) -> (
        AgentLoop,
        Arc<Mutex<Vec<EventMsg>>>,
        CancellationToken,
        Arc<ApprovalBroker>,
    ) {
        struct WipeTool {
            spec: ToolSpec,
        }
        impl Default for WipeTool {
            fn default() -> Self {
                Self {
                    spec: ToolSpec::new("wipe_disk", "破坏性", serde_json::json!({"type":"object"})),
                }
            }
        }
        #[async_trait::async_trait]
        impl ToolExecutor for WipeTool {
            fn spec(&self) -> &ToolSpec {
                &self.spec
            }
            async fn execute(&self, _a: Value, _c: &ToolContext) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput::ok("wiped"))
            }
        }
        let (mut agent, store, cancel, broker) = make_loop(turns);
        let mut router_raw = ToolRouter::new();
        router_raw.register(Arc::new(WipeTool::default()));
        // 直接补注册（保持既有 router 引用结构）
        let router = Arc::new({
            let mut r = ToolRouter::new();
            r.register(Arc::new(EchoTool::default()));
            r.register(Arc::new(WipeTool::default()));
            r
        });
        agent.router = router;
        drop(router_raw);
        (agent, store, cancel, broker)
    }

    // ---- 系统提示下发（docs/04 §6.1）----

    /// 记录 (系统提示, 请求历史快照) 的后端：验证系统提示不写入 history、
    /// 且历史里只多一条任务开始时的环境快照。
    type Recorded = Arc<Mutex<Vec<(String, Vec<HistoryItem>)>>>;

    struct PromptRecordingBackend {
        recorded: Recorded,
        /// 每轮脚本；排空后退化为"只回文本"的收尾轮。
        script: Mutex<std::collections::VecDeque<Vec<ModelStreamEvent>>>,
    }

    #[async_trait::async_trait]
    impl ModelBackend for PromptRecordingBackend {
        async fn stream(
            &self,
            history: &[HistoryItem],
            _tools: &[ToolDeclaration],
            instructions: &str,
        ) -> Result<ModelStream, crate::model_client::ModelError> {
            self.recorded.lock().unwrap().push((instructions.to_string(), history.to_vec()));
            let next = self.script.lock().unwrap().pop_front().unwrap_or_else(|| {
                // 无工具调用 → 单轮即完成
                vec![ModelStreamEvent::OutputTextDelta { delta: "ok".into() }]
            });
            Ok(Box::pin(futures::stream::iter(next)))
        }
    }

    fn agent_with_recording(
        workspace: PathBuf,
        plan_active: bool,
    ) -> (AgentLoop, Recorded, Arc<Mutex<Vec<EventMsg>>>) {
        agent_with_recording_script(workspace, plan_active, Vec::new())
    }

    fn agent_with_recording_script(
        workspace: PathBuf,
        plan_active: bool,
        script: Vec<Vec<ModelStreamEvent>>,
    ) -> (AgentLoop, Recorded, Arc<Mutex<Vec<EventMsg>>>) {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let backend = Arc::new(PromptRecordingBackend {
            recorded: recorded.clone(),
            script: Mutex::new(script.into()),
        });
        let (_sink, store) = CapturingSink::new();
        let plan_state = Arc::new(PlanState::new());
        if plan_active {
            plan_state.activate();
        }
        let mut router = ToolRouter::new();
        router.register(Arc::new(EchoTool::default()));
        let agent = AgentLoop {
            backend,
            router: Arc::new(router),
            broker: Arc::new(ApprovalBroker::new()),
            plan_state,
            workspace_root: workspace,
            max_turns: 2,
            token_budget: 0,
            tree_budget: TreeBudget::unlimited(),
            policy: Arc::new(policy::SessionPolicyHandle::new(policy::SessionPolicy {
                approval: crate::config::ApprovalMode::OnRequest,
                sandbox: crate::config::SandboxMode::WorkspaceWrite,
            })),
            rollout: None,
            rollout_broke: std::sync::atomic::AtomicBool::new(false),
            session_id: None,
            jobs: None,
            stop_hook: None,
            verify: Default::default(),
            retry: crate::model_client::RetryPolicy::disabled(),
        };
        (agent, recorded, store)
    }

    #[tokio::test]
    async fn system_instruction_sent_to_model_and_kept_out_of_history() {
        let ws = std::env::temp_dir();
        let (agent, recorded, store) = agent_with_recording(ws.clone(), false);
        agent
            .run_task("任务", Arc::new(CapturingSink(store)), CancellationToken::new())
            .await;

        let calls = recorded.lock().unwrap();
        assert_eq!(calls.len(), 1, "单轮任务应只采样一次");
        let (prompt, history) = &calls[0];
        assert!(prompt.contains(&ws.display().to_string()), "提示须含工作区路径");
        assert!(prompt.contains("工作区写入"), "提示须反映沙箱模式");
        assert!(!prompt.contains("当前状态：计划模式"));
        // 系统提示本体进 instructions，不写 history；history 只多一条环境快照（时间/Git）。
        assert_eq!(history.len(), 2, "历史 = 环境快照 + 用户任务");
        match &history[0] {
            HistoryItem::Message { role: Role::System, text, .. } => {
                assert!(text.contains("## 环境快照"), "尾部消息只允许任务开始时的环境快照：{text}");
                assert!(!text.contains("## 沙箱边界"), "系统提示本体不得混入 history：{text}");
            }
            other => panic!("历史首条必须是环境快照 system 消息，实际 {other:?}"),
        }
        assert!(
            matches!(&history[1], HistoryItem::Message { role: Role::User, .. }),
            "环境快照必须插在用户任务之前"
        );
    }

    /// 前缀稳定化的端到端断言：同一任务多轮采样，system 必须逐字节一致。
    #[tokio::test]
    async fn system_prompt_is_identical_across_turns() {
        // 第一轮发起一次工具调用，逼出第二轮采样。
        let script = vec![vec![
            ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                name: "read_file".into(),
                arguments: "{}".into(),
                call_id: "c1".into(),
            }),
            usage_ev(),
        ]];
        let (agent, recorded, store) =
            agent_with_recording_script(std::env::temp_dir(), false, script);
        agent
            .run_task("任务", Arc::new(CapturingSink(store)), CancellationToken::new())
            .await;

        let calls = recorded.lock().unwrap();
        assert!(calls.len() >= 2, "需要多轮才能验证前缀稳定，实际 {} 轮", calls.len());
        let first = &calls[0].0;
        for (index, (prompt, _)) in calls.iter().enumerate() {
            assert_eq!(prompt, first, "第 {} 轮 system 与首轮不一致，前缀缓存会全灭", index + 1);
        }
    }

    #[tokio::test]
    async fn system_instruction_reflects_plan_mode() {
        let (agent, recorded, store) = agent_with_recording(std::env::temp_dir(), true);
        agent
            .run_task("先做计划", Arc::new(CapturingSink(store)), CancellationToken::new())
            .await;
        let calls = recorded.lock().unwrap();
        let (prompt, _) = &calls[0];
        assert!(prompt.contains("当前状态：计划模式"), "计划态必须写入提示");
        assert!(prompt.contains("exit_plan_mode"), "计划态须指引退出路径");
    }

    // ---- Plan Mode（ADR-003 只读计划态）----

    #[test]
    fn plan_mode_declarations_hide_write_and_execute_tools() {
        let mut router = ToolRouter::new();
        crate::tools::register_builtin(&mut router, Arc::new(PlanState::new()), true);

        let names = |plan: bool| -> Vec<String> {
            declarations_for(&router, plan, true, 0, false, None).into_iter().map(|d| d.name).collect()
        };
        let planned = names(true);
        assert!(planned.contains(&"read_file".to_string()));
        assert!(planned.contains(&"list_dir".to_string()));
        assert!(planned.contains(&"search".to_string()));
        assert!(planned.contains(&"exit_plan_mode".to_string()));
        assert!(!planned.contains(&"apply_patch".to_string()), "计划态不得向模型声明写入工具");
        assert!(!planned.contains(&"unified_exec".to_string()), "计划态不得向模型声明命令执行工具");
        assert!(!planned.contains(&"enter_plan_mode".to_string()), "已在计划态，无需重复进入");

        let full = names(false);
        assert!(full.contains(&"apply_patch".to_string()));
        assert!(full.contains(&"unified_exec".to_string()));
    }

    #[tokio::test]
    async fn plan_mode_guard_blocks_write_tool_even_if_called() {
        struct PatchEcho {
            spec: ToolSpec,
        }
        #[async_trait::async_trait]
        impl ToolExecutor for PatchEcho {
            fn spec(&self) -> &ToolSpec {
                &self.spec
            }
            async fn execute(&self, _a: Value, _c: &ToolContext) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput::ok("patched"))
            }
        }
        let (mut agent, store, cancel, _broker) = make_loop(vec![
            vec![ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                name: "apply_patch".into(),
                arguments: r#"{"patch":"*** Begin Patch"}"#.into(),
                call_id: "c5".into(),
            }), usage_ev()],
            vec![ModelStreamEvent::OutputTextDelta { delta: "改为只读调研".into() }, usage_ev()],
        ]);
        // 只注册写入工具（read_file 回显 + apply_patch），并置为计划态
        let mut router = ToolRouter::new();
        router.register(Arc::new(EchoTool::default()));
        router.register(Arc::new(PatchEcho {
            spec: ToolSpec::new("apply_patch", "写入", serde_json::json!({"type":"object"})),
        }));
        agent.router = Arc::new(router);
        let plan_state = Arc::new(PlanState::new());
        plan_state.activate();
        agent.plan_state = plan_state;

        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.reason, TurnEndReason::Completed);
        let events = store.lock().unwrap();
        assert!(
            events.iter().any(|e| matches!(
                e,
                EventMsg::ToolCallEnd { call_id, error: Some(msg), .. }
                    if call_id == "c5" && msg.contains("计划模式")
            )),
            "计划态下写入工具必须被执行守卫拒绝"
        );
        // 未被真正执行：没有「成功」的 ToolCallEnd
        assert!(!events.iter().any(|e| matches!(
            e,
            EventMsg::ToolCallEnd { call_id, exit_code: Some(0), error: None, .. } if call_id == "c5"
        )));
        // 也不应触发审批（守卫先于审批门禁）
        assert!(!events.iter().any(|e| matches!(e, EventMsg::ExecApprovalRequest { .. })));
    }

    /// SH-090 批 2：会话 mention 只在本步请求副本里展开——首条 user 消息剥离 mention，
    /// 紧跟一条 `<referenced-sessions>` 上下文；持久化历史（`run_with_history` 的入参）不动。
    #[tokio::test]
    async fn session_reference_mentions_expand_into_the_request_only() {
        use crate::rollout::{ConfigSnapshot, RolloutItem, RolloutWriter};

        struct RecordingBackend {
            seen: Arc<Mutex<Vec<Vec<HistoryItem>>>>,
            script: Mutex<VecDeque<Vec<ModelStreamEvent>>>,
        }

        #[async_trait::async_trait]
        impl ModelBackend for RecordingBackend {
            async fn stream(
                &self,
                history: &[HistoryItem],
                _tools: &[ToolDeclaration],
                _instructions: &str,
            ) -> Result<ModelStream, crate::model_client::ModelError> {
                self.seen.lock().unwrap().push(history.to_vec());
                let mut queue = self.script.lock().unwrap();
                match queue.pop_front() {
                    Some(script) => Ok(Box::pin(futures::stream::iter(script))),
                    None => Ok(Box::pin(futures::stream::pending())),
                }
            }
        }

        fn meta(id: &str) -> RolloutItem {
            RolloutItem::SessionMeta {
                session_id: id.into(),
                agent_id: id.into(),
                parent_id: None,
                created_at: 1,
                config_snapshot: ConfigSnapshot {
                    provider: "p".into(),
                    base_url: String::new(),
                    model: "m".into(),
                    sandbox: "WorkspaceWrite".into(),
                    approval: "Ask".into(),
                    workspace_root: "D:/ws".into(),
                },
            }
        }

        let dir = crate::test_support::TestTempDir::new("sref-loop");
        let writer = Arc::new(RolloutWriter::create(&dir, &meta("self")).unwrap());
        let referenced = RolloutWriter::create(&dir, &meta("ref-1")).unwrap();
        referenced
            .append(&RolloutItem::ResponseItem {
                item: HistoryItem::Message { role: Role::User, text: "被引用的问题".into(), reasoning: None },
                at_ms: 0,
            })
            .unwrap();
        referenced
            .append(&RolloutItem::ResponseItem {
                item: HistoryItem::Message { role: Role::Assistant, text: "被引用的回答".into(), reasoning: None },
                at_ms: 0,
            })
            .unwrap();

        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut router = ToolRouter::new();
        router.register(Arc::new(EchoTool::default()));
        let broker = Arc::new(ApprovalBroker::new());
        let agent = AgentLoop {
            backend: Arc::new(RecordingBackend {
                seen: seen.clone(),
                script: Mutex::new(VecDeque::from([vec![
                    ModelStreamEvent::OutputTextDelta { delta: "收到".into() },
                    usage_ev(),
                ]])),
            }),
            router: Arc::new(router),
            broker: broker.clone(),
            plan_state: Arc::new(PlanState::new()),
            workspace_root: std::env::temp_dir(),
            max_turns: 1,
            token_budget: 0,
            tree_budget: TreeBudget::unlimited(),
            policy: Arc::new(policy::SessionPolicyHandle::new(policy::SessionPolicy {
                approval: crate::config::ApprovalMode::OnRequest,
                sandbox: crate::config::SandboxMode::WorkspaceWrite,
            })),
            rollout: Some(writer),
            rollout_broke: std::sync::atomic::AtomicBool::new(false),
            session_id: None,
            jobs: None,
            stop_hook: None,
            verify: Default::default(),
            retry: crate::model_client::RetryPolicy::disabled(),
        };
        let (sink, _store) = CapturingSink::new();
        let task = format!(
            "请参考 @[旧会话]({}) 回答",
            crate::session_reference::encode_uri("ref-1")
        );
        agent
            .run_with_history(
                vec![HistoryItem::Message { role: Role::User, text: task, reasoning: None }],
                0,
                Arc::new(sink),
                CancellationToken::new(),
            )
            .await;
        let captured = seen.lock().unwrap();
        let request = captured.first().expect("后端应收到请求");
        // 任务开始时的环境快照（DSH in-history）+ 原消息 + 引用上下文
        assert_eq!(request.len(), 3, "环境快照 + 原消息 + 引用上下文");
        match &request[0] {
            HistoryItem::Message { role: Role::System, text, .. } => {
                assert!(text.starts_with("## 环境快照"), "首条是任务开始时的环境快照");
            }
            other => panic!("首条应是环境快照 system 消息，实际 {other:?}"),
        }
        match &request[1] {
            HistoryItem::Message { text, .. } => assert_eq!(text, "请参考 @旧会话 回答"),
            _ => panic!("第二条应是剥离 mention 的用户消息"),
        }
        match &request[2] {
            HistoryItem::Message { role: Role::User, text, .. } => {
                assert!(text.starts_with("## Referenced sessions"));
                assert!(text.contains("被引用的回答"));
            }
            _ => panic!("第三条应是引用上下文"),
        }
    }

    // ---- SH-050：AgentInstance（root 与子代理共用同一执行引擎）----

    fn assistant_text(items: &[HistoryItem]) -> String {
        items
            .iter()
            .filter_map(|item| match item {
                HistoryItem::Message { role: Role::Assistant, text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    fn user_text(items: &[HistoryItem]) -> String {
        items
            .iter()
            .filter_map(|item| match item {
                HistoryItem::Message { role: Role::User, text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// root 与子代理跑在同一个 `AgentLoop` 上，但历史/身份/深度互不串。
    #[tokio::test]
    async fn root_and_child_instances_share_the_engine_but_not_state() {
        let (agent, store, _cancel, _broker) = make_loop(vec![
            vec![ModelStreamEvent::OutputTextDelta { delta: "answer-a".into() }, usage_ev()],
            vec![ModelStreamEvent::OutputTextDelta { delta: "answer-b".into() }, usage_ev()],
        ]);
        let agent = Arc::new(agent);
        let mut root = AgentInstance::root(
            vec![HistoryItem::Message { role: Role::User, text: "root-task".into(), reasoning: None }],
            0,
            CancellationToken::new(),
        );
        let mut child = AgentInstance::child("agent-1", "root", "child-task", CancellationToken::new());

        let sink: Arc<dyn crate::events::EventSink> = Arc::new(CapturingSink(store.clone()));
        let (root_summary, child_summary) = tokio::join!(
            agent.run_instance(&mut root, sink.clone()),
            agent.run_instance(&mut child, sink),
        );

        assert_eq!(root_summary.reason, TurnEndReason::Completed);
        assert_eq!(child_summary.reason, TurnEndReason::Completed);
        // 同一引擎各跑一轮：两次 turn_started，而不是在共享历史里跑了两轮。
        let started = store
            .lock()
            .unwrap()
            .iter()
            .filter(|event| matches!(event, EventMsg::TurnStarted { .. }))
            .count();
        assert_eq!(started, 2);

        // 两条脚本响应的分配顺序取决于并发调度，断言的是"各自只拿到一条、且不相交"。
        let root_answer = assistant_text(&root.history);
        let child_answer = assistant_text(&child.history);
        assert_ne!(root_answer, child_answer);
        assert_eq!(
            [root_answer.as_str(), child_answer.as_str()].iter().copied().collect::<std::collections::BTreeSet<_>>(),
            ["answer-a", "answer-b"].into_iter().collect::<std::collections::BTreeSet<_>>(),
        );
        // 子代理不继承父历史（ADR-102）：它只有 spawn 的 task 描述。
        assert_eq!(user_text(&child.history), "child-task");
        assert_eq!(user_text(&root.history), "root-task");

        // 身份与深度：只有 root 能继续分叉（ADR-101 硬限）。
        assert_eq!((root.agent_id.as_str(), root.depth), ("root", 0));
        assert_eq!(
            (child.agent_id.as_str(), child.parent_id.as_deref(), child.depth),
            ("agent-1", Some("root"), 1),
        );
        assert!(root.can_spawn());
        assert!(!child.can_spawn());
    }

    /// 取消子是实例级的：取消子代理不会打断同一引擎上的 root。
    #[tokio::test]
    async fn cancelling_a_child_leaves_the_root_running() {
        let (agent, store, _cancel, _broker) = make_loop(vec![
            // 子代理：空脚本 = 永不产出的事件流；root：正常完成。
            vec![],
            vec![ModelStreamEvent::OutputTextDelta { delta: "root-ok".into() }, usage_ev()],
        ]);
        let agent = Arc::new(agent);
        let child_cancel = CancellationToken::new();
        let handle = tokio::spawn({
            let agent = agent.clone();
            let mut child = AgentInstance::child("agent-1", "root", "阻塞任务", child_cancel.clone());
            let sink: Arc<dyn crate::events::EventSink> = Arc::new(CapturingSink(store.clone()));
            async move {
                let summary = agent.run_instance(&mut child, sink).await;
                (summary, child)
            }
        });
        tokio::task::yield_now().await;
        child_cancel.cancel();
        let (child_summary, child) = handle.await.expect("子代理任务不应 panic");
        assert_eq!(child_summary.reason, TurnEndReason::Cancelled);
        // 取消的轮次不追加 assistant 消息；历史停在「环境快照 + task」两条。
        assert_eq!(child.history.len(), 2, "环境快照 + task，取消不追加 assistant");

        let mut root = AgentInstance::root(
            vec![HistoryItem::Message { role: Role::User, text: "root-task".into(), reasoning: None }],
            0,
            CancellationToken::new(),
        );
        let root_summary = agent.run_instance(&mut root, Arc::new(CapturingSink(store))).await;
        assert_eq!(root_summary.reason, TurnEndReason::Completed);
        assert_eq!(root.last_assistant_text(), Some("root-ok"));
    }
}
