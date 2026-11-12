//! 目录 DACL 管理（SH-022）：只给容器 SID 加写 ACE，且**先备份再改**。
//!
//! 硬约束（用户 2026-11-08 定）：
//!  - ACE 只加在**工作区根 / temp / 工具链目录**三类位置；
//!  - 授权对象是**容器 SID**，不是 Everyone / Users；
//!  - 改安全描述符前先备份原始 DACL；备份失败就不许改（不"先改了再说"）；
//!  - 卸载/退出按备份还原，避免残留 ACE 指向已删除的容器 SID。
//!
//! 第四类：**祖先链**（2026-11-12 定案，见 docs/04 §1.5.1、docs/14）。给每个授权根补
//! 「盘根以下 → 工作区根」每一级的 `(RA,S)` = `FILE_READ_ATTRIBUTES|SYNCHRONIZE`
//! （`0x100080`：够 `fs::metadata`，**不含** `FILE_LIST_DIRECTORY`，不暴露祖先条目名）。
//!
//! **判定口径（之前写反过，别再踩）**：容器对某个对象做 `metadata` / `read_dir`，需要
//! **该对象自身**有一条命名容器 SID 的 ACE；**穿过**祖先不需要 ACE（`SeChangeNotifyPrivilege`
//! 由对象管理器实现，整条路径的 `FILE_TRAVERSE` 被绕过）。所以「盘根没 ACE」不等于
//! 「路径在盘根就断了」——断点永远是被访问的那个对象。
//!
//! 为什么必须补：容器里的构建工具用 `glob` 逐段 `fs::metadata` 探测路径，
//! `glob-0.3.4` 对任何一级失败都 `.flatten()` **静默丢弃整条路径**（不报错）。
//! 实测后果：`tauri-build` 收集不到 `core:*` 权限，冷构建报
//! `Permission core:window:allow-minimize not found`，全程没有一句「拒绝访问」。
//!
//! **持久化、不进备份、不在 exec 路径**：祖先 ACE 一次写入长期有效，绝不随退出还原
//! （还原等于每次启动重写）。写一条不继承 ACE 仍会让系统重算整棵子树——本机实测
//! `D:\MyWorkspace` 25.6s / `D:\MyWorkspace\SHANIU` 25.4s（与子树大小关系不大，是整树
//! 继承重算的固定成本），所以只能**后台预置**（`mod::provision_ancestors_in_background`），
//! 绝不能在第一次 exec 里现补。下一次启动按真实 DACL 探测命中，1–2ms 跳过。
//!
//! 幂等与范围（2026-11-08 补，见 docs/04 §1.5）：
//!  - 目录已有容器 SID 的**显式** ACE（掩码覆盖授权掩码）→ 不改写、不写备份；
//!  - scope 里被父目录严格包含的路径不再单独授权（继承 ACE 已覆盖）；
//!  - 每个工作区根每进程只解析一次 scope，重复 exec 零 DACL 操作。
//!
//! 备份落盘在数据目录 `sandbox/acl/` 下，键为规范化路径的 FNV-1a 哈希；
//! 进程崩溃后下次启动先 `recover_pending()` 还原，再重新授权。

use std::collections::HashSet;
use std::ffi::c_void;
use std::path::{Path, PathBuf};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSidToSidW, GetNamedSecurityInfoW, SetEntriesInAclW,
    SetNamedSecurityInfoW, EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS,
    SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows::Win32::Security::{
    AclSizeInformation, EqualSid, GetAce, GetAclInformation, GetSecurityDescriptorDacl,
    GetSecurityDescriptorLength, ACCESS_ALLOWED_ACE, ACE_FLAGS, ACL, ACL_SIZE_INFORMATION,
    DACL_SECURITY_INFORMATION, INHERITED_ACE, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SUB_CONTAINERS_AND_OBJECTS_INHERIT,
    UNPROTECTED_DACL_SECURITY_INFORMATION,
};
use windows::Win32::Storage::FileSystem::{
    DELETE, FILE_DELETE_CHILD, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
};

use super::windows_impl::wide;
use crate::clock::now_ms;

const BACKUP_MAGIC: &[u8; 8] = b"SHNIACL1";
const BACKUP_EXT: &str = "dacl";
const AUDIT_FILE: &str = "acl-audit.jsonl";

/// 授权档位（2026-11-08 用户定：目录级勾选只有这三种）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Access {
    /// 列目录 + 读文件（不给写、不给删）。
    Read,
    /// 读 + 写 + 删（目录还要能删子项）。
    Write,
    /// 祖先链补洞：只给「读属性 + 同步」，够 `fs::metadata`，**不给列举、不给读内容**。
    ///
    /// 不要用 `FILE_GENERIC_EXECUTE` / `Traverse`：它含 `FILE_LIST_DIRECTORY`，
    /// 会把祖先目录的条目名暴露给容器。穿越本身不需要 ACE（`SeChangeNotifyPrivilege`
    /// 绕过），所以这里买的只是「能对祖先对象取属性」这一件事。
    Ancestor,
}

/// `Ancestor` 的掩码：`FILE_READ_ATTRIBUTES`(0x80) | `SYNCHRONIZE`(0x100000)。
///
/// 逐档实测（容器内 `fs::metadata`）：`FILE_READ_ATTRIBUTES` 单独**不够**、
/// `READ_CONTROL` 单独**不够**、本条**够用且 `read_dir` 仍被拒**。
pub const ANCESTOR_ACCESS_MASK: u32 = 0x0000_0080 | 0x0010_0000;

impl Access {
    pub(crate) fn mask(self) -> u32 {
        match self {
            // 读：文件读 + 执行/遍历（目录列举走 FILE_READ_DATA）。
            Access::Read => FILE_GENERIC_READ.0 | FILE_GENERIC_EXECUTE.0,
            // 写：在读的基础上加写、删、删子项。
            Access::Write => {
                FILE_GENERIC_READ.0
                    | FILE_GENERIC_WRITE.0
                    | FILE_GENERIC_EXECUTE.0
                    | DELETE.0
                    | FILE_DELETE_CHILD.0
            }
            // 祖先：只买「读属性 + 同步」。
            Access::Ancestor => ANCESTOR_ACCESS_MASK,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Access::Read => "read",
            Access::Write => "write",
            Access::Ancestor => "ancestor",
        }
    }
}

/// 三类允许位置：工作区根、temp、工具链目录（只取真实存在的目录）。
pub fn write_scope(workspace_root: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    paths.push(workspace_root.to_path_buf());

    for key in ["TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(key) {
            paths.push(PathBuf::from(value));
        }
    }
    paths.push(std::env::temp_dir());

    // 工具链目录：只有显式环境变量或存在的默认目录才纳入，避免给无关目录开口子。
    for key in [
        "CARGO_HOME",
        "RUSTUP_HOME",
        "PNPM_HOME",
        "NPM_CONFIG_PREFIX",
    ] {
        if let Some(value) = std::env::var_os(key) {
            if !value.is_empty() {
                paths.push(PathBuf::from(value));
            }
        }
    }
    if let Some(home) = std::env::var_os("USERPROFILE") {
        let home = PathBuf::from(home);
        for name in [".cargo", ".rustup"] {
            let candidate = home.join(name);
            if candidate.is_dir() {
                paths.push(candidate);
            }
        }
    }

    let mut seen = HashSet::new();
    let paths: Vec<PathBuf> = paths
        .into_iter()
        .filter(|path| path.is_dir())
        .filter(|path| seen.insert(normalize(path)))
        .collect();
    // 嵌套目录已被父目录的继承 ACE 覆盖，不再单独改写一次 DACL。
    drop_nested(paths)
}

/// 去掉被其他条目严格包含的路径（ACE 带 `SUB_CONTAINERS_AND_OBJECTS_INHERIT`，父目录已覆盖）。
fn drop_nested(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort_by_key(|path| path.components().count());
    let mut kept: Vec<PathBuf> = Vec::new();
    for path in paths {
        if kept
            .iter()
            .any(|parent| path != *parent && path.starts_with(parent))
        {
            continue;
        }
        kept.push(path);
    }
    kept
}

/// 「盘根以下 → 父目录」的完整祖先链，最外层在前，**不含自己、不含盘根**。
///
/// 只给这些层级补 `(RA,S)`：容器里的构建工具会逐段 `fs::metadata` 探测路径，
/// 任何一级返回「拒绝访问」都会让整条链断掉（`glob-0.3.4` 直接 `.flatten()` 丢掉，
/// 连错误都不报）。盘根本身不写：`glob` 只 stat 盘根以下每一段，盘根写了没用
/// （2026-11-12 实测，`C:\`/`D:\` 全裸也能跑通冷构建）。
/// 这里不做存在性判断，调用方按需过滤——纯函数便于对 UNC / 盘根做单测。
pub fn ancestor_chain(dir: &Path) -> Vec<PathBuf> {
    ancestor_chain_until(dir, None)
}

/// 同 [`ancestor_chain`]，但止于 `stop_at` 的下一级（**不含** `stop_at` 自身）。
///
/// 只给用例用：验证时在 `%TEMP%` 下自造链条，不能让写入一路顶到盘根
/// （`C:\Users\<user>` 这类目录上一次 DACL 写入要几十秒，且普通用户写不进）。
/// 生产路径（后台预置）走 `None`，即整条链。
fn ancestor_chain_until(dir: &Path, stop_at: Option<&Path>) -> Vec<PathBuf> {
    let mut chain: Vec<PathBuf> = Vec::new();
    let mut current = normalize(dir);
    while let Some(parent) = current.parent() {
        let parent = normalize(parent);
        // 盘根/共享根的 `parent()` 可能回到自身；用相等判断兜住，避免死循环。
        if parent.as_os_str().is_empty() || parent == current {
            break;
        }
        // 盘根本身不写（`parent()` 为 `None` 即盘根）：真正被 stat 的是盘根以下每一级，
        // 盘根写了没用（2026-11-12 实测：`C:\`/`D:\` 全裸也能跑通冷构建）；而且盘根
        // owner 是 TrustedInstaller/SYSTEM、普通用户没有 `WRITE_DAC`，写进去只会把
        // `ancestors` 状态永久钉在 `needs_admin`。
        if parent.parent().is_none() {
            break;
        }
        chain.push(parent.clone());
        current = parent;
    }
    chain.reverse();
    if let Some(stop) = stop_at {
        if let Some(index) = chain.iter().position(|path| same_path(path, stop)) {
            return chain.split_off(index + 1);
        }
    }
    chain
}

impl Access {
    /// 权限包含关系：`Write ⊇ Read ⊇ Ancestor`。
    fn covers(self, other: Access) -> bool {
        matches!(
            (self, other),
            (Access::Write, _)
                | (Access::Read, Access::Read | Access::Ancestor)
                | (Access::Ancestor, Access::Ancestor)
        )
    }
}

/// 授权清单里每一项的角色：决定它是否受「过宽根」约束。
///
/// 宽不宽看**目录本身**，不看它从哪个配置键写进来：主目录当工作区根太宽，
/// 当 `allowed_roots` 一样宽——换个键就能绕过的守卫等于没有守卫。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootRole {
    /// 用户自己指定的目录（工作区根、cwd、`allowed_roots`）：过宽即拒。
    UserPicked,
    /// 程序推导出来的目录（工具链、MCP 临时目录）：本来就可能落在
    /// `Program Files` 这类系统目录下，不受过宽约束。
    Derived,
}

/// 收敛一份授权清单：同路径取最强档位与**最严角色**，被祖先目录以「不更弱」权限覆盖的条目去掉。
///
/// 只为少改 DACL 与少绕一层锁：每个 MCP server 的清单会同时包含工作区根、
/// 其子目录与工具链目录，逐条授权会重复改写同一个安全描述符。
pub fn collapse(entries: Vec<(PathBuf, Access, RootRole)>) -> Vec<(PathBuf, Access, RootRole)> {
    let mut merged: Vec<(PathBuf, Access, RootRole)> = Vec::new();
    for (path, access, role) in entries {
        let path = normalize(&path);
        match merged.iter_mut().find(|(existing, _, _)| *existing == path) {
            Some((_, existing, existing_role)) => {
                if !existing.covers(access) {
                    *existing = access;
                }
                // 同一路径既被用户指定又被程序推导：按用户指定判（更严）。
                if role == RootRole::UserPicked {
                    *existing_role = RootRole::UserPicked;
                }
            }
            None => merged.push((path, access, role)),
        }
    }
    merged.retain(|(path, _, _)| path.is_dir());
    merged.sort_by_key(|(path, _, _)| path.components().count());
    let mut kept: Vec<(PathBuf, Access, RootRole)> = Vec::new();
    for (path, access, role) in merged {
        let covered = kept.iter().any(|(parent, parent_access, _)| {
            path != *parent && path.starts_with(parent) && parent_access.covers(access)
        });
        if !covered {
            kept.push((path, access, role));
        }
    }
    kept
}

pub struct AclManager {
    backup_dir: PathBuf,
    audit_path: PathBuf,
    /// 已授权项：`(规范化路径, SID 字符串, 档位)`。
    ///
    /// **档位必须进键**：祖先链先记 `Ancestor`，同一路径随后又作为授权根要 `Write` 时，
    /// 只按 `(路径, SID)` 命中缓存会把 `Write` 静默跳过——DACL 上只剩一条只路过的 ACE，
    /// 工作区反而写不进去。命中条件用 `Access::covers`，强档位可以顶替弱档位。
    granted: HashSet<(PathBuf, String, Access)>,
    /// 本进程见过的**持久**写权限：`(路径, SID 文本)`。
    ///
    /// 只给 `restore_all` 收尾复查用：如果某次备份还原把持久 ACE 一并抹掉了，
    /// 退出日志要能看见（下一次启动会重新补齐，不在退出路径重写 DACL）。
    persistent: Vec<(PathBuf, String)>,
    recovered_stale: usize,
}

impl AclManager {
    pub fn new(backup_dir: PathBuf) -> Self {
        // 审计文件放在 `sandbox/` 下（与 `acl/` 备份目录同级）。
        let audit_path = backup_dir
            .parent()
            .map(|parent| parent.join(AUDIT_FILE))
            .unwrap_or_else(|| backup_dir.join(AUDIT_FILE));
        Self {
            backup_dir,
            audit_path,
            granted: HashSet::new(),
            persistent: Vec::new(),
            recovered_stale: 0,
        }
    }

    /// 上次异常退出留下的备份：先还原，再允许新的授权。
    /// 返回还原条数；任何一条失败都返回 Err（fail-closed）。
    pub fn recover_pending(&mut self) -> Result<usize, String> {
        if !self.backup_dir.is_dir() {
            return Ok(0);
        }
        let mut restored = 0usize;
        let mut failures: Vec<String> = Vec::new();
        let entries = std::fs::read_dir(&self.backup_dir).map_err(|e| {
            format!(
                "读取 ACL 备份目录失败（{}）: {e}",
                self.backup_dir.display()
            )
        })?;
        // 先全部读出并**按对象层级由浅到深**排序，再逐个还原。
        //
        // 顺序不是细节：`SetNamedSecurityInfoW` 写回未保护 DACL 时，系统会按父目录重新
        // 计算继承。子目录先还原、父目录后还原的话，子目录写回那一刻父目录还带着我们的
        // 祖先 ACE，继承重算会把那条 ACE 重新灌进来——还原结果比备份多一条授权。
        let mut pending: Vec<(usize, PathBuf, Vec<u8>)> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| format!("读取 ACL 备份项失败: {e}"))?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some(BACKUP_EXT) {
                continue;
            }
            let blob = match std::fs::read(&path) {
                Ok(blob) => blob,
                Err(error) => {
                    failures.push(format!("{}: 读取备份文件失败（{error}）", path.display()));
                    continue;
                }
            };
            match parse_backup(&blob) {
                Ok((object, _)) => pending.push((object.components().count(), path, blob)),
                Err(error) => failures.push(format!("{}: {error}", path.display())),
            }
        }
        pending.sort_by_key(|(depth, _, _)| *depth);
        for (_, path, blob) in pending {
            match restore_backup_blob(&path, &blob) {
                Ok(()) => restored += 1,
                Err(error) => failures.push(format!("{}: {error}", path.display())),
            }
        }
        if !failures.is_empty() {
            return Err(format!(
                "上次异常退出留下的 ACL 备份还原失败（拒绝在未还原的 DACL 上继续）：{}",
                failures.join("; ")
            ));
        }
        self.recovered_stale = restored;
        if restored > 0 {
            tracing::info!(restored, "已还原上次异常退出遗留的目录 DACL 备份");
        }
        Ok(restored)
    }

    /// 给**用户指定的根**加 ACE：先挡住过宽根，再走普通授权。
    ///
    /// 覆盖三类用户指定入口（工作区根、cwd、`allowed_roots`）——它们共用同一套判定，
    /// 否则换个配置键就能绕过。工具链目录走 `grant`：`node.exe` 在
    /// `Program Files\nodejs` 是子目录，只有 `Program Files` 本身作为根才拒。
    pub fn grant_guarded(&mut self, dir: &Path, sid: PSID, access: Access) -> Result<(), String> {
        self.grant_guarded_entry(dir, sid, access).map(|_| ())
    }

    /// [`grant_guarded`] 的回报版：`true` = 本次真的改了 DACL，`false` = 已在位。
    ///
    /// 后台预置要区分「写了」与「本来就有」：前者首条命令才可能变快，
    /// 后者说明上一次已经写过（持久化生效），两者的日志含义完全不同。
    pub(crate) fn grant_guarded_entry(
        &mut self,
        dir: &Path,
        sid: PSID,
        access: Access,
    ) -> Result<bool, String> {
        let normalized = normalize(dir);
        if let Some(reason) = overbroad_root_reason(&normalized) {
            let sid_text = sid_to_string(sid)?;
            // 拒绝必须落进审计：只 warn 一行日志，用户与后来人都看不到它发生过。
            self.audit(
                "grant",
                &normalized,
                &sid_text,
                access.label(),
                "refused",
                Some(&reason),
            );
            return Err(reason);
        }
        self.grant_entry(dir, sid, access, SUB_CONTAINERS_AND_OBJECTS_INHERIT, false)
    }

    /// 工作区根 / temp / 工具链的**持久**写权限入口（不写备份、不随退出还原）。
    ///
    /// 口径与祖先 `(RA,S)` 完全一致：按真实 DACL 探测（命中即跳过，毫秒级）、
    /// 写成功才追加账本（供卸载按 `existed_before=false` 精确删除）。
    ///
    /// 为什么不再备份：这四处每次启动重写一次，本机约 40s（工作区根 24s 占大头），
    /// 用户看不见但磁盘在抖。dsh 的做法是写一次常驻复用（README:130），我们采用同一口径。
    /// 备份只保留给**会随会话消失**的授权（MCP 每 server 一个容器 SID）。
    ///
    /// `guarded` = 该路径是**用户指定的根**（挡过宽根）；temp / 工具链目录传 `false`。
    pub(crate) fn grant_scope_persistent(
        &mut self,
        dir: &Path,
        sid: PSID,
        guarded: bool,
        ledger: &Path,
    ) -> Result<ScopeGrant, String> {
        let normalized = normalize(dir);
        if !normalized.is_dir() {
            return Err(format!(
                "不能给非目录路径加沙箱 ACE：{}",
                normalized.display()
            ));
        }
        let sid_text = sid_to_string(sid)?;
        if guarded {
            if let Some(reason) = overbroad_root_reason(&normalized) {
                self.audit(
                    "grant",
                    &normalized,
                    &sid_text,
                    Access::Write.label(),
                    "refused",
                    Some(&reason),
                );
                return Err(reason);
            }
        }
        // 进程内缓存：同一进程重复 exec 不再问 DACL。
        if self
            .granted
            .iter()
            .any(|(path, granted_sid, granted_access)| {
                *path == normalized
                    && *granted_sid == sid_text
                    && granted_access.covers(Access::Write)
            })
        {
            return Ok(ScopeGrant::AlreadyPresent);
        }
        let mask = Access::Write.mask();
        if has_explicit_grant(&normalized, sid, mask)? {
            tracing::info!(
                dir = %normalized.display(),
                "目录已有容器 SID 的显式 ACE，跳过改写（持久授权命中）"
            );
            self.remember_persistent(&normalized, &sid_text);
            self.granted.insert((normalized, sid_text, Access::Write));
            return Ok(ScopeGrant::AlreadyPresent);
        }

        // 写之前记一次「这个 SID 有没有任何显式 ACE」：卸载只许删 `existed_before=false` 的。
        let existed_before = has_any_explicit_grant(&normalized, sid)?;
        match apply_grant(&normalized, sid, mask, SUB_CONTAINERS_AND_OBJECTS_INHERIT) {
            Ok(()) => {
                self.audit(
                    "grant",
                    &normalized,
                    &sid_text,
                    Access::Write.label(),
                    "ok",
                    None,
                );
                self.remember_persistent(&normalized, &sid_text);
                self.granted
                    .insert((normalized.clone(), sid_text, Access::Write));
                if let Err(error) = append_ledger(
                    ledger,
                    LEDGER_KIND_SCOPE,
                    &normalized,
                    sid,
                    mask,
                    true,
                    existed_before,
                ) {
                    // ACE 已生效；账本丢了只影响卸载能否精确回滚这一处。
                    tracing::warn!(
                        path = %normalized.display(),
                        %error,
                        "写权限已持久化但账本写入失败（卸载时无法精确回滚这一处）"
                    );
                }
                Ok(ScopeGrant::Written { existed_before })
            }
            // 没有 `WRITE_DAC`（系统目录）：交给调用方决定降级还是 fail-closed。
            Err(failure) if failure.access_denied => {
                self.audit(
                    "grant",
                    &normalized,
                    &sid_text,
                    Access::Write.label(),
                    "denied",
                    Some(&failure.message),
                );
                Ok(ScopeGrant::Denied(failure.message))
            }
            Err(failure) => {
                self.audit(
                    "grant",
                    &normalized,
                    &sid_text,
                    Access::Write.label(),
                    "error",
                    Some(&failure.message),
                );
                Err(failure.message)
            }
        }
    }

    /// 给目录加容器 SID 的 ACE。备份失败 → 直接失败，**不改安全描述符**。
    pub fn grant(&mut self, dir: &Path, sid: PSID, access: Access) -> Result<(), String> {
        self.grant_entry(dir, sid, access, SUB_CONTAINERS_AND_OBJECTS_INHERIT, false)
            .map(|_| ())
    }

    /// 给授权根加 ACE，并给「盘根以下 → 父目录」每一级补 `(RA,S)`。
    ///
    /// 祖先条目**不带继承**（`ACE_FLAGS(0)`）——但别指望它能省掉重传播：本机实测
    /// 不继承 ACE 仍让系统重算整棵子树（`D:\MyWorkspace` 25.6s / `D:\MyWorkspace\SHANIU`
    /// 25.4s，与子树大小关系不大）。
    ///
    /// 祖先 DACL 不可写（`C:\Users` 属 SYSTEM，普通用户没有 `WRITE_DAC`）时**降级**：
    /// 审计记 `denied` + 一行 warn，继续执行——少一条只买「读属性」的 ACE 只会更严。
    ///
    /// **2026-11-12：不在 exec 路径上调用**。这条路径会写备份、会随退出还原，
    /// 天生是「每次启动重写一遍」。生产改走 [`ensure_ancestor_grant`] + 后台预置
    /// （`mod::provision_ancestors_in_background`），持久化、不写备份。
    /// 这里保留给用例与显式的一次性授权。
    pub fn grant_chain(&mut self, dir: &Path, sid: PSID, access: Access) -> Result<(), String> {
        self.grant(dir, sid, access)?;
        self.grant_ancestors(dir, sid, None)
    }

    /// `grant_guarded` 的祖先链版本（用户指定的根：先挡过宽根，再授权 + 补祖先）。
    pub fn grant_guarded_chain(
        &mut self,
        dir: &Path,
        sid: PSID,
        access: Access,
    ) -> Result<(), String> {
        self.grant_guarded(dir, sid, access)?;
        self.grant_ancestors(dir, sid, None)
    }

    /// 逐级补 `(RA,S)`。`stop_at` 见 [`ancestor_chain_until`]，生产路径传 `None`。
    fn grant_ancestors(
        &mut self,
        dir: &Path,
        sid: PSID,
        stop_at: Option<&Path>,
    ) -> Result<(), String> {
        for ancestor in ancestor_chain_until(dir, stop_at) {
            // 祖先按磁盘真实层级构造；并发下被删掉的层级没有可授权的对象，跳过。
            if !ancestor.is_dir() {
                continue;
            }
            self.grant_entry(&ancestor, sid, Access::Ancestor, ACE_FLAGS(0), true)?;
        }
        Ok(())
    }

    /// 授权实现：返回 `true` = 本次真的改了 DACL，`false` = 无需改动（已授权 / 祖先降级）。
    fn grant_entry(
        &mut self,
        dir: &Path,
        sid: PSID,
        access: Access,
        inheritance: ACE_FLAGS,
        tolerate_denied: bool,
    ) -> Result<bool, String> {
        let normalized = normalize(dir);
        if !normalized.is_dir() {
            return Err(format!(
                "不能给非目录路径加沙箱 ACE：{}",
                normalized.display()
            ));
        }
        let sid_text = sid_to_string(sid)?;
        if self
            .granted
            .iter()
            .any(|(path, granted_sid, granted_access)| {
                *path == normalized && *granted_sid == sid_text && granted_access.covers(access)
            })
        {
            return Ok(false);
        }
        let mask = access.mask();

        // 幂等：磁盘上已有该容器 SID 的显式 ACE 且掩码够用时，不改写 DACL、不写备份。
        // 这种 ACE 来自上次运行残留或外部手工设置，视为外部授权——退出时也不撤销，
        // 避免误删同时运行的另一个实例正在使用的授权。
        if has_explicit_grant(&normalized, sid, mask)? {
            tracing::info!(
                dir = %normalized.display(),
                "目录已有容器 SID 的显式 ACE，跳过改写（幂等，避免触发文件监听器重扫）"
            );
            self.granted.insert((normalized, sid_text, access));
            return Ok(false);
        }

        let snapshot = read_dacl_snapshot(&normalized)?;
        let backup_path = self.backup_path(&normalized);
        // 同一路径可能有多个 SID（每个 MCP server 一个容器）：只保留**最早**那份原始 DACL，
        // 否则后一次授权会把备份覆盖成「已有前一个 ACE」的状态，退出还原就不干净了。
        let wrote_backup = if backup_path.exists() {
            false
        } else {
            write_backup_file(&backup_path, &normalized, &snapshot).map_err(|error| {
                self.audit(
                    "grant",
                    &normalized,
                    &sid_text,
                    access.label(),
                    "error",
                    Some(&error),
                );
                format!(
                    "备份原始 DACL 失败（{}）：{error}。按规则不改安全描述符，命令拒绝在沙箱外执行",
                    normalized.display()
                )
            })?;
            true
        };

        if let Err(error) = apply_grant(&normalized, sid, mask, inheritance) {
            if should_degrade(tolerate_denied, &error) {
                // 祖先链专用：权限不足只会「少给」，不会放宽，因此降级而不是 fail-closed。
                //
                // 拒绝访问只可能发生在「读 DACL」或「写 DACL」被拒——`SetNamedSecurityInfoW`
                // 是原子的，被拒就不可能留下半改的 DACL，所以这里**不**回滚（回滚本身也会被
                // 同样拒掉）。备份一并清掉，避免下次启动重放一份相同快照。
                let backup_note = backup_rollback_note(wrote_backup, &backup_path);
                // 降级也必须可观测：审计 + 一行 warn，绝不静默跳过。
                self.audit(
                    "grant",
                    &normalized,
                    &sid_text,
                    access.label(),
                    "denied",
                    Some(&format!("{}{backup_note}", error.message)),
                );
                tracing::warn!(
                    dir = %normalized.display(),
                    "祖先目录 DACL 不可写，降级跳过：容器内对该级的路径探测可能返回拒绝访问"
                );
                self.granted.insert((normalized, sid_text, access));
                return Ok(false);
            }
            // 已经改了一半的风险：立刻按备份还原，再报错。
            let restore = restore_from_snapshot(&normalized, &snapshot);
            // 备份删不掉 = 下次启动 `recover_pending` 会重放这份「授权前快照」，
            // 把此后对同一目录的合法授权一并抹掉。不能 `let _`：并进错误文案，
            // 让调用方知道 DACL 之外还留了一个待处理的备份。
            let backup_note = backup_rollback_note(wrote_backup, &backup_path);
            self.audit(
                "grant",
                &normalized,
                &sid_text,
                access.label(),
                "error",
                Some(&format!("{}{backup_note}", error.message)),
            );
            return Err(match restore {
                Ok(()) => format!(
                    "给「{}」加沙箱 ACE 失败，已按备份还原：{error}{}{backup_note}",
                    normalized.display(),
                    grant_hint(error.access_denied)
                ),
                Err(restore_error) => format!(
                    "给「{}」加沙箱 ACE 失败，且按备份还原也失败（需人工检查 DACL）：{error}{}；{restore_error}{backup_note}",
                    normalized.display(),
                    grant_hint(error.access_denied)
                ),
            });
        }

        self.audit("grant", &normalized, &sid_text, access.label(), "ok", None);
        self.granted.insert((normalized, sid_text, access));
        Ok(true)
    }

    /// 撤销某个 `(路径, SID)` 的显式 ACE。**只删这一条**，其它 trustee 的 ACE 一个不动。
    /// 返回是否真的删掉了东西（本来就没有 → false，不算错误）。
    pub fn revoke(&mut self, dir: &Path, sid: PSID) -> Result<bool, String> {
        let normalized = normalize(dir);
        let sid_text = sid_to_string(sid)?;
        if !normalized.is_dir() {
            return Ok(false);
        }
        let removed = revoke_ace(&normalized, sid).inspect_err(|error| {
            self.audit("revoke", &normalized, &sid_text, "-", "error", Some(error));
        })?;
        // 先记账、再清备份。`granted` 必须反映磁盘现状（ACE 此刻确实已经撤销）：
        // 留着旧条目会让后续同键 `grant` 命中 `contains` 直接返回 Ok，留下一条
        // 「已授权」却没有 ACE 的假记录——那比备份残留更难发现。
        // 同一路径可能同时记着 `Ancestor`（祖先链）与 `Write`（授权根）两条，按路径+SID 全清。
        self.granted
            .retain(|(path, granted_sid, _)| !(*path == normalized && *granted_sid == sid_text));
        // 主动撤销的持久授权不再需要退出复查（否则 `restore_all` 会误报「被抹掉」）。
        self.persistent
            .retain(|(path, known_sid)| !(*path == normalized && *known_sid == sid_text));
        let other_grants = self.granted.iter().any(|(path, _, _)| *path == normalized);
        if !other_grants {
            // 该路径上已无其它授权 → 备份文件也删掉，否则下次启动 `recover_pending`
            // 会把它原样重放，把刚撤销的 ACE 又写回去。
            //
            // 删失败**不能吞**：审计会记 revoke ok，而重启后的 DACL 与用户看到的结果相反。
            // 这里**不**以 `removed` 为前提：上一次失败可能正是「ACE 已撤、备份没删掉」，
            // 用户重试时 `revoke_ace` 返回 false，遗留备份仍必须清掉。
            let backup = self.backup_path(&normalized);
            if let Err(message) = remove_backup(&backup) {
                self.audit(
                    "revoke",
                    &normalized,
                    &sid_text,
                    "-",
                    "error",
                    Some(&message),
                );
                return Err(format!(
                    "撤销「{}」的沙箱 ACE 已生效，但{message}；\
                     不删掉备份的话，下次启动会把它还原回来，请关闭占用该文件的程序后重试",
                    normalized.display()
                ));
            }
        }
        if removed {
            self.audit("revoke", &normalized, &sid_text, "-", "ok", None);
        }
        Ok(removed)
    }

    /// 按备份还原全部已授权目录。返回还原条数。
    pub fn restore_all(&mut self) -> Result<usize, String> {
        let restored = self.recover_pending()?;
        // 备份快照可能早于持久写权限（例如 MCP 先授权、exec 后授权），还原会把持久 ACE
        // 一并抹掉。这里**不重写** DACL——退出路径不能被 25s 的写入卡住——只把事实说清楚，
        // 下一次启动按真实 DACL 探测时自会补齐。
        for (path, sid_text) in &self.persistent {
            let missing = match super::appcontainer::capability_sid(sid_text) {
                Ok(sid) => {
                    has_explicit_grant(path, sid.as_psid(), Access::Write.mask()).map(|hit| !hit)
                }
                Err(_) => Ok(true),
            };
            if matches!(missing, Ok(true)) {
                tracing::warn!(
                    path = %path.display(),
                    "持久写权限被本次备份还原抹掉（下一次启动会重新补齐）"
                );
            }
        }
        Ok(restored)
    }

    /// 记下「这一处是本进程见过的持久授权」，供 `restore_all` 收尾复查用。
    fn remember_persistent(&mut self, path: &Path, sid_text: &str) {
        if self
            .persistent
            .iter()
            .any(|(known_path, known_sid)| known_path == path && known_sid == sid_text)
        {
            return;
        }
        self.persistent
            .push((path.to_path_buf(), sid_text.to_string()));
    }

    /// 是否从异常退出中接管了 profile（用于退出时删除陈旧容器）。
    pub fn recovered_stale(&self) -> usize {
        self.recovered_stale
    }

    fn backup_path(&self, dir: &Path) -> PathBuf {
        self.backup_dir.join(format!(
            "{:016x}.{BACKUP_EXT}",
            hash_path(&dir.to_string_lossy())
        ))
    }

    /// 每次授权/撤销写一条审计（JSONL，追加）。写失败只记日志，不阻断安全操作。
    fn audit(
        &self,
        action: &str,
        path: &Path,
        sid: &str,
        access: &str,
        result: &str,
        error: Option<&str>,
    ) {
        let record = serde_json::json!({
            "ts": now_ms(),
            "action": action,
            "path": path.display().to_string(),
            "sid": sid,
            "access": access,
            "result": result,
            "error": error,
        });
        if let Some(parent) = self.audit_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.audit_path)
        {
            Ok(mut file) => {
                use std::io::Write;
                if let Err(error) = writeln!(file, "{record}") {
                    tracing::warn!(%error, "写 ACL 审计文件失败");
                }
            }
            Err(error) => tracing::warn!(
                path = %self.audit_path.display(),
                %error,
                "打开 ACL 审计文件失败"
            ),
        }
        tracing::info!(action, path = %path.display(), sid, access, result, "目录授权审计");
    }
}

/// SID → `S-1-15-2-...` 文本（审计与幂等键都用它）。
fn sid_to_string(sid: PSID) -> Result<String, String> {
    let mut text = PWSTR::null();
    unsafe { ConvertSidToStringSidW(sid, &mut text) }
        .map_err(|error| format!("SID 转字符串失败: {error}"))?;
    let value = unsafe { text.to_string() }.map_err(|error| format!("SID 字符串解码失败: {error}"));
    unsafe {
        let _ = LocalFree(Some(HLOCAL(text.0.cast::<c_void>())));
    }
    value
}

/// `\\?\D:\ws` → `D:\ws`，并尽量取 canonical 形式。
///
/// `pub(crate)`：写约束模型的身份派生（`sandbox::write_sid`）必须用同一套规范化，
/// 否则同一目录的两种拼写会派生出两个写 SID（docs/16 §1.2）。
pub(crate) fn normalize(path: &Path) -> PathBuf {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    match canonical.to_string_lossy().strip_prefix(r"\\?\") {
        Some(stripped) => PathBuf::from(stripped),
        None => canonical,
    }
}

/// 同一路径？两侧都先规范化（`..`、尾斜杠、`\\?\`、junction 会收敛到同一形态），
/// 再按 Windows 口径忽略大小写——`canonicalize` 对不存在的路径会原样返回，
/// 那时大小写不一定与磁盘一致，不能只靠字符串相等。
pub fn same_path(a: &Path, b: &Path) -> bool {
    normalize(a).to_string_lossy().to_lowercase() == normalize(b).to_string_lossy().to_lowercase()
}

/// 过宽根判定（对外：读环境变量取主目录与系统目录）。
///
/// 过宽根不是「慢」，是**范围错**：把它授给容器等于把凭据、浏览器数据、
/// 整盘内容一起交出去，远远超过工具需要。
fn overbroad_root_reason(path: &Path) -> Option<String> {
    let home = crate::config::home_dir();
    let mut system_dirs: Vec<PathBuf> = Vec::new();
    for (key, fallback) in [
        ("SystemRoot", Some("C:\\Windows")),
        ("ProgramFiles", Some("C:\\Program Files")),
        ("ProgramFiles(x86)", Some("C:\\Program Files (x86)")),
    ] {
        match std::env::var_os(key).filter(|value| !value.is_empty()) {
            Some(value) => system_dirs.push(PathBuf::from(value)),
            None => {
                if let Some(value) = fallback {
                    system_dirs.push(PathBuf::from(value));
                }
            }
        }
    }
    overbroad_root_reason_with(path, Some(&home), &system_dirs)
}

/// 纯函数部分：调用方给出主目录与系统目录，便于测试不依赖真实环境。
///
/// **只比规范化后的最终路径**，不做字符串前缀匹配——前缀匹配漏掉的形态
/// （`C:\Users\jun\`、`C:\Users\jun\..\jun`、`\\?\C:\Users\jun`、指向主目录的
/// junction）都会在 [`normalize`] 里收敛成同一形态。
fn overbroad_root_reason_with(
    path: &Path,
    home: Option<&Path>,
    system_dirs: &[PathBuf],
) -> Option<String> {
    if home.is_some_and(|home| same_path(path, home)) {
        return Some(format!(
            "拒绝把「{}」作为工作区根授权：这是用户主目录，下面有凭据（.ssh、\
             浏览器与 AppData 里的登录态）等与工具无关的数据，授出去的范围远超工具需要。\
             请改选它下面的具体项目目录，例如「{}\\work」。这不是权限不足，是本工具的边界。",
            path.display(),
            path.display()
        ));
    }
    if is_drive_or_share_root(path) {
        return Some(format!(
            "拒绝把「{}」作为工作区根授权：这是盘符/共享根，等于把整块盘的内容都交给容器。\
             请改选具体的项目目录。这不是权限不足，是本工具的边界。",
            path.display()
        ));
    }
    if system_dirs.iter().any(|dir| same_path(path, dir)) {
        return Some(format!(
            "拒绝把「{}」作为工作区根授权：这是 Windows 系统目录（Windows / Program Files），\
             不是项目目录；其中的程序与数据不属于本工具的作用面。请改选你自己的项目目录。",
            path.display()
        ));
    }
    None
}

/// 盘符根（`C:\`）或 UNC 共享根（`\\server\share`）：只有前缀 + 根，没有下级成分。
fn is_drive_or_share_root(path: &Path) -> bool {
    use std::path::Component;
    let mut components = path.components();
    let prefixed = matches!(components.next(), Some(Component::Prefix(_)));
    let rooted = matches!(components.next(), Some(Component::RootDir));
    prefixed && rooted && components.next().is_none()
}

fn hash_path(path: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.to_lowercase().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

/// `SE_DACL_PROTECTED`（winnt.h）：DACL 已保护，父目录的继承 ACE 不再灌入。
const SE_DACL_PROTECTED: u16 = 0x1000;

/// 读取自相对安全描述符头里的控制位（`SECURITY_DESCRIPTOR_RELATIVE.Control`，偏移 2）。
fn snapshot_control(snapshot: &[u8]) -> Result<u16, String> {
    snapshot
        .get(2..4)
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
        .ok_or_else(|| format!("安全描述符过短（{} 字节），无法读取控制位", snapshot.len()))
}

/// 读取命名对象的安全描述符（自相对格式）并复制成字节，供备份/回滚使用。
pub(crate) fn read_dacl_snapshot(path: &Path) -> Result<Vec<u8>, String> {
    let name = wide(path.as_os_str());
    let mut descriptor = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(name.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut dacl),
            None,
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!(
            "读取「{}」的 DACL 失败（Win32 错误 {}）",
            path.display(),
            status.0
        ));
    }
    let length = unsafe { GetSecurityDescriptorLength(descriptor) } as usize;
    if length == 0 {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        }
        return Err(format!("「{}」的安全描述符长度为 0", path.display()));
    }
    let bytes = unsafe { std::slice::from_raw_parts(descriptor.0.cast::<u8>(), length) }.to_vec();
    unsafe {
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }
    Ok(bytes)
}

/// 目录上是否已存在容器 SID 的**显式**（非继承）允许 ACE，且掩码覆盖我们要求的全部权限。
/// 幂等判定用：已存在就不再改写 DACL（改写会触发文件监听器重扫，见 docs/04 §1.5）。
pub(crate) fn has_explicit_grant(path: &Path, sid: PSID, mask: u32) -> Result<bool, String> {
    let name = wide(path.as_os_str());
    let mut descriptor = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(name.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut dacl),
            None,
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!(
            "读取「{}」的 DACL 做幂等判定失败（Win32 错误 {}）",
            path.display(),
            status.0
        ));
    }
    let result = unsafe { scan_acl_for_grant(dacl, sid, mask) };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }
    result
}

/// 该对象上是否已有容器 SID 的**任何**显式允许 ACE（不看掩码覆盖）。
///
/// 只给账本的 `existed_before` 用：掩码不覆盖 ≠ 没有别人的 ACE。只有这里返回
/// `false` 时，新写的那条才是「我们独创的」，卸载/回滚时才允许删。
pub(crate) fn has_any_explicit_grant(path: &Path, sid: PSID) -> Result<bool, String> {
    // 掩码传 0：`Mask & 0 != 0` 恒假，等于「任意掩码都算命中」。
    has_explicit_grant(path, sid, 0)
}

unsafe fn scan_acl_for_grant(dacl: *mut ACL, sid: PSID, mask: u32) -> Result<bool, String> {
    if dacl.is_null() {
        // NULL DACL = 所有人完全控制。这种状态本身异常，不往上叠 ACE 掩盖它。
        return Err("目录 DACL 为 NULL（所有人完全控制），拒绝在其上叠加沙箱 ACE".into());
    }
    let mut size = ACL_SIZE_INFORMATION::default();
    unsafe {
        GetAclInformation(
            dacl,
            &mut size as *mut ACL_SIZE_INFORMATION as *mut c_void,
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    }
    .map_err(|e| format!("读取 ACL 信息失败: {e}"))?;

    for index in 0..size.AceCount {
        let mut raw: *mut c_void = std::ptr::null_mut();
        unsafe { GetAce(dacl, index, &mut raw) }
            .map_err(|e| format!("读取第 {index} 条 ACE 失败: {e}"))?;
        let ace = raw.cast::<ACCESS_ALLOWED_ACE>();
        let header = unsafe { (*ace).Header };
        // AceType 0 = ACCESS_ALLOWED_ACE_TYPE（windows crate 未导出该常量）。
        if header.AceType != 0 {
            continue;
        }
        // 只认显式 ACE：继承来的（父目录下发的）不代表本目录被单独授权。
        if header.AceFlags & INHERITED_ACE.0 as u8 != 0 {
            continue;
        }
        if unsafe { (*ace).Mask } & mask != mask {
            continue;
        }
        let ace_sid = unsafe { std::ptr::addr_of_mut!((*ace).SidStart) }.cast::<c_void>();
        if unsafe { EqualSid(PSID(ace_sid), sid) }.is_ok() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `apply_grant` 的失败：`access_denied` 让调用方区分「没有写 DACL 的权限」
/// （设备对象只有管理员能改，可降级上报）与真正的错误（fail-closed）。
pub(crate) struct GrantFailure {
    pub(crate) access_denied: bool,
    pub(crate) message: String,
}

impl std::fmt::Display for GrantFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// 祖先链授权失败是否**降级**（继续跑），而不是 fail-closed。
///
/// 两个条件同时成立才降级：调用方明确允许降级（只有祖先链这么调），且失败原因是
/// 「拒绝访问」。其它失败（DACL 结构异常、备份写不进、路径不是目录……）仍然
/// fail-closed——那些可能留下半改状态，不能当成「少给一条权限」处理。
fn should_degrade(tolerate_denied: bool, failure: &GrantFailure) -> bool {
    tolerate_denied && failure.access_denied
}

/// 授权被拒时补一句「用户能照做什么」。只影响报错文案，不改授权流程：
/// 失败仍然 fail-closed，也不会为了让命令跑起来而绕过沙箱。
fn grant_hint(access_denied: bool) -> &'static str {
    if access_denied {
        "。原因：这个目录的权限当前用户改不了——它通常属于系统目录，或是由管理员权限安装的软件\
         （Windows 错误 5 = 拒绝访问）。可行的做法：把要运行的程序连同它依赖的解释器复制到你自己的\
         目录（例如工作区或用户目录），再把配置里的 command 指到新位置后重试；如果必须用现在这个\
         目录，需要请管理员给当前用户加上修改权限。"
    } else {
        ""
    }
}

pub(crate) fn apply_grant(
    path: &Path,
    sid: PSID,
    mask: u32,
    inheritance: windows::Win32::Security::ACE_FLAGS,
) -> Result<(), GrantFailure> {
    let denied = |message: String, status: windows::Win32::Foundation::WIN32_ERROR| GrantFailure {
        access_denied: status == windows::Win32::Foundation::ERROR_ACCESS_DENIED,
        message,
    };
    let name = wide(path.as_os_str());
    let mut descriptor = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
    let mut old_dacl: *mut ACL = std::ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(name.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut old_dacl),
            None,
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(denied(
            format!("读取 DACL 失败（Win32 错误 {}）", status.0),
            status,
        ));
    }

    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: mask,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: inheritance,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: PWSTR(sid.0.cast::<u16>()),
        },
    };

    let mut new_acl: *mut ACL = std::ptr::null_mut();
    let status = unsafe {
        let old = if old_dacl.is_null() {
            None
        } else {
            Some(old_dacl as *const ACL)
        };
        SetEntriesInAclW(Some(std::slice::from_ref(&entry)), old, &mut new_acl)
    };
    if status != ERROR_SUCCESS {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        }
        return Err(denied(
            format!("构造新 DACL 失败（Win32 错误 {}）", status.0),
            status,
        ));
    }

    let status = unsafe {
        SetNamedSecurityInfoW(
            PCWSTR(name.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(new_acl as *const ACL),
            None,
        )
    };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(new_acl.cast::<c_void>())));
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }
    if status != ERROR_SUCCESS {
        return Err(denied(
            format!(
                "写入「{}」的新 DACL 失败（Win32 错误 {}）",
                path.display(),
                status.0
            ),
            status,
        ));
    }
    Ok(())
}

/// 精确撤销：只删这一条 `(路径, SID)` 的显式 ACE，其它 trustee 的 ACE 原样保留。
/// 返回是否真的删掉了东西。
///
/// `pub(crate)`：设备对象（`\Device\Null`，SH-064 B）也要按 SID 精确撤销，
/// 但它没有 `exists()`、也不进目录备份台账，所以走不了 `AclManager::revoke`。
pub(crate) fn revoke_ace(path: &Path, sid: PSID) -> Result<bool, String> {
    let name = wide(path.as_os_str());
    let before = read_dacl_snapshot(path)?;

    let mut descriptor = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
    let mut old_dacl: *mut ACL = std::ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(name.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut old_dacl),
            None,
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!(
            "读取「{}」的 DACL 失败（Win32 错误 {}）",
            path.display(),
            status.0
        ));
    }

    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: 0,
        grfAccessMode: REVOKE_ACCESS,
        grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: PWSTR(sid.0.cast::<u16>()),
        },
    };

    let mut new_acl: *mut ACL = std::ptr::null_mut();
    let status = unsafe {
        let old = if old_dacl.is_null() {
            None
        } else {
            Some(old_dacl as *const ACL)
        };
        SetEntriesInAclW(Some(std::slice::from_ref(&entry)), old, &mut new_acl)
    };
    if status != ERROR_SUCCESS {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        }
        return Err(format!(
            "构造「{}」的撤销 DACL 失败（Win32 错误 {}）",
            path.display(),
            status.0
        ));
    }

    let status = unsafe {
        SetNamedSecurityInfoW(
            PCWSTR(name.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(new_acl as *const ACL),
            None,
        )
    };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(new_acl.cast::<c_void>())));
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }
    if status != ERROR_SUCCESS {
        return Err(format!(
            "撤销「{}」的 ACE 失败（Win32 错误 {}）",
            path.display(),
            status.0
        ));
    }

    // DACL 真的变了才算删掉了东西（SID 本来就没有 ACE 时 SetEntriesInAcl 幂等）。
    Ok(read_dacl_snapshot(path)
        .map(|after| after != before)
        .unwrap_or(true))
}

pub(crate) fn write_backup_file(path: &Path, object: &Path, snapshot: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建 ACL 备份目录失败（{}）: {e}", parent.display()))?;
    }
    let object = object.to_string_lossy();
    let mut blob: Vec<u8> = Vec::with_capacity(16 + object.len() + snapshot.len());
    blob.extend_from_slice(BACKUP_MAGIC);
    blob.extend_from_slice(&(object.len() as u32).to_le_bytes());
    blob.extend_from_slice(object.as_bytes());
    blob.extend_from_slice(&(snapshot.len() as u32).to_le_bytes());
    blob.extend_from_slice(snapshot);

    let temp = path.with_extension("tmp");
    std::fs::write(&temp, &blob)
        .map_err(|e| format!("写入 ACL 备份临时文件失败（{}）: {e}", temp.display()))?;
    std::fs::rename(&temp, path)
        .map_err(|e| format!("提交 ACL 备份文件失败（{}）: {e}", path.display()))
}

/// 删掉一份 DACL 备份。**备份不存在算成功**（幂等跳过路径本来就没写备份），
/// 存在但删不掉必须上抛：下次启动 `recover_pending` 会无条件重放它。
fn remove_backup(backup: &Path) -> Result<(), String> {
    match std::fs::remove_file(backup) {
        Ok(()) => Ok(()),
        Err(_) if !backup.exists() => Ok(()),
        Err(error) => Err(format!(
            "删除 DACL 备份失败（{}）：{error}",
            backup.display()
        )),
    }
}

/// `grant` 失败回滚时清掉刚写的备份，返回要并进错误文案的补充说明（成功 / 没写过 → 空串）。
/// 单独成函数是为了让「删不掉时的完整文案」能被直接断言——这条路径在 `grant` 里
/// 需要同时满足「写过备份 + 应用 DACL 失败 + 备份被占用」，没有注入点无法复现。
fn backup_rollback_note(wrote_backup: bool, backup: &Path) -> String {
    if !wrote_backup {
        return String::new();
    }
    match remove_backup(backup) {
        Ok(()) => String::new(),
        Err(note) => {
            format!("；且{note}。下次启动会按它还原，可能覆盖此后对同一目录的合法授权，请手工删除")
        }
    }
}

fn restore_backup_blob(path: &Path, blob: &[u8]) -> Result<(), String> {
    let (object, snapshot) = parse_backup(blob)?;
    if !object.exists() {
        std::fs::remove_file(path).ok();
        return Ok(());
    }
    restore_from_snapshot(&object, snapshot)?;
    // 删备份与 `remove_backup` 同口径：文件已经被删掉 = 目标达成。
    // 祖先链让**多个进程/多个工作区**共用同一批备份（`C:\Users\<user>` 等对谁都是祖先），
    // 并发启动时另一个进程可能已经重放过并删除了同一个文件；此时报错会把
    // 「已经干净」误判成「未还原」，整个沙箱 fail-closed 起不来。
    remove_backup(path)
}

pub(crate) fn parse_backup(blob: &[u8]) -> Result<(PathBuf, &[u8]), String> {
    if blob.len() < 16 || &blob[..8] != BACKUP_MAGIC {
        return Err("备份文件头不合法（不是 SHANIU 的 DACL 备份）".into());
    }
    let object_len = u32::from_le_bytes(blob[8..12].try_into().unwrap()) as usize;
    let object_end = 12 + object_len;
    if blob.len() < object_end + 4 {
        return Err("备份文件被截断（路径段）".into());
    }
    let object = std::str::from_utf8(&blob[12..object_end])
        .map_err(|e| format!("备份文件里的路径不是 UTF-8: {e}"))?;
    let snapshot_len =
        u32::from_le_bytes(blob[object_end..object_end + 4].try_into().unwrap()) as usize;
    let snapshot_start = object_end + 4;
    if blob.len() < snapshot_start + snapshot_len {
        return Err("备份文件被截断（安全描述符段）".into());
    }
    Ok((
        PathBuf::from(object),
        &blob[snapshot_start..snapshot_start + snapshot_len],
    ))
}

pub(crate) fn restore_from_snapshot(path: &Path, snapshot: &[u8]) -> Result<(), String> {
    let name = wide(path.as_os_str());
    let descriptor = PSECURITY_DESCRIPTOR(snapshot.as_ptr() as *mut c_void);
    let mut present = windows::core::BOOL(0);
    let mut defaulted = windows::core::BOOL(0);
    let mut dacl: *mut ACL = std::ptr::null_mut();
    unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) }
        .map_err(|e| format!("解析备份的安全描述符失败: {e}"))?;

    // 还原必须连「DACL 是否保护」一起还原：2026-11-09 实测，快照带
    // SE_DACL_PROTECTED（`icacls /inheritance:r` 后的目录就是这种）时，若按
    // UNPROTECTED 写回，父目录的继承 ACE 会被重新灌入——28 字节的空 DACL
    // 还原成 4572 字节，等于**凭空多给出一批权限**。
    let protection = if snapshot_control(snapshot)? & SE_DACL_PROTECTED != 0 {
        PROTECTED_DACL_SECURITY_INFORMATION
    } else {
        UNPROTECTED_DACL_SECURITY_INFORMATION
    };

    let status = unsafe {
        SetNamedSecurityInfoW(
            PCWSTR(name.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | protection,
            None,
            None,
            if present.as_bool() {
                Some(dacl as *const ACL)
            } else {
                None
            },
            None,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!(
            "按备份还原「{}」的 DACL 失败（Win32 错误 {}）",
            path.display(),
            status.0
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 祖先链持久预置（docs/14）
// ---------------------------------------------------------------------------

/// 单级祖先的预置结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AncestorGrant {
    /// 本次真的写了 DACL。`existed_before` 记的是**写之前**该对象上有没有容器 SID
    /// 的任何显式 ACE（哪怕掩码不够）——账本据此区分「我们独创的」与「在别人的基础上加码」。
    Written { existed_before: bool },
    /// 磁盘上已有覆盖 `(RA,S)` 的显式 ACE（上次运行写的，或外部授权）——零写入。
    AlreadyPresent,
    /// 当前用户对该级没有 `WRITE_DAC`（`C:\Users` 这类系统拥有的层级）：需要一次管理员。
    Denied(String),
}

impl AncestorGrant {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Written { .. } => "written",
            Self::AlreadyPresent => "present",
            Self::Denied(_) => "denied",
        }
    }
}

/// 工作区根 / temp / 工具链的持久写权限结果（与 [`AncestorGrant`] 同构）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeGrant {
    /// 本次真的写了 DACL；`existed_before` 记写之前该 SID 有没有任何显式 ACE。
    Written { existed_before: bool },
    /// 真实 DACL 已覆盖 `Write` 掩码——零写入。
    AlreadyPresent,
    /// 当前用户没有该目录的 `WRITE_DAC`：调用方决定降级还是 fail-closed。
    Denied(String),
}

impl ScopeGrant {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Written { .. } => "written",
            Self::AlreadyPresent => "present",
            Self::Denied(_) => "denied",
        }
    }
}

/// 给一级祖先补 `(RA,S)`：**不写备份**（持久化，绝不随退出还原），先按真实 DACL 判缺。
///
/// 判定口径：这一级对象上有没有容器 SID 的**显式** ACE 且掩码覆盖 `(RA,S)`。
/// 有就跳过（实测 1–2ms），没有才写；写不进去只可能因为没有 `WRITE_DAC`，
/// 按 `Denied` 上报而不是 fail-closed——少一条只买「读属性」的 ACE 只会更严，不会放宽。
pub fn ensure_ancestor_grant(dir: &Path, sid: PSID) -> Result<AncestorGrant, String> {
    if !dir.is_dir() {
        return Err(format!("祖先路径不是目录：{}", dir.display()));
    }
    if has_explicit_grant(dir, sid, ANCESTOR_ACCESS_MASK)? {
        return Ok(AncestorGrant::AlreadyPresent);
    }
    // 写之前记一次「有没有别人的 ACE」：掩码不覆盖也算，这样账本不把别人的授权记成我们的。
    let existed_before = has_any_explicit_grant(dir, sid)?;
    // 祖先专用 `ACE_FLAGS(0)`：不继承（可继承 ACE 会被转发进整棵子树）。
    match apply_grant(dir, sid, ANCESTOR_ACCESS_MASK, ACE_FLAGS(0)) {
        Ok(()) => Ok(AncestorGrant::Written { existed_before }),
        Err(failure) if failure.access_denied => Ok(AncestorGrant::Denied(failure.message)),
        Err(failure) => Err(failure.message),
    }
}

/// 持久预置账本路径。**故意不放 `acl/` 子目录**：那一层是 DACL 备份台账，
/// `recover_pending` 会把里面的文件当成备份重放。
pub fn persistent_ledger_path(data_root: &Path) -> PathBuf {
    data_root.join("sandbox").join("ancestor-ace.jsonl")
}

/// 账本里「祖先 `(RA,S)`」与「持久写权限」两类记录用 `kind` 区分。
///
/// 老版本写的行没有 `kind` 字段，按祖先处理（见 docs/14 §六 的回滚步骤）。
pub const LEDGER_KIND_ANCESTOR: &str = "ancestor";
pub const LEDGER_KIND_SCOPE: &str = "scope";

/// 追加一条账本记录：只记「我们加的、加之前没有」，供卸载/回滚使用。
///
/// `existed_before` 必须如实——别人的 ACE 记成我们的，卸载时就会删掉不属于我们的授权。
/// 运行时**不读**这个账本做判断（判缺一律按真实 DACL 探测）。
pub fn append_persistent_ledger(
    ledger: &Path,
    object: &Path,
    sid: PSID,
    existed_before: bool,
) -> Result<(), String> {
    append_ledger(
        ledger,
        LEDGER_KIND_ANCESTOR,
        object,
        sid,
        ANCESTOR_ACCESS_MASK,
        false,
        existed_before,
    )
}

/// 账本的唯一写入实现：`kind` / `mask` / `inherit` 都如实落盘，
/// 卸载时只凭这几个字段就能判断该删哪一条、删之前要不要先确认掩码。
fn append_ledger(
    ledger: &Path,
    kind: &str,
    object: &Path,
    sid: PSID,
    mask: u32,
    inherit: bool,
    existed_before: bool,
) -> Result<(), String> {
    if let Some(parent) = ledger.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建 ACE 账本目录失败（{}）: {e}", parent.display()))?;
    }
    let line = serde_json::json!({
        "kind": kind,
        "path": object.to_string_lossy(),
        "sid": sid_to_string(sid)?,
        "mask": mask,
        "inherit": inherit,
        "existed_before": existed_before,
        "added_at_ms": now_ms(),
    });
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(ledger)
        .map_err(|e| format!("打开 ACE 账本失败（{}）: {e}", ledger.display()))?;
    {
        use std::io::Write as _;
        writeln!(file, "{line}")
            .map_err(|e| format!("写 ACE 账本失败（{}）: {e}", ledger.display()))?;
    }
    Ok(())
}

/// 账本回放的结果（卸载/撤销时给调用方与日志看）。
#[derive(Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct LedgerRevokeReport {
    /// 去重后、`existed_before=false` 的条目数（真正要考虑撤销的）。
    pub considered: usize,
    /// 真的删掉了 ACE 的条目。
    pub revoked: usize,
    /// 目标本来就没有这条 ACE（重复调用 / 已被别处删掉），DACL 无变化。
    pub unchanged: usize,
    /// 路径已经不存在：跳过，不当失败（目录可能已被用户删掉）。
    pub missing: usize,
    /// 逐条失败原因；单条失败不阻断其余条目。
    pub failed: Vec<String>,
}

impl LedgerRevokeReport {
    pub fn is_clean(&self) -> bool {
        self.failed.is_empty()
    }
}

/// 按账本精确撤销我们加过的授权（卸载/回滚用）。
///
/// 口径（对齐 codex，**不做 DACL 快照还原**——快照会把别人在这段时间里新加的
/// 授权一起抹掉，且跨版本结构不同就无法还原）：
/// 1. 只处理 `existed_before == false` 的条目：别人本来就有的 ACE 一律不碰；
/// 2. 按 `(path, sid)` 去重，同一个对象只撤一次；
/// 3. 用 `REVOKE_ACCESS` 只删该 trustee 的 ACE，其它 trustee 原样保留；
/// 4. 幂等：目标已没有这条 ACE 时算成功（`unchanged`）；
/// 5. 路径不存在只计数（`missing`），不当失败。
///
/// 账本不存在 = 没有任何我们加的授权，返回全 0 报告。
pub fn revoke_from_ledger(ledger: &Path) -> LedgerRevokeReport {
    let mut report = LedgerRevokeReport::default();
    let body = match std::fs::read_to_string(ledger) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return report,
        Err(error) => {
            report
                .failed
                .push(format!("读取 ACE 账本失败（{}）: {error}", ledger.display()));
            return report;
        }
    };

    let mut seen: HashSet<(String, String)> = HashSet::new();
    for (index, line) in body.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(error) => {
                // 我们把路径写坏了也不会因此拒绝撤销别的条目——如实报出来即可。
                report
                    .failed
                    .push(format!("账本第 {} 行不是合法 JSON: {error}", index + 1));
                continue;
            }
        };
        if value.get("existed_before").and_then(|v| v.as_bool()) != Some(false) {
            continue;
        }
        let (Some(path), Some(sid_text)) = (
            value.get("path").and_then(|v| v.as_str()),
            value.get("sid").and_then(|v| v.as_str()),
        ) else {
            report
                .failed
                .push(format!("账本第 {} 行缺 path/sid", index + 1));
            continue;
        };
        if !seen.insert((path.to_string(), sid_text.to_string())) {
            continue;
        }
        report.considered += 1;

        let target = Path::new(path);
        if !target.exists() {
            report.missing += 1;
            continue;
        }
        let raw = match sid_from_text(sid_text) {
            Ok(raw) => raw,
            Err(error) => {
                report.failed.push(error);
                continue;
            }
        };
        let outcome = revoke_ace(target, raw);
        unsafe {
            let _ = LocalFree(Some(HLOCAL(raw.0)));
        }
        match outcome {
            Ok(true) => report.revoked += 1,
            Ok(false) => report.unchanged += 1,
            Err(error) => report.failed.push(error),
        }
    }
    report
}

/// SID 文本 → 二进制 SID。调用方负责 `LocalFree`。
fn sid_from_text(text: &str) -> Result<PSID, String> {
    let wide_text = wide(std::ffi::OsStr::new(text));
    let mut raw = PSID(std::ptr::null_mut());
    unsafe {
        ConvertStringSidToSidW(PCWSTR(wide_text.as_ptr()), &mut raw)
    }
    .map_err(|error| format!("SID 文本「{text}」转二进制失败: {error}"))?;
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> crate::test_support::TestTempDir {
        crate::test_support::TestTempDir::new(&format!("acl-{tag}"))
    }

    #[test]
    fn backup_blob_round_trips_path_and_snapshot() {
        let object = PathBuf::from(r"D:\ws\demo");
        let snapshot = vec![1u8, 2, 3, 4, 5];
        let dir = temp_dir("blob");
        let file = dir.join("probe.dacl");
        write_backup_file(&file, &object, &snapshot).unwrap();
        let blob = std::fs::read(&file).unwrap();
        let (parsed_path, parsed_snapshot) = parse_backup(&blob).unwrap();
        assert_eq!(parsed_path, object);
        assert_eq!(parsed_snapshot, snapshot.as_slice());
    }

    #[test]
    fn corrupted_backup_is_rejected() {
        let mut blob = Vec::from(*BACKUP_MAGIC);
        blob.extend_from_slice(&999u32.to_le_bytes());
        assert!(parse_backup(&blob).is_err());
    }

    #[test]
    fn write_scope_contains_only_existing_dirs_and_dedupes() {
        let ws = temp_dir("scope");
        let scope = write_scope(&ws);
        // 测试目录建在 %TEMP% 下：父目录条目已覆盖它，按范围收敛规则不应再单列。
        assert!(
            !scope.iter().any(|path| normalize(path) == normalize(&ws)),
            "被父目录 ACE 覆盖的工作区根不应单独出现"
        );
        assert!(scope.iter().all(|path| path.is_dir()));
        let unique: HashSet<PathBuf> = scope.iter().map(|p| normalize(p)).collect();
        assert_eq!(unique.len(), scope.len(), "write_scope 不应返回重复目录");
    }

    #[test]
    fn grant_requires_backup_dir_to_be_writable() {
        // 备份目录被一个同名文件占住 → create_dir_all 失败 → 拒绝授权，且不改 DACL。
        let ws = temp_dir("nobackup");
        let blocked = ws.join("blocked");
        std::fs::write(&blocked, b"x").unwrap();
        let mut manager = AclManager::new(blocked.clone());
        let sid = super::super::appcontainer::internet_client_capability().unwrap();
        let target = ws.join("target");
        std::fs::create_dir_all(&target).unwrap();
        let error = manager
            .grant(&target, sid.as_psid(), Access::Write)
            .expect_err("备份失败时必须拒绝改动安全描述符");
        assert!(error.contains("备份原始 DACL 失败"), "{error}");
    }

    #[test]
    fn nested_scope_paths_are_dropped_in_favor_of_parent_ace() {
        let root = PathBuf::from(r"D:\ws");
        let kept = drop_nested(vec![
            root.join("sub"),
            root.clone(),
            PathBuf::from(r"D:\other"),
            root.join("sub").join("toolchain"),
        ]);
        assert_eq!(kept, vec![root, PathBuf::from(r"D:\other")]);
    }

    /// 祖先链是纯函数：盘根 → 父目录，最外层在前、不含自己；盘根/共享根没有上级。
    #[test]
    fn ancestor_chain_runs_from_the_root_down_to_the_parent() {
        assert_eq!(
            ancestor_chain(Path::new(r"C:\Users\jun\AppData\Local\Temp")),
            vec![
                PathBuf::from(r"C:\Users"),
                PathBuf::from(r"C:\Users\jun"),
                PathBuf::from(r"C:\Users\jun\AppData"),
                PathBuf::from(r"C:\Users\jun\AppData\Local"),
            ],
            "盘根不写：写它既没用（实测 `C:\\` 全裸也能跑通冷构建），普通用户也没有它的 WRITE_DAC"
        );
        assert!(ancestor_chain(Path::new(r"D:\")).is_empty(), "盘根没有祖先");
        assert!(
            ancestor_chain(Path::new(r"\\server\share")).is_empty(),
            "共享根没有祖先"
        );
        // 用例的上界口径：截到上界的下一级，上界自身与其以上一概不返回。
        assert_eq!(
            ancestor_chain_until(
                Path::new(r"C:\Users\jun\AppData\Local\Temp\l1\l2"),
                Some(Path::new(r"C:\Users\jun\AppData\Local\Temp"))
            ),
            vec![PathBuf::from(r"C:\Users\jun\AppData\Local\Temp\l1")]
        );
    }

    /// 降级只对「祖先链 + 拒绝访问」成立；其它组合必须 fail-closed。
    #[test]
    fn degrade_only_for_ancestor_chain_and_access_denied() {
        let denied = GrantFailure {
            access_denied: true,
            message: "拒绝访问".into(),
        };
        let other = GrantFailure {
            access_denied: false,
            message: "不是权限问题".into(),
        };
        assert!(should_degrade(true, &denied), "祖先链遇到拒绝访问应降级");
        assert!(
            !should_degrade(false, &denied),
            "非祖先链调用（授权根本身）必须 fail-closed，不得因错少给而放行"
        );
        assert!(
            !should_degrade(true, &other),
            "非权限错误不得降级：可能留下半改状态"
        );
        assert!(!should_degrade(false, &other));
    }

    /// 断言失败也把 DACL 还原回去：本用例会真实改写目录 DACL，
    /// panic 时把 ACE 留在临时目录里比测试红更糟。
    struct RestoreGuard<'a> {
        manager: &'a mut AclManager,
        done: bool,
    }

    impl<'a> RestoreGuard<'a> {
        fn new(manager: &'a mut AclManager) -> Self {
            Self {
                manager,
                done: false,
            }
        }

        /// 正常收尾：由用例断言还原条数，`Drop` 不再重复还原。
        fn finish(mut self) -> Result<usize, String> {
            self.done = true;
            self.manager.restore_all()
        }
    }

    impl Drop for RestoreGuard<'_> {
        fn drop(&mut self) {
            if !self.done {
                let _ = self.manager.restore_all();
            }
        }
    }

    /// 祖先链必须真的落到可写层级上：每一级一条 `Ancestor`（不含列目录/读内容），
    /// 退出后按备份逐级还原。
    ///
    /// 只在 `%TEMP%` 下**自造链条**（上界钉在临时根），不向盘根写：给 `C:\Users\<user>`
    /// 这类目录写 DACL 会触发整棵子树的继承重传播（本机实测 62s），而 `C:\`、`C:\Users`
    /// 在非提权下根本写不进，既危险又验证不了「真的被加上」。
    ///
    /// 降级分支（写不进 → 审计 `denied` + 单条 warn）在本机无法于可写层级内自造
    /// 「`WRITE_DAC` 被拒」的目录——给自己加 Deny 会连恢复也一起锁死，不能留在用例里。
    /// 该分支由 `should_degrade` 的判定单测覆盖，端到端证据是真机审计记录（见 docs/13）。
    #[test]
    fn grant_chain_covers_ancestors_and_restores_them() {
        let ws = temp_dir("chain");
        let leaf = ws.join("l1").join("l2").join("l3");
        std::fs::create_dir_all(&leaf).unwrap();
        let l1 = ws.join("l1");
        let l2 = l1.join("l2");
        let before_l1 = read_dacl_snapshot(&l1).unwrap();
        let before_l2 = read_dacl_snapshot(&l2).unwrap();

        let sid = super::super::appcontainer::internet_client_capability().unwrap();
        let mut manager = AclManager::new(ws.join("backups"));
        // 与 `grant_chain` 同序（先授权根、再逐级补祖先），只是把上界钉在 `ws`：
        // `%TEMP%` 及以上的层级一条都不写。
        manager.grant(&leaf, sid.as_psid(), Access::Write).unwrap();
        manager
            .grant_ancestors(&leaf, sid.as_psid(), Some(&ws))
            .unwrap();
        let guard = RestoreGuard::new(&mut manager);

        // 能写 DACL 的祖先必须真的拿到「只路过」ACE，且没有被升级成可列举/可读。
        for path in [&l1, &l2] {
            assert!(
                has_explicit_grant(path, sid.as_psid(), Access::Ancestor.mask()).unwrap(),
                "祖先 {} 应有容器 SID 的 Ancestor ACE",
                path.display()
            );
            assert!(
                !has_explicit_grant(path, sid.as_psid(), Access::Read.mask()).unwrap(),
                "祖先只能只路过，不得升级成可列举/可读：{}",
                path.display()
            );
        }
        assert!(has_explicit_grant(&leaf, sid.as_psid(), Access::Write.mask()).unwrap());

        // 每一级祖先都要有交代：要么落一条 `ok`/`denied` 审计，要么本来就已有该 ACE
        // （幂等跳过不改 DACL，靠 DACL 状态本身可查）。不许的是「没授权、也没记录」。
        let audit = std::fs::read_to_string(ws.join("acl-audit.jsonl")).unwrap();
        let records: Vec<serde_json::Value> = audit
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        for ancestor in ancestor_chain_until(&leaf, Some(&ws)) {
            let needle = ancestor.display().to_string();
            let matched: Vec<&serde_json::Value> = records
                .iter()
                .filter(|record| {
                    record["action"] == "grant"
                        && record["access"] == "ancestor"
                        && record["path"] == needle.as_str()
                })
                .collect();
            if matched.is_empty() {
                assert!(
                    has_explicit_grant(&ancestor, sid.as_psid(), Access::Ancestor.mask())
                        .unwrap_or(false),
                    "祖先既没有授权审计、磁盘上也没有 ACE（静默跳过）：{needle}（审计：{audit}）"
                );
                continue;
            }
            assert_eq!(
                matched.len(),
                1,
                "每一级祖先应恰好一条审计：{needle}（审计：{audit}）"
            );
            let result = matched[0]["result"].as_str().unwrap();
            assert!(
                result == "ok" || result == "denied",
                "祖先授权结果只能是 ok/denied（{needle} → {result}）"
            );
        }
        for record in &records {
            if record["result"] == "denied" {
                eprintln!(
                    "[chain] denied: {} ({})",
                    record["path"].as_str().unwrap(),
                    record["access"].as_str().unwrap()
                );
            }
        }
        let backups = std::fs::read_dir(ws.join("backups")).unwrap().count();
        eprintln!(
            "[chain] 审计 {} 条（ok {}），备份文件 {} 个",
            records.len(),
            records
                .iter()
                .filter(|record| record["result"] == "ok")
                .count(),
            backups
        );
        for record in &records {
            eprintln!(
                "[chain] {} {} {}",
                record["access"].as_str().unwrap(),
                record["result"].as_str().unwrap(),
                record["path"].as_str().unwrap()
            );
        }

        let restored = guard.finish().unwrap();
        eprintln!("[chain] 还原 {restored} 条备份");
        assert!(
            restored >= 2,
            "叶目录与可写祖先都应留有备份，实际还原 {restored} 条"
        );
        compare_restored(&before_l1, &read_dacl_snapshot(&l1).unwrap()).unwrap();
        compare_restored(&before_l2, &read_dacl_snapshot(&l2).unwrap()).unwrap();
        for path in [&l1, &l2] {
            assert!(
                !has_explicit_grant(path, sid.as_psid(), Access::Ancestor.mask()).unwrap(),
                "还原后祖先不应残留容器 SID 的 ACE：{}",
                path.display()
            );
        }
    }

    /// 缓存键必须带档位：同一路径先记 `Ancestor`，随后要 `Write` 时不能被缓存吞掉，
    /// 否则 DACL 上只留一条只路过 ACE，工作区写不进去。
    #[test]
    fn weak_cached_grant_does_not_swallow_a_stronger_grant() {
        let ws = temp_dir("cache-upgrade");
        let sid = super::super::appcontainer::internet_client_capability().unwrap();

        let mut manager = AclManager::new(ws.join("backups"));
        manager.grant(&ws, sid.as_psid(), Access::Ancestor).unwrap();
        assert!(
            has_explicit_grant(&ws, sid.as_psid(), Access::Ancestor.mask()).unwrap(),
            "前置条件：目录应已有 Ancestor ACE"
        );
        manager.grant(&ws, sid.as_psid(), Access::Write).unwrap();
        assert!(
            has_explicit_grant(&ws, sid.as_psid(), Access::Write.mask()).unwrap(),
            "同一路径从 Ancestor 升级到 Write 时不得被缓存吞掉"
        );
        let _ = manager.restore_all();
    }

    /// 收敛规则：父 `Write` 覆盖子 `Read`/`Write`；父 `Read` 不覆盖子 `Write`。
    #[test]
    fn collapse_keeps_only_needed_grants() {
        let root = crate::test_support::TestTempDir::new("acl-collapse");
        let child = root.join("child");
        std::fs::create_dir_all(&child).unwrap();

        let collapsed = collapse(vec![
            (root.to_path_buf(), Access::Read, RootRole::UserPicked),
            (child.clone(), Access::Read, RootRole::UserPicked),
            (child.clone(), Access::Write, RootRole::UserPicked),
        ]);
        assert!(collapsed.contains(&(normalize(&root), Access::Read, RootRole::UserPicked)));
        assert!(
            collapsed.contains(&(normalize(&child), Access::Write, RootRole::UserPicked)),
            "子目录的 Write 不得被父目录的 Read 吞掉: {collapsed:?}"
        );

        let collapsed = collapse(vec![
            (root.to_path_buf(), Access::Write, RootRole::UserPicked),
            (child.clone(), Access::Read, RootRole::UserPicked),
        ]);
        assert_eq!(
            collapsed,
            vec![(normalize(&root), Access::Write, RootRole::UserPicked)],
            "父 Write 已覆盖子 Read，子项应被收敛掉"
        );

        // 同一路径两次不同档位：取更强的一档。
        let same = collapse(vec![
            (child.clone(), Access::Read, RootRole::Derived),
            (child.clone(), Access::Write, RootRole::UserPicked),
        ]);
        assert_eq!(
            same,
            vec![(normalize(&child), Access::Write, RootRole::UserPicked)],
            "同路径同时是用户指定与程序推导时，按更严的角色判"
        );
    }

    #[test]
    fn existing_explicit_ace_is_not_rewritten_and_leaves_no_backup() {
        let ws = temp_dir("idem");
        let target = ws.join("target");
        std::fs::create_dir_all(&target).unwrap();
        let sid = super::super::appcontainer::internet_client_capability().unwrap();

        let first_backups = ws.join("backups-1");
        let mut first = AclManager::new(first_backups.clone());
        first.grant(&target, sid.as_psid(), Access::Write).unwrap();
        assert!(
            has_explicit_grant(&target, sid.as_psid(), Access::Write.mask()).unwrap(),
            "授权后目录应有容器 SID 的显式 ACE"
        );
        // 同一进程内重复授权：不重复写备份。
        first.grant(&target, sid.as_psid(), Access::Write).unwrap();
        assert_eq!(
            std::fs::read_dir(&first_backups).unwrap().count(),
            1,
            "重复授权不应重复写备份"
        );
        let after_first = read_dacl_snapshot(&target).unwrap();

        // 模拟重启/另一实例：磁盘上已有 ACE，不得再改写。
        let second_backups = ws.join("backups-2");
        let mut second = AclManager::new(second_backups.clone());
        second.grant(&target, sid.as_psid(), Access::Write).unwrap();
        assert_eq!(
            read_dacl_snapshot(&target).unwrap(),
            after_first,
            "已有显式 ACE 时不得改写 DACL"
        );
        assert!(!second_backups.exists(), "幂等跳过时不应写备份");
        assert_eq!(
            second.restore_all().unwrap(),
            0,
            "跳过改写时没有可还原的备份"
        );

        assert_eq!(first.restore_all().unwrap(), 1);
        assert!(
            !has_explicit_grant(&target, sid.as_psid(), Access::Write.mask()).unwrap(),
            "按备份还原后显式 ACE 应消失"
        );
    }

    /// 从快照里取出 DACL 的原始字节（ACL 结构本身：修订/大小/计数/ACE 序列）。
    fn dacl_bytes(snapshot: &[u8]) -> Option<Vec<u8>> {
        let descriptor = PSECURITY_DESCRIPTOR(snapshot.as_ptr() as *mut c_void);
        let mut present = windows::core::BOOL(0);
        let mut defaulted = windows::core::BOOL(0);
        let mut dacl: *mut ACL = std::ptr::null_mut();
        unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) }
            .unwrap();
        if !present.as_bool() {
            return None;
        }
        let mut info = ACL_SIZE_INFORMATION::default();
        unsafe {
            GetAclInformation(
                dacl,
                &mut info as *mut ACL_SIZE_INFORMATION as *mut c_void,
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        }
        .unwrap();
        Some(
            unsafe { std::slice::from_raw_parts(dacl as *const u8, info.AclBytesInUse as usize) }
                .to_vec(),
        )
    }

    fn header_word(snapshot: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes([
            snapshot[offset],
            snapshot[offset + 1],
            snapshot[offset + 2],
            snapshot[offset + 3],
        ])
    }

    /// 还原结果与备份的等价判定（白名单式，2026-11-09 用户定）。
    ///
    /// 只允许两类头部差异，其余任何头部差异都算失败：
    /// 1. `SE_DACL_AUTO_INHERITED` 由无变有——`SetNamedSecurityInfoW` 重算继承的
    ///    **固定**行为（非时序），本机与 CI 的初始 DACL 形态不同才会只在 CI 出现；
    /// 2. owner/group 偏移一侧为 0——`GetNamedSecurityInfoW` 只取 DACL 时返回的描述符
    ///    可能不带这两块，两个平台的返回形态不同。
    ///
    /// DACL 本体（含 ACE 顺序）必须逐字节一致；DACL 的存在性也必须一致。
    fn compare_restored(before: &[u8], after: &[u8]) -> Result<(), String> {
        const SE_DACL_AUTO_INHERITED: u16 = 0x0400;
        let (before_control, after_control) = (snapshot_control(before)?, snapshot_control(after)?);
        let normalized = after_control & !SE_DACL_AUTO_INHERITED;
        if normalized != before_control & !SE_DACL_AUTO_INHERITED {
            return Err(format!(
                "控制位差异超出白名单：before=0x{before_control:04x} after=0x{after_control:04x}"
            ));
        }
        if after_control & SE_DACL_AUTO_INHERITED != 0
            && before_control & SE_DACL_AUTO_INHERITED == 0
        {
            // 允许：Windows 重算继承时置位。反向（丢掉该位）不允许，单测覆盖。
        } else if after_control != before_control {
            return Err(format!(
                "控制位不允许的差异：before=0x{before_control:04x} after=0x{after_control:04x}"
            ));
        }
        for (name, offset) in [("owner", 4usize), ("group", 8usize)] {
            let (old, new) = (header_word(before, offset), header_word(after, offset));
            if old != new && old != 0 && new != 0 {
                return Err(format!(
                    "{name} 偏移差异超出白名单：before={old} after={new}"
                ));
            }
        }
        if header_word(before, 12) != header_word(after, 12) {
            return Err("SACL 偏移不应被还原流程改变".to_string());
        }
        match (dacl_bytes(before), dacl_bytes(after)) {
            (None, None) => Ok(()),
            (Some(old), Some(new)) if old == new => Ok(()),
            (Some(_), Some(_)) => Err("DACL 本体与备份不一致".to_string()),
            (old, new) => Err(format!(
                "DACL 存在性不一致：before={} after={}",
                old.is_some(),
                new.is_some()
            )),
        }
    }

    #[test]
    fn grant_and_restore_round_trip_changes_dacl() {
        let ws = temp_dir("roundtrip");
        let target = ws.join("target");
        std::fs::create_dir_all(&target).unwrap();
        let before = read_dacl_snapshot(&target).unwrap();

        let backups = ws.join("backups");
        let mut manager = AclManager::new(backups);
        let sid = super::super::appcontainer::internet_client_capability().unwrap();
        manager
            .grant(&target, sid.as_psid(), Access::Write)
            .unwrap();

        let after = read_dacl_snapshot(&target).unwrap();
        assert_ne!(before, after, "加 ACE 后 DACL 应发生变化");

        let restored = manager.restore_all().unwrap();
        assert_eq!(restored, 1);
        let final_snapshot = read_dacl_snapshot(&target).unwrap();
        compare_restored(&before, &final_snapshot).unwrap();
        // 负向：还原后 DACL 仍在，且容器 SID 的 ACE 确实没了。
        assert!(
            dacl_bytes(&final_snapshot).is_some(),
            "还原后 DACL 不应消失"
        );
        assert!(
            !has_explicit_grant(&target, sid.as_psid(), Access::Write.mask()).unwrap(),
            "还原后不应残留容器 SID 的 ACE"
        );
    }

    /// 真实还原牙齿（本机可跑）：目录带 `SE_DACL_PROTECTED` 时，还原必须连保护位
    /// 一起还原。2026-11-09 之前按 `UNPROTECTED` 写回，父目录的继承 ACE 会被重新
    /// 灌入（本机实测 28 字节 → 4572 字节），等于凭空多给权限。
    #[test]
    fn restore_preserves_protected_dacl() {
        let ws = temp_dir("protected");
        let target = ws.join("target");
        std::fs::create_dir_all(&target).unwrap();
        let status = std::process::Command::new("icacls")
            .arg(&target)
            .arg("/inheritance:r")
            .status()
            .expect("需要 icacls 才能构造无继承的目录");
        assert!(status.success(), "icacls /inheritance:r 失败：{status}");

        let before = read_dacl_snapshot(&target).unwrap();
        assert!(
            snapshot_control(&before).unwrap() & SE_DACL_PROTECTED != 0,
            "前置条件不成立：icacls /inheritance:r 后 DACL 应受保护（control=0x{:04x}）",
            snapshot_control(&before).unwrap()
        );

        let sid = super::super::appcontainer::internet_client_capability().unwrap();
        let mut manager = AclManager::new(ws.join("backups"));
        manager
            .grant(&target, sid.as_psid(), Access::Write)
            .unwrap();
        assert_eq!(manager.restore_all().unwrap(), 1);

        let final_snapshot = read_dacl_snapshot(&target).unwrap();
        compare_restored(&before, &final_snapshot).unwrap();
        assert!(
            snapshot_control(&final_snapshot).unwrap() & SE_DACL_PROTECTED != 0,
            "还原后 DACL 保护位丢失（继承 ACE 会被重新灌入）"
        );
    }

    /// 比较器自身的单测：白名单只放行两类差异，其它头部差异与 DACL 差异必须失败。
    #[test]
    fn restore_comparator_whitelists_only_known_normalizations() {
        let ws = temp_dir("compare");
        let target = ws.join("target");
        std::fs::create_dir_all(&target).unwrap();
        let base = read_dacl_snapshot(&target).unwrap();
        compare_restored(&base, &base).expect("同一份快照必须判等");

        let control = snapshot_control(&base).unwrap();
        let dacl_offset = header_word(&base, 16) as usize;

        // 放行：只多了 SE_DACL_AUTO_INHERITED。
        let mut added = base.clone();
        added[2..4].copy_from_slice(&(control | 0x0400).to_le_bytes());
        compare_restored(&base, &added).expect("AUTO_INHERITED 置位属白名单");

        // 拒绝：其它控制位变化。
        let mut other_bit = base.clone();
        other_bit[2..4].copy_from_slice(&(control | 0x1000).to_le_bytes());
        assert!(
            compare_restored(&base, &other_bit).is_err(),
            "其它控制位变化必须失败"
        );

        // 拒绝：owner 偏移两侧都非零且不相等。
        let mut left = base.clone();
        let mut right = base.clone();
        left[4..8].copy_from_slice(&8u32.to_le_bytes());
        right[4..8].copy_from_slice(&12u32.to_le_bytes());
        assert!(
            compare_restored(&left, &right).is_err(),
            "owner 偏移非零且不同必须失败"
        );

        // 拒绝：DACL 本体哪怕差一个字节。
        let mut dacl_flipped = base.clone();
        let dacl_len = dacl_bytes(&base).unwrap().len();
        let last = dacl_offset + dacl_len - 1;
        assert!(
            last < dacl_flipped.len(),
            "DACL 越过描述符边界，前置条件不成立"
        );
        dacl_flipped[last] ^= 0xFF;
        assert!(
            compare_restored(&base, &dacl_flipped).is_err(),
            "DACL 本体差异必须失败"
        );
    }

    /// 撤销必须按 `(路径, SID)` 精确删：另一个 SID 的 ACE 一个都不许动。
    #[test]
    fn revoke_removes_only_the_target_sid_ace() {
        let ws = temp_dir("revoke");
        let target = ws.join("target");
        std::fs::create_dir_all(&target).unwrap();
        let sid_a = super::super::appcontainer::internet_client_capability().unwrap();
        let sid_b = super::super::appcontainer::capability_sid("S-1-15-3-2").unwrap();

        let mut manager = AclManager::new(ws.join("backups"));
        manager
            .grant(&target, sid_a.as_psid(), Access::Write)
            .unwrap();
        manager
            .grant(&target, sid_b.as_psid(), Access::Write)
            .unwrap();

        assert!(manager.revoke(&target, sid_a.as_psid()).unwrap());
        assert!(
            !has_explicit_grant(&target, sid_a.as_psid(), Access::Write.mask()).unwrap(),
            "被撤销的 SID 不应还有显式 ACE"
        );
        assert!(
            has_explicit_grant(&target, sid_b.as_psid(), Access::Write.mask()).unwrap(),
            "撤销一个 SID 不得动另一个 SID 的 ACE"
        );
        assert!(
            !manager.revoke(&target, sid_a.as_psid()).unwrap(),
            "重复撤销应为幂等（返回 false）"
        );

        let audit = std::fs::read_to_string(ws.join("acl-audit.jsonl")).unwrap();
        assert!(
            audit.contains("\"action\":\"grant\""),
            "审计缺少授权记录: {audit}"
        );
        assert!(
            audit.contains("\"action\":\"revoke\""),
            "审计缺少撤销记录: {audit}"
        );

        assert_eq!(
            manager.restore_all().unwrap(),
            1,
            "还剩一份原始 DACL 备份应被还原"
        );
        assert!(
            !has_explicit_grant(&target, sid_b.as_psid(), Access::Write.mask()).unwrap(),
            "还原后另一个 SID 的 ACE 也应消失"
        );
    }

    /// 备份删不掉时撤销必须报错，而不是记 `revoke ok`：留着备份，下次启动
    /// `recover_pending` 会把刚撤销的 ACE 原样还原，而用户看到的是「已撤销」。
    #[test]
    fn revoke_reports_locked_backup_and_retry_cleans_it_up() {
        use std::os::windows::fs::OpenOptionsExt;

        let ws = temp_dir("revoke-locked");
        let target = ws.join("target");
        std::fs::create_dir_all(&target).unwrap();
        let sid = super::super::appcontainer::internet_client_capability().unwrap();
        let mut manager = AclManager::new(ws.join("backups"));
        manager
            .grant(&target, sid.as_psid(), Access::Write)
            .unwrap();
        let backup = manager.backup_path(&normalize(&target));
        assert!(backup.exists(), "授权后应留下原始 DACL 备份");

        // 独占打开（不共享删除）：`remove_file` 必然失败，不需要给产品代码加注入点。
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&backup)
            .unwrap();
        let error = manager
            .revoke(&target, sid.as_psid())
            .expect_err("备份删不掉时撤销必须报错");
        assert!(error.contains("删除 DACL 备份失败"), "{error}");
        assert!(
            error.contains(&backup.display().to_string()),
            "错误文案必须带备份路径：{error}"
        );
        assert!(
            !has_explicit_grant(&target, sid.as_psid(), Access::Write.mask()).unwrap(),
            "ACE 确实已经撤销，报错不能掩盖这个事实"
        );
        let audit = std::fs::read_to_string(ws.join("acl-audit.jsonl")).unwrap();
        let revoke_lines: Vec<&str> = audit
            .lines()
            .filter(|line| line.contains("\"action\":\"revoke\""))
            .collect();
        assert!(!revoke_lines.is_empty(), "撤销必须落审计：{audit}");
        assert!(
            revoke_lines
                .iter()
                .all(|line| !line.contains("\"result\":\"ok\"")),
            "没删掉备份不能记 ok：{audit}"
        );

        drop(lock);
        // 重试：ACE 已不在（`revoke_ace` 返回 false），但遗留备份仍必须被清掉。
        assert!(!manager.revoke(&target, sid.as_psid()).unwrap());
        assert!(
            !backup.exists(),
            "重试必须清掉遗留备份，否则下次启动会把它还原"
        );
        assert_eq!(manager.restore_all().unwrap(), 0, "没有备份可还原");
    }

    /// `grant` 失败回滚时备份删不掉：文案必须明说，否则用户不知道 DACL 之外
    /// 还留着一份会在下次启动重放的快照。
    #[test]
    fn failed_grant_reports_backup_that_could_not_be_removed() {
        use std::os::windows::fs::OpenOptionsExt;

        let ws = temp_dir("grant-rollback");
        let backup = ws.join("locked.dacl");
        std::fs::write(&backup, b"stale").unwrap();
        // 独占打开（不共享删除），`remove_file` 必然失败。
        // 只读属性不行：Rust 的 `remove_file` 在 Windows 上会先清掉该属性再删。
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&backup)
            .unwrap();

        let note = backup_rollback_note(true, &backup);
        assert!(note.contains("删除 DACL 备份失败"), "{note}");
        assert!(note.contains(&backup.display().to_string()), "{note}");
        assert!(note.contains("请手工删除"), "{note}");
        // 没写过备份时不该凭空冒出这句（幂等跳过路径没有可清理的东西）。
        assert_eq!(backup_rollback_note(false, &backup), "");
        // 备份不存在 = 成功，不算失败。
        remove_backup(&ws.join("absent.dacl")).expect("没有备份不应算失败");

        drop(lock);
        remove_backup(&backup).expect("解除占用后应能删掉");
    }

    #[test]
    fn overbroad_roots_are_judged_after_normalization() {
        let ws = temp_dir("overbroad");
        let home = ws.join("home");
        let work = home.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let home_normalized = normalize(&home);

        // 尾斜杠
        let trailing = PathBuf::from(format!("{}\\", home.display()));
        assert!(
            overbroad_root_reason_with(&normalize(&trailing), Some(&home_normalized), &[])
                .is_some(),
            "尾斜杠不能绕过"
        );
        // `..` 绕行
        let dotted = work.join("..");
        assert!(
            overbroad_root_reason_with(&normalize(&dotted), Some(&home_normalized), &[]).is_some(),
            ".. 绕行不能绕过"
        );
        // `\\?\` 前缀
        let verbatim = PathBuf::from(format!("\\\\?\\{}", home.display()));
        assert!(
            overbroad_root_reason_with(&normalize(&verbatim), Some(&home_normalized), &[])
                .is_some(),
            "\\\\?\\ 前缀不能绕过"
        );
        // 盘符根 / UNC 共享根
        assert!(overbroad_root_reason_with(Path::new(r"C:\"), None, &[]).is_some());
        assert!(overbroad_root_reason_with(Path::new(r"\\server\share"), None, &[]).is_some());
        // 系统目录
        let system = vec![PathBuf::from(r"C:\Windows")];
        assert!(
            overbroad_root_reason_with(&normalize(Path::new(r"C:\Windows")), None, &system)
                .is_some()
        );
        // 主目录下的子目录必须放行——否则把所有人都挡在外面。
        assert!(
            overbroad_root_reason_with(&normalize(&work), Some(&home_normalized), &[]).is_none(),
            "主目录的子目录不是过宽根"
        );
    }

    // （临时性能分解用例已移除：结论见 docs/13。）
    #[test]
    fn grant_guarded_refuses_home_and_leaves_audit() {
        let ws = temp_dir("refuse");
        let backups = ws.join("backups");
        let mut manager = AclManager::new(backups.clone());
        let sid = super::super::appcontainer::internet_client_capability().unwrap();
        let home = crate::config::home_dir();
        let error = manager
            .grant_guarded(&home, sid.as_psid(), Access::Read)
            .expect_err("用户主目录必须拒绝授权");
        assert!(error.contains("用户主目录"), "{error}");
        assert!(error.contains("请改选它下面的具体项目目录"), "{error}");
        let audit = std::fs::read_to_string(ws.join("acl-audit.jsonl")).unwrap();
        assert!(audit.contains("refused"), "拒绝必须落进审计：{audit}");
        assert!(audit.contains("用户主目录"), "{audit}");

        // 主目录的子目录照旧放行（临时目录本身就在用户主目录下）。
        let work = ws.join("work");
        std::fs::create_dir_all(&work).unwrap();
        manager
            .grant_guarded(&work, sid.as_psid(), Access::Read)
            .expect("主目录的子目录不是过宽根，不应被拦");
        assert!(has_explicit_grant(&work, sid.as_psid(), Access::Read.mask()).unwrap());
    }

    /// 祖先预置的持久化语义（docs/14）：写一次 → 第二次命中跳过 → 账本如实记
    /// 「写之前有没有」。**不走备份**：这里只用 `revoke_ace` 收尾，模拟卸载。
    #[test]
    fn ancestor_grant_is_persistent_idempotent_and_records_existed_before() {
        let ws = temp_dir("ancestor-persist");
        let dir = ws.join("level");
        std::fs::create_dir_all(&dir).unwrap();
        let sid = super::super::appcontainer::internet_client_capability().unwrap();
        let ledger = ws.join("ancestor-ace.jsonl");
        let before = read_dacl_snapshot(&dir).unwrap();

        // 第一次：真写，且写之前这一级没有容器 SID 的任何显式 ACE。
        let first = ensure_ancestor_grant(&dir, sid.as_psid()).unwrap();
        assert_eq!(
            first,
            AncestorGrant::Written {
                existed_before: false
            },
            "首写必须如实报告「之前没有」"
        );
        assert!(has_explicit_grant(&dir, sid.as_psid(), ANCESTOR_ACCESS_MASK).unwrap());

        // 第二次：按真实 DACL 命中，零写入（DACL 逐字节不变）。
        let after_first = read_dacl_snapshot(&dir).unwrap();
        assert_eq!(
            ensure_ancestor_grant(&dir, sid.as_psid()).unwrap(),
            AncestorGrant::AlreadyPresent
        );
        assert_eq!(
            read_dacl_snapshot(&dir).unwrap(),
            after_first,
            "命中后不得改写 DACL"
        );

        // 账本：一条记录、掩码是 `(RA,S)`、不继承、`existed_before: false`。
        append_persistent_ledger(&ledger, &dir, sid.as_psid(), false).unwrap();
        let records: Vec<serde_json::Value> = std::fs::read_to_string(&ledger)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["mask"], ANCESTOR_ACCESS_MASK);
        assert_eq!(records[0]["inherit"], false);
        assert_eq!(records[0]["existed_before"], false);
        assert_eq!(
            records[0]["path"].as_str().unwrap(),
            dir.display().to_string()
        );

        // 卸载/回滚就是删掉我们那条（持久化不还原）。
        assert!(revoke_ace(&dir, sid.as_psid()).unwrap());
        compare_restored(&before, &read_dacl_snapshot(&dir).unwrap()).unwrap();
    }

    /// 别人的 ACE 不能被记成我们的：只加「属性」这种弱于 `(RA,S)` 的显式 ACE 时，
    /// 我们仍要补写，但账本必须写 `existed_before: true`（回滚时不许删掉别人那条）。
    #[test]
    fn ancestor_grant_marks_existed_before_when_sid_already_had_an_ace() {
        let ws = temp_dir("ancestor-existed");
        let dir = ws.join("level");
        std::fs::create_dir_all(&dir).unwrap();
        let sid = super::super::appcontainer::internet_client_capability().unwrap();
        let before = read_dacl_snapshot(&dir).unwrap();

        // 先只给 `FILE_READ_ATTRIBUTES`（0x80）：够不上 `(RA,S)`，但确实是这个 SID 的 ACE。
        apply_grant(&dir, sid.as_psid(), 0x0000_0080, ACE_FLAGS(0))
            .map_err(|failure| failure.message)
            .unwrap();
        assert!(
            !has_explicit_grant(&dir, sid.as_psid(), ANCESTOR_ACCESS_MASK).unwrap(),
            "弱 ACE 不应被当成已覆盖 (RA,S)"
        );

        let grant = ensure_ancestor_grant(&dir, sid.as_psid()).unwrap();
        assert_eq!(
            grant,
            AncestorGrant::Written {
                existed_before: true
            },
            "写之前已有该 SID 的 ACE，必须记 existed_before: true"
        );
        assert!(has_explicit_grant(&dir, sid.as_psid(), ANCESTOR_ACCESS_MASK).unwrap());

        // 收尾：撤销这个 SID 的全部显式 ACE，回到原状。
        assert!(revoke_ace(&dir, sid.as_psid()).unwrap());
        compare_restored(&before, &read_dacl_snapshot(&dir).unwrap()).unwrap();
    }

    /// 工作区根 / temp / 工具链的持久写权限：首写记 `existed_before`、命中零写入、
    /// **不产生备份**（否则退出时 `restore_all` 会把它还原掉），账本按 `kind=scope` 记账。
    #[test]
    fn scope_grant_is_persistent_idempotent_and_writes_no_backup() {
        let ws = temp_dir("scope-persist");
        let dir = ws.join("workspace");
        std::fs::create_dir_all(&dir).unwrap();
        let sid = super::super::appcontainer::internet_client_capability().unwrap();
        let backup_dir = ws.join("acl");
        let ledger = ws.join("ace.jsonl");
        let before = read_dacl_snapshot(&dir).unwrap();
        let mut manager = AclManager::new(backup_dir.clone());

        let first = manager
            .grant_scope_persistent(&dir, sid.as_psid(), false, &ledger)
            .unwrap();
        assert_eq!(
            first,
            ScopeGrant::Written {
                existed_before: false
            }
        );
        assert!(has_explicit_grant(&dir, sid.as_psid(), Access::Write.mask()).unwrap());
        // 持久化 = 不写备份：备份目录要么不存在，要么是空的。
        let leftovers = std::fs::read_dir(&backup_dir)
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(leftovers, 0, "持久授权不得产生退出时会还原的备份");

        // 换一个 manager（没有进程内缓存）再问一次：必须按真实 DACL 命中、零写入。
        let after_first = read_dacl_snapshot(&dir).unwrap();
        let mut fresh = AclManager::new(backup_dir.clone());
        assert_eq!(
            fresh
                .grant_scope_persistent(&dir, sid.as_psid(), false, &ledger)
                .unwrap(),
            ScopeGrant::AlreadyPresent
        );
        assert_eq!(
            read_dacl_snapshot(&dir).unwrap(),
            after_first,
            "命中后不得改写 DACL"
        );

        // 账本：一条、`kind=scope`、掩码是 Write、可继承、existed_before=false。
        let records: Vec<serde_json::Value> = std::fs::read_to_string(&ledger)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["kind"], LEDGER_KIND_SCOPE);
        assert_eq!(records[0]["mask"], Access::Write.mask());
        assert_eq!(records[0]["inherit"], true);
        assert_eq!(records[0]["existed_before"], false);
        // 账本记的是**规范化后**的路径（卸载据此定位对象），不能拿原始 tempdir 直接比：
        // CI 的 `%TEMP%` 会带 8.3 短名（`RUNNER~1`），canonicalize 之后是长名。
        assert_eq!(
            records[0]["path"].as_str().unwrap(),
            normalize(&dir).display().to_string()
        );

        // 卸载/回滚：只按账本删我们那条，DACL 回到原状。
        assert!(revoke_ace(&dir, sid.as_psid()).unwrap());
        compare_restored(&before, &read_dacl_snapshot(&dir).unwrap()).unwrap();
    }

    /// 卸载用的账本回放：按 `(path, sid)` 精确撤销、只撤 `existed_before=false`、
    /// 重复调用幂等、路径已不存在只计数。
    #[test]
    fn revoke_from_ledger_is_precise_idempotent_and_tolerates_missing_paths() {
        let ws = temp_dir("revoke-ledger");
        let dir = ws.join("workspace");
        std::fs::create_dir_all(&dir).unwrap();
        let sid = super::super::write_sid::workspace_write_sid(&dir).unwrap();
        let ledger = ws.join("ace.jsonl");
        let mut manager = AclManager::new(ws.join("acl"));
        let before = read_dacl_snapshot(&dir).unwrap();
        manager
            .grant_scope_persistent(&dir, sid.as_psid(), false, &ledger)
            .unwrap();

        // 账本追加：同一 (path, sid) 的重复行（去重）、别人本来就有的 ACE（跳过）、
        // 一条指向已删除目录的记录（容错）。用 serde_json 拼，避免手写 Windows 路径时
        // 反斜杠被当成 JSON 转义（那会变成非法行，测的就不是撤销逻辑了）。
        let sid_text = sid.text().to_string();
        let path_text = normalize(&dir).display().to_string();
        let gone = ws.join("deleted");
        std::fs::create_dir_all(&gone).unwrap();
        let gone_sid = super::super::write_sid::workspace_write_sid(&gone).unwrap();
        let gone_text = normalize(&gone).display().to_string();
        std::fs::remove_dir_all(&gone).unwrap();
        let record = |path: &str, sid: &str, existed_before: bool| {
            serde_json::json!({
                "kind": LEDGER_KIND_SCOPE,
                "path": path,
                "sid": sid,
                "mask": 1,
                "inherit": true,
                "existed_before": existed_before,
                "added_at_ms": 1,
            })
            .to_string()
        };
        let extra = format!(
            "{}\n{}\n{}\n",
            record(&path_text, &sid_text, false),
            record(&path_text, "S-1-5-32-545", true),
            record(&gone_text, gone_sid.text(), false),
        );
        let existing = std::fs::read_to_string(&ledger).unwrap();
        std::fs::write(&ledger, format!("{existing}{extra}")).unwrap();

        let report = revoke_from_ledger(&ledger);
        assert_eq!(report.considered, 2, "两条 existed_before=false 去重后各算一次");
        assert_eq!(report.revoked, 1, "只有真实存在的那条会被删");
        assert_eq!(report.missing, 1, "已删除的目录只计数");
        assert_eq!(report.unchanged, 0);
        assert!(report.is_clean(), "缺路径不是失败：{report:?}");
        assert!(
            !has_explicit_grant(&dir, sid.as_psid(), Access::Write.mask()).unwrap(),
            "撤销后不得留下我们那条 ACE"
        );
        compare_restored(&before, &read_dacl_snapshot(&dir).unwrap()).unwrap();

        // 幂等：再跑一遍，DACL 不变、也不算失败。
        let second = revoke_from_ledger(&ledger);
        assert_eq!(second.considered, 2);
        assert_eq!(second.revoked, 0);
        assert_eq!(second.unchanged, 1);
        assert_eq!(second.missing, 1);
        assert!(second.is_clean(), "{second:?}");
        compare_restored(&before, &read_dacl_snapshot(&dir).unwrap()).unwrap();

        // 账本不存在 = 没有我们加的授权。
        let absent = revoke_from_ledger(&ws.join("no-such-ledger.jsonl"));
        assert_eq!(absent, LedgerRevokeReport::default());
    }
}
