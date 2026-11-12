//! Windows 路径规范化的唯一实现（FF-1 收敛，2026-11-13）。
//!
//! **为什么落在 `config`**：`sandbox`、`mcp_client`、`tool_router` 需要同一套 verbatim
//! 前缀规则，而 `tool_router` 在 `sandbox` 之上——让 `sandbox` 反向依赖 `tool_router`
//! 是层次错误（`scripts/check-deps.ps1` 会判违规，docs/01 §2.3.1）。`config` 是
//! docs/01 §2.3.3 的共享基础设施，任何模块都可依赖；它本来就持有数据根、主目录与
//! 工作区判据，路径比较口径放在这里不需要新增架构节点。
//!
//! 这里只放两个原语：剥 verbatim 前缀、取最深已存在祖先。是否先 `canonicalize`、
//! 是否小写、是否去尾分隔符这些**比较口径留在调用点**——收进来会变成"万能 normalize"，
//! 把各调用点原本不同的语义悄悄抹平。

use std::path::{Component, Path, PathBuf};

/// 去掉 Windows 的 `\\?\` verbatim 前缀，还原成等价的普通路径。
///
/// `canonicalize()` 在 Windows 上必然产出 `\\?\C:\...` 形态，而 cmd.exe 认不了它
/// （`cd /d "\\?\C:\..."` 失败、重定向静默不产出文件，SH-085 的真实成因），所以
/// 展示、指纹与 ACL 键都要先还原。UNC 的 verbatim 拼写是 `\\?\UNC\server\share\...`：
/// 只剥 `\\?\` 会得到**相对路径** `UNC\server\share\...`，与 `\\server\share\...`
/// 收敛不到一起——同一目录会派生两个写 SID（docs/16 §1.2）。
///
/// 非 verbatim 路径（普通绝对/相对路径）原样返回；`\\.\` 设备路径等其它前缀不是
/// `canonicalize` 的产物，也原样交出，不猜它的等价形态。
pub fn strip_verbatim(path: &Path) -> PathBuf {
    let mut components = path.components();
    match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            // drive 是 ASCII 码（u8），必须转成字符；根已在字符串里给出，
            // 后面的 RootDir 组件必须跳过，否则拼出 "D:\\\x"
            std::path::Prefix::VerbatimDisk(drive) => {
                let mut out = PathBuf::from(format!("{}:\\", char::from(drive)));
                out.extend(components.filter(|c| !matches!(c, Component::RootDir)));
                out
            }
            std::path::Prefix::VerbatimUNC(host, share) => {
                // host/share 是 OsStr，不能进 format!；用 push 逐段拼
                let mut out = PathBuf::from(r"\\");
                out.push(host);
                out.push(share);
                out.extend(components.filter(|c| !matches!(c, Component::RootDir)));
                out
            }
            _ => path.to_path_buf(),
        },
        _ => path.to_path_buf(),
    }
}

/// 沿祖先链找**最深的一个能 canonicalize 的路径**，把剩下的尾部按字面接回去。
///
/// 给"目标尚不存在"的比较用（工作区根判据）：不存在的尾部不能因为 canonicalize
/// 失败就放弃展开已存在部分，否则 junction / 8.3 短名 / 大小写别名就漏判了。
/// 全链都不存在时退回 `std::path::absolute`（仍失败才按字面返回）。
pub fn deepest_existing(path: &Path) -> PathBuf {
    if let Ok(resolved) = path.canonicalize() {
        return resolved;
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = path;
    while let Some(parent) = cursor.parent() {
        if let Some(name) = cursor.file_name() {
            tail.push(name.to_os_string());
        }
        cursor = parent;
        if let Ok(mut resolved) = cursor.canonicalize() {
            for part in tail.iter().rev() {
                resolved.push(part);
            }
            return resolved;
        }
    }
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestTempDir;

    /// 两种 verbatim 前缀都必须还原；普通路径与 `\\.\` 设备路径原样交出。
    #[cfg(windows)]
    #[test]
    fn strip_verbatim_restores_drive_and_unc_spellings() {
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\C:\Users\jun\a")),
            PathBuf::from(r"C:\Users\jun\a")
        );
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\UNC\fileserver\share\x")),
            PathBuf::from(r"\\fileserver\share\x")
        );
        assert_eq!(
            strip_verbatim(Path::new(r"\\fileserver\share\x")),
            PathBuf::from(r"\\fileserver\share\x")
        );
        assert_eq!(
            strip_verbatim(Path::new(r"C:\Users\jun\a")),
            PathBuf::from(r"C:\Users\jun\a")
        );
        // 设备路径不是 canonicalize 的产物，不猜等价形态
        assert_eq!(
            strip_verbatim(Path::new(r"\\.\COM1")),
            PathBuf::from(r"\\.\COM1")
        );
    }

    /// 已存在的祖先必须被展开，缺失的尾部按字面接回（工作区根判据依赖这条）。
    #[cfg(windows)]
    #[test]
    fn deepest_existing_expands_the_existing_ancestor_and_keeps_the_missing_tail() {
        let dir = TestTempDir::new("path-norm-missing");
        let existing = dir.join("real");
        std::fs::create_dir_all(&existing).unwrap();

        let missing = existing.join("not-created").join("file.txt");
        let resolved = deepest_existing(&missing);
        assert_eq!(resolved.file_name().unwrap(), "file.txt");
        assert_eq!(
            resolved.parent().unwrap(),
            existing.canonicalize().unwrap().join("not-created"),
            "已存在的前缀要展开成 canonical 形态，缺失尾段原样接回"
        );
    }

    #[test]
    fn deepest_existing_canonicalizes_an_existing_path() {
        let dir = TestTempDir::new("path-norm-exists");
        assert_eq!(deepest_existing(&dir), dir.canonicalize().unwrap());
    }

    /// 全链都不存在时仍要给绝对路径（不能退回空相对路径）。
    #[cfg(windows)]
    #[test]
    fn deepest_existing_falls_back_to_absolute_for_a_fully_missing_chain() {
        let resolved = deepest_existing(Path::new(r"Z:\shaniu-no-such-drive\a\b.txt"));
        assert!(
            resolved.is_absolute(),
            "全链不存在时也要给绝对路径: {}",
            resolved.display()
        );
    }
}
