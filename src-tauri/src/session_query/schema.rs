//! 派生索引库的 schema 归属与安全打开（SH-113）。
//!
//! 上游 `session-query-sqlite/src/schema.ts` 的 Rust 等价：
//! - application id + schema version 归属校验；
//! - 只认领空库或**已识别的** SHANIU 派生索引，绝不清空不认识的库；
//! - 版本不匹配的已识别派生索引原地重置（只 DROP 认识的表），文件本身不删；
//! - 文件缺失或真损坏才安全重建（损坏件**改名保留**，不误删、不改写任何 rollout 档案）。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, Transaction};

use crate::session_query::SearchError;

/// 索引连接的忙等待上限（2026-11-11）。派生索引会被第二个实例并发访问。
/// rusqlite 0.37 的 `Connection::open` 默认已设 5000ms；这里显式写出来不是"从 0 改成 5 秒"，
/// 而是不依赖依赖库默认值（升级后可能漂移）。5 秒覆盖一次对账事务的正常时长，
/// 超过 5 秒说明另一方卡死，报错比无限等更好。
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// SHANIU 派生索引的 application id（"SHNQ"）。
/// 刻意**不共用**上游 DSH 的 `0x44534851`：两侧 schema 不同，共用会让上游库
/// 被误认成"我们的旧版本"而原地 DROP——那正是上游"只 reset 认识的派生 schema"
/// 要防的事故。
pub const APPLICATION_ID: i64 = 0x5348_4e51;
/// SHANIU 派生索引的 schema 版本；不兼容版本原地重置。
///
/// v2：`surface` 不再恒为 `current`——被压缩锚点取代的原文记 `shadowed`
/// （能搜到，但模型当前不再看到）。旧索引里的 `current` 已经失真，
/// 必须靠版本号强制重建，而不是等档案下次变化才顺手修正。
pub const SCHEMA_VERSION: i64 = 2;

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
            conn.busy_timeout(BUSY_TIMEOUT).map_err(|e| {
                SearchError::IndexFailed(format!("设置新建索引忙等待失败 {}: {e}", path.display()))
            })?;
            conn.execute_batch(&format!("PRAGMA application_id = {APPLICATION_ID};"))
                .map_err(|e| SearchError::IndexFailed(format!("写入 application id 失败: {e}")))?;
            conn
        }
    };
    // 先配连接参数再碰 schema：下面的 DDL 与对账都要经过写锁，忙等待必须在它们之前生效。
    configure_connection(&conn, path)?;
    ensure_persistent_schema(&mut conn)?;
    ensure_temp_schema(&conn)?;
    Ok(conn)
}

/// 连接参数：忙等待 + WAL（2026-11-11）。
///
/// `busy_timeout` 必须每条连接都设（SQLite 是 per-connection 语义），包括
/// `classify_existing` 里那条——否则双开时归属校验会在争用下被判成 "Corrupt"，
/// 进而尝试 quarantine 一个其实健康的库。
///
/// WAL 让读不阻塞写、写不阻塞读，正是双开搜索的场景。设不上（网络盘等不支持 WAL 的
/// 文件系统）只记 warn 继续用当前模式：派生索引可用性优先于模式偏好，不为一个
/// 模式选择让搜索整体不可用。WAL 的 `-wal/-shm` 残留由 `quarantine` 一并改名带走。
fn configure_connection(conn: &Connection, path: &Path) -> Result<(), SearchError> {
    conn.busy_timeout(BUSY_TIMEOUT).map_err(|e| {
        SearchError::IndexFailed(format!("设置索引忙等待失败 {}: {e}", path.display()))
    })?;
    match conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get::<_, String>(0)) {
        Ok(mode) if mode.eq_ignore_ascii_case("wal") => {}
        Ok(mode) => tracing::warn!(
            path = %path.display(),
            mode = %mode,
            "派生索引未进入 WAL 模式，继续使用当前模式"
        ),
        Err(error) => tracing::warn!(
            path = %path.display(),
            %error,
            "派生索引切换 WAL 失败，继续使用当前模式"
        ),
    }
    Ok(())
}

/// 校验既有文件归属。只读 PRAGMA 与 sqlite_master；除"认识的版本不匹配原地重置"外不写。
fn classify_existing(path: &Path) -> Existing {
    let conn = match Connection::open(path) {
        Ok(conn) => conn,
        Err(e) => return Existing::Corrupt(format!("打不开: {e}")),
    };
    // 与 open_index 同一条忙等待口径：这里的读不该被另一个实例的写瞬间打回 BUSY，
    // 否则"争用"会被误判成"文件损坏"并触发 quarantine。
    if let Err(e) = conn.busy_timeout(BUSY_TIMEOUT) {
        return Existing::Corrupt(format!("设置忙等待失败: {e}"));
    }
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

/// 损坏件保留 **第一份 + 最近两份**（总数上限）。
///
/// 不采用单纯「最近 N 份」：轮转删除先删最早的，而最早那份恰恰是**第一次损坏的
/// 原始现场**——后面几份可能只是同一个 bug 在重复发生，只留最近的等于把唯一干净
/// 样本丢掉，等想查的时候已经没有可对照的基线。
///
/// 自创（参考源里都没有自动上限）：DSH `storage-domain` 的 backup-and-skip 只隔离不清理；
/// ZCode `settingService.ts` 的 `.corrupt-<ts>` 交给「存储清理」界面由用户删；
/// codex 无对应机制。清理是尽力而为，失败不影响重建。
pub(crate) const QUARANTINE_KEEP: usize = 3;

/// 把损坏的索引文件改名保留（含 WAL/SHM/journal 附属），再让调用方新建。
/// 目标是"不误删"，所以 rename 失败一律报错而不是强行覆盖。
fn quarantine(path: &Path) -> Result<(), SearchError> {
    let stamp = crate::clock::now_ms();
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
    prune_quarantined(&target, &name);
    Ok(())
}

/// 隔离件文件名里的毫秒时间戳：`{name}.corrupt-{pid}-{stamp}[-{counter}]`。
///
/// 不能用 mtime 排序：`rename` 会保留原文件的修改时间，一份「今天刚改名」的隔离件
/// 可能带着几个月前的 mtime —— 按 mtime 会把第一次损坏的现场误判成最新的，
/// 恰好把要保的那份删掉。文件名里的 stamp 才是隔离发生的真实先后。
fn quarantine_stamp(file_name: &str, prefix: &str) -> Option<u128> {
    let rest = file_name.strip_prefix(prefix)?;
    let mut parts = rest.split('-');
    parts.next()?; // pid：不参与排序
    parts.next()?.parse().ok()
}

/// 保留**第一份 + 最近两份**，其余连同 `-wal/-shm/-journal` 一起清掉。
///
/// `newest` 是刚隔离出来的那一份，无条件保留（即使它的 stamp 因时钟回拨排不进最新两份）。
fn prune_quarantined(newest: &Path, name: &str) {
    let Some(dir) = newest.parent() else { return };
    let prefix = format!("{name}.corrupt-");
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut found: Vec<(u128, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let file_name = entry.file_name().to_string_lossy().into_owned();
            // 附属件跟着主件走，不作为独立候选。
            if ["-wal", "-shm", "-journal"]
                .iter()
                .any(|suffix| file_name.ends_with(suffix))
            {
                return None;
            }
            let stamp = quarantine_stamp(&file_name, &prefix)?;
            Some((stamp, entry.path()))
        })
        .collect();
    if found.len() <= QUARANTINE_KEEP {
        return;
    }
    // 新 → 旧；同一毫秒按文件名倒序（counter 大的更新）。
    found.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.file_name().cmp(&a.1.file_name()))
    });

    // 保留集：刚隔离的一份 + 最早的一份（首次损坏现场）+ 最新的若干份补足上限。
    let mut keep: HashSet<PathBuf> = HashSet::new();
    keep.insert(newest.to_path_buf());
    if let Some((_, oldest)) = found.last() {
        keep.insert(oldest.clone());
    }
    for (_, path) in found.iter().take(QUARANTINE_KEEP - 1) {
        if keep.len() >= QUARANTINE_KEEP {
            break;
        }
        keep.insert(path.clone());
    }
    for (_, old) in &found {
        if keep.contains(old) {
            continue;
        }
        tracing::debug!(path = %old.display(), "清理过旧的损坏索引保留件");
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let mut candidate = old.as_os_str().to_os_string();
            candidate.push(suffix);
            let _ = fs::remove_file(PathBuf::from(candidate));
        }
    }
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
