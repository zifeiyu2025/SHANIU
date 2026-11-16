//! 宿主恢复对账（状态源收敛 Phase 3）。
//!
//! 「回合已经结束」与「回合的结局是什么」是两件事：宿主确认本会话没有任务在跑，
//! 只证明了前者。这里把「结束」这个事实补进档案**一次**，前端据此收口，不再自己往
//! 档案里写结局。
//!
//! 结局只写两种（D12）：
//! - `interrupted`：有中断证据——档案里出现过 `TurnEnd`（写者会为每条终止路径写结局），
//!   或最后一条提问之后有 `AbortNote`（退出流程记下了强制中止）；
//! - `indeterminate`：没有证据（旧档根本没有结局行 / 新档首回合被杀，分不清）——
//!   这是「未知」，不是成功，也不是失败。
//!
//! 幂等由 [`RolloutWriter::append_once`] 的档案写锁保证：重开重建与 60s 兜底并发到达时，
//! 只有一个能看见「没有结局」，另一个在锁内重读后返回已有结论，档案里只留一条。

use std::path::Path;

use super::{
    conclude_turn, counts_as_user_turn, RolloutError, RolloutItem, RolloutWriter, TurnConclusion,
};

/// 补写的结局该写「被中断」还是「未知」——只认证据，不猜。
///
/// 1. 档案里已经出现过 `TurnEnd`：写者会为每条终止路径（完成/取消/失败/预算）写结局，
///    最后一条提问却没有结论，只可能是写它的进程在回合中途退出；
/// 2. 最后一条提问之后有 `AbortNote`：退出流程自己记下了「取消等待超时仍要退出」。
///
/// 两条都不成立时保持 `indeterminate`：旧档（本功能上线前）根本没有结局行，新档首回合
/// 被杀也长这样，两者分不清——分不清就不许替用户下结论（「未知」是一等状态）。
fn interruption_evidence(items: &[RolloutItem]) -> bool {
    if items.iter().any(|item| matches!(item, RolloutItem::TurnEnd { .. })) {
        return true;
    }
    match items.iter().rposition(counts_as_user_turn) {
        Some(last_user) => items[last_user..]
            .iter()
            .any(|item| matches!(item, RolloutItem::AbortNote { .. })),
        None => false,
    }
}

/// 对账结果分类（日志与前端只读它决定要不要收口，不参与语义推断）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileStatus {
    /// 本次补写了一条结论（`interrupted` 或 `indeterminate`，见模块文档）。
    Appended,
    /// 已经有结论（新档或旧档），没有写任何东西。
    AlreadyConcluded,
    /// 宿主说本会话还有任务在跑：不写。
    Running,
    /// 档案不存在（空会话 / 从未落档）：没有回合可对账。
    NoArchive,
    /// 档案存在但没有任何用户提问：没有回合可对账。
    NothingToConclude,
}

impl ReconcileStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Appended => "appended",
            Self::AlreadyConcluded => "already_concluded",
            Self::Running => "running",
            Self::NoArchive => "no_archive",
            Self::NothingToConclude => "nothing_to_conclude",
        }
    }
}

/// 结论行（与 `SessionEntry::TurnEnd` 同形），供前端重建时直接复用，不必再读一遍档案。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConcludedTurn {
    pub turn: u32,
    pub task_id: Option<String>,
    pub iterations: Option<u32>,
    pub reason: String,
    pub at_ms: u64,
    pub error: Option<crate::events::TurnErrorInfo>,
}

impl ConcludedTurn {
    fn from_item(item: &RolloutItem) -> Option<Self> {
        match item {
            RolloutItem::TurnEnd { turn, task_id, iterations, reason, at_ms, error } => Some(Self {
                turn: *turn,
                task_id: task_id.clone(),
                iterations: *iterations,
                reason: reason.clone(),
                at_ms: *at_ms,
                error: error.clone(),
            }),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileOutcome {
    pub status: ReconcileStatus,
    /// `Appended` 是刚写的结论；`AlreadyConcluded` 是档案里已有的最后一条结论。
    pub entry: Option<ConcludedTurn>,
}

fn outcome(status: ReconcileStatus, entry: Option<ConcludedTurn>) -> ReconcileOutcome {
    ReconcileOutcome { status, entry }
}

/// 对账一次：宿主空闲且档案里最后一个用户回合没有结论时，补一条 `interrupted`
/// （有中断证据）或 `indeterminate`（无证据）。
///
/// `running` 由调用方从宿主的活动任务槽取（唯一运行态口径），这里不自行推断。
pub fn reconcile_once(
    path: &Path,
    running: bool,
    at_ms: u64,
) -> Result<ReconcileOutcome, RolloutError> {
    if running {
        return Ok(outcome(ReconcileStatus::Running, None));
    }
    if !path.exists() {
        return Ok(outcome(ReconcileStatus::NoArchive, None));
    }
    // 计数与后面 `append_once` 里的重读都基于同一份档案；两者之间即使有别的写入，
    // 也只影响 `turn` 是否落后一行，不会写出重复结论（判重在锁内重读）。
    let writer = RolloutWriter::open_existing(path)?;
    let turn = writer.user_turns();
    if turn == 0 {
        return Ok(outcome(ReconcileStatus::NothingToConclude, None));
    }
    let agent_id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("session")
        .to_string();
    let mut conclusion: Option<TurnConclusion> = None;
    let appended = writer.append_once(|items| {
        // 判定与编号与 agent_loop 收口共用同一份口径（`conclude_turn`）：
        // 最后一条提问已经有结局时返回 None，档案里只留一条结论。
        // D12：有中断证据就写 `interrupted`，没有证据才退回 `indeterminate`。
        let reason = if interruption_evidence(items) { "interrupted" } else { "indeterminate" };
        let mut decided = conclude_turn(items, &agent_id, reason, None, None, at_ms);
        let item = decided.item.take();
        conclusion = Some(decided);
        item
    })?;
    let conclusion = conclusion.take().expect("append_once 一定调用一次判定");
    match appended {
        Some(item) => Ok(outcome(
            ReconcileStatus::Appended,
            ConcludedTurn::from_item(&item),
        )),
        None => Ok(outcome(
            ReconcileStatus::AlreadyConcluded,
            conclusion.existing.as_ref().and_then(ConcludedTurn::from_item),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_client::{HistoryItem, Role};
    use crate::rollout::read;
    use std::path::PathBuf;
    use std::sync::{Arc, Barrier};

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_dir(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "shaniu-reconcile-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn meta(agent: &str) -> RolloutItem {
        RolloutItem::SessionMeta {
            session_id: "session".into(),
            agent_id: agent.into(),
            parent_id: None,
            created_at: 0,
            config_snapshot: super::super::ConfigSnapshot::default(),
        }
    }

    fn user(text: &str) -> RolloutItem {
        RolloutItem::ResponseItem {
            item: HistoryItem::Message { role: Role::User, text: text.into(), reasoning: None },
            at_ms: 1,
        }
    }

    fn assistant(text: &str) -> RolloutItem {
        RolloutItem::ResponseItem {
            item: HistoryItem::Message { role: Role::Assistant, text: text.into(), reasoning: None },
            at_ms: 2,
        }
    }

    fn seed(dir: &TempDir, agent: &str, with_ending: bool) -> PathBuf {
        let writer = RolloutWriter::create(&dir.0, &meta(agent)).unwrap();
        writer.append(&user("跑一下")).unwrap();
        writer.append(&assistant("跑到一半")).unwrap();
        if with_ending {
            writer
                .append(&RolloutItem::TurnEnd {
                    turn: 1,
                    task_id: Some(format!("{agent}#1")),
                    iterations: Some(1),
                    reason: "cancelled".into(),
                    at_ms: 3,
                    error: None,
                })
                .unwrap();
        }
        writer.path().to_path_buf()
    }

    fn turn_ends(path: &Path) -> Vec<ConcludedTurn> {
        read(path)
            .unwrap()
            .items
            .iter()
            .filter_map(ConcludedTurn::from_item)
            .collect()
    }

    #[test]
    fn reconcile_appends_indeterminate_exactly_once() {
        let dir = temp_dir("once");
        let path = seed(&dir, "agent", false);

        let first = reconcile_once(&path, false, 42).unwrap();
        assert_eq!(first.status, ReconcileStatus::Appended);
        let entry = first.entry.expect("补写必须带回结论行");
        assert_eq!(entry.turn, 1);
        assert_eq!(entry.task_id.as_deref(), Some("agent#1"));
        assert_eq!(entry.iterations, None, "对账不知道采样次数，必须留未知");
        assert_eq!(entry.reason, "indeterminate");
        assert_eq!(entry.at_ms, 42);

        // 第二次：读侧已经看得见结论，不再追加。
        let second = reconcile_once(&path, false, 99).unwrap();
        assert_eq!(second.status, ReconcileStatus::AlreadyConcluded);
        assert_eq!(second.entry.expect("已有结论也要带回来").reason, "indeterminate");
        let ends = turn_ends(&path);
        assert_eq!(ends.len(), 1, "幂等：档案里只能有一条结论，实际 {ends:?}");
        assert_eq!(ends[0].at_ms, 42, "已有结论不能被第二次调用的时刻覆盖");
    }

    /// D12：崩溃中断必须有自己的结局，不能一律 `indeterminate`。
    ///
    /// 证据：档案里已经出现过 `TurnEnd`——写者会为每条终止路径（完成/取消/失败/预算）
    /// 写结局；最后一条提问却没有结局，只可能是写它的进程在回合中途退出。
    /// 步数仍然未知，所以 `iterations` 必须是 `None`，不能拿「助手消息条数」去编。
    ///
    /// 反向对照：把补写理由改回固定 `indeterminate`，本用例必红。
    #[test]
    fn reconcile_marks_crash_orphan_as_interrupted_when_writer_is_known() {
        let dir = temp_dir("interrupted");
        let writer = RolloutWriter::create(&dir.0, &meta("agent")).unwrap();
        writer.append(&user("第一问")).unwrap();
        writer.append(&assistant("答完")).unwrap();
        writer
            .append(&RolloutItem::TurnEnd {
                turn: 1,
                task_id: Some("agent#1".into()),
                iterations: Some(1),
                reason: "completed".into(),
                at_ms: 3,
                error: None,
            })
            .unwrap();
        writer.append(&user("第二问")).unwrap();
        writer.append(&assistant("写一半")).unwrap();
        let path = writer.path().to_path_buf();
        drop(writer);

        let result = reconcile_once(&path, false, 99).unwrap();
        assert_eq!(result.status, ReconcileStatus::Appended);
        let entry = result.entry.expect("补写必须带回结论行");
        assert_eq!(entry.turn, 2, "回合号是用户回合，不是采样步数");
        assert_eq!(entry.reason, "interrupted", "有证据时不许退回 indeterminate");
        assert_eq!(entry.iterations, None, "中断时不知道跑了几步，不许编");
        assert_eq!(turn_ends(&path).len(), 2, "前一条结论不能被覆盖");
    }

    /// 强制中止留痕本身就是证据：退出流程已经写下「取消等待超时仍要退出」。
    /// 这一条覆盖「旧档没有 TurnEnd」也能判 interrupted 的情形。
    #[test]
    fn abort_note_is_evidence_of_interruption() {
        let dir = temp_dir("abort-note");
        let path = seed(&dir, "agent-abort", false);
        let writer = RolloutWriter::open_existing(&path).unwrap();
        writer
            .append(&RolloutItem::AbortNote {
                reason: "因退出而强制中止：取消等待超时".into(),
                at_ms: 9,
            })
            .unwrap();
        drop(writer);

        let result = reconcile_once(&path, false, 99).unwrap();
        assert_eq!(result.status, ReconcileStatus::Appended);
        assert_eq!(result.entry.expect("补写必须带回结论行").reason, "interrupted");
    }

    #[test]
    fn reconcile_skips_running_absent_and_empty() {
        let dir = temp_dir("skip");
        let path = seed(&dir, "agent", false);
        let before = std::fs::read_to_string(&path).unwrap();

        // 宿主说还在跑：一行都不写。
        let running = reconcile_once(&path, true, 1).unwrap();
        assert_eq!(running.status, ReconcileStatus::Running);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        // 档案不存在：没有回合可对账。
        let missing = reconcile_once(&dir.0.join("nobody.jsonl"), false, 1).unwrap();
        assert_eq!(missing.status, ReconcileStatus::NoArchive);

        // 有档案但没有用户提问：没有回合可对账，也不写「空回合」的结论。
        let empty = RolloutWriter::create(&dir.0, &meta("agent-empty")).unwrap();
        let empty_path = empty.path().to_path_buf();
        drop(empty);
        let nothing = reconcile_once(&empty_path, false, 1).unwrap();
        assert_eq!(nothing.status, ReconcileStatus::NothingToConclude);
        assert!(turn_ends(&empty_path).is_empty());
    }

    #[test]
    fn legacy_ending_counts_as_concluded_without_new_line() {
        let dir = temp_dir("legacy");
        let path = seed(&dir, "agent-legacy", false);
        // 旧档结局行：没有 task_id/iterations。
        let legacy = r#"{"type":"turn_end","turn":3,"reason":"completed","at_ms":7}"#;
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        {
            use std::io::Write;
            file.write_all(legacy.as_bytes()).unwrap();
            file.write_all(b"\n").unwrap();
        }
        drop(file);

        let result = reconcile_once(&path, false, 99).unwrap();
        assert_eq!(result.status, ReconcileStatus::AlreadyConcluded);
        let entry = result.entry.expect("旧档结论也要带回");
        assert_eq!(entry.turn, 3, "旧档字段原样透出，由读侧按 legacy 解释");
        assert_eq!(entry.task_id, None);
        assert_eq!(turn_ends(&path).len(), 1, "旧档已有结论，不得再补一条");
    }

    #[test]
    fn concurrent_reconcile_appends_exactly_once() {
        let dir = temp_dir("concurrent");
        let path = seed(&dir, "agent", false);
        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let path = path.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                reconcile_once(&path, false, 5).unwrap().status
            }));
        }
        let statuses: Vec<ReconcileStatus> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        let appended = statuses.iter().filter(|s| **s == ReconcileStatus::Appended).count();
        assert_eq!(appended, 1, "并发对账只能有一次生效，实际 {statuses:?}");
        assert_eq!(turn_ends(&path).len(), 1);
    }
}
