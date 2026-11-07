//! 崩溃捕获探针：真的 panic 一次，验证 hook 落盘、脱敏、且不阻断原有 unwind。
//!
//! 放在独立 integration test 二进制里：panic hook 是进程级的，装在 lib 单测进程里会
//! 污染其他用例；这里替换本测试进程的 hook，不影响 lib 测试。

use std::fs;

#[test]
fn panic_hook_writes_redacted_crash_report() {
    let dir = std::env::temp_dir().join(format!("shaniu-crash-hook-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    shaniu_lib::config::install_crash_hook(dir.clone());

    let caught = std::panic::catch_unwind(|| {
        panic!("crash-hook-probe sk-abcdefgh12345678");
    });
    assert!(caught.is_err(), "catch_unwind 必须仍能接住 panic（hook 不改变 unwind）");

    let reports: Vec<_> = fs::read_dir(&dir)
        .expect("crash 目录应存在")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect();
    assert_eq!(reports.len(), 1, "一次 panic 只应落一份报告：{reports:?}");
    let text = fs::read_to_string(&reports[0]).expect("读崩溃报告");
    assert!(text.contains("location="), "缺少 panic 位置：{text}");
    assert!(text.contains("[REDACTED]"), "密钥未脱敏：{text}");
    assert!(!text.contains("sk-abcdefgh12345678"), "密钥原文落盘：{text}");
    // 报告只写本地，不得包含上报字段/URL。
    assert!(!text.contains("http://") && !text.contains("https://"), "崩溃报告不应含网络地址：{text}");
    fs::remove_dir_all(&dir).ok();
}
