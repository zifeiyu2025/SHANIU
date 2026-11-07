//! 派生索引库的 schema 归属与安全打开（SH-113）。
//!
//! 上游 `session-query-sqlite/src/schema.ts` 的 Rust 等价：
//! - application id + schema version 归属校验；
//! - 只认领空库或**已识别的** SHANIU 派生索引，绝不清空不认识的库；
//! - 版本不匹配的已识别派生索引原地重置（只 DROP 认识的表），文件本身不删；
//! - 文件缺失或真损坏才安全重建（损坏件**改名保留**，不误删、不改写任何 rollout 档案）。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, Transaction};

use crate::session_query::SearchError;

/// SHANIU 派生索引的 application id（"SHNQ"）。
/// 刻意**不共用**上游 DSH 的 `0x44534851`：两侧 schema 不同，共用会让上游库
/// 被误认成"我们的旧版本"而原地 DROP——那正是上游"只 reset 认识的派生 schema"
/// 要防的事故。
pub const APPLICATION_ID: i64 = 0x5348_4e51;
/// SHANIU 派生索引的 schema 版本；不兼容版本原地重置。
pub const SCHEMA_VERSION: i64 = 1;

/// 认识的用户表（含 FTS5 的 shadow 表）。出现任何未知表即拒绝，绝不 DROP 别人的数据。
const DERIVED_USER_TABLES: &[&str] = &[
    "search_state",
    "persisted_sessions",
    "persisted_docs",
    "persisted_docs_data",
    "persisted_docs_idx",
    "persisted_docs_content",
    "persisted_docs_docsize",
    "persisted_docs_config",
];

/// 对既有文件的分类结果。
enum Existing {
    /// 校验通过，可直接用（可能已做过原地版本重置）。
    Ready(Connection),
    /// 不认识的库 / 别人的库：**拒绝**，文件一个字节都不动。
    Rejected(String),
    /// 真损坏（打不开 / PRAGMA 读失败）：允许安全重建。
    Corrupt(String),
}

/// 打开、校验并初始化索引。这是 `first-search` 惰性打开的唯一入口。
pub fn open_index(path: &Path) -> Result<Connection, SearchError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            SearchError::IndexFailed(format!("创建索引目录失败 {}: {e}", parent.display()))
        })?;
    }
    let existing = if path.exists() {
        classify_existing(path)
    } else {
        Existing::Corrupt("索引文件不存在".to_string())
    };
    let mut conn = match existing {
        Existing::Ready(conn) => conn,
        Existing::Rejected(reason) => return Err(SearchError::IndexFailed(reason)),
        Existing::Corrupt(reason) => {
            tracing::debug!("派生索引 {} 将安全重建：{reason}", path.display());
            if path.exists() {
                // 保守重建：损坏件改名保留，绝不 unlink 一个可能是别人的数据。
                quarantine(path)?;
            }
            let conn = Connection::open(path).map_err(|e| {
                SearchError::IndexFailed(format!("新建派生索引失败 {}: {e}", path.display()))
            })?;
            conn.execute_batch(&format!("PRAGMA application_id = {APPLICATION_ID};"))
                .map_err(|e| SearchError::IndexFailed(format!("写入 application id 失败: {e}")))?;
            conn
        }
    };
    ensure_persistent_schema(&mut conn)?;
    ensure_temp_schema(&conn)?;
    Ok(conn)
}

/// 校验既有文件归属。只读 PRAGMA 与 sqlite_master；除"认识的版本不匹配原地重置"外不写。
fn classify_existing(path: &Path) -> Existing {
    let conn = match Connection::open(path) {
        Ok(conn) => conn,
        Err(e) => return Existing::Corrupt(format!("打不开: {e}")),
    };
    let application_id = match pragma_i64(&conn, "PRAGMA application_id") {
        Ok(value) => value,
        Err(e) => return Existing::Corrupt(format!("读 application_id 失败: {e}")),
    };
    let version = match pragma_i64(&conn, "PRAGMA user_version") {
        Ok(value) => value,
        Err(e) => return Existing::Corrupt(format!("读 user_version 失败: {e}")),
    };
    let tables = match list_user_tables(&conn) {
        Ok(tables) => tables,
        Err(e) => return Existing::Corrupt(format!("读 sqlite_master 失败: {e}")),
    };

    if application_id != 0 && application_id != APPLICATION_ID {
        return Existing::Rejected(format!(
            "索引路径 {} 属于其它应用（application_id={application_id}），拒绝改写；请另选路径或手动移除该文件",
            path.display()
        ));
    }
    if application_id == 0 && !tables.is_empty() {
        return Existing::Rejected(format!(
            "索引路径 {} 不是空库或可识别的 SHANIU 派生索引（含 {} 张用户表），拒绝改写",
            path.display(),
            tables.len()
        ));
    }
    if application_id == APPLICATION_ID {
        let unknown: Vec<&String> = tables
            .iter()
            .filter(|name| !DERIVED_USER_TABLES.contains(&name.as_str()))
            .collect();
        if !unknown.is_empty() {
            return Existing::Rejected(format!(
                "派生索引 {} 含未识别的用户表（{}），拒绝原地重置",
                path.display(),
                unknown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
        if version != SCHEMA_VERSION {
            // 只 DROP 认识的表，随后由 ensure_persistent_schema 按当前版本重建。
            if let Err(e) = reset_derived_schema(&conn, &tables) {
                return Existing::Rejected(format!("重置旧版本派生索引失败: {e}"));
            }
        }
    }
    Existing::Ready(conn)
}

fn pragma_i64(conn: &Connection, sql: &str) -> Result<i64, rusqlite::Error> {
    conn.query_row(sql, [], |row| row.get(0))
}

fn list_user_tables(conn: &Connection) -> Result<Vec<String>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT GLOB 'sqlite_*' ORDER BY name",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn reset_derived_schema(conn: &Connection, tables: &[String]) -> Result<(), rusqlite::Error> {
    for name in tables {
        // 表名来自 sqlite_master 且先经过白名单校验，用双引号包裹后不含可执行 SQL。
        conn.execute(&format!("DROP TABLE IF EXISTS \"{}\"", name.replace('"', "\"\"")), [])?;
    }
    conn.execute_batch("PRAGMA user_version = 0;")?;
    Ok(())
}

/// 把损坏的索引文件改名保留（含 WAL/SHM/journal 附属），再让调用方新建。
/// 目标是"不误删"，所以 rename 失败一律报错而不是强行覆盖。
fn quarantine(path: &Path) -> Result<(), SearchError> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "session-search.db".to_string());
    let mut counter = 0u32;
    let target: PathBuf;
    loop {
        let suffix = if counter == 0 {
            format!("{name}.corrupt-{}-{stamp}", std::process::id())
        } else {
            format!("{name}.corrupt-{}-{stamp}-{counter}", std::process::id())
        };
        let candidate = path.with_file_name(suffix);
        if !candidate.exists() {
            target = candidate;
            break;
        }
        counter += 1;
    }
    fs::rename(path, &target).map_err(|e| {
        SearchError::IndexFailed(format!(
            "损坏索引改名保留失败 {} → {}: {e}（不删除原文件）",
            path.display(),
            target.display()
        ))
    })?;
    // 附属文件尽力一并移走；残留不阻断重建，但绝不主动删除。
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut side = path.as_os_str().to_os_string();
        side.push(suffix);
        let side = PathBuf::from(side);
        if side.exists() {
            let mut side_target = target.as_os_str().to_os_string();
            side_target.push(suffix);
            let _ = fs::rename(&side, PathBuf::from(side_target));
        }
    }
    Ok(())
}

fn ensure_persistent_schema(conn: &mut Connection) -> Result<(), SearchError> {
    let ddl = format!(
        "PRAGMA application_id = {APPLICATION_ID};
         CREATE TABLE IF NOT EXISTS search_state (
           singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
           global_generation INTEGER NOT NULL
         );
         INSERT OR IGNORE INTO search_state (singleton, global_generation) VALUES (1, 0);
         CREATE TABLE IF NOT EXISTS persisted_sessions (
           id TEXT PRIMARY KEY,
           revision TEXT NOT NULL,
           generation INTEGER NOT NULL
         );
         CREATE VIRTUAL TABLE IF NOT EXISTS persisted_docs USING fts5(
           text,
           session_id UNINDEXED,
           seq UNINDEXED,
           type UNINDEXED,
           time UNINDEXED,
           surface UNINDEXED,
           codepoint_length UNINDEXED,
           tokenize = 'unicode61'
         );
         PRAGMA user_version = {SCHEMA_VERSION};"
    );
    conn.execute_batch(&ddl)
        .map_err(|e| SearchError::IndexFailed(format!("初始化持久化索引 schema 失败: {e}")))
}

/// connection-local TEMP 表：本次观察的 live 增量，shadow 同会话的持久化 base。
fn ensure_temp_schema(conn: &Connection) -> Result<(), SearchError> {
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS live_sessions (
           id TEXT PRIMARY KEY,
           fingerprint TEXT NOT NULL,
           persisted INTEGER NOT NULL,
           generation INTEGER NOT NULL
         );
         CREATE VIRTUAL TABLE IF NOT EXISTS temp.live_docs USING fts5(
           text,
           session_id UNINDEXED,
           seq UNINDEXED,
           type UNINDEXED,
           time UNINDEXED,
           surface UNINDEXED,
           codepoint_length UNINDEXED,
           tokenize = 'unicode61'
         );",
    )
    .map_err(|e| SearchError::IndexFailed(format!("初始化 live 观察表失败: {e}")))
}

/// 读当前语料 generation（cursor 绑定它，语料变化即陈旧）。
pub fn read_generation(tx: &Transaction<'_>) -> Result<String, SearchError> {
    tx.query_row(
        "SELECT global_generation FROM search_state WHERE singleton = 1",
        [],
        |row| row.get::<_, i64>(0),
    )
    .map(|value| value.to_string())
    .map_err(|e| SearchError::IndexFailed(format!("读取索引世代失败: {e}")))
}
