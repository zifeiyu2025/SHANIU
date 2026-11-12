//! AppContainer 容器 profile 生命周期（SH-021，ADR-002 M2）。
//!
//! 职责边界（用户 2026-11-08 定的机制选型）：
//!  - **AppContainer 打底**：路径/读隔离（网络能力由调用方按执行路径传入）；
//!  - 现有 `CreateRestrictedToken` + JobObject **保留做纵深**，两层各自可独立验证。
//!
//! 生命周期：
//!  1. `ensure()`：先按名字派生容器 SID；没有 profile 才创建（已存在返回
//!     `ERROR_ALREADY_EXISTS`，按复用处理，不视作失败）。
//!  2. `delete()`：退出/卸载时删除 profile，避免残留容器 SID 与悬空 profile。
//!     只删自己创建的（见 `Runtime::owns_profile`），别把同时在跑的另一实例的
//!     profile 删掉。
//!
//! 失败语义：任何一步失败都返回可解释错误，绝不静默降级（AGENTS §11.3）。

use windows::core::PCWSTR;
use windows::Win32::Foundation::{LocalFree, HLOCAL, ERROR_ALREADY_EXISTS};
use windows::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile,
    DeriveAppContainerSidFromAppContainerName,
};
use windows::Win32::Security::{FreeSid, PSID};

use super::windows_impl::wide;

/// 容器 profile 名：稳定、可派生（容器 SID 由名字决定，重启后仍是同一个 SID）。
pub const PROFILE_NAME: &str = "Shaniu.Sandbox.v1";
const PROFILE_DISPLAY: &str = "SHANIU 命令沙箱";
const PROFILE_DESCRIPTION: &str = "SHANIU 命令级 OS 沙箱容器（退出时清理）";

/// 每个 MCP server 一个容器 profile（SH-064 5b）：名字由 server id 派生，
/// 稳定可派生、可按前缀回收，不同 server 的 ACE 互不共享。
///
/// 全小写：Windows 会把 `%LOCALAPPDATA%\Packages` 下的包目录名规范成小写，
/// 前缀比较与孤儿回收都依赖这一点（配置层的 id 本来就只允许 `[a-z0-9-]`）。
pub const MCP_PROFILE_PREFIX: &str = "shaniu.mcp.";
/// 同时存在的 MCP 容器上限：每个 profile 都是真实用户 profile（含包目录与注册记录），
/// 无上限就是「配多少 server 就建多少」，不能接受。
pub const MAX_MCP_PROFILES: usize = 16;

/// `internetClient` 能力 SID（`S-1-15-3-1`）：只有显式授予时才允许出站网络。
pub const INTERNET_CLIENT_SID: &str = "S-1-15-3-1";

/// 容器 SID（或能力 SID）的所有权守卫：按分配方式选 `FreeSid` / `LocalFree`。
pub enum SidMem {
    /// `CreateAppContainerProfile` / `DeriveAppContainerSidFromAppContainerName` / `FreeSid`
    AppContainerSid,
    /// `ConvertStringSidToSidW` / `LocalFree`
    LocalAlloc,
}

pub struct Sid {
    raw: PSID,
    mem: SidMem,
}

// SID 缓冲区在生命周期内不可变，且 Win32 的 SID 操作是线程安全的；
// 这里只为把指针放进进程级 Mutex 而显式声明。
unsafe impl Send for Sid {}
unsafe impl Sync for Sid {}

impl Sid {
    fn new(raw: PSID, mem: SidMem) -> Self {
        Self { raw, mem }
    }

    pub fn as_psid(&self) -> PSID {
        self.raw
    }

    /// 供 `TRUSTEE_W.ptstrName` 使用的裸指针。
    pub fn as_ptr(&self) -> *mut core::ffi::c_void {
        self.raw.0
    }
}

impl Drop for Sid {
    fn drop(&mut self) {
        if self.raw.0.is_null() {
            return;
        }
        unsafe {
            match self.mem {
                SidMem::AppContainerSid => {
                    let _ = FreeSid(self.raw);
                }
                SidMem::LocalAlloc => {
                    let _ = LocalFree(Some(HLOCAL(self.raw.0)));
                }
            }
        }
    }
}

/// 已就绪的 AppContainer：`created` 表示 profile 是本进程创建（因此退出时由本进程删）。
pub struct AppContainer {
    name: String,
    sid: Sid,
    created: bool,
}

impl AppContainer {
    /// 创建或复用生产用容器 profile。
    pub fn ensure() -> Result<Self, String> {
        Self::ensure_named(PROFILE_NAME)
    }

    /// 按名字创建或复用（测试与多 profile 场景）。
    pub fn ensure_named(name: &str) -> Result<Self, String> {
        // 先派生：已存在时最省事，也避免 CreateAppContainerProfile 的 AlreadyExists 噪音。
        match unsafe { derive_sid(name) } {
            Ok(sid) => {
                // profile 注册过但包目录丢了 = 陈旧 profile：CreateProcess 会报
                // ERROR_FILE_NOT_FOUND。删掉重建，别让用户对着一个假存在的容器排错。
                if package_dir(name).is_some_and(|dir| dir.is_dir()) {
                    return Ok(Self { name: name.to_string(), sid, created: false });
                }
                tracing::warn!(
                    profile = name,
                    "发现陈旧 AppContainer profile（包目录缺失），删除后重建"
                );
                let wide_name = wide(name);
                unsafe {
                    let _ = DeleteAppContainerProfile(PCWSTR(wide_name.as_ptr()));
                }
                match unsafe { create_profile(name) } {
                    Ok(sid) => Ok(Self { name: name.to_string(), sid, created: true }),
                    Err(error) => Err(create_failed_message(name, error)),
                }
            }
            Err(_) => match unsafe { create_profile(name) } {
                Ok(sid) => Ok(Self { name: name.to_string(), sid, created: true }),
                Err(error) => {
                    // 竞态：另一线程/实例刚好创建成功。
                    if error == hresult_from_win32(ERROR_ALREADY_EXISTS.0) {
                        let sid = unsafe { derive_sid(name) }.map_err(|e| {
                            format!(
                                "AppContainer profile「{name}」已存在但无法派生 SID：{e}。\
                                 这是陈旧 profile，请删除用户下的容器记录后重试"
                            )
                        })?;
                        Ok(Self { name: name.to_string(), sid, created: false })
                    } else {
                        Err(create_failed_message(name, error))
                    }
                }
            },
        }
    }

    pub fn sid(&self) -> PSID {
        self.sid.as_psid()
    }

    pub fn created(&self) -> bool {
        self.created
    }

    /// 删除容器 profile。失败返回原因，调用方负责记录（退出路径不得因此 panic）。
    pub fn delete(&self) -> Result<(), String> {
        let name = wide(self.name.as_str());
        unsafe { DeleteAppContainerProfile(PCWSTR(name.as_ptr())) }
            .map_err(|e| format!("删除 AppContainer profile「{}」失败: {e}", self.name))
    }
}

/// 只派生容器 SID，**不创建 profile**（后台祖先 ACE 预置用）。
///
/// 后台线程不能走 `ensure()`：它会在 `build_runtime` 之前把 profile 建好，
/// 让运行时的 `created()` 变成 `false`，退出时就把本该由本进程删掉的 profile 留下。
pub fn derived_sid(name: &str) -> Result<Sid, String> {
    unsafe { derive_sid(name) }
}

/// `internetClient` 能力 SID。
/// exec/终端路径恒带（网络不是策略维度）；MCP 路径按 server 的 `allow_network` 决定。
pub fn internet_client_capability() -> Result<Sid, String> {
    capability_sid(INTERNET_CLIENT_SID)
}

/// server id → 容器 profile 名。id 必须满足配置层的 `^[a-z0-9][a-z0-9-]{0,31}$`：
/// profile 名要能被稳定派生与按前缀回收，不能带任意字符。
pub fn mcp_profile_name(server_id: &str) -> Result<String, String> {
    let valid = !server_id.is_empty()
        && server_id.len() <= 32
        && !server_id.starts_with('-')
        && !server_id.ends_with('-')
        && server_id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !valid {
        return Err(format!(
            "MCP server id「{server_id}」不能派生容器 profile 名：\
             只允许 ^[a-z0-9][a-z0-9-]{{0,31}}$（不以 - 开头或结尾）"
        ));
    }
    Ok(format!("{MCP_PROFILE_PREFIX}{server_id}"))
}

/// 本机已有的 MCP 容器 profile 名（`%LOCALAPPDATA%\Packages` 下按前缀扫描）。
pub fn list_mcp_profiles() -> Vec<String> {
    let Some(base) = std::env::var_os("LOCALAPPDATA")
        .map(|base| std::path::PathBuf::from(base).join("Packages"))
    else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(base) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .map(|name| name.to_ascii_lowercase())
        .filter(|name| name.starts_with(MCP_PROFILE_PREFIX))
        .collect();
    names.sort();
    names
}

/// 回收孤儿：删除所有不在 `active` 里的 `Shaniu.Mcp.*` profile。返回删掉的名字。
///
/// **边界**：判定依据是「本进程当前活跃的 server」。若同时跑了另一个 SHANIU 实例，
/// 它正在用的容器会被这里当成孤儿删掉（另一实例的下次启动会重建）。
/// 单实例是既定运行模型（数据目录与信任台账都不支持并发写），这里只记录删除清单。
pub fn reclaim_orphans(active: &std::collections::HashSet<String>) -> Vec<String> {
    let mut deleted = Vec::new();
    for name in list_mcp_profiles() {
        if active.contains(&name) {
            continue;
        }
        let wide_name = wide(&name);
        match unsafe { DeleteAppContainerProfile(PCWSTR(wide_name.as_ptr())) } {
            Ok(()) => deleted.push(name),
            Err(error) => {
                tracing::warn!(profile = %name, %error, "回收孤儿 MCP 容器 profile 失败")
            }
        }
    }
    if !deleted.is_empty() {
        tracing::info!(profiles = ?deleted, "已回收上次遗留的 MCP 容器 profile");
    }
    deleted
}

/// 把 SID 文本解析成能力 SID（测试与其它能力用；调用方负责生命周期）。
pub fn capability_sid(text_sid: &str) -> Result<Sid, String> {
    let text = wide(text_sid);
    let mut sid = PSID(std::ptr::null_mut());
    unsafe { ConvertStringSidToSidW(PCWSTR(text.as_ptr()), &mut sid) }
        .map_err(|e| format!("解析能力 SID {text_sid} 失败: {e}"))?;
    Ok(Sid::new(sid, SidMem::LocalAlloc))
}

unsafe fn derive_sid(name: &str) -> Result<Sid, String> {
    let name = wide(name);
    let sid = unsafe { DeriveAppContainerSidFromAppContainerName(PCWSTR(name.as_ptr())) }
        .map_err(|e| format!("派生容器 SID 失败: {e}"))?;
    Ok(Sid::new(sid, SidMem::AppContainerSid))
}

unsafe fn create_profile(name: &str) -> Result<Sid, i32> {
    let name = wide(name);
    let display = wide(PROFILE_DISPLAY);
    let description = wide(PROFILE_DESCRIPTION);
    unsafe {
        CreateAppContainerProfile(
            PCWSTR(name.as_ptr()),
            PCWSTR(display.as_ptr()),
            PCWSTR(description.as_ptr()),
            // 能力不写在 profile 上：逐次进程创建时按执行路径传入（exec 恒带，MCP 按配置）。
            None,
        )
    }
    .map(|sid| Sid::new(sid, SidMem::AppContainerSid))
    .map_err(|e| e.code().0)
}

fn hresult_from_win32(code: u32) -> i32 {
    (0x8007_0000u32 | (code & 0xFFFF)) as i32
}

/// AppContainer 的包目录位置（`%LOCALAPPDATA%\Packages\<profile 名>`）。
/// 是否存在决定 profile 能不能真正启动进程——注册表里残留但目录丢了就是陈旧 profile。
fn package_dir(name: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(|base| std::path::PathBuf::from(base).join("Packages").join(name))
}

fn create_failed_message(name: &str, error: i32) -> String {
    format!(
        "创建 AppContainer profile「{name}」失败（0x{error:08X}）：\
         当前系统可能不支持 AppContainer。\
         如确需继续，请在配置里显式设置 sandbox.os = \"logical\" \
         （届时 UI 与日志都会标注「当前未启用 OS 沙箱」）"
    )
}

/// 测试互斥：孤儿回收会删除本进程之外的所有 `shaniu.mcp.*` profile，
/// 与其它建容器的测试并行时会互删对方的 profile，必须串行。
///
/// 用 tokio 的 Mutex：同步测试走 `blocking_lock()`，异步测试走 `.lock().await`，
/// 两种测试共用同一把锁（std Mutex 的守卫不能跨 `.await`）。
#[cfg(test)]
pub(crate) static PROFILE_TEST_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internet_client_sid_is_well_known_value() {
        let sid = internet_client_capability().expect("internetClient SID 应可解析");
        assert!(!sid.as_ptr().is_null());
    }

    #[test]
    fn hresult_mapping_matches_facility_win32() {
        // HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS) = 0x800700B7
        assert_eq!(hresult_from_win32(ERROR_ALREADY_EXISTS.0), 0x8007_00B7u32 as i32);
    }

    #[test]
    fn profile_lifecycle_create_reuse_delete() {
        // 与其它建/删 profile 的用例串行：`CreateAppContainerProfile` 会写
        // `%LOCALAPPDATA%\Packages` 与注册表，多个用例并行时陈旧 profile 判定
        // （注册在、目录无）会互相误判成「陈旧」而删掉对方刚建的容器。
        let _serial = PROFILE_TEST_MUTEX.blocking_lock();
        let name = format!("Shaniu.Test.{}.{}", std::process::id(), line!());
        let first = AppContainer::ensure_named(&name).expect("首次应创建 profile");
        assert!(first.created(), "首次 ensure 应标记为创建");
        assert!(!first.sid().0.is_null());

        let second = AppContainer::ensure_named(&name).expect("已存在时应复用");
        assert!(!second.created(), "已存在时不应重复创建");

        first.delete().expect("删除 profile 应成功");
        let third = AppContainer::ensure_named(&name).expect("删除后应能重建");
        assert!(third.created(), "删除后再次 ensure 应重新创建");
        third.delete().expect("清理测试 profile");
    }

    #[test]
    fn mcp_profile_name_derives_from_configured_ids_only() {
        assert_eq!(mcp_profile_name("filesystem").unwrap(), "shaniu.mcp.filesystem");
        assert_eq!(mcp_profile_name("a1-b2").unwrap(), "shaniu.mcp.a1-b2");
        for bad in ["", "UPPER", "a b", "-lead", "trail-", "中文", "a/b"] {
            assert!(mcp_profile_name(bad).is_err(), "应拒绝非法 server id: {bad:?}");
        }
        // 32 字符上限与配置层 `^[a-z0-9][a-z0-9-]{0,31}$` 一致。
        assert!(mcp_profile_name(&"a".repeat(32)).is_ok());
        assert!(mcp_profile_name(&"a".repeat(33)).is_err());
    }

    /// 真实 profile 往返：列得到、回收删得掉，非 `Shaniu.Mcp.` 前缀的不受影响。
    #[test]
    fn mcp_profiles_are_listed_and_reclaimed() {
        let _serial = PROFILE_TEST_MUTEX.blocking_lock();
        let id = format!("probe{}", std::process::id());
        let name = mcp_profile_name(&id).unwrap();
        let container = AppContainer::ensure_named(&name).expect("创建测试容器");
        assert!(
            list_mcp_profiles().contains(&name),
            "新建的 MCP 容器 profile 应能被列出"
        );

        let active: std::collections::HashSet<String> =
            std::collections::HashSet::from([name.clone()]);
        assert!(
            !reclaim_orphans(&active).contains(&name),
            "活跃 profile 不得被当成孤儿回收"
        );

        // 只把本测试的 profile 当孤儿：把其它现存 profile 全列为 active，
        // 避免测试误删开发机上正在运行的容器。
        let keep: std::collections::HashSet<String> = list_mcp_profiles()
            .into_iter()
            .filter(|existing| *existing != name)
            .collect();
        let deleted = reclaim_orphans(&keep);
        assert!(deleted.contains(&name), "不在 active 里的 profile 应被回收");
        assert!(
            !list_mcp_profiles().contains(&name),
            "回收后不应再出现在列表里"
        );
        drop(container);
    }
}
