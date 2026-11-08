//! 目录 DACL 管理（SH-022）：只给容器 SID 加写 ACE，且**先备份再改**。
//!
//! 硬约束（用户 2026-11-08 定）：
//!  - ACE 只加在**工作区根 / temp / 工具链目录**三类位置；
//!  - 授权对象是**容器 SID**，不是 Everyone / Users；
//!  - 改安全描述符前先备份原始 DACL；备份失败就不许改（不"先改了再说"）；
//!  - 卸载/退出按备份还原，避免残留 ACE 指向已删除的容器 SID。
//!
//! 备份落盘在数据目录 `sandbox/acl/` 下，键为规范化路径的 FNV-1a 哈希；
//! 进程崩溃后下次启动先 `recover_pending()` 还原，再重新授权。

use std::collections::HashSet;
use std::ffi::c_void;
use std::path::{Path, PathBuf};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{LocalFree, HLOCAL, ERROR_SUCCESS};
use windows::Win32::Security::Authorization::{
    GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W,
    GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN,
    TRUSTEE_W,
};
use windows::Win32::Security::{
    GetSecurityDescriptorDacl, GetSecurityDescriptorLength, ACL, DACL_SECURITY_INFORMATION, PSID,
    PSECURITY_DESCRIPTOR, SUB_CONTAINERS_AND_OBJECTS_INHERIT, UNPROTECTED_DACL_SECURITY_INFORMATION,
};
use windows::Win32::Storage::FileSystem::{
    DELETE, FILE_DELETE_CHILD, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
};

use super::windows_impl::wide;

const BACKUP_MAGIC: &[u8; 8] = b"SHNIACL1";
const BACKUP_EXT: &str = "dacl";

/// 授权掩码：读 / 写 / 执行 / 删除（目录还要能删子项）。
/// 不给 WRITE_DAC / WRITE_OWNER——沙箱进程不得改动安全描述符。
fn grant_mask() -> u32 {
    FILE_GENERIC_READ.0
        | FILE_GENERIC_WRITE.0
        | FILE_GENERIC_EXECUTE.0
        | DELETE.0
        | FILE_DELETE_CHILD.0
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
    paths
        .into_iter()
        .filter(|path| path.is_dir())
        .filter(|path| seen.insert(normalize(path)))
        .collect()
}

pub struct AclManager {
    backup_dir: PathBuf,
    granted: HashSet<PathBuf>,
    recovered_stale: usize,
}

impl AclManager {
    pub fn new(backup_dir: PathBuf) -> Self {
        Self { backup_dir, granted: HashSet::new(), recovered_stale: 0 }
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

    /// 给目录加容器 SID 的写 ACE。备份失败 → 直接失败，**不改安全描述符**。
    pub fn grant_write(&mut self, dir: &Path, sid: PSID) -> Result<(), String> {
        let normalized = normalize(dir);
        if self.granted.contains(&normalized) {
            return Ok(());
        }
        if !normalized.is_dir() {
            return Err(format!("不能给非目录路径加沙箱 ACE：{}", normalized.display()));
        }

        let snapshot = read_dacl_snapshot(&normalized)?;
        let backup_path = self.backup_path(&normalized);
        write_backup_file(&backup_path, &normalized, &snapshot).map_err(|error| {
            format!(
                "备份原始 DACL 失败（{}）：{error}。按规则不改安全描述符，命令拒绝在沙箱外执行",
                normalized.display()
            )
        })?;

        if let Err(error) = apply_grant(&normalized, sid) {
            // 已经改了一半：立刻按备份还原，再报错。
            let restore = restore_from_snapshot(&normalized, &snapshot);
            let _ = std::fs::remove_file(&backup_path);
            return Err(match restore {
                Ok(()) => format!("给「{}」加沙箱 ACE 失败，已按备份还原：{error}", normalized.display()),
                Err(restore_error) => format!(
                    "给「{}」加沙箱 ACE 失败，且按备份还原也失败（需人工检查 DACL）：{error}；{restore_error}",
                    normalized.display()
                ),
            });
        }

        self.granted.insert(normalized);
        Ok(())
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

fn apply_grant(path: &Path, sid: PSID) -> Result<(), String> {
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
        grfAccessPermissions: grant_mask(),
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
        return Err(format!("写入新 DACL 失败（Win32 错误 {}）", status.0));
    }
    Ok(())
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
        assert!(scope.iter().any(|path| normalize(path) == normalize(&ws)));
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
            .grant_write(&target, sid.as_psid())
            .expect_err("备份失败时必须拒绝改动安全描述符");
        assert!(error.contains("备份原始 DACL 失败"), "{error}");
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
        manager.grant_write(&target, sid.as_psid()).unwrap();

        let after = read_dacl_snapshot(&target).unwrap();
        assert_ne!(before, after, "加 ACE 后 DACL 应发生变化");

        let restored = manager.restore_all().unwrap();
        assert_eq!(restored, 1);
        let final_snapshot = read_dacl_snapshot(&target).unwrap();
        assert_eq!(final_snapshot, before, "还原后 DACL 应与备份完全一致");
        std::fs::remove_dir_all(&ws).ok();
    }
}
