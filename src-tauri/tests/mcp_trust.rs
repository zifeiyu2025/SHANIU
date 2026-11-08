//! SH-063 真机验收：指纹审查、逐工具风险、信任持久化、撤销与注入豁免正向往返。
//!
//! 覆盖 docs/02 §3.1.7 的负向用例：首连不成授权、未勾选不注册、
//! server 自报注解不改风险、撤销后立即不可调用、工具清单变化强制重批。
//! server 仍用 `tests/fixtures/fake_mcp_server.js`（本机 node，不触网）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::json;
use shaniu_lib::config::{McpServerConfig, SandboxMode};
use shaniu_lib::events::{EventMsg, EventSink, RiskLevel};
use shaniu_lib::mcp_client::runtime::{
    McpEvent, McpEventSink, McpRuntime, McpStatus, ReviewPayload, TrustDecisionInput,
};
use shaniu_lib::tool_router::{ToolContext, ToolExecutor};
use tokio_util::sync::CancellationToken;

const SERVER: &str = "fake";

static SEQ: AtomicU32 = AtomicU32::new(0);

fn fixture_path() -> String {
    format!(
        "{}/tests/fixtures/fake_mcp_server.js",
        env!("CARGO_MANIFEST_DIR").replace('\\', "/")
    )
}

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "shaniu-mcp-trust-{tag}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("创建临时配置根");
    dir
}

fn config(extra_tool: bool) -> McpServerConfig {
    let mut env = BTreeMap::new();
    env.insert("FAKE_ALLOWED".to_string(), "1".to_string());
    // Windows 上 node 启动需要 SYSTEMROOT（winsock）；TEMP 供运行时使用。
    for key in ["SYSTEMROOT", "TEMP"] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.to_string(), value);
        }
    }
    if extra_tool {
        env.insert("FAKE_EXTRA_TOOL".to_string(), "1".to_string());
    }
    McpServerConfig {
        id: SERVER.to_string(),
        command: "node".to_string(),
        args: vec![fixture_path()],
        env,
        cwd: env!("CARGO_MANIFEST_DIR").to_string(),
        ..McpServerConfig::default()
    }
}

#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<McpEvent>>,
}

impl Recorder {
    fn count_of(&self, predicate: impl Fn(&McpEvent) -> bool) -> usize {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| predicate(event))
            .count()
    }
}

impl McpEventSink for Recorder {
    fn emit(&self, event: &McpEvent) {
        self.events.lock().unwrap().push(event.clone());
    }
}

fn runtime_with_sink(root: &Path) -> (McpRuntime, Arc<Recorder>) {
    let runtime = McpRuntime::new(root);
    let sink = Arc::new(Recorder::default());
    runtime.set_sink(sink.clone());
    (runtime, sink)
}

fn decide(name: &str, approved: bool, risk: Option<RiskLevel>) -> TrustDecisionInput {
    TrustDecisionInput {
        dispatch_name: name.to_string(),
        approved,
        risk,
        enable_injection: false,
    }
}

fn context() -> ToolContext {
    /// 丢弃事件的出口：这些测试只关心工具调用结果。
    struct DiscardSink;

    impl EventSink for DiscardSink {
        fn emit(&self, _msg: &EventMsg) -> Result<(), String> {
            Ok(())
        }
    }

    ToolContext::new(
        workspace(),
        CancellationToken::new(),
        "call-mcp-trust",
        Arc::new(DiscardSink),
        SandboxMode::WorkspaceWrite,
    )
}

fn executor_names(runtime: &McpRuntime) -> Vec<String> {
    let mut names: Vec<String> = runtime
        .executors()
        .iter()
        .map(|executor| executor.spec().name.clone())
        .collect();
    names.sort();
    names
}

fn executor(runtime: &McpRuntime, name: &str) -> Arc<dyn ToolExecutor> {
    runtime
        .executors()
        .into_iter()
        .find(|executor| executor.spec().name == name)
        .unwrap_or_else(|| panic!("执行器 {name} 未注册"))
}

fn tool<'a>(
    review: &'a ReviewPayload,
    name: &str,
) -> &'a shaniu_lib::mcp_client::runtime::ReviewTool {
    review
        .tools
        .iter()
        .find(|tool| tool.dispatch_name == name)
        .unwrap_or_else(|| panic!("审查载荷里没有 {name}"))
}

#[tokio::test]
async fn first_connect_requires_review_and_registers_nothing() {
    let root = temp_root("first-review");
    let (runtime, sink) = runtime_with_sink(&root);

    let review = runtime
        .connect_and_review(&config(false), &workspace())
        .await
        .expect("首次连接应进入审查");

    assert_eq!(review.status, McpStatus::Review);
    assert_eq!(review.fingerprint.len(), 64, "指纹应是 SHA-256 十六进制");
    assert_eq!(review.executable_sha256.len(), 64);
    assert!(!review.executable.is_empty());
    assert!(runtime.executors().is_empty(), "审查阶段任何工具都不得注册");

    // 审批对象是指纹：载荷必须带齐可执行文件、argv、cwd、env 摘要与原始工具清单。
    assert!(review
        .argv
        .iter()
        .any(|arg| arg.ends_with("fake_mcp_server.js")));
    assert!(!review.cwd.is_empty());
    assert!(
        review
            .env
            .iter()
            .any(|line| line.starts_with("FAKE_ALLOWED = sha256:")),
        "env 只展示键与摘要: {:?}",
        review.env
    );
    assert!(
        review.env.iter().all(|line| !line.contains("= 1")),
        "env 值不得明文出现: {:?}",
        review.env
    );
    assert!(review.tools.iter().any(|tool| tool.raw_name == "crash"));

    // 逐工具默认 destructive、默认不批准（用户没表态前一律按最高风险处理）。
    assert!(review
        .tools
        .iter()
        .all(|tool| tool.risk == RiskLevel::Destructive));
    assert!(review.tools.iter().all(|tool| !tool.approved));
    assert!(review
        .tools
        .iter()
        .all(|tool| tool.fingerprint == review.fingerprint));

    // 注入命中可解释，且默认不放行。
    let hostile = tool(&review, "mcp__fake__hostile");
    assert!(!hostile.guard_hits.is_empty());
    assert!(hostile
        .guard_explanation
        .as_deref()
        .unwrap_or("")
        .contains("指令覆盖"));
    assert!(!hostile.guard_exempted);

    // 事件：审查事件 + 状态事件。
    assert_eq!(
        sink.count_of(|event| matches!(event, McpEvent::ManifestReview { .. })),
        1
    );
    assert_eq!(
        sink.count_of(|event| matches!(
            event,
            McpEvent::ServerStatusChanged {
                status: McpStatus::Review,
                ..
            }
        )),
        1
    );

    // 指纹不匹配的裁决必须被拒绝（防止按旧指纹洗白新形态）。
    let error = runtime
        .resolve_trust(
            SERVER,
            "not-the-fingerprint",
            &[decide("mcp__fake__echo", true, None)],
        )
        .await
        .expect_err("指纹不匹配必须拒绝");
    assert!(error.contains("指纹"), "错误应说明指纹不一致: {error}");

    runtime.shutdown_all().await;
}

#[tokio::test]
async fn approval_registers_only_checked_tools_with_user_risk() {
    let root = temp_root("approve");
    let (runtime, _sink) = runtime_with_sink(&root);
    let review = runtime
        .connect_and_review(&config(false), &workspace())
        .await
        .unwrap();

    let resolved = runtime
        .resolve_trust(
            SERVER,
            &review.fingerprint,
            &[
                decide("mcp__fake__echo", true, Some(RiskLevel::Read)),
                decide("mcp__fake__env", true, Some(RiskLevel::Destructive)),
                TrustDecisionInput {
                    dispatch_name: "mcp__fake__hostile".into(),
                    approved: true,
                    risk: Some(RiskLevel::Destructive),
                    enable_injection: true,
                },
                decide("mcp__fake__forge", false, Some(RiskLevel::Read)),
            ],
        )
        .await
        .expect("裁决应成功");

    assert_eq!(resolved.status, McpStatus::Trusted);
    // 批准的三个（hostile 显式启用）注册；forge 明确不批准；crash 未表态。
    assert_eq!(
        executor_names(&runtime),
        vec!["mcp__fake__echo", "mcp__fake__env", "mcp__fake__hostile"]
    );

    // 档位逐工具落进 ToolSpec，且不受 server 注解影响（annotations 为空也必须是用户值）。
    assert_eq!(
        executor(&runtime, "mcp__fake__echo").spec().risk,
        Some(RiskLevel::Read)
    );
    assert_eq!(
        executor(&runtime, "mcp__fake__env").spec().risk,
        Some(RiskLevel::Destructive)
    );

    // 批准的工具真的能调用（输出仍在不可信包裹内）。
    let output = executor(&runtime, "mcp__fake__echo")
        .execute(json!({ "text": "hi" }), &context())
        .await
        .expect("批准的 echo 应可调用");
    assert!(output.success);
    assert!(output.output.contains("ok"));
    assert!(output.output.contains("不可信工具输出"));

    runtime.shutdown_all().await;
}

#[tokio::test]
async fn revoke_trust_invalidates_inflight_executors_immediately() {
    let root = temp_root("revoke");
    let (runtime, _sink) = runtime_with_sink(&root);
    let review = runtime
        .connect_and_review(&config(false), &workspace())
        .await
        .unwrap();
    runtime
        .resolve_trust(
            SERVER,
            &review.fingerprint,
            &[decide("mcp__fake__echo", true, None)],
        )
        .await
        .unwrap();

    // 已经下发给任务的执行器副本：撤销后必须立刻失败，不能等下一轮重建 router。
    let in_flight = executor(&runtime, "mcp__fake__echo");

    runtime.revoke_trust(SERVER).await.expect("撤销应成功");
    assert!(runtime.executors().is_empty(), "撤销后不得再暴露执行器");
    assert!(
        runtime.statuses().await.is_empty(),
        "撤销后不得残留信任记录"
    );

    let error = in_flight
        .execute(json!({ "text": "after" }), &context())
        .await
        .expect_err("撤销后调用必须失败");
    assert!(
        error.to_string().contains("授权已被撤销"),
        "错误应说明授权已撤销: {error}"
    );

    runtime.shutdown_all().await;
    // 撤销后持久化文件里不再有这台服务器的记录（文件可以存在，但内容必须为空表）。
    let raw = std::fs::read_to_string(root.join("secrets").join("mcp-trust.json"))
        .unwrap_or_else(|_| "{\"servers\":[]}".to_string());
    assert!(!raw.contains("\"fake\""), "撤销后不得残留记录: {raw}");
}

#[tokio::test]
async fn injection_guard_has_a_working_positive_exit() {
    // 正向出口：用户看完命中片段并显式启用后，工具必须真的可用——
    // 不允许出现「检测器拦住一切、却没有任何办法放行」的死局。
    let root = temp_root("exempt");
    let (runtime, _sink) = runtime_with_sink(&root);
    let review = runtime
        .connect_and_review(&config(false), &workspace())
        .await
        .unwrap();

    let resolved = runtime
        .resolve_trust(
            SERVER,
            &review.fingerprint,
            &[TrustDecisionInput {
                dispatch_name: "mcp__fake__hostile".into(),
                approved: true,
                risk: Some(RiskLevel::Destructive),
                enable_injection: true,
            }],
        )
        .await
        .expect("显式启用应成功");

    assert_eq!(resolved.status, McpStatus::Trusted);
    assert!(resolved
        .exemptions
        .iter()
        .any(|entry| entry.dispatch_name == "mcp__fake__hostile"));
    assert!(tool(&resolved, "mcp__fake__hostile").guard_exempted);
    assert_eq!(executor_names(&runtime), vec!["mcp__fake__hostile"]);

    let output = executor(&runtime, "mcp__fake__hostile")
        .execute(json!({}), &context())
        .await
        .expect("显式启用的注入工具必须可调用");
    assert!(output.success);

    // 撤销豁免立即生效：同一个执行器再见即失败。
    let in_flight = executor(&runtime, "mcp__fake__hostile");
    runtime
        .revoke_exemption(SERVER, "mcp__fake__hostile")
        .await
        .expect("撤销豁免应成功");
    let error = in_flight
        .execute(json!({}), &context())
        .await
        .expect_err("撤销豁免后必须不可调用");
    assert!(
        error.to_string().contains("授权已被撤销"),
        "错误应说明授权已撤销: {error}"
    );

    runtime.shutdown_all().await;
}

#[tokio::test]
async fn without_enable_injection_the_hit_tool_stays_blocked_but_explained() {
    // 反向：批准了注入工具但没勾「显式启用」→ 仍然不注册，且原因可解释。
    let root = temp_root("no-exempt");
    let (runtime, _sink) = runtime_with_sink(&root);
    let review = runtime
        .connect_and_review(&config(false), &workspace())
        .await
        .unwrap();

    let resolved = runtime
        .resolve_trust(
            SERVER,
            &review.fingerprint,
            &[decide(
                "mcp__fake__hostile",
                true,
                Some(RiskLevel::Destructive),
            )],
        )
        .await
        .expect("裁决应成功");

    assert_eq!(
        resolved.status,
        McpStatus::Blocked,
        "零个工具真正注册时应为 blocked"
    );
    assert!(runtime.executors().is_empty());
    assert!(!tool(&resolved, "mcp__fake__hostile").guard_exempted);
    assert!(!tool(&resolved, "mcp__fake__hostile").guard_hits.is_empty());
    assert!(resolved.exemptions.is_empty());

    runtime.shutdown_all().await;
}

#[tokio::test]
async fn trust_record_survives_restart_and_skips_review() {
    let root = temp_root("restart");
    let first = McpRuntime::new(&root);
    let review = first
        .connect_and_review(&config(false), &workspace())
        .await
        .unwrap();
    first
        .resolve_trust(
            SERVER,
            &review.fingerprint,
            &[decide("mcp__fake__echo", true, Some(RiskLevel::Read))],
        )
        .await
        .unwrap();
    first.shutdown_all().await;
    drop(first);

    // 同一配置根重启：指纹未变 → 直接 trusted，不再要求审查。
    let second = McpRuntime::new(&root);
    let reopened = second
        .connect_and_review(&config(false), &workspace())
        .await
        .unwrap();
    assert_eq!(reopened.status, McpStatus::Trusted);
    assert_eq!(reopened.fingerprint, review.fingerprint);
    assert!(tool(&reopened, "mcp__fake__echo").approved);
    assert_eq!(tool(&reopened, "mcp__fake__echo").risk, RiskLevel::Read);
    assert_eq!(executor_names(&second), vec!["mcp__fake__echo"]);

    // 授权可追溯到具体指纹（界面/审计要能看出「这条授权是给谁的」）。
    let statuses = second.statuses().await;
    let status = statuses
        .iter()
        .find(|item| item.server_id == SERVER)
        .expect("应有状态记录");
    assert_eq!(status.fingerprint, review.fingerprint);
    assert_eq!(status.status, McpStatus::Trusted);

    second.shutdown_all().await;
}

#[tokio::test]
async fn tool_manifest_change_forces_review_again() {
    let root = temp_root("manifest-change");
    let first = McpRuntime::new(&root);
    let review = first
        .connect_and_review(&config(false), &workspace())
        .await
        .unwrap();
    first
        .resolve_trust(
            SERVER,
            &review.fingerprint,
            &[decide("mcp__fake__echo", true, None)],
        )
        .await
        .unwrap();
    first.shutdown_all().await;
    drop(first);

    // server 多给一个工具：清单增删必须重批（不照 codex 的「什么变更都不重批」）。
    let second = McpRuntime::new(&root);
    let reopened = second
        .connect_and_review(&config(true), &workspace())
        .await
        .unwrap();
    assert_eq!(reopened.status, McpStatus::Review, "清单变化必须回到审查");
    assert_ne!(reopened.fingerprint, review.fingerprint);
    let reason = reopened.reason.as_deref().unwrap_or("");
    assert!(reason.contains("新增"), "原因应指出清单新增项: {reason}");
    assert!(
        reason.contains("mcp__fake__extra"),
        "原因应点名新增工具: {reason}"
    );
    assert!(second.executors().is_empty(), "重批前旧批准不得继续生效");

    second.shutdown_all().await;
}
