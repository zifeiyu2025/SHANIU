//! SH-064 5d 验收：MCP stdio server 跑在 **AppContainer** 里时的行为边界。
//!
//! 与 `mcp_process.rs` 的分工：那边验协议语义（受限令牌路径），这边验 OS 级隔离
//! ——容器内握手/清单往返、越界读写被拒、`allowed_roots` 的 read/write 两档、
//! `TEMP` 逐 server 重定向、失败置 `blocked` 而不回退受限令牌。
//!
//! server 是 `tests/fixtures/mcp_sandbox_probe.js`，工具直接做文件读写与 HTTPS 请求。
//!
//! 为什么要把 node 复制到 `target/mcp-sandbox-node/`：本机 `D:\Programs\nodejs` 归
//! SYSTEM 所有，普通用户改不了它的 DACL，容器也读不到它（正是 5b 里 `blocked` 的
//! 真实场景）。复制到用户可写目录后，`toolchain_roots` 能给容器加只读 ACE。
//!
//! 网络用例默认 `#[ignore]`（要外网）：`cargo test --test mcp_sandbox -- --ignored --nocapture`

#![cfg(windows)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use shaniu_lib::config::{
    AllowedRoot, McpServerConfig, OsSandbox, SandboxMode, WorkspaceAccess,
};
use shaniu_lib::mcp_client::command;
use shaniu_lib::mcp_client::output::flatten_call_result;
use shaniu_lib::mcp_client::process::SpawnSpec;
use shaniu_lib::mcp_client::runtime::{McpRuntime, McpStatus};
use shaniu_lib::mcp_client::session::McpSession;

const HANDSHAKE: Duration = Duration::from_secs(30);
const CALL: Duration = Duration::from_secs(20);

fn unique_dir(tag: &str) -> shaniu_lib::test_support::TestTempDir {
    shaniu_lib::test_support::TestTempDir::new(&format!("mcpsb-{tag}"))
}

/// 用户主目录下、不在任何授权范围内的目录（`%TEMP%` 是 exec 的允许位置，不能当越界靶子）。
///
/// 这是**唯一**允许落到工作区/`%TEMP%` 之外的测试写入（决策见 AGENTS.md §3 与用户
/// 2026-11-09 指令）：越界读写的边界只能在受保护范围之外验证，拿允许位置当靶子测不出
/// 隔离是否生效。约束：名字带 pid + ulid 不可预测（防撞上用户真实目录），
/// 结束时必须 `remove_outside_and_assert` 断言已删干净。
fn outside_dir(tag: &str) -> PathBuf {
    let home = std::env::var_os("USERPROFILE").expect("探针需要 USERPROFILE");
    let dir = PathBuf::from(home).join(format!(
        "shaniu-mcpsb-{tag}-{}-{}",
        std::process::id(),
        ulid::Ulid::generate()
    ));
    std::fs::create_dir_all(&dir).expect("创建越界目录");
    dir
}

/// 删除越界探针目录并**断言已不存在**：先有界重试（容器/子进程句柄释放有延迟），
/// 仍失败就 fail——越界目录留在用户主目录里是不可接受的。
fn remove_outside_and_assert(dir: &Path) {
    let mut last = None;
    for attempt in 0..10 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(100));
        }
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => last = Some(error),
        }
    }
    panic!(
        "越界探针目录未能删除，会残留在用户主目录：{}（{:?}）",
        dir.display(),
        last
    );
}

fn fixture_path() -> String {
    format!(
        "{}/tests/fixtures/mcp_sandbox_probe.js",
        env!("CARGO_MANIFEST_DIR").replace('\\', "/")
    )
}

/// 复制一份用户可写的 node（见文件头说明）。已复制且大小一致就复用。
///
/// **不要直接覆盖已存在的 node.exe**：并行的其它用例可能正从同一个路径起进程，
/// 覆盖会撞 `ERROR_SHARING_VIOLATION`（os error 32，CI 上偶发红）。
/// 改为"写唯一临时文件再 rename"——rename 到已存在的目标在 Windows 上会失败，
/// 失败即说明别的线程已经放好了，视为成功；最后统一校验大小，对不上就 fail。
fn staged_node() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("mcp-sandbox-node");
    std::fs::create_dir_all(&dir).expect("创建 node staging 目录");
    let exe = dir.join("node.exe");
    let source = command::resolve_executable("node").expect("解析 node 可执行文件");
    let expected = std::fs::metadata(&source).map(|meta| meta.len()).ok();
    let same = exe.is_file()
        && std::fs::metadata(&exe).map(|meta| meta.len()).ok() == expected;
    if !same {
        let staged = dir.join(format!("node-{}-{:?}.tmp", std::process::id(), std::thread::current().id()));
        std::fs::copy(&source, &staged).expect("复制 node 到临时 staging 文件");
        if std::fs::rename(&staged, &exe).is_err() {
            // 目标已存在（别的用例先到）：临时文件作废，改用已有的那份。
            let _ = std::fs::remove_file(&staged);
        }
    }
    let actual = std::fs::metadata(&exe).map(|meta| meta.len()).ok();
    assert_eq!(
        actual, expected,
        "staging 的 node.exe 大小与源不一致（{} vs {:?}），可能是上一次复制被中断",
        exe.display(),
        expected
    );
    exe
}

fn allowed(path: &Path, access: WorkspaceAccess) -> AllowedRoot {
    AllowedRoot { path: path.display().to_string(), access }
}

fn config(
    id: &str,
    node: &Path,
    workspace: &Path,
    allowed_roots: Vec<AllowedRoot>,
    allow_network: bool,
) -> McpServerConfig {
    McpServerConfig {
        id: id.to_string(),
        command: node.display().to_string(),
        args: vec![fixture_path(), workspace.display().to_string()],
        env: BTreeMap::new(),
        cwd: workspace.display().to_string(),
        allowed_roots,
        allow_network,
        // 工作区默认只读（Q3），写用例必须走 allowed_roots 的 write 档。
        workspace_access: WorkspaceAccess::Read,
        ..McpServerConfig::default()
    }
}

async fn started(config: &McpServerConfig, workspace: &Path) -> McpSession {
    shaniu_lib::sandbox::set_os_mode(OsSandbox::AppContainer);
    let exe = command::resolve_executable(&config.command).expect("解析 node");
    let hash = command::file_sha256(&exe).expect("计算 node 哈希");
    let mut session = McpSession::start(SpawnSpec {
        config,
        sandbox_mode: SandboxMode::WorkspaceWrite,
        cwd: workspace,
        workspace_root: workspace,
        approved_sha256: &hash,
        approved_module_tree: None,
        trusted_server_ids: &[],
    })
    .await
    .expect("容器内应能启动 node server");
    if let Err(error) = session.initialize(HANDSHAKE).await {
        let code = session.wait_exit().await;
        panic!(
            "容器内握手应成功: {error}; 退出码={code}; 诊断={}",
            session.crash_report(code)
        );
    }
    session
}

async fn call(session: &mut McpSession, name: &str, args: Value) -> (String, bool) {
    let value = session
        .call_tool(name, args, CALL, None)
        .await
        .unwrap_or_else(|error| panic!("调用 {name} 失败: {error}"));
    flatten_call_result(&value)
}

/// 记录 `ServerStatusChanged`，用事件（而不是 trust 记录投影）确认 blocked 终态。
#[derive(Default)]
struct RecordingSink {
    statuses: std::sync::Mutex<Vec<(String, McpStatus, Option<String>)>>,
}

impl shaniu_lib::mcp_client::runtime::McpEventSink for RecordingSink {
    fn emit(&self, event: &shaniu_lib::mcp_client::runtime::McpEvent) {
        if let shaniu_lib::mcp_client::runtime::McpEvent::ServerStatusChanged {
            server_id,
            status,
            reason,
        } = event
        {
            self.statuses
                .lock()
                .unwrap()
                .push((server_id.clone(), *status, reason.clone()));
        }
    }
}

impl RecordingSink {
    fn blocked_reason(&self, server_id: &str) -> Option<String> {
        self.statuses
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(id, status, _)| id == server_id && *status == McpStatus::Blocked)
            .and_then(|(_, _, reason)| reason.clone())
    }
}

#[tokio::test]
async fn container_handshake_and_path_boundaries() {
    let node = staged_node();
    let workspace = unique_dir("ws");
    let outside = outside_dir("outside");
    let read_root = outside_dir("ro");
    let write_root = outside_dir("rw");
    std::fs::write(workspace.join("inside.txt"), "inside-marker").unwrap();
    std::fs::write(outside.join("secret.txt"), "outside-secret-marker").unwrap();
    std::fs::write(read_root.join("secret.txt"), "allowed-read-marker").unwrap();

    let config = config(
        &format!("sandbox-{}", std::process::id()),
        &node,
        &workspace,
        vec![
            allowed(&read_root, WorkspaceAccess::Read),
            allowed(&write_root, WorkspaceAccess::Write),
        ],
        false,
    );
    let mut session = started(&config, &workspace).await;

    // 1) 容器内握手 + 清单往返。
    let identity = session.identity().expect("握手后应有身份");
    assert_eq!(identity.name, "sandbox-probe");
    let tools = session.list_tools(HANDSHAKE).await.expect("tools/list 应成功");
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(names, vec!["read_file", "write_file", "temp_dir", "fetch_url"]);

    // 2) 工作区内读：默认只读档应允许。
    let (text, is_error) = call(&mut session, "read_file", json!({"path": workspace.join("inside.txt")})).await;
    assert!(!is_error, "工作区内读应成功: {text}");
    assert!(text.contains("inside-marker"));

    // 3) 工作区外读：容器读隔离必须拒绝。
    let (text, is_error) = call(&mut session, "read_file", json!({"path": outside.join("secret.txt")})).await;
    assert!(is_error, "工作区外读必须失败，实际: {text}");
    assert!(!text.contains("outside-secret-marker"), "越界内容不得进入容器: {text}");

    // 4) 工作区默认只读：写入必须被拒，且不得落盘。
    let target = workspace.join("denied.txt");
    let (text, is_error) = call(
        &mut session,
        "write_file",
        json!({"path": target, "text": "x"}),
    )
    .await;
    assert!(is_error, "工作区默认只读时写入必须失败，实际: {text}");
    assert!(!target.exists(), "被拒的写入不得落盘");

    // 5) allowed_roots read 档：读可以、写不可以。
    let (text, is_error) = call(&mut session, "read_file", json!({"path": read_root.join("secret.txt")})).await;
    assert!(!is_error, "allowed_roots(read) 应允许读: {text}");
    assert!(text.contains("allowed-read-marker"));
    let (text, is_error) = call(
        &mut session,
        "write_file",
        json!({"path": read_root.join("nope.txt"), "text": "x"}),
    )
    .await;
    assert!(is_error, "allowed_roots(read) 不得允许写，实际: {text}");
    assert!(!read_root.join("nope.txt").exists());

    // 6) allowed_roots write 档：写可以，且真实落在宿主机上。
    let (text, is_error) = call(
        &mut session,
        "write_file",
        json!({"path": write_root.join("ok.txt"), "text": "written-by-probe"}),
    )
    .await;
    assert!(!is_error, "allowed_roots(write) 应允许写: {text}");
    assert_eq!(std::fs::read_to_string(write_root.join("ok.txt")).unwrap(), "written-by-probe");

    // 7) 未声明的越界写仍然被拒。
    let (text, is_error) = call(
        &mut session,
        "write_file",
        json!({"path": outside.join("escaped.txt"), "text": "x"}),
    )
    .await;
    assert!(is_error, "越界写必须失败，实际: {text}");
    assert!(!outside.join("escaped.txt").exists(), "越界写不得落盘");

    // 8) TEMP 逐 server 隔离，且该目录可写。
    // AppContainer 会把 TEMP/TMP 改写到自己 profile 的 AC\Temp（也是逐 server）；
    // 若系统保留了我们传入的 server_temp，则断言同样成立。
    let (temp, is_error) = call(&mut session, "temp_dir", json!({})).await;
    assert!(!is_error, "temp_dir 应成功: {temp}");
    assert!(
        temp.contains("mcp-temp") || temp.contains("\\AC\\Temp"),
        "TEMP 应指向逐 server 临时目录: {temp}"
    );
    assert!(temp.contains(&config.id), "TEMP 应带 server id: {temp}");
    let marker = Path::new(temp.trim()).join("probe.txt");
    let (text, is_error) = call(
        &mut session,
        "write_file",
        json!({"path": marker, "text": "temp-ok"}),
    )
    .await;
    assert!(!is_error, "逐 server TEMP 应可写: {text}");
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "temp-ok");

    session.shutdown();
    // workspace 由 TestTempDir 守卫自动清理；outside 目录不归夹具管，保持手工删。
    for dir in [&outside, &read_root, &write_root] {
        remove_outside_and_assert(dir);
    }
}

/// 容器准备失败时必须 `blocked`，且错误说明「未回退到受限令牌」。
///
/// 失败注入：把逐 server 的临时目录路径先占成一个**文件**，`create_dir_all` 必定失败。
/// 这发生在任何 DACL 改动之前，不碰系统目录，也不依赖管理员权限。
#[tokio::test]
async fn container_failure_is_blocked_not_silently_fallen_back() {
    shaniu_lib::sandbox::set_os_mode(OsSandbox::AppContainer);
    let node = staged_node();
    let workspace = unique_dir("blocked");
    let root = unique_dir("blocked-runtime");
    let server_id = format!("blocked-{}", std::process::id());
    let config = config(&server_id, &node, &workspace, Vec::new(), false);

    let temp_parent = shaniu_lib::config::resolve_dirs_from_env()
        .data_root
        .join("sandbox")
        .join("mcp-temp");
    std::fs::create_dir_all(&temp_parent).expect("创建 mcp-temp 父目录");
    let blocker = temp_parent.join(&server_id);
    let _ = std::fs::remove_dir_all(&blocker);
    std::fs::write(&blocker, b"occupied").expect("占住临时目录路径");

    let runtime = std::sync::Arc::new(McpRuntime::new(&root));
    let sink = std::sync::Arc::new(RecordingSink::default());
    runtime.set_sink(sink.clone());
    let error = runtime
        .connect_and_review(&config, &workspace)
        .await
        .expect_err("临时目录不可用时必须报错，而不是悄悄退回受限令牌");
    eprintln!("blocked 原因：{error}");
    assert!(
        error.contains("未回退到受限令牌"),
        "错误必须说明没有静默降级: {error}"
    );
    let reason = sink
        .blocked_reason(&config.id)
        .expect("失败后必须广播 blocked 状态事件");
    assert!(reason.contains("未回退到受限令牌"), "{reason}");
    assert!(runtime.executors().is_empty(), "blocked 不得注册任何执行器");

    runtime.shutdown_all().await;
    shaniu_lib::sandbox::release_mcp_server(&server_id);
    std::fs::remove_file(&blocker).ok();
    std::fs::remove_dir_all(&workspace).ok();
    std::fs::remove_dir_all(&root).ok();
}

/// `allow_network` 是**容器能力**开关：false 断网，true 放行。
///
/// 必须打外网而不是 loopback：AppContainer 的 loopback 隔离是另一条限制，
/// 拿 loopback 当「放行后应该通」的靶子会得到与授权无关的假红。
#[tokio::test]
#[ignore = "需要外网：验证 per-server allow_network 真正落到容器能力上"]
async fn container_network_follows_allow_network_switch() {
    let node = staged_node();
    let workspace = unique_dir("net");
    let url = std::env::var("SHANIU_NETWORK_PROBE_URL")
        .unwrap_or_else(|_| "https://www.baidu.com/".to_string());

    let pid = std::process::id();
    let mut denied = started(
        &config(&format!("net-deny-{pid}"), &node, &workspace, Vec::new(), false),
        &workspace,
    )
    .await;
    let (text, is_error) = call(&mut denied, "fetch_url", json!({"url": url})).await;
    eprintln!("allow_network=false 结果：is_error={is_error} {text}");
    assert!(is_error, "allow_network=false 时容器必须连不出去: {text}");
    denied.shutdown();

    let mut allowed = started(
        &config(&format!("net-allow-{pid}"), &node, &workspace, Vec::new(), true),
        &workspace,
    )
    .await;
    let (text, is_error) = call(&mut allowed, "fetch_url", json!({"url": url})).await;
    eprintln!("allow_network=true 结果：is_error={is_error} {text}");
    assert!(!is_error, "allow_network=true 时容器应能出站: {text}");
    assert!(text.contains("status=200"), "应拿到 HTTP 200: {text}");
    allowed.shutdown();

    std::fs::remove_dir_all(&workspace).ok();
}
