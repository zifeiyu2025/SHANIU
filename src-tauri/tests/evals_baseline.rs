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
//!
//! **并发隔离（2026-11-09 事故后重做）**：曾发生多进程同时写 `evals/latest.json`，
//! 磁盘结果与进程自报对不上、并发下 repair 任务全灭。现在：
//!  - 一进程一份结果文件 `evals/results/<epoch_ms>-<pid>-<role>.json`，不写共享单文件；
//!  - `evals/.eval.lock` 是 OS 级独占锁，第二个进程直接 panic 说明原因；
//!  - 工作区（`target/evals-<task>-<pid>-<seq>`）与档案目录（`target/evals-rollouts-<pid>`）
//!    都带 pid，两臂不可能共享可写状态；
//!  - 报告自带 pid / 提交号 / 端点 / 模型，并校验「样本数 == 实际档案数」，不一致标 `valid:false`。
//!
//! 对比用 `node evals/score.mjs <实验.json> <对照.json>`（显式传路径，不读"最新"）。

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
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

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时钟早于 1970")
        .as_millis()
}

fn short_commit() -> String {
    let root = repo_root();
    std::process::Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_else(|| "-".to_string())
}

// ============================== 运行隔离 ==============================
//
// 事故（2026-11-09）：多个评估进程同时写 `evals/latest.json`，磁盘上的结果与进程自报
// 不一致（100% / 17,216 vs 83% / 21,201），并发下 repair 任务 5 次全失败。结论：
// **评估结果一旦会被静默覆盖，所有基于 eval 的判断都不可信**。因此这里加三道闸：
//  1. OS 级独占锁：第二个进程根本起不来，而不是"起来了但结果被覆盖"；
//  2. 一进程一份结果文件，文件名带时间戳与 pid，谁也不会覆盖谁；
//  3. 工作区与档案目录都按 pid 隔离，两臂之间不存在共享可写状态。

/// 独占锁。持有期间别的进程 `acquire` 会 panic（带持有者信息）。
/// 锁随文件句柄释放，进程崩溃也不会留下永久死锁。
struct EvalRunLock {
    _file: File,
}

impl EvalRunLock {
    fn acquire() -> Self {
        let path = repo_root().join("evals").join(".eval.lock");
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        // `truncate(false)`：锁文件在抢到锁之后才写入持有者信息，抢锁前不能清空
        // （否则后到的进程可能把「正在跑」的持有者信息抹掉）。显式写出来也给 clippy 看。
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .unwrap_or_else(|error| panic!("打不开评估锁 {}：{error}", path.display()));
        if let Err(error) = file.try_lock() {
            // 持有者信息是抢到锁之后才写的，极端情况下会读到"刚抢到、还没落盘"的空文件。
            let mut owner = std::fs::read_to_string(&path).unwrap_or_default();
            if owner.trim().is_empty() {
                std::thread::sleep(Duration::from_millis(50));
                owner = std::fs::read_to_string(&path).unwrap_or_default();
            }
            let owner = owner.trim().to_string();
            panic!(
                "已有评估进程在跑，拒绝并行启动（{error}）。\n\
锁：{}\n\
持有者：{}\n\
并行会互相覆盖结果与工作区，结论不可用。等它结束再跑；\
确认没有进程在跑时，删除该锁文件即可（锁本身随进程退出自动释放）。",
                path.display(),
                if owner.is_empty() { "（未记录）" } else { owner.as_str() },
            );
        }
        let info = format!(
            "pid={} started_ms={} role={} endpoint={} model={}\n",
            id(),
            now_ms(),
            std::env::var("SHANIU_EVAL_ROLE").unwrap_or_else(|_| "treatment".to_string()),
            std::env::var("SHANIU_TEST_BASE_URL").unwrap_or_default(),
            std::env::var("SHANIU_TEST_MODEL").unwrap_or_default(),
        );
        file.set_len(0).ok();
        file.write_all(info.as_bytes()).ok();
        file.flush().ok();
        Self { _file: file }
    }
}

/// 一进程一份结果文件：`evals/results/<epoch_ms>-<pid>-<role>.json`。
fn result_path(role: &str) -> PathBuf {
    let dir = repo_root().join("evals").join("results");
    std::fs::create_dir_all(&dir).expect("创建 evals/results");
    dir.join(format!("{}-{}-{role}.json", now_ms(), id()))
}

/// 本进程独占的档案目录：评估要断言"真实端点跑完留下可读档案"，共享目录会被
/// 并发进程删改，断言就失去意义。
fn rollout_dir() -> PathBuf {
    let dir = repo_root().join("target").join(format!("evals-rollouts-{}", id()));
    // 起点清空：样本校验靠"数 jsonl 文件"，残留会让校验永远对不上。
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("创建评估档案目录");
    dir
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
    /// 任务级 fixture 覆盖层：与全局 fixture 合并，同名键以任务级为准。
    /// 失败路径类任务需要自己的工程骨架，不能塞进全局 fixture 去干扰其它任务。
    #[serde(default)]
    fixture: BTreeMap<String, String>,
}

#[derive(Deserialize, Default)]
struct Check {
    #[serde(default)]
    path: String,
    #[serde(default)]
    contains_all: Vec<String>,
    #[serde(default)]
    absent_all: Vec<String>,
    /// 命令式判据：在工作区里执行（`cmd /C`），退出码 0 视为通过。
    /// 「跑通测试」这类目标没法用文件内容断言，只能真的跑一次。
    #[serde(default)]
    command: Option<String>,
    /// 命令通过时还必须出现的输出片段（stdout + stderr 合并匹配）。
    #[serde(default)]
    expect_contains: Vec<String>,
}

/// 判据求值：Ok(()) = 通过；Err 文本同时进报告和失败原因。
fn verify(ws: &Path, check: &Check) -> Result<(), String> {
    if let Some(command) = &check.command {
        let output = std::process::Command::new("cmd")
            .arg("/C")
            .arg(command)
            .current_dir(ws)
            .output()
            .map_err(|e| format!("判据命令无法启动（{e}）"))?;
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if !output.status.success() {
            let tail: String = combined.chars().rev().take(400).collect::<String>()
                .chars().rev().collect();
            return Err(format!("判据命令 `{command}` 退出码非 0：{tail}"));
        }
        for needle in &check.expect_contains {
            if !combined.contains(needle) {
                return Err(format!("判据命令 `{command}` 输出缺少 {needle:?}"));
            }
        }
    }
    if check.path.is_empty() {
        return Ok(());
    }
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
    /// 每一次审批打断的归因（工具 + 风险档 + 原因，上限 12 条）：SH-119 要求报告
    /// 「每次打断的工具、原因、是否本可降噪」，只给总数无法回答"该不该降噪"。
    prompts: Vec<String>,
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
    /// 代答用的决定：默认 `AllowAlways`（等价"总是允许"，量任务能力）；
    /// `SHANIU_EVAL_APPROVAL=allow_once` 切到 `Allow`，模拟真实用户逐次确认，
    /// 用于 SH-119 的打断次数测量——用 AllowAlways 会把"记忆"混进指标，量不准。
    decision: ApprovalDecision,
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
            EventMsg::ExecApprovalRequest { request_id, tool, risk, reason, args, .. } => {
                agg.approvals += 1;
                if agg.prompts.len() < 12 {
                    agg.prompts.push(format!(
                        "{tool} risk={risk:?} 原因={} 参数={}",
                        reason.as_deref().unwrap_or("-"),
                        args_summary(args)
                    ));
                }
                if self.broker.resolve(request_id, self.decision) {
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
    prompts: Vec<String>,
    turns: u32,
    input_tokens: u64,
    output_tokens: u64,
    tool_calls: u32,
    approvals: u32,
    unresolved: u32,
    ms: u128,
}

/// 评估用的 broker：默认装**生产语义**（同 turn 批量确认 + 前缀白名单都在），
/// 两个 `SHANIU_EVAL_ABLATE_*` 开关只用于 SH-119 的前后对照——关掉它们就复现
/// "降噪前"的逐次打断。两处用例（任务集 + repair）共用同一套装配，避免漂移。
fn eval_broker() -> Arc<ApprovalBroker> {
    let broker = Arc::new(ApprovalBroker::new());
    if std::env::var("SHANIU_EVAL_ABLATE_BATCH").is_ok() {
        broker.disable_batch();
    }
    if std::env::var("SHANIU_EVAL_ABLATE_PREFIX").is_err() {
        let (rules, _) = policy::PrefixAllowlist::from_config(
            &shaniu_lib::config::default_allow_prefixes(),
        );
        broker.set_prefix_rules(rules);
    }
    broker
}

/// 代答决定：默认 `AllowAlways`（量任务能力）；`allow_once` 模拟真实用户逐次确认。
fn eval_decision() -> ApprovalDecision {
    if std::env::var("SHANIU_EVAL_APPROVAL").as_deref() == Ok("allow_once") {
        ApprovalDecision::Allow
    } else {
        ApprovalDecision::AllowAlways
    }
}

async fn run_one(
    task: &Task,
    fixture: &BTreeMap<String, String>,
    backend: Arc<ResponsesBackend>,
    rollout_dir: &Path,
) -> Outcome {
    let seq = SEQ.fetch_add(1, Ordering::SeqCst);
    // 会话与工作区都带本次序号：档案目录按 pid 独占，文件名按任务+序号唯一，
    // 这样"跑了几个样本"可以直接数档案文件，不会互相覆盖。
    let session_id = format!("{}-{seq}", task.id);
    let ws = repo_root()
        .join("target")
        .join(format!("evals-{}-{}-{}", task.id, id(), seq));
    seed_fixture(&ws, fixture);
    // 任务级覆盖层：同名键后写覆盖前写。
    seed_fixture(&ws, &task.fixture);

    let plan_state = Arc::new(PlanState::new());
    let mut router = ToolRouter::new();
    tools::register_builtin(&mut router, plan_state.clone(), true);
    let broker = eval_broker();
    let decision = eval_decision();
    let sink = Arc::new(EvalSink {
        agg: Mutex::new(Agg::default()),
        broker: broker.clone(),
        decision,
    });
    // 顺带验证持久化链路（SH-030）：真实端点跑完一轮，档案必须落盘且能原样读回。
    // 放在评估里做是因为它跑的是生产同一套 AgentLoop 装配——单测造得出历史，造不出真实时序。
    let writer = Arc::new(
        shaniu_lib::rollout::RolloutWriter::create(
            rollout_dir,
            &shaniu_lib::rollout::RolloutItem::SessionMeta {
                session_id: session_id.clone(),
                agent_id: session_id.clone(),
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
        policy: Arc::new(policy::SessionPolicyHandle::new(policy::SessionPolicy {
            approval: ApprovalMode::OnRequest,
            sandbox: SandboxMode::WorkspaceWrite,
        })),
        rollout: Some(writer),
        rollout_broke: std::sync::atomic::AtomicBool::new(false),
        session_id: None,
        jobs: None,
        stop_hook: None,
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
        prompts: agg.prompts.clone(),
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
    let archive = shaniu_lib::rollout::read(&rollout_dir.join(format!("{session_id}.jsonl")))
        .unwrap_or_else(|e| panic!("任务 {session_id} 的档案读回失败：{e}"));
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
    // 第一件事就是抢锁：没有锁，后面的任何数字都可能是别人的。
    let _lock = EvalRunLock::acquire();
    let started_ms = now_ms();
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
    let rollout_dir = rollout_dir();
    for run in 1..=repeats {
        let mut rows = Vec::new();
        let mut passed = 0usize;
        let count = selected.len();
        for task in &selected {
            let out = run_one(task, &spec.fixture, backend.clone(), &rollout_dir).await;
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
                "prompts": out.prompts,
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
            "[evals] run {run}/{repeats} 小计 成功率={passed}/{count}，平均轮数={:.2}，平均 token={:.0}，平均打断={:.2}",
            f64::from(sum_turns) / n,
            sum_tokens as f64 / n,
            if count == 0 {
                0.0
            } else {
                rows.iter().filter_map(|r| r["approvals"].as_u64()).sum::<u64>() as f64 / n
            },
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
                // SH-119：每任务打断次数（审批卡数），中位数口径由 score.mjs/人工比对
                "avg_approvals": if count == 0 {
                    0.0
                } else {
                    rows.iter().filter_map(|r| r["approvals"].as_u64()).sum::<u64>() as f64 / n
                },
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
    // SH-125 成本测量的对照臂：关掉失败自愈闭环，其余参数完全一致。
    let repair_ablated = std::env::var("SHANIU_EVAL_ABLATE_REPAIR")
        .ok()
        .is_some_and(|value| value.trim() == "1");
    let role = std::env::var("SHANIU_EVAL_ROLE").unwrap_or_else(|_| "treatment".to_string());
    let arm = match (ablated, repair_ablated) {
        (true, _) => "ar6_off",
        (false, true) => "repair_off",
        (false, false) => "ar6_on",
    };
    let expected_samples = repeats * selected.len();
    let actual_sessions = std::fs::read_dir(&rollout_dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
                .count()
        })
        .unwrap_or(0);
    let samples_match = expected_samples == actual_sessions;
    let report = json!({
        "version": spec.version,
        "arm": arm,
        "repair_enabled": !repair_ablated,
        "role": role,
        "epoch_ms": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis(),
        "provider": provider.provider,
        "base_url": provider.base_url,
        "model": provider.model,
        "max_turns_per_task": MAX_TURNS_PER_TASK,
        "repeats": repeats,
        "aggregate": "median-of-runs",
        // 来源自证：谁跑的、什么时候、哪份提交、写到了哪个文件。
        // 没有这四项，"磁盘上的数字是谁的"这个问题在事后无法回答。
        "process": {
            "pid": id(),
            "commit": short_commit(),
            "started_ms": started_ms,
            "finished_ms": now_ms(),
            "rollout_dir": rollout_dir.display().to_string(),
        },
        // 样本数 == 实际档案数，是"这份报告确实跑满了"的唯一硬证据；
        // 对不上说明有样本丢失或被别的进程删改，整份报告标无效。
        "valid": samples_match,
        "samples": {
            "expected": expected_samples,
            "actual_sessions": actual_sessions,
        },
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
    println!(
        "[evals] 样本校验：期望 {expected_samples} 份档案，实际 {actual_sessions} 份 → {}",
        if samples_match { "一致" } else { "不一致（报告已标记 valid:false，禁止据此下结论）" }
    );
    // 一进程一份，文件名带时间戳与 pid：不存在"谁覆盖谁"，也不再写 latest.json。
    let out_path = result_path(&role);
    std::fs::write(&out_path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    println!(
        "[evals] 报告已写入 {}（arm={arm} role={role} pid={} commit={}）",
        out_path.display(),
        id(),
        short_commit()
    );
    println!(
        "[evals] 对比：node evals/score.mjs {} <对照臂.json>",
        out_path.display()
    );
    if !only.is_empty() {
        // 单任务调试也写自己那份：调试结果混进门禁是无意义的，但也不该丢证据。
        println!("[evals] 调试模式（SHANIU_EVAL_ONLY={only}）：本份仅供定位，不要当作回归结论");
    }
    assert!(
        samples_match,
        "样本数与实际档案数不一致（期望 {expected_samples}，实际 {actual_sessions}）：\
这份报告不可信，先查是否有并发进程或档案目录被清理"
    );

    // 运行器自身的底线：全 0 分通常是环境坏了（端点/密钥/沙箱），不是"模型笨"，
    // 必须让跑的人立刻分辨这两种情况，否则基线报告会被当成可信数据用。
    assert!(
        median(&passed_values) > 0.0,
        "多次运行的中位数仍是 0 分：先怀疑环境（端点可达性/沙箱创建/审批代答），再怀疑模型能力"
    );
}

// ============================== 并发隔离实测（不需要端点与密钥）==============================

/// 子进程侧探针：只有父进程会设 `SHANIU_EVAL_LOCK_PROBE`，平常跑测试时它只是空转。
#[test]
fn eval_lock_probe_child() {
    if std::env::var("SHANIU_EVAL_LOCK_PROBE").is_err() {
        return;
    }
    match std::panic::catch_unwind(EvalRunLock::acquire) {
        Ok(_) => println!("LOCKED-ACQUIRED"),
        Err(_) => println!("LOCKED-DENIED"),
    }
}

/// 真起第二个进程：本进程持锁后，子进程必须被拒绝。
/// 验的是 OS 级互斥，而不是同进程里的一个布尔标志——事故就是跨进程发生的。
#[test]
fn eval_lock_rejects_second_process() {
    let _lock = EvalRunLock::acquire();
    let exe = std::env::current_exe().expect("测试二进制路径");
    let output = std::process::Command::new(exe)
        .args(["eval_lock_probe_child", "--exact", "--nocapture"])
        .env("SHANIU_EVAL_LOCK_PROBE", "1")
        .output()
        .expect("应能启动子进程");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains("LOCKED-DENIED") && !text.contains("LOCKED-ACQUIRED"),
        "第二个进程必须被拒绝，实际输出：{text}"
    );
}

// ============================== SH-125 真机自愈样本 ==============================

/// 在 workspace 里跑 `cargo test`，返回（是否通过，stdout+stderr）。
fn run_cargo_test(ws: &Path) -> (bool, String) {
    let output = std::process::Command::new("cargo")
        .args(["test", "--offline", "--quiet"])
        .current_dir(ws)
        .output()
        .expect("真机样本依赖本机 Rust 工具链（cargo 不在 PATH）");
    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.success(), text)
}

/// 注入两个真实缺陷：一个编译错误（E0308）+ 一个逻辑错误（减法/期望和）。
/// 修好编译错误后测试仍会红，因此必须跑完两轮自愈才算闭环。
const BROKEN_LIB: &str = r#"pub fn add(a: i32, b: i32) -> i32 {
    let total: i32 = "not a number";
    let _ = total;
    a - b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_two_numbers() {
        assert_eq!(add(2, 3), 5);
    }
}
"#;

/// SH-125④：真机样本——向真实 Rust 工程注入编译错误，自愈闭环修到 `cargo test` 绿。
#[tokio::test]
#[ignore = "SH-125 真机自愈样本需要真实端点密钥与本机 Rust 工具链"]
async fn repair_loop_fixes_broken_rust_project_until_cargo_test_green() {
    let api_key = match std::env::var("SHANIU_TEST_API_KEY") {
        Ok(key) if !key.trim().is_empty() => key,
        _ => {
            println!("[repair] 跳过：未设置 SHANIU_TEST_API_KEY（密钥只从环境变量读）");
            return;
        }
    };
    let provider = ProviderConfig {
        provider: std::env::var("SHANIU_TEST_PROVIDER").unwrap_or_else(|_| "openai-chat".into()),
        base_url: std::env::var("SHANIU_TEST_BASE_URL")
            .unwrap_or_else(|_| "https://api.deepseek.com/v1".into()),
        model: std::env::var("SHANIU_TEST_MODEL").unwrap_or_else(|_| "deepseek-flash".into()),
        reasoning_effort: None,
        headers: Default::default(),
    };
    let backend = Arc::new(ResponsesBackend { provider, api_key });

    let ws = repo_root().join("target").join(format!(
        "repair-eval-{}-{}",
        id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(
        ws.join("Cargo.toml"),
        "[package]\nname = \"shaniu_repair_demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    )
    .unwrap();
    std::fs::write(ws.join("src/lib.rs"), BROKEN_LIB).unwrap();
    // 前置断言：注入后的工程确实是红的（不是"本来就能过"）
    let (green_before, before_text) = run_cargo_test(&ws);
    assert!(!green_before, "注入后必须先是红的：{before_text}");

    let plan_state = Arc::new(PlanState::new());
    let mut router = ToolRouter::new();
    tools::register_builtin(&mut router, plan_state.clone(), true);
    let broker = eval_broker();
    let decision = eval_decision();
    let sink = Arc::new(EvalSink {
        agg: Mutex::new(Agg::default()),
        broker: broker.clone(),
        decision,
    });
    let agent = AgentLoop {
        backend,
        router: Arc::new(router),
        broker: broker.clone(),
        plan_state,
        workspace_root: ws.clone(),
        max_turns: 8,
        token_budget: 0,
        tree_budget: Default::default(),
        policy: Arc::new(policy::SessionPolicyHandle::new(policy::SessionPolicy {
            approval: ApprovalMode::OnRequest,
            sandbox: SandboxMode::WorkspaceWrite,
        })),
        rollout: None,
        rollout_broke: std::sync::atomic::AtomicBool::new(false),
        session_id: None,
        jobs: None,
        stop_hook: None,
    };
    let prompt = "这台机器上的 Rust 工程 `cargo test` 不通过。请运行 `cargo test`，读取错误输出，\
                  修复 src/lib.rs 里的问题，直到 `cargo test` 全部通过。不要修改测试断言。";
    let started = Instant::now();
    let summary = tokio::time::timeout(
        Duration::from_secs(300),
        agent.run_task(prompt, sink.clone(), CancellationToken::new()),
    )
    .await;
    let ms = started.elapsed().as_millis();
    let (turns, reason) = match &summary {
        Ok(s) => (s.turns, format!("{:?}", s.reason)),
        Err(_) => (8, "超时（>300s）".to_string()),
    };

    let (green_after, after_text) = run_cargo_test(&ws);
    let source = std::fs::read_to_string(ws.join("src/lib.rs")).unwrap_or_default();
    let agg = sink.agg.lock().unwrap();
    println!(
        "[repair] turns={turns} calls={} 审批={} 耗时={ms}ms 收口={reason}",
        agg.tool_calls, agg.approvals
    );
    for line in &agg.trace {
        println!("    · {line}");
    }
    println!("[repair] 修复后 cargo test:\n{after_text}");
    assert!(
        green_after,
        "自愈闭环没有修到 cargo test 绿（turns={turns} 收口={reason}）"
    );
    assert!(
        source.contains("assert_eq!(add(2, 3), 5)"),
        "不得修改测试断言来冒充成功：{source}"
    );
    assert!(
        source.contains("a + b") || source.contains("a+b"),
        "逻辑错误应由模型修复，而不是硬编码返回值：{source}"
    );
    std::fs::remove_dir_all(&ws).ok();
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
