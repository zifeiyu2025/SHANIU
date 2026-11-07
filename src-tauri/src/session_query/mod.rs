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

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};

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
}

/// 一次搜索的完整入口（被 `lib.rs::search_sessions` 命令壳调用）。
///
/// `data_root` 来自 `AppState::data_root()`（§9.3 目录解析），索引路径复用
/// `config::CACHE_DIR`，不新造一套目录体系。空/纯空白查询按上游语义直接回空表，
/// 连 SQLite 都不打开（首搜惰性更彻底）。
pub fn search(data_root: &Path, raw_query: &str) -> Result<SearchPage, SearchError> {
    if raw_query.trim().is_empty() {
        return Ok(SearchPage { items: Vec::new(), has_more: false });
    }
    let normalized = query::normalize_query(raw_query)?;

    // first-search 惰性打开：启动阶段完全不碰 SQLite。
    let index_path = data_root.join(config::CACHE_DIR).join(INDEX_FILE);
    let mut conn = schema::open_index(&index_path)?;

    let rollouts = data_root.join(rollout::ROLLOUT_DIR);
    let tx = conn
        .transaction()
        .map_err(|e| SearchError::IndexFailed(format!("开启索引对账事务失败: {e}")))?;
    reconcile(&tx, &rollouts)?;
    // 多取一条判断「是否超过上限」；SQL 内部已按 session_id 分组只留最强命中。
    let rows = run_query(&tx, &normalized.expression, RESULT_LIMIT + 1)?;
    tx.commit()
        .map_err(|e| SearchError::IndexFailed(format!("提交索引对账失败: {e}")))?;

    let has_more = rows.len() > RESULT_LIMIT as usize;
    let items: Vec<SearchHit> = rows
        .into_iter()
        .take(RESULT_LIMIT as usize)
        .map(|row| SearchHit {
            session_id: row.session_id,
            snippet: query::make_snippet(&row.marked_text, SNIPPET_CHARS),
        })
        .collect();
    Ok(SearchPage { items, has_more })
}

/// 一行查询结果的中间形状（只保留生成 snippet 所需的字段）。
struct QueryRow {
    session_id: String,
    marked_text: String,
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
fn reconcile(tx: &Transaction<'_>, rollouts: &Path) -> Result<(), SearchError> {
    tx.execute("DELETE FROM temp.live_docs", [])
        .and_then(|_| tx.execute("DELETE FROM temp.live_sessions", []))
        .map_err(|e| SearchError::IndexFailed(format!("清空 live 观察表失败: {e}")))?;

    let indexed = read_indexed_revisions(tx)?;
    let observed = observe_archives(rollouts)?;

    let mut changed: Vec<ObservedSession> = Vec::new();
    for session in observed.values() {
        match indexed.get(&session.id) {
            Some(revision) if revision == &session.revision => {}
            _ => changed.push(session.clone()),
        }
    }
    let deleted: Vec<String> = indexed
        .keys()
        .filter(|id| !observed.contains_key(*id))
        .cloned()
        .collect();
    let has_writes = !changed.is_empty() || !deleted.is_empty();
    if !has_writes {
        return Ok(());
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
    Ok(())
}

/// 一个磁盘上观察到的会话（本次搜索的当前态）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ObservedSession {
    id: String,
    revision: String,
    created_at: u64,
    documents: Vec<documents::SearchDoc>,
}

/// 扫描 rollout 目录，对每个 `.jsonl` 算 revision；变化的会话顺带抽出语义文档。
/// 只读：整个函数对 rollouts 目录没有任何写操作（AGENTS §3 + SH-113 验收红线）。
fn observe_archives(rollouts: &Path) -> Result<BTreeMap<String, ObservedSession>, SearchError> {
    let mut out = BTreeMap::new();
    let entries = match fs::read_dir(rollouts) {
        Ok(entries) => entries,
        // 目录不存在 = 首次运行的正常状态（与 rollout::list 同口径），不是错误。
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
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
        let bytes = fs::read(&path).map_err(|e| {
            SearchError::ArchiveReadFailed(format!("读取会话档案失败 {}: {e}", path.display()))
        })?;
        let revision = revision_of(&bytes);
        let archive = rollout::read(&path).map_err(|e| {
            SearchError::ArchiveReadFailed(format!("会话档案 {} 读不回: {e}", path.display()))
        })?;
        let created_at = match &archive.meta {
            RolloutItem::SessionMeta { created_at, .. } => *created_at,
            _ => 0,
        };
        let documents = documents::build_documents(&archive.items, created_at);
        out.insert(
            id.clone(),
            ObservedSession { id, revision, created_at, documents },
        );
    }
    Ok(out)
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
