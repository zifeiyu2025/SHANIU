//! 可续播的事件信封日志（SH-094 批 1）。
//!
//! 广播出口的 `AgentEvent` 逐条追加到 `<data_root>/sessions/<session_id>.events.jsonl`，
//! 供 gateway 以 `after_event_id` 续播。**只追加、不改写**：事件信封一旦编号就不可变；
//! 读取按 event_id 单调序（ULID 同宽字典序），容忍崩溃留下的半截尾行（与 rollout 同口径），
//! 中途坏行跳过——续播是尽力而为的恢复面，不替协议层掩盖事件缺失。
//!
//! 已知限制（批 1 明示）：没有落盘保留策略/压缩，长会话会持续增长（批 2 的 gateway
//! 需要时再加轮转）；本模块不负责广播传输，只负责持久化与读取。
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::events::AgentEvent;

/// 事件日志文件后缀：`<session_id>.events.jsonl`，落在 `sessions/` 下。
pub const EVENT_LOG_SUFFIX: &str = ".events.jsonl";
/// 单次读取的默认条数。
pub const DEFAULT_LIMIT: usize = 500;
/// 单次读取的硬上限：防止一次续播把整段长会话读进内存。
pub const MAX_LIMIT: usize = 5_000;

/// 一页事件与「后面还有」标记；`has_more` 为真时客户端应带最后一条 `event_id` 再取。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventPage {
    pub events: Vec<AgentEvent>,
    pub has_more: bool,
}

fn log_path(dir: &Path, session_id: &str) -> Result<PathBuf, String> {
    if session_id.is_empty()
        || !session_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!("非法的会话 id：{session_id}"));
    }
    Ok(dir.join(format!("{session_id}{EVENT_LOG_SUFFIX}")))
}

/// 追加一条信封。调用方在广播出口内按 event_id 顺序调用（编号锁内），因此文件行序即编号序。
pub fn append(dir: &Path, event: &AgentEvent) -> Result<(), String> {
    let path = log_path(dir, &event.session_id)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("事件日志打开失败（{}）: {e}", path.display()))?;
    let line = serde_json::to_string(event).map_err(|e| format!("事件信封序列化失败: {e}"))?;
    file.write_all(line.as_bytes())
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.flush())
        .map_err(|e| format!("事件日志写入失败（{}）: {e}", path.display()))
}

/// 读取日志最后一条合法事件 id（只用于 live-only 订阅的游标种子，不重放大历史）。
/// 不存在的日志 = 没有历史；坏行/torn 尾行跳过，与 `read_after` 同口径。
pub fn latest_event_id(dir: &Path, session_id: &str) -> Result<Option<String>, String> {
    let path = log_path(dir, session_id)?;
    if !path.exists() {
        return Ok(None);
    }
    let file = std::fs::File::open(&path)
        .map_err(|e| format!("事件日志打开失败（{}）: {e}", path.display()))?;
    let mut last = None;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<AgentEvent>(line) {
            last = Some(event.event_id);
        }
    }
    Ok(last)
}

/// 读取 `after_event_id` 之后的一页事件。`None` = 从头读；不存在的日志如实空页。
pub fn read_after(
    dir: &Path,
    session_id: &str,
    after_event_id: Option<&str>,
    limit: usize,
) -> Result<EventPage, String> {
    let path = log_path(dir, session_id)?;
    if !path.exists() {
        return Ok(EventPage { events: Vec::new(), has_more: false });
    }
    let file = std::fs::File::open(&path)
        .map_err(|e| format!("事件日志打开失败（{}）: {e}", path.display()))?;
    let limit = limit.clamp(1, MAX_LIMIT);
    let mut events = Vec::new();
    let mut has_more = false;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // 半截尾行/中途坏行跳过：日志是恢复面，不因一条坏行拒绝整页续播。
        let Ok(event) = serde_json::from_str::<AgentEvent>(line) else { continue };
        if let Some(after) = after_event_id {
            if event.event_id.as_str() <= after {
                continue;
            }
        }
        if events.len() == limit {
            has_more = true;
            break;
        }
        events.push(event);
    }
    Ok(EventPage { events, has_more })
}

/// 反查某个 agent 的事件归属会话（SH-056 批 2）。
///
/// 子代理**没有独立档案**：它的全部事件记在父会话的 `<parent>.events.jsonl` 里，
/// 信封的 `session_id` 是父任务会话、`agent_id` 才是子代理 ULID。重启后前端直接
/// 打开泳道时只有一个 id（`laneIdOf(agent_id)`，即 ULID 本身），靠这里找回父会话
/// 再整树回放——此前这条路径必然读不回，主面板只能空着。
///
/// 口径：
/// - **不存在 = 没有该 agent**（返回 `Ok(None)`），不是错误：本进程刚 spawn 的
///   agent 还没落盘、或 id 拼错，都走这一支由调用方决定降级；
/// - 坏行跳过，与 `read_after` 同口径——反查是恢复面，不因一条坏行判整个会话不可开；
/// - 多个档案同时命中取**文件名序第一条**：`agent_id` 全局唯一，重复只可能来自档案
///   被复制，取先写的那份比随机返回稳定。
pub fn owner_session_of_agent(dir: &Path, agent_id: &str) -> Result<Option<String>, String> {
    if agent_id.is_empty() || !dir.exists() {
        return Ok(None);
    }
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("会话目录读取失败（{}）: {e}", dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(EVENT_LOG_SUFFIX))
        })
        .collect();
    paths.sort();
    for path in paths {
        let Ok(file) = std::fs::File::open(&path) else { continue };
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else { break };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(event) = serde_json::from_str::<AgentEvent>(line) else { continue };
            if event.agent_id == agent_id {
                return Ok(Some(event.session_id));
            }
        }
    }
    Ok(None)
}

/// 落盘到固定目录的事件日志实现：组合根把它注入 `TauriEventSink`（依赖倒置，
/// `events` 不反向依赖本模块）。线程安全由 `append` 自身的打开-追加-Flush 保证，
/// 调用方在编号锁内串行调用，故无需额外锁。
pub struct FileEventJournal {
    dir: PathBuf,
}

impl FileEventJournal {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
}

impl crate::events::EventJournal for FileEventJournal {
    fn append(&self, event: &AgentEvent) -> Result<(), String> {
        append(&self.dir, event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventMsg;

    fn event(session: &str, id: &str, turn: u32) -> AgentEvent {
        AgentEvent {
            event_id: id.into(),
            session_id: session.into(),
            agent_id: "root".into(),
            parent_id: None,
            ts_ms: 1,
            msg: EventMsg::TurnStarted { turn },
        }
    }

    fn temp_dir() -> crate::test_support::TestTempDir {
        crate::test_support::TestTempDir::new("events")
    }

    #[test]
    fn appends_in_order_and_pages_after_a_cursor() {
        let dir = temp_dir();
        for (id, turn) in [("01", 1), ("02", 2), ("03", 3)] {
            append(&dir, &event("s1", id, turn)).unwrap();
        }
        let all = read_after(&dir, "s1", None, 10).unwrap();
        let ids: Vec<&str> = all.events.iter().map(|e| e.event_id.as_str()).collect();
        assert_eq!(ids, vec!["01", "02", "03"]);
        assert!(!all.has_more);
        let after = read_after(&dir, "s1", Some("01"), 10).unwrap();
        assert_eq!(after.events.len(), 2);
        assert_eq!(after.events[0].event_id, "02");
        let page = read_after(&dir, "s1", None, 2).unwrap();
        assert_eq!(page.events.len(), 2);
        assert!(page.has_more);
    }

    #[test]
    fn sessions_are_isolated_and_unknown_ids_are_rejected() {
        let dir = temp_dir();
        append(&dir, &event("s1", "01", 1)).unwrap();
        append(&dir, &event("s2", "02", 1)).unwrap();
        assert_eq!(read_after(&dir, "s1", None, 10).unwrap().events.len(), 1);
        assert!(read_after(&dir, "s2", None, 10).unwrap().events[0].event_id == "02");
        assert!(read_after(&dir, "missing", None, 10).unwrap().events.is_empty());
        assert!(read_after(&dir, "../escape", None, 10).is_err());
    }

    #[test]
    fn latest_event_id_reads_tail_and_tolerates_corrupt_lines() {
        let dir = temp_dir();
        assert_eq!(latest_event_id(&dir, "s1").unwrap(), None);
        append(&dir, &event("s1", "01", 1)).unwrap();
        append(&dir, &event("s1", "02", 2)).unwrap();
        assert_eq!(latest_event_id(&dir, "s1").unwrap().as_deref(), Some("02"));
        // torn 尾行不改变最后一条合法事件
        let path = dir.join("s1.events.jsonl");
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"event_id\":\"03\"").unwrap();
        file.write_all(b"\n").unwrap();
        assert_eq!(latest_event_id(&dir, "s1").unwrap().as_deref(), Some("02"));
        assert!(latest_event_id(&dir, "../escape").is_err());
    }

    #[test]
    fn torn_tail_and_corrupt_lines_do_not_block_the_page() {
        let dir = temp_dir();
        append(&dir, &event("s1", "01", 1)).unwrap();
        let path = dir.join("s1.events.jsonl");
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"event_id\":\"02\",\"session_id\":").unwrap();
        file.write_all(b"\n").unwrap();
        append(&dir, &event("s1", "03", 3)).unwrap();
        let page = read_after(&dir, "s1", None, 10).unwrap();
        let ids: Vec<&str> = page.events.iter().map(|e| e.event_id.as_str()).collect();
        assert_eq!(ids, vec!["01", "03"]);
    }

    #[test]
    fn owner_session_of_agent_finds_parent_across_sessions() {
        let dir = temp_dir();
        // 两个父会话各自带子代理事件，反查按 agent_id 精确命中、跨文件也能找到。
        append(&dir, &event("s1", "01", 1)).unwrap();
        append(&dir, &AgentEvent {
            event_id: "02".into(),
            session_id: "s1".into(),
            agent_id: "child-a".into(),
            parent_id: Some("root".into()),
            ts_ms: 2,
            msg: EventMsg::TurnStarted { turn: 1 },
        })
        .unwrap();
        append(&dir, &AgentEvent {
            event_id: "03".into(),
            session_id: "s2".into(),
            agent_id: "child-b".into(),
            parent_id: Some("root".into()),
            ts_ms: 3,
            msg: EventMsg::TurnStarted { turn: 1 },
        })
        .unwrap();

        assert_eq!(owner_session_of_agent(&dir, "child-a").unwrap().as_deref(), Some("s1"));
        assert_eq!(owner_session_of_agent(&dir, "child-b").unwrap().as_deref(), Some("s2"));
        // 未知 agent / 空 id / 不存在的目录如实返回 None；空 id 不做全量扫描。
        assert_eq!(owner_session_of_agent(&dir, "child-unknown").unwrap(), None);
        assert_eq!(owner_session_of_agent(&dir, "").unwrap(), None);
        assert_eq!(owner_session_of_agent(&dir.join("missing"), "child-a").unwrap(), None);
    }

    #[test]
    fn owner_session_of_agent_skips_corrupt_lines() {
        let dir = temp_dir();
        let path = dir.join("s1.events.jsonl");
        let mut file = OpenOptions::new().create(true).append(true).open(&path).unwrap();
        file.write_all(b"{\"event_id\":\"01\",\"session_id\":\n").unwrap();
        append(&dir, &AgentEvent {
            event_id: "02".into(),
            session_id: "s1".into(),
            agent_id: "child-a".into(),
            parent_id: Some("root".into()),
            ts_ms: 2,
            msg: EventMsg::TurnStarted { turn: 1 },
        })
        .unwrap();
        // 坏行在前不阻断：反查仍能命中后面的合法事件。
        assert_eq!(owner_session_of_agent(&dir, "child-a").unwrap().as_deref(), Some("s1"));
    }
}
