//! 测试夹具公共设施。
//!
//! 规矩（2026-11-09 定）：测试产物可以放 `%TEMP%`（系统目录可用），但 **teardown 必须
//! 自己删**，不许依赖系统兜底——这台机器的存储感知没有开「临时文件清理」，堆了 2384 个
//! `shaniu-*` 才被发现。删除失败只 warn 不 panic：清理不该把绿色测试变成红色。
//!
//! 用法：`let ws = TestTempDir::new("sb");` 然后按 `&Path` 用（实现了 `Deref<Target=Path>`）；
//! panic 时 `Drop` 照样执行，目录不会残留。

use std::ops::Deref;
use std::path::{Path, PathBuf};

/// `%TEMP%/shaniu-<prefix>-<pid>-<ulid>`，离开作用域即整目录删除。
pub struct TestTempDir {
    path: PathBuf,
    keep: bool,
}

impl TestTempDir {
    pub fn new(prefix: &str) -> Self {
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
