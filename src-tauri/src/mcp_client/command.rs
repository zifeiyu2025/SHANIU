//! 可执行文件解析与哈希（docs/02 §3.1.4：批准的是「要执行什么」）。
//!
//! 两条硬约束：
//! - **C-R5 不做 shell 解释**：只接受绝对路径或 PATH 中的裸文件名，`.cmd/.bat` 直接拒绝
//!   （v1 没有独立传输类型，也不让 argv 落到 `cmd /c`）；
//! - **哈希先于启动**：批准记录里存的是文件 SHA-256，启动与每次重启都要重算比对。

use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// 可执行文件解析失败（文本可直接展示给用户，含可操作建议）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    #[error("MCP server 的 command 为空")]
    Empty,
    #[error("拒绝相对路径 {0}：command 必须是绝对路径，或 PATH 中能找到的可执行文件名")]
    Relative(String),
    #[error("找不到可执行文件 {0}（已按 PATH / PATHEXT 搜索）")]
    NotFound(String),
    #[error("拒绝启动脚本型 server {0}：.cmd/.bat 需要 shell 解释，与 C-R5 冲突，v1 不支持")]
    ShellScript(String),
    #[error("不是普通文件: {0}")]
    NotAFile(String),
}

/// 把配置里的 `command` 解析成绝对路径。
///
/// 解析结果会固化进指纹——批准之后不再走 PATH 解析（防止 PATH 被改动后换掉目标）。
pub fn resolve_executable(command: &str) -> Result<PathBuf, ResolveError> {
    let command = command.trim();
    if command.is_empty() {
        return Err(ResolveError::Empty);
    }
    if is_shell_script(command) {
        return Err(ResolveError::ShellScript(command.to_string()));
    }

    let path = Path::new(command);
    if path.is_absolute() {
        return finish(path);
    }
    if command.contains('/') || command.contains('\\') {
        return Err(ResolveError::Relative(command.to_string()));
    }

    for dir in path_dirs() {
        for extension in candidate_extensions(command) {
            let candidate = dir.join(format!("{command}{extension}"));
            if candidate.is_file() {
                return finish(&candidate);
            }
        }
    }
    Err(ResolveError::NotFound(command.to_string()))
}

fn finish(path: &Path) -> Result<PathBuf, ResolveError> {
    if !path.exists() {
        return Err(ResolveError::NotFound(path.display().to_string()));
    }
    if !path.is_file() {
        return Err(ResolveError::NotAFile(path.display().to_string()));
    }
    Ok(normalize(path))
}

fn is_shell_script(command: &str) -> bool {
    matches!(
        Path::new(command).extension().map(|ext| ext.to_string_lossy().to_ascii_lowercase()),
        Some(ext) if ext == "cmd" || ext == "bat"
    )
}

fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|value| std::env::split_paths(&value).filter(|dir| !dir.as_os_str().is_empty()).collect())
        .unwrap_or_default()
}

/// 裸名字按 PATHEXT 尝试补全；已带扩展名的只按原样试一次。
fn candidate_extensions(name: &str) -> Vec<String> {
    if Path::new(name).extension().is_some() {
        return vec![String::new()];
    }
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    let mut extensions: Vec<String> = pathext
        .split(';')
        .map(str::trim)
        .filter(|ext| !ext.is_empty())
        .map(str::to_string)
        .collect();
    if extensions.is_empty() {
        extensions.push(".EXE".to_string());
    }
    extensions
}

/// 展开 `canonicalize` 在 Windows 上加的 `\\?\` 前缀（指纹与展示都用干净路径）。
///
/// 实现只有一份，在 `config::path_norm::canonical_or_literal`："先 canonicalize、
/// 失败按字面、再剥 verbatim"。UNC 的 verbatim 拼写不能只剥 `\\?\`（剩下的
/// `UNC\host\share` 是相对路径，会与 `\\host\share` 算成两份指纹）。
fn normalize(path: &Path) -> PathBuf {
    crate::config::path_norm::canonical_or_literal(path)
}

/// 计算文件 SHA-256（小写十六进制）。批准记录、启动前校验、重启前复核共用同一算法。
pub fn file_sha256(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str, content: &[u8]) -> (crate::test_support::TestTempDir, PathBuf) {
        let dir = crate::test_support::TestTempDir::new("mcp-cmd");
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

    /// `canonicalize` 在 UNC 上产出 `\\?\UNC\server\share\...`：剥前缀时必须还原成
    /// `\\server\share\...`，不能留下相对路径 `UNC\server\share\...`（那会让同一个
    /// 可执行文件算出两个指纹，批准记录与启动前校验对不上）。
    #[test]
    fn verbatim_unc_normalizes_like_plain_unc() {
        let verbatim = Path::new(r"\\?\UNC\localhost\shaniu-nonexistent-share\server.exe");
        let plain = Path::new(r"\\localhost\shaniu-nonexistent-share\server.exe");
        let normalized = normalize(verbatim);
        assert_eq!(normalized, normalize(plain), "UNC 两种拼写必须归一为同一路径");
        assert!(
            normalized.is_absolute(),
            "还原后必须仍是绝对路径（UNC），实际：{}",
            normalized.display()
        );
    }

    /// 钉住既有口径：`canonicalize` 失败时保留字面路径，但仍剥 verbatim 前缀
    /// （指纹必须与批准时一致，不能因为文件临时不可访问就换一把指纹）。
    #[cfg(windows)]
    #[test]
    fn non_existent_path_keeps_literal_spelling_but_still_strips_verbatim() {
        let missing = Path::new(r"\\?\C:\shaniu-no-such-dir\server.exe");
        assert_eq!(
            normalize(missing),
            PathBuf::from(r"C:\shaniu-no-such-dir\server.exe")
        );
    }

    #[test]
    fn absolute_path_resolves_to_itself() {
        let (_dir, file) = temp_file("server.exe", b"MZ");
        let resolved = resolve_executable(file.to_str().unwrap()).unwrap();
        assert_eq!(resolved, normalize(&file));
        assert!(resolved.is_absolute());
    }

    #[test]
    fn missing_absolute_path_is_reported_not_silently_absent() {
        let err = resolve_executable(r"C:\definitely-not-here-xyz\server.exe").unwrap_err();
        assert!(matches!(err, ResolveError::NotFound(_)), "实际: {err:?}");
    }

    #[test]
    fn relative_path_with_separator_is_rejected() {
        let err = resolve_executable(r".\tools\server.exe").unwrap_err();
        assert!(matches!(err, ResolveError::Relative(_)), "实际: {err:?}");
        assert!(err.to_string().contains("绝对路径"), "错误要说清怎么改: {err}");
    }

    #[test]
    fn batch_scripts_are_rejected_with_actionable_reason() {
        let (_dir, file) = temp_file("server.bat", b"@echo off");
        let err = resolve_executable(file.to_str().unwrap()).unwrap_err();
        assert!(matches!(err, ResolveError::ShellScript(_)), "实际: {err:?}");
        assert!(err.to_string().contains("shell"), "错误要点出 shell 解释: {err}");
    }

    #[test]
    fn bare_name_resolves_through_path() {
        // `cmd` 在 Windows 上必然存在，且 PATHEXT 能补出 .exe。
        let resolved = resolve_executable("cmd").expect("应能在 PATH 中找到 cmd");
        assert!(resolved.is_absolute());
        assert_eq!(
            resolved.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()),
            Some("exe".to_string())
        );
    }

    #[test]
    fn unknown_bare_name_is_not_found() {
        let err = resolve_executable("shaniu-no-such-binary-xyz").unwrap_err();
        assert!(matches!(err, ResolveError::NotFound(_)), "实际: {err:?}");
    }

    #[test]
    fn hash_is_hex_and_changes_with_content() {
        let (_dir_a, first) = temp_file("a.exe", b"first");
        let (_dir_b, second) = temp_file("b.exe", b"second");
        let a = file_sha256(&first).unwrap();
        let b = file_sha256(&second).unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "内容不同必须得到不同哈希");
        assert_eq!(a, file_sha256(&first).unwrap(), "同一文件多次计算必须稳定");
    }
}
