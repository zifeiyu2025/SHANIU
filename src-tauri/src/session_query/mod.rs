//! SH-113 会话全文搜索：宿主侧派生索引（SQLite FTS5）。
//!
//! 设计契约逐条对齐上游 `session-query-sqlite`（README.zh.md / schema.ts /
//! query.ts / index.ts），实现是 Rust 等价而非 vendored TS：
//!
//! 1. **派生索引，绝不动源存储**：只读 `<data_root>/rollouts/*.jsonl`，只写
//!    `<data_root>/cache/session-search.db`。索引是 disposable 的派生读模型。
//! 2. **live-preferred observation**：每次搜索重新观察磁盘当前态；本次观察到但
//!    base 尚未更新的会话文档并入 connection-local TEMP 表 shadow durable base，
//!    UNION 查询反映当前态（运行中的会话新写入的正文/工具输出即时可搜）。
//! 3. **query-only + hasMore 提示（vendored wire 真实契约）**：DSH
//!    `session.search` 只送 `{query}`，返回 `{items:[{sessionId,snippet}], hasMore}`；
//!    `hasMore` 不是"还有下一页"，而是"命中会话数超过宿主固定上限、请收窄查询"
//!    （`ui-workspace/src/client/contract/slots.ts:280`）。宿主按 sessionId 去重，
//!    每会话只给最强命中的一条 snippet，不引入前端永远传不进来的 cursor。
//! 4. **literal phrase as data**：用户输入 trim + 空白归一后 quoted 成**单个**
//!    FTS5 phrase，引号/`OR`/`NEAR`/`*` 一律当数据，绝不拼成可执行 MATCH 表达式。
//! 5. **first-search 惰性打开**：启动不碰 SQLite，第一次非空搜索才打开。
//!
//! 有意偏差见 docs/05 SH-113 完成注：CJK 单字化预处理、按验收放宽到工具输出片段
//! 可搜、卡内「游标分页」描述按 vendored wire 收敛为 query-only + hasMore=需收窄。

pub mod documents;
pub mod query;
pub mod schema;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rusqlite::{params, Transaction};
use serde::{Deserialize, Serialize};

use crate::config;
use crate::rollout::{self, RolloutItem};

/// 派生索引文件名（落在 `<data_root>/cache/`，可安全清空重建）。
pub const INDEX_FILE: &str = "session-search.db";
/// 宿主固定的结果上限：与 DSH `SESSION_SEARCH_RESULT_LIMIT = 20` 同源。
/// 命中会话数超过它时 `hasMore=true`，语义是「请收窄查询」，不是分页游标。
pub const RESULT_LIMIT: u32 = 20;
/// 最大 snippet 长度（Unicode 码点）：与上游 `snippetChars = 240` 同源。
pub const SNIPPET_CHARS: usize = 240;

/// 类型化搜索错误：前端按 `code` 分派 wire 错误，不靠匹配中文措辞。
/// 形状与 `rollout::RenameError` / `lib::WorkspaceOpError` 一致（04 §1.3 风格）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", content = "message", rename_all = "kebab-case")]
pub enum SearchError {
    /// 查询含 NUL（空/纯空白不是错误，按上游回空表）。
    InvalidQuery(String),
    /// 查询超过 wire 上限（500 UTF-16 code units）。
    QueryTooLong(String),
    /// 索引打不开、schema 校验失败、对账或查询失败。
    IndexFailed(String),
    /// rollout 档案读失败（严格口径，不返回空表冒充"没搜到"）。
    ArchiveReadFailed(String),
}

/// 命中一段已被压缩的原文时，snippet 前置的标注。
///
/// 「能查到」与「还在生效」是两件事：原文确实还在档案里，但模型当前只看到摘要。
/// 只给前半句，用户会以为模型还记得那段，然后基于这个假设做判断。
pub const SHADOWED_SNIPPET_PREFIX: &str = "〔该段已被压缩，模型当前不再看到〕";

/// `session.search` wire 的一条命中（DSH `SessionSearchResultItem`）：只有两键。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub session_id: String,
    pub snippet: String,
}

/// `session.search` wire 的返回页：`hasMore` = 命中会话数超过 `RESULT_LIMIT`，
/// 只驱动 UI 一行「结果较多，请收窄查询」提示，没有续页请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchPage {
    pub items: Vec<SearchHit>,
    pub has_more: bool,
    /// 本次搜索**跳过**的会话档案数（读不回来：被占用、权限、半截文件）。
    ///
    /// 局部失败不该升级成全局失败：坏的是某一个会话的档案，不能罚到所有会话头上。
    /// 但少给了就必须说出来——`skipped_sessions > 0` 表示「这次结果不完整」，
    /// 与「标注缺失好过标注错位」同一条原则：缺失要说出来，不能悄悄少给。
    pub skipped_sessions: u32,
}

/// 索引失败后的冷却时长：这段时间内不再尝试打开索引文件。
///
/// 行为对齐 codex `codex-mcp/src/client_tool_catalog.rs:175-232`：失败后记下
/// `retry_not_before`，冷却期内的再次请求直接返回，不再重新抓取（它取 5 秒，
/// 那里是进程内的内存目录；我们面对的是磁盘/权限故障，取 30 秒）。
const INDEX_FAILURE_COOLDOWN: Duration = Duration::from_secs(30);

/// 一条失败记忆：哪条索引、什么原因、什么时候、当时的文件状态。
struct IndexFailure {
    path: PathBuf,
    message: String,
    at: Instant,
    /// 失败时索引文件的指纹（大小 + 修改时间）。文件被换掉/删掉/重建后指纹变化，
    /// 说明"现场被改过了"，就不该继续用旧失败拦着——用户清磁盘、换文件后
    /// 不必等满冷却或点重试。
    stamp: Option<(u64, Option<std::time::SystemTime>)>,
}

/// 失败记忆表：按索引路径各记一条。进程内通常只有一条（一个 `data_root`），
/// 但测试与多根场景会并行，单槽会被互相覆盖，所以按路径分开存。
static INDEX_FAILURES: Mutex<Vec<IndexFailure>> = Mutex::new(Vec::new());

/// 最多保留几条失败记忆（防止无界增长；正常只有一个 `data_root`）。
const FAILURE_SLOTS: usize = 8;

// 测试专用：打开索引的尝试次数。用于证明冷却期真的没有再去碰文件，
// 而不是"又撞了一次墙但恰好返回了同一条错误"。
// 用线程局部量：测试并行跑，全局计数会把别的用例的尝试次数算进来。
#[cfg(test)]
thread_local! {
    static OPEN_ATTEMPTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn open_attempts() -> usize {
    OPEN_ATTEMPTS.with(std::cell::Cell::get)
}

fn failure_slot() -> std::sync::MutexGuard<'static, Vec<IndexFailure>> {
    // 失败记忆只是 hint，锁中毒不该让搜索整体不可用。
    INDEX_FAILURES.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 索引文件的当前指纹：不存在时为 `None`。
fn index_stamp(index_path: &Path) -> Option<(u64, Option<std::time::SystemTime>)> {
    let meta = fs::metadata(index_path).ok()?;
    Some((meta.len(), meta.modified().ok()))
}

/// 冷却期内：返回上次的同一条错误（同一个 code、同一个原因），不再触碰索引文件。
fn cooldown_message(index_path: &Path) -> Option<String> {
    let guard = failure_slot();
    let failure = guard.iter().find(|failure| failure.path == index_path)?;
    if failure.at.elapsed() >= INDEX_FAILURE_COOLDOWN {
        return None;
    }
    // 现场被改过（文件被删/被换/被重建）就不再拦：那可能正是用户在自救。
    if failure.stamp != index_stamp(index_path) {
        return None;
    }
    Some(format!(
        "{}（索引仍不可用，{} 秒内不再重复尝试，可手动重试）",
        failure.message,
        INDEX_FAILURE_COOLDOWN.as_secs()
    ))
}

fn remember_failure(index_path: &Path, message: &str) {
    let mut guard = failure_slot();
    let failure = IndexFailure {
        path: index_path.to_path_buf(),
        message: message.to_string(),
        at: Instant::now(),
        stamp: index_stamp(index_path),
    };
    match guard.iter_mut().find(|slot| slot.path == index_path) {
        Some(slot) => *slot = failure,
        None => {
            if guard.len() == FAILURE_SLOTS {
                // 淘汰最老的一条：记忆只是 hint，丢掉不影响正确性。
                guard.remove(0);
            }
            guard.push(failure);
        }
    }
}

/// 一次成功搜索只清掉**自己这条路径**的失败记忆，不影响别的索引路径。
fn clear_failure(index_path: &Path) {
    failure_slot().retain(|failure| failure.path != index_path);
}

/// 清掉全部失败记忆：重试入口调用它，下一次搜索重新尝试打开索引。
pub fn retry_index_now() {
    failure_slot().clear();
}

/// 一次搜索的完整入口（被 `lib.rs::search_sessions` 命令壳调用）。
///
/// `data_root` 来自 `AppState::data_root()`（§9.3 目录解析），索引路径复用
/// `config::CACHE_DIR`，不新造一套目录体系。空/纯空白查询按上游语义直接回空表，
/// 连 SQLite 都不打开（首搜惰性更彻底）。
pub fn search(data_root: &Path, raw_query: &str) -> Result<SearchPage, SearchError> {
    if raw_query.trim().is_empty() {
        return Ok(SearchPage { items: Vec::new(), has_more: false, skipped_sessions: 0 });
    }
    let normalized = query::normalize_query(raw_query)?;

    // first-search 惰性打开：启动阶段完全不碰 SQLite。
    let index_path = data_root.join(config::CACHE_DIR).join(INDEX_FILE);
    if let Some(message) = cooldown_message(&index_path) {
        return Err(SearchError::IndexFailed(message));
    }

    let rollouts = data_root.join(rollout::ROLLOUT_DIR);
    let result = search_with_index(&index_path, &rollouts, &normalized.expression);
    match &result {
        Ok(_) => clear_failure(&index_path),
        // 只有索引侧的失败才进冷却；档案侧的局部失败已经在 reconcile 里降级了。
        Err(SearchError::IndexFailed(message)) => remember_failure(&index_path, message),
        Err(_) => {}
    }
    result
}

fn search_with_index(
    index_path: &Path,
    rollouts: &Path,
    expression: &str,
) -> Result<SearchPage, SearchError> {
    #[cfg(test)]
    OPEN_ATTEMPTS.with(|attempts| attempts.set(attempts.get() + 1));
    let mut conn = schema::open_index(index_path)?;

    let tx = conn
        .transaction()
        .map_err(|e| SearchError::IndexFailed(format!("开启索引对账事务失败: {e}")))?;
    let skipped_sessions = reconcile(&tx, rollouts)?;
    // 多取一条判断「是否超过上限」；SQL 内部已按 session_id 分组只留最强命中。
    let rows = run_query(&tx, expression, RESULT_LIMIT + 1)?;
    tx.commit()
        .map_err(|e| SearchError::IndexFailed(format!("提交索引对账失败: {e}")))?;

    let has_more = rows.len() > RESULT_LIMIT as usize;
    let items: Vec<SearchHit> = rows
        .into_iter()
        .take(RESULT_LIMIT as usize)
        .map(|row| {
            let snippet = query::make_snippet(&row.marked_text, SNIPPET_CHARS);
            // wire 只有 {sessionId, snippet} 两键（与上游同形）：标注随 snippet 走，
            // 不新增字段——新字段没有消费方就是"写了没接上"。
            let snippet = if row.surface == "shadowed" {
                format!("{SHADOWED_SNIPPET_PREFIX} {snippet}")
            } else {
                snippet
            };
            SearchHit { session_id: row.session_id, snippet }
        })
        .collect();
    Ok(SearchPage {
        items,
        has_more,
        // 截到 wire 的类型范围：它的唯一用途是「这次结果不完整」的数量提示。
        skipped_sessions: u32::try_from(skipped_sessions).unwrap_or(u32::MAX),
    })
}

/// 一行查询结果的中间形状（只保留生成 snippet 所需的字段）。
struct QueryRow {
    session_id: String,
    marked_text: String,
    /// `current` / `shadowed`：这段原文是否还在模型当前的历史里。
    surface: String,
}

/// 执行 UNION + 排名 + 限量查询。排名按上游口径：
/// 命中 span 多者优先 → 文档短者优先 → 时间 → session id → seq 兜底；不伪造数值分数。
fn run_query(
    tx: &Transaction<'_>,
    expression: &str,
    limit: u32,
) -> Result<Vec<QueryRow>, SearchError> {
    let mut stmt = tx
        .prepare(query::SEARCH_SQL)
        .map_err(|e| SearchError::IndexFailed(format!("准备搜索语句失败: {e}")))?;
    let marker_bytes = query::FTS_HIGHLIGHT_START.len() as i64;
    let rows = stmt
        .query_map(
            params![
                query::FTS_HIGHLIGHT_START,
                query::FTS_HIGHLIGHT_END,
                expression,
                query::FTS_HIGHLIGHT_START,
                query::FTS_HIGHLIGHT_END,
                expression,
                query::FTS_HIGHLIGHT_START,
                marker_bytes,
                i64::from(limit),
            ],
            |row| {
                Ok(QueryRow {
                    session_id: row.get::<_, String>("session_id")?,
                    marked_text: row.get::<_, String>("marked_text")?,
                    surface: row.get::<_, String>("surface")?,
                })
            },
        )
        .map_err(|e| SearchError::IndexFailed(format!("搜索执行失败: {e}")))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| SearchError::IndexFailed(format!("读取搜索结果失败: {e}")))?);
    }
    Ok(out)
}

/// 磁盘档案的 revision：内容哈希 + 字节数。内容变（运行中追加）必然变；
/// mtime 不作为判据（Windows 上的时间戳粒度与跨机器复制都不可靠）。
fn revision_of(bytes: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    format!("{:016x}:{}", hasher.finish(), bytes.len())
}

/// 一次串行对账（上游 `_reconcile` 的 SHANIU 等价）：
/// 列出磁盘快照 → 与 base 的逐会话 revision 比对 → 只读新增/变化档案 → 抽语义文档
/// → 在一个事务里把变化会话写入 TEMP live 表（shadow base）并同步更新 base → bump generation。
///
/// 返回本次**读不回来而跳过**的档案数；`> 0` 表示这次结果不完整，必须交给调用方。
fn reconcile(tx: &Transaction<'_>, rollouts: &Path) -> Result<usize, SearchError> {
    tx.execute("DELETE FROM temp.live_docs", [])
        .and_then(|_| tx.execute("DELETE FROM temp.live_sessions", []))
        .map_err(|e| SearchError::IndexFailed(format!("清空 live 观察表失败: {e}")))?;

    let indexed = read_indexed_revisions(tx)?;
    let observed = observe_archives(rollouts)?;
    let skipped = observed.unreadable.len();
    let unreadable: HashSet<&String> = observed.unreadable.iter().collect();

    let mut changed: Vec<ObservedSession> = Vec::new();
    for session in observed.sessions.values() {
        match indexed.get(&session.id) {
            Some(revision) if revision == &session.revision => {}
            _ => changed.push(session.clone()),
        }
    }
    // 读不回来的档案不等于「已删除」：删掉它已索引的行，等于因为一次读失败
    // 把历史内容从搜索里抹掉（codex `client_tool_catalog.rs:9` 对同类情况的口径是
    // 保留 last good catalog）。保留旧行 + 计入 skipped，宁可给稍旧的命中。
    let deleted: Vec<String> = indexed
        .keys()
        .filter(|id| !observed.sessions.contains_key(*id) && !unreadable.contains(*id))
        .cloned()
        .collect();
    let has_writes = !changed.is_empty() || !deleted.is_empty();
    if !has_writes {
        return Ok(skipped);
    }

    let generation = schema::read_generation(tx)?.parse::<i64>().unwrap_or(0) + 1;
    for id in &deleted {
        tx.execute("DELETE FROM persisted_docs WHERE session_id = ?1", params![id])
            .map_err(|e| SearchError::IndexFailed(format!("删除失效索引行失败: {e}")))?;
        tx.execute("DELETE FROM persisted_sessions WHERE id = ?1", params![id])
            .map_err(|e| SearchError::IndexFailed(format!("删除失效会话登记失败: {e}")))?;
    }
    for session in &changed {
        replace_session(tx, session, generation)?;
    }
    tx.execute(
        "UPDATE search_state SET global_generation = ?1 WHERE singleton = 1",
        params![generation],
    )
    .map_err(|e| SearchError::IndexFailed(format!("更新索引世代失败: {e}")))?;
    Ok(skipped)
}

/// 一个磁盘上观察到的会话（本次搜索的当前态）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ObservedSession {
    id: String,
    revision: String,
    created_at: u64,
    documents: Vec<documents::SearchDoc>,
}

/// 一次磁盘观察的结果：读得回来的会话，以及读不回来的档案 id。
///
/// 「单个档案读不回来」是局部失败，不是全局失败：坏的是某一个会话，不该罚到
/// 所有会话头上（DSH `session-query/src/corpus.ts:30-33,139-176` 把批次结果
/// 定义成逐会话 `fulfilled | rejected`，只有取消才整体中止）。
struct ObservedArchives {
    sessions: BTreeMap<String, ObservedSession>,
    /// 读不回来的档案 id：对账时不能被当成「已删除」，并且要计入 `skipped_sessions`。
    unreadable: Vec<String>,
}

/// 扫描 rollout 目录，对每个 `.jsonl` 算 revision；变化的会话顺带抽出语义文档。
/// 只读：整个函数对 rollouts 目录没有任何写操作（AGENTS §3 + SH-113 验收红线）。
fn observe_archives(rollouts: &Path) -> Result<ObservedArchives, SearchError> {
    let mut sessions = BTreeMap::new();
    let mut unreadable = Vec::new();
    let entries = match fs::read_dir(rollouts) {
        Ok(entries) => entries,
        // 目录不存在 = 首次运行的正常状态（与 rollout::list 同口径），不是错误。
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ObservedArchives { sessions, unreadable })
        }
        Err(e) => {
            return Err(SearchError::ArchiveReadFailed(format!(
                "读取会话档案目录失败 {}: {e}",
                rollouts.display()
            )))
        }
    };
    for entry in entries {
        let entry = entry.map_err(|e| {
            SearchError::ArchiveReadFailed(format!("枚举会话档案失败: {e}"))
        })?;
        let path: PathBuf = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(id) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        // 单个档案读不回来（被占用、权限、写了一半）：跳过它继续索引其余的，
        // 并把它记进 `unreadable`。跳过 ≠ 假装它不存在——调用方必须把数量交出去。
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!(session = %id, path = %path.display(), "会话档案读不回来，本次搜索跳过: {e}");
                unreadable.push(id);
                continue;
            }
        };
        let revision = revision_of(&bytes);
        let archive = match rollout::read(&path) {
            Ok(archive) => archive,
            Err(e) => {
                tracing::warn!(session = %id, path = %path.display(), "会话档案解析失败，本次搜索跳过: {e}");
                unreadable.push(id);
                continue;
            }
        };
        let created_at = match &archive.meta {
            RolloutItem::SessionMeta { created_at, .. } => *created_at,
            _ => 0,
        };
        let documents = documents::build_documents(&archive.items, created_at);
        sessions.insert(
            id.clone(),
            ObservedSession { id, revision, created_at, documents },
        );
    }
    Ok(ObservedArchives { sessions, unreadable })
}

fn read_indexed_revisions(
    tx: &Transaction<'_>,
) -> Result<HashMap<String, String>, SearchError> {
    let mut stmt = tx
        .prepare("SELECT id, revision FROM persisted_sessions")
        .map_err(|e| SearchError::IndexFailed(format!("读取已索引会话失败: {e}")))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| SearchError::IndexFailed(format!("读取已索引会话失败: {e}")))?;
    let mut out = HashMap::new();
    for row in rows {
        let (id, revision) =
            row.map_err(|e| SearchError::IndexFailed(format!("读取已索引会话失败: {e}")))?;
        out.insert(id, revision);
    }
    Ok(out)
}

/// 把一个变化会话同时写进 TEMP live 表（本次观察的当前态，shadow base）与 persisted base
/// （跨重启保留）。SQL 的 `NOT EXISTS` 保证同一会话只从 live 表出结果，不会重复计分。
fn replace_session(
    tx: &Transaction<'_>,
    session: &ObservedSession,
    generation: i64,
) -> Result<(), SearchError> {
    tx.execute("DELETE FROM persisted_docs WHERE session_id = ?1", params![session.id])
        .map_err(|e| SearchError::IndexFailed(format!("清理旧索引行失败: {e}")))?;
    tx.execute(
        "INSERT OR REPLACE INTO persisted_sessions (id, revision, generation) VALUES (?1, ?2, ?3)",
        params![session.id, session.revision, generation],
    )
    .map_err(|e| SearchError::IndexFailed(format!("登记会话索引失败: {e}")))?;
    tx.execute(
        "INSERT OR REPLACE INTO temp.live_sessions (id, fingerprint, persisted, generation) VALUES (?1, ?2, 1, ?3)",
        params![session.id, session.revision, generation],
    )
    .map_err(|e| SearchError::IndexFailed(format!("登记 live 观察失败: {e}")))?;

    for doc in &session.documents {
        let values = params![
            doc.text,
            session.id,
            doc.seq,
            doc.doc_type,
            doc.time,
            doc.surface,
            doc.codepoint_length,
        ];
        tx.execute(
            "INSERT INTO persisted_docs (text, session_id, seq, type, time, surface, codepoint_length) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            values,
        )
        .map_err(|e| SearchError::IndexFailed(format!("写入持久化索引行失败: {e}")))?;
        tx.execute(
            "INSERT INTO temp.live_docs (text, session_id, seq, type, time, surface, codepoint_length) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            values,
        )
        .map_err(|e| SearchError::IndexFailed(format!("写入 live 索引行失败: {e}")))?;
    }
    Ok(())
}
