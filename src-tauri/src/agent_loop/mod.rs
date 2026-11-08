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
pub mod instance;
pub mod prompt;
pub mod repair;
pub mod rules;

use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

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
/// `instructions` 每轮由 `prompt::system_instruction` 生成（docs/04 §6.1），
/// 不写入 history，避免逐轮重复累积。
#[async_trait::async_trait]
pub trait ModelBackend: Send + Sync {
    async fn stream(
        &self,
        history: &[HistoryItem],
        tools: &[ToolDeclarationShim],
        instructions: &str,
    ) -> Result<ModelStream, String>;
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
    ) -> Result<ModelStream, String> {
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
    /// 失败不中断本轮执行——工具与模型都不依赖档案；但必须**播报**：
    /// 静默降级会让用户以为"会话有存档、可以 resume"，而实际没有。
    fn flush_history(
        &self,
        rollout: &Option<Arc<crate::rollout::RolloutWriter>>,
        sink: &Arc<dyn crate::events::EventSink>,
        history: &[HistoryItem],
        recorded: &mut usize,
    ) {
        let Some(writer) = rollout else { *recorded = history.len(); return };
        for item in &history[*recorded..] {
            let entry = crate::rollout::RolloutItem::ResponseItem { item: item.clone() };
            if let Err(e) = writer.append(&entry) {
                if !self.rollout_broke.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    let _ = sink.emit(&EventMsg::Error {
                        message: format!("会话档案写入失败（后续不再重复播报）：{e}"),
                    });
                }
                // 失败后不再重试剩余条目：同一块盘继续试只会刷更多错误
                break;
            }
        }
        *recorded = history.len();
    }

    /// 就地压缩一段历史：发一次不带工具的总结请求 → 写 `Compacted` 锚点 → 用摘要替换区间。
    ///
    /// 三件事的顺序是硬性的：
    /// 1. **先落锚点、再改内存**。锚点的语义是「重放出的 history 里 `[start,end)` 区间被
    ///    summary 取代」，所以它必须写在那段条目**之后**——`recorded >= target.range_end`
    ///    由「每轮末尾都 flush」保证，锚点紧随其后，回放时正好接在那些条目之后生效。
    ///    顺序反了就会出现锚点指向尚未写入的条目，resume 直接报区间越界。
    /// 2. **锚点写失败不阻断内存压缩**。丢一次存档可以补，预算超限继续原样发请求
    ///    就是等着上下文溢出、整轮失败——后者严重得多。落盘失败本身已由
    ///    `flush_history` 负责播报，这里不再重复刷一条。
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
    ) -> Result<(), String> {
        let request = crate::compaction::build_request(history, &target);
        // 总结请求不带工具声明：它不需要调工具，带上只会让端点多算一份声明开销，
        // 并给模型一个"还可以继续查"的错误暗示。
        let mut stream = self
            .backend
            .stream(&request, &[], instructions)
            .await
            .map_err(|e| format!("压缩请求发起失败，本轮中止：{e}"))?;
        let mut summary = String::new();
        loop {
            let mut stream_pin = stream.as_mut();
            let next_item = stream_pin.next();
            tokio::select! {
                _ = cancel.cancelled() => return Err("压缩被用户取消".to_string()),
                item = next_item => match item {
                    None => break,
                    Some(ModelStreamEvent::OutputTextDelta { delta }) => summary.push_str(&delta),
                    // 压缩请求不该产出工具调用；真产出了也只能忽略——它不是本轮的答案
                    Some(_) => {}
                }
            }
        }
        let summary = crate::compaction::extract_summary(&summary)?;

        if let Some(writer) = rollout {
            // 写失败在这里静默是刻意的：见上面第 2 点。
            let _ = writer.append(&crate::rollout::RolloutItem::Compacted {
                range_start: target.range_start,
                range_end: target.range_end,
                summary: summary.clone(),
            });
        }

        history.splice(
            target.range_start..target.range_end,
            // 摘要必须是 **user** 而不是 assistant。两个理由各自独立、都成立：
            // 1. thinking 档端点要求重放的每条 assistant 消息都带回 `reasoning_content`
            //    （SH-083），而摘要没有 reasoning——以 assistant 身份进历史就直接 400。
            //    这不是理论风险：1500 预算的真机基线里 `search-and-report` 就是这么挂的。
            // 2. 摘要不是"模型说过的话"，而是交给下一个模型的交接指令——语义上就是 user 侧输入。
            //    codex 同此：`CompactionSummary::role()` 返回 "user"。
            // 注意这并不违反 04 §6.1 不变量③：那条禁止的是把摘要塞进 **system**。
            [HistoryItem::Message { role: Role::User, text: summary, reasoning: None }],
        );
        *recorded = history.len();
        tracing::info!(
            compressed_items = target.range_end - target.range_start,
            history_len = history.len(),
            "上下文已压缩（SH-032）"
        );
        Ok(())
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
            token_budget: cfg.agent.token_budget,
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
            crate::tools::view_image::ImageBudget::default(),
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
        let mut last_turn = 0u32;
        // 最近一次端点报回的上下文规模，作为压缩判据的观测侧（SH-032）。
        // 字节估算看不见推理 token 与工具声明开销，只有端点的 input_tokens 是真实值。
        let mut observed_input_tokens = 0usize;
        // 结束前否决点是否已经用过（SH-121②：每次任务最多一次）。
        let mut stop_vetoed = false;
        let emit = |sink: &Arc<dyn crate::events::EventSink>, msg: EventMsg| {
            let _ = sink.emit(&msg);
        };

        // SH-091：技能目录在每次任务开始时发现一次。目录为空时系统提示不加技能段，
        // 且不向模型声明 `skill` 工具——避免无技能工作区的 token 开销与假能力。
        let skill_summaries = crate::skills::discover(&self.workspace_root);
        let skill_catalog = crate::skills::catalog_lines(&skill_summaries, 500);
        let has_model_skills = skill_summaries.iter().any(|skill| skill.model_invocable);

        'outer: for turn in 1..=self.max_turns {
            last_turn = turn;
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
            // 系统提示每轮重算（docs/04 §6.1 不变量③）：计划态与沙箱模式变化必须反映到模型
            let instructions = prompt::system_instruction(&prompt::PromptContext {
                workspace_root: &self.workspace_root,
                sandbox: turn_policy.sandbox,
                os: crate::sandbox::current_os_mode(),
                plan_mode: self.plan_state.is_active(),
                skill_catalog: &skill_catalog,
                network_granted: crate::sandbox::network::global()
                    .is_granted(session_id.as_deref()),
                env: prompt::EnvironmentFacts::detect(&self.workspace_root),
            });
            // 计划态下只向模型声明只读工具 + exit_plan_mode（ADR-003 双层防御第一层）
            let declarations = declarations_for(
                &self.router,
                self.plan_state.is_active(),
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
                if let Err(message) = self
                    .compact_in_place(rollout, history, recorded, target, &instructions, cancel)
                    .await
                {
                    emit(&sink, EventMsg::Error { message });
                    reason = TurnEndReasonLocal::Failed;
                    break 'outer;
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
            let mut stream = match self.backend.stream(&request_history, &declarations, &instructions).await {
                Ok(s) => s,
                Err(e) => {
                    emit(&sink, EventMsg::Error { message: e });
                    reason = TurnEndReasonLocal::Failed;
                    break 'outer;
                }
            };

            // 2) 消费流：转发增量、收集输出项
            let mut assistant_text = String::new();
            let mut reasoning_text = String::new();
            let mut full_message: Option<String> = None;
            let mut calls: Vec<(String, String, String)> = Vec::new(); // (name, arguments, call_id)
            let mut usage: Option<Usage> = None;
            loop {
                let mut stream_pin = stream.as_mut(); // Pin<&mut dyn S>，Next 借用其存活
                let next_item = stream_pin.next();
                tokio::select! {
                    _ = cancel.cancelled() => {
                        reason = TurnEndReasonLocal::Cancelled;
                        break 'outer;
                    }
                    item = next_item => match item {
                        None => break,
                        Some(ModelStreamEvent::OutputTextDelta { delta }) => {
                            assistant_text.push_str(&delta);
                            emit(&sink, EventMsg::AgentMessageDelta { delta });
                        }
                        // 思考增量单独成事件：它不是答案内容，前端要能折叠显示，
                        // FF-5 也要把它算进"首个可见反馈"（业界同一口径，见 latency_budget）
                        Some(ModelStreamEvent::ReasoningDelta { delta }) => {
                            reasoning_text.push_str(&delta);
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
                        }
                        Some(ModelStreamEvent::Failed { message }) => {
                            emit(&sink, EventMsg::Error { message });
                            reason = TurnEndReasonLocal::Failed;
                            break 'outer;
                        }
                    }
                }
            }
            if let Some(u) = usage {
                // 实例级累计（ExitSummary 的 token_usage）。
                totals.absorb(&u);
                // 树级共账（SH-055）：父与全部后代记同一本账；cached 是 input 的子集，不重复计。
                budget.record_tokens(u64::from(u.input_tokens) + u64::from(u.output_tokens));
                observed_input_tokens = usize::try_from(u.input_tokens).unwrap_or(usize::MAX);
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
            // 结束前否决点（SH-121②）：先算再入历史——`text` 之后会被 move。
            // 只在「确实要结束」且预算还够再跑一轮时询问；每次任务最多否决一次。
            let veto = if calls.is_empty() && !stop_vetoed && turn < self.max_turns {
                self.stop_hook.as_ref().and_then(|hook| {
                    hook.before_stop(StopContext {
                        history,
                        last_text: &text,
                        turn,
                        plan_active: self.plan_state.is_active(),
                    })
                })
            } else {
                None
            };
            if !text.trim().is_empty() || reasoning.is_some() {
                history.push(HistoryItem::Message { role: Role::Assistant, text, reasoning });
            }

            // 4) 无工具调用 → 结束前否决点放行/否决
            if calls.is_empty() {
                match veto {
                    Some(reminder) => {
                        stop_vetoed = true;
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

            // 5) 本轮已到预算上限且仍需工具 → 预算耗尽
            if turn == self.max_turns {
                emit(&sink, EventMsg::Error { message: format!("已达最大轮数上限（{turn}）") });
                reason = TurnEndReasonLocal::BudgetExhausted;
                break;
            }

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
            for (name, arguments, call_id) in &calls {
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
                if self.plan_state.is_active() && !policy::plan_mode_allows(name) {
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
                .with_scope(scope.clone());
                // 只有 root（depth=0）拿到编排能力；子代理沿用 scope 但无 orchestrator，
                // 分发层会拒绝它调用编排工具（第二层深度门控）。
                let tool_ctx = match &orchestrator {
                    Some(orchestrator) => tool_ctx.with_orchestration(scope.clone(), orchestrator.clone()),
                    None => tool_ctx,
                };
                let mut result = self.router.execute(name, args_val.clone(), &tool_ctx).await;
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
                let output_text = match &result {
                    Ok(out) => {
                        emit(&sink, EventMsg::ToolCallEnd {
                            call_id: call_id.clone(),
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
                        emit(&sink, EventMsg::ToolCallEnd {
                            call_id: call_id.clone(),
                            exit_code: None,
                            error: Some("已取消".into()),
                            result: Some(ToolCallResult::Cancelled),
                        });
                        reason = TurnEndReasonLocal::Cancelled;
                        break 'outer;
                    }
                    Err(e) => {
                        emit(&sink, EventMsg::ToolCallEnd {
                            call_id: call_id.clone(),
                            exit_code: None,
                            error: Some(e.to_string()),
                            result: Some(ToolCallResult::Error),
                        });
                        e.to_string()
                    }
                };
                history.push(HistoryItem::FunctionCallOutput {
                    call_id: call_id.clone(),
                    output: output_text.clone(),
                });
                // SH-072：view_image 的图片以引用形式回填成一条用户消息——Chat 协议不允许
                // 图片挂在 tool 消息里，模型只能从用户消息里看到它。字节取自进程内缓存，
                // 历史里只有 ImageRef（rollout 因此不会变大）。
                if let Ok(out) = &result {
                    if out.success {
                        if let Some(image) = crate::tools::view_image::attachment(&out.output) {
                            history.push(HistoryItem::ImageMessage {
                                role: crate::model_client::Role::User,
                                text: format!("[view_image] {}", image.path),
                                images: vec![image],
                            });
                        }
                    }
                }
                // SH-126：把本次调用的写入搬进本地缓冲。写日志本身是 take 语义，
                // 不搬走就会被下一次写入覆盖掉一半判定依据。
                pending_writes.extend(write_journal.take());
                // SH-125：命令失败 → 提取「文件:行」→ 下一轮以独立 System 条目提示
                // 模型读错、修复、重跑；达到上限或同一错误重复则明示放弃（不伪造成功）。
                let succeeded = match &result {
                    Ok(out) => out.success,
                    Err(_) => false,
                };
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
            // 轮末落盘：崩溃只丢当前轮，之前各轮的档案已经 flush 到位
            self.flush_history(rollout, &sink, history, recorded);
        }

        // 覆盖所有 break 路径（取消/失败/预算耗尽）——它们都从这里收口
        self.flush_history(rollout, &sink, history, recorded);

        emit(&sink, EventMsg::TurnCompleted {
            turn: last_turn,
            reason: match reason {
                TurnEndReasonLocal::Completed => crate::events::TurnEndReason::Completed,
                TurnEndReasonLocal::Cancelled => crate::events::TurnEndReason::Cancelled,
                TurnEndReasonLocal::Failed => crate::events::TurnEndReason::Failed,
                TurnEndReasonLocal::BudgetExhausted => crate::events::TurnEndReason::BudgetExhausted,
            },
        });
        TurnSummary { turns: last_turn, reason: reason.into() }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnSummary {
    pub turns: u32,
    pub reason: crate::events::TurnEndReason,
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
        ) -> Result<ModelStream, String> {
            let mut q = self.0.lock().unwrap();
            match q.pop_front() {
                Some(script) if !script.is_empty() => Ok(Box::pin(futures::stream::iter(script))),
                // 空脚本 = 永不产出的事件流（取消/超时类确定性测试用）
                _ => Ok(Box::pin(futures::stream::pending())),
            }
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
        let mut router = ToolRouter::new();
        router.register(Arc::new(EchoTool::default()));
        let (_sink, store) = CapturingSink::new();
        let broker = Arc::new(ApprovalBroker::new());
        let agent = AgentLoop {
            backend: Arc::new(ScriptedBackend::new(turns)),
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
            EventMsg::AgentMessageDelta { .. } => "delta",
            EventMsg::TokenUsage { .. } => "usage",
            EventMsg::TurnCompleted { .. } => "turn_completed",
            other => panic!("意外事件: {other:?}"),
        }).collect();
        assert_eq!(tags, ["turn_started", "delta", "usage", "turn_completed"]);
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

    #[tokio::test]
    async fn model_failure_maps_to_failed() {
        let (agent, store, cancel, _broker) = make_loop(vec![vec![ModelStreamEvent::Failed { message: "boom".into() }]]);
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.reason, TurnEndReason::Failed);
        assert!(store.lock().unwrap().iter().any(|e| matches!(
            e,
            EventMsg::Error { message } if message == "boom"
        )));
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
            ) -> Result<ModelStream, String> {
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
            ) -> Result<ModelStream, String> {
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

    /// 记录 (系统提示, 历史长度) 的后端：验证每轮下发、且不写入 history。
    type Recorded = Arc<Mutex<Vec<(String, usize)>>>;

    struct PromptRecordingBackend(Recorded);

    #[async_trait::async_trait]
    impl ModelBackend for PromptRecordingBackend {
        async fn stream(
            &self,
            history: &[HistoryItem],
            _tools: &[ToolDeclaration],
            instructions: &str,
        ) -> Result<ModelStream, String> {
            self.0.lock().unwrap().push((instructions.to_string(), history.len()));
            // 无工具调用 → 单轮即完成
            Ok(Box::pin(futures::stream::iter(vec![ModelStreamEvent::OutputTextDelta {
                delta: "ok".into(),
            }])))
        }
    }

    fn agent_with_recording(
        workspace: PathBuf,
        plan_active: bool,
    ) -> (AgentLoop, Recorded, Arc<Mutex<Vec<EventMsg>>>) {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let backend = Arc::new(PromptRecordingBackend(recorded.clone()));
        let (_sink, store) = CapturingSink::new();
        let plan_state = Arc::new(PlanState::new());
        if plan_active {
            plan_state.activate();
        }
        let agent = AgentLoop {
            backend,
            router: Arc::new(ToolRouter::new()),
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
        let (prompt, history_len) = &calls[0];
        assert!(prompt.contains(&ws.display().to_string()), "提示须含工作区路径");
        assert!(prompt.contains("工作区写入"), "提示须反映沙箱模式");
        assert!(!prompt.contains("当前状态：计划模式"));
        // 提示不写入 history：历史里只有用户这一条
        assert_eq!(*history_len, 1, "系统提示不得混入 history");
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
        crate::tools::register_builtin(&mut router, Arc::new(PlanState::new()));

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
            ) -> Result<ModelStream, String> {
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
            })
            .unwrap();
        referenced
            .append(&RolloutItem::ResponseItem {
                item: HistoryItem::Message { role: Role::Assistant, text: "被引用的回答".into(), reasoning: None },
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
        assert_eq!(request.len(), 2, "原消息 + 引用上下文");
        match &request[0] {
            HistoryItem::Message { text, .. } => assert_eq!(text, "请参考 @旧会话 回答"),
            _ => panic!("首条应是剥离 mention 的用户消息"),
        }
        match &request[1] {
            HistoryItem::Message { role: Role::User, text, .. } => {
                assert!(text.starts_with("## Referenced sessions"));
                assert!(text.contains("被引用的回答"));
            }
            _ => panic!("第二条应是引用上下文"),
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
        // 取消的轮次不追加 assistant 消息，实例历史停在 task 那一条。
        assert_eq!(child.history.len(), 1);

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
