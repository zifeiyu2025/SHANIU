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
//! **默认跑 5 次**（`SHANIU_EVAL_REPEATS` 可覆盖），报告里同时保留每次运行的原始数字
//! 与中位数；单次运行会被端点漂移支配（实测整体上抬 32%/轮），不足以判红绿。
//! 产出 `evals/latest.json`，由 `evals/score.mjs` 与 `evals/baseline.<model>.json` 比对判退化。

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
    /// 工具调用轨迹（begin → end 配对，上限 10 条）：SH-085 这类"跑完但没产出"
    /// 的失败，只有看到模型实际发了什么才有归因，否则只能猜提示词。
    trace: Vec<String>,
}

/// 参数摘要：够认出模型发了什么，又不至于把报告撑爆。
fn args_summary(args: &serde_json::Value) -> String {
    let text = args.to_string();
    let text = text.chars().take(90).collect::<String>();
    if text.chars().count() < args.to_string().chars().count() {
        format!("{text}…")
    } else {
        text
    }
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
            EventMsg::ToolCallBegin { tool, args, .. } => {
                agg.tool_calls += 1;
                if agg.trace.len() < 10 {
                    agg.trace.push(format!("{tool} {}", args_summary(args)));
                }
            }
            EventMsg::ToolCallEnd { result, error, .. } => {
                if let Some(last) = agg.trace.last_mut() {
                    let outcome = match (result, error) {
                        (Some(r), None) => format!("→ {r:?}"),
                        (Some(r), Some(e)) => {
                            format!("→ {r:?} {}", e.chars().take(80).collect::<String>())
                        }
                        (None, Some(e)) => format!("→ 无终态 {e}"),
                        (None, None) => "→ 无终态信息".to_string(),
                    };
                    last.push_str(&outcome);
                }
            }
            EventMsg::ToolCallOutputDelta { delta, .. } => {
                // 输出正文是归因的关键：只看命令会误判成"模型没写文件"，
                // 实际可能是它看到的 cwd/结果与预期不符。每条轨迹限 240 字符。
                if let Some(last) = agg.trace.last_mut() {
                    if last.matches("｜输出=").count() < 1 {
                        let head: String = delta.chars().take(240).collect();
                        last.push_str(&format!("｜输出={head:?}"));
                    }
                }
            }
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
    trace: Vec<String>,
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
    // 顺带验证持久化链路（SH-030）：真实端点跑完一轮，档案必须落盘且能原样读回。
    // 放在评估里做是因为它跑的是生产同一套 AgentLoop 装配——单测造得出历史，造不出真实时序。
    let rollout_dir = repo_root().join("target").join("evals-rollouts");
    std::fs::create_dir_all(&rollout_dir).unwrap();
    std::fs::remove_file(rollout_dir.join(format!("{}.jsonl", task.id))).ok();
    let writer = Arc::new(
        shaniu_lib::rollout::RolloutWriter::create(
            &rollout_dir,
            &shaniu_lib::rollout::RolloutItem::SessionMeta {
                session_id: task.id.clone(),
                agent_id: task.id.clone(),
                parent_id: None,
                created_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or_default(),
                config_snapshot: shaniu_lib::rollout::ConfigSnapshot {
                    model: "evals".into(),
                    sandbox: "WorkspaceWrite".into(),
                    approval: "AutoReadOnly".into(),
                    workspace_root: ws.display().to_string(),
                    ..Default::default()
                },
            },
        )
        .expect("评估档案应可创建（同名旧档已先删除）"),
    );
    let agent = AgentLoop {
        backend,
        router: Arc::new(router),
        broker: broker.clone(),
        plan_state,
        workspace_root: ws.clone(),
        max_turns: MAX_TURNS_PER_TASK,
        // 压缩默认关闭，由 `SHANIU_EVAL_TOKEN_BUDGET` 显式打开。
        // 必须能分别跑"禁用/启用"两档：SH-032 的验收是"长会话不丢上下文"，
        // 而基线退化判定要能归因到压缩本身——混在一起就分不清是端点波动还是压缩代价。
        token_budget: std::env::var("SHANIU_EVAL_TOKEN_BUDGET")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        tree_budget: Default::default(),
        policy: policy::SessionPolicy {
            approval: ApprovalMode::AutoReadOnly,
            sandbox: SandboxMode::WorkspaceWrite,
        },
        rollout: Some(writer),
        rollout_broke: std::sync::atomic::AtomicBool::new(false),
        session_id: None,
        jobs: None,
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
        trace: agg.trace.clone(),
        turns,
        input_tokens: agg.input_tokens,
        output_tokens: agg.output_tokens,
        tool_calls: agg.tool_calls,
        approvals: agg.approvals,
        unresolved: agg.unresolved,
        ms,
    };
    // 档案契约（SH-030 的真机一侧）：真实端点跑完必须留下可读回的档案。
    // 断言放在这里而不是另开单测——单测造得出历史，造不出真实端点的时序与取消路径。
    let archive = shaniu_lib::rollout::read(&rollout_dir.join(format!("{}.jsonl", task.id)))
        .unwrap_or_else(|e| panic!("任务 {} 的档案读回失败：{e}", task.id));
    assert!(
        archive.items.len() >= 2,
        "档案至少要有用户任务与一条助手输出，实际 {} 条：{:?}",
        archive.items.len(),
        archive.items
    );
    assert_eq!(archive.torn_tail_lines, 0, "正常收口不该有半截写残留");
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
        reasoning_effort: None,
            headers: Default::default(),
    };
    let backend = Arc::new(ResponsesBackend { provider: provider.clone(), api_key });

    let raw = std::fs::read_to_string(repo_root().join("evals").join("tasks.json"))
        .expect("evals/tasks.json 必须存在");
    let spec: TasksFile = serde_json::from_str(&raw).expect("evals/tasks.json 解析失败");
    // 单任务调试用（SHANIU_EVAL_ONLY=create-note）：全量一轮要几分钟与真实额度，
    // 归因时只需要重跑失败那一个
    let only = std::env::var("SHANIU_EVAL_ONLY").unwrap_or_default();
    let selected: Vec<&Task> = spec
        .tasks
        .iter()
        .filter(|t| only.is_empty() || t.id == only)
        .collect();
    if selected.is_empty() {
        panic!("SHANIU_EVAL_ONLY={only} 在 tasks.json 里没有对应任务 id");
    }

    // 单次运行不足以判定退化：端点抖动能轻易造成 30% 级别的 token 偏移（实测 32%/轮）。
    // 门禁口径因此改为「默认 5 次，取中位数」，并保留每一次的数字（含变红的那些）。
    let repeats = std::env::var("SHANIU_EVAL_REPEATS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(5);
    if repeats < 5 && only.is_empty() {
        println!("[evals] 警告：SHANIU_EVAL_REPEATS={repeats} < 5，样本不足，score.mjs 会拒绝下结论");
    }

    let mut runs = Vec::new();
    let mut first_rows: Option<Vec<serde_json::Value>> = None;
    for run in 1..=repeats {
        let mut rows = Vec::new();
        let mut passed = 0usize;
        let count = selected.len();
        for task in &selected {
            let out = run_one(task, &spec.fixture, backend.clone()).await;
            if out.success {
                passed += 1;
            }
            println!(
                "[evals] run {run}/{repeats} {:<20} {} turns={} calls={} tok={}/{} appr={} 耗时={}ms {}",
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
            if !out.success {
                // 失败必带轨迹：没有"模型实际发了什么"，归因只能靠猜
                for line in &out.trace {
                    println!("    · {line}");
                }
            }
            rows.push(json!({
                "id": task.id,
                "title": task.title,
                "success": out.success,
                "turns": out.turns,
                "tool_calls": out.tool_calls,
                "trace": out.trace,
                "input_tokens": out.input_tokens,
                "output_tokens": out.output_tokens,
                "approvals": out.approvals,
                "unresolved_approvals": out.unresolved,
                "ms": out.ms,
                "detail": out.detail,
            }));
        }

        let n = count as f64;
        let sum_turns: u32 = rows.iter().filter_map(|r| r["turns"].as_u64()).map(|t| t as u32).sum();
        let sum_tokens: u64 = rows
            .iter()
            .map(|r| r["input_tokens"].as_u64().unwrap_or(0) + r["output_tokens"].as_u64().unwrap_or(0))
            .sum();
        println!(
            "[evals] run {run}/{repeats} 小计 成功率={passed}/{count}，平均轮数={:.2}，平均 token={:.0}",
            f64::from(sum_turns) / n,
            sum_tokens as f64 / n,
        );
        if first_rows.is_none() {
            first_rows = Some(rows.clone());
        }
        runs.push(json!({
            "run": run,
            "summary": {
                "n": count,
                "passed": passed,
                "success_rate": passed as f64 / n,
                "avg_turns": f64::from(sum_turns) / n,
                "avg_total_tokens": sum_tokens as f64 / n,
            },
            "tasks": rows,
        }));
    }

    // 汇总口径：三项指标都取多次运行的中位数。单次极值不参与判定，
    // 但每一次的原始数字都留在 `runs` 里（含变红的那些），禁止只保留最好的一次。
    let metric = |key: &str| -> Vec<f64> {
        runs.iter()
            .map(|run| run["summary"][key].as_f64().unwrap_or(0.0))
            .collect()
    };
    let success_rates = metric("success_rate");
    let turns = metric("avg_turns");
    let tokens = metric("avg_total_tokens");
    let passed_values: Vec<f64> = runs
        .iter()
        .map(|run| run["summary"]["passed"].as_f64().unwrap_or(0.0))
        .collect();
    let median_success = median(&success_rates);
    // 判定只在「同日、同端点、同参数」的两臂之间做：判定的分母是**当日对照**，
    // 不是冻结基线。冻结基线会被端点漂移整体抬升（实测 32%/轮），拿它当分母结论永远红。
    let ablated = std::env::var("SHANIU_EVAL_ABLATE_AR6")
        .ok()
        .is_some_and(|value| value.trim() == "1");
    let role = std::env::var("SHANIU_EVAL_ROLE").unwrap_or_else(|_| "treatment".to_string());
    let arm = if ablated { "ar6_off" } else { "ar6_on" };
    let report = json!({
        "version": spec.version,
        "arm": arm,
        "role": role,
        "epoch_ms": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis(),
        "provider": provider.provider,
        "base_url": provider.base_url,
        "model": provider.model,
        "max_turns_per_task": MAX_TURNS_PER_TASK,
        "repeats": repeats,
        "aggregate": "median-of-runs",
        "summary": {
            "n": selected.len(),
            "passed": median(&passed_values) as u32,
            "success_rate": median_success,
            "avg_turns": median(&turns),
            "avg_total_tokens": median(&tokens),
            // 区间一并留档：中位数相同但离散度暴涨，同样是退化信号。
            "success_rate_range": [min(&success_rates), max(&success_rates)],
            "avg_turns_range": [min(&turns), max(&turns)],
            "avg_total_tokens_range": [min(&tokens), max(&tokens)],
        },
        "runs": runs,
        // 逐任务差异对比用第一次运行；完整数据在 runs 里。
        "tasks": first_rows.unwrap_or_default(),
    });
    println!(
        "[evals] 汇总（{repeats} 次中位数）成功率={:.0}%（区间 {:.0}%–{:.0}%），平均轮数={:.2}（{:.2}–{:.2}），平均 token={:.0}（{:.0}–{:.0}）",
        median_success * 100.0,
        min(&success_rates) * 100.0,
        max(&success_rates) * 100.0,
        median(&turns),
        min(&turns),
        max(&turns),
        median(&tokens),
        min(&tokens),
        max(&tokens),
    );
    if only.is_empty() {
        let file = if role == "control" { "control.json" } else { "latest.json" };
        let out_path = repo_root().join("evals").join(file);
        std::fs::write(&out_path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
        println!("[evals] 报告已写入 {}（arm={arm} role={role}）", out_path.display());
    } else {
        // 单任务调试绝不能覆写 latest.json：否则一次调试跑就把基线门禁的参照物换掉
        println!("[evals] 调试模式（SHANIU_EVAL_ONLY={only}）：不写 latest.json，不影响门禁");
    }

    // 运行器自身的底线：全 0 分通常是环境坏了（端点/密钥/沙箱），不是"模型笨"，
    // 必须让跑的人立刻分辨这两种情况，否则基线报告会被当成可信数据用。
    assert!(
        median(&passed_values) > 0.0,
        "多次运行的中位数仍是 0 分：先怀疑环境（端点可达性/沙箱创建/审批代答），再怀疑模型能力"
    );
}

/// 中位数（1 个样本时就是它自己）。偶数个样本取中间两个的均值。
fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    }
}

fn min(values: &[f64]) -> f64 {
    values.iter().copied().fold(f64::INFINITY, f64::min)
}

fn max(values: &[f64]) -> f64 {
    values.iter().copied().fold(f64::NEG_INFINITY, f64::max)
}
