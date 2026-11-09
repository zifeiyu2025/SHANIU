//! `\Device\Null` 的设备对象 ACE（SH-133）。
//!
//! 现场（2026-11-09，GitHub `windows-2025-vs2026`，Build 26100）：
//! 容器内 `cargo test` 在启动 rustc 时报
//! `failed to open NUL device for child stdin: Access is denied. (os error 5)`。
//!
//! 取证结论（同一次 CI 诊断，容器内逐一验证）：
//!  - `type NUL` / `echo x>NUL` 返回 0 —— 这是 cmd 的**内建特例**，不经 `CreateFile`，不能当作可达；
//!  - `.NET` 的 `OpenRead` / `OpenWrite` / `Open(ReadWrite)` 与 Rust 的 `Stdio::null()` /
//!    `Command::output()` **全部 Access denied** —— 容器根本打不开 NUL；
//!  - 设备 DACL 差异：Server 2025 为 `WD/SY/BA/RC`，**没有任何应用包 SID**；客户端 SKU
//!    （Win11 26300）额外带 `AC`（S-1-15-2-1）与 `S-1-15-2-2`。
//!
//! 为什么这会让容器整体失效：AppContainer 的受限访问检查只认受限 SID（容器 SID / 能力 SID /
//! 令牌自动带入的 `S-1-15-2-2`），设备 DACL 里没有这些 SID 就一律拒绝。于是容器里任何
//! `Stdio::null()`、`Command::output()` 的子进程（cargo、node、python 的常见用法）都会失败。
//!
//! 处置（最小授权）：只给**我们自己容器的 SID** 在这一个对象上补读写 ACE——不碰
//! Everyone/Users，不放宽设备命名空间，不动其它设备；改前备份原始 DACL，退出/下次启动还原。
//! 没有写 DACL 权限（非管理员，设备只有 `BA/SY` 有 `WRITE_DAC`）时**不降级隔离**：
//! 上报 `NoPermission` 并继续，依赖 null stdio 的命令按实际错误失败，日志里有明因。

use std::path::Path;

use windows::Win32::Security::PSID;
use windows::Win32::Storage::FileSystem::{FILE_GENERIC_READ, FILE_GENERIC_WRITE};

use super::acl;

/// `NUL` 的 Win32 设备路径（解析到 `\Device\Null`）。
pub(crate) const NULL_DEVICE: &str = r"\\.\NUL";

/// 容器里任何 `CreateFile("NUL")` 都需要读写两种访问（Rust 的 `Stdio::null()` 就要 `READ|WRITE`）。
/// `pub(crate)` 是给 `sandbox::tests` 用的：断言必须查**同一个**掩码，另写一份就有漂移空间。
pub(crate) const REQUIRED_MASK: u32 = FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0;

/// 备份文件名：一个设备一份原始 DACL（多个容器 SID 叠加时，保留最早那份）。
const BACKUP_FILE: &str = "null-device.dacl";

/// 一个容器与 NUL 设备的关系（启动时记录一次，供日志与诊断）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NullDeviceAccess {
    /// 设备 DACL 已覆盖容器可用的 SID（客户端 SKU 默认如此），本次未改动任何 DACL。
    AlreadyAccessible,
    /// 本次给容器 SID 补了读写 ACE（原始 DACL 已备份，退出/下次启动还原）。
    Granted,
    /// 没有写 DACL 的权限：容器内 NUL 仍不可达，依赖 null stdio 的命令会报 Access denied。
    NoPermission(String),
}

/// 确保容器能打开 `\Device\Null`。幂等：已覆盖则一个字节都不改。
pub(crate) fn ensure_container_can_open_null(
    sid: PSID,
    backup_dir: &Path,
) -> Result<NullDeviceAccess, String> {
    let device = Path::new(NULL_DEVICE);

    // 客户端 SKU：受限 SID 里自动带 `S-1-15-2-2`（`AC` 是给全信任应用包的），够用就不改。
    for shared in ["S-1-15-2-2", "S-1-15-2-1"] {
        let shared = super::appcontainer::capability_sid(shared)?;
        if acl::has_explicit_grant(device, shared.as_psid(), REQUIRED_MASK)? {
            return Ok(NullDeviceAccess::AlreadyAccessible);
        }
    }
    // 本进程之前已经给这个容器补过（MCP 多个 server、重复启动）。
    if acl::has_explicit_grant(device, sid, REQUIRED_MASK)? {
        return Ok(NullDeviceAccess::AlreadyAccessible);
    }

    let backup = backup_dir.join(BACKUP_FILE);
    let created_backup = if backup.exists() {
        false
    } else {
        let snapshot = acl::read_dacl_snapshot(device)?;
        acl::write_backup_file(&backup, device, &snapshot)?;
        true
    };

    // 设备对象上没有继承：ACE 必须显式（inheritance = 0）。
    match acl::apply_grant(device, sid, REQUIRED_MASK, windows::Win32::Security::ACE_FLAGS(0)) {
        Ok(()) => Ok(NullDeviceAccess::Granted),
        Err(failure) => {
            // 没改成就不许留"原始 DACL"的误导性备份：否则下次启动会拿它去还原一个从未改过的对象。
            if created_backup {
                let _ = std::fs::remove_file(&backup);
            }
            if failure.access_denied {
                Ok(NullDeviceAccess::NoPermission(failure.message))
            } else {
                Err(failure.message)
            }
        }
    }
}

/// 撤销本进程给某个容器 SID 在 `\Device\Null` 上加的读写 ACE（server 停止 / 撤信时调用）。
///
/// **只删这一个 SID 的显式 ACE**，不还原整份备份：其它 server 的 SID 可能还在用同一个设备，
/// 整份还原留给进程退出（`restore_null_device`）。调用方必须先确认这条 ACE 是本进程加的
/// （`NullDeviceAccess::Granted`），否则会把系统自带的共享授权删掉。
pub(crate) fn revoke_container_access(sid: PSID) -> Result<bool, String> {
    let device = Path::new(NULL_DEVICE);
    // 先查再写：设备的 `WRITE_DAC` 通常只有管理员有，而「本机没补过 / 重复释放」是常态。
    // 不查就写会把一次无害的幂等释放变成 Access denied，还会白触发一次 DACL 规范化。
    if !acl::has_explicit_grant(device, sid, REQUIRED_MASK)? {
        return Ok(false);
    }
    acl::revoke_ace(device, sid)
}

/// 按备份还原 NUL 设备的 DACL。返回是否真的还原了（没有备份 = 什么都没做）。
///
/// 注意：设备路径 `Path::exists()` 恒为 false，**不能**走目录备份那条"对象不存在就丢弃备份"
/// 的分支（`acl::restore_backup_file`），否则会静默丢掉要还原的原始 DACL。
pub(crate) fn restore_null_device(backup_dir: &Path) -> Result<bool, String> {
    let backup = backup_dir.join(BACKUP_FILE);
    if !backup.is_file() {
        return Ok(false);
    }
    let blob = std::fs::read(&backup)
        .map_err(|e| format!("读取 NUL 设备 DACL 备份失败（{}）: {e}", backup.display()))?;
    let (object, snapshot) = acl::parse_backup(&blob)?;
    acl::restore_from_snapshot(&object, snapshot)?;
    std::fs::remove_file(&backup)
        .map_err(|e| format!("还原成功但删除 NUL 设备备份失败（{}）: {e}", backup.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> crate::test_support::TestTempDir {
        crate::test_support::TestTempDir::new(&format!("null-device-{tag}"))
    }

    /// 没备份就是没事可做：退出路径不能被"从没授权过"绊住。
    #[test]
    fn restore_without_backup_is_a_noop() {
        let dir = temp_dir("noop");
        assert!(!restore_null_device(&dir).unwrap());
    }

    /// 备份被损坏时必须报错并**保留**文件：还原失败要能被下一次启动继续尝试。
    #[test]
    fn corrupted_backup_fails_closed_and_keeps_the_file() {
        let dir = temp_dir("corrupt");
        let backup = dir.join(BACKUP_FILE);
        std::fs::write(&backup, b"not-a-dacl-backup").unwrap();
        assert!(restore_null_device(&dir).is_err());
        assert!(backup.is_file(), "还原失败时不得删除备份文件");
    }

    /// 设备路径不能用 `Path::exists()` 判断存在性——这正是不能复用目录还原分支的原因。
    #[test]
    fn device_path_is_not_a_file_path() {
        assert!(!Path::new(NULL_DEVICE).exists());
        assert_eq!(REQUIRED_MASK, FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0);
    }

    /// 撤销必须幂等：从没补过的 SID 上撤销返回 `false`，不报错、不写 DACL。
    ///
    /// 口径说明（SH-064 B）：本机是客户端 SKU，令牌自带 `S-1-15-2-2`，容器走
    /// `AlreadyAccessible`，`null_granted=false`，**「补 → 撤」这条路径在本机跑不到**；
    /// 它只在没有共享应用包 SID 的机器（Server runner）才真的发生。这里只钉住
    /// 「没补过就不许动设备 DACL」这一半，另一半只能在提权 / Server runner 上验。
    #[test]
    fn revoke_of_an_absent_grant_is_a_noop() {
        let sid = crate::sandbox::appcontainer::capability_sid("S-1-15-2-99").unwrap();
        assert!(
            !revoke_container_access(sid.as_psid()).unwrap(),
            "从未授权过的 SID 上撤销必须返回 false（幂等且不报错）"
        );
    }
}
