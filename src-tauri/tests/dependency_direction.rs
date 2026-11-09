//! FF-1 的 cargo test 级门禁：把「依赖方向违规数 = 0」挂进 `cargo test`。
//!
//! 为什么需要它：CI 早已把 `scripts/check-deps.ps1` 作为第一个阻塞步骤，但 2026-11-08
//! 起有 39 个本地提交从未推送，FF-1 在本地从没被跑到——脚本是红的，而提交照常发生。
//! 只靠 CI 等于把门禁放在"推送之后"；这条断言把同一把尺子放在每次 `cargo test` 里，
//! 本地绿了才算绿，红着不会再攒三周。
//!
//! 判据只用脚本退出码（0 = 通过），不在这里复刻白名单：事实来源仍是
//! `docs/01 §2.3.1` + `scripts/check-deps.ps1`，第二份规则必然漂移。

#![cfg(windows)]

use std::path::PathBuf;
use std::process::Command;

#[test]
fn dependency_direction_is_clean() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let script = manifest
        .parent()
        .expect("src-tauri 应位于仓库根下")
        .join("scripts")
        .join("check-deps.ps1");
    assert!(script.is_file(), "找不到依赖方向脚本: {}", script.display());

    let output = Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script)
        .output()
        .expect("启动 PowerShell 失败（FF-1 门禁无法执行）");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "FF-1 依赖方向检查未通过（exit {:?}）：\n{stdout}\n{stderr}",
        output.status.code()
    );
}
