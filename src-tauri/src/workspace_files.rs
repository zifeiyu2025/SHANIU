//! UI 侧的工作区文件读（SH-092「文档预览真实读取」）。
//!
//! **为什么单独一条通道而不是复用 agent 工具**：`tools/fs_read.rs` 的 read_file/list_dir
//! 产出给模型看的纯文本（带截断注记），而右侧栏预览要的是**结构化**的字节窗口、行窗口、
//! 目录项与新鲜度令牌，两者的形状不可共用。
//!
//! **边界口径（2026-11-07 用户拍板）**：canonicalize 后不越出该工作区根即放行——读类
//! 不受沙箱挡位约束、不逐次审批，与 `tool_router::resolve_in_workspace` 同判据。作用域
//! 由 `workspaceId` 决定，而根路径**只由宿主注册表解析**（默认项 = `AppState::workspace_root`，
//! 注册项 = `config.workspaces` 的 path）：客户端能指定看哪个工作区，但不能指定看哪个目录。
//! 未知 id 一律拒绝，不回落全局根——回落会让"删掉的工作区"仍被读。
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::Serialize;
use tauri::Manager;

use crate::app_state::AppState;

/// 目录列出的上限，与 `tools/fs_read.rs` 的 MAX_LIST_ENTRIES 同值（一处口径两处用要同步）。
pub const MAX_LIST_ENTRIES: usize = 500;
/// 单次读的字节上限：文本页与显式字节窗口的硬顶（超出如实拒绝，不静默截断）。
pub const MAX_BYTES: u64 = 1_048_576;
/// 整文件字节读的上限（`readBytes` 不传 range 时）；与上游 `maxFileBytes` 默认值同量级。
pub const MAX_FILE_BYTES: u64 = 33_554_432;
/// `read` 缺省页大小（行）。
pub const DEFAULT_MAX_LINES: usize = 2_000;
/// `read` 允许的行页上限。
pub const MAX_LINES: usize = 20_000;

/// 一个文件的身份与新鲜度（不含内容），对应上游 `WorkspaceFileStat`。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileStat {
    pub absolute_path: String,
    /// 不透明令牌 = mtime 毫秒 + 字节数；前端只比较、不解析。
    pub version: String,
    pub bytes: u64,
}

/// 一行文本页，对应上游 `WorkspaceFileText`。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileText {
    pub absolute_path: String,
    pub version: String,
    pub bytes: u64,
    pub offset: usize,
    pub text: String,
    pub lines: usize,
    pub eof: bool,
}

/// 一个字节窗口，对应上游 `WorkspaceFileBytes`；`data` 走 base64，由适配层还原成 Uint8Array。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileBytes {
    pub absolute_path: String,
    pub version: String,
    pub bytes: u64,
    pub offset: u64,
    pub data: String,
    pub eof: bool,
}

/// 目录的一个直接子项，对应上游 `WorkspaceDirectoryEntry`。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryEntry {
    pub name: String,
    /// JSON 里叫 `type`（`type` 在 Rust 是关键字，字段名保持 kind）。
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// 目录清单，对应上游 `WorkspaceDirectoryListing`。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryListing {
    pub path: String,
    pub entries: Vec<DirectoryEntry>,
    pub truncated: bool,
}

/// 类型化失败：`code` 逐一对齐上游 `workspace-files` 的错误表，适配层按码回 RemoteError。
#[derive(Debug, Serialize)]
pub struct FileOpError {
    pub code: String,
    pub message: String,
    pub details: serde_json::Value,
}

impl FileOpError {
    fn at(code: &str, path: &str, message: String) -> Self {
        Self { code: code.to_string(), message, details: serde_json::json!({ "path": path }) }
    }
    fn not_found(path: &str) -> Self {
        Self::at("workspace-file/not-found", path, format!("路径不存在: {path}"))
    }
    fn outside(path: &str) -> Self {
        Self::at("workspace-file/outside-workspace", path, format!("路径越界（工作区之外）: {path}"))
    }
    fn too_large(path: &str, limit: u64) -> Self {
        Self {
            code: "workspace-file/too-large".into(),
            message: format!("超出单次读取上限 {limit} 字节: {path}"),
            details: serde_json::json!({ "path": path, "limit": limit }),
        }
    }
    fn not_text(path: &str) -> Self {
        Self::at("workspace-file/not-text", path, format!("不是可解码的 UTF-8 文本: {path}"))
    }
    fn not_regular_file(path: &str, kind: &str) -> Self {
        Self {
            code: "workspace-file/not-regular-file".into(),
            message: format!("不是普通文件（{kind}）: {path}"),
            details: serde_json::json!({ "path": path, "kind": kind }),
        }
    }
    fn not_directory(path: &str, kind: &str) -> Self {
        Self {
            code: "workspace-file/not-directory".into(),
            message: format!("不是目录（{kind}）: {path}"),
            details: serde_json::json!({ "path": path, "kind": kind }),
        }
    }
    fn unreadable(path: &str, reason: &str) -> Self {
        Self::at("gateway/internal", path, format!("读取失败: {reason}"))
    }
}

/// 会话绑定的工作区根（作用域只由宿主决定，客户端无法注入目录）。
///
/// **为什么不接 workspaceId**：上游契约里 `read`/`readBytes`/`stat`/`changes` 用绝对路径、
/// `list` 用工作区相对路径，两者都以"该会话的根"为边界（types.ts 的 `outside-workspace`
/// 注记）。让客户端交出它认为的根 = 把边界交给越界方；档案首行的 `config_snapshot
/// .workspace_root` 才是建档时固化的事实，也正是 `continue_session` 给工具沙箱用的同一个值。
/// 档案不存在或该字段为空 → 回落全局默认根：新会话在写下第一行档案之前就绑在全局根上，
/// 与 `create_session` 的口径一致，不是猜。
pub(crate) fn session_root(state: &AppState, session_id: &str) -> PathBuf {
    // 档案名只接受 id 字符集：`../` 之类会把路径拼出 rollouts 目录（只影响回落值也不该放行）。
    if session_id.is_empty() || !session_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return state.workspace_root();
    }
    let path = state
        .data_root()
        .join(crate::rollout::ROLLOUT_DIR)
        .join(format!("{session_id}.jsonl"));
    let Ok(file) = std::fs::File::open(&path) else { return state.workspace_root() };
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    use std::io::BufRead as _;
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return state.workspace_root();
    }
    // 首行 = SessionMeta（04 §3.1），只看 config_snapshot.workspace_root；解析失败按空处理。
    let root = serde_json::from_str::<serde_json::Value>(&line)
        .ok()
        .and_then(|v| v.get("config_snapshot")?.get("workspace_root")?.as_str().map(str::to_string))
        .unwrap_or_default();
    if root.trim().is_empty() { state.workspace_root() } else { PathBuf::from(root) }
}

/// 把请求路径归一到根内并确认存在（文件读类）。
///
/// 先对**请求路径本身**做 lstat：末段是符号链接就按上游 `not-regular-file{kind:'symlink'}`
/// 拒绝（read 类不跟随最终链接）；中间段链接照常解析，随后仍受根边界约束。
/// 根自身必须存在：不存在说明工作区目录被外部删掉，如实报错而不是建目录。
fn resolve_existing(root: &Path, path: &str) -> Result<PathBuf, FileOpError> {
    let root_canonical = root
        .canonicalize()
        .map_err(|e| FileOpError::unreadable(&root.display().to_string(), &e.to_string()))?;
    let raw = PathBuf::from(path);
    let joined = if raw.is_absolute() { raw } else { root_canonical.join(raw) };
    match std::fs::symlink_metadata(&joined) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(FileOpError::not_regular_file(path, "symlink"));
        }
        Ok(_) => {}
        Err(_) => return Err(FileOpError::not_found(path)),
    }
    let canonical = joined.canonicalize().map_err(|_| FileOpError::not_found(path))?;
    if !canonical.starts_with(&root_canonical) {
        return Err(FileOpError::outside(path));
    }
    // shell_usable 还原 verbatim 前缀：拼写与注册表/会话 cwd 一致（SH-110 随批结论）。
    Ok(crate::tool_router::shell_usable(&canonical))
}

/// 上游契约：symlink 报告**目标**的类型（`read` 仍拒绝它，所以 file 只是清单事实）。
fn entry_kind(meta: &std::fs::Metadata, is_symlink: bool) -> (&'static str, bool) {
    if is_symlink {
        return ("other", false);
    }
    if meta.is_dir() {
        ("directory", false)
    } else if meta.is_file() {
        ("file", true)
    } else {
        ("other", false)
    }
}

fn stat_of(target: &Path) -> Result<FileStat, FileOpError> {
    let meta = std::fs::metadata(target).map_err(|e| FileOpError::unreadable(&target.display().to_string(), &e.to_string()))?;
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    Ok(FileStat {
        absolute_path: target.display().to_string(),
        version: format!("{mtime_ms}-{}", meta.len()),
        bytes: meta.len(),
    })
}

/// 普通文件检查：符号链接与非文件都按上游码拒绝。
fn require_regular(target: &Path, requested: &str) -> Result<(), FileOpError> {
    let link = std::fs::symlink_metadata(target).map_err(|_| FileOpError::not_found(requested))?;
    if link.file_type().is_symlink() {
        return Err(FileOpError::not_regular_file(requested, "symlink"));
    }
    if !link.is_file() {
        let kind = if link.is_dir() { "directory" } else { "other" };
        return Err(FileOpError::not_regular_file(requested, kind));
    }
    Ok(())
}

/// 读文本行页（`read`）。按页切读：文件本身不设大小上限，单页超出 `MAX_BYTES` 才拒绝。
/// 上游语义：页外内容不读、不为了算 `eof` 而整读文件；只多探一次「还有没有下一行」。
fn read_text(target: &Path, requested: &str, offset: usize, limit: usize) -> Result<FileText, FileOpError> {
    use std::io::BufRead as _;
    require_regular(target, requested)?;
    let stat = stat_of(target)?;
    let file = std::fs::File::open(target).map_err(|e| FileOpError::unreadable(requested, &e.to_string()))?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    let mut line_number = 1usize;
    // 跳过页前内容：只计数不保留。
    while line_number < offset {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => line_number += 1,
            Err(e) => {
                let not_text = e.kind() == std::io::ErrorKind::InvalidData;
                return Err(if not_text { FileOpError::not_text(requested) } else { FileOpError::unreadable(requested, &e.to_string()) });
            }
        }
    }
    let mut page: Vec<String> = Vec::new();
    let mut page_bytes = 0u64;
    let mut eof = true;
    while line_number >= offset && page.len() < limit {
        line.clear();
        let read = match reader.read_line(&mut line) {
            Ok(read) => read,
            Err(e) => {
                let not_text = e.kind() == std::io::ErrorKind::InvalidData;
                return Err(if not_text { FileOpError::not_text(requested) } else { FileOpError::unreadable(requested, &e.to_string()) });
            }
        };
        if read == 0 {
            break;
        }
        let body = line.strip_suffix('\n').unwrap_or(&line);
        // 字节上限按页计：行正文 + 行间分隔符（一行一个 \n，首行没有），与上游 `cutPage` 同口径。
        page_bytes += body.len() as u64 + u64::from(!page.is_empty());
        if page_bytes > MAX_BYTES {
            return Err(FileOpError::too_large(requested, MAX_BYTES));
        }
        page.push(body.to_string());
        line_number += 1;
    }
    if page.len() == limit {
        // 页满：探一下还有没有下一行（只填内部缓冲，不整行读取）。
        eof = reader.fill_buf().map(|buf| buf.is_empty()).unwrap_or(true);
    }
    if page.iter().any(|line| line.contains('\0')) {
        return Err(FileOpError::not_text(requested));
    }
    Ok(FileText {
        absolute_path: stat.absolute_path,
        version: stat.version,
        bytes: stat.bytes,
        offset,
        text: page.join("\n"),
        lines: page.len(),
        eof,
    })
}

/// 读字节窗口（`readBytes`）。显式窗口按 `MAX_BYTES` 拒绝；不带 range = 整读，按 `MAX_FILE_BYTES` 拒绝。
/// 两者都不静默截断（上游 `resolveWindow`/`readBytes` 同语义）。
fn read_bytes(target: &Path, requested: &str, offset: Option<u64>, length: Option<u64>) -> Result<FileBytes, FileOpError> {
    require_regular(target, requested)?;
    let stat = stat_of(target)?;
    let start = offset.unwrap_or(0).min(stat.bytes);
    let window = match length {
        Some(n) => {
            if n > MAX_BYTES {
                return Err(FileOpError::too_large(requested, MAX_BYTES));
            }
            n
        }
        None => {
            let remaining = stat.bytes - start;
            if remaining > MAX_FILE_BYTES {
                return Err(FileOpError::too_large(requested, MAX_FILE_BYTES));
            }
            remaining
        }
    };
    use std::io::{Read, Seek};
    let mut file = std::fs::File::open(target).map_err(|e| FileOpError::unreadable(requested, &e.to_string()))?;
    file.seek(std::io::SeekFrom::Start(start)).map_err(|e| FileOpError::unreadable(requested, &e.to_string()))?;
    let mut data = Vec::with_capacity(window as usize);
    file.take(window).read_to_end(&mut data).map_err(|e| FileOpError::unreadable(requested, &e.to_string()))?;
    Ok(FileBytes {
        absolute_path: stat.absolute_path.clone(),
        version: stat.version,
        bytes: stat.bytes,
        offset: start,
        data: base64::engine::general_purpose::STANDARD.encode(&data),
        eof: start + data.len() as u64 >= stat.bytes,
    })
}

/// 列目录（`list`：路径可以是工作区相对拼写，也可以是绝对路径——上游契约两者都收；
/// 空串 = 根。返回的 `path` 一律是**工作区相对**拼写，与上游 `workspacePathOf` 同口径）。
fn list_dir(root: &Path, requested: &str) -> Result<DirectoryListing, FileOpError> {
    let root_canonical = root
        .canonicalize()
        .map_err(|e| FileOpError::unreadable(&root.display().to_string(), &e.to_string()))?;
    let raw = PathBuf::from(requested);
    let target = if requested.trim().is_empty() {
        root_canonical.clone()
    } else if raw.is_absolute() {
        raw
    } else {
        root_canonical.join(raw)
    };
    let canonical = target.canonicalize().map_err(|_| FileOpError::not_found(requested))?;
    if !canonical.starts_with(&root_canonical) {
        return Err(FileOpError::outside(requested));
    }
    if !canonical.is_dir() {
        let kind = if canonical.is_file() { "file" } else { "other" };
        return Err(FileOpError::not_directory(requested, kind));
    }
    let rel = {
        let rest = canonical.strip_prefix(&root_canonical).unwrap_or(Path::new(""));
        rest.components()
            .map(|part| part.as_os_str().to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("/")
    };
    let mut entries = Vec::new();
    let read = std::fs::read_dir(&canonical).map_err(|e| FileOpError::unreadable(requested, &e.to_string()))?;
    for item in read.flatten() {
        let link = item.metadata().ok();
        let Some(meta) = (if link.as_ref().is_some_and(|m| m.file_type().is_symlink()) {
            std::fs::metadata(item.path()).ok()
        } else {
            link.clone()
        }) else {
            continue;
        };
        let is_symlink = link.as_ref().is_some_and(|m| m.file_type().is_symlink());
        let (kind, sized) = entry_kind(&meta, is_symlink);
        entries.push(DirectoryEntry {
            name: item.file_name().to_string_lossy().to_string(),
            kind: kind.to_string(),
            size: sized.then_some(meta.len()),
        });
    }
    // 稳定的名字序：大小写不敏感、同值按原序（前端自己决定展示序，这里只保证可重复）。
    entries.sort_by_key(|entry| entry.name.to_lowercase());
    let truncated = entries.len() > MAX_LIST_ENTRIES;
    entries.truncate(MAX_LIST_ENTRIES);
    Ok(DirectoryListing { path: rel, entries, truncated })
}

/// 把 `readBytes` 的相对 target 按 `baseFile` 所在目录展开（HTML 预览读同目录资源用）。
fn with_base_file(root: &Path, path: &str, base_file: Option<&str>) -> String {
    let raw = PathBuf::from(path);
    if raw.is_absolute() || base_file.map(str::trim).unwrap_or("").is_empty() {
        return path.to_string();
    }
    // baseFile 是执行世界的绝对路径（契约注记）；它自己也要先过根边界。
    let base = PathBuf::from(base_file.unwrap_or_default());
    let Some(dir) = base.parent() else { return path.to_string() };
    if !dir.starts_with(root) {
        return path.to_string();
    }
    let joined = dir.join(raw);
    match joined.canonicalize() {
        Ok(c) => crate::tool_router::shell_usable(&c).display().to_string(),
        Err(_) => joined.display().to_string(),
    }
}

#[tauri::command]
pub fn workspace_file_stat(
    app: tauri::AppHandle,
    session_id: String,
    path: String,
) -> Result<FileStat, FileOpError> {
    let state = app.state::<AppState>();
    let root = session_root(&state, &session_id);
    let target = resolve_existing(&root, &path)?;
    let link = std::fs::symlink_metadata(&target).map_err(|_| FileOpError::not_found(&path))?;
    if link.is_dir() {
        // 目录的"stat"在上游没有形状（version 属文件消费方），如实拒绝而不是编一个。
        return Err(FileOpError::not_regular_file(&path, "directory"));
    }
    stat_of(&target)
}

#[tauri::command]
pub fn workspace_file_read(
    app: tauri::AppHandle,
    session_id: String,
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<FileText, FileOpError> {
    let state = app.state::<AppState>();
    let root = session_root(&state, &session_id);
    let target = resolve_existing(&root, &path)?;
    let offset = offset.unwrap_or(1).max(1);
    let limit = limit.unwrap_or(DEFAULT_MAX_LINES).clamp(1, MAX_LINES);
    read_text(&target, &path, offset, limit)
}

#[tauri::command]
pub fn workspace_file_read_bytes(
    app: tauri::AppHandle,
    session_id: String,
    path: String,
    offset: Option<u64>,
    length: Option<u64>,
    base_file: Option<String>,
) -> Result<FileBytes, FileOpError> {
    let state = app.state::<AppState>();
    let root = session_root(&state, &session_id);
    let resolved = with_base_file(&root, &path, base_file.as_deref());
    let target = resolve_existing(&root, &resolved)?;
    read_bytes(&target, &resolved, offset, length)
}

#[tauri::command]
pub fn workspace_file_list(
    app: tauri::AppHandle,
    session_id: String,
    path: String,
) -> Result<DirectoryListing, FileOpError> {
    let state = app.state::<AppState>();
    let root = session_root(&state, &session_id);
    list_dir(&root, &path)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Temp {
        root: PathBuf,
    }

    impl Temp {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("shaniu-wsf-{}-{}", std::process::id(), ulid::Ulid::generate()));
            std::fs::create_dir_all(&root).expect("临时根可建");
            Self { root }
        }
        fn write(&self, name: &str, body: &str) -> PathBuf {
            let path = self.root.join(name);
            std::fs::write(&path, body).expect("临时文件可写");
            path
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).ok();
        }
    }

    #[test]
    fn reads_a_line_page_with_eof_flag() {
        let temp = Temp::new();
        let path = temp.write("a.txt", "一\n二\n三\n");
        let page = read_text(&path, "a.txt", 1, 2).expect("首两行可读");
        assert_eq!(page.text, "一\n二");
        assert_eq!(page.lines, 2);
        assert!(!page.eof, "还有第三行时不该报 eof");
        let tail = read_text(&path, "a.txt", 2, 10).expect("尾窗可读");
        assert_eq!(tail.text, "二\n三");
        assert!(tail.eof);
        assert_eq!(tail.lines, 2, "结尾换行不得多出一个空行");
        let past = read_text(&path, "a.txt", 99, 10).expect("越界页是空页不是错");
        assert_eq!(past.text, "");
        assert_eq!(past.lines, 0);
        assert!(past.eof);
    }

    #[test]
    fn binary_and_oversized_reads_are_refused_by_code() {
        let temp = Temp::new();
        let bin = temp.root.join("b.bin");
        std::fs::write(&bin, [0u8, 1, 2, 3]).expect("二进制可写");
        let err = read_text(&bin, "b.bin", 1, 10).expect_err("NUL 内容不是文本");
        assert_eq!(err.code, "workspace-file/not-text");
        // 单页超限（一行就超 1 MiB）如实拒：文件本身不设上限，超的是这一页。
        let big_line = temp.root.join("bigline.txt");
        std::fs::write(&big_line, vec![b'x'; (MAX_BYTES + 1) as usize]).expect("大行可写");
        let err = read_text(&big_line, "bigline.txt", 1, 10).expect_err("超上限页要如实拒");
        assert_eq!(err.code, "workspace-file/too-large");
        assert_eq!(err.details["limit"], MAX_BYTES);
        // 字节窗口不受文本判据约束：同一份 NUL 内容能以二进制读出来
        let bytes = read_bytes(&bin, "b.bin", None, None).expect("字节读不拒二进制");
        assert_eq!(bytes.bytes, 4);
        assert!(bytes.eof);
        assert!(!bytes.data.is_empty());
        // 显式窗口超上限同样是拒绝而不是静默截断
        let err = read_bytes(&bin, "b.bin", Some(0), Some(MAX_BYTES + 1)).expect_err("窗口超限要拒");
        assert_eq!(err.code, "workspace-file/too-large");
    }

    #[test]
    fn large_files_are_paged_not_refused() {
        let temp = Temp::new();
        // 2 MiB 的短行文件：整文件超 MAX_BYTES，但一页仍可读。
        let mut body = String::new();
        while body.len() < (MAX_BYTES as usize + 4096) {
            body.push_str("0123456789abcdef\n");
        }
        let path = temp.write("big.txt", &body);
        let page = read_text(&path, "big.txt", 2, 3).expect("大文件仍可按页读");
        assert_eq!(page.text, "0123456789abcdef\n0123456789abcdef\n0123456789abcdef");
        assert_eq!(page.lines, 3);
        assert!(!page.eof);
        // 默认页大小（2 000 行）同样成立：字节上限按页累计，不能被行数放大成假超限。
        let full_page = read_text(&path, "big.txt", 1, DEFAULT_MAX_LINES).expect("默认页可读");
        assert_eq!(full_page.lines, DEFAULT_MAX_LINES);
        assert!(!full_page.eof);
        // 整文件字节读在上限内放行（图片预览不带 range 的场景）。
        let whole = read_bytes(&path, "big.txt", None, None).expect("上限内整读放行");
        assert_eq!(whole.bytes, body.len() as u64);
        assert!(whole.eof);
        assert!(!whole.data.is_empty());
    }

    #[test]
    fn symlinked_files_are_refused_like_the_upstream_wire() {
        let temp = Temp::new();
        let _ = temp.write("target.txt", "x");
        let link = temp.root.join("link.txt");
        #[cfg(windows)]
        let created = std::os::windows::fs::symlink_file(temp.root.join("target.txt"), &link).is_ok();
        #[cfg(not(windows))]
        let created = std::os::unix::fs::symlink(temp.root.join("target.txt"), &link).is_ok();
        if !created {
            // 无符号链接权限的机器上跳过（CI 常见）；边界判据由越界与 not-found 两例守着。
            return;
        }
        let err = resolve_existing(&temp.root, link.to_str().unwrap()).expect_err("read 类不跟随最终链接");
        assert_eq!(err.code, "workspace-file/not-regular-file");
        assert_eq!(err.details["kind"], "symlink");
    }

    #[test]
    fn byte_window_offsets_are_honored() {
        let temp = Temp::new();
        let path = temp.write("c.txt", "0123456789");
        let window = read_bytes(&path, "c.txt", Some(3), Some(4)).expect("窗口可读");
        assert_eq!(window.offset, 3);
        assert!(!window.eof);
        let decoded = base64::engine::general_purpose::STANDARD.decode(&window.data).expect("base64 可解");
        assert_eq!(decoded, b"3456".to_vec());
    }

    #[test]
    fn paths_outside_the_root_and_missing_paths_are_typed() {
        let temp = Temp::new();
        let _ = temp.write("in.txt", "hi");
        let outside = std::env::temp_dir().join(format!("shaniu-outside-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&outside).expect("根外目录可建");
        std::fs::write(outside.join("secret.txt"), "x").expect("根外文件可写");
        let err = resolve_existing(&temp.root, outside.join("secret.txt").to_str().unwrap())
            .expect_err("根外必须拒");
        assert_eq!(err.code, "workspace-file/outside-workspace");
        let err = resolve_existing(&temp.root, "不存在.txt").expect_err("不存在要分开报");
        assert_eq!(err.code, "workspace-file/not-found");
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn listing_sorts_names_and_marks_directories() {
        let temp = Temp::new();
        std::fs::create_dir_all(temp.root.join("子目录")).expect("子目录可建");
        let _ = temp.write("b.txt", "1");
        let _ = temp.write("A.txt", "22");
        let listing = list_dir(&temp.root, "").expect("根可列");
        assert_eq!(listing.path, "");
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["A.txt", "b.txt", "子目录"]);
        let kinds: Vec<&str> = listing.entries.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, vec!["file", "file", "directory"]);
        assert_eq!(listing.entries[1].size, Some(1));
        assert_eq!(listing.entries[2].size, None, "目录不给 size");
        assert!(!listing.truncated);
        let err = list_dir(&temp.root, "A.txt").expect_err("文件不能列");
        assert_eq!(err.code, "workspace-file/not-directory");
        // 上游契约两种拼写都收：绝对路径与相对子路径；回值一律工作区相对。
        let absolute = list_dir(&temp.root, temp.root.to_str().unwrap()).expect("绝对根路径可列");
        assert_eq!(absolute.path, "");
        let nested = list_dir(&temp.root, "子目录").expect("相对子路径可列");
        assert_eq!(nested.path, "子目录");
        let nested_absolute = list_dir(&temp.root, temp.root.join("子目录").to_str().unwrap()).expect("绝对子路径可列");
        assert_eq!(nested_absolute.path, "子目录");
        assert!(nested_absolute.entries.is_empty());
    }

    #[test]
    fn relative_targets_resolve_from_base_file_only_inside_the_root() {
        let temp = Temp::new();
        std::fs::create_dir_all(temp.root.join("docs")).expect("子目录可建");
        let _ = temp.write("docs/img.png", "px");
        let base = temp.root.join("docs").join("page.html");
        let resolved = with_base_file(&temp.root, "img.png", Some(base.to_str().unwrap()));
        assert_eq!(PathBuf::from(&resolved), temp.root.join("docs").join("img.png"));
        // baseFile 落在根外时不展开：让后续边界检查照旧拒掉
        let foreign = std::env::temp_dir().join("shaniu-foreign-base.html");
        let untouched = with_base_file(&temp.root, "x.png", Some(foreign.to_str().unwrap()));
        assert_eq!(untouched, "x.png");
    }
}
