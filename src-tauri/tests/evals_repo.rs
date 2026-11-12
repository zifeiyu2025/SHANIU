//! 真实仓库端到端评测运行器（`evals/repo-tasks.json`）。
//!
//! 与 `evals_baseline.rs` 的关系：**装配部分逐行照搬**（`EvalRunLock`、`EvalSink`、
//! `eval_broker`、`AgentLoop` 字段、`result_path`、`assert_key_sane`）。差别只在
//! 「工作区从哪来」与「判据怎么算」：
//!
//! - 基线任务集是合成 fixture；这里跑的是**仓库自身的历史快照**，题目由补丁注入，
//!   判据是外部脚本 `evals/repo/judge/run.ps1`（退出码 0 且 stdout 含 `REPO-CHECK: PASS`）。
//! - 快照固定在 `base_commit`（不是 HEAD），用 `git read-tree` + `git checkout-index`
//!   物化到 `target/evals-repo-pristine-<pid>/`。刻意不用 `git archive`：Windows 下
//!   非 UTF-8 文件名会让它整体失败。
//! - 每题开始前用 robocopy `/MIR` 把工作区还原成快照，然后**重新 `git init` 并提交一次**，
//!   让 agent 看到的历史只有「本题基线」这一条——快照的真实 git 历史（含答案提交）不能给它。
//! - 每题跑 agent 之前先跑一次判据，**必须红**。前置不红说明补丁没打上或判据不区分，
//!   该题标 `invalid`，不进分母、不算失败。
//!
//! 运行（需要密钥，密钥只从环境变量读，永不入库/入日志）：
//! ```text
//! SHANIU_TEST_API_KEY=... cargo test --test evals_repo -- --ignored --nocapture
//! ```
//! `SHANIU_EVAL_REPEATS` 默认 3（同基线：单次会被端点漂移支配，取各轮中位数）。
//!
//! 报告：`evals/results/<epoch_ms>-<pid>-repo.json`，阅读器 `node evals/score-repo.mjs <报告>`。

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{id, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use shaniu_lib::agent_loop::{AgentLoop, ModelBackend, ResponsesBackend};
use shaniu_lib::clock::now_ms;
use shaniu_lib::config::{ApprovalMode, SandboxMode};
use shaniu_lib::events::{ApprovalDecision, EventMsg, EventSink};
use shaniu_lib::model_client::provider::ProviderConfig;
use shaniu_lib::model_client::{HistoryItem, ModelError, ModelStream, ToolDeclaration};
use shaniu_lib::permission::{policy, ApprovalBroker};
use shaniu_lib::tool_router::{PlanState, ToolRouter};
use shaniu_lib::tools;
use tokio_util::sync::CancellationToken;

const DEFAULT_TASKS_FILE: &str = "evals/repo-tasks.json";
const DEFAULT_REPEATS: usize = 3;
/// 判据通过标记：退出码 0 之外还要求 stdout 真的说出这句话，
/// 否则脚本「参数错误但退出码 0」会被读成通过。
const JUDGE_PASS_MARKER: &str = "REPO-CHECK: PASS";
/// 评测侧模型请求超时（只包住「拿到响应头」这一步）。超时归因固定写成
/// 「评测自身/环境」，不把超时算成模型判断错误。
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 180;
/// 整题硬超时 = 题面 `timeout_secs` + 这段余量（准备快照、判据编译、判据本身）。
/// 子进程到点直接被强杀，保证一题挂死不会拖住整轮。
const DEFAULT_TASK_HARD_EXTRA_SECS: u64 = 420;

static RUN_LOG: OnceLock<Mutex<File>> = OnceLock::new();

fn run_log_path() -> PathBuf {
    std::env::var("SHANIU_EVAL_RUN_LOG").map_or_else(
        |_| {
            repo_root()
                .join("target")
                .join(format!("evals-repo-run-{}.log", id()))
        },
        PathBuf::from,
    )
}

fn run_log() -> &'static Mutex<File> {
    RUN_LOG.get_or_init(|| {
        let path = run_log_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap_or_else(|error| {
                panic!("打开运行日志 {} 失败：{error}", path.display())
            });
        Mutex::new(file)
    })
}

/// 运行日志：文件 + stdout 双写。父子进程用同一份文件，靠 append 模式共享。
fn log_line(line: &str) {
    if let Ok(mut file) = run_log().lock() {
        let _ = writeln!(file, "[{}] {line}", now_ms());
        let _ = file.flush();
    }
    println!("[repo-eval] {line}");
}

fn request_timeout() -> Duration {
    let secs = std::env::var("SHANIU_EVAL_REQUEST_TIMEOUT_SECS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

fn task_hard_extra() -> Duration {
    let secs = std::env::var("SHANIU_EVAL_TASK_HARD_EXTRA_SECS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(DEFAULT_TASK_HARD_EXTRA_SECS);
    Duration::from_secs(secs)
}

/// 模型后端打点装饰器：每次请求记录开始/结束/超时，并给「拿响应头」套超时。
///
/// 为什么加在评测侧而不是被测产品：本轮要回答的是「评测能不能稳定跑完」，
/// 模型端点若中途不返回，必须能被判定为评测/环境层的超时，而不是被读成
/// 「模型选错了修改方向」。产品自身的模型超时（connect 15s / 首字节 120s /
/// 整请求 900s）保持原样，不在这里改。
struct TimedBackend {
    inner: Arc<ResponsesBackend>,
    label: String,
    seq: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelBackend for TimedBackend {
    async fn stream(
        &self,
        history: &[HistoryItem],
        tools: &[ToolDeclaration],
        instructions: &str,
    ) -> Result<ModelStream, ModelError> {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let limit = request_timeout();
        log_line(&format!(
            "[req] task={} seq={seq} start timeout={}s",
            self.label,
            limit.as_secs()
        ));
        let started = Instant::now();
        let result = tokio::time::timeout(
            limit,
            self.inner.stream(history, tools, instructions),
        )
        .await;
        let ms = started.elapsed().as_millis();
        match result {
            Ok(Ok(stream)) => {
                log_line(&format!(
                    "[req] task={} seq={seq} end ok ms={ms}",
                    self.label
                ));
                Ok(stream)
            }
            Ok(Err(error)) => {
                log_line(&format!(
                    "[req] task={} seq={seq} end err ms={ms} kind={:?} code={}",
                    self.label,
                    error.kind,
                    error.code()
                ));
                Err(error)
            }
            Err(_) => {
                log_line(&format!(
                    "[req] task={} seq={seq} end TIMEOUT ms={ms}（超时归因=评测自身/环境，不是模型判断错误）",
                    self.label
                ));
                Err(ModelError::timeout(format!(
                    "评测侧模型请求超时（{}s 内未返回响应头）；超时归因=评测自身/环境，不是模型判断错误",
                    limit.as_secs()
                )))
            }
        }
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("src-tauri 的上一级是仓库根")
        .to_path_buf()
}

fn rel_path(path: &Path) -> String {
    let root = repo_root();
    let text = path
        .strip_prefix(&root)
        .map_or_else(|_| path.display().to_string(), |p| p.display().to_string());
    text.replace('\\', "/")
}

// ============================== 运行隔离（照搬 evals_baseline.rs）==============================

/// 独占锁：同一时刻只允许一个评估进程。锁随文件句柄释放，崩溃不会留下死锁。
struct EvalRunLock {
    _file: File,
}

impl EvalRunLock {
    fn acquire() -> Self {
        let path = repo_root().join("evals").join(".eval.lock");
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .unwrap_or_else(|error| panic!("打不开评估锁 {}：{error}", path.display()));
        if let Err(error) = file.try_lock() {
            let owner = std::fs::read_to_string(&path)
                .unwrap_or_default()
                .trim()
                .to_string();
            panic!(
                "已有评估进程在跑，拒绝并行启动（{error}）。\n\
锁：{}\n\
持有者：{}\n\
并行会互相覆盖结果与工作区，结论不可用。等它结束再跑。",
                path.display(),
                if owner.is_empty() {
                    "（未记录）"
                } else {
                    owner.as_str()
                },
            );
        }
        let info = format!(
            "pid={} started_ms={} role=repo endpoint={} model={}\n",
            id(),
            now_ms(),
            std::env::var("SHANIU_TEST_BASE_URL").unwrap_or_default(),
            std::env::var("SHANIU_TEST_MODEL").unwrap_or_default(),
        );
        file.set_len(0).ok();
        file.write_all(info.as_bytes()).ok();
        file.flush().ok();
        Self { _file: file }
    }
}

/// 一进程一份结果文件：`evals/results/<epoch_ms>-<pid>-repo.json`（并发下互不覆盖）。
fn result_path() -> PathBuf {
    let dir = repo_root().join("evals").join("results");
    std::fs::create_dir_all(&dir).expect("创建 evals/results");
    dir.join(format!("{}-{}-repo.json", now_ms(), id()))
}

/// 本进程独占的档案目录：`target/evals-repo-rollouts-<pid>/<task id>/`。
/// 样本校验靠「数 jsonl」，起点必须先清空，否则残留会让校验永远对不上。
fn reset_rollout_dir() -> PathBuf {
    let dir = repo_root()
        .join("target")
        .join(format!("evals-repo-rollouts-{}", id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("创建 repo 档案目录");
    dir
}

// ============================== 任务集（evals/repo-tasks.json，只读）==============================

#[derive(Deserialize)]
struct RepoTasksFile {
    #[serde(default = "default_version")]
    version: u32,
    base_commit: String,
    judge: String,
    tasks: Vec<RepoTask>,
}

fn default_version() -> u32 {
    1
}

#[derive(Deserialize, Clone)]
struct RepoTask {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    title: String,
    prompt: String,
    /// 相对仓库根的补丁路径；`null` 表示本题靠 fixture 落盘（新增验收用例）。
    patch: Option<String>,
    #[serde(default)]
    fixtures: Vec<Fixture>,
    judge: String,
    timeout_secs: u64,
    max_turns: u32,
    #[serde(default)]
    forbid_paths: Vec<String>,
    /// 初始状态是否允许编译不过。默认 `true`（预热失败 = 准备失败 = invalid）；
    /// 重构类题目（如「改了函数名、调用点未跟上」）初始必然编译不过，
    /// 这类题置 `false`：预热失败只记账，不判 invalid——判据仍必须前置红。
    #[serde(default = "default_true")]
    warmup_compile: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize, Clone)]
struct Fixture {
    from: String,
    to: String,
}

// ============================== 子进程工具 ==============================

struct CmdOut {
    code: i32,
    stdout: String,
    stderr: String,
}

impl CmdOut {
    fn ok(&self) -> bool {
        self.code == 0
    }

    fn combined(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

fn exec(
    program: &str,
    args: &[String],
    cwd: &Path,
    envs: &[(&str, String)],
) -> Result<CmdOut, String> {
    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(cwd);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    let output = cmd
        .output()
        .map_err(|e| format!("无法启动 {program}（{e}）"))?;
    Ok(CmdOut {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

fn git(root: &Path, args: &[&str]) -> Result<CmdOut, String> {
    let owned: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
    exec("git", &owned, root, &[])
}

fn git_env(root: &Path, args: &[&str], envs: &[(&str, String)]) -> Result<CmdOut, String> {
    let owned: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
    exec("git", &owned, root, envs)
}

/// 输出尾部（判据摘录用）：只留最后 `limit` 个字符。
fn tail(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    let count = trimmed.chars().count();
    if count <= limit {
        return trimmed.to_string();
    }
    trimmed.chars().skip(count - limit).collect()
}

/// `detail` 上限 400 字（字符，不是字节）。
fn clip(text: &str, limit: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect::<String>()
}

// ============================== 快照物化 ==============================

/// `git rev-parse --verify <base_commit>^{commit}`：必须解析成完整 sha。
fn resolve_base_commit(root: &Path, short: &str) -> Result<String, String> {
    let spec = format!("{short}^{{commit}}");
    let out = git(root, &["rev-parse", "--verify", &spec])?;
    if !out.ok() {
        return Err(format!(
            "base_commit `{short}` 解析失败：{}",
            tail(&out.combined(), 300)
        ));
    }
    let sha = out.stdout.trim().to_string();
    if sha.len() != 40 {
        return Err(format!("base_commit `{short}` 解析结果不是完整 sha：{sha}"));
    }
    Ok(sha)
}

fn count_files(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            total += count_files(&path);
        } else {
            total += 1;
        }
    }
    total
}

fn count_jsonl(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            total += count_jsonl(&path);
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            total += 1;
        }
    }
    total
}

/// 固定快照物化：`read-tree <sha>` 到私有 index，再 `checkout-index -a --prefix=<abs>/`。
/// 返回物化的文件数（调用方与 `git ls-tree` 比）。
fn materialize_snapshot(root: &Path, sha: &str, dest: &Path) -> Result<usize, String> {
    std::fs::create_dir_all(dest).map_err(|e| format!("创建快照目录失败：{e}"))?;
    let index = root
        .join("target")
        .join(format!("evals-repo-index-{}", id()));
    std::fs::create_dir_all(index.parent().unwrap_or(root))
        .map_err(|e| format!("创建 target 失败：{e}"))?;
    let index_value = index.display().to_string().replace('\\', "/");
    let envs = [("GIT_INDEX_FILE", index_value)];
    let read = git_env(root, &["read-tree", sha], &envs)?;
    if !read.ok() {
        return Err(format!(
            "git read-tree {sha} 失败：{}",
            tail(&read.combined(), 300)
        ));
    }
    let prefix = format!("{}/", dest.display().to_string().replace('\\', "/"));
    let checkout = git_env(
        root,
        &["checkout-index", "-a", &format!("--prefix={prefix}")],
        &envs,
    )?;
    std::fs::remove_file(&index).ok();
    if !checkout.ok() {
        return Err(format!(
            "git checkout-index 失败：{}",
            tail(&checkout.combined(), 300)
        ));
    }
    Ok(count_files(dest))
}

/// `git ls-tree -r <sha> --name-only` 的行数：快照完整性的唯一口径。
fn snapshot_expected_files(root: &Path, sha: &str) -> Result<usize, String> {
    let out = git(root, &["ls-tree", "-r", sha, "--name-only"])?;
    if !out.ok() {
        return Err(format!("git ls-tree 失败：{}", tail(&out.combined(), 300)));
    }
    Ok(out.stdout.lines().count())
}

// ============================== 判据 ==============================

struct JudgeResult {
    pass: bool,
    detail: String,
}

/// `pwsh -NoProfile -File <judge> -Task <key> -Root <ws>`：
/// 通过 = 退出码 0 且 stdout 含 `REPO-CHECK: PASS`。
fn run_judge(script: &Path, key: &str, ws: &Path) -> Result<JudgeResult, String> {
    let args = vec![
        "-NoProfile".to_string(),
        "-File".to_string(),
        script.display().to_string(),
        "-Task".to_string(),
        key.to_string(),
        "-Root".to_string(),
        ws.display().to_string(),
    ];
    let out = exec("pwsh", &args, ws, &[])?;
    let pass = out.code == 0 && out.stdout.contains(JUDGE_PASS_MARKER);
    Ok(JudgeResult {
        pass,
        detail: tail(&out.combined(), 400),
    })
}

// ============================== 事件采集 + 审批代答（照搬 evals_baseline.rs）==============================

#[derive(Default)]
struct Agg {
    approvals: usize,
    /// 每次打断的工具 + 风险档 + 原因（上限 12 条）：只报总数无法回答「该不该降噪」。
    prompts: Vec<String>,
    errors: Vec<String>,
}

fn args_summary(args: &Value) -> String {
    let text = args.to_string();
    let clipped = text.chars().take(90).collect::<String>();
    if clipped.chars().count() < text.chars().count() {
        format!("{clipped}…")
    } else {
        clipped
    }
}

/// 审批口径固定 `allow_once`：本题组量的是「真实用户逐次确认」的打断成本，
/// 用 `AllowAlways` 会把策略记忆混进指标（对应基线 `SHANIU_EVAL_APPROVAL=allow_once`）。
struct EvalSink {
    agg: Mutex<Agg>,
    broker: Arc<ApprovalBroker>,
}

impl EventSink for EvalSink {
    fn emit(&self, msg: &EventMsg) -> Result<(), String> {
        let mut agg = self.agg.lock().unwrap();
        match msg {
            EventMsg::Error { message } => {
                if agg.errors.len() < 3 {
                    agg.errors.push(message.chars().take(200).collect());
                }
            }
            EventMsg::ExecApprovalRequest {
                request_id,
                tool,
                risk,
                reason,
                args,
                ..
            } => {
                agg.approvals += 1;
                if agg.prompts.len() < 12 {
                    agg.prompts.push(format!(
                        "{tool} risk={risk:?} 原因={} 参数={}",
                        reason.as_deref().unwrap_or("-"),
                        args_summary(args)
                    ));
                }
                // resolve 返回 false 说明请求 ID 不是后端签发的——必须可见，不能静默。
                if !self.broker.resolve(request_id, ApprovalDecision::Allow) {
                    agg.errors.push(format!(
                        "审批代答失败：{request_id}（请求不是 broker 签发的）"
                    ));
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// 与基线同款 broker：同 turn 批量确认 + 前缀白名单都在（生产语义，不阉割）。
fn eval_broker() -> Arc<ApprovalBroker> {
    let broker = Arc::new(ApprovalBroker::new());
    let (rules, _) =
        policy::PrefixAllowlist::from_config(&shaniu_lib::config::default_allow_prefixes());
    broker.set_prefix_rules(rules);
    broker
}

// ============================== 单题结果 ==============================

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct TaskOutcome {
    id: String,
    kind: String,
    base_commit: String,
    patch_applied: bool,
    /// 补丁应用校验结果：`ok`（`git apply --check` 通过）/ `none`（本题无补丁）/
    /// `failed`（校验或应用失败）。报告逐题必带，缺失即 invalid。
    patch_check: String,
    /// 预热编译是否通过（初始状态允许编译不过的题会为 `false`）。
    warmup_ok: bool,
    precondition_red: bool,
    /// 是否真的跑到「前置红」这一步（准备阶段就失败时为 false）。
    precondition_checked: bool,
    invalid: bool,
    invalid_reason: Option<String>,
    success: bool,
    timeout: bool,
    /// 超时归因层：`agent`（题内超时，含模型请求/工具未在时限内返回）/
    /// `eval_hard`（评测侧整题子进程硬超时被强杀）。`None` 表示未超时。
    /// 两种都属于「评测自身/环境」，不能读成模型判断错误。
    timeout_layer: Option<String>,
    turns: Option<u32>,
    interruptions: usize,
    prompt_attribution: Vec<String>,
    agent_ms: u128,
    setup_ms: u128,
    check_ms: u128,
    warmup_ms: u128,
    files_changed: usize,
    forbidden_touched: bool,
    detail: String,
}

impl TaskOutcome {
    fn invalidate(&mut self, reason: impl Into<String>) {
        self.invalid = true;
        self.invalid_reason = Some(reason.into());
    }

    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "type": self.kind,
            "base_commit": self.base_commit,
            "patch_applied": self.patch_applied,
            "patch_check": self.patch_check,
            "warmup_ok": self.warmup_ok,
            "precondition_red": self.precondition_red,
            "invalid": self.invalid,
            "invalid_reason": self.invalid_reason,
            "success": self.success,
            "timeout": self.timeout,
            "timeout_layer": self.timeout_layer,
            "turns": self.turns,
            "interruptions": self.interruptions,
            "prompt_attribution": self.prompt_attribution,
            "agent_ms": self.agent_ms as u64,
            "setup_ms": self.setup_ms as u64,
            "check_ms": self.check_ms as u64,
            "warmup_ms": self.warmup_ms as u64,
            "files_changed": self.files_changed,
            "forbidden_touched": self.forbidden_touched,
            "detail": self.detail,
        })
    }
}

// ============================== 统计口径 ==============================

fn rate(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

/// 中位数（照搬 evals_baseline.rs 的 `median`）。
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

fn median_option(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        None
    } else {
        Some(median(values))
    }
}

/// 最近秩 P90（小样本下取上界，避免插值造出不存在的轮数）。
fn percentile(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let index = ((sorted.len() - 1) as f64 * p).ceil() as usize;
    Some(sorted[index.min(sorted.len() - 1)])
}

fn mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        None
    } else {
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }
}

/// Wilson 95% 区间（自己实现，不引依赖）。n=0 时返回 [0,0]。
fn wilson(passed: usize, n: usize) -> [f64; 2] {
    if n == 0 {
        return [0.0, 0.0];
    }
    const Z: f64 = 1.959_963_984_540_054;
    let n = n as f64;
    let p = passed as f64 / n;
    let denom = 1.0 + Z * Z / n;
    let center = (p + Z * Z / (2.0 * n)) / denom;
    let half = Z * ((p * (1.0 - p) / n + Z * Z / (4.0 * n * n)).sqrt()) / denom;
    [(center - half).max(0.0), (center + half).min(1.0)]
}

fn interruptions_stats(valid: &[&TaskOutcome]) -> Value {
    let counts: Vec<f64> = valid.iter().map(|task| task.interruptions as f64).collect();
    json!({
        "total": valid.iter().map(|task| task.interruptions).sum::<usize>(),
        "mean": mean(&counts),
        "median": median_option(&counts),
        "p90": percentile(&counts, 0.9),
        "tasks_with_any": valid.iter().filter(|task| task.interruptions > 0).count(),
    })
}

/// 单轮 summary：分母只算有效题（invalid 单列，不进分母）。
fn summarize(tasks: &[TaskOutcome], max_turns: u32) -> Value {
    let valid: Vec<&TaskOutcome> = tasks.iter().filter(|task| !task.invalid).collect();
    let n = valid.len();
    let invalid = tasks.len() - n;
    let passed = valid.iter().filter(|task| task.success).count();
    let timeouts = valid.iter().filter(|task| task.timeout).count();
    let passed_no_timeout = valid
        .iter()
        .filter(|task| task.success && !task.timeout)
        .count();
    // 轮数只在**已知**的样本上统计：超时样本既不填 0 也不填上限。
    let turns: Vec<f64> = valid
        .iter()
        .filter_map(|task| task.turns)
        .map(f64::from)
        .collect();
    let agent: Vec<f64> = valid.iter().map(|task| task.agent_ms as f64).collect();
    let success_rate = rate(passed, n);
    json!({
        "n": n,
        "invalid": invalid,
        "passed": passed,
        "success_rate": success_rate,
        "success_rate_ci95": wilson(passed, n),
        // 与 success_rate 同分母（invalid 已剔除），单列是为了让读取方不必猜分母。
        "success_rate_no_timeout": rate(passed_no_timeout, n - timeouts),
        "success_rate_valid_only": success_rate,
        "avg_turns": mean(&turns),
        "median_turns": median_option(&turns),
        "p90_turns": percentile(&turns, 0.9),
        "turns_unknown": n - turns.len(),
        "max_turns": max_turns,
        "interruptions": interruptions_stats(&valid),
        "timeout_rate": rate(timeouts, n),
        "duration": {
            "agent_ms_p50": median_option(&agent),
            "agent_ms_p90": percentile(&agent, 0.9),
            "agent_ms_total": valid.iter().map(|task| task.agent_ms).sum::<u128>() as u64,
            "setup_ms_total": valid.iter().map(|task| task.setup_ms).sum::<u128>() as u64,
            "check_ms_total": valid.iter().map(|task| task.check_ms).sum::<u128>() as u64,
        },
    })
}

fn pick<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = value;
    for key in path {
        current = current.get(key)?;
    }
    Some(current)
}

fn run_numbers(runs: &[Value], path: &[&str]) -> Vec<f64> {
    runs.iter()
        .filter_map(|run| pick(run, path).and_then(Value::as_f64))
        .collect()
}

// ============================== 工作区准备 ==============================

/// 工作区放临时目录，故意**不放**仓库 `target/` 下：
/// `scripts/dev/check-doc-claims.ps1` 用正则 `\target\` 排除构建产物，工作区若在
/// `target/` 下会被整棵跳过——读 0 个源码文件后直接判「环境不可比」，doc-claim 题
/// 永远红，且这种红与题目本身无关。临时目录同时不污染工作树。
fn workspace_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("shaniu-{tag}-{}", id()))
}

/// 快照 → 工作区（`/MIR`，排除 cargo 产物目录以保住增量）。robocopy 0-7 都算成功。
/// 排除 `src-tauri/target`：预热就落在这里，agent 的 `cargo test` 复用同一目录才不会
/// 在沙箱里重新触发 tauri-build（冷构建 + AppContainer 下它只认到 dialog 权限，
/// 会把 `core:window:allow-minimize` 判成不存在，任务直接变成不可解）。
fn mirror_into_workspace(pristine: &Path, ws: &Path) -> Result<(), String> {
    std::fs::create_dir_all(ws).map_err(|e| format!("创建工作区失败：{e}"))?;
    let args = vec![
        pristine.display().to_string(),
        ws.display().to_string(),
        "/MIR".to_string(),
        "/XD".to_string(),
        ws.join("target").display().to_string(),
        ws.join("src-tauri").join("target").display().to_string(),
        "/COPY:DAT".to_string(),
        "/NFL".to_string(),
        "/NDL".to_string(),
        "/NJH".to_string(),
        "/NJS".to_string(),
        "/NP".to_string(),
    ];
    let out = exec("robocopy", &args, pristine, &[])?;
    // robocopy 用位掩码表达结果：0-7 是「有拷贝/有跳过但成功」，>=8 才是错误。
    if !(0..=7).contains(&out.code) {
        return Err(format!(
            "robocopy 失败（code={}）：{}",
            out.code,
            tail(&out.combined(), 300)
        ));
    }
    Ok(())
}

/// 准备工作区；失败返回 `(原因, patch_applied)`。
/// 成功返回 `(预热耗时, task_base, 预热是否编译通过)`。
fn prepare_workspace(
    task: &RepoTask,
    pristine: &Path,
    ws: &Path,
) -> Result<(u128, String, bool), (String, bool)> {
    // 1) 还原快照。先删 .git：否则 /MIR 之外还会留着上一题的提交历史。
    std::fs::remove_dir_all(ws.join(".git")).ok();
    mirror_into_workspace(pristine, ws).map_err(|reason| (reason, task.patch.is_none()))?;

    // 2) 先建立本题自己的仓库，再打补丁：工作区没有 `.git`，`git apply`
    //    会向上发现主仓库，把补丁落到仓库根而不是 ws 根。
    let init = git(ws, &["init", "-q"]).map_err(|reason| (reason, task.patch.is_none()))?;
    if !init.ok() {
        return Err((
            format!("工作区 git init 失败：{}", tail(&init.combined(), 300)),
            false,
        ));
    }

    // 3) 补丁（相对仓库根；此时 ws 已是独立仓库，路径按 ws 根解释）。
    let mut patch_applied = task.patch.is_none();
    if let Some(patch) = &task.patch {
        let patch_path = repo_root().join(patch);
        if !patch_path.exists() {
            return Err((format!("补丁缺失：{patch}"), false));
        }
        let patch_arg = patch_path.display().to_string();
        let check = git(ws, &["apply", "--check", &patch_arg]).map_err(|reason| (reason, false))?;
        if !check.ok() {
            return Err((
                format!("补丁 `{patch}` 无法应用：{}", tail(&check.combined(), 300)),
                false,
            ));
        }
        let apply = git(ws, &["apply", &patch_arg]).map_err(|reason| (reason, false))?;
        if !apply.ok() {
            return Err((
                format!("补丁 `{patch}` 应用失败：{}", tail(&apply.combined(), 300)),
                false,
            ));
        }
        patch_applied = true;
    }

    // 3) fixtures：`from` 相对仓库根，`to` 相对工作区根。
    for fixture in &task.fixtures {
        let source = repo_root().join(&fixture.from);
        let dest = ws.join(&fixture.to);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                (
                    format!("创建 fixture 目录失败 {}：{e}", parent.display()),
                    patch_applied,
                )
            })?;
        }
        std::fs::copy(&source, &dest).map_err(|e| {
            (
                format!("fixture 拷贝失败 {} → {}：{e}", fixture.from, fixture.to),
                patch_applied,
            )
        })?;
    }

    // 4) 新历史：只有「本题基线」一条提交，agent 无法 `git show` 出答案。
    let commit_message = format!("task {}", task.id);
    for args in [
        vec!["add", "-A"],
        vec![
            "-c",
            "user.email=eval@local",
            "-c",
            "user.name=eval",
            "commit",
            "-q",
            "-m",
            commit_message.as_str(),
        ],
    ] {
        let out = git(ws, &args).map_err(|reason| (reason, patch_applied))?;
        if !out.ok() {
            return Err((
                format!(
                    "git {} 失败：{}",
                    args.join(" "),
                    tail(&out.combined(), 300)
                ),
                patch_applied,
            ));
        }
    }
    // task_base 显式留档：改动面必须跟这一条比，agent 自己再 commit 也不能把改动藏掉。
    let task_base = git(ws, &["rev-parse", "HEAD"]).map_err(|reason| (reason, patch_applied))?;
    if !task_base.ok() {
        return Err((
            format!(
                "git rev-parse HEAD 失败：{}",
                tail(&task_base.combined(), 300)
            ),
            patch_applied,
        ));
    }
    let task_base = task_base.stdout.trim().to_string();

    // 5) 预热：编进 agent 自己的默认产物目录（`src-tauri/target`）。
    //    刻意不设 CARGO_TARGET_DIR：agent 的 `cargo test` 用的是默认目录，只有预热落在
    //    同一处，构建脚本（tauri-build）才不会在沙箱内重跑；否则 AppContainer 下 ACL
    //    重新生成只认到 dialog 权限，编译直接红，题目变成不可解。
    let warm_start = Instant::now();
    let manifest = ws
        .join("src-tauri")
        .join("Cargo.toml")
        .display()
        .to_string();
    let warm = exec(
        "cargo",
        &[
            "test".to_string(),
            "--manifest-path".to_string(),
            manifest,
            "--lib".to_string(),
            "--offline".to_string(),
            "--no-run".to_string(),
        ],
        ws,
        &[],
    )
    .map_err(|reason| (reason, patch_applied))?;
    let warmup_ms = warm_start.elapsed().as_millis();
    let warmup_ok = warm.ok();
    if !warmup_ok && task.warmup_compile {
        return Err((
            format!(
                "预热失败（cargo test --no-run）：{}",
                tail(&warm.combined(), 400)
            ),
            patch_applied,
        ));
    }
    Ok((warmup_ms, task_base, warmup_ok))
}

// ============================== 改动面 ==============================

fn normalize_path(path: &str) -> String {
    let text = path.trim().trim_matches('"').replace('\\', "/");
    text.strip_prefix("./").unwrap_or(&text).to_string()
}

fn changed_files(ws: &Path, task_base: &str) -> Vec<String> {
    let mut files = BTreeSet::new();
    if let Ok(status) = git(ws, &["status", "--porcelain"]) {
        for line in status.stdout.lines() {
            if line.len() < 4 {
                continue;
            }
            let rest = &line[3..];
            // 重命名行是 `old -> new`，只取新路径。
            let path = rest.rsplit(" -> ").next().unwrap_or(rest);
            if !path.is_empty() {
                files.insert(normalize_path(path));
            }
        }
    }
    if let Ok(diff) = git(ws, &["diff", "--name-only", task_base]) {
        for line in diff.stdout.lines() {
            if !line.trim().is_empty() {
                files.insert(normalize_path(line));
            }
        }
    }
    files.into_iter().collect()
}

// ============================== 单题执行 ==============================

/// 单题执行的输入集合。参数收进结构体而不是堆在签名上：`run_one` 需要
/// 题目、第几轮、基座、判据、快照、工作区、档案根共 7 项（模型后端单独传），
/// 平铺会触发 `clippy::too_many_arguments`，且调用点顺序错了很难看出来。
struct RunOneCtx<'a> {
    task: &'a RepoTask,
    run: usize,
    base_short: &'a str,
    judge_script: &'a Path,
    pristine: &'a Path,
    ws: &'a Path,
    rollout_root: &'a Path,
}

async fn run_one(ctx: RunOneCtx<'_>, backend: Arc<dyn ModelBackend>) -> (TaskOutcome, u128) {
    let RunOneCtx {
        task,
        run,
        base_short,
        judge_script,
        pristine,
        ws,
        rollout_root,
    } = ctx;
    let mut out = TaskOutcome {
        id: task.id.clone(),
        kind: task.kind.clone(),
        base_commit: base_short.to_string(),
        patch_applied: task.patch.is_none(),
        // 无补丁的题写作 `none`：报告必须能区分「没补丁」与「补丁校验失败」。
        patch_check: if task.patch.is_none() {
            "none"
        } else {
            "pending"
        }
        .to_string(),
        ..Default::default()
    };
    let setup_start = Instant::now();

    if !judge_script.exists() {
        out.invalidate(format!("判据脚本缺失：{}", rel_path(judge_script)));
        out.setup_ms = setup_start.elapsed().as_millis();
        out.detail = clip(&format!("判据脚本缺失：{}", judge_script.display()), 400);
        return (out, 0);
    }

    let (warmup_ms, task_base, warmup_ok) = match prepare_workspace(task, pristine, ws) {
        Ok(triple) => triple,
        Err((reason, patch_applied)) => {
            out.patch_applied = patch_applied;
            out.patch_check = if task.patch.is_some() {
                "failed"
            } else {
                "none"
            }
            .to_string();
            out.setup_ms = setup_start.elapsed().as_millis();
            out.detail = clip(&reason, 400);
            out.invalidate(reason);
            return (out, 0);
        }
    };
    out.patch_applied = true;
    out.patch_check = if task.patch.is_some() { "ok" } else { "none" }.to_string();
    out.warmup_ok = warmup_ok;
    out.setup_ms = setup_start.elapsed().as_millis();

    // 前置红：判据必须先在「注入缺陷」的工作区上失败，否则这道题不区分能力。
    let pre_start = Instant::now();
    match run_judge(judge_script, &task.judge, ws) {
        Err(reason) => {
            out.check_ms += pre_start.elapsed().as_millis();
            out.detail = clip(&reason, 400);
            out.invalidate(reason);
            return (out, warmup_ms);
        }
        Ok(judge) if judge.pass => {
            out.check_ms += pre_start.elapsed().as_millis();
            out.precondition_checked = true;
            out.precondition_red = false;
            out.detail = clip(
                &format!(
                    "前置判据已通过（该题不含缺陷或不区分能力）：{}",
                    judge.detail
                ),
                400,
            );
            out.invalidate("前置判据未红（precondition_red=false）");
            return (out, warmup_ms);
        }
        Ok(_) => {
            out.check_ms += pre_start.elapsed().as_millis();
            out.precondition_checked = true;
            out.precondition_red = true;
        }
    }

    // 生产同一套件：工具面 / 系统提示 / 审批门禁 / workspace-write 沙箱，一项不改。
    let plan_state = Arc::new(PlanState::new());
    let mut router = ToolRouter::new();
    tools::register_builtin(&mut router, plan_state.clone(), true);
    let broker = eval_broker();
    let sink = Arc::new(EvalSink {
        agg: Mutex::new(Agg::default()),
        broker: broker.clone(),
    });
    let session_id = format!("{}-r{run}", task.id);
    let writer = match shaniu_lib::rollout::RolloutWriter::create(
        &rollout_root.join(&task.id),
        &shaniu_lib::rollout::RolloutItem::SessionMeta {
            session_id: session_id.clone(),
            agent_id: session_id.clone(),
            parent_id: None,
            created_at: now_ms(),
            config_snapshot: shaniu_lib::rollout::ConfigSnapshot {
                model: "evals".into(),
                sandbox: "WorkspaceWrite".into(),
                approval: "AllowOnce".into(),
                workspace_root: ws.display().to_string(),
                ..Default::default()
            },
        },
    ) {
        Ok(writer) => Arc::new(writer),
        Err(error) => {
            let reason = format!("档案创建失败（{error}）");
            out.detail = clip(&reason, 400);
            out.invalidate(reason);
            return (out, warmup_ms);
        }
    };
    let agent = Arc::new(AgentLoop {
        backend,
        router: Arc::new(router),
        broker: broker.clone(),
        plan_state,
        workspace_root: ws.to_path_buf(),
        max_turns: task.max_turns,
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
        retry: shaniu_lib::model_client::RetryPolicy::disabled(),
        verify: Default::default(),
    });

    // 任务 prompt 原样使用：不加任何解题提示，工作区事实由生产系统提示给出。
    log_line(&format!(
        "[agent] start task={} timeout={}s max_turns={}",
        task.id, task.timeout_secs, task.max_turns
    ));
    let started = Instant::now();
    let summary = tokio::time::timeout(
        Duration::from_secs(task.timeout_secs),
        agent.run_task(&task.prompt, sink.clone(), CancellationToken::new()),
    )
    .await;
    out.agent_ms = started.elapsed().as_millis();
    let reason = match &summary {
        Ok(summary) => {
            out.turns = Some(summary.turns);
            format!("{:?}", summary.reason)
        }
        // 超时被丢弃：真实跑了多少轮**不知道**，记 None 而不是填上限。
        // 归因层固定为 agent（评测自身/环境），不评价模型判断。
        Err(_) => {
            out.timeout = true;
            out.timeout_layer = Some("agent".into());
            format!("超时被丢弃（>{}s）", task.timeout_secs)
        }
    };
    log_line(&format!(
        "[agent] end task={} ms={} turns={:?} reason={reason}",
        task.id, out.agent_ms, out.turns
    ));

    // 判据一次定成败。
    log_line(&format!("[judge] start task={} check={}", task.id, task.judge));
    let check_start = Instant::now();
    let judge = run_judge(judge_script, &task.judge, ws);
    out.check_ms += check_start.elapsed().as_millis();
    log_line(&format!(
        "[judge] end task={} ms={} pass={} detail={}",
        task.id,
        out.check_ms,
        judge.as_ref().is_ok_and(|judge| judge.pass),
        judge
            .as_ref()
            .map_or_else(|error| error.clone(), |judge| judge.detail.clone())
    ));

    let files = changed_files(ws, &task_base);
    out.files_changed = files.len();
    let hit: Vec<String> = files
        .iter()
        .filter(|file| {
            task.forbid_paths
                .iter()
                .any(|prefix| file.starts_with(&normalize_path(prefix)))
        })
        .cloned()
        .collect();
    out.forbidden_touched = !hit.is_empty();

    let agg = sink.agg.lock().unwrap();
    out.interruptions = agg.approvals;
    out.prompt_attribution = agg.prompts.clone();

    let mut detail = match &judge {
        Ok(judge) if judge.pass => format!("判据通过｜收口={reason}"),
        Ok(judge) => format!("判据未通过｜收口={reason}｜{}", judge.detail),
        Err(error) => format!("判据无法执行：{error}｜收口={reason}"),
    };
    if !hit.is_empty() {
        detail.push_str(&format!("｜命中禁止路径：{}", hit.join(", ")));
    }
    if !agg.errors.is_empty() {
        detail.push_str(&format!("｜错误=[{}]", agg.errors.join(" / ")));
    }
    out.detail = clip(&detail, 400);
    out.success =
        judge.as_ref().is_ok_and(|judge| judge.pass) && !out.forbidden_touched && !out.timeout;
    (out, warmup_ms)
}

// ============================== 子进程隔离（整题硬超时）==============================

/// 父进程传给子进程的最小规格。只传路径与题号，题面仍从任务集读，
/// 避免两边各存一份题面漂移。
#[derive(Serialize, Deserialize)]
struct ChildSpec {
    task_id: String,
    run: usize,
    base_short: String,
    judge_script: String,
    pristine: String,
    ws: String,
    rollout_root: String,
    tasks_file: String,
    outcome: String,
}

fn provider_from_env() -> ProviderConfig {
    ProviderConfig {
        provider: std::env::var("SHANIU_TEST_PROVIDER").unwrap_or_else(|_| "openai-chat".into()),
        base_url: std::env::var("SHANIU_TEST_BASE_URL")
            .unwrap_or_else(|_| "https://api.deepseek.com/v1".into()),
        model: std::env::var("SHANIU_TEST_MODEL").unwrap_or_else(|_| "deepseek-flash".into()),
        reasoning_effort: None,
        headers: Default::default(),
    }
}

fn base_outcome(task: &RepoTask, base_short: &str) -> TaskOutcome {
    TaskOutcome {
        id: task.id.clone(),
        kind: task.kind.clone(),
        base_commit: base_short.to_string(),
        patch_applied: task.patch.is_none(),
        patch_check: if task.patch.is_none() {
            "none"
        } else {
            "pending"
        }
        .to_string(),
        ..Default::default()
    }
}

/// 子进程入口：只跑一题、把 `TaskOutcome` 原子落盘、退出。
/// 不抢锁、不写报告——锁和报告都归父进程。
async fn run_child_mode(spec_path: &str) {
    let raw = std::fs::read_to_string(spec_path).expect("子进程读不到 spec");
    let spec: ChildSpec = serde_json::from_str(&raw).expect("子进程 spec 解析失败");
    let api_key = std::env::var("SHANIU_TEST_API_KEY").expect("子进程缺少 SHANIU_TEST_API_KEY");
    assert_key_sane(&api_key);
    let tasks_raw = std::fs::read_to_string(repo_root().join(&spec.tasks_file))
        .expect("子进程读不到任务集");
    let tasks: RepoTasksFile = serde_json::from_str(&tasks_raw).expect("子进程任务集解析失败");
    let task = tasks
        .tasks
        .into_iter()
        .find(|task| task.id == spec.task_id)
        .expect("子进程找不到题目");
    let backend: Arc<dyn ModelBackend> = Arc::new(TimedBackend {
        inner: Arc::new(ResponsesBackend {
            provider: provider_from_env(),
            api_key,
        }),
        label: format!("{}-r{}", spec.task_id, spec.run),
        seq: AtomicUsize::new(0),
    });
    log_line(&format!(
        "[child] start task={} run={}",
        spec.task_id, spec.run
    ));
    let (mut outcome, warmup_ms) = run_one(
        RunOneCtx {
            task: &task,
            run: spec.run,
            base_short: &spec.base_short,
            judge_script: Path::new(&spec.judge_script),
            pristine: Path::new(&spec.pristine),
            ws: Path::new(&spec.ws),
            rollout_root: Path::new(&spec.rollout_root),
        },
        backend,
    )
    .await;
    outcome.warmup_ms = warmup_ms;
    let path = PathBuf::from(&spec.outcome);
    let tmp = path.with_extension("tmp");
    std::fs::write(
        &tmp,
        serde_json::to_string(&outcome).expect("子进程结果序列化失败"),
    )
    .expect("子进程结果写入失败");
    std::fs::rename(&tmp, &path).expect("子进程结果改名失败");
    log_line(&format!(
        "[child] end task={} run={}",
        spec.task_id, spec.run
    ));
}

/// 父进程侧：拉起子进程跑一题，超过 `hard` 直接 taskkill 进程树。
///
/// 为什么必须落到子进程：`tokio::time::timeout` 只能中断「有 await 点」的 future，
/// 一旦被测代码或工具里出现同步阻塞调用，整个运行器就会跟着卡死（上一轮 7060 就是这样
/// 挂在第 8 题后面，锁一直被占着、报告写不出来）。进程边界是唯一能保证「一题挂住
/// 不拖住整轮」的边界。
async fn run_one_isolated(children_dir: &Path, ctx: RunOneCtx<'_>, hard: Duration) -> TaskOutcome {
    let task = ctx.task;
    let stem = format!("{}-r{}", task.id, ctx.run);
    let spec_path = children_dir.join(format!("{stem}.spec.json"));
    let outcome_path = children_dir.join(format!("{stem}.outcome.json"));
    let stdout_path = children_dir.join(format!("{stem}.child.log"));
    let stderr_path = children_dir.join(format!("{stem}.child.err.log"));
    let rollout_dir = ctx.rollout_root.join(&task.id);
    let rollout_before = count_jsonl(&rollout_dir);

    let spec = ChildSpec {
        task_id: task.id.clone(),
        run: ctx.run,
        base_short: ctx.base_short.to_string(),
        judge_script: ctx.judge_script.display().to_string(),
        pristine: ctx.pristine.display().to_string(),
        ws: ctx.ws.display().to_string(),
        rollout_root: ctx.rollout_root.display().to_string(),
        tasks_file: std::env::var("SHANIU_EVAL_REPO_TASKS")
            .unwrap_or_else(|_| DEFAULT_TASKS_FILE.to_string()),
        outcome: outcome_path.display().to_string(),
    };
    std::fs::write(&spec_path, serde_json::to_string(&spec).expect("spec 序列化失败"))
        .unwrap_or_else(|error| panic!("写子进程 spec 失败：{error}"));

    log_line(&format!(
        "[task] start id={} run={} hard_timeout={}s",
        task.id,
        ctx.run,
        hard.as_secs()
    ));
    let exe = std::env::current_exe().expect("取当前测试可执行文件路径失败");
    let stdout = File::create(&stdout_path).expect("创建子进程 stdout 日志失败");
    let stderr = File::create(&stderr_path).expect("创建子进程 stderr 日志失败");
    let mut command = Command::new(exe);
    command
        .args(["evals_repo_v0", "--exact", "--ignored", "--nocapture"])
        .env("SHANIU_EVAL_CHILD_SPEC", &spec_path)
        .env("SHANIU_EVAL_RUN_LOG", run_log_path())
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    let mut child = command.spawn().expect("拉起子进程失败");
    let child_pid = child.id();

    let started = Instant::now();
    let mut killed = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(error) => {
                log_line(&format!("[task] wait 失败 id={} err={error}", task.id));
                break;
            }
        }
        if started.elapsed() >= hard {
            let kill = Command::new("taskkill")
                .args(["/T", "/F", "/PID", &child_pid.to_string()])
                .output();
            if let Err(error) = kill {
                log_line(&format!(
                    "[task] taskkill 失败 pid={child_pid} err={error}"
                ));
            }
            let _ = child.wait();
            killed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let elapsed = started.elapsed().as_millis();

    let mut out = std::fs::read_to_string(&outcome_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<TaskOutcome>(&raw).ok())
        .unwrap_or_else(|| base_outcome(task, ctx.base_short));
    let rollout_grew = count_jsonl(&rollout_dir) > rollout_before;
    if killed {
        out.timeout = true;
        out.timeout_layer = Some("eval_hard".into());
        out.agent_ms = elapsed;
        out.success = false;
        if rollout_grew {
            // 已有档案 = 前置红已过、agent 真的跑过；只是没能在硬时限内收口。
            out.precondition_checked = true;
            out.precondition_red = true;
            out.detail = clip(
                &format!(
                    "评测侧整题硬超时：子进程在 {}s 后被强杀（超时归因=评测自身/环境；无模型判断结论）｜{}",
                    hard.as_secs(),
                    out.detail
                ),
                400,
            );
        } else {
            out.invalidate(format!(
                "评测侧整题硬超时：{}s 后在准备/判据阶段被强杀（无模型结论）",
                hard.as_secs()
            ));
            out.detail = clip(
                &format!(
                    "{}；子进程日志 {}",
                    out.invalid_reason.clone().unwrap_or_default(),
                    stdout_path.display()
                ),
                400,
            );
        }
    } else if out.detail.is_empty() {
        // 子进程没写出结果：崩溃、被外部杀掉或 libtest 提前退出。
        let err_tail = std::fs::read_to_string(&stderr_path).unwrap_or_default();
        out.precondition_checked = rollout_grew;
        out.precondition_red = rollout_grew;
        out.agent_ms = elapsed;
        out.invalidate(format!(
            "子进程未写出结果（stderr 末尾：{}）",
            tail(&err_tail, 200)
        ));
        out.detail = clip(&format!("子进程日志 {}", stdout_path.display()), 400);
    }
    log_line(&format!(
        "[task] end id={} run={} status={} timeout_layer={} ms={}",
        task.id,
        ctx.run,
        if out.invalid {
            "INVALID"
        } else if out.success {
            "PASS"
        } else {
            "FAIL"
        },
        out.timeout_layer.as_deref().unwrap_or("-"),
        elapsed
    ));
    out
}

// ============================== 报告 ==============================

/// 启动阶段就不可信时：写一份 `valid:false` 的报告再 panic，别让磁盘上没有产物。
fn write_invalid_report(
    reason: &str,
    tasks_file: &str,
    base_commit: &str,
    provider: &ProviderConfig,
    repeats: usize,
    repo: &Value,
    epoch: u64,
) -> PathBuf {
    let report = json!({
        "version": 1,
        "kind": "repo",
        "arm": "repo-baseline",
        "tasks_file": tasks_file,
        "base_commit": base_commit,
        "epoch_ms": epoch,
        "process": { "pid": id() },
        "provider": provider.provider,
        "base_url": provider.base_url,
        "model": provider.model,
        "repeats": repeats,
        "mode": "seq",
        "repo": repo,
        "valid": false,
        "validity": {
            "precondition_red_all": false,
            "patch_applied_all": false,
            "base_commit_pinned": false,
            "snapshot_count_match": false,
        },
        "summary": summarize(&[], 0),
        "by_type": {},
        "by_category": {},
        "timeouts": 0,
        "samples": 0,
        "tasks": [],
        "runs": [],
        "invalid_reason": reason,
    });
    let path = result_path();
    std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    path
}

/// 密钥形态前置校验（AGENTS §4.1：只输出布尔与长度，绝不输出内容）。
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

/// 管线自检：不调用模型，只验证「快照 → 补丁 → 预热 → 判据必须红」这条链路。
/// 任何一题准备失败、判据无法执行、或判据竟然通过（题目不成立），都算自检失败。
#[tokio::test]
#[ignore = "管线自检：不需要密钥；里程碑跑基线前先跑它"]
async fn evals_repo_pipeline_selfcheck() {
    let _lock = EvalRunLock::acquire();
    let root = repo_root();
    let tasks_file =
        std::env::var("SHANIU_EVAL_REPO_TASKS").unwrap_or_else(|_| DEFAULT_TASKS_FILE.to_string());
    let raw = std::fs::read_to_string(root.join(&tasks_file))
        .unwrap_or_else(|e| panic!("任务集 {tasks_file} 读不到：{e}"));
    let spec: RepoTasksFile = serde_json::from_str(&raw).expect("evals/repo-tasks.json 解析失败");
    let base_sha = resolve_base_commit(&root, &spec.base_commit).expect("base_commit 不可解析");
    let short: String = base_sha.chars().take(7).collect();

    let pristine = root
        .join("target")
        .join(format!("evals-repo-selfcheck-pristine-{}", id()));
    std::fs::remove_dir_all(&pristine).ok();
    let files = materialize_snapshot(&root, &base_sha, &pristine).expect("快照物化失败");
    let expected = snapshot_expected_files(&root, &base_sha).expect("ls-tree 失败");
    assert_eq!(files, expected, "快照文件数与 git ls-tree 不一致");

    let ws = workspace_dir("evals-repo-selfcheck-ws");
    std::fs::remove_dir_all(&ws).ok();
    let judge_script = root.join(&spec.judge);
    let mut failures: Vec<String> = Vec::new();

    for task in &spec.tasks {
        let started = Instant::now();
        match prepare_workspace(task, &pristine, &ws) {
            Err((reason, _)) => {
                println!("[selfcheck] {:<34} 准备失败：{reason}", task.id);
                failures.push(format!("{}: 准备失败（{reason}）", task.id));
            }
            Ok((warmup_ms, _, _)) => match run_judge(&judge_script, &task.judge, &ws) {
                Ok(judge) if !judge.pass => println!(
                    "[selfcheck] {:<34} 前置红 OK（常驻 {}ms，预热 {}ms）",
                    task.id,
                    started.elapsed().as_millis(),
                    warmup_ms
                ),
                Ok(_) => {
                    println!("[selfcheck] {:<34} 判据未红：题目不成立", task.id);
                    failures.push(format!("{}: 判据未红（题目不成立）", task.id));
                }
                Err(reason) => {
                    println!("[selfcheck] {:<34} 判据无法执行：{reason}", task.id);
                    failures.push(format!("{}: 判据无法执行（{reason}）", task.id));
                }
            },
        }
    }

    println!("[selfcheck] base={short} 任务数={}", spec.tasks.len());
    assert!(
        failures.is_empty(),
        "管线自检失败（{} 题）：\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[tokio::test]
#[ignore = "真实仓库端到端评测需要真实端点密钥、本机 Rust 工具链与 pwsh，按里程碑/变更回归手动运行"]
async fn evals_repo_v0() {
    // 子进程模式：父进程按题拉起本测试二进制，只跑一题就退出。
    if let Ok(spec_path) = std::env::var("SHANIU_EVAL_CHILD_SPEC") {
        run_child_mode(&spec_path).await;
        return;
    }
    // 父子进程共用同一份运行日志；先固定路径，再抢锁。
    std::env::set_var("SHANIU_EVAL_RUN_LOG", run_log_path());

    // 第一件事就是抢锁：没有锁，后面的任何数字都可能是别人的。
    let _lock = EvalRunLock::acquire();
    let epoch = now_ms();

    // 父进程不直接调用模型，但必须先确认密钥形态合法，别等子进程逐个报错。
    let _api_key = std::env::var("SHANIU_TEST_API_KEY")
        .expect("repo 评测必须配真实端点：设置 SHANIU_TEST_API_KEY（勿把密钥写进仓库）");
    assert_key_sane(&_api_key);
    let provider = provider_from_env();

    let tasks_file =
        std::env::var("SHANIU_EVAL_REPO_TASKS").unwrap_or_else(|_| DEFAULT_TASKS_FILE.to_string());
    let raw = std::fs::read_to_string(repo_root().join(&tasks_file))
        .unwrap_or_else(|e| panic!("任务集 {tasks_file} 读不到：{e}"));
    let spec: RepoTasksFile = serde_json::from_str(&raw).expect("evals/repo-tasks.json 解析失败");
    if spec.tasks.is_empty() {
        panic!("任务集 {tasks_file} 为空");
    }
    let repeats = std::env::var("SHANIU_EVAL_REPEATS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_REPEATS);
    let judge_script = repo_root().join(&spec.judge);

    let root = repo_root();
    // 快照固定在 base_commit，不跟随 HEAD。
    let base_sha = match resolve_base_commit(&root, &spec.base_commit) {
        Ok(sha) => sha,
        Err(reason) => {
            let path = write_invalid_report(
                &reason,
                &tasks_file,
                &spec.base_commit,
                &provider,
                repeats,
                &json!({
                    "snapshot_files": 0,
                    "pristine": "",
                    "workspace": "",
                    "warmup_ms_total": 0,
                }),
                epoch,
            );
            panic!("{reason}（已写无效报告 {}）", path.display());
        }
    };
    let base_short: String = base_sha.chars().take(7).collect();

    let pristine = root
        .join("target")
        .join(format!("evals-repo-pristine-{}", id()));
    std::fs::remove_dir_all(&pristine).ok();
    let snapshot_files = match materialize_snapshot(&root, &base_sha, &pristine) {
        Ok(count) => count,
        Err(reason) => {
            let path = write_invalid_report(
                &reason,
                &tasks_file,
                &base_sha,
                &provider,
                repeats,
                &json!({
                    "snapshot_files": 0,
                    "pristine": rel_path(&pristine),
                    "workspace": "",
                    "warmup_ms_total": 0,
                }),
                epoch,
            );
            panic!("{reason}（已写无效报告 {}）", path.display());
        }
    };
    let expected_files = snapshot_expected_files(&root, &base_sha).unwrap_or_else(|reason| {
        let path = write_invalid_report(
            &reason,
            &tasks_file,
            &base_sha,
            &provider,
            repeats,
            &json!({
                "snapshot_files": snapshot_files,
                "pristine": rel_path(&pristine),
                "workspace": "",
                "warmup_ms_total": 0,
            }),
            epoch,
        );
        panic!("{reason}（已写无效报告 {}）", path.display());
    });
    if snapshot_files != expected_files {
        let reason = format!(
            "快照文件数不一致：物化 {snapshot_files}，git ls-tree {expected_files} \
             （checkout-index 有文件未落盘，报告作废）"
        );
        let path = write_invalid_report(
            &reason,
            &tasks_file,
            &base_sha,
            &provider,
            repeats,
            &json!({
                "snapshot_files": snapshot_files,
                "pristine": rel_path(&pristine),
                "workspace": "",
                "warmup_ms_total": 0,
            }),
            epoch,
        );
        panic!("{reason}（已写无效报告 {}）", path.display());
    }

    // 工作区同一路径跨题复用：cargo 增量才有意义。每题由 robocopy 还原。
    let ws = workspace_dir("evals-repo-ws");
    std::fs::remove_dir_all(&ws).ok();
    let rollout_root = reset_rollout_dir();
    let max_turns = spec
        .tasks
        .iter()
        .map(|task| task.max_turns)
        .max()
        .unwrap_or(0);

    let mut all_runs: Vec<Vec<TaskOutcome>> = Vec::new();
    let mut run_values: Vec<Value> = Vec::new();
    let mut warmup_ms_total: u128 = 0;
    let mut attempted: usize = 0;

    let children_dir = root
        .join("target")
        .join(format!("evals-repo-children-{}", id()));
    std::fs::remove_dir_all(&children_dir).ok();
    std::fs::create_dir_all(&children_dir).expect("创建子进程工作目录失败");
    let hard_extra = task_hard_extra();
    // 压测/演练用：直接钉死整题硬超时（不设则 = 题面 timeout_secs + 余量）。
    let hard_override = std::env::var("SHANIU_EVAL_TASK_HARD_SECS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs);
    if let Some(hard) = hard_override {
        log_line(&format!(
            "整题硬超时被 SHANIU_EVAL_TASK_HARD_SECS 钉死为 {}s",
            hard.as_secs()
        ));
    }

    for run in 1..=repeats {
        let mut outcomes: Vec<TaskOutcome> = Vec::new();
        for task in &spec.tasks {
            let outcome = run_one_isolated(
                &children_dir,
                RunOneCtx {
                    task,
                    run,
                    base_short: &base_short,
                    judge_script: &judge_script,
                    pristine: &pristine,
                    ws: &ws,
                    rollout_root: &rollout_root,
                },
                hard_override.unwrap_or_else(|| Duration::from_secs(task.timeout_secs) + hard_extra),
            )
            .await;
            warmup_ms_total += outcome.warmup_ms;
            // 「尝试过」= 真的把 agent 跑起来了；准备/前置阶段失败的题没有档案，不能算样本。
            if outcome.precondition_checked {
                attempted += 1;
            }
            log_line(&format!(
                "run {run}/{repeats} {:<34} {} turns={} 打断={} 耗时={}ms files={} setup={}ms check={}ms 超时层={} {}｜{}",
                outcome.id,
                if outcome.invalid {
                    "INVALID"
                } else if outcome.success {
                    "PASS"
                } else {
                    "FAIL"
                },
                outcome
                    .turns
                    .map_or_else(|| "unknown".to_string(), |turns| turns.to_string()),
                outcome.interruptions,
                outcome.agent_ms,
                outcome.files_changed,
                outcome.setup_ms,
                outcome.check_ms,
                outcome.timeout_layer.as_deref().unwrap_or("-"),
                outcome.detail,
                task.title,
            ));
            outcomes.push(outcome);
        }
        let summary = summarize(&outcomes, max_turns);
        let run_timeouts = outcomes.iter().filter(|task| task.timeout).count();
        log_line(&format!(
            "run {run}/{repeats} 小计 成功率={}/{}（invalid={}），轮次中位={}，打断合计={}，超时={}",
            summary["passed"].as_u64().unwrap_or(0),
            summary["n"].as_u64().unwrap_or(0),
            summary["invalid"].as_u64().unwrap_or(0),
            summary["median_turns"]
                .as_f64()
                .map_or_else(|| "unknown".to_string(), |value| format!("{value:.2}")),
            summary["interruptions"]["total"].as_u64().unwrap_or(0),
            run_timeouts,
        ));
        run_values.push(json!({
            "run": run,
            "summary": summary,
            "tasks": outcomes.iter().map(TaskOutcome::to_json).collect::<Vec<_>>(),
        }));
        all_runs.push(outcomes);
    }

    // ---------- 顶层汇总：各轮中位数（成功率/轮次/耗时），与基线同一口径 ----------
    let first = &all_runs[0];
    let n = first.iter().filter(|task| !task.invalid).count();
    let invalid = first.len() - n;
    let success_rate =
        median_option(&run_numbers(&run_values, &["summary", "success_rate"])).unwrap_or(0.0);
    let passed = (success_rate * n as f64).round() as usize;
    let pooled_valid: Vec<&TaskOutcome> = all_runs
        .iter()
        .flat_map(|run| run.iter())
        .filter(|task| !task.invalid)
        .collect();
    let timeouts: usize = pooled_valid.iter().filter(|task| task.timeout).count();
    let summary = json!({
        "n": n,
        "invalid": invalid,
        "passed": passed,
        "success_rate": success_rate,
        "success_rate_ci95": wilson(passed, n),
        "success_rate_no_timeout": median_option(&run_numbers(
            &run_values,
            &["summary", "success_rate_no_timeout"],
        ))
        .unwrap_or(0.0),
        // 分母本来就是有效题，这里与 success_rate 同值（显式保留，避免读取方猜口径）。
        "success_rate_valid_only": success_rate,
        "avg_turns": median_option(&run_numbers(&run_values, &["summary", "avg_turns"])),
        "median_turns": median_option(&run_numbers(&run_values, &["summary", "median_turns"])),
        "p90_turns": median_option(&run_numbers(&run_values, &["summary", "p90_turns"])),
        "turns_unknown": median_option(&run_numbers(&run_values, &["summary", "turns_unknown"]))
            .unwrap_or(0.0)
            .round() as usize,
        "max_turns": max_turns,
        // 打断统计跨轮汇总（每题的打断次数是独立观测，pooled 比「中位数之中位数」更可解释）。
        "interruptions": interruptions_stats(&pooled_valid),
        "timeout_rate": median_option(&run_numbers(&run_values, &["summary", "timeout_rate"]))
            .unwrap_or(0.0),
        "duration": {
            "agent_ms_p50": median_option(&run_numbers(
                &run_values,
                &["summary", "duration", "agent_ms_p50"],
            )),
            "agent_ms_p90": median_option(&run_numbers(
                &run_values,
                &["summary", "duration", "agent_ms_p90"],
            )),
            "agent_ms_total": run_numbers(&run_values, &["summary", "duration", "agent_ms_total"])
                .iter()
                .sum::<f64>() as u64,
            "setup_ms_total": run_numbers(&run_values, &["summary", "duration", "setup_ms_total"])
                .iter()
                .sum::<f64>() as u64,
            "check_ms_total": run_numbers(&run_values, &["summary", "duration", "check_ms_total"])
                .iter()
                .sum::<f64>() as u64,
        },
    });

    // ---------- 分组：by_type（有效题）/ by_category（含 invalid 归入 infra）----------
    let mut by_type: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut by_category: BTreeMap<String, (usize, usize, usize, usize)> = BTreeMap::new();
    for task in first {
        if !task.invalid {
            let entry = by_type.entry(task.kind.clone()).or_insert((0, 0));
            entry.1 += 1;
            if task.success {
                entry.0 += 1;
            }
        }
        let entry = by_category.entry(task.kind.clone()).or_insert((0, 0, 0, 0));
        entry.0 += 1;
        if task.success {
            entry.1 += 1;
        }
        if task.timeout {
            entry.2 += 1;
        }
        // infra = 判据/补丁/预热/档案这类环境问题，与模型能力无关。
        if task.invalid {
            entry.3 += 1;
        }
    }
    let by_type = by_type
        .into_iter()
        .map(|(kind, (passed, n))| {
            (
                kind,
                json!({ "n": n, "passed": passed, "success_rate": rate(passed, n) }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let by_category = by_category
        .into_iter()
        .map(|(kind, (n, passed, timeouts, infra))| {
            (
                kind,
                json!({ "n": n, "passed": passed, "timeouts": timeouts, "infra": infra }),
            )
        })
        .collect::<serde_json::Map<_, _>>();

    // ---------- 自证 ----------
    let actual_sessions = count_jsonl(&rollout_root);
    let samples_match = attempted == actual_sessions;
    let precondition_red_all = all_runs
        .iter()
        .flatten()
        .filter(|task| task.precondition_checked)
        .all(|task| task.precondition_red);
    let patch_applied_all = all_runs.iter().flatten().all(|task| task.patch_applied);
    let snapshot_count_match = snapshot_files == expected_files;
    let invalid_total: usize = all_runs
        .iter()
        .flatten()
        .filter(|task| task.invalid)
        .count();
    let validity = json!({
        "precondition_red_all": precondition_red_all,
        "patch_applied_all": patch_applied_all,
        "base_commit_pinned": true,
        "snapshot_count_match": snapshot_count_match,
    });
    let valid = precondition_red_all
        && patch_applied_all
        && snapshot_count_match
        && samples_match
        && invalid_total == 0;

    let report = json!({
        "version": spec.version,
        "kind": "repo",
        "arm": "repo-baseline",
        "tasks_file": tasks_file,
        "base_commit": base_sha,
        "epoch_ms": epoch,
        "process": { "pid": id() },
        "provider": provider.provider,
        "base_url": provider.base_url,
        "model": provider.model,
        "repeats": repeats,
        "mode": "seq",
        "repo": {
            "snapshot_files": snapshot_files,
            "pristine": rel_path(&pristine),
            "workspace": rel_path(&ws),
            "warmup_ms_total": warmup_ms_total as u64,
        },
        "valid": valid,
        "validity": validity,
        "summary": summary,
        "by_type": by_type,
        "by_category": by_category,
        "timeouts": timeouts,
        "samples": actual_sessions,
        "tasks": first.iter().map(TaskOutcome::to_json).collect::<Vec<_>>(),
        "runs": run_values,
    });

    let out_path = result_path();
    std::fs::write(&out_path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    println!(
        "[repo-eval] 汇总（{repeats} 轮中位数）成功率={:.0}%（Wilson95 {:.0}%–{:.0}%），轮次中位={}，打断={}，超时={}/{}",
        summary["success_rate"].as_f64().unwrap_or(0.0) * 100.0,
        summary["success_rate_ci95"][0].as_f64().unwrap_or(0.0) * 100.0,
        summary["success_rate_ci95"][1].as_f64().unwrap_or(0.0) * 100.0,
        summary["median_turns"]
            .as_f64()
            .map_or_else(|| "unknown".to_string(), |value| format!("{value:.2}")),
        summary["interruptions"]["total"].as_u64().unwrap_or(0),
        timeouts,
        n,
    );
    println!(
        "[repo-eval] 样本校验：尝试 {attempted} 份档案，实际 {actual_sessions} 份 → {}；invalid={invalid_total}",
        if samples_match { "一致" } else { "不一致（valid:false）" }
    );
    if !valid {
        println!("[repo-eval] 警告：本份报告 valid=false，禁止据此下结论（见 validity 与 tasks[].invalid_reason）");
    }
    println!(
        "[repo-eval] 报告已写入 {}\n[repo-eval] 阅读：node evals/score-repo.mjs {}",
        out_path.display(),
        out_path.display()
    );
}
