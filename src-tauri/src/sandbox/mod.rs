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
pub mod network;
pub mod windows_impl;

use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

use tokio::process::Command;
use tokio::sync::mpsc;

use crate::config::{OsSandbox, SandboxMode};

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

#[cfg(windows)]
fn spawn_in_app_container(
    request: &SpawnRequest<'_>,
    tx: mpsc::Sender<ExecLine>,
) -> Result<PreparedSpawn, SandboxError> {
    let mutex = RUNTIME.get_or_init(|| Mutex::new(None));
    let mut guard = mutex
        .lock()
        .map_err(|_| SandboxError::CreateFailed("沙箱运行时锁已中毒".into()))?;
    if guard.is_none() {
        *guard = Some(build_runtime()?);
    }
    let runtime = guard.as_mut().expect("刚初始化的沙箱运行时");

    // ACE 只加三类位置：工作区根 / temp / 工具链目录；授权对象是容器 SID。
    // 每个工作区根在进程内只解析一次 scope：重复 exec 不再碰 DACL（见 docs/04 §1.5）。
    if !runtime.resolved_roots.contains(request.workspace_root) {
        for dir in acl::write_scope(request.workspace_root) {
            runtime
                .acl
                .grant(&dir, runtime.container.sid(), acl::Access::Write)
                .map_err(SandboxError::CreateFailed)?;
        }
        runtime.resolved_roots.insert(request.workspace_root.to_path_buf());
    }

    let capabilities = match request.network {
        NetworkAccess::Allow => vec![runtime.capability_internet.as_psid()],
        NetworkAccess::Deny => Vec::new(),
    };
    let child = windows_impl::spawn_appcontainer_command(
        request.program,
        request.args,
        request.cwd,
        runtime.container.sid(),
        capabilities,
        tx,
    )
    .map_err(SandboxError::CreateFailed)?;
    Ok(PreparedSpawn::Restricted(child))
}

struct Runtime {
    container: appcontainer::AppContainer,
    acl: acl::AclManager,
    capability_internet: appcontainer::Sid,
    /// 已经解析过写权限 scope 的工作区根：同一进程内重复 exec 不再碰 DACL。
    resolved_roots: std::collections::HashSet<std::path::PathBuf>,
    /// 退出时是否删除 profile：本进程创建，或接管了上次异常退出留下的备份。
    owns_profile: bool,
}

static RUNTIME: OnceLock<Mutex<Option<Runtime>>> = OnceLock::new();

fn build_runtime() -> Result<Runtime, SandboxError> {
    let data_root = crate::config::resolve_dirs_from_env().data_root;
    let mut acl = acl::AclManager::new(data_root.join("sandbox").join("acl"));

    // 先还原上次异常退出留下的 DACL，再建/复用 profile，避免在被污染的 ACL 上继续。
    acl.recover_pending().map_err(SandboxError::CreateFailed)?;
    let recovered = acl.recovered_stale();

    let container =
        appcontainer::AppContainer::ensure().map_err(SandboxError::CreateFailed)?;
    let capability_internet = appcontainer::internet_client_capability()
        .map_err(SandboxError::CreateFailed)?;
    let owns_profile = container.created() || recovered > 0;
    Ok(Runtime {
        container,
        acl,
        capability_internet,
        resolved_roots: std::collections::HashSet::new(),
        owns_profile,
    })
}

/// 进程退出时调用：先还原 DACL，再删除自己创建的容器 profile。
pub fn shutdown() {
    let Some(mutex) = RUNTIME.get() else {
        return;
    };
    let Ok(mut guard) = mutex.lock() else {
        tracing::error!("沙箱退出清理：运行时锁已中毒，DACL 可能未还原");
        return;
    };
    let Some(mut runtime) = guard.take() else {
        return;
    };
    match runtime.acl.restore_all() {
        Ok(restored) => {
            tracing::info!(restored, "沙箱退出：目录 DACL 已按备份还原");
        }
        Err(error) => {
            tracing::error!(%error, "沙箱退出：目录 DACL 还原失败，需人工检查备份目录");
        }
    }
    if runtime.owns_profile {
        match runtime.container.delete() {
            Ok(()) => tracing::info!("沙箱退出：AppContainer profile 已删除"),
            Err(error) => tracing::error!(%error, "沙箱退出：AppContainer profile 删除失败"),
        }
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

/// 按策略准备 MCP stdio server（SH-060，X10：默认一律走沙箱）。
///
/// - read-only：纯策略拒绝（只读沙箱不允许任何进程执行）；
/// - workspace-write：受限令牌 + Job + 句柄白名单（默认路径）；
/// - danger-full-access：**调用方必须先确认该 server 的 `unsandboxed=true` 且原因已记录**
///   （逐服务器显式豁免），这里才走未沙箱启动。
///
/// **已知边界（SH-021 注）**：MCP stdio 仍是逻辑沙箱，未套 AppContainer——
/// 其二进制定位在工作区/temp/工具链目录之外，套容器会先把 server 自己挡住。
/// 该降级在 `status()` 与日志中标出，等后续卡处理。
pub fn prepare_stdio(
    policy: SandboxMode,
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &std::collections::BTreeMap<String, String>,
    tx: mpsc::Sender<StdioEvent>,
) -> Result<windows_impl::RestrictedStdioChild, SandboxError> {
    match policy {
        SandboxMode::ReadOnly => Err(SandboxError::PolicyDenied(
            "只读沙箱禁止启动 MCP server 进程".into(),
        )),
        SandboxMode::WorkspaceWrite => {
            warn_logical_fallback_once();
            windows_impl::spawn_restricted_stdio(program, args, cwd, env, tx)
                .map_err(SandboxError::CreateFailed)
        }
        SandboxMode::DangerFullAccess => {
            windows_impl::spawn_unsandboxed_stdio(program, args, cwd, env, tx)
                .map_err(SandboxError::CreateFailed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn ws() -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("shaniu-sb-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn readonly_policy_denies_mcp_stdio() {
        let (tx, _rx) = mpsc::channel(4);
        let err = match prepare_stdio(
            SandboxMode::ReadOnly,
            "cmd",
            &["/C", "echo"],
            &ws(),
            &Default::default(),
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
        std::fs::remove_dir_all(&dir).ok();
    }
}
