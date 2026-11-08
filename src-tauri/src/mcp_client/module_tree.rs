//! 解释器型 server 的「模块代码树」指纹（docs/02 §3.1.4 补充）。
//!
//! 问题：`python -m windows_mcp` 这类启动方式，可执行文件只是解释器。只哈希 `python.exe`
//! 等于**审了外壳没审逻辑**——`site-packages` 里的包被换掉不会触发 retrust。
//!
//! 做法：静态解析 `-m <module>`，在解释器所在的虚拟环境里定位模块根目录，对该目录下
//! 所有可加载文件（`.py/.pyd/.so/.dll`）做**内容哈希**（不是 mtime：mtime 可伪造），
//! 排序后汇总成一个树摘要，纳入指纹与启动前校验。
//!
//! 三条边界：
//! - **静态解析，绝不执行目标代码**：定位过程只读文件系统，不跑 `python -c "import ..."`，
//!   否则在审查通过之前就把未审代码执行了一遍；
//! - **有界**：超过体积/文件数上限直接拒绝启动，不静默跳过（静默跳过等于假装审过）；
//! - **残余风险**：模块再 import 的其他包不在覆盖范围内。这是「覆盖该 server 自身代码」，
//!   不是「完整供应链校验」。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 默认文件数上限：正常 server 的包远小于这个数；超限说明不是「一个模块」。
pub const DEFAULT_MAX_FILES: usize = 2000;
/// 默认总字节上限（64 MiB）：覆盖源码与常见二进制扩展，同时防止把大目录拖进指纹计算。
pub const DEFAULT_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// 视为「可加载代码」的扩展名。
///
/// 刻意**不含 `.pyc`**：CPython 用源码哈希/时间戳校验字节码，单换 `.pyc` 不会被执行；
/// 反过来把 `.pyc` 纳入会因运行时自动重编译造成指纹抖动（假 retrust）。
const LOADABLE_EXTENSIONS: &[&str] = &["py", "pyi", "pyd", "so", "dll"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_files: usize,
    pub max_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_files: DEFAULT_MAX_FILES,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

/// 模块代码树摘要（进指纹、进审查载荷）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleTree {
    pub module: String,
    /// 展示用根路径（目录或单个模块文件）。
    pub root: String,
    pub files: usize,
    pub bytes: u64,
    pub sha256: String,
}

/// 覆盖失败：每一种都必须能直接告诉用户「怎么改」。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModuleCoverageError {
    #[error(
        "无法静态定位 `{module}` 的代码（已查找：{searched}）。只信任解释器不足以覆盖真正执行的代码；请把 command 改成该 server 的直接可执行文件，或改用不带 -m 的启动方式"
    )]
    Unresolved { module: String, searched: String },
    #[error(
        "`{module}` 的代码树超出指纹上限（{files} 个文件 / {bytes} 字节，上限 {max_files} 个 / {max_bytes} 字节）：拒绝在未完整覆盖代码的情况下启动"
    )]
    TooLarge {
        module: String,
        files: usize,
        bytes: u64,
        max_files: usize,
        max_bytes: u64,
    },
    #[error("计算 `{module}` 的代码树哈希失败：{reason}")]
    Io { module: String, reason: String },
}

/// 计算解释器型启动的模块树摘要。非解释器、或没有 `-m` 的启动返回 `Ok(None)`。
pub fn digest_for(executable: &Path, args: &[String]) -> Result<Option<ModuleTree>, ModuleCoverageError> {
    digest_with_limits(executable, args, Limits::default())
}

pub fn digest_with_limits(
    executable: &Path,
    args: &[String],
    limits: Limits,
) -> Result<Option<ModuleTree>, ModuleCoverageError> {
    let Some(module) = detect_module(executable, args) else {
        return Ok(None);
    };
    let entry = locate(executable, &module)?;
    let (root, files) = match entry {
        Entry::Dir(dir) => {
            let files = collect_files(&dir, &module, &limits)?;
            if files.is_empty() {
                return Err(ModuleCoverageError::Unresolved {
                    module,
                    searched: format!("{}（目录中没有可覆盖的代码文件）", dir.display()),
                });
            }
            (dir, files)
        }
        Entry::File(file) => {
            let size = std::fs::metadata(&file)
                .map_err(|error| ModuleCoverageError::Io {
                    module: module.clone(),
                    reason: error.to_string(),
                })?
                .len();
            (
                file.clone(),
                vec![(
                    file.file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    file,
                    size,
                )],
            )
        }
    };

    let bytes = files.iter().map(|(_, _, size)| *size).sum();
    let mut hasher = Sha256::new();
    hasher.update(b"shaniu-mcp-module-tree-v1\0");
    hasher.update(module.as_bytes());
    hasher.update([0u8]);
    for (relative, path, _) in &files {
        let digest = super::command::file_sha256(path).map_err(|error| ModuleCoverageError::Io {
            module: module.clone(),
            reason: format!("{}: {error}", path.display()),
        })?;
        hasher.update(relative.as_bytes());
        hasher.update([0u8]);
        hasher.update(digest.as_bytes());
        hasher.update([0u8]);
    }

    Ok(Some(ModuleTree {
        module,
        root: root.display().to_string(),
        files: files.len(),
        bytes,
        sha256: hex(&hasher.finalize()),
    }))
}

/// 解析 `python -m <module>` 的模块名。只对 python 系解释器生效。
pub fn detect_module(executable: &Path, args: &[String]) -> Option<String> {
    let stem = executable.file_stem()?.to_string_lossy().to_ascii_lowercase();
    if !stem.starts_with("python") {
        return None;
    }
    for (index, arg) in args.iter().enumerate() {
        if arg == "-m" {
            return args
                .get(index + 1)
                .filter(|module| !module.is_empty() && !module.starts_with('-'))
                .cloned();
        }
        if let Some(module) = arg.strip_prefix("-m") {
            if !module.is_empty() {
                return Some(module.to_string());
            }
        }
    }
    None
}

enum Entry {
    Dir(PathBuf),
    File(PathBuf),
}

fn locate(executable: &Path, module: &str) -> Result<Entry, ModuleCoverageError> {
    let mut searched = Vec::new();
    for site_packages in candidate_roots(executable) {
        searched.push(site_packages.display().to_string());
        let mut path = site_packages;
        for part in module.split('.') {
            path.push(part);
        }
        if path.is_dir() && path.join("__init__.py").is_file() {
            return Ok(Entry::Dir(path));
        }
        let single = path.with_extension("py");
        if single.is_file() {
            return Ok(Entry::File(single));
        }
    }
    Err(ModuleCoverageError::Unresolved {
        module: module.to_string(),
        searched: if searched.is_empty() {
            "解释器所在目录下没有 site-packages".to_string()
        } else {
            searched.join("; ")
        },
    })
}

/// 候选模块根：`site-packages` 本身，加上 `.pth` 指向的目录。
///
/// `.pth` 必须支持：`uv pip install -e .` 这类可编辑安装只往 `site-packages` 丢一个
/// `__editable__.xxx.pth`，真正的代码在仓库的 `src/` 下（本机 Windows-MCP 就是这种）。
/// 只读 `.pth` 里的**路径行**，`import` 行一律跳过——`.pth` 能执行代码，绝不能为了
/// 定位模块去跑它。
fn candidate_roots(executable: &Path) -> Vec<PathBuf> {
    let site_packages = site_packages_roots(executable);
    let mut roots = site_packages.clone();
    for dir in &site_packages {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .extension()
                .is_none_or(|extension| !extension.eq_ignore_ascii_case("pth"))
            {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') || line.starts_with("import ") || line.starts_with("import\t") {
                    continue;
                }
                let candidate = PathBuf::from(line);
                roots.push(if candidate.is_absolute() {
                    candidate
                } else {
                    dir.join(candidate)
                });
            }
        }
    }
    roots
}

/// 由解释器路径推虚拟环境的 `site-packages`。
///
/// 只做布局推断，不跑解释器：Windows venv 是 `<venv>/Scripts/python.exe` +
/// `<venv>/Lib/site-packages`；也有把解释器放在根目录的嵌入版布局。
fn site_packages_roots(executable: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let Some(exe_dir) = executable.parent() else {
        return roots;
    };
    if let Some(root) = exe_dir.parent() {
        roots.push(root.join("Lib").join("site-packages"));
        roots.push(root.join("lib").join("site-packages"));
    }
    roots.push(exe_dir.join("Lib").join("site-packages"));
    roots.push(exe_dir.join("site-packages"));
    roots
}

fn collect_files(
    root: &Path,
    module: &str,
    limits: &Limits,
) -> Result<Vec<(String, PathBuf, u64)>, ModuleCoverageError> {
    let mut files = Vec::new();
    let mut bytes = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|error| ModuleCoverageError::Io {
            module: module.to_string(),
            reason: format!("{}: {error}", dir.display()),
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| ModuleCoverageError::Io {
                module: module.to_string(),
                reason: error.to_string(),
            })?;
            let file_type = entry.file_type().map_err(|error| ModuleCoverageError::Io {
                module: module.to_string(),
                reason: error.to_string(),
            })?;
            let path = entry.path();
            // 不跟随符号链接：防止一条指向系统目录的链接把指纹计算拖爆。
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if entry.file_name().to_string_lossy().eq_ignore_ascii_case("__pycache__") {
                    continue;
                }
                stack.push(path);
                continue;
            }
            if !is_loadable(&path) {
                continue;
            }
            let size = entry
                .metadata()
                .map_err(|error| ModuleCoverageError::Io {
                    module: module.to_string(),
                    reason: error.to_string(),
                })?
                .len();
            bytes = bytes.saturating_add(size);
            files.push((relative_name(root, &path), path, size));
            if files.len() > limits.max_files || bytes > limits.max_bytes {
                return Err(ModuleCoverageError::TooLarge {
                    module: module.to_string(),
                    files: files.len(),
                    bytes,
                    max_files: limits.max_files,
                    max_bytes: limits.max_bytes,
                });
            }
        }
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(files)
}

fn is_loadable(path: &Path) -> bool {
    path.extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|extension| LOADABLE_EXTENSIONS.contains(&extension.as_str()))
}

/// 相对路径统一用 `/`：Windows 上 `\` 与 `/` 混用不会把同一棵树算成两个摘要。
fn relative_name(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("/")
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> crate::test_support::TestTempDir {
        crate::test_support::TestTempDir::new(&format!("module-tree-{tag}"))
    }

    /// 造一个 venv 布局：`<root>/Scripts/python.exe` + `<root>/Lib/site-packages/<pkg>/…`。
    fn fake_venv(tag: &str, module: &str) -> (crate::test_support::TestTempDir, PathBuf) {
        let root = temp_dir(tag);
        let scripts = root.join("Scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        let python = scripts.join("python.exe");
        std::fs::write(&python, b"MZ fake interpreter").unwrap();

        let package = root.join("Lib").join("site-packages").join(module);
        std::fs::create_dir_all(package.join("sub")).unwrap();
        std::fs::write(package.join("__init__.py"), b"VALUE = 1\n").unwrap();
        std::fs::write(package.join("core.py"), b"def run():\n    return 1\n").unwrap();
        std::fs::write(package.join("sub").join("mod.py"), b"X = 2\n").unwrap();
        // 非代码文件不参与指纹。
        std::fs::write(package.join("README.md"), b"docs").unwrap();
        (root, python)
    }

    #[test]
    fn non_python_or_plain_script_start_is_outside_coverage() {
        let args = vec!["-m".to_string(), "pkg".to_string()];
        assert_eq!(detect_module(Path::new(r"C:\tools\node.exe"), &args), None);
        assert_eq!(
            detect_module(Path::new(r"C:\venv\Scripts\python.exe"), &["server.py".to_string()]),
            None
        );
    }

    #[test]
    fn module_flag_forms_are_both_recognized() {
        let exe = Path::new(r"C:\venv\Scripts\python.exe");
        assert_eq!(
            detect_module(exe, &["-m".to_string(), "pkg".to_string()]),
            Some("pkg".to_string())
        );
        assert_eq!(
            detect_module(exe, &["-mpkg".to_string()]),
            Some("pkg".to_string())
        );
    }

    #[test]
    fn digest_covers_module_sources_and_changes_with_content() {
        let (root, python) = fake_venv("digest", "fakepkg");
        let args = vec!["-m".to_string(), "fakepkg".to_string()];
        let first = digest_for(&python, &args).unwrap().expect("应覆盖模块树");
        assert_eq!(first.module, "fakepkg");
        assert_eq!(first.files, 3, "只有 .py 进入摘要（README 不算）");
        assert!(first.sha256.len() == 64);

        // 同一棵树多次计算必须稳定。
        let again = digest_for(&python, &args).unwrap().unwrap();
        assert_eq!(first.sha256, again.sha256);

        // 改任意一个 .py 的内容 → 摘要必须变（mtime 不变也变，因为算的是内容）。
        std::fs::write(root.join("Lib/site-packages/fakepkg/sub/mod.py"), b"X = 3\n").unwrap();
        let changed = digest_for(&python, &args).unwrap().unwrap();
        assert_ne!(first.sha256, changed.sha256, "模块代码变化必须改变指纹");
    }

    #[test]
    fn editable_install_via_pth_is_resolved() {
        let root = temp_dir("pth");
        let scripts = root.join("Scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        let python = scripts.join("python.exe");
        std::fs::write(&python, b"MZ").unwrap();

        // 可编辑安装：site-packages 里只有一个 .pth，代码在 src/ 下。
        let site_packages = root.join("Lib").join("site-packages");
        std::fs::create_dir_all(&site_packages).unwrap();
        let source = root.join("src").join("editable_pkg");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("__init__.py"), b"OK = 1\n").unwrap();
        std::fs::write(
            site_packages.join("__editable__.editable_pkg.pth"),
            format!("{}\n", root.join("src").display()),
        )
        .unwrap();

        let tree = digest_for(&python, &["-m".to_string(), "editable_pkg".to_string()])
            .unwrap()
            .expect("可编辑安装必须能定位到模块");
        assert!(tree.root.contains("src"), "根目录应指向 .pth 里的路径: {tree:?}");
        assert_eq!(tree.files, 1);
    }

    #[test]
    fn oversized_module_tree_is_refused_not_silently_skipped() {
        let (_root, python) = fake_venv("too-large", "fakepkg");
        let args = vec!["-m".to_string(), "fakepkg".to_string()];
        let error = digest_with_limits(
            &python,
            &args,
            Limits {
                max_files: 1,
                max_bytes: DEFAULT_MAX_BYTES,
            },
        )
        .unwrap_err();
        assert!(matches!(error, ModuleCoverageError::TooLarge { .. }), "实际: {error:?}");
        assert!(error.to_string().contains("拒绝"), "错误要说清拒绝而不是跳过: {error}");
    }

    #[test]
    fn unlocatable_module_is_an_error_with_actionable_text() {
        let root = temp_dir("missing");
        let scripts = root.join("Scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        let python = scripts.join("python.exe");
        std::fs::write(&python, b"MZ").unwrap();

        let error = digest_for(&python, &["-m".to_string(), "ghost".to_string()]).unwrap_err();
        assert!(matches!(error, ModuleCoverageError::Unresolved { .. }), "实际: {error:?}");
        assert!(error.to_string().contains("直接可执行文件"), "错误要给可操作建议: {error}");
    }

    #[test]
    fn single_file_module_is_covered() {
        let root = temp_dir("single");
        let scripts = root.join("Scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        let python = scripts.join("python.exe");
        std::fs::write(&python, b"MZ").unwrap();
        let site_packages = root.join("Lib").join("site-packages");
        std::fs::create_dir_all(&site_packages).unwrap();
        std::fs::write(site_packages.join("single.py"), b"print('x')\n").unwrap();

        let tree = digest_for(&python, &["-m".to_string(), "single".to_string()])
            .unwrap()
            .expect("单文件模块也应覆盖");
        assert_eq!(tree.files, 1);
    }
}
