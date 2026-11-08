//! 目录 DACL 管理（SH-022）：只给容器 SID 加写 ACE，且**先备份再改**。
//!
//! 硬约束（用户 2026-11-08 定）：
//!  - ACE 只加在**工作区根 / temp / 工具链目录**三类位置；
//!  - 授权对象是**容器 SID**，不是 Everyone / Users；
//!  - 改安全描述符前先备份原始 DACL；备份失败就不许改（不"先改了再说"）；
//!  - 卸载/退出按备份还原，避免残留 ACE 指向已删除的容器 SID。
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
use windows::Win32::Foundation::{LocalFree, HLOCAL, ERROR_SUCCESS};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW,
    EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS, SE_FILE_OBJECT,
    TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows::Win32::Security::{
    AclSizeInformation, EqualSid, GetAce, GetAclInformation, GetSecurityDescriptorDacl,
    GetSecurityDescriptorLength, ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION,
    DACL_SECURITY_INFORMATION, INHERITED_ACE, PSID, PSECURITY_DESCRIPTOR,
    SUB_CONTAINERS_AND_OBJECTS_INHERIT, UNPROTECTED_DACL_SECURITY_INFORMATION,
};
use windows::Win32::Storage::FileSystem::{
    DELETE, FILE_DELETE_CHILD, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
};

use super::windows_impl::wide;

const BACKUP_MAGIC: &[u8; 8] = b"SHNIACL1";
const BACKUP_EXT: &str = "dacl";
const AUDIT_FILE: &str = "acl-audit.jsonl";

/// 授权档位（2026-11-08 用户定：目录级勾选只有这三种）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// 列目录 + 读文件（不给写、不给删）。
    Read,
    /// 读 + 写 + 删（目录还要能删子项）。
    Write,
    /// 只给穿过父目录的权限（祖先链补洞用，不给读内容）。
    Traverse,
}

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
            // 遍历：只给穿过这一级目录的权限。
            Access::Traverse => FILE_GENERIC_EXECUTE.0,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Access::Read => "read",
            Access::Write => "write",
            Access::Traverse => "traverse",
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
    for key in ["CARGO_HOME", "RUSTUP_HOME", "PNPM_HOME", "NPM_CONFIG_PREFIX"] {
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
        if kept.iter().any(|parent| path != *parent && path.starts_with(parent)) {
            continue;
        }
        kept.push(path);
    }
    kept
}

impl Access {
    /// 权限包含关系：`Write ⊇ Read ⊇ Traverse`。
    fn covers(self, other: Access) -> bool {
        matches!(
            (self, other),
            (Access::Write, _)
                | (Access::Read, Access::Read | Access::Traverse)
                | (Access::Traverse, Access::Traverse)
        )
    }
}

/// 收敛一份授权清单：同路径取最强档位，被祖先目录以「不更弱」权限覆盖的条目去掉。
///
/// 只为少改 DACL 与少绕一层锁：每个 MCP server 的清单会同时包含工作区根、
/// 其子目录与工具链目录，逐条授权会重复改写同一个安全描述符。
pub fn collapse(entries: Vec<(PathBuf, Access)>) -> Vec<(PathBuf, Access)> {
    let mut merged: Vec<(PathBuf, Access)> = Vec::new();
    for (path, access) in entries {
        let path = normalize(&path);
        match merged.iter_mut().find(|(existing, _)| *existing == path) {
            Some((_, existing)) => {
                if !existing.covers(access) {
                    *existing = access;
                }
            }
            None => merged.push((path, access)),
        }
    }
    merged.retain(|(path, _)| path.is_dir());
    merged.sort_by_key(|(path, _)| path.components().count());
    let mut kept: Vec<(PathBuf, Access)> = Vec::new();
    for (path, access) in merged {
        let covered = kept
            .iter()
            .any(|(parent, parent_access)| {
                path != *parent && path.starts_with(parent) && parent_access.covers(access)
            });
        if !covered {
            kept.push((path, access));
        }
    }
    kept
}

pub struct AclManager {
    backup_dir: PathBuf,
    audit_path: PathBuf,
    /// 已授权项：`(规范化路径, SID 字符串)`，撤销按这个键精确匹配。
    granted: HashSet<(PathBuf, String)>,
    recovered_stale: usize,
}

impl AclManager {
    pub fn new(backup_dir: PathBuf) -> Self {
        // 审计文件放在 `sandbox/` 下（与 `acl/` 备份目录同级）。
        let audit_path = backup_dir
            .parent()
            .map(|parent| parent.join(AUDIT_FILE))
            .unwrap_or_else(|| backup_dir.join(AUDIT_FILE));
        Self { backup_dir, audit_path, granted: HashSet::new(), recovered_stale: 0 }
    }

    /// 上次异常退出留下的备份：先还原，再允许新的授权。
    /// 返回还原条数；任何一条失败都返回 Err（fail-closed）。
    pub fn recover_pending(&mut self) -> Result<usize, String> {
        if !self.backup_dir.is_dir() {
            return Ok(0);
        }
        let mut restored = 0usize;
        let mut failures: Vec<String> = Vec::new();
        let entries = std::fs::read_dir(&self.backup_dir)
            .map_err(|e| format!("读取 ACL 备份目录失败（{}）: {e}", self.backup_dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("读取 ACL 备份项失败: {e}"))?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some(BACKUP_EXT) {
                continue;
            }
            match restore_backup_file(&path) {
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

    /// 给目录加容器 SID 的 ACE。备份失败 → 直接失败，**不改安全描述符**。
    pub fn grant(&mut self, dir: &Path, sid: PSID, access: Access) -> Result<(), String> {
        let normalized = normalize(dir);
        if !normalized.is_dir() {
            return Err(format!("不能给非目录路径加沙箱 ACE：{}", normalized.display()));
        }
        let sid_text = sid_to_string(sid)?;
        let key = (normalized.clone(), sid_text.clone());
        if self.granted.contains(&key) {
            return Ok(());
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
            self.granted.insert(key);
            return Ok(());
        }

        let snapshot = read_dacl_snapshot(&normalized)?;
        let backup_path = self.backup_path(&normalized);
        // 同一路径可能有多个 SID（每个 MCP server 一个容器）：只保留**最早**那份原始 DACL，
        // 否则后一次授权会把备份覆盖成「已有前一个 ACE」的状态，退出还原就不干净了。
        let wrote_backup = if backup_path.exists() {
            false
        } else {
            write_backup_file(&backup_path, &normalized, &snapshot).map_err(|error| {
                self.audit("grant", &normalized, &sid_text, access.label(), "error", Some(&error));
                format!(
                    "备份原始 DACL 失败（{}）：{error}。按规则不改安全描述符，命令拒绝在沙箱外执行",
                    normalized.display()
                )
            })?;
            true
        };

        if let Err(error) = apply_grant(&normalized, sid, mask) {
            // 已经改了一半：立刻按备份还原，再报错。
            let restore = restore_from_snapshot(&normalized, &snapshot);
            if wrote_backup {
                let _ = std::fs::remove_file(&backup_path);
            }
            self.audit("grant", &normalized, &sid_text, access.label(), "error", Some(&error));
            return Err(match restore {
                Ok(()) => format!("给「{}」加沙箱 ACE 失败，已按备份还原：{error}", normalized.display()),
                Err(restore_error) => format!(
                    "给「{}」加沙箱 ACE 失败，且按备份还原也失败（需人工检查 DACL）：{error}；{restore_error}",
                    normalized.display()
                ),
            });
        }

        self.audit("grant", &normalized, &sid_text, access.label(), "ok", None);
        self.granted.insert(key);
        Ok(())
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
        self.granted.remove(&(normalized.clone(), sid_text.clone()));
        if removed {
            // 该路径上已无其它授权 → 备份文件也删掉，
            // 否则退出还原会把刚撤销的 ACE 又写回去。
            if !self.granted.iter().any(|(path, _)| *path == normalized) {
                let _ = std::fs::remove_file(self.backup_path(&normalized));
            }
            self.audit("revoke", &normalized, &sid_text, "-", "ok", None);
        }
        Ok(removed)
    }

    /// 按备份还原全部已授权目录。返回还原条数。
    pub fn restore_all(&mut self) -> Result<usize, String> {
        self.recover_pending()
    }

    /// 是否从异常退出中接管了 profile（用于退出时删除陈旧容器）。
    pub fn recovered_stale(&self) -> usize {
        self.recovered_stale
    }

    fn backup_path(&self, dir: &Path) -> PathBuf {
        self.backup_dir
            .join(format!("{:016x}.{BACKUP_EXT}", hash_path(&dir.to_string_lossy())))
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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// SID → `S-1-15-2-...` 文本（审计与幂等键都用它）。
fn sid_to_string(sid: PSID) -> Result<String, String> {
    let mut text = PWSTR::null();
    unsafe { ConvertSidToStringSidW(sid, &mut text) }
        .map_err(|error| format!("SID 转字符串失败: {error}"))?;
    let value = unsafe { text.to_string() }
        .map_err(|error| format!("SID 字符串解码失败: {error}"));
    unsafe {
        let _ = LocalFree(Some(HLOCAL(text.0.cast::<c_void>())));
    }
    value
}

/// `\\?\D:\ws` → `D:\ws`，并尽量取 canonical 形式。
fn normalize(path: &Path) -> PathBuf {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    match canonical.to_string_lossy().strip_prefix(r"\\?\") {
        Some(stripped) => PathBuf::from(stripped),
        None => canonical,
    }
}

fn hash_path(path: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.to_lowercase().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

/// 读取命名对象的安全描述符（自相对格式）并复制成字节，供备份/回滚使用。
fn read_dacl_snapshot(path: &Path) -> Result<Vec<u8>, String> {
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

fn apply_grant(path: &Path, sid: PSID, mask: u32) -> Result<(), String> {
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
        return Err(format!("读取 DACL 失败（Win32 错误 {}）", status.0));
    }

    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: mask,
        grfAccessMode: GRANT_ACCESS,
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
        let old = if old_dacl.is_null() { None } else { Some(old_dacl as *const ACL) };
        SetEntriesInAclW(Some(std::slice::from_ref(&entry)), old, &mut new_acl)
    };
    if status != ERROR_SUCCESS {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        }
        return Err(format!("构造新 DACL 失败（Win32 错误 {}）", status.0));
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
            "写入「{}」的新 DACL 失败（Win32 错误 {}）",
            path.display(),
            status.0
        ));
    }
    Ok(())
}

/// 精确撤销：只删这一条 `(路径, SID)` 的显式 ACE，其它 trustee 的 ACE 原样保留。
/// 返回是否真的删掉了东西。
fn revoke_ace(path: &Path, sid: PSID) -> Result<bool, String> {
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
        let old = if old_dacl.is_null() { None } else { Some(old_dacl as *const ACL) };
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
    Ok(read_dacl_snapshot(path).map(|after| after != before).unwrap_or(true))
}

fn write_backup_file(path: &Path, object: &Path, snapshot: &[u8]) -> Result<(), String> {
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

fn restore_backup_file(path: &Path) -> Result<(), String> {
    let blob = std::fs::read(path)
        .map_err(|e| format!("读取备份文件失败（{}）: {e}", path.display()))?;
    let (object, snapshot) = parse_backup(&blob)?;
    if !object.exists() {
        std::fs::remove_file(path).ok();
        return Ok(());
    }
    restore_from_snapshot(&object, snapshot)?;
    std::fs::remove_file(path)
        .map_err(|e| format!("还原成功但删除备份文件失败（{}）: {e}", path.display()))?;
    Ok(())
}

fn parse_backup(blob: &[u8]) -> Result<(PathBuf, &[u8]), String> {
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

fn restore_from_snapshot(path: &Path, snapshot: &[u8]) -> Result<(), String> {
    let name = wide(path.as_os_str());
    let descriptor = PSECURITY_DESCRIPTOR(snapshot.as_ptr() as *mut c_void);
    let mut present = windows::core::BOOL(0);
    let mut defaulted = windows::core::BOOL(0);
    let mut dacl: *mut ACL = std::ptr::null_mut();
    unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) }
        .map_err(|e| format!("解析备份的安全描述符失败: {e}"))?;

    let status = unsafe {
        SetNamedSecurityInfoW(
            PCWSTR(name.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | UNPROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            if present.as_bool() { Some(dacl as *const ACL) } else { None },
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "shaniu-acl-{tag}-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
        std::fs::remove_dir_all(&dir).ok();
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
        std::fs::remove_dir_all(&ws).ok();
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
        std::fs::remove_dir_all(&ws).ok();
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

    /// 收敛规则：父 `Write` 覆盖子 `Read`/`Write`；父 `Read` 不覆盖子 `Write`。
    #[test]
    fn collapse_keeps_only_needed_grants() {
        let root = std::env::temp_dir();
        let child = root.join("shaniu-collapse-child");
        std::fs::create_dir_all(&child).unwrap();

        let collapsed = collapse(vec![
            (root.clone(), Access::Read),
            (child.clone(), Access::Read),
            (child.clone(), Access::Write),
        ]);
        assert!(collapsed.contains(&(normalize(&root), Access::Read)));
        assert!(
            collapsed.contains(&(normalize(&child), Access::Write)),
            "子目录的 Write 不得被父目录的 Read 吞掉: {collapsed:?}"
        );

        let collapsed = collapse(vec![(root.clone(), Access::Write), (child.clone(), Access::Read)]);
        assert_eq!(
            collapsed,
            vec![(normalize(&root), Access::Write)],
            "父 Write 已覆盖子 Read，子项应被收敛掉"
        );

        // 同一路径两次不同档位：取更强的一档。
        let same = collapse(vec![(child.clone(), Access::Read), (child.clone(), Access::Write)]);
        assert_eq!(same, vec![(normalize(&child), Access::Write)]);

        std::fs::remove_dir_all(&child).ok();
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
        assert_eq!(second.restore_all().unwrap(), 0, "跳过改写时没有可还原的备份");

        assert_eq!(first.restore_all().unwrap(), 1);
        assert!(
            !has_explicit_grant(&target, sid.as_psid(), Access::Write.mask()).unwrap(),
            "按备份还原后显式 ACE 应消失"
        );
        std::fs::remove_dir_all(&ws).ok();
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
        manager.grant(&target, sid.as_psid(), Access::Write).unwrap();

        let after = read_dacl_snapshot(&target).unwrap();
        assert_ne!(before, after, "加 ACE 后 DACL 应发生变化");

        let restored = manager.restore_all().unwrap();
        assert_eq!(restored, 1);
        let final_snapshot = read_dacl_snapshot(&target).unwrap();
        assert_eq!(final_snapshot, before, "还原后 DACL 应与备份完全一致");
        std::fs::remove_dir_all(&ws).ok();
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
        manager.grant(&target, sid_a.as_psid(), Access::Write).unwrap();
        manager.grant(&target, sid_b.as_psid(), Access::Write).unwrap();

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
        assert!(audit.contains("\"action\":\"grant\""), "审计缺少授权记录: {audit}");
        assert!(audit.contains("\"action\":\"revoke\""), "审计缺少撤销记录: {audit}");

        assert_eq!(manager.restore_all().unwrap(), 1, "还剩一份原始 DACL 备份应被还原");
        assert!(
            !has_explicit_grant(&target, sid_b.as_psid(), Access::Write.mask()).unwrap(),
            "还原后另一个 SID 的 ACE 也应消失"
        );
        std::fs::remove_dir_all(&ws).ok();
    }
}
