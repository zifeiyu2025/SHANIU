//! 沙箱策略入口（docs §8 sandbox/mod.rs，ADR-002 三级策略，fail-closed）。
//!
//! 两层机制（用户 2026-11-08 定：AppContainer 打底 + 受限令牌/Job 保留做纵深）：
//!  1. **AppContainer**（`appcontainer`）负责路径/读隔离与网络默认拒绝；
//!  2. **RestrictedToken + JobObject**（`windows_impl`）继续管提权面与进程树/资源。
//!     两层各自可独立验证：探针既验证容器内越界写被拒，也验证受限令牌仍生效。
//!
//! - read-only：纯策略拒绝（不经子进程，双保险）；
//! - workspace-write：容器内启动子进程；`sandbox.os = "logical"` 时**显式**回退到
//!   只有受限令牌 + Job 的逻辑沙箱（UI/日志标注「当前未启用 OS 沙箱」）；
//! - danger-full-access：直接执行（用户明示，UI 全程标识）。
//!
//! fail-closed（FF-4）：任何沙箱准备步骤失败 → Err，**绝不降级为无沙箱执行**。

pub mod acl;
pub mod appcontainer;
pub mod msvc;
pub mod network;
pub mod windows_impl;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use tokio::process::Command;
use tokio::sync::mpsc;

use windows::Win32::Security::PSID;

use crate::config::{AllowedRoot, OsSandbox, SandboxMode, WorkspaceAccess};

pub use windows_impl::{ExecLine, RestrictedStdioChild, StdioEvent};

/// 沙箱错误（文本回填模型/日志；不包含敏感内容）。
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("策略拒绝: {0}")]
    PolicyDenied(String),
    #[error("沙箱创建失败: {0}")]
    CreateFailed(String),
}

/// 准备结果：
/// - Plain：未启动的命令（danger 路径，调用方 tokio spawn）；
/// - Restricted：**已启动**的受限子进程（纯 Win32 wait/kill + Job 守卫，读线程已接 tx）。
pub enum PreparedSpawn {
    Plain(Command),
    Restricted(windows_impl::RestrictedChild),
}

/// 单条命令的沙箱请求。
pub struct SpawnRequest<'a> {
    pub policy: SandboxMode,
    pub workspace_root: &'a Path,
    pub program: &'a str,
    pub args: &'a [&'a str],
    pub cwd: &'a Path,
    pub network: NetworkAccess,
    pub os: OsSandbox,
}

/// 会话级网络授权结果（默认 Deny）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkAccess {
    Deny,
    Allow,
}

/// 进程级 OS 沙箱机制（启动时由 `[sandbox] os` 注入；默认 AppContainer）。
static OS_MODE: AtomicU8 = AtomicU8::new(0);

pub fn set_os_mode(mode: OsSandbox) {
    OS_MODE.store(
        match mode {
            OsSandbox::AppContainer => 0,
            OsSandbox::Logical => 1,
        },
        Ordering::Relaxed,
    );
}

pub fn current_os_mode() -> OsSandbox {
    match OS_MODE.load(Ordering::Relaxed) {
        1 => OsSandbox::Logical,
        _ => OsSandbox::AppContainer,
    }
}

/// 按策略准备子进程（Windows 实现）。tx 供受限路径的读线程回传 stdio 行。
pub fn prepare_spawn(
    policy: SandboxMode,
    program: &str,
    args: &[&str],
    cwd: &Path,
    tx: mpsc::Sender<ExecLine>,
) -> Result<PreparedSpawn, SandboxError> {
    prepare_spawn_with(
        SpawnRequest {
            policy,
            workspace_root: cwd,
            program,
            args,
            cwd,
            // 便捷入口默认断网；生产路径（unified_exec）按会话授权传 Allow。
            network: NetworkAccess::Deny,
            os: current_os_mode(),
        },
        tx,
    )
}

/// 带工作区根与网络策略的完整入口。
pub fn prepare_spawn_with(
    request: SpawnRequest<'_>,
    tx: mpsc::Sender<ExecLine>,
) -> Result<PreparedSpawn, SandboxError> {
    match request.policy {
        // read-only：纯策略拒绝（fail-closed 双保险）
        SandboxMode::ReadOnly => Err(SandboxError::PolicyDenied(
            "只读沙箱禁止命令执行".into(),
        )),
        // danger-full-access：直接执行
        SandboxMode::DangerFullAccess => {
            let mut command = Command::new(request.program);
            command.args(request.args).current_dir(request.cwd);
            Ok(PreparedSpawn::Plain(command))
        }
        SandboxMode::WorkspaceWrite => {
            #[cfg(windows)]
            {
                match request.os {
                    OsSandbox::AppContainer => spawn_in_app_container(&request, tx),
                    OsSandbox::Logical => {
                        warn_logical_fallback_once();
                        let child = windows_impl::spawn_restricted_command(
                            request.program,
                            request.args,
                            request.cwd,
                            tx,
                        )
                        .map_err(SandboxError::CreateFailed)?;
                        Ok(PreparedSpawn::Restricted(child))
                    }
                }
            }
            #[cfg(not(windows))]
            {
                let _ = (request, tx);
                Err(SandboxError::CreateFailed(
                    "workspace-write 沙箱当前仅支持 Windows（fail-closed）".into(),
                ))
            }
        }
    }
}

/// 持久 PTY 终端的启动请求（SH-070）。与一次性命令共用同一套策略与三层边界。
pub struct TerminalRequest<'a> {
    pub policy: SandboxMode,
    pub os: OsSandbox,
    pub network: NetworkAccess,
    pub workspace_root: &'a Path,
    pub program: &'a str,
    pub args: &'a [&'a str],
    pub cwd: &'a Path,
    pub cols: u16,
    pub rows: u16,
}

/// 启动一个持久终端会话（ConPTY）。
///
/// 三个沙箱档与一次性命令一一对应：read-only 拒绝、workspace-write 走
/// AppContainer/Logical、danger-full-access 无令牌无容器。**无论哪档都挂 Job**，
/// 因为 R106 要求「关闭会话即整树终止」，这条不随沙箱档放宽。
pub fn prepare_terminal(
    request: &TerminalRequest<'_>,
) -> Result<windows_impl::TerminalSpawn, SandboxError> {
    let launch = windows_impl::TerminalLaunch {
        program: request.program,
        args: request.args,
        cwd: request.cwd,
        cols: request.cols,
        rows: request.rows,
    };
    match request.policy {
        SandboxMode::ReadOnly => Err(SandboxError::PolicyDenied(
            "只读沙箱禁止打开交互终端".into(),
        )),
        SandboxMode::DangerFullAccess => {
            windows_impl::spawn_unsandboxed_terminal(launch).map_err(SandboxError::CreateFailed)
        }
        SandboxMode::WorkspaceWrite => match request.os {
            OsSandbox::AppContainer => {
                let (sid, capabilities) = app_container_scope(request.workspace_root, request.network)?;
                windows_impl::spawn_appcontainer_terminal(
                    launch,
                    sid,
                    capabilities,
                    msvc::toolchain_env(),
                )
                .map_err(SandboxError::CreateFailed)
            }
            OsSandbox::Logical => {
                warn_logical_fallback_once();
                windows_impl::spawn_restricted_terminal(launch).map_err(SandboxError::CreateFailed)
            }
        },
    }
}

#[cfg(windows)]
fn spawn_in_app_container(
    request: &SpawnRequest<'_>,
    tx: mpsc::Sender<ExecLine>,
) -> Result<PreparedSpawn, SandboxError> {
    let (sid, capabilities) = app_container_scope(request.workspace_root, request.network)?;
    let child = windows_impl::spawn_appcontainer_command(
        request.program,
        request.args,
        request.cwd,
        sid,
        capabilities,
        msvc::toolchain_env(),
        tx,
    )
    .map_err(SandboxError::CreateFailed)?;
    Ok(PreparedSpawn::Restricted(child))
}

/// 容器运行时 + 工作区写 ACE 的公共准备步骤。
/// ACE 只加三类位置：工作区根 / temp / 工具链目录；授权对象是容器 SID。
/// 每个工作区根在进程内只解析一次 scope：重复 exec / 终端不再碰 DACL（docs/04 §1.5）。
#[cfg(windows)]
fn app_container_scope(
    workspace_root: &Path,
    network: NetworkAccess,
) -> Result<(PSID, Vec<PSID>), SandboxError> {
    let mutex = RUNTIME.get_or_init(|| Mutex::new(None));
    let mut guard = mutex
        .lock()
        .map_err(|_| SandboxError::CreateFailed("沙箱运行时锁已中毒".into()))?;
    if guard.is_none() {
        *guard = Some(build_runtime()?);
    }
    let runtime = guard.as_mut().expect("刚初始化的沙箱运行时");

    if !runtime.resolved_roots.contains(workspace_root) {
        let sid = runtime.container.sid();
        for dir in acl::write_scope(workspace_root) {
            with_acl(|acl| acl.grant(&dir, sid, acl::Access::Write))?;
        }
        runtime.resolved_roots.insert(workspace_root.to_path_buf());
    }

    let capabilities = match network {
        NetworkAccess::Allow => vec![runtime.capability_internet.as_psid()],
        NetworkAccess::Deny => Vec::new(),
    };
    Ok((runtime.container.sid(), capabilities))
}

/// 进程内唯一的目录 ACL 管理器。
///
/// exec 容器与每个 MCP 容器共用它，原因有二：
///  1. `recover_pending()` 只能跑一次——两个管理器各自还原会互相把对方刚加的 ACE 抹掉；
///  2. 撤销按 `(路径, SID)` 精确匹配，天然支持「每个 server 一个容器 SID」。
static ACL: OnceLock<Mutex<Option<acl::AclManager>>> = OnceLock::new();
/// 上次异常退出遗留的备份还原条数（用于判断是否需要接管旧 profile）。
static ACL_RECOVERED: AtomicUsize = AtomicUsize::new(0);

/// 懒初始化：只跑一次 `recover_pending`，失败即 fail-closed 返回错误。
fn ensure_acl() -> Result<(), SandboxError> {
    let mutex = ACL.get_or_init(|| Mutex::new(None));
    let mut guard = mutex
        .lock()
        .map_err(|_| SandboxError::CreateFailed("ACL 管理器锁已中毒".into()))?;
    if guard.is_some() {
        return Ok(());
    }
    let data_root = crate::config::resolve_dirs_from_env().data_root;
    let mut manager = acl::AclManager::new(data_root.join("sandbox").join("acl"));
    // 先还原上次异常退出留下的 DACL，再允许新的授权，避免在被污染的 ACL 上继续。
    let restored = manager.recover_pending().map_err(SandboxError::CreateFailed)?;
    ACL_RECOVERED.store(restored, Ordering::Relaxed);
    *guard = Some(manager);
    Ok(())
}

fn with_acl<R>(f: impl FnOnce(&mut acl::AclManager) -> Result<R, String>) -> Result<R, SandboxError> {
    ensure_acl()?;
    let mutex = ACL.get().expect("ACL 已初始化");
    let mut guard = mutex
        .lock()
        .map_err(|_| SandboxError::CreateFailed("ACL 管理器锁已中毒".into()))?;
    let manager = guard
        .as_mut()
        .ok_or_else(|| SandboxError::CreateFailed("ACL 管理器未初始化".into()))?;
    f(manager).map_err(SandboxError::CreateFailed)
}

struct Runtime {
    container: appcontainer::AppContainer,
    capability_internet: appcontainer::Sid,
    /// 已经解析过写权限 scope 的工作区根：同一进程内重复 exec 不再碰 DACL。
    resolved_roots: HashSet<PathBuf>,
    /// 退出时是否删除 profile：本进程创建，或接管了上次异常退出留下的备份。
    owns_profile: bool,
}

static RUNTIME: OnceLock<Mutex<Option<Runtime>>> = OnceLock::new();

fn build_runtime() -> Result<Runtime, SandboxError> {
    ensure_acl()?;
    let recovered = ACL_RECOVERED.load(Ordering::Relaxed);

    let container =
        appcontainer::AppContainer::ensure().map_err(SandboxError::CreateFailed)?;
    let capability_internet = appcontainer::internet_client_capability()
        .map_err(SandboxError::CreateFailed)?;
    let owns_profile = container.created() || recovered > 0;
    Ok(Runtime {
        container,
        capability_internet,
        resolved_roots: HashSet::new(),
        owns_profile,
    })
}

/// 进程退出时调用：先还原 DACL，再删除自己创建的容器 profile。
pub fn shutdown() {
    release_all_mcp_containers();
    // 不论有没有 exec 运行时，只要动过 DACL 就要还原：MCP 容器也会改目录权限。
    if ACL.get().is_some() {
        match with_acl(|acl| acl.restore_all()) {
            Ok(restored) => {
                tracing::info!(restored, "沙箱退出：目录 DACL 已按备份还原");
            }
            Err(error) => {
                tracing::error!(%error, "沙箱退出：目录 DACL 还原失败，需人工检查备份目录");
            }
        }
    }
    let Some(mutex) = RUNTIME.get() else {
        return;
    };
    let Ok(mut guard) = mutex.lock() else {
        tracing::error!("沙箱退出清理：运行时锁已中毒，DACL 可能未还原");
        return;
    };
    let Some(runtime) = guard.take() else {
        return;
    };
    if runtime.owns_profile {
        match runtime.container.delete() {
            Ok(()) => tracing::info!("沙箱退出：AppContainer profile 已删除"),
            Err(error) => tracing::error!(%error, "沙箱退出：AppContainer profile 删除失败"),
        }
    }
}

// ---------------------------------------------------------------------------
// 每个 MCP server 一个容器（SH-064 5b）
// ---------------------------------------------------------------------------

/// 单个 MCP server 的容器与其 ACE 台账。
struct McpRuntime {
    container: appcontainer::AppContainer,
    capability_internet: appcontainer::Sid,
    /// 本进程给这个容器的 SID 加过的 ACE：释放时按 `(路径, SID)` 精确撤销。
    granted: Vec<(PathBuf, acl::Access)>,
}

static MCP_RUNTIMES: OnceLock<Mutex<HashMap<String, McpRuntime>>> = OnceLock::new();

fn mcp_runtimes() -> &'static Mutex<HashMap<String, McpRuntime>> {
    MCP_RUNTIMES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn mcp_lock() -> Result<std::sync::MutexGuard<'static, HashMap<String, McpRuntime>>, SandboxError>
{
    mcp_runtimes()
        .lock()
        .map_err(|_| SandboxError::CreateFailed("MCP 容器锁已中毒".into()))
}

/// 建立（或复用）某个 server 的容器 profile。
///
/// 数量上限与孤儿回收都在这里做：本进程首次接管容器时，`Shaniu.Mcp.*` 里
/// 不属于当前 server 的都是上次遗留的孤儿（见 `appcontainer::reclaim_orphans` 的边界）。
fn ensure_mcp_container(server_id: &str) -> Result<(), SandboxError> {
    let profile = appcontainer::mcp_profile_name(server_id).map_err(SandboxError::CreateFailed)?;
    let mut runtimes = mcp_lock()?;
    if runtimes.contains_key(server_id) {
        return Ok(());
    }
    if runtimes.is_empty() {
        let active: HashSet<String> = HashSet::from([profile.clone()]);
        appcontainer::reclaim_orphans(&active);
    }
    let existing = appcontainer::list_mcp_profiles();
    if !existing.contains(&profile) && existing.len() >= appcontainer::MAX_MCP_PROFILES {
        return Err(SandboxError::CreateFailed(format!(
            "已有 {} 个 MCP 容器 profile，达到上限 {}：请先撤销不再使用的 server 信任\
             （会同时删除其容器 profile）再连接新的 server",
            existing.len(),
            appcontainer::MAX_MCP_PROFILES
        )));
    }
    let container =
        appcontainer::AppContainer::ensure_named(&profile).map_err(SandboxError::CreateFailed)?;
    let capability_internet =
        appcontainer::internet_client_capability().map_err(SandboxError::CreateFailed)?;
    runtimes.insert(
        server_id.to_string(),
        McpRuntime { container, capability_internet, granted: Vec::new() },
    );
    Ok(())
}

fn grant_mcp_paths(
    server_id: &str,
    desired: &[(PathBuf, acl::Access)],
) -> Result<(), SandboxError> {
    let mut runtimes = mcp_lock()?;
    let runtime = runtimes.get_mut(server_id).ok_or_else(|| {
        SandboxError::CreateFailed(format!("MCP server {server_id} 的容器尚未建立"))
    })?;
    let sid = runtime.container.sid();
    for (path, access) in desired {
        if runtime.granted.iter().any(|(granted, level)| granted == path && level == access) {
            continue;
        }
        with_acl(|acl| acl.grant(path, sid, *access)).map_err(|error| {
            // 常见根因：运行时装在管理员所有的目录（普通用户改不了它的 DACL，
            // 容器也没读到它的权限）。给出能照做的两个出口。
            let detail = match &error {
                SandboxError::CreateFailed(message) => message.clone(),
                other => other.to_string(),
            };
            SandboxError::CreateFailed(format!(
                "{detail}。若该目录或运行时由管理员安装，容器读不到它：\
                 请把运行时装到用户可写目录，或在配置里为这个 server 显式设置 \
                 unsandboxed = true 并填写原因（逐服务器豁免）"
            ))
        })?;
        runtime.granted.push((path.clone(), *access));
    }
    Ok(())
}

/// 释放某个 server 的容器：撤销本进程加过的全部 ACE，再删除 profile。
///
/// 调用点：停止 / 撤信 / 卸载 / 进程退出。失败只记录（退出路径不许 panic），
/// 下次启动的孤儿回收会兜底。
pub fn release_mcp_server(server_id: &str) {
    let Some(mutex) = MCP_RUNTIMES.get() else {
        return;
    };
    let Ok(mut runtimes) = mutex.lock() else {
        tracing::error!(server_id, "释放 MCP 容器：锁已中毒，ACE 与 profile 可能残留");
        return;
    };
    let Some(runtime) = runtimes.remove(server_id) else {
        return;
    };
    revoke_and_delete(runtime);
}

fn release_all_mcp_containers() {
    let Some(mutex) = MCP_RUNTIMES.get() else {
        return;
    };
    let Ok(mut runtimes) = mutex.lock() else {
        tracing::error!("释放全部 MCP 容器：锁已中毒，ACE 与 profile 可能残留");
        return;
    };
    for (_, runtime) in runtimes.drain() {
        revoke_and_delete(runtime);
    }
}

fn revoke_and_delete(runtime: McpRuntime) {
    let sid = runtime.container.sid();
    for (path, _) in &runtime.granted {
        if let Err(error) = with_acl(|acl| acl.revoke(path, sid)) {
            tracing::error!(
                %error,
                path = %path.display(),
                "撤销 MCP 容器 ACE 失败（下次启动的备份还原会兜底）"
            );
        }
    }
    match runtime.container.delete() {
        Ok(()) => tracing::info!("MCP 容器 profile 已删除"),
        Err(error) => tracing::error!(%error, "MCP 容器 profile 删除失败"),
    }
}

/// 供 UI 展示的沙箱状态（Tauri 命令用）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SandboxStatus {
    pub mode: &'static str,
    pub os: &'static str,
    /// 是否真的启用了 OS 级隔离；false 时 UI 必须常驻标注。
    pub os_enabled: bool,
    /// 容器 profile 名（UI 常驻展示"跑的到底是哪个容器"）；logical 模式为空串。
    pub profile: String,
    pub note: String,
}

pub fn status(mode: SandboxMode, os: OsSandbox) -> SandboxStatus {
    let (os_name, os_enabled, note) = match os {
        OsSandbox::AppContainer => (
            "app_container",
            true,
            "AppContainer 路径/读隔离 + 网络默认拒绝；受限令牌与 Job 作为纵深保留",
        ),
        OsSandbox::Logical => (
            "logical",
            false,
            "当前未启用 OS 沙箱：仅受限令牌 + Job（逻辑隔离），没有路径级写入边界",
        ),
    };
    SandboxStatus {
        mode: match mode {
            SandboxMode::ReadOnly => "read_only",
            SandboxMode::WorkspaceWrite => "workspace_write",
            SandboxMode::DangerFullAccess => "danger_full_access",
        },
        os: os_name,
        os_enabled,
        profile: match os {
            OsSandbox::AppContainer => appcontainer::PROFILE_NAME.to_string(),
            OsSandbox::Logical => String::new(),
        },
        note: note.to_string(),
    }
}

fn warn_logical_fallback_once() {
    static WARNED: OnceLock<()> = OnceLock::new();
    WARNED.get_or_init(|| {
        tracing::warn!(
            "当前未启用 OS 沙箱（sandbox.os = \"logical\"，用户显式选择）：\
             命令只有受限令牌 + Job 的逻辑隔离，没有路径级写入边界"
        );
    });
}

/// 启动一个 MCP stdio server 的请求（SH-060 / SH-064）。
pub struct StdioRequest<'a> {
    pub policy: SandboxMode,
    /// 配置里的 server id（派生容器 profile 名与临时目录）。
    pub server_id: &'a str,
    pub program: &'a str,
    pub args: &'a [&'a str],
    pub cwd: &'a Path,
    /// 会话工作区根（`workspace_access` 作用于此）。
    pub workspace_root: &'a Path,
    pub workspace_access: WorkspaceAccess,
    /// 用户逐条勾选的目录授权（Q2）。
    pub allowed_roots: &'a [AllowedRoot],
    pub allow_network: bool,
    pub env: &'a BTreeMap<String, String>,
}

/// 按策略准备 MCP stdio server（SH-060，X10：默认一律走沙箱）。
///
/// - read-only：纯策略拒绝（只读沙箱不允许任何进程执行）；
/// - workspace-write + `sandbox.os = "app_container"`：**逐 server 一个容器**，
///   ACE 按 server 的 cwd/allowed_roots 逐条授权，网络默认拒绝；
/// - workspace-write + `sandbox.os = "logical"`：受限令牌 + Job（用户显式选择）；
/// - danger-full-access：**调用方必须先确认该 server 的 `unsandboxed=true` 且原因已记录**
///   （逐服务器显式豁免），这里才走未沙箱启动。
///
/// 失败语义：容器/授权任何一步失败都返回 Err，调用方必须把该 server 置为 `blocked`。
/// **绝不回退到受限令牌**——那会让用户以为它在容器里，实际没有路径边界。
pub fn prepare_stdio(
    request: StdioRequest<'_>,
    tx: mpsc::Sender<StdioEvent>,
) -> Result<windows_impl::RestrictedStdioChild, SandboxError> {
    match request.policy {
        SandboxMode::ReadOnly => Err(SandboxError::PolicyDenied(
            "只读沙箱禁止启动 MCP server 进程".into(),
        )),
        SandboxMode::DangerFullAccess => {
            windows_impl::spawn_unsandboxed_stdio(
                request.program,
                request.args,
                request.cwd,
                request.env,
                tx,
            )
            .map_err(SandboxError::CreateFailed)
        }
        SandboxMode::WorkspaceWrite => match current_os_mode() {
            OsSandbox::Logical => {
                warn_logical_fallback_once();
                windows_impl::spawn_restricted_stdio(
                    request.program,
                    request.args,
                    request.cwd,
                    request.env,
                    tx,
                )
                .map_err(SandboxError::CreateFailed)
            }
            OsSandbox::AppContainer => spawn_mcp_in_app_container(&request, tx),
        },
    }
}

#[cfg(windows)]
fn spawn_mcp_in_app_container(
    request: &StdioRequest<'_>,
    tx: mpsc::Sender<StdioEvent>,
) -> Result<windows_impl::RestrictedStdioChild, SandboxError> {
    ensure_mcp_container(request.server_id)?;

    let data_root = crate::config::resolve_dirs_from_env().data_root;
    let server_temp = data_root
        .join("sandbox")
        .join("mcp-temp")
        .join(request.server_id);
    std::fs::create_dir_all(&server_temp).map_err(|error| {
        SandboxError::CreateFailed(format!(
            "创建 MCP 临时目录 {} 失败: {error}",
            server_temp.display()
        ))
    })?;

    let workspace_access = acl_access(request.workspace_access);
    let mut desired: Vec<(PathBuf, acl::Access)> = vec![
        // 工作区按用户所选档位；cwd 单独列出（可能在工作区之外）。
        (request.workspace_root.to_path_buf(), workspace_access),
        (request.cwd.to_path_buf(), workspace_access),
        // 临时目录必须可写：TEMP/TMP 会在下面改指到这里。
        (server_temp.clone(), acl::Access::Write),
    ];
    for root in request.allowed_roots {
        desired.push((PathBuf::from(root.path.trim()), acl_access(root.access)));
    }
    for root in toolchain_roots(request.program, request.args) {
        desired.push((root, acl::Access::Read));
    }
    let desired = acl::collapse(desired);
    grant_mcp_paths(request.server_id, &desired)?;

    let mut env = request.env.clone();
    let temp_text = server_temp.to_string_lossy().to_string();
    env.insert("TEMP".to_string(), temp_text.clone());
    env.insert("TMP".to_string(), temp_text);
    add_node_symlink_flags(&mut env, request.program, &server_temp)?;

    let (sid, internet) = {
        let runtimes = mcp_lock()?;
        let runtime = runtimes.get(request.server_id).ok_or_else(|| {
            SandboxError::CreateFailed(format!(
                "MCP server {} 的容器尚未建立",
                request.server_id
            ))
        })?;
        (runtime.container.sid(), runtime.capability_internet.as_psid())
    };
    let capabilities = if request.allow_network { vec![internet] } else { Vec::new() };
    windows_impl::spawn_appcontainer_stdio(
        request.program,
        request.args,
        request.cwd,
        &env,
        sid,
        &capabilities,
        tx,
    )
    .map_err(SandboxError::CreateFailed)
}

#[cfg(not(windows))]
fn spawn_mcp_in_app_container(
    _request: &StdioRequest<'_>,
    _tx: mpsc::Sender<StdioEvent>,
) -> Result<windows_impl::RestrictedStdioChild, SandboxError> {
    Err(SandboxError::CreateFailed(
        "workspace-write 沙箱当前仅支持 Windows（fail-closed）".into(),
    ))
}

fn acl_access(access: WorkspaceAccess) -> acl::Access {
    match access {
        WorkspaceAccess::Read => acl::Access::Read,
        WorkspaceAccess::Write => acl::Access::Write,
    }
}

/// AppContainer 里跑 node 的 loader 需要额外开关。
///
/// node 的 `fs.realpathSync`（loader 用的 JS 版本）会逐级 lstat 到**盘符根**；AppContainer
/// 令牌对本机数据盘根（如 `D:\`，归 SYSTEM 所有）没有读属性权限，非提权也改不了它的
/// DACL，于是 entry 解析直接 EPERM（`lstat 'D:\'`）。MXC/codex 的做法是给卷根只读授权，
/// 但那需要提权；非提权下用官方开关让 loader 跳过 realpath，再由预加载 shim 兜住
/// user code 里对 `fs.realpathSync` 的直接调用（如 server-filesystem 的 `isEntryPoint`）。
/// 只改模块路径缓存/身份语义，不放松文件系统边界：真正读写在目标路径上仍按 ACL 判定。
#[cfg(windows)]
const NODE_PRELOAD_SHIM: &str = include_str!("../../assets/sandbox/node-preload.cjs");

#[cfg(windows)]
fn add_node_symlink_flags(
    env: &mut BTreeMap<String, String>,
    program: &str,
    shim_dir: &Path,
) -> Result<(), SandboxError> {
    let stem = Path::new(program)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default();
    if !stem.eq_ignore_ascii_case("node") && !stem.eq_ignore_ascii_case("nodejs") {
        return Ok(());
    }
    // 逐 server 落一份 shim（server_temp 已可写/可读，无需额外授权）。
    let preload = shim_dir.join("node-preload.cjs");
    std::fs::write(&preload, NODE_PRELOAD_SHIM).map_err(|error| {
        SandboxError::CreateFailed(format!(
            "写入 node 兼容 shim {} 失败: {error}",
            preload.display()
        ))
    })?;
    let existing_key = env
        .keys()
        .find(|key| key.eq_ignore_ascii_case("NODE_OPTIONS"))
        .cloned();
    let mut options = existing_key
        .as_ref()
        .and_then(|key| env.remove(key))
        .unwrap_or_default();
    for flag in ["--preserve-symlinks", "--preserve-symlinks-main"] {
        if !options.split_whitespace().any(|token| token == flag) {
            if !options.trim().is_empty() {
                options.push(' ');
            }
            options.push_str(flag);
        }
    }
    // node 解析 NODE_OPTIONS 时把反斜杠当转义符，路径必须用正斜杠并加引号。
    let require_flag = format!(
        "--require \"{}\"",
        preload.to_string_lossy().replace('\\', "/")
    );
    if !options.contains(&require_flag) {
        if !options.trim().is_empty() {
            options.push(' ');
        }
        options.push_str(&require_flag);
    }
    env.insert("NODE_OPTIONS".to_string(), options);
    Ok(())
}

/// 容器内启动 server 需要的**只读**工具链目录：可执行文件所在目录、venv 根、
/// 解释器型启动时代码包根。没有它们，server 连自己的运行时与模块都读不到。
///
/// 系统自有目录（`%SystemRoot%`、`Program Files`、`ProgramData`）**不授权**：
/// AppContainer 对它们本来就有读权限，而给它们加 ACE 需要管理员权限，
/// 强行加只会让连接以「拒绝访问（Win32 错误 5）」失败。
fn toolchain_roots(program: &str, args: &[&str]) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let exe = Path::new(program);
    if let Some(parent) = exe.parent().filter(|parent| parent.is_dir()) {
        if !is_os_owned(parent) {
            roots.push(parent.to_path_buf());
        }
        if let Some(venv) = find_ancestor(parent, "pyvenv.cfg") {
            if !is_os_owned(&venv) {
                roots.push(venv.clone());
            }
            // venv 的 python.exe 只是壳：stdlib/标准库在 `pyvenv.cfg` 的 home 指向的
            // 基础解释器目录里。只授权 venv 会让 python 启动阶段就 EPERM 退出。
            if let Some(home) = venv_runtime_home(&venv) {
                if !is_os_owned(&home) {
                    roots.push(home);
                }
            }
        }
    }
    // 解释器型启动（`python <script.py>` / `node <entry.js>`）：参数里的现存文件
    // 是真正的代码。向上找 `package.json` 得到包根，找不到就退到它的父目录。
    for arg in args {
        let candidate = Path::new(arg);
        if !candidate.is_absolute() || !candidate.is_file() {
            continue;
        }
        let Some(dir) = candidate.parent() else {
            continue;
        };
        let root = find_ancestor(dir, "package.json").unwrap_or_else(|| dir.to_path_buf());
        if !is_os_owned(&root) {
            roots.push(root);
        }
        // node 的依赖解析走 `node_modules` 链：entry 自己的包根读得到还不够，
        // npm `--prefix` 安装时依赖被提升到安装前缀下的 `node_modules`，那里也必须
        // 可读，否则 server 起得来但 import 依赖时 EPERM。
        if let Some(prefix) = find_node_modules_prefix(dir) {
            if !is_os_owned(&prefix) {
                roots.push(prefix);
            }
        }
    }
    roots
}

/// 从 entry 所在目录向上找 `node_modules`，返回它的父目录（= 安装前缀）。
fn find_node_modules_prefix(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        if dir
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("node_modules"))
        {
            return dir.parent().map(Path::to_path_buf);
        }
        current = dir.parent();
    }
    None
}

/// 是否是 OS 自有目录（AppContainer 默认可读，且加 ACE 会被拒绝访问）。
fn is_os_owned(dir: &Path) -> bool {
    if dir.parent().is_none() {
        return true;
    }
    // Windows 路径比较不区分大小写，环境变量与解析出的 exe 路径大小写常不一致。
    let text = dir.to_string_lossy().to_lowercase();
    ["SystemRoot", "ProgramFiles", "ProgramFiles(x86)", "ProgramData"]
        .iter()
        .filter_map(std::env::var_os)
        .map(|owned| owned.to_string_lossy().to_lowercase())
        .any(|owned| !owned.is_empty() && text.starts_with(&owned))
}

fn find_ancestor(start: &Path, marker: &str) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        if dir.join(marker).exists() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

/// 读 `pyvenv.cfg` 的 `home = ...`：venv 里 python 的 stdlib 实际装在这里。
fn venv_runtime_home(venv: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(venv.join("pyvenv.cfg")).ok()?;
    text.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim().eq_ignore_ascii_case("home")).then(|| PathBuf::from(value.trim()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn ws() -> crate::test_support::TestTempDir {
        // 自删临时目录：panic 也会清（2026-11-09 夹具规矩）。
        crate::test_support::TestTempDir::new("sb")
    }

    #[test]
    fn readonly_policy_fails_closed() {
        let (tx, _rx) = mpsc::channel(4);
        let err = match prepare_spawn(SandboxMode::ReadOnly, "cmd", &["/C", "echo"], &ws(), tx) {
            Err(e) => e,
            Ok(_) => panic!("readonly 应拒绝命令执行"),
        };
        assert!(matches!(err, SandboxError::PolicyDenied(_)));
    }

    #[test]
    fn danger_policy_yields_plain_command() {
        let dir = ws();
        let (tx, _rx) = mpsc::channel(4);
        let spawn = match prepare_spawn(SandboxMode::DangerFullAccess, "cmd", &["/C", "echo"], &dir, tx) {
            Ok(PreparedSpawn::Plain(command)) => command,
            _ => panic!("danger 应为 Plain"),
        };
        drop(spawn);
    }

    #[test]
    fn readonly_policy_denies_mcp_stdio() {
        let (tx, _rx) = mpsc::channel(4);
        let dir = ws();
        let err = match prepare_stdio(
            StdioRequest {
                policy: SandboxMode::ReadOnly,
                server_id: "probe",
                program: "cmd",
                args: &["/C", "echo"],
                cwd: &dir,
                workspace_root: &dir,
                workspace_access: WorkspaceAccess::Read,
                allowed_roots: &[],
                allow_network: false,
                env: &BTreeMap::new(),
            },
            tx,
        ) {
            Err(error) => error,
            Ok(_) => panic!("readonly 应拒绝启动 MCP server"),
        };
        assert!(matches!(err, SandboxError::PolicyDenied(_)));
    }

    #[test]
    fn status_marks_logical_fallback_as_os_disabled() {
        let logical = status(SandboxMode::WorkspaceWrite, OsSandbox::Logical);
        assert!(!logical.os_enabled);
        assert!(logical.note.contains("当前未启用 OS 沙箱"));
        let container = status(SandboxMode::WorkspaceWrite, OsSandbox::AppContainer);
        assert!(container.os_enabled);
        assert_eq!(container.os, "app_container");
    }

    /// 非法 server id 派生不出容器名：必须**直接失败**，不能悄悄退回受限令牌。
    #[cfg(windows)]
    #[test]
    fn invalid_server_id_fails_closed_instead_of_token_fallback() {
        set_os_mode(OsSandbox::AppContainer);
        let dir = ws();
        let (tx, _rx) = mpsc::channel(4);
        let err = match prepare_stdio(
            StdioRequest {
                policy: SandboxMode::WorkspaceWrite,
                server_id: "Bad Id",
                program: "cmd",
                args: &["/C", "echo"],
                cwd: &dir,
                workspace_root: &dir,
                workspace_access: WorkspaceAccess::Read,
                allowed_roots: &[],
                allow_network: false,
                env: &BTreeMap::new(),
            },
            tx,
        ) {
            Err(error) => error,
            Ok(_) => panic!("非法 id 必须拒绝启动，而不是回退受限令牌"),
        };
        assert!(matches!(err, SandboxError::CreateFailed(_)), "实际: {err}");
        assert!(err.to_string().contains("profile"), "错误应指清是容器名派生失败: {err}");
    }

    /// 每个 server 一个容器：建立后可释放，释放时 profile 一并删除。
    #[cfg(windows)]
    #[test]
    fn mcp_container_is_per_server_and_deleted_on_release() {
        let _serial = appcontainer::PROFILE_TEST_MUTEX.blocking_lock();
        let id = format!("release-{}", std::process::id());
        ensure_mcp_container(&id).expect("应能建立 per-server 容器");
        {
            let runtimes = mcp_lock().unwrap();
            let runtime = runtimes.get(&id).expect("容器应已登记");
            assert!(!runtime.container.sid().0.is_null());
        }
        let profile = appcontainer::mcp_profile_name(&id).unwrap();
        assert!(
            appcontainer::list_mcp_profiles().contains(&profile),
            "容器 profile 应真实存在"
        );

        release_mcp_server(&id);
        assert!(!mcp_lock().unwrap().contains_key(&id), "释放后不应还登记着");
        assert!(
            !appcontainer::list_mcp_profiles().contains(&profile),
            "释放后 profile 应被删除"
        );
    }

    /// 每 server 的目录授权：ACE 加在该 server 自己的容器 SID 上，释放时按
    /// `(路径, SID)` 精确撤销，并且释放本身幂等。
    #[cfg(windows)]
    #[test]
    fn mcp_acl_grant_is_exact_and_release_revokes_it() {
        let _serial = appcontainer::PROFILE_TEST_MUTEX.blocking_lock();
        set_os_mode(OsSandbox::AppContainer);
        let dir = ws();
        let target = dir.join("granted");
        std::fs::create_dir_all(&target).unwrap();
        let id = format!("acl-{}", std::process::id());
        ensure_mcp_container(&id).expect("建立 per-server 容器");
        let sid = {
            let runtimes = mcp_lock().unwrap();
            runtimes.get(&id).expect("容器应已登记").container.sid()
        };
        let profile = appcontainer::mcp_profile_name(&id).unwrap();

        grant_mcp_paths(&id, &[(target.clone(), acl::Access::Read)]).expect("授权读");
        assert!(
            acl::has_explicit_grant(&target, sid, acl::Access::Read.mask()).unwrap(),
            "容器 SID 应有显式读 ACE"
        );
        // 幂等：重复授权不报错，ACE 仍在。
        grant_mcp_paths(&id, &[(target.clone(), acl::Access::Read)]).expect("重复授权应幂等");
        assert!(acl::has_explicit_grant(&target, sid, acl::Access::Read.mask()).unwrap());

        release_mcp_server(&id);
        assert!(
            !acl::has_explicit_grant(&target, sid, acl::Access::Read.mask()).unwrap(),
            "释放后必须精确撤销该容器 SID 的 ACE"
        );
        assert!(
            !appcontainer::list_mcp_profiles().contains(&profile),
            "释放后 profile 应被删除"
        );
        // 重复释放是 no-op，不得 panic。
        release_mcp_server(&id);
    }

    /// 真机：容器内启动 cmd，并读取工作区里的文件（验证 ACE 真的生效）。
    #[cfg(windows)]
    #[test]
    fn mcp_server_runs_inside_container_and_reads_workspace() {
        let _serial = appcontainer::PROFILE_TEST_MUTEX.blocking_lock();
        set_os_mode(OsSandbox::AppContainer);
        let dir = ws();
        let marker = dir.join("marker.txt");
        std::fs::write(&marker, "container-read-ok").unwrap();
        let id = format!("spawn-{}", std::process::id());
        let program = "cmd".to_string();
        let marker_arg = marker.to_string_lossy().to_string();
        let args = ["/C", "type", marker_arg.as_str()];

        let (tx, mut rx) = mpsc::channel(64);
        let mut child = prepare_stdio(
            StdioRequest {
                policy: SandboxMode::WorkspaceWrite,
                server_id: &id,
                program: &program,
                args: &args,
                cwd: &dir,
                workspace_root: &dir,
                workspace_access: WorkspaceAccess::Write,
                allowed_roots: &[],
                allow_network: false,
                env: &BTreeMap::new(),
            },
            tx,
        )
        .expect("容器内应能启动 cmd");

        // 用独立 runtime 阻塞等待，避免把同步锁守卫跨 `.await` 持有。
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("测试用 runtime");
        let code = runtime.block_on(child.wait()).unwrap();
        assert_eq!(code, 0, "容器内 type 应成功");
        let mut output = String::new();
        while let Ok(event) = rx.try_recv() {
            if let StdioEvent::Line(line) = event {
                output.push_str(&line);
            }
        }
        assert!(output.contains("container-read-ok"), "容器内应能读到工作区文件，实际: {output}");
        release_mcp_server(&id);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn workspace_write_runs_sandboxed_and_succeeds() {
        // 真实 Win32 沙箱 smoke：受限令牌 + Job + AppContainer 下运行 echo，退出码必须为 0
        let dir = ws();
        let (tx, mut rx) = mpsc::channel(64);
        let mut child = match prepare_spawn(SandboxMode::WorkspaceWrite, "cmd", &["/C", "echo sb-ok"], &dir, tx)
        {
            Ok(PreparedSpawn::Restricted(child)) => child,
            _ => panic!("workspace-write 应为 Restricted"),
        };
        let code = child.wait().await.unwrap();
        assert_eq!(code, 0);
        // 排干读线程输出（echo sb-ok 应出现）
        let mut found = false;
        while let Ok(line) = rx.try_recv() {
            if line.1.contains("sb-ok") {
                found = true;
                break;
            }
        }
        assert!(found, "沙箱化子进程 stdout 应含输出");
    }
}
