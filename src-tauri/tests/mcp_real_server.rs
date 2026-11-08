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
use shaniu_lib::mcp_client::module_tree;
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
    let runtime = std::sync::Arc::new(McpRuntime::new(&root));
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
    eprintln!("coverage={:?}", review.coverage);
    eprintln!("warnings={:?}", review.warnings);
    assert_eq!(
        review.coverage.kind, "module_tree",
        "python -m 启动必须覆盖模块代码树；若这里是 interpreter_only 说明定位失败"
    );
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
///
/// 必须用 `tokio::time::sleep`：`#[tokio::test]` 默认是单线程运行时，用
/// `std::thread::sleep` 会**阻塞监管任务**，让「重启」看起来永远没发生。
async fn wait_until_gone(marker: &std::path::Path, limit: std::time::Duration) -> std::time::Duration {
    let started = std::time::Instant::now();
    while started.elapsed() < limit {
        if process_count(marker) == 0 {
            return started.elapsed();
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    started.elapsed()
}

/// 在模块目录里找第一个 `.py`（跳过 `__pycache__`）。
fn walk_py(root: &std::path::Path) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).ok()?;
        let mut paths: Vec<PathBuf> = entries.filter_map(|entry| entry.ok()).map(|entry| entry.path()).collect();
        paths.sort();
        for path in paths {
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name.eq_ignore_ascii_case("__pycache__")) {
                    continue;
                }
                stack.push(path);
            } else if path.extension().is_some_and(|extension| extension == "py") {
                return Some(path);
            }
        }
    }
    None
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

    let runtime = std::sync::Arc::new(McpRuntime::new(&temp_root("retrust-trust")));
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
    let waited = wait_until_gone(&staging, std::time::Duration::from_secs(10)).await;
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

/// 真机崩溃重启：一次重启成功 → 再崩 → degraded，此后**不再 spawn**。
///
/// 这是上一轮真机暴露的资源泄漏（「每秒持续 spawn 新 python」）的复测。
/// 只按「父进程 = 本测试进程 且命令含 windows_mcp」定位子进程，避免误杀用户其它 Python。
#[tokio::test]
#[ignore = "真机探针：需要 SHANIU_REAL_MCP_PYTHON"]
async fn real_server_crash_restarts_once_then_degrades() {
    let Some(python) = python() else {
        eprintln!("跳过：未设置 SHANIU_REAL_MCP_PYTHON");
        return;
    };
    let runtime = std::sync::Arc::new(McpRuntime::new(&temp_root("kill")));
    let review = runtime
        .connect_and_review(&config(&python), &server_root())
        .await
        .expect("启动真实 server");
    runtime
        .resolve_trust(SERVER, &review.fingerprint, &decisions_for(&review))
        .await
        .expect("批准");
    let in_flight = runtime.executors().into_iter().next().expect("至少一个执行器");
    let marker = std::path::Path::new("windows_mcp");
    wait_until_instances(1, std::time::Duration::from_secs(5)).await;
    assert_eq!(server_instance_count(), 1, "批准后应恰有一个真实 server 实例");

    // 第一次崩溃：退避期内不得立刻补进程。
    let killed = kill_children_of(std::process::id());
    eprintln!("第一次崩溃 killed={killed:?}");
    assert!(!killed.is_empty(), "必须找到真实子进程");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(server_instance_count(), 0, "退避期内不得立刻重启");

    let waited = wait_until_instances(1, std::time::Duration::from_secs(20)).await;
    eprintln!("第 1 次自动重启出现耗时={waited:?}");
    assert_eq!(server_instance_count(), 1, "应恰好重启一次（不得补出第二个实例）");
    // 进程出现与状态回写之间有一个极短的窗口（先 spawn、后写 live_reason），
    // 这里等状态落地再断言，不能把「还没写完」当成「没写」。
    let status = wait_for_reason(&runtime, "第 1 次自动重启成功", std::time::Duration::from_secs(5))
        .await
        .expect("重启后状态应回写「第 1 次自动重启成功」");
    eprintln!("重启后状态={:?}", status);
    assert_eq!(status.status, McpStatus::Trusted);
    let reason = status.reason.unwrap_or_default();
    assert!(!reason.contains("首次连接"), "重启后不得再显示首连原因: {reason}");

    // 第二次崩溃：额度用尽，进入 degraded，此后不再 spawn。
    let killed = kill_children_of(std::process::id());
    eprintln!("第二次崩溃 killed={killed:?}");
    assert!(!killed.is_empty(), "必须找到重启后的子进程");
    let mut degraded = None;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let status = runtime
            .statuses()
            .await
            .into_iter()
            .find(|status| status.server_id == SERVER)
            .expect("状态应存在");
        if status.status == McpStatus::Degraded {
            degraded = Some(status);
            break;
        }
    }
    let degraded = degraded.expect("第二次崩溃后 20s 内应进入 degraded");
    eprintln!("degraded 状态={degraded:?}");
    let reason = degraded.reason.unwrap_or_default();
    assert!(reason.contains("不再自动重启"), "reason={reason}");

    // 观察 10 秒：实例数与进程树都必须稳定为 0（这就是「每秒新 python」现象消失的证据）。
    let started = std::time::Instant::now();
    while started.elapsed() < std::time::Duration::from_secs(10) {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let live = server_process_tree_count();
        assert_eq!(live, 0, "degraded 之后不得再 spawn（观察到 {live} 个进程）");
        assert_eq!(server_instance_count(), 0, "degraded 之后不得再有实例");
    }
    eprintln!(
        "degraded 后观察 {:?}：进程树恒为 0（marker 全机计数={}）",
        started.elapsed(),
        process_count(marker)
    );

    assert!(runtime.executors().is_empty(), "degraded 后必须撤掉执行器");
    let error = in_flight
        .execute(json!({}), &context())
        .await
        .expect_err("degraded 后调用必须明确失败");
    assert!(error.to_string().contains("授权已被撤销"), "错误应可解释: {error}");
    runtime.shutdown_all().await;
}

/// 真机模块树覆盖：改 `site-packages` 里任意一个 `.py` → 必须 retrust 且 spawn 为 0。
#[tokio::test]
#[ignore = "真机探针：需要 SHANIU_REAL_MCP_PYTHON"]
async fn real_site_packages_change_enters_retrust_without_spawn() {
    let Some(_python) = python() else {
        eprintln!("跳过：未设置 SHANIU_REAL_MCP_PYTHON");
        return;
    };
    let staging = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("qa-real-mcp-tree");
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

    // Windows-MCP 是可编辑安装（`.pth` → 仓库 `src/`）。为了不改用户源码，
    // 把 src 也复制一份，并把副本 venv 里的 `.pth` 重指到这份副本。
    let staged_src = staging.join("src");
    let copy_src = std::process::Command::new("robocopy")
        .arg(server_root().join("src"))
        .arg(&staged_src)
        .args(["/E", "/NFL", "/NDL", "/NJH", "/NJS", "/NP", "/R:1", "/W:1"])
        .output()
        .expect("运行 robocopy");
    assert!(copy_src.status.code().unwrap_or(9) < 8, "复制 src 失败: {copy_src:?}");
    // `.pth` 里写的是 Windows 反斜杠路径，而 `Path::display()` 用的是正斜杠；
    // 按字符串替换会静默匹配不到（上一轮就是这样把「覆盖 staging 副本」测成了
    // 「覆盖原仓库」）。改为逐行解析成路径再比较，且至少要改写一条，否则测试无效。
    let original_src = std::fs::canonicalize(server_root().join("src")).expect("解析源 src");
    let site_packages = venv.join("Lib").join("site-packages");
    let mut rewritten_pth = 0usize;
    for entry in std::fs::read_dir(&site_packages).expect("读 site-packages").flatten() {
        let path = entry.path();
        if !path.extension().is_some_and(|extension| extension == "pth") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut changed = false;
        let lines: Vec<String> = text
            .lines()
            .map(|line| {
                let trimmed = line.trim();
                if trimmed.is_empty()
                    || trimmed.starts_with('#')
                    || trimmed.starts_with("import ")
                    || trimmed.starts_with("import\t")
                {
                    return line.to_string();
                }
                let points_to_source = std::fs::canonicalize(trimmed)
                    .map(|target| target == original_src)
                    .unwrap_or(false);
                if points_to_source {
                    changed = true;
                    staged_src.display().to_string()
                } else {
                    line.to_string()
                }
            })
            .collect();
        if changed {
            std::fs::write(&path, lines.join("\n") + "\n").expect("重写 .pth");
            rewritten_pth += 1;
            eprintln!("重写 .pth: {}", path.display());
        }
    }
    assert!(rewritten_pth > 0, "没有改写任何 .pth：测试将覆盖原仓库而不是 staging 副本");

    let exe_path = venv.join("Scripts").join("python.exe");
    let args = vec!["-m".to_string(), "windows_mcp".to_string(), "serve".to_string()];
    let config = McpServerConfig {
        id: SERVER.to_string(),
        command: exe_path.display().to_string(),
        args: args.clone(),
        env: BTreeMap::new(),
        cwd: server_root().display().to_string(),
        ..McpServerConfig::default()
    };

    let runtime = std::sync::Arc::new(McpRuntime::new(&temp_root("tree-trust")));
    let first = runtime
        .connect_and_review(&config, &server_root())
        .await
        .expect("复制出来的解释器应能启动");
    eprintln!("coverage={:?}", first.coverage);
    assert_eq!(
        first.coverage.kind, "module_tree",
        "python -m 启动必须覆盖模块代码树，而不是只覆盖解释器"
    );
    assert!(first.coverage.detail.contains("windows_mcp"));
    runtime
        .resolve_trust(SERVER, &first.fingerprint, &decisions_for(&first))
        .await
        .expect("批准");
    runtime.shutdown_all().await;
    let waited = wait_until_gone(&staging, std::time::Duration::from_secs(15)).await;
    eprintln!("shutdown 后进程清零耗时={waited:?}");
    assert_eq!(process_count(&staging), 0);

    // 改模块代码（不动解释器）：追加一行注释即可改变内容哈希。
    // 先确认覆盖的确实是 staging 副本：否则改文件改不到指纹，测试会假通过。
    let before_tree = module_tree::digest_for(&exe_path, &args)
        .expect("计算模块树")
        .expect("python -m 必须产出模块树摘要");
    assert!(
        before_tree.root.contains("qa-real-mcp-tree"),
        "定位到的是 {}，不是 staging 副本；测试会覆盖错误的目录",
        before_tree.root
    );
    let package = staged_src.join("windows_mcp");
    let victim = walk_py(&package).expect("windows_mcp 包内应有 .py");
    let mut source = std::fs::read_to_string(&victim).expect("读源码");
    source.push_str("\n# shaniu probe: module tree change\n");
    std::fs::write(&victim, source).expect("写回源码");
    eprintln!("改写 {}", victim.display());

    let after_tree = module_tree::digest_for(&exe_path, &args)
        .expect("计算模块树")
        .expect("python -m 必须产出模块树摘要");
    assert_ne!(
        before_tree.sha256, after_tree.sha256,
        "改一个 .py 的内容必须改变模块树摘要（否则指纹根本没覆盖模块代码）"
    );

    let before = process_count(&staging);
    let review = runtime
        .connect_and_review(&config, &server_root())
        .await
        .expect("应返回 retrust 载荷而不是错误");
    let after = process_count(&staging);
    eprintln!(
        "模块树变化 retrust status={:?} reason={:?} spawns {}→{}",
        review.status, review.reason, before, after
    );
    assert_eq!(review.status, McpStatus::Retrust, "模块代码变化必须进入 retrust");
    assert!(
        review.reason.as_deref().unwrap_or_default().contains("模块代码树"),
        "reason 应指出是模块代码树变化: {:?}",
        review.reason
    );
    assert_eq!(after, 0, "retrust 必须不 spawn");
    assert!(runtime.executors().is_empty());
    runtime.shutdown_all().await;
}

/// 本进程的子进程里，命令行含 `windows_mcp` 的 python PID。
///
/// 必须按进程名过滤：查询用的 powershell 自身也是本进程的子进程，且它的命令行里
/// 带着 `windows_mcp` 这段字面量，不过滤会把自己数进去。
fn child_pids_of(parent: u32) -> Vec<u32> {
    let script = format!(
        "@(Get-CimInstance Win32_Process | Where-Object {{ $_.ParentProcessId -eq {parent} -and $_.Name -like 'python*' -and $_.CommandLine -like '*windows_mcp*' }} | ForEach-Object {{ $_.ProcessId }})"
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
        // `/T` 连子进程一起杀：venv 的 `python.exe` 是重定向器，真正的解释器是它的
        // 子进程；只杀重定向器会留下孤儿解释器，污染后续计数。
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .output();
    }
    pids
}

/// 本进程名下的 server **逻辑实例**数。
///
/// venv 的 `python.exe` 是重定向器：它会再起一个真正的解释器子进程，两者命令行相同，
/// 按命令行数会把 1 个 server 数成 2 个。按「本进程的直接子进程」计数才是实例数。
fn server_instance_count() -> usize {
    child_pids_of(std::process::id()).len()
}

/// 本测试启动的 server **进程树**计数（含 venv 重定向器与它拉起的解释器）。
///
/// 按父进程限定范围：全机命令行匹配会把用户其它 Python server 算进来，那种计数不可用。
fn server_process_tree_count() -> usize {
    let self_pid = std::process::id();
    let script = format!(
        "$self={self_pid}; $kids=@(Get-CimInstance Win32_Process | Where-Object {{ $_.ParentProcessId -eq $self -and $_.Name -like 'python*' }} | ForEach-Object {{ $_.ProcessId }}); @(Get-CimInstance Win32_Process | Where-Object {{ $_.Name -like 'python*' -and $_.CommandLine -like '*windows_mcp*' -and ($_.ParentProcessId -eq $self -or $kids -contains $_.ParentProcessId) }}).Count"
    );
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .expect("运行 powershell 统计进程树");
    String::from_utf8_lossy(&output.stdout).trim().parse().unwrap_or(0)
}

/// 等状态原因里出现某个片段，返回命中的状态。
async fn wait_for_reason(
    runtime: &McpRuntime,
    needle: &str,
    limit: std::time::Duration,
) -> Option<shaniu_lib::mcp_client::runtime::ServerStatus> {
    let started = std::time::Instant::now();
    while started.elapsed() < limit {
        let status = runtime
            .statuses()
            .await
            .into_iter()
            .find(|status| status.server_id == SERVER);
        if let Some(status) = status {
            if status.reason.as_deref().unwrap_or_default().contains(needle) {
                return Some(status);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    None
}

/// 等实例数达到期望值，返回实际耗时；超时原样返回上限。
async fn wait_until_instances(want: usize, limit: std::time::Duration) -> std::time::Duration {
    let started = std::time::Instant::now();
    while started.elapsed() < limit {
        if server_instance_count() == want {
            return started.elapsed();
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    started.elapsed()
}
