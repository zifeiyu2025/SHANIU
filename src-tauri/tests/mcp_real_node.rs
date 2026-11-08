//! 真机探针：第三方 **node 类** MCP server（`@modelcontextprotocol/server-filesystem`），
//! 验证我们与上游的差异点——**混合风险工具面可以逐工具定档**。
//!
//! 依赖装在仓库外的隔离目录（不进 `package.json` / lockfile）：
//! ```text
//! npm install --prefix D:\AI\mcp-node-test --no-save --no-package-lock @modelcontextprotocol/server-filesystem
//! ```
//! 运行：
//! `SHANIU_REAL_MCP_NODE=D:\Programs\nodejs\node.exe`
//! `cargo test --test mcp_real_node -- --ignored --nocapture`
//!
//! 覆盖两件事：
//! 1. **不同风险档位**：只读工具（list/read/search）给 `read`，写入工具（write/edit/move/create）
//!    给 `destructive`，批准记录里必须逐工具保留，不能一刀切；
//! 2. **未批准的工具不存在「弹审批」路径**：只批准只读工具后，写入工具**不注册**，
//!    按名分发直接得到「未知工具」，且全过程**没有一条审批请求事件**。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::json;
use shaniu_lib::config::{McpServerConfig, SandboxMode};
use shaniu_lib::events::{EventMsg, EventSink, RiskLevel};
use shaniu_lib::mcp_client::runtime::{McpRuntime, McpStatus, TrustDecisionInput};
use shaniu_lib::tool_router::{ToolContext, ToolRouter};
use tokio_util::sync::CancellationToken;

const SERVER: &str = "fsmcp";

/// 隔离安装的入口（不在仓库内，也不在 `node_modules` 依赖树里）。
fn server_entry() -> PathBuf {
    std::env::var("SHANIU_REAL_MCP_NODE_SERVER")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(
                r"D:\AI\mcp-node-test\node_modules\@modelcontextprotocol\server-filesystem\dist\index.js",
            )
        })
}

fn node() -> String {
    std::env::var("SHANIU_REAL_MCP_NODE").unwrap_or_else(|_| "node".to_string())
}

/// 记录事件类型，用来断言「没有审批请求」这类**不存在**的事实。
#[derive(Default)]
struct Recorder {
    approvals: Mutex<Vec<String>>,
}

impl EventSink for Recorder {
    fn emit(&self, msg: &EventMsg) -> Result<(), String> {
        if let EventMsg::ExecApprovalRequest { .. } = msg {
            self.approvals.lock().unwrap().push(format!("{msg:?}"));
        }
        Ok(())
    }
}

fn workspace(tag: &str) -> shaniu_lib::test_support::TestTempDir {
    let dir = shaniu_lib::test_support::TestTempDir::new(&format!("real-node-{tag}"));
    std::fs::write(dir.join("hello.txt"), "shaniu node probe\n").expect("写入示例文件");
    dir
}

fn config(workspace: &Path) -> McpServerConfig {
    McpServerConfig {
        id: SERVER.to_string(),
        command: node(),
        args: vec![
            server_entry().display().to_string(),
            workspace.display().to_string(),
        ],
        env: BTreeMap::new(),
        cwd: workspace.display().to_string(),
        ..McpServerConfig::default()
    }
}

fn context(root: &Path, sink: Arc<Recorder>) -> ToolContext {
    ToolContext::new(
        root,
        CancellationToken::new(),
        "call-real-node-mcp",
        sink,
        SandboxMode::WorkspaceWrite,
    )
}

/// 只读工具：列目录 / 读文件 / 搜索 / 元信息。
fn is_read_tool(raw: &str) -> bool {
    let name = raw.to_ascii_lowercase();
    name.starts_with("read_")
        || name.starts_with("list_")
        || name.starts_with("search_")
        || name.starts_with("get_")
        || name == "directory_tree"
}

/// 写入工具：写 / 追加 / 复制 / 编辑 / 建目录 / 移动。
fn is_write_tool(raw: &str) -> bool {
    let name = raw.to_ascii_lowercase();
    name.starts_with("write_")
        || name.starts_with("append_")
        || name.starts_with("copy_")
        || name.starts_with("edit_")
        || name.starts_with("create_")
        || name.starts_with("move_")
}

#[tokio::test]
#[ignore = "真机探针：需要仓库外安装的 server-filesystem"]
async fn real_node_filesystem_mixed_risk_and_denied_write() {
    if !server_entry().is_file() {
        eprintln!(
            "跳过：未找到 {}（按文件头注释安装到仓库外）",
            server_entry().display()
        );
        return;
    }

    // —— 场景 1：混合工具面逐工具定档 ——
    let root = workspace("mixed");
    let runtime = Arc::new(McpRuntime::new(&root));
    let review = runtime
        .connect_and_review(&config(&root), &root)
        .await
        .expect("node 类 server 在受限 env 下应能启动");

    eprintln!("=== 工具清单（{} 个）===", review.tools.len());
    for tool in &review.tools {
        eprintln!("- {} risk 待定 read={} write={}", tool.raw_name, is_read_tool(&tool.raw_name), is_write_tool(&tool.raw_name));
    }
    assert_eq!(review.status, McpStatus::Review, "首连必须是待审查");
    let reads: Vec<String> = review
        .tools
        .iter()
        .filter(|tool| is_read_tool(&tool.raw_name))
        .map(|tool| tool.dispatch_name.clone())
        .collect();
    let writes: Vec<String> = review
        .tools
        .iter()
        .filter(|tool| is_write_tool(&tool.raw_name))
        .map(|tool| tool.dispatch_name.clone())
        .collect();
    assert!(!reads.is_empty(), "filesystem server 必须提供只读工具");
    assert!(!writes.is_empty(), "filesystem server 必须提供写入工具（否则测不到混合风险面）");
    eprintln!("只读工具 {} 个，写入工具 {} 个", reads.len(), writes.len());

    let decisions: Vec<TrustDecisionInput> = review
        .tools
        .iter()
        .map(|tool| TrustDecisionInput {
            dispatch_name: tool.dispatch_name.clone(),
            approved: true,
            risk: Some(if is_read_tool(&tool.raw_name) {
                RiskLevel::Read
            } else {
                RiskLevel::Destructive
            }),
            enable_injection: false,
        })
        .collect();
    let resolved = runtime
        .resolve_trust(SERVER, &review.fingerprint, &decisions)
        .await
        .expect("全量批准应成功");
    assert_eq!(resolved.status, McpStatus::Trusted);

    // 批准记录必须逐工具保留档位：只读不是 destructive，写入不是 read。
    let status = runtime
        .statuses()
        .await
        .into_iter()
        .find(|status| status.server_id == SERVER)
        .expect("状态应存在");
    for tool in &status.approved_tools {
        let expected = if reads.contains(&tool.dispatch_name) {
            RiskLevel::Read
        } else {
            RiskLevel::Destructive
        };
        assert_eq!(tool.risk, expected, "{} 的风险档位不应被一刀切", tool.dispatch_name);
    }
    assert!(
        status.approved_tools.iter().any(|tool| tool.risk == RiskLevel::Read)
            && status.approved_tools.iter().any(|tool| tool.risk == RiskLevel::Destructive),
        "必须同时存在 read 与 destructive 两档，才证明逐工具定档生效"
    );

    // 执行器侧的声明同样分档：模型看到的风险与批准记录一致。
    let specs = runtime.executors().iter().map(|executor| executor.spec().clone()).collect::<Vec<_>>();
    for spec in &specs {
        let expected = if reads.contains(&spec.name) {
            RiskLevel::Read
        } else {
            RiskLevel::Destructive
        };
        assert_eq!(spec.risk, Some(expected), "{} 的执行器档位应与批准一致", spec.name);
    }
    runtime.shutdown_all().await;

    // —— 场景 2：只批准只读工具，写入工具「被拒」而不是「弹审批」 ——
    let root = workspace("readonly");
    let sink = Arc::new(Recorder::default());
    let runtime = Arc::new(McpRuntime::new(&root));
    let review = runtime
        .connect_and_review(&config(&root), &root)
        .await
        .expect("第二次连接应成功");
    let read_decisions: Vec<TrustDecisionInput> = review
        .tools
        .iter()
        .filter(|tool| is_read_tool(&tool.raw_name))
        .map(|tool| TrustDecisionInput {
            dispatch_name: tool.dispatch_name.clone(),
            approved: true,
            risk: Some(RiskLevel::Read),
            enable_injection: false,
        })
        .collect();
    assert!(!read_decisions.is_empty(), "至少要批准一个只读工具");
    runtime
        .resolve_trust(SERVER, &review.fingerprint, &read_decisions)
        .await
        .expect("只批准只读工具应成功");

    let executors = runtime.executors();
    let names: Vec<String> = executors.iter().map(|e| e.spec().name.clone()).collect();
    for write in &writes {
        assert!(!names.contains(write), "未批准的写入工具不得注册执行器: {write}");
    }

    // 按名分发写入工具：必须是「未知工具」，而不是进入审批流程。
    let mut router = ToolRouter::new();
    for executor in executors {
        router
            .try_register(executor)
            .expect("已批准的工具应能注册进路由");
    }
    let write_name = writes.first().expect("至少一个写入工具").clone();

    // 模型可见面 = `declarations()`，不是内部执行器表。未批准的工具必须**根本不在清单里**：
    // 若它出现在声明中，模型会反复尝试调用，用户看到的是「模型一直报错」，
    // 而不是「这个能力我没授权」——「不可见」和「可见但被拒」是两种体验。
    let declared: Vec<String> = router.declarations().into_iter().map(|d| d.name).collect();
    for write in &writes {
        assert!(
            !declared.contains(write),
            "未批准的写入工具不得出现在模型可见清单: {write}"
        );
    }
    eprintln!(
        "模型可见工具 {} 个（写入工具已被排除在声明之外）",
        declared.len()
    );

    let error = router
        .execute(&write_name, json!({"path": root.join("pwned.txt").display().to_string()}), &context(&root, sink.clone()))
        .await
        .expect_err("未批准的写入工具必须调用失败");
    eprintln!("未批准写入工具调用结果：{error}");
    assert!(
        error.to_string().contains("未知工具"),
        "未批准的工具应是「未知工具」而不是弹审批: {error}"
    );
    assert!(
        sink.approvals.lock().unwrap().is_empty(),
        "未批准的工具不得触发任何审批请求事件"
    );
    assert!(!root.join("pwned.txt").exists(), "写入工具不得落盘");

    // 已批准的只读工具照常可用，输出仍在不可信包裹内。
    let read_name = reads.first().expect("至少一个只读工具").clone();
    let reader = router
        .get(&read_name)
        .cloned()
        .unwrap_or_else(|| panic!("只读工具 {read_name} 应已注册"));
    let output = reader.execute(json!({}), &context(&root, sink.clone())).await;
    match output {
        Ok(output) => {
            eprintln!("只读工具 {read_name} 调用成功，{} 字节", output.output.len());
            assert!(output.output.contains("不可信工具输出"), "输出必须在不可信包裹内");
        }
        Err(error) => panic!("只读工具 {read_name} 调用失败: {error}"),
    }
    eprintln!(
        "审批请求事件数={}（实际计数，非断言字面量）",
        sink.approvals.lock().unwrap().len()
    );

    runtime.shutdown_all().await;
}
