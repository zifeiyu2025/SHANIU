//! 可续播的事件信封日志（SH-094 批 1）。
//!
//! 广播出口的 `AgentEvent` 逐条追加到 `<data_root>/sessions/<session_id>.events.jsonl`，
//! 供 gateway 以 `after_event_id` 续播。**只追加、不改写**：事件信封一旦编号就不可变；
//! 读取按 event_id 单调序（ULID 同宽字典序），容忍崩溃留下的半截尾行（与 rollout 同口径），
//! 中途坏行跳过——续播是尽力而为的恢复面，不替协议层掩盖事件缺失。
//!
//! 已知限制（批 1 明示）：没有落盘保留策略/压缩，长会话会持续增长（批 2 的 gateway
//! 需要时再加轮转）；本模块不负责广播传输，只负责持久化与读取。
//!
//! 写路径（SH-127 M1）：同一 `FileEventJournal` 内每会话复用一条常驻写句柄（上限 8 条 LRU），
//! 不再每事件 open/close；被淘汰的句柄下次以 **append** 模式重开，绝不截断。
//! 落盘仍在编号锁内——文件行序必须等于 event_id 序，挪出锁会让 `read_after` 读错（见 M1b）。
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

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

/// 信封序列化成一行 NDJSON（含行尾换行）。
fn encode(event: &AgentEvent) -> Result<String, String> {
    let mut line = serde_json::to_string(event).map_err(|e| format!("事件信封序列化失败: {e}"))?;
    line.push('\n');
    Ok(line)
}

/// 写一行并 flush：`flush` 保留原语义（先落盘再广播，客户端续播时日志里一定已有它）。
fn write_line(file: &mut File, line: &str, path: &Path) -> Result<(), String> {
    file.write_all(line.as_bytes())
        .and_then(|_| file.flush())
        .map_err(|e| format!("事件日志写入失败（{}）: {e}", path.display()))
}

/// 一次性追加：每调用 open/close 一次。生产路径走 `FileEventJournal`（复用句柄），
/// 这里保留给「无状态一次写入」的调用点与测试基座。
pub fn append(dir: &Path, event: &AgentEvent) -> Result<(), String> {
    let path = log_path(dir, &event.session_id)?;
    let line = encode(event)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("事件日志打开失败（{}）: {e}", path.display()))?;
    write_line(&mut file, &line, &path)
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

/// 常驻写句柄上限。事件按会话分文件；一个任务里通常是「父会话 + 若干子代理」共用一份日志，
/// 8 条足够覆盖相邻打开的会话，超了按最久未用淘汰（淘汰 = 关闭句柄，不影响文件内容）。
const HANDLE_CACHE_LIMIT: usize = 8;

struct CachedFile {
    file: File,
    last_used: u64,
}

#[derive(Default)]
struct HandleCache {
    entries: HashMap<String, CachedFile>,
    tick: u64,
}

impl HandleCache {
    /// 命中则刷新最近使用时间。未命中返回 false，调用方负责打开并 `insert`。
    fn hit(&mut self, session_id: &str) -> bool {
        self.tick += 1;
        match self.entries.get_mut(session_id) {
            Some(cached) => {
                cached.last_used = self.tick;
                true
            }
            None => false,
        }
    }

    /// 放入新句柄。满了先淘汰最久未用的那条（drop = 关闭句柄）。
    /// **下次 `append` 走 `OpenOptions::append(true)` 重开**，不截断既有内容。
    fn insert(&mut self, session_id: &str, file: File) {
        if self.entries.len() >= HANDLE_CACHE_LIMIT {
            if let Some(victim) = self
                .entries
                .iter()
                .min_by_key(|(_, cached)| cached.last_used)
                .map(|(id, _)| id.clone())
            {
                self.entries.remove(&victim);
            }
        }
        self.tick += 1;
        self.entries
            .insert(session_id.to_owned(), CachedFile { file, last_used: self.tick });
    }

    fn write(&mut self, session_id: &str, line: &str, path: &Path) -> Result<(), String> {
        let cached = self
            .entries
            .get_mut(session_id)
            .ok_or_else(|| "事件日志句柄缓存未命中".to_string())?;
        write_line(&mut cached.file, line, path)
    }

    fn discard(&mut self, session_id: &str) {
        self.entries.remove(session_id);
    }
}

/// 落盘到固定目录的事件日志实现：组合根把它注入 `TauriEventSink`（依赖倒置，
/// `events` 不反向依赖本模块）。调用方在编号锁内串行调用，故这里的锁不会争用；
/// 它存在只是为了满足 `EventJournal: Send + Sync`。
///
/// M1（SH-127）：每会话复用一条常驻写句柄，省掉每事件的 open/close 与路径解析；
/// 写入仍是「write + flush」，行序仍由编号锁保证。
pub struct FileEventJournal {
    dir: PathBuf,
    handles: Mutex<HandleCache>,
    /// 打开次数。验收指标：N 条事件应只有 1 次打开，而不是 N 次。
    opens: AtomicU64,
}

impl FileEventJournal {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            handles: Mutex::new(HandleCache::default()),
            opens: AtomicU64::new(0),
        }
    }

    /// 累计打开文件次数（只用于验收与基准，不影响行为）。
    pub fn opens(&self) -> u64 {
        self.opens.load(Ordering::Relaxed)
    }
}

impl crate::events::EventJournal for FileEventJournal {
    fn append(&self, event: &AgentEvent) -> Result<(), String> {
        let path = log_path(&self.dir, &event.session_id)?;
        let line = encode(event)?;
        let mut handles = self.handles.lock().map_err(|e| format!("事件日志句柄表不可用: {e}"))?;
        if !handles.hit(&event.session_id) {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| format!("事件日志打开失败（{}）: {e}", path.display()))?;
            self.opens.fetch_add(1, Ordering::Relaxed);
            handles.insert(&event.session_id, file);
        }
        let result = handles.write(&event.session_id, &line, &path);
        if result.is_err() {
            // 句柄可能已经写不进去（文件被删/被轮转/磁盘满）：丢掉它，
            // 下一条事件重开一次即可，不要攥着一个坏句柄把后面的事件全丢光。
            handles.discard(&event.session_id);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{EventJournal, EventMsg};

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
    fn reuses_one_handle_for_many_appends() {
        let dir = temp_dir();
        let journal = FileEventJournal::new(dir.path().to_path_buf());
        for index in 0..200u32 {
            journal.append(&event("s1", &format!("{index:03}"), index)).unwrap();
        }
        // M1 验收：200 条事件只应打开文件 1 次（改前是 200 次）。
        assert_eq!(journal.opens(), 1, "同一会话的 200 条事件应复用同一条写句柄");
        let page = read_after(&dir, "s1", None, 500).unwrap();
        assert_eq!(page.events.len(), 200);
        let ids: Vec<&str> = page.events.iter().map(|e| e.event_id.as_str()).collect();
        let expected: Vec<String> = (0..200u32).map(|i| format!("{i:03}")).collect();
        assert_eq!(ids, expected.iter().map(String::as_str).collect::<Vec<_>>());
    }

    #[test]
    fn evicted_handle_reopens_in_append_mode_without_truncating() {
        let dir = temp_dir();
        let journal = FileEventJournal::new(dir.path().to_path_buf());
        // 写满句柄表再超一条：容量 8，第 9 个会话会把最久未用的 s1 挤掉。
        for index in 1..=9 {
            let session = format!("s{index}");
            journal.append(&event(&session, "01", 1)).unwrap();
        }
        assert_eq!(journal.opens(), 9);
        // s1 的句柄已被淘汰；再写一条必须重开并**追加**，不能截断已有内容。
        journal.append(&event("s1", "02", 2)).unwrap();
        assert_eq!(journal.opens(), 10, "淘汰后应重新打开一次，而不是永久丢句柄");

        let page = read_after(&dir, "s1", None, 10).unwrap();
        let ids: Vec<&str> = page.events.iter().map(|e| e.event_id.as_str()).collect();
        assert_eq!(ids, vec!["01", "02"], "淘汰再写回后必须仍按序完整");
        let raw = std::fs::read_to_string(dir.path().join("s1.events.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), 2, "重开必须是 append，不能把旧行覆盖掉");
        assert!(raw.contains("\"event_id\":\"01\""), "首条事件的内容必须还在");
    }

    /// 基准（不参与门禁）：`cargo test --lib bench_append -- --ignored --nocapture`
    /// 同一临时目录、同一组 N=1000 条固定信封、debug 构建，各跑 5 轮取中位数。
    #[test]
    #[ignore = "基准用例，手动运行：cargo test --lib bench_append -- --ignored --nocapture"]
    fn bench_append_open_each_vs_reused_handle() {
        use std::time::{Duration, Instant};
        const N: u32 = 1000;
        const ROUNDS: usize = 5;
        let events: Vec<AgentEvent> =
            (0..N).map(|index| event("bench", &format!("{index:06}"), index)).collect();
        let median = |mut values: Vec<Duration>| -> Duration {
            values.sort();
            values[values.len() / 2]
        };
        let mut each = Vec::new();
        let mut reused = Vec::new();
        let mut opens = 0;
        for _ in 0..ROUNDS {
            let dir = temp_dir();
            let start = Instant::now();
            for e in &events {
                append(&dir, e).unwrap();
            }
            each.push(start.elapsed());

            let dir = temp_dir();
            let journal = FileEventJournal::new(dir.path().to_path_buf());
            let start = Instant::now();
            for e in &events {
                journal.append(e).unwrap();
            }
            reused.push(start.elapsed());
            opens = journal.opens();
        }
        let each_ms = median(each).as_secs_f64() * 1000.0;
        let reused_ms = median(reused).as_secs_f64() * 1000.0;
        println!(
            "[bench] N={N} 每事件 open/close: {each_ms:.1} ms（open {N} 次） | 复用句柄: {reused_ms:.1} ms（open {opens} 次） | 提速 {:.2}x",
            each_ms / reused_ms
        );
        assert_eq!(opens, 1);
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
