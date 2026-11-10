//! 上游 `ui-open-in-app` 的 Windows 宿主面（SH-092 批 3）。
//!
//! **边界**：目录启动只认本模块解析出的目录 id（explorer/vscode/cursor/…），文件「打开方式」
//! 只认注册表当前为该扩展名登记的处理器，显式 application 还会在打开前**重新校验**一次
//! （上游 resolver 的 revalidate 语义）。所有路径必须绝对且存在；目录走 ShellExecute 的 open
//! 动词（直接 spawn explorer 不保证抬升窗口），文件走默认处理器或已登记的处理器。
//!
//! **图标不做**：上游从可执行文件抽 PNG；本仓不引入图像解码，`icon` 一律 null，
//! 客户端按自己的回退规则画原生图钉（`OpenInAppApps` 的 apps 路由仍给图标 URL，
//! `/open-in-app/icon/*` 由前端路由垫片回 404 → vendored 组件回退）。
use std::path::{Path, PathBuf};

use serde::Serialize;

/// 文件关联的一条处理器（上游 `NativeFileApplication` 的 wire 形状）。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileApplication {
    /// 可执行文件路径：客户端原样回传，宿主打开前重新校验。
    pub id: String,
    pub name: String,
    pub default: bool,
    pub icon: Option<String>,
}

/// 目录打开的应用目录：只暴露确实解析到可执行文件的项。
#[derive(Debug, Clone)]
enum Launch {
    /// 交给 shell 的 open 动词（explorer）。
    ShellOpen,
    /// 直接 spawn（GUI 应用走 detached 子进程）。
    Exe { exe: PathBuf },
}

struct CatalogEntry {
    id: &'static str,
    launch: Launch,
}

#[cfg(windows)]
mod win {
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegEnumValueW, RegOpenKeyExW, RegQueryValueExW, HKEY, KEY_READ, REG_EXPAND_SZ,
        REG_SZ, REG_VALUE_TYPE,
    };

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    unsafe fn open(root: HKEY, subkey: &str) -> Option<HKEY> {
        let name = wide(subkey);
        let mut key = HKEY::default();
        let status = RegOpenKeyExW(root, PCWSTR(name.as_ptr()), None, KEY_READ, &mut key);
        (status == ERROR_SUCCESS).then_some(key)
    }

    /// 读一个字符串值；`value = None` 读默认值。只认 REG_SZ / REG_EXPAND_SZ。
    pub fn read_string(root: HKEY, subkey: &str, value: Option<&str>) -> Option<String> {
        unsafe {
            let key = open(root, subkey)?;
            let value_name = value.map(wide);
            let value_ptr = value_name.as_ref().map_or(PCWSTR::null(), |name| PCWSTR(name.as_ptr()));
            let mut kind = REG_VALUE_TYPE::default();
            let mut size = 0u32;
            let status = RegQueryValueExW(key, value_ptr, None, Some(&mut kind), None, Some(&mut size));
            if status != ERROR_SUCCESS || size == 0 {
                let _ = RegCloseKey(key);
                return None;
            }
            let mut buffer = vec![0u8; size as usize];
            let status = RegQueryValueExW(
                key,
                value_ptr,
                None,
                Some(&mut kind),
                Some(buffer.as_mut_ptr()),
                Some(&mut size),
            );
            let _ = RegCloseKey(key);
            if status != ERROR_SUCCESS || (kind != REG_SZ && kind != REG_EXPAND_SZ) {
                return None;
            }
            let units: Vec<u16> = buffer
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes(*pair))
                .collect();
            let end = units.iter().position(|&unit| unit == 0).unwrap_or(units.len());
            Some(String::from_utf16_lossy(&units[..end]))
        }
    }

    /// 枚举一个键下的值名（OpenWithList / OpenWithProgids 都是值而非子键）。
    pub fn enum_value_names(root: HKEY, subkey: &str) -> Vec<String> {
        unsafe {
            let Some(key) = open(root, subkey) else { return Vec::new() };
            let mut names = Vec::new();
            let mut index = 0u32;
            loop {
                let mut buffer = vec![0u16; 512];
                let mut length = buffer.len() as u32;
                let status = RegEnumValueW(
                    key,
                    index,
                    Some(PWSTR(buffer.as_mut_ptr())),
                    &mut length,
                    None,
                    None,
                    None,
                    None,
                );
                if status != ERROR_SUCCESS {
                    break;
                }
                names.push(String::from_utf16_lossy(&buffer[..length as usize]));
                index += 1;
            }
            let _ = RegCloseKey(key);
            names
        }
    }
}

/// 展开 `%VAR%`（注册表命令与 App Paths 里常见）；未知变量原样保留。
fn expand_env(text: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start + 1..].find('%') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let name = &rest[start + 1..start + 1 + end];
        match lookup(name) {
            Some(value) if !name.is_empty() => out.push_str(&value),
            _ => out.push_str(&rest[start..start + end + 2]),
        }
        rest = &rest[start + end + 2..];
    }
    out.push_str(rest);
    out
}

fn env_lookup(name: &str) -> Option<String> {
    std::env::vars_os().find_map(|(key, value)| {
        if key.to_string_lossy().eq_ignore_ascii_case(name) {
            Some(value.to_string_lossy().into_owned())
        } else {
            None
        }
    })
}

/// 从 `"C:\path\app.exe" "%1"` 这类注册表命令行里取可执行文件路径。
fn command_exe(command: &str) -> Option<PathBuf> {
    let expanded = expand_env(command.trim(), env_lookup);
    let trimmed = expanded.trim();
    let raw = if let Some(stripped) = trimmed.strip_prefix('"') {
        stripped.split('"').next()?
    } else {
        trimmed.split_whitespace().next()?
    };
    let path = PathBuf::from(raw.trim());
    (!path.as_os_str().is_empty()).then_some(path)
}

fn app_paths(exe: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        use windows::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
        let subkey = format!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\{exe}");
        let value = win::read_string(HKEY_CURRENT_USER, &subkey, None)
            .or_else(|| win::read_string(HKEY_LOCAL_MACHINE, &subkey, None))?;
        let path = PathBuf::from(value.trim().trim_matches('"'));
        path.is_file().then_some(path)
    }
    #[cfg(not(windows))]
    {
        let _ = exe;
        None
    }
}

fn env_path(name: &str) -> Option<PathBuf> {
    env_lookup(name).map(PathBuf::from)
}

fn path_lookup(exe: &str) -> Option<PathBuf> {
    let path = env_lookup("PATH")?;
    path.split(';')
        .filter(|entry| !entry.trim().is_empty())
        .map(|entry| PathBuf::from(entry.trim()).join(exe))
        .find(|candidate| candidate.is_file())
}

fn resolve_exe(exe: &str, candidates: &[PathBuf]) -> Option<PathBuf> {
    if let Some(path) = app_paths(exe) {
        return Some(path);
    }
    candidates
        .iter()
        .find(|candidate| candidate.is_file())
        .cloned()
        .or_else(|| path_lookup(exe))
}

fn program_files() -> Option<PathBuf> {
    env_path("ProgramFiles").or_else(|| env_path("ProgramFiles(x86)"))
}

fn local_app_data() -> Option<PathBuf> {
    env_path("LOCALAPPDATA")
}

/// 目录打开的应用目录（win32 子集，顺序照上游 catalog）。
fn catalog() -> Vec<CatalogEntry> {
    let mut apps = vec![CatalogEntry { id: "explorer", launch: Launch::ShellOpen }];
    let mut add = |id: &'static str, exe: &str, candidates: Vec<Option<PathBuf>>| {
        let candidates: Vec<PathBuf> = candidates.into_iter().flatten().collect();
        if let Some(path) = resolve_exe(exe, &candidates) {
            apps.push(CatalogEntry { id, launch: Launch::Exe { exe: path } });
        }
    };
    add(
        "cursor",
        "Cursor.exe",
        vec![local_app_data().map(|dir| dir.join("Programs/cursor/Cursor.exe"))],
    );
    add(
        "vscode",
        "Code.exe",
        vec![
            local_app_data().map(|dir| dir.join("Programs/Microsoft VS Code/Code.exe")),
            program_files().map(|dir| dir.join("Microsoft VS Code/Code.exe")),
        ],
    );
    add(
        "vscodeinsiders",
        "Code - Insiders.exe",
        vec![
            local_app_data().map(|dir| dir.join("Programs/Microsoft VS Code Insiders/Code - Insiders.exe")),
        ],
    );
    add(
        "windsurf",
        "Windsurf.exe",
        vec![local_app_data().map(|dir| dir.join("Programs/Windsurf/Windsurf.exe"))],
    );
    add(
        "sublimetext",
        "sublime_text.exe",
        vec![program_files().map(|dir| dir.join("Sublime Text/sublime_text.exe"))],
    );
    add("windowsterminal", "wt.exe", vec![]);
    add(
        "gitbash",
        "git-bash.exe",
        vec![program_files().map(|dir| dir.join("Git/git-bash.exe"))],
    );
    apps
}

/// 每个应用的 argv（目录在启动时才展开；上表存了空占位，这里按 id 生成）。
fn launch_args(id: &str, path: &Path) -> Vec<String> {
    let directory = path.to_string_lossy().into_owned();
    match id {
        "windowsterminal" => vec!["-d".to_string(), directory],
        "gitbash" => vec![format!("--cd={directory}")],
        _ => vec![directory],
    }
}

#[cfg(windows)]
fn shell_open(path: &Path) -> Result<(), String> {
    use windows::core::HSTRING;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let operation = HSTRING::from("open");
    let target = HSTRING::from(path.to_string_lossy().into_owned());
    let result = unsafe { ShellExecuteW(None, &operation, &target, None, None, SW_SHOWNORMAL) };
    if result.0 as isize <= 32 {
        return Err(format!("系统拒绝打开该路径（ShellExecute 返回 {}）", result.0 as isize));
    }
    Ok(())
}

#[cfg(not(windows))]
fn shell_open(_path: &Path) -> Result<(), String> {
    Err("仅支持 Windows 桌面".to_string())
}

#[cfg(windows)]
fn reveal(path: &Path) -> Result<(), String> {
    std::process::Command::new("explorer.exe")
        .arg(format!("/select,{}", path.display()))
        .spawn()
        .map_err(|e| format!("无法唤起资源管理器: {e}"))?;
    Ok(())
}

#[cfg(not(windows))]
fn reveal(_path: &Path) -> Result<(), String> {
    Err("仅支持 Windows 桌面".to_string())
}

fn existing_absolute(path: &str) -> Result<PathBuf, String> {
    let target = PathBuf::from(path.trim());
    if !target.is_absolute() {
        return Err("路径必须是绝对路径".to_string());
    }
    if !target.exists() {
        return Err(format!("路径不存在: {path}"));
    }
    Ok(target)
}

fn friendly_name(exe: &Path) -> String {
    #[cfg(windows)]
    {
        use windows::Win32::System::Registry::HKEY_CLASSES_ROOT;
        if let Some(name) = exe.file_name().and_then(|name| name.to_str()) {
            if let Some(friendly) = win::read_string(
                HKEY_CLASSES_ROOT,
                &format!("Applications\\{name}"),
                Some("FriendlyAppName"),
            ) {
                if !friendly.trim().is_empty() {
                    return friendly;
                }
            }
        }
    }
    exe.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "应用".to_string())
}

fn progid_app(progid: &str) -> Option<(PathBuf, String)> {
    #[cfg(windows)]
    {
        use windows::Win32::System::Registry::HKEY_CLASSES_ROOT;
        let command = win::read_string(
            HKEY_CLASSES_ROOT,
            &format!("{progid}\\shell\\open\\command"),
            None,
        )?;
        let exe = command_exe(&command)?;
        if !exe.is_file() {
            return None;
        }
        let display = win::read_string(HKEY_CLASSES_ROOT, progid, None)
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| friendly_name(&exe));
        Some((exe, display))
    }
    #[cfg(not(windows))]
    {
        let _ = progid;
        None
    }
}

/// 某文件当前的注册处理器（默认项在最前，其余按注册表枚举序）。
fn file_applications(target: &Path) -> Vec<FileApplication> {
    let Some(extension) = target.extension().and_then(|ext| ext.to_str()) else {
        return Vec::new();
    };
    if extension.is_empty() {
        return Vec::new();
    }
    let ext = format!(".{extension}");
    let mut entries: Vec<(PathBuf, String, bool)> = Vec::new();
    let mut push = |exe: PathBuf, name: String, default: bool| {
        if entries.iter().any(|(known, ..)| known == &exe) {
            return;
        }
        entries.push((exe, name, default));
    };

    #[cfg(windows)]
    {
        use windows::Win32::System::Registry::{HKEY_CLASSES_ROOT, HKEY_CURRENT_USER};
        let choice = win::read_string(
            HKEY_CURRENT_USER,
            &format!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\FileExts\\{ext}\\UserChoice"),
            Some("ProgId"),
        );
        if let Some(progid) = choice.as_deref() {
            if let Some((exe, name)) = progid_app(progid) {
                push(exe, name, true);
            }
        }
        for value in win::enum_value_names(
            HKEY_CURRENT_USER,
            &format!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\FileExts\\{ext}\\OpenWithList"),
        ) {
            if value.eq_ignore_ascii_case("MRUList") {
                continue;
            }
            if let Some(exe) = resolve_exe(&value, &[]) {
                let name = friendly_name(&exe);
                push(exe, name, false);
            }
        }
        for progid in win::enum_value_names(HKEY_CLASSES_ROOT, &format!("{ext}\\OpenWithProgids")) {
            if let Some((exe, name)) = progid_app(&progid) {
                push(exe, name, false);
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = &mut push;
    }

    entries
        .into_iter()
        .map(|(exe, name, default)| FileApplication {
            id: exe.to_string_lossy().into_owned(),
            name,
            default,
            icon: None,
        })
        .collect()
}

/// 目录可用的应用 id（上游 apps 路由的 `{apps:[…]}`）。
#[tauri::command]
pub fn open_in_app_catalog() -> Vec<String> {
    catalog().into_iter().map(|entry| entry.id.to_string()).collect()
}

/// 用一个目录应用打开已校验的目录（审计 H2 批 2：路径判定上移到组合根）。
///
/// `app` 只认 [`catalog`] 里的 id；本函数不做路径范围校验——调用方必须先把
/// 目标限制在已注册工作区内（E32：本模块零业务依赖，拿不到 AppState）。
pub fn open_in_app_launch_at(app: &str, target: &Path) -> Result<(), String> {
    if !target.is_dir() {
        return Err("目录应用只能打开目录".to_string());
    }
    let entry = catalog()
        .into_iter()
        .find(|entry| entry.id == app)
        .ok_or_else(|| {
            tracing::warn!(application = app, "拒绝：应用不在目录中");
            format!("应用未安装或不在目录中: {app}")
        })?;
    match entry.launch {
        Launch::ShellOpen => shell_open(target),
        Launch::Exe { exe } => {
            std::process::Command::new(&exe)
                .args(launch_args(app, target))
                .spawn()
                .map(|_| ())
                .map_err(|e| format!("启动 {} 失败: {e}", exe.display()))
        }
    }
}

/// 文件当前的注册处理器（上游 `workspacePathApplications`）。
#[tauri::command]
pub fn workspace_path_applications(path: String) -> Result<Vec<FileApplication>, String> {
    let target = existing_absolute(&path)?;
    if target.is_dir() {
        return Ok(Vec::new());
    }
    Ok(file_applications(&target))
}

/// 打开/定位一个已校验的工作区路径：`reveal` 走资源管理器选中，否则默认应用或显式处理器。
///
/// **路径范围由调用方先行限制**（审计 H2 批 2）：本函数只做动作分派，不判断目标
/// 是否在工作区内——那一步用组合根里与沙箱同源的 `tool_router::resolve_in_workspace`。
pub fn open_workspace_path_at(
    target: &Path,
    action: Option<&str>,
    application: Option<&str>,
) -> Result<(), String> {
    if action == Some("reveal") {
        return reveal(target);
    }
    match application.filter(|app| !app.trim().is_empty()) {
        Some(app) => {
            if target.is_dir() {
                // 目录：只认 catalog id。改前这里把 `application` 当 exe 路径直接
                // spawn，等于「任意 exe + 任意目录」——注入可直接执行指定程序。
                open_in_app_launch_at(app, target)
            } else {
                // 文件：只认「该文件当前注册处理器」这个封闭集合。条目 id 本身就是
                // 宿主从注册表读出的 exe 路径，调用方自带的任意路径一律拒绝。
                let known = file_applications(target);
                let hit = known
                    .iter()
                    .find(|known| known.id.eq_ignore_ascii_case(app))
                    .ok_or_else(|| {
                        tracing::warn!(
                            application = app,
                            target = %target.display(),
                            "拒绝：应用不是该文件的注册处理器"
                        );
                        "应用不再是该文件的处理程序".to_string()
                    })?;
                std::process::Command::new(&hit.id)
                    .arg(target)
                    .spawn()
                    .map(|_| ())
                    .map_err(|e| format!("启动 {} 失败: {e}", hit.id))
            }
        }
        None => shell_open(target),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_command_lines_yield_the_executable() {
        assert_eq!(
            command_exe("\"C:\\Program Files\\App\\app.exe\" \"%1\"").as_deref(),
            Some(Path::new("C:\\Program Files\\App\\app.exe"))
        );
        assert_eq!(
            command_exe("app.exe %1").as_deref(),
            Some(Path::new("app.exe"))
        );
        assert_eq!(command_exe("   "), None);
    }

    #[test]
    fn env_expansion_keeps_unknown_placeholders() {
        let expanded = expand_env("%KNOWN%\\bin\\%UNKNOWN%\\x.exe", |name| {
            (name == "KNOWN").then(|| "C:\\App".to_string())
        });
        assert_eq!(expanded, "C:\\App\\bin\\%UNKNOWN%\\x.exe");
    }

    #[test]
    fn launch_arguments_follow_the_upstream_catalog() {
        let dir = Path::new("D:\\ws");
        assert_eq!(launch_args("vscode", dir), vec!["D:\\ws"]);
        assert_eq!(launch_args("windowsterminal", dir), vec!["-d", "D:\\ws"]);
        assert_eq!(launch_args("gitbash", dir), vec!["--cd=D:\\ws"]);
    }

    #[test]
    fn paths_must_be_absolute_and_exist() {
        assert!(existing_absolute("relative/path").is_err());
        assert!(existing_absolute("C:\\definitely-not-here-shaniu").is_err());
    }
}