//! 沙箱策略入口（docs §8 sandbox/mod.rs，ADR-002 三级策略，fail-closed）。
//!
//! 两层机制（用户 2026-11-08 定：AppContainer 打底 + 受限令牌/Job 保留做纵深）：
//!  1. **AppContainer**（`appcontainer`）负责路径/读隔离；
//!  2. **RestrictedToken + JobObject**（`windows_impl`）继续管提权面与进程树/资源。
//!     两层各自可独立验证：探针既验证容器内越界写被拒，也验证受限令牌仍生效。
//!
//! **网络不是策略维度**（2026-11-10 定，对齐 DSH）：容器一律带 `internetClient`，
//! 没有会话级网络闸、没有网络审计。档位只管文件效果；出不出网不随档位变化。
//!
//! - read-only：纯策略拒绝（不经子进程，双保险）；
//! - workspace-write：容器内启动子进程；`sandbox.os = "logical"` 时**显式**回退到
//!   只有受限令牌 + Job 的逻辑沙箱（UI/日志标注「当前未启用 OS 沙箱」）；
//! - danger-full-access：直接执行（用户明示，UI 全程标识）。
//!
//! fail-closed（FF-4）：任何沙箱准备步骤失败 → Err，**绝不降级为无沙箱执行**。

pub mod acl;
pub mod appcontainer;
pub mod capture;
mod device_acl;
pub mod msvc;
pub mod windows_impl;
pub mod write_sid;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use tokio::process::Command;
use tokio::sync::mpsc;

use windows::Win32::Security::PSID;

use crate::config::{AllowedRoot, OsSandbox, SandboxMode, WorkspaceAccess};

pub use capture::{spawn_reader, CaptureError, CapturedRun, Waiter};
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
    pub os: OsSandbox,
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

/// 强制完整度自报（对齐 DSH `SandboxEnforcement = 'full' | 'partial'`）。
///
/// 判据是「宿主能不能真的把声明的档位强制到位」，不是「隔离有多强」：
/// - AppContainer 打底：读隔离 + 路径写入边界都由容器强制 → `full`；
/// - logical 回退：只有受限令牌 + Job，**没有路径级边界** → 写入类档位只能是 `partial`；
/// - logical + `danger_full_access`：本来就没有边界要强制，声明与事实一致 → `full`。
///
/// 只有真的覆盖到才许写 `full`：这里不做「大概能拦住」的乐观判断。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxEnforcement {
    Full,
    Partial,
}

impl SandboxEnforcement {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Partial => "partial",
        }
    }
}

pub fn enforcement_of(os: OsSandbox, mode: SandboxMode) -> SandboxEnforcement {
    match (os, mode) {
        (OsSandbox::AppContainer, _) => SandboxEnforcement::Full,
        (OsSandbox::Logical, SandboxMode::DangerFullAccess) => SandboxEnforcement::Full,
        (OsSandbox::Logical, _) => SandboxEnforcement::Partial,
    }
}

/// 当前进程的强制完整度（状态条与工具结果共用同一处判定）。
pub fn current_enforcement(mode: SandboxMode) -> SandboxEnforcement {
    // 写约束模型：写入边界由 OS（WRITE_RESTRICTED + restricting SID）强制，
    // 声明与事实一致 → `full`。这是本期唯一新增的 `full` 来源，默认关闭时不可达。
    if use_restricted_token() {
        return SandboxEnforcement::Full;
    }
    enforcement_of(current_os_mode(), mode)
}

/// 第一期 A 的机制开关（docs/16 §7）。
///
/// `config.rs` 在本期冻结，所以新机制用环境变量门控：`SHANIU_SANDBOX_OS=restricted_token`。
/// **不设或设成别的值 → 完全走旧机制**，行为与今天一致；`sandbox.os="logical"` 是用户
/// 显式选择，优先级高于本开关（不会被 env 悄悄改成写约束模式）。
pub fn restricted_token_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var("SHANIU_SANDBOX_OS").as_deref(),
            Ok("restricted_token")
        )
    })
}

fn use_restricted_token() -> bool {
    restricted_token_enabled() && current_os_mode() == OsSandbox::AppContainer
}

/// 会真正落下执行副作用的工具：只有它们的输出才需要带上沙箱自报。
/// 读写文件类工具的结果里塞这行只是噪声，模型也拿它做不了判断。
const EXEC_TOOLS: &[&str] = &["unified_exec", "terminal"];

/// 给模型看的沙箱自报（拼在工具结果末尾）。非执行类工具返回 `None`。
///
/// 措辞固定为 `[sandbox] mode=… enforcement=…`：机器可读，不随界面文案漂移。
/// 祖先 ACE 还在后台补 / 补不上时追加 `ancestors=preparing|needs_admin`——这是
/// 「这条命令为什么可能看不到某个目录」的唯一线索，别让它只留在日志里。
pub fn execution_note(tool: &str, mode: SandboxMode) -> Option<String> {
    if !EXEC_TOOLS.contains(&tool) {
        return None;
    }
    let name = mode.as_str();
    let mut note = format!(
        "[sandbox] mode={name} enforcement={}",
        current_enforcement(mode).as_str()
    );
    // 只有容器路径才有祖先链这回事；写约束模式没有祖先链，逻辑回退与全权模式也没有。
    if mode == SandboxMode::WorkspaceWrite
        && current_os_mode() == OsSandbox::AppContainer
        && !restricted_token_enabled()
    {
        match ancestor_state() {
            AncestorState::Preparing => note.push_str(" ancestors=preparing"),
            AncestorState::NeedsAdmin => note.push_str(" ancestors=needs_admin"),
            AncestorState::Idle | AncestorState::Ready => {}
        }
    }
    Some(note)
}

/// 命令失败时补给人/模型的一句解释（`None` 表示与预置无关）。
fn ancestor_hint() -> Option<&'static str> {
    if current_os_mode() != OsSandbox::AppContainer {
        return None;
    }
    match ancestor_state() {
        AncestorState::Preparing => Some("沙箱正在准备文件权限，稍等重试就好"),
        AncestorState::NeedsAdmin => Some(
            "有些目录的文件权限还没准备好（需要以管理员身份运行一次 SHANIU），个别命令可能提示找不到路径",
        ),
        AncestorState::Idle | AncestorState::Ready => None,
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
            os: current_os_mode(),
        },
        tx,
    )
}

/// 带工作区根的完整入口。
pub fn prepare_spawn_with(
    request: SpawnRequest<'_>,
    tx: mpsc::Sender<ExecLine>,
) -> Result<PreparedSpawn, SandboxError> {
    match request.policy {
        // read-only：纯策略拒绝（fail-closed 双保险）
        SandboxMode::ReadOnly => Err(SandboxError::PolicyDenied("只读沙箱禁止命令执行".into())),
        // danger-full-access：直接执行
        SandboxMode::DangerFullAccess => {
            let mut command = Command::new(request.program);
            command.args(request.args).current_dir(request.cwd);
            // Python stdio 编码补丁：danger 档同样要，否则 ≤3.14 下中文输出按 GBK 出。
            for (key, value) in msvc::PYTHON_UTF8_ENV {
                command.env(key, value);
            }
            Ok(PreparedSpawn::Plain(command))
        }
        SandboxMode::WorkspaceWrite => {
            #[cfg(windows)]
            {
                // 第一期 A：门控打开时走「只约束写」的受限令牌；默认关闭，旧分流不变。
                if request.os == OsSandbox::AppContainer && restricted_token_enabled() {
                    return prepare_write_restricted(&request, tx);
                }
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
                let (sid, capabilities) = app_container_scope(request.workspace_root)?;
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
    let (sid, capabilities) = app_container_scope(request.workspace_root)?;
    let child = windows_impl::spawn_appcontainer_command(
        request.program,
        request.args,
        request.cwd,
        sid,
        capabilities,
        msvc::toolchain_env(),
        tx,
    )
    .map_err(|error| match ancestor_hint() {
        // 起进程就失败、且祖先权限还在准备：把「稍后重试」说清楚，别让人去查策略。
        Some(hint) => SandboxError::CreateFailed(format!("{error}（{hint}）")),
        None => SandboxError::CreateFailed(error),
    })?;
    Ok(PreparedSpawn::Restricted(child))
}

/// 第一期 A：以「只约束写」的受限令牌启动 `workspace-write` 子进程。
///
/// 步骤（docs/16 §1.2–1.4）：
/// 1. 由规范化路径派生两个写 SID：工作区 SID、temp SID；
/// 2. 只给 `acl::write_scope` 里的位置补可继承 allow ACE——工作区根用工作区 SID，
///    其余（TEMP/工具链）用 temp SID（restricting 列表只有四个身份，不新增身份）；
/// 3. 用 `[logon, Everyone, 工作区 SID, temp SID]` 的 restricting 列表启动子进程。
///
/// **fail-closed**：任一目录授不上写权限就拒绝启动，绝不退化成无沙箱执行。
/// 读与网络不在本机制的约束范围内（用户裁定，对齐 codex/dsh）。
#[cfg(windows)]
pub fn prepare_write_restricted(
    request: &SpawnRequest<'_>,
    tx: mpsc::Sender<ExecLine>,
) -> Result<PreparedSpawn, SandboxError> {
    let workspace_sid = write_sid::workspace_write_sid(request.workspace_root)
        .map_err(SandboxError::CreateFailed)?;
    let temp_sid =
        write_sid::temp_write_sid(&std::env::temp_dir()).map_err(SandboxError::CreateFailed)?;

    let ledger = acl::persistent_ledger_path(&crate::config::resolve_dirs_from_env().data_root);
    for dir in acl::write_scope(request.workspace_root) {
        let is_workspace_root = acl::same_path(&dir, request.workspace_root);
        let sid = if is_workspace_root {
            workspace_sid.as_psid()
        } else {
            temp_sid.as_psid()
        };
        match with_acl(|acl| acl.grant_scope_persistent(&dir, sid, is_workspace_root, &ledger))? {
            acl::ScopeGrant::Written { .. } | acl::ScopeGrant::AlreadyPresent => {}
            acl::ScopeGrant::Denied(detail) => {
                return Err(SandboxError::CreateFailed(format!(
                    "{detail}。写约束沙箱无法给「{}」授写权限，拒绝在沙箱外执行",
                    dir.display()
                )))
            }
        }
    }

    tracing::info!(
        workspace = %request.workspace_root.display(),
        workspace_sid = workspace_sid.text(),
        temp_sid = temp_sid.text(),
        "写约束沙箱就绪：受限令牌 WRITE_RESTRICTED，读与网络不约束"
    );

    let child = windows_impl::spawn_write_restricted_command(
        request.program,
        request.args,
        request.cwd,
        &[workspace_sid.as_psid(), temp_sid.as_psid()],
        tx,
    )
    .map_err(SandboxError::CreateFailed)?;
    Ok(PreparedSpawn::Restricted(child))
}

/// 容器运行时 + 工作区写 ACE 的公共准备步骤。
/// ACE 只加三类位置：工作区根 / temp / 工具链目录；授权对象是容器 SID。
/// **不加祖先链**：那几级由后台预置负责（见 `provision_ancestors_in_background`）。
/// 每个工作区根在进程内只解析一次 scope：重复 exec / 终端不再碰 DACL（docs/04 §1.5）。
///
/// 能力列表恒为 `internetClient`：网络不是策略维度，容器一律可出网（2026-11-10）。
/// 仍走 AppContainer 的意义在文件/进程边界与 loopback 隔离，不在断网。
#[cfg(windows)]
fn app_container_scope(workspace_root: &Path) -> Result<(PSID, Vec<PSID>), SandboxError> {
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
        // 账本与后台预置共用一份：写权限也是持久的，卸载要能按账本精确删。
        let ledger = acl::persistent_ledger_path(&crate::config::resolve_dirs_from_env().data_root);
        for dir in acl::write_scope(workspace_root) {
            // 工作区根与工具链目录走同一份清单，但只有前者受「过宽根」约束：
            // 工具链本来就可能在 Program Files 下。
            // 注意：**不在 exec 路径上补祖先链**。写 DACL 会让系统重算整棵子树
            // （本机 2026-11-12 实测：`D:\MyWorkspace` 25.6s、`D:\MyWorkspace\SHANIU`
            // 25.4s，与子树大小关系不大），绝不能出现在第一次 exec 里。
            // 祖先 `(RA,S)` 由 `provision_ancestors_in_background` 在启动后异步补齐。
            // 这四处写权限同理走**持久**入口：命中即跳过（ms 级），首次才写一次。
            let is_workspace_root = dir == *workspace_root;
            match with_acl(|acl| {
                acl.grant_scope_persistent(&dir, sid, is_workspace_root, &ledger)
            })? {
                acl::ScopeGrant::Written { .. } | acl::ScopeGrant::AlreadyPresent => {}
                acl::ScopeGrant::Denied(detail) => {
                    return Err(SandboxError::CreateFailed(format!(
                        "{detail}。该目录的权限当前改不了，命令拒绝在沙箱外执行"
                    )))
                }
            }
        }
        runtime.resolved_roots.insert(workspace_root.to_path_buf());
        // 新根第一次被用到：把它的祖先链交给后台补（非阻塞，本轮命令按当前状态跑）。
        // 启动时已预置过的根会被台账挡住，这里等于零成本。
        provision_ancestors_in_background(vec![workspace_root.to_path_buf()]);
    }

    Ok((
        runtime.container.sid(),
        vec![runtime.capability_internet.as_psid()],
    ))
}

// ---------------------------------------------------------------------------
// 祖先 ACE 后台预置（docs/14）
// ---------------------------------------------------------------------------
//
// 为什么必须后台：给目录写一条 ACE 会让系统重算整棵子树的继承（本机实测
// `D:\MyWorkspace` 25.6s、`D:\MyWorkspace\SHANIU` 25.4s，与子树大小关系不大）。
// 放 exec 路径上就是用户第一条命令卡半分钟，所以这里异步补、exec 只按当前状态跑。
//
// **持久化是方案本身**：祖先 ACE 写一次长期有效，不写备份、不随退出还原
// （还原等于每次启动重写一遍，白付 51s）。账本只记「我们加过哪一级、加之前有没有」，
// 供卸载/回滚使用；运行期判缺一律按真实 DACL 探测，绝不信任状态位或账本。

/// 后台预置状态。**只用于自报与提示**，绝不用来判断「缺不缺」——那必须按真实 DACL
/// 探测（`acl::ensure_ancestor_grant`）。状态会与磁盘漂移（用户手工改过 DACL、
/// 别的实例写过），账本同理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AncestorState {
    /// 没触发过（logical 档、只读工作区，或尚未启动）。
    Idle,
    /// 后台正在补齐：命令照常执行，可能因某一级还没 ACE 而看不到路径。
    Preparing,
    /// 需要的层级都已是 `(RA,S)`（本次写入或上次已写）。
    Ready,
    /// 至少一级没补上：缺 `WRITE_DAC`（`C:\Users` 这类系统拥有的层级）或 DACL 读不了。
    /// **不是 fail-closed**：命令照常按当前状态执行，只是逐段 `fs::metadata` 会断。
    NeedsAdmin,
}

impl AncestorState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Preparing => "preparing",
            Self::Ready => "ready",
            Self::NeedsAdmin => "needs_admin",
        }
    }
}

/// 正在跑预置线程的个数：`>0` 即 `Preparing`（多个工作区可能先后触发）。
static ANCESTOR_ACTIVE: AtomicUsize = AtomicUsize::new(0);
/// 至少跑完过一轮（用来把 `Idle` 与 `Ready` 分开）。
static ANCESTOR_DONE: AtomicBool = AtomicBool::new(false);
/// 至少一级被拒或失败。
static ANCESTOR_DENIED: AtomicBool = AtomicBool::new(false);
/// 退出期间置位：后台线程在每级之间检查，尽快收手（写 DACL 本身不可中断）。
static ANCESTOR_CANCEL: AtomicBool = AtomicBool::new(false);
/// 已交给后台的根：切工作区/重开窗口不重扫同一条链。
static ANCESTOR_CLAIMED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
/// 是否允许后台预置。**默认关**：这是应用生命周期行为（`lib.rs::run` 打开），
/// 不该由沙箱原语替调用方决定。默认关也保证单测/集成测试不会顺手改写真实盘的
/// 祖先 DACL（`%TEMP%` 的链条一路顶到 `C:\Users\jun`，既慢又不是用例要验的东西）。
static ANCESTOR_ENABLED: AtomicBool = AtomicBool::new(false);

/// 由应用入口显式打开后台预置（`lib.rs::run`）。
pub fn enable_ancestor_provisioning() {
    ANCESTOR_ENABLED.store(true, Ordering::Relaxed);
}

pub fn ancestor_state() -> AncestorState {
    if ANCESTOR_ACTIVE.load(Ordering::Relaxed) > 0 {
        return AncestorState::Preparing;
    }
    if ANCESTOR_DENIED.load(Ordering::Relaxed) {
        return AncestorState::NeedsAdmin;
    }
    if ANCESTOR_DONE.load(Ordering::Relaxed) {
        return AncestorState::Ready;
    }
    AncestorState::Idle
}

/// 把若干工作区根的文件权限准备交给后台。**立即返回**，从不阻塞调用方。
///
/// 两件事一起做，顺序固定：
///  1. exec 路径上的写权限（工作区根 / TEMP / 工具链目录）——`AclManager` 语义，
///     **持久化、不写备份**（`grant_scope_persistent`，2026-11-12 起与祖先 ACE 同口径）；
///  2. 祖先链的 `(RA,S)`——持久化、不写备份（见上）。
///
/// 先 1 后 2 是有意的：1 决定「第一条命令能不能立刻写文件」，2 决定「构建工具逐段
/// 取属性会不会断」，前者对用户更靠前。
///
/// 可重复调用：同一根只排一次；中途退出由 [`ANCESTOR_CANCEL`] 收手；
/// 任何一级失败都只记录并置 `NeedsAdmin`，**绝不 panic、绝不 fail-closed**。
/// 只派生容器 SID、不创建 profile：后台线程不能抢在 `build_runtime` 前建好容器，
/// 否则 `owns_profile` 会误判成「别人建的」，退出时把 profile 留下。
pub fn provision_ancestors_in_background(roots: Vec<PathBuf>) {
    if !ANCESTOR_ENABLED.load(Ordering::Relaxed) || ANCESTOR_CANCEL.load(Ordering::Relaxed) {
        return;
    }
    let pending: Vec<PathBuf> = {
        let claimed = ANCESTOR_CLAIMED.get_or_init(|| Mutex::new(HashSet::new()));
        let Ok(mut guard) = claimed.lock() else {
            tracing::warn!("祖先 ACE 后台预置：根台账锁已中毒，跳过本轮（命令不受影响）");
            return;
        };
        roots
            .into_iter()
            .filter(|root| root.is_dir() && guard.insert(root.clone()))
            .collect()
    };
    if pending.is_empty() {
        return;
    }
    let sid = match appcontainer::derived_sid(appcontainer::PROFILE_NAME) {
        Ok(sid) => sid,
        Err(error) => {
            tracing::warn!(
                %error,
                "祖先 ACE 后台预置：派生容器 SID 失败，按当前状态继续（命令不阻塞）"
            );
            return;
        }
    };
    ANCESTOR_ACTIVE.fetch_add(1, Ordering::Relaxed);
    let spawned = std::thread::Builder::new()
        .name("shaniu-ancestor-ace".into())
        .spawn(move || run_ancestor_provision(pending, sid));
    if let Err(error) = spawned {
        // 线程起不来不影响功能，只是祖先链这一轮不补：别让启动因此失败。
        ANCESTOR_ACTIVE.fetch_sub(1, Ordering::Relaxed);
        ANCESTOR_DENIED.store(true, Ordering::Relaxed);
        tracing::warn!(%error, "祖先 ACE 后台预置：线程启动失败，按当前状态继续（命令不阻塞）");
    }
}

/// 给一级祖先补 `(RA,S)`（探针/诊断用公开入口，按容器 profile 名定位 SID）。
///
/// 生产路径不打这里：`run_ancestor_provision` 直接握着一个 `Sid` 逐级调用，
/// 不必每级重新派生。给集成测试用是因为它们不能命名 `windows::...::PSID`。
pub fn ensure_ancestor_for(dir: &Path, profile_name: &str) -> Result<acl::AncestorGrant, String> {
    let sid = appcontainer::derived_sid(profile_name)?;
    acl::ensure_ancestor_grant(dir, sid.as_psid())
}

/// 撤销该容器在某个目录上的**全部**显式 ACE（探针收尾 / 未来卸载工具用）。
pub fn revoke_ancestor_for(dir: &Path, profile_name: &str) -> Result<bool, String> {
    let sid = appcontainer::derived_sid(profile_name)?;
    acl::revoke_ace(dir, sid.as_psid())
}

/// 一次预置里的单级结果（真机验收/诊断用；界面只用 [`ancestor_state`]）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct AncestorStep {
    pub path: String,
    /// `written` / `present` / `denied` / `failed`，与 `AncestorGrant::as_str` 同一套口径。
    pub result: String,
    pub elapsed_ms: u64,
}

/// 最近一次预置的汇总。**只用于验收与日志取证**，不参与任何判定。
#[derive(Debug, Clone, serde::Serialize)]
pub struct AncestorRun {
    pub steps: Vec<AncestorStep>,
    pub total_ms: u64,
    pub cancelled: bool,
    pub state: String,
}

static ANCESTOR_LAST_RUN: OnceLock<Mutex<Option<AncestorRun>>> = OnceLock::new();

/// 取最近一次预置的逐级结果；`None` 表示本进程还没跑过。
pub fn ancestor_last_run() -> Option<AncestorRun> {
    ANCESTOR_LAST_RUN
        .get()
        .and_then(|slot| slot.lock().ok())
        .and_then(|guard| guard.clone())
}

/// 后台线程本体：逐级探测 + 补 `(RA,S)`，写入成功才记账。
fn run_ancestor_provision(roots: Vec<PathBuf>, sid: appcontainer::Sid) {
    let started = std::time::Instant::now();
    let ledger = acl::persistent_ledger_path(&crate::config::resolve_dirs_from_env().data_root);
    let mut written = 0usize;
    let mut present = 0usize;
    let mut denied = 0usize;
    let mut failed = 0usize;
    let mut cancelled = false;
    let mut steps: Vec<AncestorStep> = Vec::new();

    // 开始也记一行：界面那条「正在准备文件权限」对应的就是这个状态，
    // 没有开始行的话日志里只剩最后一次写入，用户看到的提示在日志里无从对证。
    tracing::info!(
        roots = roots.len(),
        state = ancestor_state().as_str(),
        "沙箱文件权限后台预置开始（不阻塞命令执行）"
    );

    // 阶段一：exec 路径上的四处写权限（工作区根 / TEMP / .cargo / .rustup）。
    //
    // 2026-11-12 搬到后台：这四处各自要让系统重算整棵子树，实测首条命令要等 5.87s
    // （冷工作区副本；`%TEMP%`、`.cargo`、`.rustup` 子树大，占大头）。
    //
    // 2026-11-12 再改为**持久化**（与祖先 `(RA,S)` 同一套口径，对齐 dsh README:130
    // 「写一次常驻复用」）：不再写随退出还原的备份，按真实 DACL 探测命中即跳过。
    // 首次启动写一次（本机约 40s，工作区根 24s 占大头），此后每次启动只探测、零写入。
    // exec 路径上的同一入口也走持久化，所以兜底不会再把持久 ACE 还原掉。
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for root in &roots {
        for dir in acl::write_scope(root) {
            if ANCESTOR_CANCEL.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }
            // `TEMP` / `TMP` / `temp_dir()` 常常是同一个目录，重复授权没有意义。
            if !dir.is_dir() || !seen.insert(dir.clone()) {
                continue;
            }
            let step_started = std::time::Instant::now();
            let guarded = dir == *root;
            let outcome: &'static str = match with_acl(|acl| {
                acl.grant_scope_persistent(&dir, sid.as_psid(), guarded, &ledger)
            }) {
                Ok(acl::ScopeGrant::Written { .. }) => {
                    written += 1;
                    tracing::info!(
                        path = %dir.display(),
                        elapsed_ms = step_started.elapsed().as_millis() as u64,
                        "沙箱写权限已持久化（不随退出还原，下次启动探测命中即跳过）"
                    );
                    "written"
                }
                Ok(acl::ScopeGrant::AlreadyPresent) => {
                    present += 1;
                    "present"
                }
                Ok(acl::ScopeGrant::Denied(detail)) => {
                    denied += 1;
                    tracing::warn!(
                        path = %dir.display(),
                        %detail,
                        "沙箱写权限未能补齐（该路径内的写入会被拒，其余照常）：需要管理员运行一次才能补齐"
                    );
                    "denied"
                }
                Err(error) => {
                    denied += 1;
                    tracing::warn!(
                        path = %dir.display(),
                        %error,
                        "沙箱写权限未能补齐（该路径内的写入会被拒，其余照常）：需要管理员运行一次才能补齐"
                    );
                    "denied"
                }
            };
            steps.push(AncestorStep {
                path: dir.display().to_string(),
                result: outcome.to_string(),
                elapsed_ms: step_started.elapsed().as_millis() as u64,
            });
        }
        if cancelled {
            break;
        }
    }

    // 多根会共用同一段祖先：逐级已有就跳过（实测 1–2ms），重复不会重写。
    for dir in roots.iter().flat_map(|root| acl::ancestor_chain(root)) {
        if ANCESTOR_CANCEL.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        if !dir.is_dir() {
            continue;
        }
        let step_started = std::time::Instant::now();
        let outcome: &'static str = match acl::ensure_ancestor_grant(&dir, sid.as_psid()) {
            Ok(acl::AncestorGrant::Written { existed_before }) => {
                written += 1;
                // 每写一级就打一行：单级就可能 25s，只报汇总等于长时间没动静。
                tracing::info!(
                    path = %dir.display(),
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "祖先 ACE 已写入 `(RA,S)`"
                );
                if let Err(error) =
                    acl::append_persistent_ledger(&ledger, &dir, sid.as_psid(), existed_before)
                {
                    // ACE 已生效；账本丢了只影响卸载时能否精确回滚这一级。
                    tracing::warn!(
                        path = %dir.display(),
                        %error,
                        "祖先 ACE 已生效但账本写入失败（卸载时无法精确回滚这一级）"
                    );
                }
                "written"
            }
            Ok(acl::AncestorGrant::AlreadyPresent) => {
                present += 1;
                "present"
            }
            Ok(acl::AncestorGrant::Denied(detail)) => {
                denied += 1;
                tracing::warn!(
                    path = %dir.display(),
                    %detail,
                    "祖先 ACE 未能写入（当前用户没有该级的 WRITE_DAC）：需要管理员运行一次才能补齐"
                );
                "denied"
            }
            Err(error) => {
                failed += 1;
                tracing::warn!(path = %dir.display(), %error, "祖先 ACE 预置失败（命令按当前状态执行）");
                "failed"
            }
        };
        steps.push(AncestorStep {
            path: dir.display().to_string(),
            result: outcome.to_string(),
            elapsed_ms: step_started.elapsed().as_millis() as u64,
        });
    }

    if denied > 0 || failed > 0 {
        ANCESTOR_DENIED.store(true, Ordering::Relaxed);
    }
    // 被取消时**不**置 DONE：这一轮没补完，状态不能自报 `ready`
    // （下次启动会重新按真实 DACL 探测，台账是进程内的，不跨进程遗留）。
    if !cancelled {
        ANCESTOR_DONE.store(true, Ordering::Relaxed);
    }
    ANCESTOR_ACTIVE.fetch_sub(1, Ordering::Relaxed);
    let run = AncestorRun {
        steps,
        total_ms: started.elapsed().as_millis() as u64,
        cancelled,
        state: ancestor_state().as_str().to_string(),
    };
    if let Ok(mut slot) = ANCESTOR_LAST_RUN
        .get_or_init(|| Mutex::new(None))
        .lock()
    {
        *slot = Some(run.clone());
    }
    tracing::info!(
        written,
        present,
        denied,
        failed,
        cancelled,
        elapsed_ms = run.total_ms,
        state = run.state,
        "沙箱文件权限后台预置结束（工作区根/临时/工具链与祖先 (RA,S) 都持久化，下次启动按真实 DACL 探测，命中即跳过）"
    );
}

/// 进程内唯一的目录 ACL 管理器。
///
/// exec 容器与每个 MCP 容器共用它，原因有二：
///  1. `recover_pending()` 只能跑一次——两个管理器各自还原会互相把对方刚加的 ACE 抹掉；
///  2. 撤销按 `(路径, SID)` 精确匹配，天然支持「每个 server 一个容器 SID」。
static ACL: OnceLock<Mutex<Option<acl::AclManager>>> = OnceLock::new();
/// 上次异常退出遗留的备份还原条数（用于判断是否需要接管旧 profile）。
static ACL_RECOVERED: AtomicUsize = AtomicUsize::new(0);

/// ACL 备份目录（目录 DACL 与 NUL 设备 DACL 共用）。
fn acl_backup_dir() -> PathBuf {
    crate::config::resolve_dirs_from_env()
        .data_root
        .join("sandbox")
        .join("acl")
}

/// 懒初始化：只跑一次 `recover_pending`，失败即 fail-closed 返回错误。
fn ensure_acl() -> Result<(), SandboxError> {
    let mutex = ACL.get_or_init(|| Mutex::new(None));
    let mut guard = mutex
        .lock()
        .map_err(|_| SandboxError::CreateFailed("ACL 管理器锁已中毒".into()))?;
    if guard.is_some() {
        return Ok(());
    }
    let backup_dir = acl_backup_dir();
    let mut manager = acl::AclManager::new(backup_dir.clone());
    // 先还原上次异常退出留下的 DACL，再允许新的授权，避免在被污染的 ACL 上继续。
    let restored = manager
        .recover_pending()
        .map_err(SandboxError::CreateFailed)?;
    // 设备对象的备份独立存放（设备路径没有 exists()，不能进目录备份的回收分支）。
    match device_acl::restore_null_device(&backup_dir) {
        Ok(true) => tracing::info!("已还原上次异常退出遗留的 NUL 设备 DACL 备份"),
        Ok(false) => {}
        Err(error) => {
            return Err(SandboxError::CreateFailed(format!(
            "上次异常退出留下的 NUL 设备 DACL 还原失败（拒绝在未还原的设备 ACL 上继续）：{error}"
        )))
        }
    }
    ACL_RECOVERED.store(restored, Ordering::Relaxed);
    *guard = Some(manager);
    Ok(())
}

/// 让容器能打开 `\Device\Null`（SH-133）。
///
/// 失败**不** fail-closed：路径隔离本身仍成立，只是依赖 null stdio 的程序会在容器内
/// 报 `Access is denied` —— 这条必须喊出来，不能静默。
/// 返回**本次**是否真的给容器补了 NUL 设备 ACE。
///
/// 只有 `true` 才允许在释放时撤销那条 ACE：`AlreadyAccessible` 说明设备 DACL
/// 本来就能让这个 SID 打开 NUL（系统共享 SID 或前一次授权），删了会打断别人。
fn ensure_null_device(sid: PSID) -> bool {
    match device_acl::ensure_container_can_open_null(sid, &acl_backup_dir()) {
        Ok(device_acl::NullDeviceAccess::AlreadyAccessible) => false,
        Ok(device_acl::NullDeviceAccess::Granted) => {
            tracing::info!(
                "已给容器 SID 补 `\\Device\\Null` 读写 ACE（该 OS 默认不给受限应用包授权，退出时还原）"
            );
            true
        }
        Ok(device_acl::NullDeviceAccess::NoPermission(detail)) => {
            tracing::warn!(
                %detail,
                "容器打不开 `\\Device\\Null`，且当前进程没有写设备 DACL 的权限（设备只有管理员可改）。\
                 容器内依赖 null stdio 的命令（cargo/node/python 派生进程）会报 Access denied"
            );
            false
        }
        Err(error) => {
            tracing::warn!(
                %error,
                "补 `\\Device\\Null` 授权时出错：容器内依赖 null stdio 的命令可能报 Access denied"
            );
            false
        }
    }
}

fn with_acl<R>(
    f: impl FnOnce(&mut acl::AclManager) -> Result<R, String>,
) -> Result<R, SandboxError> {
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

    let container = appcontainer::AppContainer::ensure().map_err(SandboxError::CreateFailed)?;
    // exec 容器的 NUL 授权由退出时的整份设备 DACL 还原兜底，不逐个撤销。
    ensure_null_device(container.sid());
    let capability_internet =
        appcontainer::internet_client_capability().map_err(SandboxError::CreateFailed)?;
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
    // 后台祖先 ACE 预置先收手：进行中的那一级会写完（DACL 写入不可中断），
    // 但不再往下级走。**不等它**——退出路径不能被 25s 的 DACL 写入拖住。
    ANCESTOR_CANCEL.store(true, Ordering::Relaxed);
    if ANCESTOR_ACTIVE.load(Ordering::Relaxed) > 0 {
        tracing::info!("沙箱退出：已通知后台祖先 ACE 预置停止（进行中的一级会写完）");
    }
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
    restore_null_device_on_exit();
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

/// 退出路径的设备 DACL 还原：失败只记录，下次启动的 `recover_pending` 会兜底。
fn restore_null_device_on_exit() {
    match device_acl::restore_null_device(&acl_backup_dir()) {
        Ok(true) => tracing::info!("沙箱退出：NUL 设备 DACL 已按备份还原"),
        Ok(false) => {}
        Err(error) => tracing::error!(%error, "沙箱退出：NUL 设备 DACL 还原失败"),
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
    /// 本进程是否给这个 SID 补过 `\Device\Null` 的 ACE（释放时要撤）。
    null_granted: bool,
}

static MCP_RUNTIMES: OnceLock<Mutex<HashMap<String, McpRuntime>>> = OnceLock::new();

fn mcp_runtimes() -> &'static Mutex<HashMap<String, McpRuntime>> {
    MCP_RUNTIMES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn mcp_lock() -> Result<std::sync::MutexGuard<'static, HashMap<String, McpRuntime>>, SandboxError> {
    mcp_runtimes()
        .lock()
        .map_err(|_| SandboxError::CreateFailed("MCP 容器锁已中毒".into()))
}

/// 孤儿回收的白名单：当前正在连的 server + 配置内所有已信任 server + **本进程正在运行的 server**。
///
/// 最后一类是硬约束：删到还在跑的 server 的 profile 会直接打死它的进程（白删白建只是浪费，
/// 删到在跑的是事故）。因此它不能只写在注释里——`reclaim_never_deletes_a_running_profile`
/// 用「运行中 + 空白名单」的最坏组合把它钉住。
fn mcp_reclaim_allowlist(
    current: &str,
    trusted: &[String],
    running: impl IntoIterator<Item = String>,
) -> HashSet<String> {
    std::iter::once(current.to_string())
        .chain(trusted.iter().cloned())
        .chain(running)
        .filter_map(|id| appcontainer::mcp_profile_name(&id).ok())
        .collect()
}

/// 建立（或复用）某个 server 的容器 profile。
///
/// 数量上限与孤儿回收都在这里做。回收**每次接管新容器都跑**（不再只在「本进程首个容器」时跑）：
/// 白名单里已经包含正在运行的 server，所以在跑的永远不会被当成孤儿；
/// 而「配置里已信任但当前没连」的 server 也不再被白删白建。
/// 不在白名单里的 `Shaniu.Mcp.*` 才是上次遗留的孤儿（见 `appcontainer::reclaim_orphans` 的边界）。
fn ensure_mcp_container(
    server_id: &str,
    trusted_server_ids: &[String],
) -> Result<(), SandboxError> {
    let profile = appcontainer::mcp_profile_name(server_id).map_err(SandboxError::CreateFailed)?;
    let mut runtimes = mcp_lock()?;
    if runtimes.contains_key(server_id) {
        return Ok(());
    }
    let active = mcp_reclaim_allowlist(
        server_id,
        trusted_server_ids,
        runtimes.keys().cloned().collect::<Vec<_>>(),
    );
    appcontainer::reclaim_orphans(&active);
    let existing = appcontainer::list_mcp_profiles();
    if !existing.contains(&profile) && existing.len() >= appcontainer::MAX_MCP_PROFILES {
        return Err(SandboxError::CreateFailed(format!(
            "已有 {} 个 MCP 容器 profile，达到上限 {}：请先撤销不再使用的 server 信任\
             （会同时删除其容器 profile）再连接新的 server。当前 profile：{}",
            existing.len(),
            appcontainer::MAX_MCP_PROFILES,
            existing.join("、")
        )));
    }
    let container =
        appcontainer::AppContainer::ensure_named(&profile).map_err(SandboxError::CreateFailed)?;
    let null_granted = ensure_null_device(container.sid());
    let capability_internet =
        appcontainer::internet_client_capability().map_err(SandboxError::CreateFailed)?;
    runtimes.insert(
        server_id.to_string(),
        McpRuntime {
            container,
            capability_internet,
            granted: Vec::new(),
            null_granted,
        },
    );
    Ok(())
}

fn grant_mcp_paths(
    server_id: &str,
    desired: &[(PathBuf, acl::Access, acl::RootRole)],
) -> Result<(), SandboxError> {
    let mut runtimes = mcp_lock()?;
    let runtime = runtimes.get_mut(server_id).ok_or_else(|| {
        SandboxError::CreateFailed(format!("MCP server {server_id} 的容器尚未建立"))
    })?;
    let sid = runtime.container.sid();
    for (path, access, role) in desired {
        if runtime
            .granted
            .iter()
            .any(|(granted, level)| granted == path && level == access)
        {
            continue;
        }
        with_acl(|acl| match role {
            // 用户指定的根（工作区、cwd、allowed_roots）过宽即拒；程序推导的目录放行。
            // 同上：运行期不补祖先链（代价与收益见 `app_container_scope` 的注释）。
            acl::RootRole::UserPicked => acl.grant_guarded(path, sid, *access),
            acl::RootRole::Derived => acl.grant(path, sid, *access),
        })
        .map_err(|error| {
            // 常见根因：运行时装在管理员所有的目录（普通用户改不了它的 DACL，
            // 容器也没读到它的权限）。给出能照做的两个出口。
            let detail = match &error {
                SandboxError::CreateFailed(message) => message.clone(),
                other => other.to_string(),
            };
            // 过宽根拒绝自带完整说明（为什么、怎么办），不再拼「管理员安装」那套。
            if detail.starts_with("拒绝把「") {
                return SandboxError::CreateFailed(detail);
            }
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
        tracing::error!(
            server_id,
            "释放 MCP 容器：锁已中毒，ACE 与 profile 可能残留"
        );
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
    // NUL 设备只撤「本进程补过的那条」：设备对象没有 exists()，也没有目录备份可还原，
    // 只删本 SID 的显式 ACE，其它 server 的 SID 与设备原有 ACE 一个都不碰。
    if runtime.null_granted {
        match device_acl::revoke_container_access(sid) {
            Ok(removed) if removed => {
                tracing::info!("已撤销该容器在 `\\Device\\Null` 上的读写 ACE")
            }
            Ok(_) => {}
            Err(error) => tracing::error!(
                %error,
                "撤销 `\\Device\\Null` 上的容器 ACE 失败（退出时的设备 DACL 还原会兜底）"
            ),
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
    /// 强制完整度自报（`full` / `partial`），与工具结果里的同一判定。
    pub enforcement: &'static str,
    /// 容器 profile 名（UI 常驻展示"跑的到底是哪个容器"）；logical 模式为空串。
    pub profile: String,
    /// 文件权限后台准备的进度：`idle` / `preparing` / `ready` / `needs_admin`。
    ///
    /// 界面只在 `preparing` 与 `needs_admin` 时显示一行字，其余不占位。
    /// **它只是自报，不是判定**：要不要重写某条 ACE 一律按真实 DACL 探测。
    pub ancestor: &'static str,
    pub note: String,
}

pub fn status(mode: SandboxMode, os: OsSandbox) -> SandboxStatus {
    // 写约束模型自报：没有容器、没有祖先链，写边界由令牌强制。
    if os == OsSandbox::AppContainer && restricted_token_enabled() {
        return SandboxStatus {
            mode: mode.as_str(),
            os: "restricted_token",
            os_enabled: true,
            enforcement: SandboxEnforcement::Full.as_str(),
            profile: String::new(),
            ancestor: AncestorState::Idle.as_str(),
            note: "受限令牌约束写入（WRITE_RESTRICTED）；读与网络不约束，无需管理员".to_string(),
        };
    }
    let (os_name, os_enabled, note) = match os {
        OsSandbox::AppContainer => (
            "app_container",
            true,
            "AppContainer 路径/读隔离；受限令牌与 Job 作为纵深保留",
        ),
        OsSandbox::Logical => (
            "logical",
            false,
            "当前未启用 OS 沙箱：仅受限令牌 + Job（逻辑隔离），没有路径级写入边界",
        ),
    };
    SandboxStatus {
        mode: mode.as_str(),
        os: os_name,
        os_enabled,
        enforcement: enforcement_of(os, mode).as_str(),
        profile: match os {
            OsSandbox::AppContainer => appcontainer::PROFILE_NAME.to_string(),
            OsSandbox::Logical => String::new(),
        },
        // 只有容器路径才有「准备文件权限」这回事：逻辑回退与只读/全权档都不补，恒为 idle。
        ancestor: match (os, mode) {
            (OsSandbox::AppContainer, SandboxMode::WorkspaceWrite) => ancestor_state().as_str(),
            _ => AncestorState::Idle.as_str(),
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
    /// 配置内**所有已信任** server 的 id：孤儿回收的白名单（它们的容器 profile 不算遗留物）。
    pub trusted_server_ids: &'a [String],
}

/// 按策略准备 MCP stdio server（SH-060，X10：默认一律走沙箱）。
///
/// - read-only：纯策略拒绝（只读沙箱不允许任何进程执行）；
/// - workspace-write + `sandbox.os = "app_container"`：**逐 server 一个容器**，
///   ACE 按 server 的 cwd/allowed_roots 逐条授权，网络默认允许（显式 `allow_network = false` 才不挂 `internetClient`）；
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
        SandboxMode::DangerFullAccess => windows_impl::spawn_unsandboxed_stdio(
            request.program,
            request.args,
            request.cwd,
            request.env,
            tx,
        )
        .map_err(SandboxError::CreateFailed),
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
/// 收集 MCP 容器要授权的「用户指定根」：工作区根、cwd、`allowed_roots`。
///
/// 判定只有 `config::workspace_gate` 一份——这里不复制规则，只把「指向数据根」翻成
/// 拒绝启动的原因。数据根里是会话档案、日志与凭据：容器一旦拿到 ACE，MCP server
/// 就能直接读写它们。`allowed_roots` / `cwd` 都直接来自配置，换个键不该绕过工作区
/// 那套判据（`workspace_root` 上游已过闸门，这里再判一次是兜底，不是第二份规则）。
fn guarded_mcp_user_roots(
    request: &StdioRequest<'_>,
    data_root: &Path,
    workspace_access: acl::Access,
) -> Result<Vec<(PathBuf, acl::Access, acl::RootRole)>, SandboxError> {
    let candidates = [
        (
            "工作区根",
            request.workspace_root.to_path_buf(),
            workspace_access,
        ),
        ("cwd", request.cwd.to_path_buf(), workspace_access),
    ]
    .into_iter()
    .chain(request.allowed_roots.iter().map(|root| {
        (
            "allowed_roots",
            PathBuf::from(root.path.trim()),
            acl_access(root.access),
        )
    }));
    let mut roots = Vec::new();
    for (label, path, access) in candidates {
        let verdict = crate::config::workspace_gate::check_against(&path, data_root);
        if let Some(rejection) = verdict.rejection() {
            return Err(SandboxError::CreateFailed(format!(
                "MCP server {} 的{label}目录不可用：{}",
                request.server_id,
                rejection.message(&path, data_root)
            )));
        }
        roots.push((path, access, acl::RootRole::UserPicked));
    }
    Ok(roots)
}

#[cfg(windows)]
fn spawn_mcp_in_app_container(
    request: &StdioRequest<'_>,
    tx: mpsc::Sender<StdioEvent>,
) -> Result<windows_impl::RestrictedStdioChild, SandboxError> {
    ensure_mcp_container(request.server_id, request.trusted_server_ids)?;

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
    // 角色决定是否受过宽约束：用户指定的一律判，程序推导的（临时目录、工具链）不判。
    let mut desired = guarded_mcp_user_roots(request, &data_root, workspace_access)?;
    // 临时目录必须可写：TEMP/TMP 会在下面改指到这里。
    desired.push((
        server_temp.clone(),
        acl::Access::Write,
        acl::RootRole::Derived,
    ));
    for root in toolchain_roots(request.program, request.args) {
        desired.push((root, acl::Access::Read, acl::RootRole::Derived));
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
            SandboxError::CreateFailed(format!("MCP server {} 的容器尚未建立", request.server_id))
        })?;
        (
            runtime.container.sid(),
            runtime.capability_internet.as_psid(),
        )
    };
    let capabilities = if request.allow_network {
        vec![internet]
    } else {
        Vec::new()
    };
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
    [
        "SystemRoot",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramData",
    ]
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
        let spawn = match prepare_spawn(
            SandboxMode::DangerFullAccess,
            "cmd",
            &["/C", "echo"],
            &dir,
            tx,
        ) {
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
                trusted_server_ids: &[],
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

    /// 强制完整度只看「宿主能不能真的把声明的档位强制到位」：
    /// logical 下写入类档位必须是 partial，不许自报 full。
    #[test]
    fn enforcement_is_honest_about_logical_fallback() {
        assert_eq!(
            enforcement_of(OsSandbox::AppContainer, SandboxMode::ReadOnly),
            SandboxEnforcement::Full
        );
        assert_eq!(
            enforcement_of(OsSandbox::AppContainer, SandboxMode::WorkspaceWrite),
            SandboxEnforcement::Full
        );
        assert_eq!(
            enforcement_of(OsSandbox::Logical, SandboxMode::WorkspaceWrite),
            SandboxEnforcement::Partial
        );
        assert_eq!(
            enforcement_of(OsSandbox::Logical, SandboxMode::ReadOnly),
            SandboxEnforcement::Partial
        );
        // danger_full_access 在 logical 下没有边界要强制，声明与事实一致。
        assert_eq!(
            enforcement_of(OsSandbox::Logical, SandboxMode::DangerFullAccess),
            SandboxEnforcement::Full
        );
        let status = status(SandboxMode::WorkspaceWrite, OsSandbox::Logical);
        assert_eq!(status.enforcement, "partial");
    }

    /// 自报只挂在真的执行了子进程的工具上，且字段名固定。
    #[test]
    fn execution_note_only_for_exec_tools() {
        assert!(execution_note("read_file", SandboxMode::WorkspaceWrite).is_none());
        let note = execution_note("unified_exec", SandboxMode::WorkspaceWrite).expect("应有自报");
        assert!(
            note.starts_with("[sandbox] mode=workspace_write enforcement="),
            "实际: {note}"
        );
        let escalated = execution_note("terminal", SandboxMode::DangerFullAccess).unwrap();
        assert!(
            escalated.contains("mode=danger_full_access"),
            "实际: {escalated}"
        );
    }

    /// 非法 server id 派生不出容器名：必须**直接失败**，不能悄悄退回受限令牌。
    #[cfg(windows)]
    #[test]
    fn invalid_server_id_fails_closed_instead_of_token_fallback() {
        let _serial = appcontainer::PROFILE_TEST_MUTEX.blocking_lock();
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
                trusted_server_ids: &[],
            },
            tx,
        ) {
            Err(error) => error,
            Ok(_) => panic!("非法 id 必须拒绝启动，而不是回退受限令牌"),
        };
        assert!(matches!(err, SandboxError::CreateFailed(_)), "实际: {err}");
        assert!(
            err.to_string().contains("profile"),
            "错误应指清是容器名派生失败: {err}"
        );
    }

    /// 每个 server 一个容器：建立后可释放，释放时 profile 一并删除。
    #[cfg(windows)]
    #[test]
    fn mcp_container_is_per_server_and_deleted_on_release() {
        let _serial = appcontainer::PROFILE_TEST_MUTEX.blocking_lock();
        let id = format!("release-{}", std::process::id());
        ensure_mcp_container(&id, &[]).expect("应能建立 per-server 容器");
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

    /// 硬约束：**正在运行的 server 的 profile 永不在回收范围内**——删到在跑的会直接
    /// 打死它的进程（白删白建只是浪费）。用最坏组合钉住：配置白名单为空，
    /// 只有「本进程正在运行」这一路能救它；若这行代码被删掉，本用例必红。
    #[cfg(windows)]
    #[test]
    fn reclaim_never_deletes_a_running_profile() {
        let _serial = appcontainer::PROFILE_TEST_MUTEX.blocking_lock();
        let stamp = std::process::id();
        let running = format!("keep{stamp}");
        let orphan = format!("leak{stamp}");
        let running_profile = appcontainer::mcp_profile_name(&running).unwrap();
        let orphan_profile = appcontainer::mcp_profile_name(&orphan).unwrap();
        let running_container =
            appcontainer::AppContainer::ensure_named(&running_profile).expect("建运行中容器");
        appcontainer::AppContainer::ensure_named(&orphan_profile).expect("建孤儿容器");

        let allowlist = mcp_reclaim_allowlist("another-server", &[], [running.clone()]);
        let deleted = appcontainer::reclaim_orphans(&allowlist);

        assert!(
            !deleted.contains(&running_profile),
            "正在运行的 server profile 永不在回收范围内（实际删除清单：{deleted:?}）"
        );
        assert!(
            deleted.contains(&orphan_profile),
            "非运行、非信任的 profile 应被回收"
        );
        assert!(
            appcontainer::list_mcp_profiles().contains(&running_profile),
            "运行中的 profile 必须仍然存在"
        );
        let _ = running_container.delete();
    }

    /// 已信任但当前没连的 server 的 profile 不算孤儿：回收不能白删白建
    /// （重建 profile 会丢包目录与注册记录，纯属浪费且有窗口期）。
    #[cfg(windows)]
    #[test]
    fn reclaim_keeps_trusted_profiles() {
        let _serial = appcontainer::PROFILE_TEST_MUTEX.blocking_lock();
        let stamp = std::process::id();
        let trusted = format!("trust{stamp}");
        let stale = format!("stale{stamp}");
        let trusted_profile = appcontainer::mcp_profile_name(&trusted).unwrap();
        let stale_profile = appcontainer::mcp_profile_name(&stale).unwrap();
        let trusted_container = appcontainer::AppContainer::ensure_named(&trusted_profile).unwrap();
        appcontainer::AppContainer::ensure_named(&stale_profile).unwrap();

        let allowlist = mcp_reclaim_allowlist(
            "current-server",
            std::slice::from_ref(&trusted),
            std::iter::empty::<String>(),
        );
        let deleted = appcontainer::reclaim_orphans(&allowlist);

        assert!(
            !deleted.contains(&trusted_profile),
            "已信任 server 的 profile 不应被回收"
        );
        assert!(
            deleted.contains(&stale_profile),
            "既不在配置也不在运行的 profile 才是孤儿"
        );
        assert!(appcontainer::list_mcp_profiles().contains(&trusted_profile));
        let _ = trusted_container.delete();
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
        ensure_mcp_container(&id, &[]).expect("建立 per-server 容器");
        let sid = {
            let runtimes = mcp_lock().unwrap();
            runtimes.get(&id).expect("容器应已登记").container.sid()
        };
        let profile = appcontainer::mcp_profile_name(&id).unwrap();

        grant_mcp_paths(
            &id,
            &[(target.clone(), acl::Access::Read, acl::RootRole::UserPicked)],
        )
        .expect("授权读");
        assert!(
            acl::has_explicit_grant(&target, sid, acl::Access::Read.mask()).unwrap(),
            "容器 SID 应有显式读 ACE"
        );
        // 幂等：重复授权不报错，ACE 仍在。
        grant_mcp_paths(
            &id,
            &[(target.clone(), acl::Access::Read, acl::RootRole::UserPicked)],
        )
        .expect("重复授权应幂等");
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

    /// `allowed_roots` 与工作区根共用同一套过宽判定：换个配置键不该能绕过去。
    #[cfg(windows)]
    #[test]
    fn overbroad_allowed_root_is_refused_and_audited() {
        let _serial = appcontainer::PROFILE_TEST_MUTEX.blocking_lock();
        set_os_mode(OsSandbox::AppContainer);
        let dir = ws();
        let sub = dir.join("allowed-sub");
        std::fs::create_dir_all(&sub).unwrap();
        let id = format!("acl-wide-{}", std::process::id());
        ensure_mcp_container(&id, &[]).expect("建立 per-server 容器");

        let audit_path = acl_backup_dir()
            .parent()
            .expect("备份目录应位于 sandbox/ 下")
            .join("acl-audit.jsonl");
        let before = std::fs::read_to_string(&audit_path)
            .map(|text| text.len())
            .unwrap_or(0);

        let home = crate::config::home_dir();
        let error = grant_mcp_paths(
            &id,
            &[(home.clone(), acl::Access::Read, acl::RootRole::UserPicked)],
        )
        .expect_err("allowed_roots 里放用户主目录必须拒绝");
        assert!(error.to_string().contains("用户主目录"), "{error}");

        let audit = std::fs::read_to_string(&audit_path).unwrap();
        let appended = audit.get(before..).unwrap_or("");
        assert!(
            appended.contains("refused") && appended.contains("用户主目录"),
            "拒绝必须落进审计，本次追加内容：{appended}"
        );

        // 主目录的子目录放行（临时目录本身就在用户主目录下）。
        grant_mcp_paths(
            &id,
            &[(sub.clone(), acl::Access::Read, acl::RootRole::UserPicked)],
        )
        .expect("主目录的子目录不应被拦");
        release_mcp_server(&id);
    }

    /// MCP 的 `cwd` / `allowed_roots` 直接来自配置，ACL 侧必须在授权前再判一次数据根：
    /// 容器一旦拿到 ACE，server 就能直接读写会话档案、日志与凭据。
    /// 反向对照：去掉 `guarded_mcp_user_roots` 里的 `check_against`，下面三条拒绝会变成 Ok。
    #[cfg(windows)]
    #[test]
    fn mcp_user_picked_roots_inside_data_root_are_refused() {
        let dir = ws();
        let data_root = dir.join("data");
        std::fs::create_dir_all(data_root.join("secrets")).unwrap();
        let workspace = dir.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let env = BTreeMap::new();
        let workspace = workspace.as_path();
        fn make<'a>(
            cwd: &'a Path,
            workspace: &'a Path,
            allowed: &'a [AllowedRoot],
            env: &'a BTreeMap<String, String>,
        ) -> StdioRequest<'a> {
            StdioRequest {
                policy: SandboxMode::WorkspaceWrite,
                server_id: "guard-probe",
                program: "cmd",
                args: &[],
                cwd,
                workspace_root: workspace,
                workspace_access: WorkspaceAccess::Read,
                allowed_roots: allowed,
                allow_network: false,
                env,
                trusted_server_ids: &[],
            }
        }

        // 正常：工作区与数据根之外的目录放行。
        let outside = dir.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let allowed = [AllowedRoot {
            path: outside.display().to_string(),
            access: WorkspaceAccess::Read,
        }];
        let roots = guarded_mcp_user_roots(
            &make(workspace, workspace, &allowed, &env),
            &data_root,
            acl::Access::Read,
        )
        .expect("数据根之外的用户指定根必须放行");
        assert_eq!(roots.len(), 3, "工作区、cwd、allowed_roots 各一条");

        // 拒绝：allowed_roots 指到 data\secrets。
        let secrets = data_root.join("secrets");
        let allowed = [AllowedRoot {
            path: secrets.display().to_string(),
            access: WorkspaceAccess::Write,
        }];
        let error = guarded_mcp_user_roots(
            &make(workspace, workspace, &allowed, &env),
            &data_root,
            acl::Access::Read,
        )
        .expect_err("allowed_roots 指向数据根内部必须拒绝");
        assert!(error.to_string().contains("allowed_roots"), "{error}");
        assert!(error.to_string().contains("secrets"), "{error}");

        // 拒绝：cwd 指向数据根本身。
        let error = guarded_mcp_user_roots(
            &make(&data_root, workspace, &[], &env),
            &data_root,
            acl::Access::Read,
        )
        .expect_err("cwd 指向数据根必须拒绝");
        assert!(error.to_string().contains("cwd"), "{error}");

        // 拒绝：数据根的上级目录（祖先）同样拿到的是「能读档案」的授权。
        let error = guarded_mcp_user_roots(
            &make(dir.path(), workspace, &[], &env),
            &data_root,
            acl::Access::Read,
        )
        .expect_err("数据根祖先必须拒绝");
        assert!(error.to_string().contains("上级目录"), "{error}");
    }

    /// 「补 NUL ACE → 释放时撤销」整条链路（SH-064 B）。
    ///
    /// 这条用例在不同 SKU 上验的不是同一半，必须写清楚：
    /// - 客户端 SKU（本机 Win11）：令牌自带 `S-1-15-2-2`，容器建起来就是 `AlreadyAccessible`，
    ///   这里只验「没补过就不许撤别人的」；
    /// - Server SKU（GitHub runner，Build 26100）：设备 DACL 里没有任何应用包 SID
    ///   （SH-133 取证），`ensure_mcp_container` 必然走 `Granted`——于是「补上 → 撤掉」
    ///   在 runner 上真实发生，本用例 `ok` 即证明两件事：ACE 真的落到了设备上，释放真的删掉了它。
    /// - 若 runner 上补不进（没有设备 `WRITE_DAC`），下面的断言会**红**，而不是留一条只写
    ///   warn 的日志：容器内所有 `Stdio::null()` 都会 Access denied，这是失效态，不许静默。
    #[cfg(windows)]
    #[test]
    fn mcp_null_device_grant_is_revoked_on_release() {
        let _serial = appcontainer::PROFILE_TEST_MUTEX.blocking_lock();
        set_os_mode(OsSandbox::AppContainer);
        let device = Path::new(device_acl::NULL_DEVICE);
        let mask = device_acl::REQUIRED_MASK;
        let shared_present = || {
            ["S-1-15-2-2", "S-1-15-2-1"].iter().any(|text| {
                let shared = appcontainer::capability_sid(text).expect("已知 SID 应可解析");
                acl::has_explicit_grant(device, shared.as_psid(), mask).unwrap_or(false)
            })
        };

        let id = format!("nul-{}", std::process::id());
        ensure_mcp_container(&id, &[]).expect("建立 per-server 容器");
        let (sid, granted) = {
            let runtimes = mcp_lock().unwrap();
            let runtime = runtimes.get(&id).expect("容器应已登记");
            (runtime.container.sid(), runtime.null_granted)
        };
        let profile = appcontainer::mcp_profile_name(&id).unwrap();

        assert!(
            granted || shared_present(),
            "容器既没被补上 NUL ACE、设备 DACL 里也没有共享应用包 SID：\
             容器内所有 `Stdio::null()` / `Command::output()` 都会 Access denied"
        );
        if granted {
            // 「做完了」的另一半由设备 DACL 自己回答：不能只信 `null_granted` 这个布尔。
            assert!(
                acl::has_explicit_grant(device, sid, mask).unwrap(),
                "null_granted = true，但设备 DACL 上查不到该容器 SID 的读写 ACE"
            );
        }

        release_mcp_server(&id);

        assert!(
            !acl::has_explicit_grant(device, sid, mask).unwrap(),
            "释放后本容器 SID 的 NUL ACE 必须撤销（不撤 = 每换一个 server 就在设备上多留一条）"
        );
        if !granted {
            // 本机口径：什么都没补，就更不许把设备上原有的共享应用包授权删掉。
            assert!(
                shared_present(),
                "释放不得删除设备 DACL 里原有的共享应用包授权"
            );
        }
        assert!(
            !appcontainer::list_mcp_profiles().contains(&profile),
            "释放后 profile 应被删除"
        );
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
                trusted_server_ids: &[],
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
        assert!(
            output.contains("container-read-ok"),
            "容器内应能读到工作区文件，实际: {output}"
        );
        release_mcp_server(&id);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn workspace_write_runs_sandboxed_and_succeeds() {
        // 与 MCP 容器用例共用一把锁：它们会把全局 `OS_MODE` 设成 AppContainer，
        // 并行跑到这里会让 `prepare_spawn` 返回 AppContainer 变体而不是 Restricted
        // （2026-11-09 全量跑实测踩到一次）。
        let _serial = appcontainer::PROFILE_TEST_MUTEX.lock().await;
        // 真实 Win32 沙箱 smoke：受限令牌 + Job + AppContainer 下运行 echo，退出码必须为 0
        let dir = ws();
        let (tx, mut rx) = mpsc::channel(64);
        let mut child = match prepare_spawn(
            SandboxMode::WorkspaceWrite,
            "cmd",
            &["/C", "echo sb-ok"],
            &dir,
            tx,
        ) {
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

    /// H3：`wait()` 的退出码必须**缓存**——`unified_exec` 的 `select!` 每收一行都会重建
    /// `waiter.status()` future，旧实现每次调用都新起一个 `spawn_blocking(INFINITE)`，
    /// 阻塞池（512）会被输出行数填满。这里验证重复调用返回同一结果，且已完成的 oneshot
    /// 不会被二次轮询（第二次调用命中 `exit_result` 缓存，不再触碰阻塞池）。
    #[cfg(windows)]
    #[tokio::test]
    async fn restricted_wait_caches_exit_code_across_calls() {
        let _serial = appcontainer::PROFILE_TEST_MUTEX.lock().await;
        let dir = ws();
        let (tx, _rx) = mpsc::channel(64);
        let mut child = match prepare_spawn(
            SandboxMode::WorkspaceWrite,
            "cmd",
            &["/C", "exit 7"],
            &dir,
            tx,
        ) {
            Ok(PreparedSpawn::Restricted(child)) => child,
            _ => panic!("workspace-write 应为 Restricted"),
        };
        let first = child.wait().await.expect("首次等待应拿到退出码");
        let second = child.wait().await.expect("重复等待必须复用缓存结果");
        let third = child.wait().await.expect("第三次等待同样走缓存");
        assert_eq!((first, second, third), (7, 7, 7));
    }

    /// 后台预置的真机验收（**不跑 CI**）：会写真实盘 DACL，单级可达 25s。
    ///
    /// ```text
    /// $env:SHANIU_ANCESTOR_PROBE='D:\MyWorkspace\SHANIU'
    /// cargo test --lib ancestor_provision_real_disk -- --ignored --nocapture
    /// ```
    /// 连跑两次：第一次应真写并收敛到 `ready`，第二次应毫秒级跳过（零写入）。
    /// 关键断言是**入队立即返回**——这 51s 绝不能挡在第一次 exec 前面。
    #[cfg(windows)]
    #[test]
    #[ignore = "写真实盘 DACL（单级可达 25s），只在真机验收时手动跑"]
    fn ancestor_provision_real_disk_acceptance() {
        use std::time::{Duration, Instant};

        let Some(root) = std::env::var_os("SHANIU_ANCESTOR_PROBE").map(PathBuf::from) else {
            eprintln!("跳过：未设置 SHANIU_ANCESTOR_PROBE");
            return;
        };
        enable_ancestor_provisioning();
        let sid = appcontainer::derived_sid(appcontainer::PROFILE_NAME).expect("派生容器 SID");
        let chain = acl::ancestor_chain(&root);
        let probe_started = Instant::now();
        let missing = chain
            .iter()
            .filter(|dir| {
                !acl::has_explicit_grant(dir, sid.as_psid(), acl::ANCESTOR_ACCESS_MASK)
                    .unwrap_or(false)
            })
            .count();
        let probed = probe_started.elapsed();
        eprintln!(
            "[provision] 按真实 DACL 探测 {} 级耗时 {}µs",
            chain.len(),
            probed.as_micros()
        );

        let started = Instant::now();
        provision_ancestors_in_background(vec![root.clone()]);
        let enqueued = started.elapsed();
        eprintln!(
            "[provision] 入队返回 {}µs（链 {} 级，其中缺 ACE {} 级）：{}",
            enqueued.as_micros(),
            chain.len(),
            missing,
            root.display()
        );
        assert!(
            enqueued < Duration::from_millis(100),
            "入队必须立即返回，实际 {enqueued:?}"
        );

        // `Preparing` 期间的自报必须带上状态：这是用户唯一能看到的「为什么看不到某个目录」。
        if ancestor_state() == AncestorState::Preparing {
            let note = execution_note("unified_exec", SandboxMode::WorkspaceWrite)
                .expect("执行类工具应有自报");
            eprintln!("[provision] Preparing 期自报：{note}");
            assert!(note.contains("ancestors=preparing"), "实际: {note}");
        }

        while ancestor_state() == AncestorState::Preparing {
            let waited = started.elapsed();
            assert!(
                waited < Duration::from_secs(300),
                "后台预置超时（>300s）"
            );
            // 快路径自旋：Windows 的 `Sleep` 取整到 15.6ms，`sleep(1)` 实测也是 15ms+，
            // 用 sleep 轮询会把「毫秒级收敛」吞成 200ms（旧写法就是这样虚报的）。
            // 超过 50ms 说明是首写那种长跑，改回 sleep，别为一个 25s 的用例白烧一核。
            if waited < Duration::from_millis(50) {
                std::hint::spin_loop();
            } else {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let total = started.elapsed();
        eprintln!(
            "[provision] 结束状态 {}，从入队到收敛 {}µs（{}ms）",
            ancestor_state().as_str(),
            total.as_micros(),
            total.as_millis()
        );

        // 逐级耗时只能从后台线程自己记的那份拿：外部轮询看不到单级粒度。
        let run = ancestor_last_run().expect("后台线程应留下最近一次预置记录");
        eprintln!(
            "[provision] 线程内自计 total={}ms cancelled={} state={}",
            run.total_ms, run.cancelled, run.state
        );
        eprintln!("[provision] 逐级结果（path | result | elapsed_ms）：");
        for step in &run.steps {
            eprintln!(
                "[provision]   {} | {} | {}",
                step.path, step.result, step.elapsed_ms
            );
        }
        let written: Vec<&AncestorStep> = run
            .steps
            .iter()
            .filter(|step| step.result == "written")
            .collect();
        eprintln!(
            "[provision] 写入 {} 级、已在位 {} 级、拒绝 {} 级、失败 {} 级",
            written.len(),
            run.steps.iter().filter(|s| s.result == "present").count(),
            run.steps.iter().filter(|s| s.result == "denied").count(),
            run.steps.iter().filter(|s| s.result == "failed").count()
        );

        for dir in &chain {
            let granted =
                acl::has_explicit_grant(dir, sid.as_psid(), acl::ANCESTOR_ACCESS_MASK).unwrap_or(false);
            eprintln!(
                "[provision] {} → {}",
                dir.display(),
                if granted { "(RA,S) 在位" } else { "缺 ACE" }
            );
        }
        // 本机验收口径：链上每一级都必须真的拿到 `(RA,S)`；拿不到的层级会被
        // `ensure_ancestor_grant` 报 `Denied` 并置 `needs_admin`，那是另一条结论。
        assert_eq!(
            ancestor_state(),
            AncestorState::Ready,
            "有层级没补上（看上面逐级输出与日志里的 WRITE_DAC 提示）"
        );
    }
}
