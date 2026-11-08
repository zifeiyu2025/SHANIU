//! 真机探针：用 `D:\AI\Windows-MCP`（Python + fastmcp，真实 stdio MCP server）走完链路，
//! 补 `fake_mcp_server.js` 覆盖不到的几件事：
//!
//! 1. **受限令牌 + env 白名单**下真实 Python server 能否启动（fixture 是 node，且测试自己
//!    塞了 SYSTEMROOT/TEMP，测不出白名单缺口）；
//! 2. 真实工具描述过一遍注入检测，看**是否误杀**；
//! 3. 批准后实际调用一次真实工具，确认输出仍在 nonce 包裹内；
//! 4. 撤销信任后 in-flight 执行器立即失败。
//!
//! 默认 `#[ignore]`：依赖本机路径。运行方式：
//! `SHANIU_REAL_MCP_PYTHON=D:\AI\Windows-MCP\.venv\Scripts\python.exe`
//! `cargo test --test mcp_real_server -- --ignored --nocapture`
//!
//! 未设置该变量时整组跳过（返回而不失败），CI 不受影响。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use shaniu_lib::config::{McpServerConfig, SandboxMode};
use shaniu_lib::events::{EventMsg, EventSink, RiskLevel};
use shaniu_lib::mcp_client::runtime::{McpRuntime, McpStatus, TrustDecisionInput};
use shaniu_lib::tool_router::{ToolContext, ToolExecutor};
use tokio_util::sync::CancellationToken;

const SERVER: &str = "winmcp";

fn python() -> Option<String> {
    std::env::var("SHANIU_REAL_MCP_PYTHON").ok().filter(|value| !value.trim().is_empty())
}

fn server_root() -> PathBuf {
    PathBuf::from("D:/AI/Windows-MCP")
}

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("shaniu-real-mcp-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("创建临时配置根");
    dir
}

/// 关键：`env` 故意留空——只靠全局环境白名单，测的才是「白名单够不够」。
fn config(python: &str) -> McpServerConfig {
    McpServerConfig {
        id: SERVER.to_string(),
        command: python.to_string(),
        args: vec!["-m".into(), "windows_mcp".into(), "serve".into()],
        env: BTreeMap::new(),
        cwd: server_root().display().to_string(),
        ..McpServerConfig::default()
    }
}

fn context() -> ToolContext {
    struct DiscardSink;

    impl EventSink for DiscardSink {
        fn emit(&self, _msg: &EventMsg) -> Result<(), String> {
            Ok(())
        }
    }

    ToolContext::new(
        server_root(),
        CancellationToken::new(),
        "call-real-mcp",
        Arc::new(DiscardSink),
        SandboxMode::WorkspaceWrite,
    )
}

/// 看起来只读、可以安全调用的工具（按名字判定，不采信 server 自报注解）。
fn is_safe_to_call(raw_name: &str) -> bool {
    matches!(
        raw_name.to_ascii_lowercase().as_str(),
        "displayinventory" | "controlstatus" | "snapshot" | "scrape" | "wait" | "waitfor"
    )
}

#[tokio::test]
#[ignore = "真机探针：需要 SHANIU_REAL_MCP_PYTHON"]
async fn real_windows_mcp_starts_lists_tools_and_calls_one() {
    let Some(python) = python() else {
        eprintln!("跳过：未设置 SHANIU_REAL_MCP_PYTHON");
        return;
    };
    assert!(
        server_root().is_dir(),
        "真机探针要求 {} 存在",
        server_root().display()
    );

    let root = temp_root("chain");
    let runtime = McpRuntime::new(&root);
    let review = match runtime.connect_and_review(&config(&python), &server_root()).await {
        Ok(review) => review,
        Err(error) => panic!("真实 server 在受限 env 下启动失败（这是要报告的产能结论）: {error}"),
    };

    eprintln!("=== 连接结果 ===");
    eprintln!("status={:?}", review.status);
    eprintln!("executable={}", review.executable);
    eprintln!("sha256={}", review.executable_sha256);
    eprintln!("server={} {}", review.server_name, review.server_version);
    eprintln!("protocol={}", review.protocol_version);
    eprintln!("fingerprint={}", review.fingerprint);
    eprintln!("cwd={}", review.cwd);
    eprintln!("env(脱敏)={:?}", review.env);
    eprintln!("warnings={:?}", review.warnings);
    eprintln!("=== 工具清单（{} 个）===", review.tools.len());
    let mut missed = Vec::new();
    for tool in &review.tools {
        eprintln!(
            "- {} (raw={}) risk={:?} read_only_hint={} desc_chars={}",
            tool.dispatch_name,
            tool.raw_name,
            tool.risk,
            tool.server_read_only_hint,
            tool.description.chars().count()
        );
        for hit in &tool.guard_hits {
            eprintln!("    命中 {} : {}", hit.rule, hit.fragment);
            missed.push((tool.raw_name.clone(), hit.rule.clone(), hit.fragment.clone()));
        }
    }
    for rejected in &review.rejected_tools {
        eprintln!("- REJECTED {}: {}", rejected.raw_name, rejected.reason);
    }
    assert_eq!(review.status, McpStatus::Review, "首连必须是待审查");
    assert!(!review.tools.is_empty(), "真实 server 必须列出工具");
    if !missed.is_empty() {
        panic!("真实服务器描述被注入规则命中（误杀清单）: {missed:#?}");
    }

    let resolved = runtime
        .resolve_trust(SERVER, &review.fingerprint, &decisions_for(&review))
        .await
        .expect("批准全部工具应成功");
    assert_eq!(resolved.status, McpStatus::Trusted);
    assert_eq!(
        runtime.executors().len(),
        review.tools.len(),
        "批准后每个工具都要有执行器"
    );

    // 实际调用一个只读工具，验证 nonce 包裹（优先最便宜的 DisplayInventory）。
    let preferred = ["displayinventory", "controlstatus", "wait", "waitfor"];
    let safe = preferred
        .iter()
        .find_map(|want| {
            review
                .tools
                .iter()
                .find(|tool| tool.raw_name.eq_ignore_ascii_case(want))
                .map(|tool| tool.dispatch_name.clone())
        });
    if let Some(name) = safe {
        let executor = runtime
            .executors()
            .into_iter()
            .find(|executor| executor.spec().name == name)
            .unwrap_or_else(|| panic!("执行器 {name} 未注册"));
        let output = executor.execute(json!({}), &context()).await;
        match output {
            Ok(output) => {
                eprintln!("=== 调用 {name} ===");
                eprintln!("success={} bytes={}", output.success, output.output.len());
                eprintln!("{}", output.output.chars().take(600).collect::<String>());
                assert!(output.output.contains("不可信工具输出"), "输出必须在不可信包裹内");
                assert!(output.output.contains("MCP_RESULT"), "包裹必须带 nonce 标记");
            }
            Err(error) => panic!("只读工具 {name} 调用失败: {error}"),
        }
    } else {
        eprintln!("注意：没有匹配到只读候选工具，跳过实际调用");
    }

    // 撤销信任：in-flight 执行器立即失效（真实 server 上复核一次）。
    let in_flight: Option<Arc<dyn ToolExecutor>> = runtime.executors().into_iter().next();
    runtime.revoke_trust(SERVER).await.expect("撤销信任应成功");
    if let Some(executor) = in_flight {
        let error = executor
            .execute(json!({}), &context())
            .await
            .expect_err("撤销后调用必须失败");
        assert!(
            error.to_string().contains("授权已被撤销"),
            "错误应说明授权已撤销: {error}"
        );
    }
    assert!(runtime.executors().is_empty(), "撤销后不得残留执行器");

    runtime.shutdown_all().await;
}

/// 逐工具定档：只读候选给 read，其余给 destructive（不采信 server 注解）。
fn decisions_for(review: &shaniu_lib::mcp_client::runtime::ReviewPayload) -> Vec<TrustDecisionInput> {
    review
        .tools
        .iter()
        .map(|tool| TrustDecisionInput {
            dispatch_name: tool.dispatch_name.clone(),
            approved: true,
            risk: Some(if is_safe_to_call(&tool.raw_name) {
                RiskLevel::Read
            } else {
                RiskLevel::Destructive
            }),
            enable_injection: false,
        })
        .collect()
}

/// 统计命令里带某个临时目录标记的 python 进程数：用来把「retrust 没有 spawn」变成可测事实。
///
/// 必须按进程名过滤：脚本自身的命令行里也含这个标记，不过滤会把 powershell 自己数进去。
fn process_count(marker: &std::path::Path) -> usize {
    let script = format!(
        "@(Get-CimInstance Win32_Process | Where-Object {{ $_.Name -like 'python*' -and $_.CommandLine -like '*{}*' }}).Count",
        marker.display().to_string().replace('\\', "\\\\").replace('\'', "''")
    );
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .expect("运行 powershell 统计进程");
    String::from_utf8_lossy(&output.stdout).trim().parse().unwrap_or(0)
}

/// 等进程清零，返回实际耗时；超时原样返回上限。
fn wait_until_gone(marker: &std::path::Path, limit: std::time::Duration) -> std::time::Duration {
    let started = std::time::Instant::now();
    while started.elapsed() < limit {
        if process_count(marker) == 0 {
            return started.elapsed();
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    started.elapsed()
}

/// 真机 retrust：替换解释器内容后必须**不 spawn**、不注册，只给旧清单作参考。
///
/// 用「整份复制 venv 到 `target/qa-real-mcp`」的方式替换，不碰用户的 `.venv`。
#[tokio::test]
#[ignore = "真机探针：需要 SHANIU_REAL_MCP_PYTHON"]
async fn real_server_binary_replacement_enters_retrust_without_spawn() {
    let Some(_python) = python() else {
        eprintln!("跳过：未设置 SHANIU_REAL_MCP_PYTHON");
        return;
    };
    // 整份复制 venv 到 target/：单独复制 python.exe 会因为 sys.prefix 找不到
    // site-packages 而 import 失败（实测），而 venv 整目录复制后 prefix 自洽。
    let staging = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("qa-real-mcp");
    let venv = staging.join(".venv");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).expect("创建 staging");
    let copy = std::process::Command::new("robocopy")
        .arg(server_root().join(".venv"))
        .arg(&venv)
        .args(["/E", "/NFL", "/NDL", "/NJH", "/NJS", "/NP", "/R:1", "/W:1"])
        .output()
        .expect("运行 robocopy");
    assert!(copy.status.code().unwrap_or(9) < 8, "复制 venv 失败: {copy:?}");
    let exe = venv.join("Scripts").join("python.exe");

    let config = McpServerConfig {
        id: SERVER.to_string(),
        command: exe.display().to_string(),
        args: vec!["-m".into(), "windows_mcp".into(), "serve".into()],
        env: BTreeMap::new(),
        cwd: server_root().display().to_string(),
        ..McpServerConfig::default()
    };

    let runtime = McpRuntime::new(&temp_root("retrust-trust"));
    let first = runtime
        .connect_and_review(&config, &server_root())
        .await
        .expect("复制出来的解释器应能启动（否则报告为环境限制）");
    assert_eq!(first.status, McpStatus::Review);
    let approved = runtime
        .resolve_trust(SERVER, &first.fingerprint, &decisions_for(&first))
        .await
        .expect("批准");
    assert_eq!(approved.status, McpStatus::Trusted);
    runtime.shutdown_all().await;
    // 等子进程真的退出：shutdown 是异步的，立刻计数会把「正在退出」误判成「又 spawn 了」。
    let waited = wait_until_gone(&staging, std::time::Duration::from_secs(10));
    eprintln!("shutdown 后进程清零耗时={waited:?}");
    assert_eq!(process_count(&staging), 0, "shutdown 后不得残留真实 server 进程");

    // 替换可执行文件内容：追加一个字节即可改哈希（PE 允许尾部填充，仍可运行）。
    let mut bytes = std::fs::read(&exe).expect("读 exe");
    bytes.extend_from_slice(b"\n");
    std::fs::write(&exe, bytes).expect("写回 exe");

    let before = process_count(&staging);
    let review = runtime
        .connect_and_review(&config, &server_root())
        .await
        .expect("应返回 retrust 载荷而不是错误");
    let after = process_count(&staging);
    eprintln!(
        "retrust status={:?} stale={} previous={:?} spawns {}→{}",
        review.status, review.tools_stale, review.previous_executable_sha256, before, after
    );
    assert_eq!(review.status, McpStatus::Retrust, "替换后必须进入 retrust");
    assert!(review.tools_stale, "旧清单只能作参考");
    assert_eq!(
        review.previous_executable_sha256.as_deref(),
        Some(first.executable_sha256.as_str()),
        "必须给出上次批准的哈希"
    );
    assert_eq!(after, 0, "retrust 必须不 spawn");
    assert!(runtime.executors().is_empty(), "retrust 不得注册工具");
    runtime.shutdown_all().await;
}

/// 杀掉真实 server 进程后，观察当前实现的行为（崩溃重启策略是否接线）。
///
/// 只按「父进程 = 本测试进程 且命令含 windows_mcp」定位子进程，避免误杀用户其它 Python。
#[tokio::test]
#[ignore = "真机探针：需要 SHANIU_REAL_MCP_PYTHON"]
async fn real_server_killed_while_trusted() {
    let Some(python) = python() else {
        eprintln!("跳过：未设置 SHANIU_REAL_MCP_PYTHON");
        return;
    };
    let runtime = McpRuntime::new(&temp_root("kill"));
    let review = runtime
        .connect_and_review(&config(&python), &server_root())
        .await
        .expect("启动真实 server");
    runtime
        .resolve_trust(SERVER, &review.fingerprint, &decisions_for(&review))
        .await
        .expect("批准");
    let executor = runtime.executors().into_iter().next().expect("至少一个执行器");

    eprintln!("spawn 后子进程={:?}", child_pids_of(std::process::id()));
    let killed = kill_children_of(std::process::id());
    eprintln!("killed pids={killed:?}");
    assert!(!killed.is_empty(), "必须找到真实子进程");
    for second in 1..=3 {
        std::thread::sleep(std::time::Duration::from_secs(1));
        eprintln!("+{second}s 存活 python={:?}", child_pids_of(std::process::id()));
    }
    eprintln!("杀后状态={:?}", runtime.statuses().await);

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(45),
        executor.execute(json!({}), &context()),
    )
    .await;
    match outcome {
        Ok(Ok(output)) => {
            eprintln!("调用返回 success={} 文本={}", output.success, output.output.chars().take(300).collect::<String>());
            assert!(!output.success, "进程已死，调用不得成功");
        }
        Ok(Err(error)) => eprintln!("调用失败（预期）: {error}"),
        Err(_) => eprintln!("调用在 45s 内没有返回（挂住，是缺陷）"),
    }
    eprintln!("最终状态={:?}", runtime.statuses().await);
    runtime.shutdown_all().await;
}

/// 本进程的子进程里，命令行含 `windows_mcp` 的 PID。
fn child_pids_of(parent: u32) -> Vec<u32> {
    let script = format!(
        "@(Get-CimInstance Win32_Process | Where-Object {{ $_.ParentProcessId -eq {parent} -and $_.CommandLine -like '*windows_mcp*' }} | ForEach-Object {{ $_.ProcessId }})"
    );
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .expect("查询子进程");
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .filter_map(|value| value.parse().ok())
        .collect()
}

fn kill_children_of(parent: u32) -> Vec<u32> {
    let pids = child_pids_of(parent);
    for pid in &pids {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .output();
    }
    pids
}
