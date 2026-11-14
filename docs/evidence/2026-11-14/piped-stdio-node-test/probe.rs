//! 归档：2026-11-14 真机探针（当时位于 `src-tauri/tests/tmp_piped_stdio_probe.rs`，跑完删除）。
//!
//! 复跑：把本文件复制回 `src-tauri/tests/tmp_piped_stdio_probe.rs`，然后
//! `cargo test --offline --test tmp_piped_stdio_probe -- --nocapture --test-threads=1`。
//! 它断言很少，作用是打印事实日志（`run-2026-11-14.log`）。

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::id;
use std::sync::atomic::{AtomicUsize, Ordering};

use shaniu_lib::config::{OsSandbox, SandboxMode};
use shaniu_lib::sandbox::{prepare_spawn_with, PreparedSpawn, SpawnRequest};
use tokio::sync::mpsc;

static SEQ: AtomicUsize = AtomicUsize::new(0);

fn unique_dir(tag: &str) -> PathBuf {
    let home = std::env::var_os("USERPROFILE").unwrap();
    let base = PathBuf::from(home).join(".shaniu-probe");
    std::fs::create_dir_all(&base).unwrap();
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    let dir = base.join(format!("{tag}-{}-{n}", id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn make_project(dir: &Path) {
    std::fs::create_dir_all(dir.join("test")).unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"probe","version":"1.0.0","scripts":{"test":"node --test"}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("test/stats.test.js"),
        "const test = require('node:test');\nconst assert = require('node:assert');\ntest('ok', () => assert.strictEqual(1, 1));\n",
    )
    .unwrap();
}

async fn run(os: OsSandbox, cwd: &Path, line: &str) -> (i32, String) {
    let (tx, mut rx) = mpsc::channel::<shaniu_lib::sandbox::ExecLine>(256);
    let collector = tokio::spawn(async move {
        let mut output = String::new();
        while let Some((_, text)) = rx.recv().await {
            output.push_str(&text);
            output.push('\n');
        }
        output
    });
    let request = SpawnRequest {
        policy: SandboxMode::WorkspaceWrite,
        workspace_root: cwd,
        program: "cmd",
        args: &["/C", line],
        cwd,
        os,
    };
    let spawn = prepare_spawn_with(request, tx).expect("沙箱应可创建");
    let code = match spawn {
        PreparedSpawn::Restricted(mut child) => child.wait().await.expect("等待子进程失败"),
        PreparedSpawn::Plain(_) => panic!("不得退化为无沙箱执行"),
    };
    let output = collector.await.expect("读线程不应 panic");
    (code, output)
}

async fn report(label: &str, os: OsSandbox, cwd: &Path, line: &str) {
    let (code, output) = run(os, cwd, line).await;
    println!("===== {label} [os={os:?}] exit={code}\n{output}\n===== end {label}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_node_test_paths() {
    let ws = unique_dir("piped");
    make_project(&ws);

    // 对照组：沙箱外直接跑，证明工程本身是绿的。
    let control = std::process::Command::new("cmd")
        .args(["/C", "node --test"])
        .current_dir(&ws)
        .output()
        .expect("需要 node");
    println!(
        "===== control(outside sandbox) exit={:?}\n{}{}\n===== end control",
        control.status.code(),
        String::from_utf8_lossy(&control.stdout),
        String::from_utf8_lossy(&control.stderr)
    );

    report("node --test", OsSandbox::RestrictedToken, &ws, "node --test").await;
    report(
        "node test/stats.test.js",
        OsSandbox::RestrictedToken,
        &ws,
        "node test/stats.test.js",
    )
    .await;
    report(
        "node --test --test-isolation=none",
        OsSandbox::RestrictedToken,
        &ws,
        "node --test --test-isolation=none",
    )
    .await;
    report(
        "node --test (logical)",
        OsSandbox::Logical,
        &ws,
        "node --test",
    )
    .await;
    report("npm test", OsSandbox::RestrictedToken, &ws, "npm test").await;
    report("npm test (logical)", OsSandbox::Logical, &ws, "npm test").await;

    std::fs::remove_dir_all(&ws).ok();
}
