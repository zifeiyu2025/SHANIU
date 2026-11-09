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
use shaniu_lib::events::{ApprovalDecision, EventMsg, EventSink, ToolCallResult};
use shaniu_lib::model_client::provider::ProviderConfig;
use shaniu_lib::model_client::{HistoryItem, Role};
use shaniu_lib::permission::{policy, ApprovalBroker};
use shaniu_lib::tool_router::{PlanState, ToolRouter};
use shaniu_lib::tools;
use tokio_util::sync::CancellationToken;

const PER_TASK_TIMEOUT: Duration = Duration::from_secs(180);
/// 单任务轮数上限。默认 8 保持既有基线口径；SH-073 的多文件难样本需要为
/// 「编译失败→读错→再改」的修复循环留余量，可用 `SHANIU_EVAL_MAX_TURNS` 提高。
/// 两臂必须用同一个值，否则比的不是自愈而是预算。
const MAX_TURNS_PER_TASK: u32 = 8;

fn max_turns_per_task() -> u32 {
    std::env::var("SHANIU_EVAL_MAX_TURNS")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(MAX_TURNS_PER_TASK)
}
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
    /// 是否给 root 注入编排能力（SH-073 多代理场景）。
    /// 默认 false：既有任务继续保持"没有 orchestrate 工具"的装配，工具面不变。
    #[serde(default)]
    multi_agent: bool,
    /// 是否计入「难样本子集」（SH-073）。汇总数字会被简单任务稀释，
    /// 判断"自愈值不值"只看这个子集，故它必须在报告里单独成列。
    #[serde(default)]
    hard: bool,
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
    /// 多文件断言：`path` 只能查一个文件，而"代码与文档同步改"这类任务必须同时验证
    /// 两个文件。**不要**改用 shell 命令带引号实现——`cmd /C` 会吃掉内层引号
    /// （2026-11-09 实测 `findstr /c:"a b"` 退化为 `Cannot open =`），判据会假红。
    #[serde(default)]
    files: Vec<FileCheck>,
}

#[derive(Deserialize, Default)]
struct FileCheck {
    path: String,
    #[serde(default)]
    contains_all: Vec<String>,
    #[serde(default)]
    absent_all: Vec<String>,
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
    if !check.path.is_empty() {
        verify_file(ws, &check.path, &check.contains_all, &check.absent_all)?;
    }
    for file in &check.files {
        verify_file(ws, &file.path, &file.contains_all, &file.absent_all)?;
    }
    Ok(())
}

fn verify_file(
    ws: &Path,
    path: &str,
    contains_all: &[String],
    absent_all: &[String],
) -> Result<(), String> {
    let target = ws.join(path);
    let text = std::fs::read_to_string(&target)
        .map_err(|e| format!("未产出 {path}（{e}）"))?;
    for needle in contains_all {
        if !text.contains(needle) {
            return Err(format!("{path} 缺少必含内容 {needle:?}"));
        }
    }
    for banned in absent_all {
        if text.contains(banned) {
            return Err(format!("{path} 仍含应被清除的内容 {banned:?}"));
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
    /// 当前轮次（`TurnStarted` 更新）：事件级指标要按轮次划恢复窗口。
    turn: u32,
    /// call_id → (起始轮次, 累积输出)：输出只在终态时用来抽错误签名。
    inflight: BTreeMap<String, (u32, String)>,
    /// 全部工具终态（上限 200，防长任务把内存撑爆）。
    tool_events: Vec<ToolEvent>,
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
            EventMsg::TurnStarted { turn } => {
                agg.turn = *turn;
            }
            EventMsg::ToolCallBegin { call_id, tool, args } => {
                agg.tool_calls += 1;
                if agg.inflight.len() < 64 {
                    let turn = agg.turn;
                    agg.inflight.insert(call_id.clone(), (turn, String::new()));
                }
                if agg.trace.len() < 10 {
                    agg.trace.push(format!("{tool} {}", args_summary(args)));
                }
            }
            EventMsg::ToolCallEnd { call_id, exit_code, error, result } => {
                // 事件级（SH-073）：失败 = 终态 Error。Denied/Cancelled 是审批与取消，
                // 不是"改坏了要修"，计进去会把审批摩擦伪装成自愈信号。
                let ok = match result {
                    Some(ToolCallResult::Ok) => true,
                    Some(_) => false,
                    None => *exit_code == Some(0) && error.is_none(),
                };
                if let Some((turn, output)) = agg.inflight.remove(call_id) {
                    if !ok && agg.tool_events.len() < 200 {
                        let mut body = output;
                        if let Some(err) = error {
                            body.push('\n');
                            body.push_str(err);
                        }
                        agg.tool_events.push(ToolEvent {
                            turn,
                            ok,
                            signature: failure_signature(&body),
                        });
                    } else if ok && agg.tool_events.len() < 200 {
                        // 成功事件也留：恢复轮次要靠它算"什么时候开始不失败了"。
                        agg.tool_events.push(ToolEvent { turn, ok: true, signature: String::new() });
                    }
                }
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
            EventMsg::ToolCallOutputDelta { call_id, delta, .. } => {
                // 输出正文是归因的关键：只看命令会误判成"模型没写文件"，
                // 实际可能是它看到的 cwd/结果与预期不符。每条轨迹限 240 字符。
                if let Some(last) = agg.trace.last_mut() {
                    if last.matches("｜输出=").count() < 1 {
                        let head: String = delta.chars().take(240).collect();
                        last.push_str(&format!("｜输出={head:?}"));
                    }
                }
                if let Some((_, output)) = agg.inflight.get_mut(call_id) {
                    // 签名只需要报错头部；8KB 足够覆盖 cargo 的错误块，又不至于吃内存。
                    if output.len() < 8192 {
                        output.push_str(delta);
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

/// 自愈闭环的现场证据（SH-073）：失败样本必须能回答三件事——
/// ① 有没有触发自愈；② 触发后有没有救回来；③ 那次"绿"是不是靠改测试换来的。
///
/// 数据源是落地档案里的 `Role::System` 条目：`RepairController` 的四种动作都以
/// 独立 System 消息进历史（文案见 `agent_loop/repair.rs`），因此无需给 events
/// 协议加字段就能复盘。前缀与 repair.rs 同源——改那边文案，这里要一起改。
#[derive(Default, Clone, serde::Serialize)]
struct RepairSignals {
    /// 「自动修复提醒」：确实触发过自愈。
    nudge: u32,
    /// 「失败自愈已停止」：触发过但没救回来（次数用尽 / 同一错误重复）。
    give_up: u32,
    /// 「修复被拒绝」：靠删/放宽断言变绿，按失败处理（SH-126）。
    rejected: u32,
    /// 「可疑修复」：改了测试文件但未检出断言放宽。
    suspicious: u32,
}

impl RepairSignals {
    fn scan(items: &[shaniu_lib::rollout::RolloutItem]) -> Self {
        use shaniu_lib::rollout::RolloutItem;
        let mut signals = Self::default();
        for item in items {
            let RolloutItem::ResponseItem { item } = item else { continue };
            let HistoryItem::Message { role, text, .. } = item else { continue };
            if *role != Role::System {
                continue;
            }
            if text.starts_with("自动修复提醒") {
                signals.nudge += 1;
            } else if text.starts_with("失败自愈已停止") {
                signals.give_up += 1;
            } else if text.starts_with("修复被拒绝") {
                signals.rejected += 1;
            } else if text.starts_with("可疑修复") {
                signals.suspicious += 1;
            }
        }
        signals
    }

    /// 是否触发过自愈（任一动作都算：触发过才有"机制值不值"的问题）。
    fn triggered(&self) -> bool {
        self.nudge + self.give_up + self.rejected + self.suspicious > 0
    }

    /// 一句人话归因，直接进失败行：把"无增益"拆成"没触发"与"触发没救回来"。
    fn note(&self, success: bool) -> String {
        if !self.triggered() {
            return if success { "未触发".to_string() } else { "未触发（失败与自愈无关）".to_string() };
        }
        format!(
            "自愈触发: 提醒{} 放弃{} 拒绝{} 可疑{}｜{}",
            self.nudge,
            self.give_up,
            self.rejected,
            self.suspicious,
            if success { "最终成功" } else { "触发后仍未成功" },
        )
    }
}

/// 单次工具调用的终态（事件级指标用）。
struct ToolEvent {
    turn: u32,
    ok: bool,
    /// 失败签名：优先 `文件:行`（跨轮次稳定），否则取首行（去杂讯）。
    /// 判"同一错误"只认签名——同一 fixture 的错误文案逐轮略有差异，位置才稳定。
    signature: String,
}

/// 事件级指标（SH-073）：自愈的作用对象是"失败之后的行为"，任务级成功率会把它稀释。
/// 口径写死，避免事后解释：
/// - 失败事件 = 工具终态为 `Error`（`Denied`/`Cancelled` 是审批与取消，不计）；
/// - 恢复 = 该失败后 **3 轮内**没有出现同签名失败；
/// - 恢复轮次 = 失败轮次到其后首个成功工具结果所在轮次的间隔（无成功记 3，上限）。
///
/// 适用范围：事件级只回答"机制有没有工作"，**不能**用来宣称任务成功率提升。
#[derive(Default, Clone, serde::Serialize)]
struct EventStats {
    failures: u32,
    recovered: u32,
    recovery_rate: f64,
    median_recovery_turns: f64,
    /// 失败签名直方图：只留前 5 个，够归因又不撑爆报告。
    top_signatures: Vec<String>,
}

impl EventStats {
    const WINDOW: u32 = 3;

    fn from_events(events: &[ToolEvent]) -> Self {
        let mut failures = 0u32;
        let mut recovered = 0u32;
        let mut latencies: Vec<u32> = Vec::new();
        let mut counts: BTreeMap<&str, u32> = BTreeMap::new();
        for (i, event) in events.iter().enumerate() {
            if event.ok {
                continue;
            }
            failures += 1;
            *counts.entry(event.signature.as_str()).or_insert(0) += 1;
            let recurred = events[i + 1..].iter().any(|next| {
                !next.ok
                    && next.signature == event.signature
                    && next.turn > event.turn
                    && next.turn <= event.turn + Self::WINDOW
            });
            if !recurred {
                recovered += 1;
                let latency = events[i + 1..]
                    .iter()
                    .find(|next| {
                        next.ok
                            && next.turn > event.turn
                            && next.turn <= event.turn + Self::WINDOW
                    })
                    .map(|next| next.turn - event.turn)
                    .unwrap_or(Self::WINDOW);
                latencies.push(latency);
            }
        }
        latencies.sort_unstable();
        let median = if latencies.is_empty() {
            0.0
        } else if latencies.len().is_multiple_of(2) {
            let mid = latencies.len() / 2;
            f64::from(latencies[mid - 1] + latencies[mid]) / 2.0
        } else {
            f64::from(latencies[latencies.len() / 2])
        };
        let mut ranked: Vec<(&str, u32)> = counts.into_iter().collect();
        ranked.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        Self {
            failures,
            recovered,
            recovery_rate: if failures == 0 { 0.0 } else { f64::from(recovered) / f64::from(failures) },
            median_recovery_turns: median,
            top_signatures: ranked
                .iter()
                .take(5)
                .map(|(sig, n)| format!("{sig} ×{n}"))
                .collect(),
        }
    }
}

/// 从输出里抽 `文件:行`：Rust 报错（`--> src/x.rs:12:5`）、`path:line` 都覆盖。
/// 抽不到就退回首行——但首行含数字（例如 "error: 3 errors"）会漂，故优先位置。
fn failure_signature(output: &str) -> String {
    for line in output.lines() {
        for token in line.split_whitespace() {
            let token = token.trim_matches(|c: char| {
                matches!(c, '`' | '\'' | '"' | '(' | ')' | ',' | ';' | '[' | ']')
            });
            let token = token.strip_prefix("-->").unwrap_or(token);
            let mut parts = token.rsplitn(3, ':');
            let (Some(col), Some(line_no), Some(path)) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if col.parse::<u32>().is_err() || line_no.parse::<u32>().is_err() {
                continue;
            }
            if path.contains('.') && (path.contains('/') || path.contains('\\')) {
                return format!("{path}:{line_no}");
            }
        }
    }
    let first = output.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("");
    let first: String = first.chars().take(120).collect();
    if first.is_empty() {
        "（无输出）".to_string()
    } else {
        first
    }
}

struct Outcome {
    success: bool,
    /// 零轮结束（端点/装配问题）：计入失败但单独统计，归因时不能算成模型能力。
    infra: bool,
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
    repair: RepairSignals,
    events: EventStats,
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
    let agent = Arc::new(AgentLoop {
        backend,
        router: Arc::new(router),
        broker: broker.clone(),
        plan_state,
        workspace_root: ws.clone(),
        max_turns: max_turns_per_task(),
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
    });
    // 多代理场景（SH-073）：只有标了 `multi_agent` 的任务才注入编排器。
    // 其余任务保持原装配（不声明 orchestrate），避免工具面变化污染既有基线。
    let orchestrator: Option<Arc<dyn shaniu_lib::tool_router::orchestration::AgentOrchestrator>> =
        task.multi_agent.then(|| {
            let registry = Arc::new(shaniu_lib::subagent_registry::SubagentRegistry::new(2));
            let orchestrator =
                shaniu_lib::subagent_registry::orchestrator::RegistryOrchestrator::new(
                    registry,
                    agent.clone(),
                );
            Arc::new(orchestrator) as Arc<dyn shaniu_lib::tool_router::orchestration::AgentOrchestrator>
        });

    let started = Instant::now();
    let summary = tokio::time::timeout(
        PER_TASK_TIMEOUT,
        agent.run_task_with_orchestrator(
            &task.prompt,
            sink.clone(),
            CancellationToken::new(),
            orchestrator,
        ),
    )
    .await;
    let ms = started.elapsed().as_millis();

    let (turns, reason) = match &summary {
        Ok(s) => (s.turns, format!("{:?}", s.reason)),
        Err(_) => (max_turns_per_task(), "超时被丢弃（>180s）".to_string()),
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
    // 档案契约（SH-030 的真机一侧）：真实端点跑完必须留下可读回的档案。
    // 断言放在这里而不是另开单测——单测造得出历史，造不出真实端点的时序与取消路径。
    let archive = shaniu_lib::rollout::read(&rollout_dir.join(format!("{session_id}.jsonl")))
        .unwrap_or_else(|e| panic!("任务 {session_id} 的档案读回失败：{e}"));
    // 自愈现场（SH-073）：从同一条档案里数 System 条目，失败样本因此能回答
    // 「没触发」还是「触发了没救回来」——这两种结论完全不同。
    let repair = RepairSignals::scan(&archive.items);
    // 端点抖动可能让第一个模型请求直接失败：档案里只剩用户任务（0 条或 1 条），
    // 没有一条助手侧产出。此前这里一律 assert，一次抖动就把整轮实验连同其它样本
    // 一起炸掉（2026-11-09 两次实测：一次只剩 SessionMeta，一次有用户消息但没有回复）。
    // 判据因此不能用"档案条数"，必须用"有没有助手产出"：零轮空档案与"跑了 1 轮但
    // 请求就失败"是同一种噪声。现在标记 infra 继续跑，该样本仍以"失败"计入
    // （保守，不排除噪声），报告里单列 infra 数量。
    let has_assistant_output = archive.items.iter().any(|item| {
        let shaniu_lib::rollout::RolloutItem::ResponseItem { item } = item else { return false; };
        match item {
            HistoryItem::Message { role, .. } => *role != Role::User,
            HistoryItem::ImageMessage { role, .. } => *role != Role::User,
            _ => true,
        }
    });
    let infra = !has_assistant_output;
    if infra {
        println!("[evals]  {session_id} 无助手产出（首个模型请求失败，记 infra）");
        detail = format!("[infra] 无助手产出（端点或装配失败，非模型能力）｜{detail}");
    }
    let out = Outcome {
        success: verdict.is_ok() && !infra,
        infra,
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
        repair,
        events: EventStats::from_events(&agg.tool_events),
    };
    assert!(
        infra || archive.items.len() >= 2,
        "档案至少要有用户任务与一条助手输出，实际 {} 条：{:?}",
        archive.items.len(),
        archive.items
    );
    assert_eq!(archive.torn_tail_lines, 0, "正常收口不该有半截写残留");
    std::fs::remove_dir_all(&ws).ok();
    out
}

// ============================== 入口 ==============================

/// 密钥形态的前置校验（AGENTS §4.1：只输出布尔与长度，绝不输出内容）。
///
/// 2026-11-09 事故：启动命令把行尾中文备注一起带进了密钥，`HeaderValue` 校验失败，
/// 请求根本没发出去，却跑完 24 个零 token 样本、产出一份「成功率 0%」的合法报告。
/// 形态错误必须在第一个样本之前炸掉，而不是留给报告去解释。
fn assert_key_sane(api_key: &str) {
    let len = api_key.len();
    let ascii = api_key.is_ascii();
    let has_blank = api_key.chars().any(char::is_whitespace);
    assert!(
        ascii && !has_blank && (16..=512).contains(&len),
        "SHANIU_TEST_API_KEY 形态不合法（长度={len}，纯 ASCII={ascii}，含空白={has_blank}）：\
         密钥只从环境变量读，检查提取命令是否把行尾备注/换行一起带进来了"
    );
}

#[tokio::test]
#[ignore = "SH-028 评估基线需要真实端点密钥与外网，按里程碑/变更回归手动或定时运行"]
async fn evals_baseline_v0() {
    // 第一件事就是抢锁：没有锁，后面的任何数字都可能是别人的。
    let _lock = EvalRunLock::acquire();
    let started_ms = now_ms();
    let api_key = std::env::var("SHANIU_TEST_API_KEY")
        .expect("评估基线必须配真实端点：设置 SHANIU_TEST_API_KEY（勿把密钥写进仓库）");
    assert_key_sane(&api_key);
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
    // 任务筛选：
    // - `SHANIU_EVAL_ONLY` 支持逗号分隔多 id（归因时重跑失败的那几个）；
    // - `SHANIU_EVAL_HARD_ONLY=1` 只跑难样本子集——对照组自身的波动必须单独量，
    //   混进简单任务会把离散度摊薄，看不出真实范围（SH-073）。
    let only = std::env::var("SHANIU_EVAL_ONLY").unwrap_or_default();
    let only_ids: Vec<&str> =
        only.split(',').map(str::trim).filter(|id| !id.is_empty()).collect();
    let hard_only = std::env::var("SHANIU_EVAL_HARD_ONLY")
        .ok()
        .is_some_and(|value| value.trim() == "1");
    let selected: Vec<&Task> = spec
        .tasks
        .iter()
        .filter(|t| (only_ids.is_empty() || only_ids.contains(&t.id.as_str())) && (!hard_only || t.hard))
        .collect();
    if selected.is_empty() {
        panic!("任务筛选后为空（SHANIU_EVAL_ONLY={only} SHANIU_EVAL_HARD_ONLY={hard_only}）");
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

    // CI 冒烟/回归探针（2026-11-09 角色分离）：易题不是能力指标，是"还能不能干活"
    // 的报警器——工具链断了、读文件坏了、审批炸了，最先红的必须是它们。
    // smoke 模式下跑全集但只有非 hard 题允许失败：易题红 = 拦；难题红 = 只记录。
    let smoke = std::env::var("SHANIU_EVAL_SMOKE")
        .ok()
        .is_some_and(|value| value.trim() == "1");
    if smoke {
        println!(
            "[evals] 冒烟模式：非基线、不做统计结论、不用于判退化；只拦易题（非 hard）失败"
        );
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
                println!("    · {}", out.repair.note(false));
                for line in &out.trace {
                    println!("    · {line}");
                }
            }
            rows.push(json!({
                "id": task.id,
                "title": task.title,
                "hard": task.hard,
                "success": out.success,
                "infra": out.infra,
                "turns": out.turns,
                "tool_calls": out.tool_calls,
                "trace": out.trace,
                "prompts": out.prompts,
                "input_tokens": out.input_tokens,
                "output_tokens": out.output_tokens,
                "approvals": out.approvals,
                "unresolved_approvals": out.unresolved,
                "ms": out.ms,
                "repair": out.repair,
                "repair_note": out.repair.note(out.success),
                "events": out.events,
                "detail": out.detail,
            }));
        }

        let n = count as f64;
        let sum_turns: u32 = rows.iter().filter_map(|r| r["turns"].as_u64()).map(|t| t as u32).sum();
        let sum_tokens: u64 = rows
            .iter()
            .map(|r| r["input_tokens"].as_u64().unwrap_or(0) + r["output_tokens"].as_u64().unwrap_or(0))
            .sum();
        // 难样本子集单独成列（SH-073）：8 个难任务被 6 个简单任务稀释后，
        // 汇总成功率看不出自愈有没有用；判断"值不值"只能看这一行。
        let hard_rows: Vec<&serde_json::Value> =
            rows.iter().filter(|r| r["hard"].as_bool().unwrap_or(false)).collect();
        let hard_n = hard_rows.len();
        let hard_passed = hard_rows.iter().filter(|r| r["success"].as_bool().unwrap_or(false)).count();
        let hard_turns: u32 =
            hard_rows.iter().filter_map(|r| r["turns"].as_u64()).map(|t| t as u32).sum();
        let hard_tokens: u64 = hard_rows
            .iter()
            .map(|r| r["input_tokens"].as_u64().unwrap_or(0) + r["output_tokens"].as_u64().unwrap_or(0))
            .sum();
        let hard_hit = hard_rows
            .iter()
            .filter(|r| r["repair"]["nudge"].as_u64().unwrap_or(0) > 0
                || r["repair"]["give_up"].as_u64().unwrap_or(0) > 0)
            .count();
        // 事件级聚合（SH-073）：样本量比任务级多一个数量级，用来回答"机制有没有工作"。
        // 注意它**不能**用来宣称任务成功率提升——两件事各管各的。
        let ev_failures: u64 = rows
            .iter()
            .map(|r| r["events"]["failures"].as_u64().unwrap_or(0))
            .sum();
        let ev_recovered: u64 = rows
            .iter()
            .map(|r| r["events"]["recovered"].as_u64().unwrap_or(0))
            .sum();
        let mut ev_latencies: Vec<f64> = rows
            .iter()
            .filter(|r| r["events"]["failures"].as_u64().unwrap_or(0) > 0)
            .filter_map(|r| r["events"]["median_recovery_turns"].as_f64())
            .collect();
        ev_latencies.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        // infra = 零轮结束的样本数：端点抖动留下的噪声，不能算进"模型能力"的归因。
        let infra_count = rows
            .iter()
            .filter(|r| r["infra"].as_bool().unwrap_or(false))
            .count();
        println!(
            "[evals] run {run}/{repeats} 小计 成功率={passed}/{count}，平均轮数={:.2}，平均 token={:.0}，平均打断={:.2}{}",
            f64::from(sum_turns) / n,
            sum_tokens as f64 / n,
            if count == 0 {
                0.0
            } else {
                rows.iter().filter_map(|r| r["approvals"].as_u64()).sum::<u64>() as f64 / n
            },
            if infra_count == 0 {
                String::new()
            } else {
                format!("｜infra={infra_count}（零轮噪声，计入失败）")
            },
        );
        println!(
            "[evals] run {run}/{repeats} 难样本 成功率={hard_passed}/{hard_n}，平均轮数={:.2}，平均 token={:.0}，自愈触发={hard_hit}",
            if hard_n == 0 { 0.0 } else { f64::from(hard_turns) / hard_n as f64 },
            if hard_n == 0 { 0.0 } else { hard_tokens as f64 / hard_n as f64 },
        );
        println!(
            "[evals] run {run}/{repeats} 事件级 失败={ev_failures} 恢复={ev_recovered}/{ev_failures} {} 恢复轮次中位数={:.1}",
            if ev_failures == 0 {
                "（无失败事件）".to_string()
            } else {
                format!("({:.0}%)", 100.0 * ev_recovered as f64 / ev_failures as f64)
            },
            if ev_latencies.is_empty() {
                0.0
            } else if ev_latencies.len().is_multiple_of(2) {
                let mid = ev_latencies.len() / 2;
                (ev_latencies[mid - 1] + ev_latencies[mid]) / 2.0
            } else {
                ev_latencies[ev_latencies.len() / 2]
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
                // 难样本子集（SH-073）：每一轮独立留档——对照组自身的波动范围
                // 就靠这几次的离散度来量，混进汇总数字里就看不出来了。
                "hard": {
                    "n": hard_n,
                    "passed": hard_passed,
                    "success_rate": if hard_n == 0 { 0.0 } else { hard_passed as f64 / hard_n as f64 },
                    "avg_turns": if hard_n == 0 { 0.0 } else { f64::from(hard_turns) / hard_n as f64 },
                    "avg_total_tokens": if hard_n == 0 { 0.0 } else { hard_tokens as f64 / hard_n as f64 },
                    "repair_triggered": hard_hit,
                    "infra": infra_count,
                },
                // 事件级（SH-073）：只看"失败之后有没有恢复"，与任务级并列但不能互相解释。
                "events": {
                    "failures": ev_failures,
                    "recovered": ev_recovered,
                    "recovery_rate": if ev_failures == 0 {
                        0.0
                    } else {
                        ev_recovered as f64 / ev_failures as f64
                    },
                    "median_recovery_turns": if ev_latencies.is_empty() {
                        0.0
                    } else if ev_latencies.len().is_multiple_of(2) {
                        let mid = ev_latencies.len() / 2;
                        (ev_latencies[mid - 1] + ev_latencies[mid]) / 2.0
                    } else {
                        ev_latencies[ev_latencies.len() / 2]
                    },
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
    // 难样本子集的中位数与区间（SH-073）：判断"自愈值不值"的唯一口径。
    let hard_metric = |key: &str| -> Vec<f64> {
        runs.iter().map(|run| run["summary"]["hard"][key].as_f64().unwrap_or(0.0)).collect()
    };
    let hard_success = hard_metric("success_rate");
    let hard_turns = hard_metric("avg_turns");
    let hard_tokens = hard_metric("avg_total_tokens");
    let hard_passed_values: Vec<f64> = runs
        .iter()
        .map(|run| run["summary"]["hard"]["passed"].as_f64().unwrap_or(0.0))
        .collect();
    let hard_total = selected.iter().filter(|task| task.hard).count();
    // 事件级（SH-073）：各轮失败数、恢复率、恢复轮次，取中位数留档。
    let ev_failures: Vec<f64> = runs
        .iter()
        .map(|run| run["summary"]["events"]["failures"].as_f64().unwrap_or(0.0))
        .collect();
    let ev_rates: Vec<f64> = runs
        .iter()
        .map(|run| run["summary"]["events"]["recovery_rate"].as_f64().unwrap_or(0.0))
        .collect();
    let ev_latencies: Vec<f64> = runs
        .iter()
        .map(|run| run["summary"]["events"]["median_recovery_turns"].as_f64().unwrap_or(0.0))
        .collect();
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
    // 门禁（2026-11-09）：整轮空跑不是「成功率 0%」，是密钥/端点装配失败。
    // 把 `assert!(archive.items.len() >= 2)` 放宽成 infra 继续跑，曾让 24 个零 token
    // 样本产出一份 valid=true 的报告——那是把红灯改成黄灯。现在：
    // 全部 infra → 直接 panic（点名密钥/端点）；部分 infra → valid:false，禁止当基线。
    let infra_total = runs
        .iter()
        .flat_map(|run| run["tasks"].as_array().cloned().unwrap_or_default())
        .filter(|row| row["infra"].as_bool().unwrap_or(false))
        .count();
    let all_infra = expected_samples > 0 && infra_total == expected_samples;
    let valid = samples_match && infra_total == 0;
    let report = json!({
        "version": spec.version,
        "arm": arm,
        "repair_enabled": !repair_ablated,
        "role": role,
        "epoch_ms": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis(),
        "provider": provider.provider,
        "base_url": provider.base_url,
        "model": provider.model,
        "max_turns_per_task": max_turns_per_task(),
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
        // infra 样本同理：只要存在"没有任何助手产出"的样本，这份报告就不能当基线。
        "valid": valid,
        // smoke = CI 回归探针（1 轮、全集、只拦易题）；statistical = 本地手动统计实验。
        "mode": if smoke { "smoke" } else { "statistical" },
        "samples": {
            "expected": expected_samples,
            "actual_sessions": actual_sessions,
            "infra": infra_total,
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
            // 难样本子集（SH-073）：与全量并列，单独看这一行才知道自愈有没有用。
            "hard": {
                "n": hard_total,
                "passed": median(&hard_passed_values) as u32,
                "success_rate": median(&hard_success),
                "avg_turns": median(&hard_turns),
                "avg_total_tokens": median(&hard_tokens),
                "success_rate_range": [min(&hard_success), max(&hard_success)],
                "avg_turns_range": [min(&hard_turns), max(&hard_turns)],
            },
            // 事件级（SH-073）：回答"失败之后机制有没有工作"，不是"任务交付有没有变好"。
            "events": {
                "failures": median(&ev_failures) as u64,
                "failures_range": [min(&ev_failures) as u64, max(&ev_failures) as u64],
                "recovery_rate": median(&ev_rates),
                "recovery_rate_range": [min(&ev_rates), max(&ev_rates)],
                "median_recovery_turns": median(&ev_latencies),
            },
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
        "[evals] 难样本汇总（{repeats} 次中位数，n={hard_total}）成功率={:.0}%（区间 {:.0}%–{:.0}%），平均轮数={:.2}（{:.2}–{:.2}），平均 token={:.0}",
        median(&hard_success) * 100.0,
        min(&hard_success) * 100.0,
        max(&hard_success) * 100.0,
        median(&hard_turns),
        min(&hard_turns),
        max(&hard_turns),
        median(&hard_tokens),
    );
    println!(
        "[evals] 样本校验：期望 {expected_samples} 份档案，实际 {actual_sessions} 份 → {}",
        if samples_match { "一致" } else { "不一致（报告已标记 valid:false，禁止据此下结论）" }
    );
    if infra_total > 0 {
        println!(
            "[evals] 警告：{infra_total}/{expected_samples} 个样本无助手产出（infra），报告已标记 valid:false，禁止当基线用"
        );
    }
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

    // smoke 门禁：易题（非 hard）失败 = 工具链/读文件/审批断了，必须拦。
    // 难题失败不拦——它们只在有分辨力时才用于能力对比，不属于回归探针。
    if smoke {
        let mut failed_easy: Vec<&str> = Vec::new();
        for run in &runs {
            for row in run["tasks"].as_array().into_iter().flatten() {
                let Some(id) = row["id"].as_str() else { continue };
                let is_hard = selected.iter().any(|task| task.id == id && task.hard);
                if !is_hard && !row["success"].as_bool().unwrap_or(false) {
                    failed_easy.push(id);
                }
            }
        }
        assert!(
            failed_easy.is_empty(),
            "冒烟门禁：易题失败 {:?}（易题不用于区分能力，但代表「还能不能干活」，红了必须拦）",
            failed_easy
        );
    }

    // 运行器自身的底线：全 0 分通常是环境坏了（端点/密钥/沙箱），不是"模型笨"，
    // 必须让跑的人立刻分辨这两种情况，否则基线报告会被当成可信数据用。
    assert!(
        !all_infra,
        "本轮 {infra_total}/{expected_samples} 个样本没有任何助手产出：密钥或端点装配失败，\
         不是模型能力。先查 SHANIU_TEST_API_KEY 形态（长度/ASCII）与 SHANIU_TEST_BASE_URL 可达性"
    );
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

// ============================== 判据自身的反向验证 ==============================
//
// 两题从假红改为 PASS 只证明「不再误判」，**没证明判据能红**。一个永远返回 Ok 的
// `verify`、一个把缺文件当静默通过的分支，会让整套评估变成"全绿"而无人察觉——
// 那比模型答错严重得多。下面四条是探针：该绿的一条必须绿，三条红的路必须
// 各自红在不同的原因上（缺内容 / 有残留 / 没产出），且失败原因可定位。

fn verify_scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("shaniu-verify-{}-{}-{tag}", id(), now_ms()));
    std::fs::create_dir_all(&dir).expect("创建判据探针临时目录");
    dir
}

fn files_check(path: &str, contains: &[&str], absent: &[&str]) -> Check {
    Check {
        files: vec![FileCheck {
            path: path.to_string(),
            contains_all: contains.iter().map(|s| (*s).to_string()).collect(),
            absent_all: absent.iter().map(|s| (*s).to_string()).collect(),
        }],
        ..Check::default()
    }
}

#[test]
fn verify_accepts_when_every_assertion_holds() {
    let ws = verify_scratch_dir("green");
    std::fs::write(ws.join("settings.toml"), "timeout_ms = 45000\n").unwrap();
    let verdict = verify(&ws, &files_check("settings.toml", &["45000"], &["30000"]));
    std::fs::remove_dir_all(&ws).ok();
    assert!(verdict.is_ok(), "合法产物必须通过：{verdict:?}");
}

#[test]
fn verify_rejects_when_contains_all_missing() {
    let ws = verify_scratch_dir("missing");
    std::fs::write(ws.join("settings.toml"), "timeout_ms = 30000\n").unwrap();
    let verdict = verify(&ws, &files_check("settings.toml", &["45000"], &[]));
    std::fs::remove_dir_all(&ws).ok();
    let reason = verdict.expect_err("目标串缺失时必须判红，否则检查形同虚设");
    assert!(
        reason.contains("缺少必含内容"),
        "失败原因必须指向缺失项，实际：{reason}"
    );
}

#[test]
fn verify_rejects_when_absent_all_still_present() {
    let ws = verify_scratch_dir("stale");
    std::fs::write(ws.join("settings.toml"), "timeout_ms = 30000\n").unwrap();
    let verdict = verify(&ws, &files_check("settings.toml", &[], &["30000"]));
    std::fs::remove_dir_all(&ws).ok();
    let reason = verdict.expect_err("应被清除的内容仍在时必须判红");
    assert!(
        reason.contains("仍含应被清除的内容"),
        "失败原因必须指向残留项，实际：{reason}"
    );
}

#[test]
fn verify_rejects_when_file_missing() {
    let ws = verify_scratch_dir("absent");
    let verdict = verify(&ws, &files_check("settings.toml", &[], &[]));
    std::fs::remove_dir_all(&ws).ok();
    let reason = verdict.expect_err("文件不存在时必须判红，不能静默通过");
    assert!(
        reason.contains("未产出"),
        "失败原因必须指向未产出文件，实际：{reason}"
    );
}
