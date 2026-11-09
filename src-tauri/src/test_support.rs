//! 测试夹具公共设施。
//!
//! 规矩（2026-11-09 定）：测试产物可以放 `%TEMP%`（系统目录可用），但 **teardown 必须
//! 自己删**，不许依赖系统兜底——这台机器的存储感知没有开「临时文件清理」，堆了 2384 个
//! `shaniu-*` 才被发现。删除失败只 warn 不 panic：清理不该把绿色测试变成红色。
//!
//! 用法：`let ws = TestTempDir::new("sb");` 然后按 `&Path` 用（实现了 `Deref<Target=Path>`）；
//! panic 时 `Drop` 照样执行，目录不会残留。
//!
//! 兜底清扫（2026-11-09 补）：`Drop` 只覆盖正常退出与 panic，`Stop-Process -Force` / 崩溃
//! 仍然漏。因此每个测试进程**首次**创建夹具时扫一遍 `%TEMP%\shaniu-*`，删掉超过 1 小时的
//! 陈旧目录（1 小时避免误删并发进程正在用的）——兜底放在下一次启动，而不是逐次清理。

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::{Duration, SystemTime};

/// 陈旧的判定门槛：超过这个年龄的 `shaniu-*` 目录才允许被清扫。
const STALE_AFTER: Duration = Duration::from_secs(60 * 60);

/// `%TEMP%/shaniu-<prefix>-<pid>-<ulid>`，离开作用域即整目录删除。
pub struct TestTempDir {
    path: PathBuf,
    keep: bool,
}

impl TestTempDir {
    pub fn new(prefix: &str) -> Self {
        sweep_stale_dirs_once();
        let path = std::env::temp_dir().join(format!(
            "shaniu-{prefix}-{}-{}",
            std::process::id(),
            ulid::Ulid::generate()
        ));
        std::fs::create_dir_all(&path).expect("创建测试临时目录");
        Self { path, keep: false }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 需要保留现场排查时显式调用（默认不保留）。
    pub fn keep(&mut self) {
        self.keep = true;
    }
}

impl Deref for TestTempDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TestTempDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestTempDir {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        // Windows 上后台子进程可能仍把该目录当 cwd，删目录会撞共享冲突；
        // 有界重试几次即可，失败只 warn——清理不该把绿色测试变成红色。
        let mut last = None;
        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            match std::fs::remove_dir_all(&self.path) {
                Ok(()) => return,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
                Err(error) => last = Some(error),
            }
        }
        if let Some(error) = last {
            eprintln!(
                "warn: 清理测试临时目录 {} 失败（忽略）: {error}",
                self.path.display()
            );
        }
    }
}

/// 每个测试进程只扫一次：多个夹具并行创建时不必重复遍历 `%TEMP%`。
fn sweep_stale_dirs_once() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let removed = sweep_stale_dirs();
        if removed > 0 {
            eprintln!(
                "[test] 清扫陈旧临时目录 {removed} 个（超过 {} 分钟）",
                STALE_AFTER.as_secs() / 60
            );
        }
    });
}

/// 删除 `%TEMP%` 下 `shaniu-*` 中修改时间早于 `STALE_AFTER` 的**目录**，返回删除个数。
/// 单个删除失败静默跳过：清扫只是兜底，不该让测试变红。
fn sweep_stale_dirs() -> usize {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return 0;
    };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with("shaniu-") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_dir() {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(modified) else {
            continue;
        };
        if age < STALE_AFTER {
            continue;
        }
        if std::fs::remove_dir_all(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 防的是「清扫把并发进程正在用的目录删掉」——门槛没到的一个都不能动。
    #[test]
    fn sweep_keeps_fresh_directories() {
        let fresh = TestTempDir::new("sweep-fresh");
        std::fs::create_dir_all(fresh.join("keep")).expect("建子目录");
        let _ = sweep_stale_dirs();
        assert!(fresh.join("keep").exists(), "新鲜目录不得被清扫");
    }
}
