//! 工作区根的唯一判据（任务 7，2026-11-11）。
//!
//! **为什么需要**：`data_root` 下面就是真源（`rollouts/`、`sessions/`、`secrets/`、
//! `logs/`）。只要允许把工作区根设成 `data_root` 本身、它的祖先、或 `data_root` 下的
//! 兄弟目录（如 `data\secrets`），模型就能通过"工作区文件"工具读到会话档案与凭据。
//! 唯一合法的应用自有工作区是 `<data_root>\Workspace` 及其子目录。
//!
//! **单一判据**：所有把外部路径变成"工作区根"的入口都必须调用 [`check`]，不许各自
//! 拷贝一份 `starts_with` 判断。收口点见 `AppState::check_workspace_root`（UI 命令面）、
//! `app_state::workspace_root_of`（配置加载/默认根）、`media_route::media_roots_of`
//! （媒体根）、`registered_workspace_roots`（审计面）、`continue_session`（resume）。
//! 新增入口必须走其中之一；本模块的判定是全仓库唯一实现。
//!
//! **比较口径**：
//! 1. 规范化到最深已存在祖先（不存在也按字面逐段规范化），展开 junction/符号链接与
//!    8.3 短名；
//! 2. 去掉 `\\?\`（`\\?\UNC\` 还原成 `\\`）；
//! 3. 统一小写（NTFS 默认大小写不敏感）；
//! 4. 尾部 `\` 由 `Path` 组件比较天然吸收。
//!
//! 比较用 `Path::starts_with`（按组件），所以 `data\secrets` 不会被误判成
//! `data\secret` 的子目录。

use std::path::{Component, Path, PathBuf};

/// 判定结论。拒绝时带原因，调用方据此给出可执行的提示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Reject(Rejection),
}

impl Verdict {
    pub fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }

    pub fn rejection(&self) -> Option<&Rejection> {
        match self {
            Self::Reject(reason) => Some(reason),
            Self::Allow => None,
        }
    }
}

/// 拒绝原因（三条判定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// 等于数据根本身。
    IsDataRoot,
    /// 是数据根的祖先（含盘根 / 用户目录）。
    AncestorOfDataRoot,
    /// 在数据根内、但不是 `data_root\Workspace` 及其子目录（如 `data\secrets`）。
    InsideDataRoot,
}

impl Rejection {
    /// 面向用户的说明：说清"为什么不行"和"该选哪里"，不只丢一句"非法路径"。
    pub fn message(&self, candidate: &Path, data_root: &Path) -> String {
        let allowed = data_root.join(crate::config::WORKSPACE_DIR);
        match self {
            Self::IsDataRoot => format!(
                "不能把数据目录本身（{}）设为工作区：会话档案、日志与凭据都在里面。\n\
                 请选择 {} 或别的目录。",
                data_root.display(),
                allowed.display()
            ),
            Self::AncestorOfDataRoot => format!(
                "不能把数据目录的上级目录（{}）设为工作区：它包含数据目录 {}，\
                 会让工具读到会话档案、日志与凭据。\n请选择 {} 或别的目录。",
                candidate.display(),
                data_root.display(),
                allowed.display()
            ),
            Self::InsideDataRoot => format!(
                "不能把数据目录内的 {} 设为工作区：只有 {} 及其子目录是应用自有的工作区。\n\
                 请改选 {} 或数据目录之外的目录。",
                candidate.display(),
                allowed.display(),
                allowed.display()
            ),
        }
    }
}

/// 用进程当前的数据根判定（生产入口）。数据根尚未初始化时（单元测试直接调）
/// 返回 `Allow`，因为此时没有任何真源路径可供泄露；生产启动路径一定先初始化数据根。
pub fn check(candidate: &Path) -> Verdict {
    match crate::config::current_data_root() {
        Some(data_root) => check_against(candidate, data_root),
        None => Verdict::Allow,
    }
}

/// 显式指定数据根判定（测试与构造阶段用；不依赖全局状态）。
pub fn check_against(candidate: &Path, data_root: &Path) -> Verdict {
    let candidate = canon_for_compare(candidate);
    let data_root = canon_for_compare(data_root);
    if candidate == data_root {
        return Verdict::Reject(Rejection::IsDataRoot);
    }
    if data_root.starts_with(&candidate) {
        return Verdict::Reject(Rejection::AncestorOfDataRoot);
    }
    let workspace = data_root.join(crate::config::WORKSPACE_DIR);
    let workspace = canon_for_compare(&workspace);
    if candidate.starts_with(&data_root) && !candidate.starts_with(&workspace) {
        return Verdict::Reject(Rejection::InsideDataRoot);
    }
    Verdict::Allow
}

/// 规范化成"只用于比较"的路径（已小写、已去 verbatim 前缀）。
///
/// 不存在的前缀按字面处理：`data\Workspace\..\secrets` 即使 `Workspace` 不存在，
/// 也会先按组件消掉 `..`，不会因为 canonicalize 失败而漏判。
pub fn canon_for_compare(path: &Path) -> PathBuf {
    let normalized = lexical_normalize(path);
    let resolved = canonicalize_deepest(&normalized);
    PathBuf::from(strip_verbatim(&resolved).to_lowercase())
}

/// 按组件消掉 `.` / `..`（仅字面，不碰盘）。
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // 盘根/UNC 根上再 `..` 无处可退：保留原样由后续规范化处理。
                if !out.pop() {
                    out.push(component.as_os_str());
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// 沿祖先链找**最深的一个能 canonicalize 的目录**，把剩下的尾部原样接回去。
/// 这样 junction / 8.3 短名 / 大小写只要出现在已存在的部分就一定能展开。
fn canonicalize_deepest(path: &Path) -> PathBuf {
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

fn strip_verbatim(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        text.into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestTempDir;

    fn data_root(base: &TestTempDir) -> PathBuf {
        let root = base.join("data");
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn reason(candidate: &Path, data_root: &Path) -> Option<Rejection> {
        check_against(candidate, data_root).rejection().copied()
    }

    /// 反向对照：不做排除时 `data\secrets` 是"合法目录"，加判据后必须拒绝。
    #[test]
    fn rejects_every_way_to_reach_the_data_root() {
        let base = TestTempDir::new("gate-reject");
        let root = data_root(&base);

        assert_eq!(reason(&root, &root), Some(Rejection::IsDataRoot), "等于数据根");
        let ancestor = root.parent().unwrap();
        assert_eq!(
            reason(ancestor, &root),
            Some(Rejection::AncestorOfDataRoot),
            "祖先目录（D:\\SHANIU 形态）"
        );
        assert_eq!(
            reason(&root.join("secrets"), &root),
            Some(Rejection::InsideDataRoot),
            "data\\secrets 是最关键的拒绝项：凭据就在里面"
        );
        assert_eq!(
            reason(&root.join("rollouts"), &root),
            Some(Rejection::InsideDataRoot),
            "会话档案不能被当工作区读"
        );
        // `..` 变体：data\Workspace\..\secrets 即使 Workspace 不存在也要判成 data\secrets。
        assert_eq!(
            reason(&root.join("Workspace").join("..").join("secrets"), &root),
            Some(Rejection::InsideDataRoot),
            ".. 变体不能绕过"
        );
        // 大小写变体（NTFS 不敏感）。
        assert_eq!(
            reason(&PathBuf::from(root.display().to_string().to_uppercase()).join("SECRETS"), &root),
            Some(Rejection::InsideDataRoot),
            "大小写变体不能绕过"
        );
        // 尾部分隔符变体。
        let trailing = PathBuf::from(format!(r"{}\", root.display()));
        assert_eq!(
            reason(&trailing, &root),
            Some(Rejection::IsDataRoot),
            "尾分隔符不能把同一目录变成另一个路径"
        );
    }

    #[test]
    fn allows_workspace_and_directories_outside_data_root() {
        let base = TestTempDir::new("gate-allow");
        let root = data_root(&base);
        std::fs::create_dir_all(root.join("Workspace").join("proj")).unwrap();
        std::fs::create_dir_all(base.join("outside")).unwrap();

        assert_eq!(reason(&root.join("Workspace"), &root), None, "默认工作区必须放行");
        assert_eq!(
            reason(&root.join("Workspace").join("proj"), &root),
            None,
            "默认工作区的子目录必须放行"
        );
        assert_eq!(reason(&base.join("outside"), &root), None, "数据目录之外必须放行");
        // 前缀相似但不同目录：组件比较不能误伤。
        assert_eq!(
            reason(&root.join("Workspace2"), &root),
            Some(Rejection::InsideDataRoot),
            "Workspace2 不是 Workspace 的子目录"
        );
        assert_eq!(
            reason(&root.join("Work").join("space"), &root),
            Some(Rejection::InsideDataRoot),
            "Work\\space 也不是 Workspace"
        );
    }

    /// 不存在的路径也要能判：按最深已存在祖先展开，尾部按字面。
    #[test]
    fn non_existent_workspace_under_data_root_is_still_rejected() {
        let base = TestTempDir::new("gate-missing");
        let root = data_root(&base);
        let candidate = root.join("not-created").join("proj");
        assert_eq!(reason(&candidate, &root), Some(Rejection::InsideDataRoot));
        assert_eq!(
            reason(&root.join("Workspace").join("not-created"), &root),
            None,
            "默认工作区下尚未创建的目录仍应放行（首次使用时才创建）"
        );
    }

    /// junction 变体：能用 mklink 时验证真实目录联接被展开；不能创建时跳过。
    #[test]
    fn junction_pointing_into_data_root_is_rejected() {
        let base = TestTempDir::new("gate-junction");
        let root = data_root(&base);
        let link = base.join("link-to-secrets");
        std::fs::create_dir_all(root.join("secrets")).unwrap();
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(root.join("secrets"))
            .status();
        let Ok(status) = status else { return };
        if !status.success() || !link.exists() {
            // 没有创建联接的权限（非管理员/受限环境）：如实跳过，不假装验证过。
            eprintln!("跳过 junction 用例：当前环境无法创建目录联接");
            return;
        }
        assert_eq!(
            reason(&link, &root),
            Some(Rejection::InsideDataRoot),
            "指向 data\\secrets 的 junction 必须被展开后拒绝"
        );
    }

    #[test]
    fn rejection_messages_name_both_sides_and_the_allowed_path() {
        let base = TestTempDir::new("gate-message");
        let root = data_root(&base);
        for (candidate, rejection) in [
            (root.clone(), Rejection::IsDataRoot),
            (root.parent().unwrap().to_path_buf(), Rejection::AncestorOfDataRoot),
            (root.join("secrets"), Rejection::InsideDataRoot),
        ] {
            let message = rejection.message(&candidate, &root);
            assert!(
                message.contains(&root.display().to_string()),
                "提示必须点名数据目录：{message}"
            );
            assert!(
                message.contains(&root.join("Workspace").display().to_string()),
                "提示必须给出可用的默认工作区：{message}"
            );
        }
    }
}
