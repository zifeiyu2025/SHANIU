//! 回归门禁（AppContainer 内构建工具链）：workspace-write 档下，容器内的 `cargo test`
//! 必须能**链接并运行**——2026-11-08 实测回归：rustc 的 MSVC 探测（vswhere/Setup COM）
//! 在 LowBox 令牌下失效，容器内 cargo 报 `linker link.exe not found`，模型任务里的
//! 构建/自愈全部烧轮次失败（evals repair-broken-rust-project 从 5/5 跌到 0/5）。
//! 修复：宿主侧解析 MSVC 布局并注入 PATH/INCLUDE/LIB（`sandbox::msvc`）。
//! 本测试在坏项目上复现完整链路：编译 → 链接 → 测试运行（断言失败可见）。
//! 无 MSVC 的机器跳过（探测返回空集合时跳过并注明，不算失败）。

use std::path::PathBuf;

use tokio::sync::mpsc;

use shaniu_lib::config::{OsSandbox, SandboxMode};
use shaniu_lib::sandbox::{self, NetworkAccess};

fn make_broken_project(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"eval_broken\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\n\n[dependencies]\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src").join("lib.rs"),
        "pub fn add(a: u32, b: u32) -> u32 {\n    0\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn add_returns_sum() {\n        assert_eq!(add(2, 2), 4);\n    }\n}\n",
    )
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn cargo_test_inside_appcontainer_links_and_runs_the_tests() {
    if sandbox::current_os_mode() != OsSandbox::AppContainer {
        println!("跳过：OS 沙箱模式不是 AppContainer（sandbox.os = logical）");
        return;
    }
    if shaniu_lib::sandbox::msvc::toolchain_env().is_empty() {
        println!("跳过：本机未探测到 MSVC 工具链（无 VS/BuildTools），无法验证容器内链接");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/sandbox-cargo-build-guard");
    let _ = std::fs::remove_dir_all(&root);
    make_broken_project(&root);

    let (tx, mut rx) = mpsc::channel::<sandbox::ExecLine>(256);
    let mut child = match sandbox::prepare_spawn_with(
        sandbox::SpawnRequest {
            policy: SandboxMode::WorkspaceWrite,
            workspace_root: &root,
            program: "cmd",
            args: &["/C", "cargo test --offline --quiet"],
            cwd: &root,
            network: NetworkAccess::Deny,
            os: sandbox::current_os_mode(),
        },
        tx.clone(),
    ) {
        Ok(sandbox::PreparedSpawn::Restricted(child)) => child,
        Ok(sandbox::PreparedSpawn::Plain(_)) => panic!("WorkspaceWrite 必须走受限路径"),
        Err(error) => panic!("沙箱内启动 cargo 失败: {error}"),
    };
    drop(tx); // 释放本地发送端：读线程结束后通道才能真正关闭

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    while let Some((stream, line)) = rx.recv().await {
        match stream {
            0 => stdout.push(line),
            _ => stderr.push(line),
        }
    }
    let code = child.wait().await.expect("等待容器内 cargo 退出");
    let out = stdout.join("\n");
    let err = stderr.join("\n");

    // 链接已修复：不许再出现「linker not found」；测试真实跑起来：断言失败必须可见。
    assert!(
        !out.contains("linker") && !err.contains("linker"),
        "容器内 cargo 仍在报链接器问题（MSVC 注入失效）:\n{out}\n{err}"
    );
    assert!(
        out.contains("add_returns_sum") || err.contains("add_returns_sum"),
        "容器内 cargo 应跑出测试失败（断言可见），实际输出:\n{out}\n{err}"
    );
    assert!(
        out.contains("left: 0") || err.contains("left: 0"),
        "断言详情应与宿主侧一致（left: 0 / right: 4），实际输出:\n{out}\n{err}"
    );
    assert_eq!(code, 101, "坏项目的 cargo test 退出码应为 101（测试失败）");
}
