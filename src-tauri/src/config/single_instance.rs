//! 同 `<data_root>` 单实例保护（2026-11-11）。
//!
//! 真源 `<data_root>/rollouts/*.jsonl` 是逐行 append + flush 的：两个进程同时写同一
//! 档案会把两条历史交错进同一个文件，产出的坏行即便被 `rollout::read` 跳过，历史本身
//! 也已经串了。所以**每个数据目录最多一个活着的 SHANIU**。
//!
//! 用 Windows 命名互斥体而不是文件锁：互斥体随最后一个句柄关闭自动消失，进程被强杀
//! 也不会留下"谁也不认领"的锁文件；且创建/判定是一次原子调用，没有"先查后建"的竞态。
//!
//! **粒度按 data_root，不是全局单实例**：便携版可以有多个副本，各自 `data\` 互不相干，
//! 同时开着做对照是合法用法；安装版 `%LOCALAPPDATA%\SHANIU` 与便携版也可能并存。
//! 用固定全局名会把合法用法当成冲突拦死，而真正危险的是"同一目录的两个写者"。
//! 名字里放规范化路径的 hash 而不是明文：互斥体名在同机进程间可见，不泄露本地路径。

use std::path::{Path, PathBuf};

use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;

/// 持有期间即"本进程独占该 data_root"；`Drop` 关闭句柄，互斥体最后一个句柄消失即释放。
pub struct InstanceLock {
    handle: HANDLE,
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// 尝试独占 `data_root`。失败返回给用户看的说明（调用方负责弹窗并退出）。
pub fn acquire(data_root: &Path) -> Result<InstanceLock, String> {
    let name = HSTRING::from(mutex_name(data_root));
    let handle = unsafe { CreateMutexW(None, false, &name) }
        .map_err(|e| format!("创建数据目录互斥体失败: {e}"))?;
    // 必须紧接着读，`CreateMutexW` 返回成功同时也可能是"已存在"（此时我们只是拿到
    // 同一个对象的句柄，没有独占权）。
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe {
            let _ = CloseHandle(handle);
        }
        return Err(format!(
            "这个数据目录已被另一个 SHANIU 实例占用：{}",
            data_root.display()
        ));
    }
    Ok(InstanceLock { handle })
}

/// 互斥体名：`Local\SHANIU-data-<规范化路径的 FNV-1a 64 位 hash>`。
///
/// `Local\` 而非 `Global\`：不需要额外特权，覆盖"同一用户开两份"这个真实场景；
/// 不同登录会话共用同一 data_root 本身就不是受支持的用法（append/SQLite 都撑不住）。
pub fn mutex_name(data_root: &Path) -> String {
    let normalized = normalize(data_root);
    let lowered = normalized.to_string_lossy().to_lowercase();
    // 按 UTF-16 取字节：Windows 路径语义以 UTF-16 为准，代理对也按同一形态入 hash。
    let mut bytes = Vec::with_capacity(lowered.len() * 2);
    for unit in lowered.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    format!("Local\\SHANIU-data-{:016x}", fnv1a_64(&bytes))
}

/// FNV-1a 64：六行的稳定 hash，替代不保证跨版本稳定的 `DefaultHasher`。
///
/// 放在本模块而不是 `config`：互斥体名与数据根记录都依赖它，但单实例这一批要能
/// 独立提交/回退，因此由最早落地的模块持有，后者复用。
pub(crate) fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 规范化：先建目录（幂等，run() 随后也会建）再 canonicalize。
///
/// 必须先建：`canonicalize` 对不存在的路径会失败，而首次启动/新副本的 `data\`
/// 往往还不存在；两侧实例若一个走 canonicalize、另一个走词法兜底，算出的 hash
/// 不一致，锁就形同虚设。
fn normalize(data_root: &Path) -> PathBuf {
    let _ = std::fs::create_dir_all(data_root);
    let canonical = std::fs::canonicalize(data_root)
        .or_else(|_| std::path::absolute(data_root))
        .unwrap_or_else(|_| data_root.to_path_buf());
    let trimmed = canonical
        .to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .to_string();
    PathBuf::from(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestTempDir;

    /// 同目录的不同写法必须命中同一把锁；不同目录必须不同（否则多副本被误拦）。
    #[test]
    fn mutex_name_is_per_data_root_and_spelling_insensitive() {
        let dir = TestTempDir::new("instance-name");
        let plain = mutex_name(&dir);
        let trailing = mutex_name(Path::new(&format!("{}\\", dir.display())));
        let upper = mutex_name(Path::new(&dir.display().to_string().to_uppercase()));
        assert_eq!(plain, trailing, "尾分隔符不该改变锁名");
        assert_eq!(plain, upper, "盘符/路径大小写不该改变锁名");

        let other = TestTempDir::new("instance-name-other");
        assert_ne!(plain, mutex_name(&other), "不同 data_root 必须是不同的锁");
    }

    #[test]
    fn acquire_succeeds_and_release_allows_reacquire() {
        let dir = TestTempDir::new("instance-acquire");
        let first = acquire(&dir).expect("首次获取应成功");
        // 同进程再获取一次：命名互斥体是跨句柄的，第二个句柄同样看到"已存在"。
        assert!(acquire(&dir).is_err(), "占用期间不得再次获取");
        drop(first);
        drop(acquire(&dir).expect("释放后应能重新获取"));
    }
}
