//! SH-114 批 2：会话日志 ZIP 导出（宿主侧打包）。
//!
//! **职责边界**：这里只做「条目路径安全校验 + ZIP 编码」。canonical JSONL 的
//! 内容由前端 `src/header-actions/session-events.ts` 那套唯一的事件翻译产出——
//! 批 1 已经定过原则：**不在 Rust 再造一份 DSH 事件 schema**，否则导出 / 回放 /
//! 搜索三处各有一份推导，迟早互相漂移（SH-108 立的口径）。
//!
//! 为什么不在这里读档案：`export_probe` 的档案读取放在 `lib.rs`（组合根），
//! 本模块因此对业务模块**零依赖**，不产生新的允许边。
//!
//! **不做什么**：不决定条目构成、不拼会话语义、不弹对话框。`dest` 由前端保存
//! 对话框给出；附件段将来走 `source_path` 流式读，本卡没有附件面，恒空。

use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// 一次「另存为」的凭据（审计 H2 批 1）。
///
/// **为什么 `dest` 不再是路径**：IPC 参数里的任何字符串都能由调用方随手拼出来，
/// 把目标换成宿主进程内存里的随机票据后，`export_session_log` 只能写「用户本人
/// 在原生对话框里选过、且尚未被消费」的那一个位置。票据不可由路径反推——它只
/// 活在宿主内存，不落盘、不回显，伪造要猜中 ULID 的随机位。
///
/// **取消即撤销**：用户点取消时清空待用票据，避免「上次确认过、这次取消」却仍
/// 写进上一次的目标。
///
/// **一次性**：`take` 取出即清空，同一张票据重复调用必然失败；重新导出必须重新
/// 弹框（即重新获得一次用户手势）。
#[derive(Default)]
pub struct SaveTicketStore {
    pending: Mutex<Option<(String, PathBuf)>>,
}

impl SaveTicketStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 签发一张新票据并替换旧票据（同一时刻只有一个待用目标）。
    pub fn issue(&self, dest: PathBuf) -> String {
        let token = format!("save-{}", ulid::Ulid::generate());
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some((token.clone(), dest));
        token
    }

    /// 用户取消选择：撤销待用票据。
    pub fn revoke(&self) {
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// 消费票据：命中即取出并清空；伪造或重复使用一律返回 None。
    pub fn take(&self, token: &str) -> Option<PathBuf> {
        let mut guard = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let hit = guard.as_ref().is_some_and(|(known, _)| known == token);
        if !hit {
            return None;
        }
        guard.take().map(|(_, dest)| dest)
    }
}

/// 上游 `archive.ts` 的 `DEFAULT_SESSION_LOG_COMPRESSION_LEVEL`。
pub const COMPRESSION_LEVEL: i64 = 6;

/// 单个 ZIP 条目的最长路径（字符数）。超长直接拒绝，不静默截断。
const MAX_PATH_CHARS: usize = 512;
/// 单个路径段的最长字符数。
const MAX_SEGMENT_CHARS: usize = 128;

/// 一个待写入 ZIP 的条目。
///
/// 文本条目给 `content`（canonical JSONL），字节条目给 `source_path`（附件流）。
/// 两者必须恰好有一个：同时给或都不给都视为调用方错误，不做"优先取谁"的猜测。
#[derive(Debug, Clone, Deserialize)]
pub struct ExportEntry {
    /// ZIP 内的相对路径，正斜杠分隔。
    pub path: String,
    /// 文本条目内容。
    #[serde(default)]
    pub content: Option<String>,
    /// 字节条目来源文件（附件段用；未接线时恒 `None`）。
    #[serde(default)]
    pub source_path: Option<String>,
}

/// 导出结果：落盘路径、字节数、条目数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExportReport {
    pub path: String,
    pub bytes: u64,
    pub entries: usize,
}

/// 探活返回的会话头（前端用它拼 canonical `session.jsonl` 的首行）。
///
/// 字段名与上游 `SessionHeader` 的语义对应，不是 SHANIU 档案 `SessionMeta` 的
/// 字段名直搬——两套格式的物理形状不同，这里是**翻译结果**。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExportHeader {
    /// 会话 id（上游 `header.id`）。
    pub id: String,
    /// Unix 毫秒创建时间。
    pub created_at: u64,
    /// 工作区根；空串视为未记录。
    pub cwd: Option<String>,
    /// 子代理的父会话；根会话为 `null`。
    pub parent_session: Option<String>,
    /// 委派深度：根 0、子代理 1（与上游 `delegationDepth` 缺省规则一致）。
    pub delegation_depth: u32,
}

/// 校验一个 ZIP 条目路径（fail-closed：可疑即拒绝，不修正、不跳过）。
///
/// 拒绝清单与理由：
/// - 反斜杠：ZIP 规范用正斜杠，反斜杠会被部分解压器当普通字符，造成"看起来有目录
///   实际是文件名"的歧义；
/// - 绝对路径 / 盘符 / 冒号：`C:/x`、`/x` 在解压时会逃出目标目录（Zip Slip）；
/// - `..` / `.` 段：同上，是最经典的目录穿越；
/// - 空段 / 空路径 / 控制字符：产物名不可预期；
/// - 超长：Windows 路径上限与解压器差异，宁可提前拒绝。
///
/// # Errors
/// 任一条不满足即返回可读原因（含原路径，便于调用方定位是哪个条目坏了）。
pub fn validate_entry_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("条目路径为空".to_string());
    }
    if path.chars().count() > MAX_PATH_CHARS {
        return Err(format!("条目路径超过 {MAX_PATH_CHARS} 字符: {path}"));
    }
    if path.contains('\\') {
        return Err(format!("条目路径不得含反斜杠（ZIP 用正斜杠）: {path}"));
    }
    if path.starts_with('/') {
        return Err(format!("条目路径不得是绝对路径: {path}"));
    }
    if path.contains(':') {
        return Err(format!("条目路径不得含盘符或流名: {path}"));
    }
    for segment in path.split('/') {
        if segment.is_empty() {
            return Err(format!("条目路径含空段: {path}"));
        }
        if segment == "." || segment == ".." {
            return Err(format!("条目路径不得含相对段: {path}"));
        }
        if segment.chars().count() > MAX_SEGMENT_CHARS {
            return Err(format!("条目路径段超过 {MAX_SEGMENT_CHARS} 字符: {path}"));
        }
        if segment.chars().any(char::is_control) {
            return Err(format!("条目路径含控制字符: {path}"));
        }
    }
    Ok(())
}

/// 把条目写成 ZIP，先落 `dest.part` 再原子改名。
///
/// **为什么写临时文件**：写到一半失败（磁盘满、条目损坏）时，目标路径上不能留下
/// 一个"能打开但缺条目"的半截归档——那比没有文件更难排查。失败一律清理临时文件。
///
/// **为什么先全校验再开写**：条目在循环里才校验的话，坏条目之前的条目已经压进临时
/// 文件了；先全验能让"调用方给错了"在没有任何 I/O 的情况下失败。
///
/// # Errors
/// 路径非法、条目同时给/都不给内容、I/O 失败、ZIP 编码失败、改名失败。
pub fn write_zip(dest: &Path, entries: &[ExportEntry]) -> Result<ExportReport, String> {
    if entries.is_empty() {
        return Err("导出条目为空：至少要有根会话日志".to_string());
    }
    for entry in entries {
        validate_entry_path(&entry.path)?;
        match (entry.content.is_some(), entry.source_path.is_some()) {
            (true, true) => {
                return Err(format!("条目同时给了 content 与 source_path: {}", entry.path));
            }
            (false, false) => {
                return Err(format!("条目既没有 content 也没有 source_path: {}", entry.path));
            }
            _ => {}
        }
    }

    let tmp = temp_path(dest);

    let written = encode_zip(&tmp, entries);
    let bytes = match written {
        Ok(bytes) => bytes,
        Err(reason) => {
            let _ = fs::remove_file(&tmp);
            return Err(reason);
        }
    };

    if let Err(reason) = fs::rename(&tmp, dest) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("移动到目标路径失败: {reason}"));
    }

    Ok(ExportReport {
        path: dest.to_string_lossy().into_owned(),
        bytes,
        entries: entries.len(),
    })
}

/// 编码主体：成功返回文件字节数，任何一步失败由调用方清理临时文件。
fn encode_zip(tmp: &Path, entries: &[ExportEntry]) -> Result<u64, String> {
    let file = File::create(tmp).map_err(|e| format!("创建临时文件失败: {e}"))?;
    let mut zip = ZipWriter::new(BufWriter::new(file));
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .compression_level(Some(COMPRESSION_LEVEL));

    for entry in entries {
        zip.start_file(entry.path.as_str(), options)
            .map_err(|e| format!("写入条目头失败 {}: {e}", entry.path))?;
        match (entry.content.as_ref(), entry.source_path.as_ref()) {
            (Some(text), None) => zip
                .write_all(text.as_bytes())
                .map_err(|e| format!("写入条目内容失败 {}: {e}", entry.path))?,
            (None, Some(source)) => {
                // 流式：附件不整读进内存（上游同样按 chunk 喂压缩器）。
                let mut input =
                    File::open(source).map_err(|e| format!("打开附件失败 {source}: {e}"))?;
                std::io::copy(&mut input, &mut zip)
                    .map_err(|e| format!("写入附件失败 {}: {e}", entry.path))?;
            }
            // write_zip 已在校验阶段排除这两种组合。
            _ => return Err(format!("条目内容形状非法: {}", entry.path)),
        }
    }

    let mut buffered = zip.finish().map_err(|e| format!("收尾 ZIP 失败: {e}"))?;
    buffered.flush().map_err(|e| format!("刷新失败: {e}"))?;
    let file = buffered
        .into_inner()
        .map_err(|e| format!("取回文件句柄失败: {}", e.error()))?;
    file.sync_all().map_err(|e| format!("同步落盘失败: {e}"))?;
    let bytes = file
        .metadata()
        .map_err(|e| format!("读取产物大小失败: {e}"))?
        .len();
    drop(file);
    Ok(bytes)
}

/// `dest` 同目录下的唯一临时文件名
/// （`session.v4.zip` → `session.v4.zip.tmp-<pid>-<seq>`）。
///
/// 同目录而非系统临时目录：跨卷 rename 会退化成 copy，失去"原子替换"的意义。
/// 唯一名而非固定 `.part`：同一目标被并发导出时，两份 `encode_zip` 不能写同一个
/// 临时文件，否则先完成的那份会把另一份还没写完的内容 rename 成正式产物。
/// `encode_zip` 用 `File::create` 截断写入，所以不需要预清理旧临时件。
fn temp_path(dest: &Path) -> PathBuf {
    crate::config::temp_sibling(dest)
}

#[cfg(test)]
mod tests;
