//! 子代理注册表（SH-051，docs/02 §2.1/§2.3，ADR-101/102）。
//!
//! 职责边界：
//! - **身份与父子边**：给每个子代理分配全局唯一 id，记录 parent/depth；
//! - **状态机**：`spawning → waiting → running → exited`（见 [`AgentStatus`]）；
//! - **取消级联**：子令牌由父令牌 `child_token()` 派生，父取消即级联，不需要逐层通知；
//! - **并发上限**：超出上限的子代理排队（`waiting`），不发起模型请求；
//! - **结果收口**：执行结束把 [`ExitSummary`] 存进注册表，供 `wait_agent` 取用（SH-052）。
//!
//! 不负责：模型调用与工具执行（复用 `AgentLoop::run_instance`）、事件协议包装
//! （SH-053 的 `AgentEvent{agent_id}`）、档案引用（SH-056）、预算共账（SH-055）。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::agent_loop::{AgentInstance, AgentLoop, TokenTotals};
use crate::events::{EventSink, TurnEndReason};
use crate::rollout::RolloutWriter;

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

/// 子代理终态（docs/02 §2.1：done / failed / cancelled）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitStatus {
    Done,
    Failed,
    Cancelled,
}

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
}

impl ExitSummary {
    /// 由一次 `run_instance` 的收口结果构造。
    pub fn from_run(reason: TurnEndReason, last_text: Option<&str>, token_usage: TokenTotals) -> Self {
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
        Self { status, summary, artifacts: Vec::new(), token_usage }
    }

    /// 取消专用的最小摘要（尚未开始运行就被取消）。
    pub fn cancelled(reason: &str) -> Self {
        Self {
            status: ExitStatus::Cancelled,
            summary: reason.to_string(),
            artifacts: Vec::new(),
            token_usage: TokenTotals::default(),
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
    status: AgentStatus,
    created_at: Instant,
    exit: Option<ExitSummary>,
    cancel: CancellationToken,
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
        parent: &AgentInstance,
        task: impl Into<String>,
        sink: Arc<dyn EventSink>,
        rollout: Option<Arc<RolloutWriter>>,
    ) -> Result<String, SpawnError> {
        let task = task.into();
        if task.trim().is_empty() {
            return Err(SpawnError::EmptyTask);
        }
        if !parent.can_spawn() {
            return Err(SpawnError::DepthExceeded { depth: parent.depth });
        }
        if parent.cancel.is_cancelled() {
            return Err(SpawnError::ParentCancelled);
        }

        let agent_id = ulid::Ulid::generate().to_string();
        // 级联的来源就在这一行：父令牌取消时，这个子令牌自动进入取消态。
        let cancel = parent.cancel.child_token();
        let mut instance = AgentInstance::child(&agent_id, &parent.agent_id, &task, cancel.clone());
        instance.rollout = rollout;

        let done = Arc::new(Notify::new());
        {
            let mut inner = self.lock();
            inner.insert(
                agent_id.clone(),
                Entry {
                    parent_id: parent.agent_id.clone(),
                    depth: instance.depth,
                    task: task.clone(),
                    status: AgentStatus::Spawning,
                    created_at: Instant::now(),
                    exit: None,
                    cancel: cancel.clone(),
                    done: done.clone(),
                    handle: None,
                },
            );
        }

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
                    finish(&weak, &task_id, &task_done, ExitSummary::cancelled("已取消（尚未开始运行）。"));
                    return;
                }
                permit = slots.acquire_owned() => match permit {
                    Ok(permit) => permit,
                    Err(_) => {
                        finish(&weak, &task_id, &task_done, ExitSummary::cancelled("子代理调度器已关闭。"));
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
            );
            // 先释放槽位再收口：等结果的一方可能立刻派生下一批子代理。
            drop(permit);
            finish(&weak, &task_id, &task_done, exit);
        });

        let mut inner = self.lock();
        if let Some(entry) = inner.get_mut(&agent_id) {
            entry.handle = Some(handle);
        } else {
            // 理论上不可达（登记在 spawn 前完成）；真出现就放弃这个任务，不留无主句柄。
            handle.abort();
        }
        Ok(agent_id)
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

fn finish(weak: &Weak<SubagentRegistry>, agent_id: &str, done: &Notify, exit: ExitSummary) {
    if let Some(registry) = weak.upgrade() {
        let mut inner = registry.lock();
        if let Some(entry) = inner.get_mut(agent_id) {
            entry.status = AgentStatus::Exited;
            entry.exit = Some(exit.capped());
        }
    }
    done.notify_waiters();
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
        ) -> Result<ModelStream, String> {
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
            policy: policy::SessionPolicy {
                approval: ApprovalMode::Ask,
                sandbox: SandboxMode::WorkspaceWrite,
            },
            rollout: None,
            rollout_broke: std::sync::atomic::AtomicBool::new(false),
            session_id: None,
            jobs: None,
        })
    }

    fn root() -> AgentInstance {
        AgentInstance::root(
            vec![HistoryItem::Message {
                role: crate::model_client::Role::User,
                text: "root-task".into(),
                reasoning: None,
            }],
            0,
            CancellationToken::new(),
        )
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
        let root = root();
        let id = registry
            .spawn(engine, &root, "探索仓库", Arc::new(DiscardSink), None)
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
        let child = AgentInstance::child("child-1", "root", "子任务", CancellationToken::new());
        let error = registry
            .spawn(engine(vec![]), &child, "孙任务", Arc::new(DiscardSink), None)
            .expect_err("深度 1 不得再派生子代理");
        assert_eq!(error, SpawnError::DepthExceeded { depth: 1 });
        assert!(registry.snapshot().is_empty(), "拒绝必须在登记前发生");
    }

    #[tokio::test]
    async fn empty_task_and_cancelled_parent_are_rejected_before_registration() {
        let registry = Arc::new(SubagentRegistry::new(DEFAULT_MAX_CONCURRENT));
        let root = root();
        assert_eq!(
            registry.spawn(engine(vec![]), &root, "   ", Arc::new(DiscardSink), None),
            Err(SpawnError::EmptyTask)
        );

        let cancelled_root = AgentInstance::root(
            vec![HistoryItem::Message {
                role: crate::model_client::Role::User,
                text: "root-task".into(),
                reasoning: None,
            }],
            0,
            CancellationToken::new(),
        );
        cancelled_root.cancel.cancel();
        assert_eq!(
            registry.spawn(engine(vec![]), &cancelled_root, "任务", Arc::new(DiscardSink), None),
            Err(SpawnError::ParentCancelled)
        );
        assert!(registry.snapshot().is_empty());
    }

    /// 并发上限：第二个子代理排队（waiting），第一个结束后才拿到槽位。
    #[tokio::test]
    async fn concurrency_cap_queues_extra_agents_until_a_slot_frees() {
        let registry = Arc::new(SubagentRegistry::new(1));
        let engine = engine(vec![]);
        let root = root();
        let first = registry
            .spawn(engine.clone(), &root, "长任务 A", Arc::new(DiscardSink), None)
            .expect("A 应成功");
        wait_status(&registry, AgentStatus::Running, Duration::from_secs(2)).await;

        let second = registry
            .spawn(engine, &root, "长任务 B", Arc::new(DiscardSink), None)
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
        let root = root();
        let a = registry.spawn(engine.clone(), &root, "A", Arc::new(DiscardSink), None).unwrap();
        let b = registry.spawn(engine, &root, "B", Arc::new(DiscardSink), None).unwrap();
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
        let root = root();
        let id = registry
            .spawn(engine, &root, "永远要工具的子任务", Arc::new(DiscardSink), None)
            .expect("派生应成功");
        let exit = registry.wait_exit(&id, Duration::from_secs(3)).await.expect("应结束");
        assert_eq!(exit.status, ExitStatus::Failed);
        assert!(exit.summary.contains("失败"), "无文本时须给出显式失败说明: {}", exit.summary);
    }
}
