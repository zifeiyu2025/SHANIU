//! 子代理注册表（SH-051，docs/02 §2.1/§2.3，ADR-101/102）。
//!
//! 职责边界：
//! - **身份与父子边**：给每个子代理分配全局唯一 id，记录 parent/depth；
//! - **状态机**：`spawning → waiting → running → exited`（见 [`AgentStatus`]）；
//! - **取消级联**：子令牌由父令牌 `child_token()` 派生，父取消即级联，不需要逐层通知；
//! - **并发上限**：超出上限的子代理排队（`waiting`），不发起模型请求；
//! - **结果收口**：执行结束把 [`ExitSummary`] 存进注册表，供 `wait_agent` 取用（SH-052）。
//! - **编排入口**（SH-052）：[`orchestrator::RegistryOrchestrator`] 实现
//!   `tool_router::orchestration::AgentOrchestrator`，由组合根注入 root 的工具上下文。
//!
//! 不负责：模型调用与工具执行（复用 `AgentLoop::run_instance`）、事件协议包装
//! （SH-053 的 `AgentEvent{agent_id}`）、档案引用（SH-056）、预算共账（SH-055）。

pub mod orchestrator;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::agent_loop::{AgentInstance, AgentLoop, TokenTotals, MAX_AGENT_DEPTH};
use crate::events::{EventMsg, EventSink, TurnEndReason};
use crate::model_client::{HistoryItem, Role};
use crate::rollout::RolloutWriter;
use crate::tool_router::orchestration::AgentScope;

/// 并发上限默认值（docs/02 §2.3：默认 4）。
pub const DEFAULT_MAX_CONCURRENT: usize = 4;
/// 并发上限硬顶（docs/02 §2.3：硬顶 8）。配置值再大也按此截断。
pub const HARD_MAX_CONCURRENT: usize = 8;
/// ExitSummary 的序列化上限（docs/02 §2.1：≤ 4KB，超限截断并标注）。
pub const EXIT_SUMMARY_MAX_BYTES: usize = 4096;

/// 注册表内的子代理状态。
///
/// `waiting` 的语义是「已登记、排队等并发槽」——此时没有模型请求，也不消耗 token。
/// docs/02 §2.1 只给出四个状态名，这里把 `waiting` 明确为排队态：它是唯一一个
/// 既未开始执行、又已经占用身份的生命周期阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// 已登记，任务尚未开始调度。
    Spawning,
    /// 并发槽已满，排队等待（无模型请求）。
    Waiting,
    /// 正在执行 turn 循环。
    Running,
    /// 已结束（成功/失败/取消），`ExitSummary` 可读取。
    Exited,
}

/// 子代理终态：协议层定义（`events::SubagentExitStatus`），此处复用同一份，
/// 不另立枚举——它要经 `subagent_exited` 事件跨线到前端（SH-053）。
pub use crate::events::SubagentExitStatus as ExitStatus;

/// 子代理结束摘要：父代理经 `wait_agent` 拿到的唯一结果通道（ADR-102）。
///
/// `artifacts` 当前恒为空——仓库还没有把工具轨迹归因到具体产物的机制，
/// 留空并在此声明，比填入猜测值更诚实（SH-056 接入整树 replay 时再补）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExitSummary {
    pub status: ExitStatus,
    /// 子代理最后一条 assistant 文本；无文本时给出显式说明。
    pub summary: String,
    pub artifacts: Vec<String>,
    pub token_usage: TokenTotals,
    /// 实际消耗的轮数（SH-121④：收口必须能回答「跑了几轮」）。
    /// 预算耗尽与正常完成用同一个字段对比，不能靠猜。
    #[serde(default)]
    pub turns: u32,
}

impl ExitSummary {
    /// 由一次 `run_instance` 的收口结果构造。
    pub fn from_run(
        reason: TurnEndReason,
        last_text: Option<&str>,
        token_usage: TokenTotals,
        turns: u32,
    ) -> Self {
        let (status, fallback) = match reason {
            TurnEndReason::Completed => (ExitStatus::Done, "子代理已完成，但没有产出文本结果。"),
            TurnEndReason::Cancelled => (ExitStatus::Cancelled, "子代理已被取消，没有回传结果。"),
            // 预算耗尽不是成功：它意味着子任务没有做完，必须让父代理看见。
            TurnEndReason::Failed | TurnEndReason::BudgetExhausted => {
                (ExitStatus::Failed, "子代理失败，没有产出文本结果。")
            }
        };
        let summary = last_text
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| fallback.to_string());
        Self { status, summary, artifacts: Vec::new(), token_usage, turns }
    }

    /// 取消专用的最小摘要（尚未开始运行就被取消）。
    pub fn cancelled(reason: &str) -> Self {
        Self {
            status: ExitStatus::Cancelled,
            summary: reason.to_string(),
            artifacts: Vec::new(),
            token_usage: TokenTotals::default(),
            turns: 0,
        }
    }

    /// 按序列化字节数截断到 4KB。截断的是 `summary`（唯一可自由伸缩的字段），
    /// 并留下显式标注——父代理必须能分辨「子代理只说了这么多」和「结果被裁掉了」。
    pub fn capped(mut self) -> Self {
        if serialized_len(&self) <= EXIT_SUMMARY_MAX_BYTES {
            return self;
        }
        const MARKER: &str = "…[结果已截断]";
        let original: Vec<char> = self.summary.chars().collect();
        // 上界先给 marker 独占，再二分字符数；JSON 转义可能让字节数非线性增长，
        // 所以每次都以真实序列化长度为准，而不是用字符数估算。
        self.summary = MARKER.to_string();
        if serialized_len(&self) > EXIT_SUMMARY_MAX_BYTES {
            self.summary = String::new();
            return self;
        }
        let (mut low, mut high) = (0usize, original.len());
        while low < high {
            let mid = (low + high).div_ceil(2);
            let candidate: String = original[..mid].iter().collect();
            self.summary = format!("{candidate}{MARKER}");
            if serialized_len(&self) <= EXIT_SUMMARY_MAX_BYTES {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        let candidate: String = original[..low].iter().collect();
        self.summary = format!("{candidate}{MARKER}");
        self
    }
}

fn serialized_len(summary: &ExitSummary) -> usize {
    serde_json::to_vec(summary).map(|bytes| bytes.len()).unwrap_or(usize::MAX)
}

/// 派生子代理的拒绝理由（全部在登记前 fail-closed，不产生半成品注册项）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpawnError {
    #[error("子代理不能再分叉：当前深度 {depth}（ADR-101 硬限，只有 root 能 spawn）")]
    DepthExceeded { depth: u32 },
    #[error("父代理已取消，拒绝派生子代理")]
    ParentCancelled,
    #[error("子代理任务描述为空")]
    EmptyTask,
    #[error("未知子代理: {agent_id}")]
    UnknownAgent { agent_id: String },
    #[error("子代理 {agent_id} 尚未结束，不能 resume")]
    NotExited { agent_id: String },
    #[error("子代理 {agent_id} 没有可恢复的历史（可能已随进程清理）")]
    HistoryUnavailable { agent_id: String },
    #[error("未知子代理 profile: {profile}（当前支持 worker / explorer）")]
    UnknownProfile { profile: String },
    #[error("allowed_tools 含未注册的工具: {tool}（拼错会静默收窄成空集，直接拒绝更诚实）")]
    UnknownTool { tool: String },
}

/// steering 注入的拒绝理由（类型化：命令面据此映射上游错误码，不靠文案匹配）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SteerError {
    #[error("未知子代理: {agent_id}")]
    UnknownAgent { agent_id: String },
    #[error("子代理已结束，无法注入消息: {agent_id}")]
    Exited { agent_id: String },
}

/// 注册表对外快照（命令面/测试用）。时间为相对创建时刻的毫秒数。
#[derive(Debug, Clone, PartialEq)]
pub struct AgentSnapshot {
    pub agent_id: String,
    pub parent_id: String,
    pub depth: u32,
    pub task: String,
    pub status: AgentStatus,
    pub elapsed_ms: u64,
    pub exit: Option<ExitSummary>,
}

struct Entry {
    parent_id: String,
    depth: u32,
    task: String,
    /// 泳道名（上游 catalog v3 的 `name`）：兄弟间唯一，派生时定下不再改。
    name: String,
    status: AgentStatus,
    created_at: Instant,
    exit: Option<ExitSummary>,
    cancel: CancellationToken,
    /// 子代理的 steering 信箱（与 `AgentInstance::inbox` 同一 Arc）。
    inbox: Arc<Mutex<Vec<String>>>,
    /// 子代理身份的事件出口（ADR-104）：生命周期与 steering 事件都从这里发。
    sink: Arc<dyn EventSink>,
    /// 本子代理的有效工具白名单（SH-054）：派生时定下，`resume` 必须沿用，
    /// 否则「结束 → resume」会成为一条绕过收窄的提权路径。
    allowed_tools: Option<crate::permission::ToolAllowlist>,
    /// 结束时的完整历史（`resume_agent` 的内存来源；SH-056 再升级为 rollout 重放）。
    history: Option<Vec<HistoryItem>>,
    done: Arc<Notify>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

/// 进程内子代理注册表。
///
/// 持 `Arc<Self>` 才能 `spawn`；任务内部只持 `Weak`，因此外部句柄全部释放时
/// `Drop` 一定执行：取消全部子令牌并 abort 任务（docs/02 R102，防止僵尸子代理）。
pub struct SubagentRegistry {
    inner: Mutex<BTreeMap<String, Entry>>,
    slots: Arc<Semaphore>,
    max_concurrent: usize,
}

impl SubagentRegistry {
    /// `max_concurrent` 会被钳制到 `1..=HARD_MAX_CONCURRENT`：0 会让所有子代理永远排队，
    /// 超过硬顶则违背 ADR-101 的成本约束。
    pub fn new(max_concurrent: usize) -> Self {
        let max_concurrent = max_concurrent.clamp(1, HARD_MAX_CONCURRENT);
        Self {
            inner: Mutex::new(BTreeMap::new()),
            slots: Arc::new(Semaphore::new(max_concurrent)),
            max_concurrent,
        }
    }

    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, Entry>> {
        // 注册表状态只有本模块写；持锁 panic 说明代码有 bug，但不应让后续所有
        // 子代理生命周期查询一起熔断，因此取回内部数据继续。
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 派生子代理：登记 → 排队 → 执行 → 收口，全程不阻塞调用方。
    ///
    /// 返回的 `agent_id` 可立即用于 `wait_exit`/`cancel`；子代理是否真正开始执行
    /// 取决于并发槽与父令牌状态。
    pub fn spawn(
        self: &Arc<Self>,
        engine: Arc<AgentLoop>,
        parent: &AgentScope,
        task: impl Into<String>,
        rollout: Option<Arc<RolloutWriter>>,
    ) -> Result<String, SpawnError> {
        self.spawn_with_budget(engine, parent, task, rollout, None)
    }

    /// `spawn` 的预算参数版本：`budget_tokens` = 该子树自己的 token 上限（SH-055）。
    /// 不传就只共享全树账本；传了就派生一本子账，消耗照旧向上汇总。
    pub fn spawn_with_budget(
        self: &Arc<Self>,
        engine: Arc<AgentLoop>,
        parent: &AgentScope,
        task: impl Into<String>,
        rollout: Option<Arc<RolloutWriter>>,
        budget_tokens: Option<u64>,
    ) -> Result<String, SpawnError> {
        self.spawn_with_constraints(
            engine,
            parent,
            task,
            rollout,
            budget_tokens,
            SpawnNarrowing::default(),
        )
    }

    /// 带权限收窄参数的派生（SH-054，ADR-103）。
    ///
    /// 有效工具集 = `min(父代理, profile, allowed_tools)`：
    /// 1. profile 给出该预设的工具集（worker = 全量，explorer = 只读档）；
    /// 2. `allowed_tools` 再收窄一层；
    /// 3. 最后与父代理的有效集取交集——父代理已被收窄的能力不会在子代理处放大。
    ///
    /// 全部收窄都在登记前完成：任何一项不合法都 fail-closed，不产生半成品条目。
    pub fn spawn_with_constraints(
        self: &Arc<Self>,
        engine: Arc<AgentLoop>,
        parent: &AgentScope,
        task: impl Into<String>,
        rollout: Option<Arc<RolloutWriter>>,
        budget_tokens: Option<u64>,
        narrowing: SpawnNarrowing<'_>,
    ) -> Result<String, SpawnError> {
        let task = task.into();
        validate_parent(parent, &task)?;
        let effective = effective_allowlist(&engine, parent, narrowing)?;
        let agent_id = ulid::Ulid::generate().to_string();
        // 级联的来源就在这一行：父令牌取消时，这个子令牌自动进入取消态。
        let cancel = parent.cancel.child_token();
        let mut instance = AgentInstance::child(&agent_id, &parent.agent_id, &task, cancel);
        instance.rollout = rollout;
        // 预算共账（SH-055）：子代理挂引擎树账本；传了 budget_tokens 就再收窄一层。
        instance.budget = engine.tree_budget.derive(budget_tokens, None);
        // 权限只降不升（SH-054）：把交点固化到实例上，执行期只读。
        instance.allowed_tools = effective.clone();
        let sink = child_sink(parent, &agent_id);
        Ok(self.start_run(engine, instance, task, sink, effective))
    }

    /// 从已退出子代理的内存历史恢复出一个**新**子代理（docs/02 §2.2 `resume_agent`）。
    ///
    /// 诚实声明：历史来源是进程内 `Entry.history`，不是 rollout 重放——进程重启后不可恢复。
    /// 每代理档案在 SH-056 落地后再升级；在此之前不假装支持跨进程 resume。
    pub fn resume(
        self: &Arc<Self>,
        engine: Arc<AgentLoop>,
        parent: &AgentScope,
        source_agent_id: &str,
        task: Option<&str>,
        rollout: Option<Arc<RolloutWriter>>,
    ) -> Result<String, SpawnError> {
        let task = task
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or("请接着上次未完成的部分继续工作，并给出结论。")
            .to_string();
        validate_parent(parent, &task)?;
        let (status, history, inherited_tools) = {
            let inner = self.lock();
            let entry = inner.get(source_agent_id).ok_or_else(|| SpawnError::UnknownAgent {
                agent_id: source_agent_id.to_string(),
            })?;
            (entry.status, entry.history.clone(), entry.allowed_tools.clone())
        };
        if status != AgentStatus::Exited {
            return Err(SpawnError::NotExited { agent_id: source_agent_id.to_string() });
        }
        let history = history.ok_or_else(|| SpawnError::HistoryUnavailable {
            agent_id: source_agent_id.to_string(),
        })?;
        // 续跑不得提权：沿用原实例的收窄结果，并与父代理再次取交集。
        let effective = crate::permission::policy::intersect_allowlist(
            parent.allowed_tools.as_ref(),
            inherited_tools.as_ref(),
        );

        let agent_id = ulid::Ulid::generate().to_string();
        let cancel = parent.cancel.child_token();
        let mut instance = AgentInstance::child(&agent_id, &parent.agent_id, &task, cancel);
        // 沿用旧历史 + 一条续跑指令；recorded 保持 0：新代理尚无自己的档案，
        // 落盘时从头写一遍才不会漏掉恢复出来的上下文。
        instance.history = history;
        instance.history.push(HistoryItem::Message { role: Role::User, text: task.clone(), reasoning: None });
        instance.rollout = rollout;
        instance.allowed_tools = effective.clone();
        // 续跑同样计入同一棵树（SH-055）。
        instance.budget = engine.tree_budget.child();
        let sink = child_sink(parent, &agent_id);
        Ok(self.start_run(engine, instance, task, sink, effective))
    }

    /// 登记 + 调度 + 收口。调用方已完成全部 fail-closed 校验。
    fn start_run(
        self: &Arc<Self>,
        engine: Arc<AgentLoop>,
        mut instance: AgentInstance,
        task: String,
        sink: Arc<dyn EventSink>,
        allowed_tools: Option<crate::permission::ToolAllowlist>,
    ) -> String {
        let agent_id = instance.agent_id.clone();
        let parent_id = instance.parent_id.clone().unwrap_or_default();
        let depth = instance.depth;
        let cancel = instance.cancel.clone();
        let inbox = instance.inbox.clone();

        let done = Arc::new(Notify::new());
        let created_at_ms = wall_clock_ms();
        let label = lane_label(&task);
        let name = {
            let mut inner = self.lock();
            let name = next_lane_name(&inner, &parent_id);
            inner.insert(
                agent_id.clone(),
                Entry {
                    parent_id,
                    depth,
                    task: task.clone(),
                    name: name.clone(),
                    status: AgentStatus::Spawning,
                    created_at: Instant::now(),
                    exit: None,
                    cancel: cancel.clone(),
                    inbox,
                    sink: sink.clone(),
                    allowed_tools,
                    history: None,
                    done: done.clone(),
                    handle: None,
                },
            );
            name
        };
        // 锁外广播：出口要做落盘/WS/IPC 三个 I/O，持注册表锁发事件会挡住兄弟派生。
        emit(
            &sink,
            EventMsg::SubagentSpawned { child_id: agent_id.clone(), task, name, label, created_at_ms },
        );

        let weak = Arc::downgrade(self);
        let slots = self.slots.clone();
        let task_id = agent_id.clone();
        let task_done = done.clone();
        let queued_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            set_status(&weak, &task_id, AgentStatus::Waiting);
            // 排队也要能被取消：否则父任务停止后，排队中的子代理会在槽位空出时
            // 突然开始烧 token。
            let permit: OwnedSemaphorePermit = tokio::select! {
                _ = queued_cancel.cancelled() => {
                    finish(&weak, &task_id, &task_done, ExitSummary::cancelled("已取消（尚未开始运行）。"), None);
                    return;
                }
                permit = slots.acquire_owned() => match permit {
                    Ok(permit) => permit,
                    Err(_) => {
                        finish(&weak, &task_id, &task_done, ExitSummary::cancelled("子代理调度器已关闭。"), None);
                        return;
                    }
                },
            };
            set_status(&weak, &task_id, AgentStatus::Running);
            let summary = engine.run_instance(&mut instance, sink).await;
            let exit = ExitSummary::from_run(
                summary.reason,
                instance.last_assistant_text(),
                instance.usage,
                summary.turns,
            );
            // 历史必须在收口前快照：finish 之后等待方可能立即 resume。
            let history = instance.history.clone();
            // 先释放槽位再收口：等结果的一方可能立刻派生下一批子代理。
            drop(permit);
            finish(&weak, &task_id, &task_done, exit, Some(history));
        });

        let mut inner = self.lock();
        if let Some(entry) = inner.get_mut(&agent_id) {
            entry.handle = Some(handle);
        } else {
            // 理论上不可达（登记在 spawn 前完成）；真出现就放弃这个任务，不留无主句柄。
            handle.abort();
        }
        agent_id
    }

    /// 向未结束的子代理注入一条 steering 消息；结束/未知都 fail-closed（类型化拒绝）。
    ///
    /// `rpc_id` 只在 UI 发起注入时带：泳道据此把消息 source 写成 `user + rpcId`，
    /// 客户端撤销乐观回显；工具/父代理注入为 `None`，落 `agent-message`。
    pub fn send_message(&self, agent_id: &str, message: &str, rpc_id: Option<&str>) -> Result<(), SteerError> {
        let (inbox, sink, sender_id) = {
            let inner = self.lock();
            let entry = inner.get(agent_id).ok_or_else(|| SteerError::UnknownAgent {
                agent_id: agent_id.to_string(),
            })?;
            if entry.status == AgentStatus::Exited {
                return Err(SteerError::Exited { agent_id: agent_id.to_string() });
            }
            (entry.inbox.clone(), entry.sink.clone(), entry.parent_id.clone())
        };
        inbox
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(message.to_string());
        // 注入已生效，事件只是通知：广播失败不回滚投递（与 agent_loop 的事件口径一致）。
        emit(
            &sink,
            EventMsg::SubagentMessage {
                child_id: agent_id.to_string(),
                sender_id,
                text: message.to_string(),
                rpc_id: rpc_id.map(str::to_string),
            },
        );
        Ok(())
    }

    /// 已退出子代理的内存历史（`resume` 的数据源）。
    pub fn history_of(&self, agent_id: &str) -> Option<Vec<HistoryItem>> {
        self.lock().get(agent_id).and_then(|entry| entry.history.clone())
    }

    /// 测试用：读取子代理有效工具白名单（`None` = 无限制）。
    #[cfg(test)]
    pub(crate) fn allowed_tools_of(
        &self,
        agent_id: &str,
    ) -> Option<Option<crate::permission::ToolAllowlist>> {
        self.lock().get(agent_id).map(|entry| entry.allowed_tools.clone())
    }

    /// 当前快照（按 ULID 即创建时间升序）。
    pub fn snapshot(&self) -> Vec<AgentSnapshot> {
        let inner = self.lock();
        inner
            .iter()
            .map(|(agent_id, entry)| AgentSnapshot {
                agent_id: agent_id.clone(),
                parent_id: entry.parent_id.clone(),
                depth: entry.depth,
                task: entry.task.clone(),
                status: entry.status,
                elapsed_ms: u64::try_from(entry.created_at.elapsed().as_millis()).unwrap_or(u64::MAX),
                exit: entry.exit.clone(),
            })
            .collect()
    }

    pub fn status_of(&self, agent_id: &str) -> Option<AgentStatus> {
        self.lock().get(agent_id).map(|entry| entry.status)
    }

    pub fn exit_of(&self, agent_id: &str) -> Option<ExitSummary> {
        self.lock().get(agent_id).and_then(|entry| entry.exit.clone())
    }

    /// 仍在运行的子代理数（不含排队与已结束）。
    pub fn running_count(&self) -> usize {
        self.lock().values().filter(|entry| entry.status == AgentStatus::Running).count()
    }

    /// 未结束的子代理数（spawning + waiting + running）。
    pub fn live_count(&self) -> usize {
        self.lock().values().filter(|entry| entry.status != AgentStatus::Exited).count()
    }

    /// 取消一个子代理；未知 id 返回 `false`。
    pub fn cancel(&self, agent_id: &str) -> bool {
        match self.lock().get(agent_id) {
            Some(entry) => {
                entry.cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// 取消全部子代理（父任务停止的兜底入口；正常路径由父令牌级联完成）。
    pub fn cancel_all(&self) {
        for entry in self.lock().values() {
            entry.cancel.cancel();
        }
    }

    /// 等待某个子代理结束；超时返回 `None`，**不取消**该子代理（docs/02 §2.2
    /// `wait_agent` 的语义：超时只返回部分结果，不杀代理）。
    pub async fn wait_exit(&self, agent_id: &str, timeout: std::time::Duration) -> Option<ExitSummary> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let (notify, exit) = {
                let inner = self.lock();
                let entry = inner.get(agent_id)?;
                (entry.done.clone(), entry.exit.clone())
            };
            if exit.is_some() {
                return exit;
            }
            // 先注册兴趣再复查：避免 finish 落在「复查之后、await 之前」导致永久挂起。
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(exit) = self.exit_of(agent_id) {
                return Some(exit);
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return self.exit_of(agent_id);
            }
            match tokio::time::timeout(remaining, notified).await {
                Ok(()) => continue,
                Err(_) => return self.exit_of(agent_id),
            }
        }
    }
}

impl Drop for SubagentRegistry {
    fn drop(&mut self) {
        let inner = self.inner.get_mut().unwrap_or_else(|poisoned| poisoned.into_inner());
        for entry in inner.values() {
            // 双重保险：令牌覆盖执行中的循环，abort 覆盖任何不观察取消的挂起 await。
            entry.cancel.cancel();
            if let Some(handle) = &entry.handle {
                handle.abort();
            }
        }
    }
}

fn set_status(weak: &Weak<SubagentRegistry>, agent_id: &str, status: AgentStatus) {
    if let Some(registry) = weak.upgrade() {
        if let Some(entry) = registry.lock().get_mut(agent_id) {
            entry.status = status;
        }
    }
}

/// 派生/恢复共用的父侧 fail-closed 校验（任一不满足都不得登记）。
fn validate_parent(parent: &AgentScope, task: &str) -> Result<(), SpawnError> {
    if task.trim().is_empty() {
        return Err(SpawnError::EmptyTask);
    }
    if parent.depth >= MAX_AGENT_DEPTH {
        return Err(SpawnError::DepthExceeded { depth: parent.depth });
    }
    if parent.cancel.is_cancelled() {
        return Err(SpawnError::ParentCancelled);
    }
    Ok(())
}

/// 派生的权限收窄声明（ADR-103）：profile 预设 + 显式工具白名单。
///
/// 两者都只能减不能加；集中成一个结构体是为了让「派生时带了哪些收窄」
/// 在签名上就是一件事，而不是散在参数列表里靠位置记忆。
#[derive(Debug, Clone, Copy, Default)]
pub struct SpawnNarrowing<'a> {
    pub profile: Option<&'a str>,
    pub allowed_tools: Option<&'a [String]>,
}

/// min(父策略, profile, allowed_tools) 的唯一落点（SH-054，ADR-103）。
///
/// 三步都是收窄操作，顺序不影响结果，但都必须在派生**之前**完成：
/// 任何一步不合法都拒绝登记，不能先建出代理再回头补救。
fn effective_allowlist(
    engine: &AgentLoop,
    parent: &AgentScope,
    narrowing: SpawnNarrowing<'_>,
) -> Result<Option<crate::permission::ToolAllowlist>, SpawnError> {
    // profile：预设的能力集。worker = 全量（None），explorer = 只读档。
    let profile_tools = match narrowing.profile {
        None | Some("worker") => None,
        Some("explorer") => Some(crate::permission::policy::readonly_allowlist(
            engine.router.specs().into_iter().map(|spec| spec.name),
        )),
        Some(other) => return Err(SpawnError::UnknownProfile { profile: other.to_string() }),
    };
    // 显式 allowed_tools：只接受路由里真实存在的工具名（拼错静默成空集比直接拒绝更难查）。
    let requested_tools = match narrowing.allowed_tools {
        None => None,
        Some(names) => {
            let mut set = std::collections::HashSet::new();
            for name in names {
                let name = name.trim();
                if name.is_empty() || engine.router.get(name).is_none() {
                    return Err(SpawnError::UnknownTool { tool: name.to_string() });
                }
                set.insert(name.to_string());
            }
            Some(Arc::new(set))
        }
    };
    let declared = crate::permission::policy::intersect_allowlist(
        profile_tools.as_ref(),
        requested_tools.as_ref(),
    );
    Ok(crate::permission::policy::intersect_allowlist(
        parent.allowed_tools.as_ref(),
        declared.as_ref(),
    ))
}

/// 子代理事件必须带自己的身份（ADR-104）：出口不支持 scoped 时退回父出口，
/// 而不是丢事件——前端宁可看到错误归属，也不能看不到子代理的进展。
fn child_sink(parent: &AgentScope, agent_id: &str) -> Arc<dyn EventSink> {
    parent
        .sink
        .scoped(agent_id, Some(&parent.agent_id))
        .unwrap_or_else(|| parent.sink.clone())
}

fn finish(
    weak: &Weak<SubagentRegistry>,
    agent_id: &str,
    done: &Notify,
    exit: ExitSummary,
    history: Option<Vec<HistoryItem>>,
) {
    let mut broadcast = None;
    if let Some(registry) = weak.upgrade() {
        let mut inner = registry.lock();
        if let Some(entry) = inner.get_mut(agent_id) {
            entry.status = AgentStatus::Exited;
            let exit = exit.capped();
            broadcast = Some((entry.sink.clone(), exit.status, exit.summary.clone()));
            entry.exit = Some(exit);
            entry.history = history;
        }
    }
    if let Some((sink, status, summary)) = broadcast {
        // 先广播再唤醒等待方：父代理拿到 ExitSummary 时，前端泳道已经收口。
        emit(&sink, EventMsg::SubagentExited { child_id: agent_id.to_string(), status, summary });
    }
    done.notify_waiters();
}

/// 事件广播失败只告警：子代理的执行结果已经落进注册表，不能因为通知失败而丢结果。
fn emit(sink: &Arc<dyn EventSink>, msg: EventMsg) {
    if let Err(error) = sink.emit(&msg) {
        tracing::warn!("子代理事件广播失败（执行结果不受影响）: {error}");
    }
}

fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// 泳道展示标签：task 首行，按字符截到 60（上游 catalog v3 的 `label` 是人读标题，
/// 不是任务全文；全文另有 `task` 字段承载）。
fn lane_label(task: &str) -> String {
    const MAX_CHARS: usize = 60;
    let first_line = task.lines().next().unwrap_or("").trim();
    let chars: Vec<char> = first_line.chars().collect();
    if chars.len() <= MAX_CHARS {
        return first_line.to_string();
    }
    format!("{}…", chars[..MAX_CHARS].iter().collect::<String>())
}

/// 兄弟间唯一的泳道名（上游 names.ts：name 是模型可寻址标识，重名会串树）。
/// SHANIU 用 agent_id 寻址，name 只作 UI 标识，因此由注册表按兄弟序派生，
/// 不让模型自选——少一个校验/冲突面，也不必改 ToolSpec。
fn next_lane_name(inner: &BTreeMap<String, Entry>, parent_id: &str) -> String {
    let siblings: Vec<&str> = inner
        .values()
        .filter(|entry| entry.parent_id == parent_id)
        .map(|entry| entry.name.as_str())
        .collect();
    let mut index = siblings.len() + 1;
    loop {
        let candidate = format!("worker-{index}");
        if !siblings.iter().any(|name| *name == candidate) {
            return candidate;
        }
        index += 1;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    use super::*;
    use crate::agent_loop::ModelBackend;
    use crate::config::{ApprovalMode, SandboxMode};
    use crate::events::EventMsg;
    use crate::model_client::{HistoryItem, ModelStream, ModelStreamEvent, ResponseItem, Usage};
    use crate::permission::{policy, ApprovalBroker};
    use crate::tool_router::{PlanState, ToolRouter};

    /// 按脚本作答的后端；脚本耗尽 = 永不结束的流（模拟长任务）。
    struct ScriptedBackend(StdMutex<std::collections::VecDeque<Vec<ModelStreamEvent>>>);

    impl ScriptedBackend {
        fn new(turns: Vec<Vec<ModelStreamEvent>>) -> Self {
            Self(StdMutex::new(turns.into()))
        }
    }

    #[async_trait::async_trait]
    impl ModelBackend for ScriptedBackend {
        async fn stream(
            &self,
            _history: &[HistoryItem],
            _tools: &[crate::model_client::ToolDeclaration],
            _instructions: &str,
        ) -> Result<ModelStream, crate::model_client::ModelError> {
            let mut queue = self.0.lock().unwrap();
            match queue.pop_front() {
                Some(script) if !script.is_empty() => Ok(Box::pin(futures::stream::iter(script))),
                _ => Ok(Box::pin(futures::stream::pending())),
            }
        }
    }

    struct DiscardSink;

    impl EventSink for DiscardSink {
        fn emit(&self, _msg: &EventMsg) -> Result<(), String> {
            Ok(())
        }
    }

    fn engine(turns: Vec<Vec<ModelStreamEvent>>) -> Arc<AgentLoop> {
        Arc::new(AgentLoop {
            backend: Arc::new(ScriptedBackend::new(turns)),
            router: Arc::new(ToolRouter::new()),
            broker: Arc::new(ApprovalBroker::new()),
            plan_state: Arc::new(PlanState::new()),
            workspace_root: std::env::temp_dir(),
            max_turns: 2,
            token_budget: 0,
            tree_budget: Default::default(),
            policy: Arc::new(policy::SessionPolicyHandle::new(policy::SessionPolicy {
                approval: ApprovalMode::OnRequest,
                sandbox: SandboxMode::WorkspaceWrite,
            })),
            rollout: None,
            rollout_broke: std::sync::atomic::AtomicBool::new(false),
            session_id: None,
            jobs: None,
            stop_hook: None,
            verify: Default::default(),
            retry: crate::model_client::RetryPolicy::disabled(),
        })
    }

    fn root_scope() -> AgentScope {
        AgentScope {
            agent_id: "root".into(),
            parent_id: None,
            depth: 0,
            cancel: CancellationToken::new(),
            sink: Arc::new(DiscardSink),
            allowed_tools: None,
        }
    }

    fn child_scope() -> AgentScope {
        AgentScope { agent_id: "child-1".into(), parent_id: Some("root".into()), depth: 1, ..root_scope() }
    }

    async fn wait_status(registry: &SubagentRegistry, expected: AgentStatus, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let states = registry.snapshot();
            if states.iter().any(|state| state.status == expected) {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "等待状态 {expected:?} 超时；当前快照: {states:?}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// 正常路径：子代理跑完，ExitSummary 带最后一条 assistant 文本与真实用量。
    #[tokio::test]
    async fn child_runs_to_completion_and_reports_exit_summary() {
        let registry = Arc::new(SubagentRegistry::new(DEFAULT_MAX_CONCURRENT));
        let engine = engine(vec![vec![
            ModelStreamEvent::OutputTextDelta { delta: "子任务结论".into() },
            ModelStreamEvent::Completed {
                usage: Usage { input_tokens: 11, cached_input_tokens: Some(3), output_tokens: 7 },
            },
        ]]);
        let scope = root_scope();
        let id = registry
            .spawn(engine, &scope, "探索仓库", None)
            .expect("派生应成功");

        let exit = registry.wait_exit(&id, Duration::from_secs(3)).await.expect("子代理应在超时前结束");
        assert_eq!(exit.status, ExitStatus::Done);
        assert_eq!(exit.summary, "子任务结论");
        assert_eq!(registry.status_of(&id), Some(AgentStatus::Exited));
        // 用量从真实事件累加，不是估算。
        assert_eq!(exit.token_usage, TokenTotals { input_tokens: 11, cached_input_tokens: 3, output_tokens: 7 });
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].parent_id, "root");
        assert_eq!(snapshot[0].depth, 1);
        assert_eq!(snapshot[0].task, "探索仓库");
    }

    /// 深度硬限：子代理自己不能 spawn（ADR-101），并且拒绝发生在登记前。
    #[tokio::test]
    async fn child_agent_cannot_spawn_grandchildren() {
        let registry = Arc::new(SubagentRegistry::new(DEFAULT_MAX_CONCURRENT));
        let child = child_scope();
        let error = registry
            .spawn(engine(vec![]), &child, "孙任务", None)
            .expect_err("深度 1 不得再派生子代理");
        assert_eq!(error, SpawnError::DepthExceeded { depth: 1 });
        assert!(registry.snapshot().is_empty(), "拒绝必须在登记前发生");
    }

    #[tokio::test]
    async fn empty_task_and_cancelled_parent_are_rejected_before_registration() {
        let registry = Arc::new(SubagentRegistry::new(DEFAULT_MAX_CONCURRENT));
        let scope = root_scope();
        assert_eq!(
            registry.spawn(engine(vec![]), &scope, "   ", None),
            Err(SpawnError::EmptyTask)
        );

        let cancelled_scope = root_scope();
        cancelled_scope.cancel.cancel();
        assert_eq!(
            registry.spawn(engine(vec![]), &cancelled_scope, "任务", None),
            Err(SpawnError::ParentCancelled)
        );
        assert!(registry.snapshot().is_empty());
    }

    /// 并发上限：第二个子代理排队（waiting），第一个结束后才拿到槽位。
    #[tokio::test]
    async fn concurrency_cap_queues_extra_agents_until_a_slot_frees() {
        let registry = Arc::new(SubagentRegistry::new(1));
        let engine = engine(vec![]);
        let scope = root_scope();
        let first = registry
            .spawn(engine.clone(), &scope, "长任务 A", None)
            .expect("A 应成功");
        wait_status(&registry, AgentStatus::Running, Duration::from_secs(2)).await;

        let second = registry
            .spawn(engine, &scope, "长任务 B", None)
            .expect("B 应成功");
        wait_status(&registry, AgentStatus::Waiting, Duration::from_secs(2)).await;
        assert_eq!(registry.status_of(&first), Some(AgentStatus::Running));
        assert_eq!(registry.running_count(), 1);

        // 释放槽位：A 取消 → B 必须自动开始，而不是被永久饿死。
        assert!(registry.cancel(&first));
        registry.wait_exit(&first, Duration::from_secs(2)).await.expect("A 应结束");
        wait_status(&registry, AgentStatus::Running, Duration::from_secs(2)).await;
        let states = registry.snapshot();
        let b = states.iter().find(|state| state.agent_id == second).expect("B 仍在表中");
        assert_eq!(b.status, AgentStatus::Running);

        registry.cancel_all();
        registry.wait_exit(&second, Duration::from_secs(2)).await.expect("B 应结束");
        assert_eq!(registry.live_count(), 0);
    }

    /// 子代理取消不牵连兄弟：每个子令牌只受自己的父/自身取消影响。
    #[tokio::test]
    async fn cancelling_one_child_leaves_the_other_running() {
        let registry = Arc::new(SubagentRegistry::new(DEFAULT_MAX_CONCURRENT));
        let engine = engine(vec![]);
        let scope = root_scope();
        let a = registry.spawn(engine.clone(), &scope, "A", None).unwrap();
        let b = registry.spawn(engine, &scope, "B", None).unwrap();
        wait_status(&registry, AgentStatus::Running, Duration::from_secs(2)).await;
        assert_eq!(registry.running_count(), 2);

        assert!(registry.cancel(&a));
        let exit = registry.wait_exit(&a, Duration::from_secs(2)).await.expect("A 应结束");
        assert_eq!(exit.status, ExitStatus::Cancelled);
        assert_eq!(registry.status_of(&b), Some(AgentStatus::Running));

        registry.cancel_all();
        registry.wait_exit(&b, Duration::from_secs(2)).await.expect("B 应结束");
    }

    /// 4KB 上限：超长摘要被截断且带明确标注，JSON 字节数不超限。
    #[test]
    fn exit_summary_is_truncated_to_four_kilobytes_with_marker() {
        let exit = ExitSummary {
            status: ExitStatus::Done,
            summary: "结".repeat(10_000),
            artifacts: vec![],
            token_usage: TokenTotals::default(),
            turns: 1,
        };
        let capped = exit.capped();
        assert!(serialized_len(&capped) <= EXIT_SUMMARY_MAX_BYTES);
        assert!(capped.summary.ends_with("…[结果已截断]"));
        // 防假绿：未超限的摘要必须原样保留（截断器不能无脑裁）。
        let short = ExitSummary {
            status: ExitStatus::Done,
            summary: "短结论".into(),
            artifacts: vec![],
            token_usage: TokenTotals::default(),
            turns: 1,
        };
        assert_eq!(short.clone().capped(), short);
    }

    /// 预算耗尽映射为 failed，而不是 done——父代理必须知道子任务没做完。
    #[tokio::test]
    async fn budget_exhaustion_maps_to_failed_exit() {
        let registry = Arc::new(SubagentRegistry::new(DEFAULT_MAX_CONCURRENT));
        let tool_call = |call_id: &str| {
            ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                name: "read_file".into(),
                arguments: "{}".into(),
                call_id: call_id.into(),
            })
        };
        let engine = engine(vec![vec![tool_call("c1")], vec![tool_call("c2")]]);
        let scope = root_scope();
        let id = registry
            .spawn(engine, &scope, "永远要工具的子任务", None)
            .expect("派生应成功");
        let exit = registry.wait_exit(&id, Duration::from_secs(3)).await.expect("应结束");
        assert_eq!(exit.status, ExitStatus::Failed);
        assert!(exit.summary.contains("失败"), "无文本时须给出显式失败说明: {}", exit.summary);
    }

    /// send_agent_message：运行中的子代理必须在本轮采样前收到 steering 消息。
    /// 用闸门把第一次采样卡住，确保注入发生在「已开始运行」之后——否则测的是
    /// 「消息碰巧排在任务前面」，证明不了运行中注入这条路径。
    #[tokio::test]
    async fn steering_message_reaches_running_child_before_next_sample() {
        struct GateBackend {
            inner: ScriptedBackend,
            started: Arc<Notify>,
            gate: Arc<Notify>,
            /// 只卡第一次采样：注入发生在第一轮进行中，消息应在下一轮进入上下文。
            gated: std::sync::atomic::AtomicBool,
        }

        #[async_trait::async_trait]
        impl ModelBackend for GateBackend {
            async fn stream(
                &self,
                history: &[HistoryItem],
                tools: &[crate::model_client::ToolDeclaration],
                instructions: &str,
            ) -> Result<ModelStream, crate::model_client::ModelError> {
                if !self.gated.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    self.started.notify_one();
                    self.gate.notified().await;
                }
                self.inner.stream(history, tools, instructions).await
            }
        }

        let started = Arc::new(Notify::new());
        let gate = Arc::new(Notify::new());
        let engine = Arc::new(AgentLoop {
            backend: Arc::new(GateBackend {
                inner: ScriptedBackend::new(vec![
                    // 第一轮必须产生工具调用，否则循环在第一轮就结束，没有「下一次采样」。
                    vec![
                        ModelStreamEvent::OutputItem(crate::model_client::ResponseItem::FunctionCall {
                            name: "read_file".into(),
                            arguments: r#"{"path":"x"}"#.into(),
                            call_id: "c1".into(),
                        }),
                        ModelStreamEvent::Completed {
                            usage: Usage { input_tokens: 1, cached_input_tokens: None, output_tokens: 1 },
                        },
                    ],
                    vec![
                        ModelStreamEvent::OutputTextDelta { delta: "完成".into() },
                        ModelStreamEvent::Completed {
                            usage: Usage { input_tokens: 1, cached_input_tokens: None, output_tokens: 1 },
                        },
                    ],
                ]),
                started: started.clone(),
                gate: gate.clone(),
                gated: std::sync::atomic::AtomicBool::new(false),
            }),
            router: Arc::new(ToolRouter::new()),
            broker: Arc::new(ApprovalBroker::new()),
            plan_state: Arc::new(PlanState::new()),
            workspace_root: std::env::temp_dir(),
            max_turns: 2,
            token_budget: 0,
            tree_budget: Default::default(),
            policy: Arc::new(policy::SessionPolicyHandle::new(policy::SessionPolicy {
                approval: ApprovalMode::OnRequest,
                sandbox: SandboxMode::WorkspaceWrite,
            })),
            rollout: None,
            rollout_broke: std::sync::atomic::AtomicBool::new(false),
            session_id: None,
            jobs: None,
            stop_hook: None,
            verify: Default::default(),
            retry: crate::model_client::RetryPolicy::disabled(),
        });
        let registry = Arc::new(SubagentRegistry::new(DEFAULT_MAX_CONCURRENT));
        let scope = root_scope();
        let id = registry.spawn(engine, &scope, "长任务", None).expect("派生应成功");

        tokio::time::timeout(Duration::from_secs(2), started.notified())
            .await
            .expect("子代理应进入采样");
        registry.send_message(&id, "请优先检查 X", None).expect("运行中应可注入");
        gate.notify_one();

        let exit = registry.wait_exit(&id, Duration::from_secs(3)).await.expect("应结束");
        assert_eq!(exit.status, ExitStatus::Done);
        let history = registry.history_of(&id).expect("结束时应保存历史");
        assert!(
            history.iter().any(|item| matches!(
                item,
                HistoryItem::Message { text, .. } if text == "请优先检查 X"
            )),
            "steering 消息必须进入子代理上下文: {history:?}"
        );
    }

    /// send_agent_message 的 fail-closed 分支：未知 id 与已结束代理都拒绝。
    #[tokio::test]
    async fn messages_to_unknown_or_exited_agents_are_rejected() {
        let registry = Arc::new(SubagentRegistry::new(DEFAULT_MAX_CONCURRENT));
        assert!(registry.send_message("no-such-agent", "hi", None).is_err());

        let engine = engine(vec![vec![ModelStreamEvent::Completed {
            usage: Usage { input_tokens: 1, cached_input_tokens: None, output_tokens: 1 },
        }]]);
        let scope = root_scope();
        let id = registry.spawn(engine, &scope, "短任务", None).expect("派生应成功");
        registry.wait_exit(&id, Duration::from_secs(3)).await.expect("应结束");
        assert!(registry.send_message(&id, "hi", None).is_err(), "已结束的代理不得再收消息");
    }

    /// resume_agent：已退出代理的历史被新代理沿用，并追加续跑指令。
    #[tokio::test]
    async fn resume_reuses_exited_history_and_appends_instruction() {
        let registry = Arc::new(SubagentRegistry::new(DEFAULT_MAX_CONCURRENT));
        let engine = engine(vec![
            vec![
                ModelStreamEvent::OutputTextDelta { delta: "第一轮结论".into() },
                ModelStreamEvent::Completed {
                    usage: Usage { input_tokens: 2, cached_input_tokens: None, output_tokens: 2 },
                },
            ],
            vec![
                ModelStreamEvent::OutputTextDelta { delta: "第二轮结论".into() },
                ModelStreamEvent::Completed {
                    usage: Usage { input_tokens: 3, cached_input_tokens: None, output_tokens: 3 },
                },
            ],
        ]);
        let scope = root_scope();
        let first = registry.spawn(engine.clone(), &scope, "初版任务", None).expect("派生应成功");
        let first_exit = registry.wait_exit(&first, Duration::from_secs(3)).await.expect("应结束");
        assert_eq!(first_exit.summary, "第一轮结论");

        let resumed = registry
            .resume(engine, &scope, &first, Some("继续检查边界"), None)
            .expect("resume 应成功");
        assert_ne!(resumed, first, "resume 必须产生新身份，不能复活旧条目");
        let exit = registry.wait_exit(&resumed, Duration::from_secs(3)).await.expect("应结束");
        assert_eq!(exit.summary, "第二轮结论");
        let history = registry.history_of(&resumed).expect("应保存历史");
        assert!(history.iter().any(|item| matches!(
            item,
            HistoryItem::Message { text, .. } if text == "第一轮结论"
        )), "恢复后的历史必须包含旧结论");
        assert!(history.iter().any(|item| matches!(
            item,
            HistoryItem::Message { text, .. } if text == "继续检查边界"
        )), "续跑指令必须进入历史");
    }

    /// 负向：父代理在子代理存续期间收窄后，resume 必须按父**当前**白名单重算，
    /// 不得让「结束 → resume」成为绕过收窄的提权路径。
    #[tokio::test]
    async fn resume_after_parent_narrowing_cannot_widen_child() {
        let registry = Arc::new(SubagentRegistry::new(DEFAULT_MAX_CONCURRENT));
        let engine = engine(vec![
            vec![ModelStreamEvent::Completed {
                usage: Usage { input_tokens: 1, cached_input_tokens: None, output_tokens: 1 },
            }],
            vec![ModelStreamEvent::Completed {
                usage: Usage { input_tokens: 1, cached_input_tokens: None, output_tokens: 1 },
            }],
        ]);
        let root = root_scope();
        let first = registry.spawn(engine.clone(), &root, "初版任务", None).expect("派生应成功");
        registry.wait_exit(&first, Duration::from_secs(3)).await.expect("应结束");

        // 子代理存续/结束之后，父侧收窄到空集；resume 必须取 min(原收窄集, 父当前)。
        let mut narrowed = root_scope();
        narrowed.allowed_tools = Some(Arc::new(std::collections::HashSet::new()));
        let resumed = registry
            .resume(engine, &narrowed, &first, Some("继续检查边界"), None)
            .expect("resume 应成功");

        let effective = registry.allowed_tools_of(&resumed).expect("续跑条目应存在");
        assert_eq!(
            effective.map(|set| set.len()),
            Some(0),
            "父已收窄，resume 不得恢复到更宽的工具集"
        );
    }

    /// resume 的 fail-closed：未知 id、未结束、无历史都不得产生半成品条目。
    #[tokio::test]
    async fn resume_rejections_leave_registry_unchanged() {
        let registry = Arc::new(SubagentRegistry::new(DEFAULT_MAX_CONCURRENT));
        let scope = root_scope();
        assert_eq!(
            registry.resume(engine(vec![]), &scope, "no-such-agent", None, None),
            Err(SpawnError::UnknownAgent { agent_id: "no-such-agent".into() })
        );

        let running = registry.spawn(engine(vec![]), &scope, "长任务", None).expect("派生应成功");
        assert_eq!(
            registry.resume(engine(vec![]), &scope, &running, None, None),
            Err(SpawnError::NotExited { agent_id: running.clone() })
        );
        assert_eq!(registry.snapshot().len(), 1, "被拒的 resume 不得登记新条目");
        registry.cancel_all();
        registry.wait_exit(&running, Duration::from_secs(2)).await.expect("应结束");
    }
}
