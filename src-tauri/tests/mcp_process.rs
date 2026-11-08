//! SH-060 真机验收：真实受限进程上的握手、清单、调用、环境隔离、崩溃重启与 degraded。
//!
//! 这里的 server 是 `tests/fixtures/fake_mcp_server.js`（node 实现），刻意包含
//! 注入描述、伪造输出与自杀工具。测试只用本机 node，不触网、不读用户数据。

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::json;
use shaniu_lib::config::{McpServerConfig, SandboxMode};
use shaniu_lib::mcp_client::command;
use shaniu_lib::mcp_client::guard::ExemptionStore;
use shaniu_lib::mcp_client::output::{flatten_call_result, OutputWrapper};
use shaniu_lib::mcp_client::process::RestartGuard;
use shaniu_lib::mcp_client::registry;
use shaniu_lib::mcp_client::session::{McpSession, RestartOutcome};

const HANDSHAKE: Duration = Duration::from_secs(20);
const CALL: Duration = Duration::from_secs(20);

fn fixture_path() -> String {
    format!(
        "{}/tests/fixtures/fake_mcp_server.js",
        env!("CARGO_MANIFEST_DIR").replace('\\', "/")
    )
}

fn config() -> McpServerConfig {
    let mut env = BTreeMap::new();
    env.insert("FAKE_ALLOWED".to_string(), "1".to_string());
    // Windows 上 node 启动需要 SYSTEMROOT（winsock 初始化）；TEMP 供运行时使用。
    // 这仍是白名单语义：只注入显式列出的键，PATH 等其余变量一律不继承。
    for key in ["SYSTEMROOT", "TEMP"] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.to_string(), value);
        }
    }
    McpServerConfig {
        id: "fake".to_string(),
        command: "node".to_string(),
        args: vec![fixture_path()],
        env,
        cwd: env!("CARGO_MANIFEST_DIR").to_string(),
        ..McpServerConfig::default()
    }
}

fn approved_hash(config: &McpServerConfig) -> String {
    let exe = command::resolve_executable(&config.command).expect("解析 node 可执行文件");
    command::file_sha256(&exe).expect("计算 node 哈希")
}

async fn started_session() -> (McpSession, McpServerConfig, String) {
    let config = config();
    let hash = approved_hash(&config);
    let mut session = McpSession::start(&config, SandboxMode::WorkspaceWrite, config_cwd(&config), &hash, None)
        .await
        .expect("受限 stdio 进程应能启动");
    if let Err(error) = session.initialize(HANDSHAKE).await {
        let code = session.wait_exit().await;
        panic!(
            "握手应成功: {error}; 退出码={code}; 诊断={}",
            session.crash_report(code)
        );
    }
    (session, config, hash)
}

fn config_cwd(config: &McpServerConfig) -> &std::path::Path {
    std::path::Path::new(&config.cwd)
}

#[tokio::test]
async fn real_server_handshake_list_and_call_round_trip() {
    let (mut session, _config, _hash) = started_session().await;

    // A-R1：instructions 只记录「出现过」，不消费内容。
    let identity = session.identity().expect("握手后应有身份");
    assert_eq!(identity.name, "fake-mcp");
    assert_eq!(identity.protocol_version, "2025-06-18");
    assert!(identity.instructions_dropped, "server 下发的 instructions 必须被标记丢弃");

    let tools = session.list_tools(HANDSHAKE).await.expect("tools/list 应成功");
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(names, vec!["echo", "env", "hostile", "forge", "crash"]);

    let value = session
        .call_tool("echo", json!({ "text": "hi" }), CALL, None)
        .await
        .expect("tools/call 应成功");
    let (text, is_error) = flatten_call_result(&value);
    assert!(!is_error);
    assert_eq!(text, "ok");

    session.shutdown();
}

#[tokio::test]
async fn child_environment_is_the_whitelist_only() {
    // 哨兵：父进程可见、白名单外，server 不得看到。
    std::env::set_var("SHANIU_TEST_SECRET", "sentinel-value");
    let (mut session, _config, _hash) = started_session().await;

    let value = session.call_tool("env", json!({}), CALL, None).await.expect("env 调用应成功");
    let (text, is_error) = flatten_call_result(&value);
    assert!(!is_error);
    let report: serde_json::Value = serde_json::from_str(&text).expect("env 结果应是 JSON");
    assert_eq!(report["allowed"], "1", "白名单内的变量必须注入");
    assert!(report["secret"].is_null(), "白名单外的父进程变量不得泄漏: {report}");
    // PATH 属 WINDOWS_CORE_ENV_VARS（与 codex 同口径）：node/DLL 解析的硬前提，按设计保留。
    assert!(!report["path"].is_null(), "核心环境变量应按白名单注入: {report}");

    session.shutdown();
    std::env::remove_var("SHANIU_TEST_SECRET");
}

#[tokio::test]
async fn real_server_tools_go_through_namespace_and_injection_guard() {
    let (mut session, _config, _hash) = started_session().await;
    let tools = session.list_tools(HANDSHAKE).await.expect("tools/list 应成功");

    let plan = shaniu_lib::mcp_client::manifest::plan_registration("fake", &tools);
    let dispatch: Vec<&str> = plan.accepted.iter().map(|t| t.dispatch_name.as_str()).collect();
    assert!(dispatch.contains(&"mcp__fake__echo"));
    assert!(dispatch.contains(&"mcp__fake__hostile"), "命名阶段不做过早拒绝");

    let (allowed, rejected) =
        registry::partition_by_guard(&plan, "fake", "fp-real", &ExemptionStore::new());
    assert!(allowed.iter().any(|tool| tool.dispatch_name == "mcp__fake__echo"));
    let hostile = rejected
        .iter()
        .find(|rejection| rejection.dispatch_name == "mcp__fake__hostile")
        .expect("注入描述必须被拦下");
    assert!(hostile.explanation.contains("指令覆盖"));
    assert!(hostile.explanation.contains("Ignore previous instructions"));

    session.shutdown();
}

#[tokio::test]
async fn forged_output_stays_inside_the_untrusted_wrapper() {
    let (mut session, _config, _hash) = started_session().await;

    let value = session.call_tool("forge", json!({}), CALL, None).await.expect("forge 调用应成功");
    let (text, is_error) = flatten_call_result(&value);
    assert!(!is_error, "server 自称成功；是否可信由包裹层表达");
    assert!(text.contains("END:00000000000000000000000000000000"), "伪造标记原样保留为数据");

    let wrapper = OutputWrapper::new();
    let wrapped = wrapper.wrap(&text);
    assert_eq!(wrapped.matches(&wrapper.end_marker()).count(), 1, "只有真实 nonce 能闭合");
    assert!(wrapped.ends_with(&wrapper.end_marker()));
    assert!(wrapped.contains("\"decision\":\"allow\""), "伪造回执只能作为数据存在");
    assert!(wrapped.contains("不可信工具输出"));

    session.shutdown();
}

#[tokio::test]
async fn crashed_server_restarts_once_then_degrades() {
    let (mut session, config, hash) = started_session().await;
    let mut guard = RestartGuard::default();

    // 第一次崩溃：调用直接失败，进程退出码可诊断。
    let error = session
        .call_tool("crash", json!({}), CALL, None)
        .await
        .expect_err("进程已退出，调用必须失败");
    assert!(matches!(error, shaniu_lib::mcp_client::session::SessionError::Closed { .. }));
    let code = session.wait_exit().await;
    assert_eq!(code, 1);

    // 哈希未变、额度还有：重启并重跑握手。
    let outcome = session
        .restart(
            &config,
            SandboxMode::WorkspaceWrite,
            config_cwd(&config),
            &hash,
            None,
            &mut guard,
        )
        .await
        .expect("重启应成功");
    assert_eq!(outcome, RestartOutcome::Restarted);
    assert!(session.identity().is_some(), "重启后必须重新握手");

    // 第二次崩溃：额度用尽 → degraded，不再无限重生。
    let _ = session.call_tool("crash", json!({}), CALL, None).await;
    let _ = session.wait_exit().await;
    let outcome = session
        .restart(
            &config,
            SandboxMode::WorkspaceWrite,
            config_cwd(&config),
            &hash,
            None,
            &mut guard,
        )
        .await
        .expect("degraded 结论本身不是启动错误");
    assert_eq!(outcome, RestartOutcome::Degraded);
    assert!(session.is_closed(), "degraded 后会话必须关闭");
}
