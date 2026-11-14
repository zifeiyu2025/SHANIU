//! 宿主恢复对账（状态源收敛 Phase 3）。
//!
//! 「回合已经结束」与「回合的结局是什么」是两件事：宿主确认本会话没有任务在跑，
//! 只证明了前者；档案里没有 `TurnEnd` 时，结论只能是「结束原因未知」。
//! 这里把那句诚实的结论补进档案一次（`reason = "indeterminate"`），前端据此收口，
//! 不再自己往档案里写结局。
//!
//! 幂等由 [`RolloutWriter::append_once`] 的档案写锁保证：重开重建与 60s 兜底并发到达时，
//! 只有一个能看见「没有结局」，另一个在锁内重读后返回已有结论，档案里只留一条。

use std::path::Path;

use super::{counts_as_user_turn, RolloutError, RolloutItem, RolloutWriter};

/// 对账结果分类（日志与前端只读它决定要不要收口，不参与语义推断）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileStatus {
    /// 本次补写了一条「结束原因未知」的结论。
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

    fn to_item(&self) -> RolloutItem {
        RolloutItem::TurnEnd {
            turn: self.turn,
            task_id: self.task_id.clone(),
            iterations: self.iterations,
            reason: self.reason.clone(),
            at_ms: self.at_ms,
            error: self.error.clone(),
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

/// 对账一次：宿主空闲且档案里最后一个用户回合没有结论时，补一条 `indeterminate`。
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
    let fresh = ConcludedTurn {
        turn,
        task_id: Some(format!("{agent_id}#{turn}")),
        // 对账不知道本次 run 跑了多少次采样；留空就是「未知」，不猜。
        iterations: None,
        reason: "indeterminate".into(),
        at_ms,
        error: None,
    };
    let mut existing: Option<ConcludedTurn> = None;
    let appended = writer.append_once(|items| {
        let last_user = items.iter().rposition(counts_as_user_turn)?;
        if let Some(entry) = items[last_user + 1..]
            .iter()
            .rev()
            .find_map(ConcludedTurn::from_item)
        {
            existing = Some(entry);
            return None;
        }
        Some(fresh.to_item())
    })?;
    match appended {
        Some(_) => Ok(outcome(ReconcileStatus::Appended, Some(fresh))),
        None => Ok(outcome(ReconcileStatus::AlreadyConcluded, existing)),
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
