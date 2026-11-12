//! 容器内 MSVC 工具链环境注入（SH-021 后续修正）。
//!
//! ## 为什么需要
//! `rustc` 找 MSVC 链接器靠 vswhere/Setup COM 探测；在 AppContainer（LowBox 令牌）里
//! 这条探测链失效（实测 `cargo test` 报 `linker link.exe not found`），而宿主侧正常。
//! codex 的 Windows 沙箱用独立沙箱用户令牌，不存在包 SID 的访问检查问题，无现成实现可抄。
//!
//! ## 做法
//! 宿主侧解析一次工具链位置（vswhere + 目录探测，不走 COM），把 PATH/INCLUDE/LIB
//! 注入容器子进程环境。这是 vcvars 在系统级做的事，等价于给「工具链目录」补上访问路径
//! ——不放宽任何安全语义：容器仍是零特权令牌 + Job + 包 SID 三层纵深。
//!
//! ## 失败语义
//! 探测不到 VS/SDK 时返回空集合，行为与修复前一致（cargo 本来也跑不了）；
//! 不阻断任何不依赖 MSVC 的命令。

use std::env;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// 解析结果缓存：进程生命周期内 vswhere/目录结构不会变。
static CACHE: OnceLock<Vec<(String, String)>> = OnceLock::new();

/// 返回要叠加到容器子进程环境的条目：MSVC 工具链 + Python stdio 编码补丁。
///
/// 编码补丁与 MSVC 无关，**探测不到 VS 也照发**（见 [`PYTHON_UTF8_ENV`]）。
pub fn toolchain_env() -> &'static [(String, String)] {
    CACHE.get_or_init(|| {
        let old_path = env::var("PATH").unwrap_or_default();
        let mut entries = resolve()
            .map(|paths| paths.into_entries_with(&old_path))
            .unwrap_or_default();
        entries.extend(
            PYTHON_UTF8_ENV
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string())),
        );
        entries
    })
}

/// Python stdio 编码补丁（对齐 ZCode `runtimeCommandEnv.ts` 的 `PYTHON_UTF8_ENV_PATCH`）。
///
/// 理由：Windows 上 CPython 未显式指定编码时按**当前代码页**（简体中文机器是 GBK/936）
/// 写 stdout/stderr，而我们的工具输出一律按 UTF-8 解析——中文会乱码、`json` 输出会解析
/// 失败。`PYTHONUTF8=1` 同时把文件默认编码切到 UTF-8，`PYTHONIOENCODING` 兜住 stdio。
/// 这属于工具链环境变量，不是放宽沙箱边界。
const PYTHON_UTF8_ENV: [(&str, &str); 2] =
    [("PYTHONIOENCODING", "utf-8"), ("PYTHONUTF8", "1")];

/// 一次探测得到的工具链布局。
#[derive(Debug, Clone, PartialEq, Eq)]
struct MsvcPaths {
    /// HostX64\x64（或 HostX86\x86）bin 目录，含 link.exe。
    bin_dir: PathBuf,
    /// 链接器要搜的库目录（LIB）：MSVC CRT + UCRT + SDK um。
    lib: String,
    /// 编译器/资源编译器的头文件目录（INCLUDE）：MSVC + UCRT/shared/um。
    include: Option<String>,
}

impl MsvcPaths {
    /// 落成注入键值：bin 目录前置进 PATH，其余原样。旧 PATH 由调用方读环境传入
    /// （**纯函数**——单元测试不许改进程环境，那会把并行测试的 PATH 全局污染）。
    fn into_entries_with(self, old_path: &str) -> Vec<(String, String)> {
        let mut entries = vec![
            (
                "PATH".to_string(),
                format!("{};{old_path}", self.bin_dir.to_string_lossy()),
            ),
            ("LIB".to_string(), self.lib),
        ];
        if let Some(include) = self.include {
            entries.push(("INCLUDE".to_string(), include));
        }
        entries
    }
}

/// 宿主侧探测。任何一步失败都返回 None（不报错——没装 VS 是正常形态）。
fn resolve() -> Option<MsvcPaths> {
    let install = installation_root()?;
    let msvc = newest_with(&install.join("VC").join("Tools").join("MSVC"), &|dir| {
        probe_host_dir(dir).is_some()
    })?;
    let bin_dir = probe_host_dir(&msvc)?;
    let kits_root = windows_kits_root()?;
    let kits_version = newest_with(&kits_root.join("Lib"), &|dir| {
        dir.join("um").join("x64").join("kernel32.lib").is_file()
    })?;
    let version_name = kits_version.file_name()?.to_string_lossy().to_string();
    let kit_lib = kits_version.clone();
    let kit_include = kits_root.join("Include").join(&version_name);

    let lib = [
        msvc.join("lib").join("x64"),
        kit_lib.join("ucrt").join("x64"),
        kit_lib.join("um").join("x64"),
    ]
    .iter()
    .filter(|dir| dir.is_dir())
    .map(|dir| dir.to_string_lossy().to_string())
    .collect::<Vec<_>>();
    if lib.is_empty() {
        return None;
    }
    let include = [
        msvc.join("include"),
        kit_include.join("ucrt"),
        kit_include.join("shared"),
        kit_include.join("um"),
    ]
    .iter()
    .filter(|dir| dir.is_dir())
    .map(|dir| dir.to_string_lossy().to_string())
    .collect::<Vec<_>>();

    Some(MsvcPaths {
        bin_dir,
        lib: lib.join(";"),
        include: (!include.is_empty()).then(|| include.join(";")),
    })
}

/// vswhere 定位 VS 安装根（BuildTools / VS 均可）。
fn installation_root() -> Option<PathBuf> {
    let program_files_x86 = env::var_os("ProgramFiles(x86)")?;
    let vswhere = PathBuf::from(program_files_x86)
        .join("Microsoft Visual Studio")
        .join("Installer")
        .join("vswhere.exe");
    if !vswhere.is_file() {
        return None;
    }
    let output = {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            // CREATE_NO_WINDOW：宿主是 GUI 进程，不给 vswhere 闪控制台。
            std::process::Command::new(&vswhere)
                .args([
                    "-latest",
                    "-products",
                    "*",
                    "-requires",
                    "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
                    "-property",
                    "installationPath",
                ])
                .creation_flags(0x0800_0000)
                .output()
                .ok()?
        }
        #[cfg(not(windows))]
        std::process::Command::new(&vswhere)
            .args(["-latest", "-property", "installationPath"])
            .output()
            .ok()?
    };
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    let path = PathBuf::from(line);
    path.is_dir().then_some(path)
}

/// Windows Kits 根（标准安装位置；不在则放弃，不读注册表）。
fn windows_kits_root() -> Option<PathBuf> {
    let program_files_x86 = env::var_os("ProgramFiles(x86)")?;
    let root = PathBuf::from(program_files_x86).join("Windows Kits").join("10");
    root.is_dir().then_some(root)
}

/// 取目录下「字典序最大且通过校验」的子目录（版本目录名即语义版本，字典序≈版本序）。
fn newest_with(parent: &Path, keep: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    let mut versions: Vec<PathBuf> = std::fs::read_dir(parent)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|dir| dir.is_dir() && keep(dir))
        .collect();
    versions.sort();
    versions.pop()
}

/// HostX64\x64 优先，退回 HostX86\x86；判定标准是目录里真有 link.exe。
fn probe_host_dir(msvc: &Path) -> Option<PathBuf> {
    for (host, target) in [("HostX64", "x64"), ("HostX86", "x86")] {
        let dir = msvc.join("bin").join(host).join(target);
        if dir.join("link.exe").is_file() {
            return Some(dir);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 探测函数对不存在的安装一律返回 None（不 panic、不误报）。
    #[test]
    fn probes_tolerate_missing_install() {
        assert_eq!(
            probe_host_dir(Path::new("Z:\\no-such-msvc")),
            None,
            "无 link.exe 时应返回 None"
        );
        assert_eq!(newest_with(Path::new("Z:\\no-such-dir"), &|_| true), None);
        assert_eq!(windows_kits_root_beside(Path::new("Z:\\no-such")), None);
    }

    /// into_entries_with：bin 前置进 PATH（旧 PATH 保留在后），LIB/INCLUDE 原样。
    #[test]
    fn entries_prepend_path_and_keep_lib_include() {
        let paths = MsvcPaths {
            bin_dir: PathBuf::from("C:\\msvc\\bin\\HostX64\\x64"),
            lib: "C:\\libs".to_string(),
            include: Some("C:\\includes".to_string()),
        };
        assert_eq!(
            paths.into_entries_with("C:\\old"),
            vec![
                (
                    "PATH".to_string(),
                    "C:\\msvc\\bin\\HostX64\\x64;C:\\old".to_string()
                ),
                ("LIB".to_string(), "C:\\libs".to_string()),
                ("INCLUDE".to_string(), "C:\\includes".to_string()),
            ]
        );
    }

    /// include 缺席（极端裁剪安装）时只注入 PATH/LIB，不伪造空 INCLUDE。
    #[test]
    fn entries_omit_include_when_absent() {
        let paths = MsvcPaths {
            bin_dir: PathBuf::from("C:\\msvc\\bin\\HostX64\\x64"),
            lib: "C:\\libs".to_string(),
            include: None,
        };
        let entries = paths.into_entries_with("C:\\old");
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|(key, _)| key != "INCLUDE"));
    }

    /// Python 编码补丁**无条件**注入：与这台机器是否探测到 MSVC 无关。
    #[test]
    fn toolchain_env_always_injects_python_utf8() {
        let entries = toolchain_env();
        assert!(
            entries
                .iter()
                .any(|(key, value)| key == "PYTHONIOENCODING" && value == "utf-8"),
            "缺 PYTHONIOENCODING=utf-8：{entries:?}"
        );
        assert!(
            entries.iter().any(|(key, value)| key == "PYTHONUTF8" && value == "1"),
            "缺 PYTHONUTF8=1：{entries:?}"
        );
    }

    /// 把 Kits 根的探测拆成可注入参数的形态：根不存在 → None。
    fn windows_kits_root_beside(program_files_x86: &Path) -> Option<PathBuf> {
        let root = program_files_x86.join("Windows Kits").join("10");
        root.is_dir().then_some(root)
    }
}
