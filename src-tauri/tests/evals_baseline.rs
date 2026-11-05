//! 评估基线 v0 运行器（docs/05 SH-028，docs/04 §6.5）。
//!
//! 这条用例是**唯一能客观回答"这次上下文工程变更有没有让 Agent 变笨"的东西**：
//! AGENTS.md §5 规定提示词/工具描述/错误回填/compaction 的变更必须附基线回归，
//! 没有它，那条规则只能靠感觉执行。
//!
//! 口径：
//! - 任务集与判据来自 `evals/tasks.json`（数据，不是代码；SH-073 扩集合不改 Rust）；
//! - 每个任务在**独立临时工作区**跑，fixture 由 tasks.json 提供，跑完即删——任务之间不互相喂答案；
//! - 跑的是**生产同一套件**：`tools::register_builtin` 的工具面、`prompt::system_instruction`
//!   的系统提示、`ApprovalBroker` 的四级门禁、workspace-write 真沙箱。桩一件都不是基线；
//! - 判据是机械的文件断言（存在 + 必含 + 必不含），不做"看着像对"的主观评分；
//! - 审批由 sink 就地代答 `AllowAlways`（等价于用户全程点"总是允许"），
//!   这样指标量的是 Agent 的任务能力，不是审批摩擦；代答次数一并入报告，异常升高说明
//!   模型开始频繁撞门禁。
//!
//! 运行（需要密钥，密钥只从环境变量读，永不入库/入日志）：
//! ```text
//! SHANIU_TEST_API_KEY=... cargo test --test evals_baseline -- --ignored --nocapture
//! ```
//! 默认端点为 deepseek-flash（非推理档，FF-5 实测 P95 1.13s，成本与抖动都最低）。
//! 产出 `evals/latest.json`，由 `evals/score.mjs` 与 `evals/baseline.json` 比对判退化。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::id;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::json;
use shaniu_lib::agent_loop::{AgentLoop, ResponsesBackend};
use shaniu_lib::config::{ApprovalMode, SandboxMode};
use shaniu_lib::events::{ApprovalDecision, EventMsg, EventSink};
use shaniu_lib::model_client::provider::ProviderConfig;
use shaniu_lib::permission::{policy, ApprovalBroker};
use shaniu_lib::tool_router::{PlanState, ToolRouter};
use shaniu_lib::tools;
use tokio_util::sync::CancellationToken;

const PER_TASK_TIMEOUT: Duration = Duration::from_secs(180);
const MAX_TURNS_PER_TASK: u32 = 8;
static SEQ: AtomicUsize = AtomicUsize::new(0);

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("src-tauri 的上一级是仓库根")
        .to_path_buf()
}

// ============================== 任务集 ==============================

#[derive(Deserialize)]
struct TasksFile {
    version: u32,
    fixture: BTreeMap<String, String>,
    tasks: Vec<Task>,
}

#[derive(Deserialize)]
struct Task {
    id: String,
    title: String,
    prompt: String,
    check: Check,
}

#[derive(Deserialize, Default)]
struct Check {
    #[serde(default)]
    path: String,
    #[serde(default)]
    contains_all: Vec<String>,
    #[serde(default)]
    absent_all: Vec<String>,
}

/// 判据求值：Ok(()) = 通过；Err 文本同时进报告和失败原因。
fn verify(ws: &Path, check: &Check) -> Result<(), String> {
    let target = ws.join(&check.path);
    let text = std::fs::read_to_string(&target)
        .map_err(|e| format!("未产出 {}（{e}）", check.path))?;
    for needle in &check.contains_all {
        if !text.contains(needle) {
            return Err(format!("{} 缺少必含内容 {needle:?}", check.path));
        }
    }
    for banned in &check.absent_all {
        if text.contains(banned) {
            return Err(format!("{} 仍含应被清除的内容 {banned:?}", check.path));
        }
    }
    Ok(())
}

fn seed_fixture(ws: &Path, fixture: &BTreeMap<String, String>) {
    std::fs::create_dir_all(ws).unwrap();
    for (rel, body) in fixture {
        let file = ws.join(rel);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&file, body).unwrap();
    }
}

// ============================== 事件采集 + 审批代答 ==============================

#[derive(Default)]
struct Agg {
    input_tokens: u64,
    output_tokens: u64,
    approvals: u32,
    auto_resolved: u32,
    unresolved: u32,
    tool_calls: u32,
    /// 错误原文（最多留 3 条，每条截 200 字符）：只给"失败"二字的话，
    /// 报告就退化成一个无法归因的数字，基线也就白跑了。
    errors: Vec<String>,
}

/// 生产语义的"总是允许"用户：请求一到就地代答。代答失败数一并记录——
/// 若 resolve 返回 false，说明请求 ID 不是后端签发的（FF-6 的失效形态），必须看得见。
struct EvalSink {
    agg: Mutex<Agg>,
    broker: Arc<ApprovalBroker>,
}

impl EventSink for EvalSink {
    fn emit(&self, msg: &EventMsg) -> Result<(), String> {
        let mut agg = self.agg.lock().unwrap();
        match msg {
            EventMsg::TokenUsage {
                input_tokens,
                output_tokens,
                ..
            } => {
                agg.input_tokens += u64::from(*input_tokens);
                agg.output_tokens += u64::from(*output_tokens);
            }
            EventMsg::ToolCallBegin { .. } => agg.tool_calls += 1,
            EventMsg::Error { message } => {
                let trimmed: String = message.chars().take(200).collect();
                if agg.errors.len() < 3 {
                    agg.errors.push(trimmed);
                }
            }
            EventMsg::ExecApprovalRequest { request_id, .. } => {
                agg.approvals += 1;
                if self.broker.resolve(request_id, ApprovalDecision::AllowAlways) {
                    agg.auto_resolved += 1;
                } else {
                    agg.unresolved += 1;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

// ============================== 单任务执行 ==============================

struct Outcome {
    success: bool,
    detail: String,
    turns: u32,
    input_tokens: u64,
    output_tokens: u64,
    tool_calls: u32,
    approvals: u32,
    unresolved: u32,
    ms: u128,
}

async fn run_one(task: &Task, fixture: &BTreeMap<String, String>, backend: Arc<ResponsesBackend>) -> Outcome {
    let ws = repo_root().join("target").join(format!(
        "evals-{}-{}-{}",
        task.id,
        id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    seed_fixture(&ws, fixture);

    let plan_state = Arc::new(PlanState::new());
    let mut router = ToolRouter::new();
    tools::register_builtin(&mut router, plan_state.clone());
    let broker = Arc::new(ApprovalBroker::new());
    let sink = Arc::new(EvalSink {
        agg: Mutex::new(Agg::default()),
        broker: broker.clone(),
    });
    let agent = AgentLoop {
        backend,
        router: Arc::new(router),
        broker: broker.clone(),
        plan_state,
        workspace_root: ws.clone(),
        max_turns: MAX_TURNS_PER_TASK,
        policy: policy::SessionPolicy {
            approval: ApprovalMode::AutoReadOnly,
            sandbox: SandboxMode::WorkspaceWrite,
        },
    };

    let started = Instant::now();
    let summary = tokio::time::timeout(
        PER_TASK_TIMEOUT,
        agent.run_task(&task.prompt, sink.clone(), CancellationToken::new()),
    )
    .await;
    let ms = started.elapsed().as_millis();

    let (turns, reason) = match &summary {
        Ok(s) => (s.turns, format!("{:?}", s.reason)),
        Err(_) => (MAX_TURNS_PER_TASK, "超时被丢弃（>180s）".to_string()),
    };
    let agg = sink.agg.lock().unwrap();
    let verdict = verify(&ws, &task.check);
    let mut detail = match &verdict {
        Ok(()) => reason,
        Err(why) => format!("{why}｜轮次收口={reason}"),
    };
    if !agg.errors.is_empty() {
        detail.push_str(&format!("｜错误=[{}]", agg.errors.join(" / ")));
    }
    let out = Outcome {
        success: verdict.is_ok(),
        detail,
        turns,
        input_tokens: agg.input_tokens,
        output_tokens: agg.output_tokens,
        tool_calls: agg.tool_calls,
        approvals: agg.approvals,
        unresolved: agg.unresolved,
        ms,
    };
    std::fs::remove_dir_all(&ws).ok();
    out
}

// ============================== 入口 ==============================

#[tokio::test]
#[ignore = "SH-028 评估基线需要真实端点密钥与外网，按里程碑/变更回归手动或定时运行"]
async fn evals_baseline_v0() {
    let api_key = std::env::var("SHANIU_TEST_API_KEY")
        .expect("评估基线必须配真实端点：设置 SHANIU_TEST_API_KEY（勿把密钥写进仓库）");
    let provider = ProviderConfig {
        provider: std::env::var("SHANIU_TEST_PROVIDER").unwrap_or_else(|_| "openai-chat".into()),
        base_url: std::env::var("SHANIU_TEST_BASE_URL")
            .unwrap_or_else(|_| "https://api.deepseek.com/v1".into()),
        model: std::env::var("SHANIU_TEST_MODEL").unwrap_or_else(|_| "deepseek-flash".into()),
    };
    let backend = Arc::new(ResponsesBackend { provider: provider.clone(), api_key });

    let raw = std::fs::read_to_string(repo_root().join("evals").join("tasks.json"))
        .expect("evals/tasks.json 必须存在");
    let spec: TasksFile = serde_json::from_str(&raw).expect("evals/tasks.json 解析失败");

    let mut rows = Vec::new();
    let mut passed = 0usize;
    for task in &spec.tasks {
        let out = run_one(task, &spec.fixture, backend.clone()).await;
        if out.success {
            passed += 1;
        }
        println!(
            "[evals] {:<20} {} turns={} calls={} tok={}/{} appr={} 耗时={}ms {}",
            task.id,
            if out.success { "PASS" } else { "FAIL" },
            out.turns,
            out.tool_calls,
            out.input_tokens,
            out.output_tokens,
            out.approvals,
            out.ms,
            out.detail,
        );
        rows.push(json!({
            "id": task.id,
            "title": task.title,
            "success": out.success,
            "turns": out.turns,
            "tool_calls": out.tool_calls,
            "input_tokens": out.input_tokens,
            "output_tokens": out.output_tokens,
            "approvals": out.approvals,
            "unresolved_approvals": out.unresolved,
            "ms": out.ms,
            "detail": out.detail,
        }));
    }

    let n = spec.tasks.len() as f64;
    let sum_turns: u32 = rows.iter().filter_map(|r| r["turns"].as_u64()).map(|t| t as u32).sum();
    let sum_tokens: u64 = rows
        .iter()
        .map(|r| r["input_tokens"].as_u64().unwrap_or(0) + r["output_tokens"].as_u64().unwrap_or(0))
        .sum();
    let report = json!({
        "version": spec.version,
        "epoch_ms": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis(),
        "provider": provider.provider,
        "base_url": provider.base_url,
        "model": provider.model,
        "max_turns_per_task": MAX_TURNS_PER_TASK,
        "summary": {
            "n": spec.tasks.len(),
            "passed": passed,
            "success_rate": passed as f64 / n,
            "avg_turns": f64::from(sum_turns) / n,
            "avg_total_tokens": sum_tokens as f64 / n,
        },
        "tasks": rows,
    });
    let out_path = repo_root().join("evals").join("latest.json");
    std::fs::write(&out_path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    println!(
        "[evals] 基线报告已写入 {}：成功率={}/{}，平均轮数={:.2}，平均 token={:.0}",
        out_path.display(),
        passed,
        spec.tasks.len(),
        f64::from(sum_turns) / n,
        sum_tokens as f64 / n,
    );

    // 运行器自身的底线：全 0 分通常是环境坏了（端点/密钥/沙箱），不是"模型笨"，
    // 必须让跑的人立刻分辨这两种情况，否则基线报告会被当成可信数据用。
    assert!(
        passed > 0,
        "5 个任务全数失败：先怀疑环境（端点可达性/沙箱创建/审批代答），再怀疑模型能力"
    );
}
