//! 写后验证闸门（design.md / spec.md）。
//!
//! 只回答一个问题：**模型准备收尾时，工作区的源码改动有没有被宿主验证过**。
//! 它不按每次工具调用触发，也不接受模型自报的「我跑过了」——命令来自配置或
//! 按脏文件就近探测，由宿主执行并判定。
//!
//! 四态判定是**封闭**的：`Passed / Failed / Inconclusive(原因) / Skipped(原因)`。
//! 凡未命中任何具名 `Inconclusive` 原因的失败，一律按 `Failed` 处理（fail-closed）。
//! 原因码定义在 `events.rs`，前端与导出共用同一份，避免两处各说一套。

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::config::{BaselineMode, VerifyConfig};
use crate::events::{InconclusiveReason, SkipReason, VerifyState};
use crate::tool_router::FileWrite;

/// 一次验证的总时限（跨命令）。单命令另受 `[verify].timeout_ms` 限制。
const TOTAL_BUDGET: Duration = Duration::from_secs(180);
/// 回填给模型的输出尾部长度。
const SUMMARY_BYTES: usize = 800;

/// 源码类扩展名：只有这些改动才值得触发编译/类型检查。
fn is_source_extension(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref(),
        Some("rs" | "ts" | "tsx" | "js" | "jsx" | "json" | "toml")
    )
}

/// 一次「脏」判定：有源码写入且内容真的变了。
#[derive(Default)]
pub struct DirtyTracker {
    files: BTreeSet<PathBuf>,
}

impl DirtyTracker {
    /// 吸收一次写日志。返回是否有**新的**源码改动。
    pub fn mark(&mut self, writes: &[FileWrite]) -> bool {
        let mut newly = false;
        for write in writes.iter().filter(|write| write.changed) {
            if !is_source_extension(&write.path) {
                continue;
            }
            if self.files.insert(write.path.clone()) {
                newly = true;
            }
        }
        newly
    }

    pub fn is_dirty(&self) -> bool {
        !self.files.is_empty()
    }

    /// 这批写入里有没有「内容真的变了的源码写入」。
    ///
    /// 与 [`DirtyTracker::mark`] 的区别：`mark` 只报告**新**脏文件，这里报告
    /// 「脏状态可能需要重判」。同一文件被改第二次（模型修复）时 `mark` 返回 false，
    /// 但验证结果必须重新取——否则一次失败会把整个任务钉死在 `unverified`。
    pub fn has_source_change(writes: &[FileWrite]) -> bool {
        writes
            .iter()
            .any(|write| write.changed && is_source_extension(&write.path))
    }

    pub fn clear(&mut self) {
        self.files.clear();
    }

    pub fn files(&self) -> impl Iterator<Item = &PathBuf> {
        self.files.iter()
    }
}

/// 「任务开始前就已经红」的基线：首次置脏之前观测到的同命令失败。
#[derive(Default)]
pub struct BaselineCache {
    seen: HashMap<String, String>,
}

impl BaselineCache {
    /// 首次置脏**之前**记录一次命令失败。调用方负责只在 clean 阶段调用。
    pub fn note_failure(&mut self, command: &str, output: &str) {
        let command = command.trim();
        if command.is_empty() || self.seen.contains_key(command) {
            return;
        }
        self.seen.insert(command.to_string(), signature_of(output));
    }

    /// 当前失败是否与基线一致。命令必须逐字相同：模糊匹配会让「改坏了」被当成「本来就红」。
    pub fn hit(&self, command: &str, output: &str) -> bool {
        self.seen
            .get(command.trim())
            .is_some_and(|baseline| baseline == &signature_of(output))
    }
}

/// 失败签名：与自愈闭环**同一个函数**（判定标准只能有一套）。
fn signature_of(output: &str) -> String {
    crate::agent_loop::repair::signature_of(output)
}

/// 一条待执行的验证命令。`display` 同时用于审计事件与失败签名比对的键。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyCommand {
    pub display: String,
    pub cwd: PathBuf,
}

/// 一次验证的完整结果（事件 + 自愈 + 前端四态的唯一来源）。
#[derive(Debug, Clone)]
pub struct VerifyOutcome {
    pub state: VerifyState,
    pub commands: Vec<String>,
    pub duration_ms: u64,
    pub summary: String,
    pub diagnostics: Vec<String>,
    /// 原始输出（不序列化）：只给 `observe_verification` 提取诊断用。
    pub output: String,
}

/// 交给结束前否决点（[`crate::agent_loop::StopHook`]）的验证结论快照。
///
/// 字段名与 `verification_result` 事件逐字对齐（`turn` / `state` / `commands` /
/// `summary` / `diagnostics`），再加一个 `vetoed`：不新造第二套术语，钩子侧读到的
/// 就是前端事件里看到的那一份。
///
/// `vetoed` 不在 [`VerifyOutcome`] 里：额度是宿主账本的事实（否决过一次之后钩子
/// 仍会拿到同一份结论），所以由构造快照时填写。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerifySnapshot {
    /// 裁决所属轮次；0 = 本任务还没有可验证的源码写入（合成的 `Skipped`）。
    pub turn: u32,
    pub state: VerifyState,
    pub commands: Vec<String>,
    pub summary: String,
    pub diagnostics: Vec<String>,
    /// 是否已因验证失败被否决过（`VetoLedger::verify_used`）。钩子据此避免重复要同一票。
    pub vetoed: bool,
}

impl VerifySnapshot {
    pub fn from_outcome(outcome: &VerifyOutcome, turn: u32) -> Self {
        Self {
            turn,
            state: outcome.state,
            commands: outcome.commands.clone(),
            summary: outcome.summary.clone(),
            diagnostics: outcome.diagnostics.clone(),
            vetoed: false,
        }
    }

    /// 没有任何源码写入时的合成裁决：`Skipped(NoSourceWrite)` 本来就是这个事实，
    /// 直接给四态里的那一态，别让钩子去猜「没有快照」是什么意思。
    pub fn no_source_write() -> Self {
        Self {
            turn: 0,
            state: VerifyState::Skipped(SkipReason::NoSourceWrite),
            commands: Vec::new(),
            summary: String::new(),
            diagnostics: Vec::new(),
            vetoed: false,
        }
    }

    /// 同一份结论 + 当前额度状态。额度会随否决推进，快照本身不会变。
    pub fn with_vetoed(mut self, vetoed: bool) -> Self {
        self.vetoed = vetoed;
        self
    }
}

/// 选命令。优先级：显式配置 > 按脏文件就近清单 > `Skipped`。
pub fn select_commands(
    cfg: &VerifyConfig,
    dirty: &DirtyTracker,
    workspace_root: &Path,
) -> Result<Vec<VerifyCommand>, SkipReason> {
    let configured = cfg.normalized_commands();
    if !configured.is_empty() {
        return Ok(configured
            .into_iter()
            .map(|display| VerifyCommand { display, cwd: workspace_root.to_path_buf() })
            .collect());
    }

    let mut result: Vec<VerifyCommand> = Vec::new();
    for file in dirty.files() {
        let Some(command) = nearest_command(file, workspace_root) else { continue };
        if result
            .iter()
            .any(|kept| kept.display == command.display && kept.cwd == command.cwd)
        {
            continue;
        }
        result.push(command);
        if result.len() >= crate::config::MAX_VERIFY_COMMANDS {
            break;
        }
    }
    if result.is_empty() {
        Err(SkipReason::NoCommandConfigured)
    } else {
        Ok(result)
    }
}

/// 从脏文件向上找最近的清单；找不到归属清单就跳过该文件（**不回落到仓库根**）。
fn nearest_command(file: &Path, workspace_root: &Path) -> Option<VerifyCommand> {
    let mut dir = file.parent()?;
    loop {
        if dir.join("Cargo.toml").is_file() {
            let workspace = std::fs::read_to_string(dir.join("Cargo.toml"))
                .map(|text| text.contains("[workspace]"))
                .unwrap_or(false);
            let display = if workspace {
                "cargo check --workspace --all-targets".to_string()
            } else {
                "cargo check --all-targets".to_string()
            };
            return Some(VerifyCommand { display, cwd: dir.to_path_buf() });
        }
        if dir.join("package.json").is_file() {
            if let Some(display) = package_command(dir) {
                return Some(VerifyCommand { display, cwd: dir.to_path_buf() });
            }
        }
        if dir == workspace_root || !dir.starts_with(workspace_root) {
            return None;
        }
        dir = dir.parent()?;
    }
}

/// package.json 的命令：只认 `scripts.typecheck`，其次 `tsconfig.json` + `npx tsc`。
/// 不猜包管理器——锁文件决定命令，没有锁文件按 npm。
fn package_command(dir: &Path) -> Option<String> {
    let manifest = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&manifest).ok()?;
    if json.pointer("/scripts/typecheck").and_then(|v| v.as_str()).is_some() {
        let runner = if dir.join("pnpm-lock.yaml").is_file() {
            "pnpm typecheck"
        } else if dir.join("yarn.lock").is_file() {
            "yarn typecheck"
        } else if dir.join("bun.lockb").is_file() {
            "bun run typecheck"
        } else {
            "npm run typecheck"
        };
        return Some(runner.to_string());
    }
    if dir.join("tsconfig.json").is_file() {
        return Some("npx tsc --noEmit".to_string());
    }
    None
}

/// 四态判定。顺序固定：超时 → 命令不存在 → 既有红 → 环境失败 → 其余一律 `Failed`。
///
/// 这个顺序就是「封闭原因码」的实现：没有任何 `_ => Inconclusive` 的兜底分支。
pub(crate) fn classify(run: &crate::sandbox::capture::CapturedRun, baseline_hit: bool) -> VerifyState {
    if run.exit_code == 0 && !run.timed_out {
        return VerifyState::Passed;
    }
    if run.timed_out {
        return VerifyState::Inconclusive(InconclusiveReason::Timeout);
    }
    if is_command_missing(&run.output) {
        return VerifyState::Inconclusive(InconclusiveReason::CommandMissing);
    }
    if baseline_hit {
        return VerifyState::Inconclusive(InconclusiveReason::PreExistingRed);
    }
    if is_env_failure(&run.output) {
        return VerifyState::Inconclusive(InconclusiveReason::EnvFailure);
    }
    VerifyState::Failed
}

fn is_command_missing(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("is not recognized as an internal or external command")
        || lower.contains("is not recognized as the name of a cmdlet")
        || lower.contains("command not found")
        || output.contains("不是内部或外部命令")
}

fn is_env_failure(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    const PATTERNS: &[&str] = &[
        "could not resolve host",
        "failed to connect",
        "connection refused",
        "connection timed out",
        "network is unreachable",
        "failed to download",
        "no space left on device",
        "disk full",
        "permission denied (os error 13)",
        "os error 112",
        "os error 5",
        "拒绝访问",
        "不是磁盘",
    ];
    PATTERNS.iter().any(|pattern| lower.contains(pattern))
}

/// 执行一组命令并聚合判定。取消时立刻返回 `Failed`（回合本来就要中止）。
pub async fn run_commands(
    commands: &[VerifyCommand],
    timeout: Duration,
    workspace_root: &Path,
    sandbox: crate::config::SandboxMode,
    cancel: &CancellationToken,
    baseline: Option<&BaselineCache>,
) -> VerifyOutcome {
    let started = Instant::now();
    let mut combined = String::new();
    let mut state = VerifyState::Passed;
    let mut duration_ms = 0u64;

    for command in commands {
        let elapsed = started.elapsed();
        if elapsed >= TOTAL_BUDGET {
            state = merge_state(state, VerifyState::Inconclusive(InconclusiveReason::Timeout));
            combined.push_str(&format!("\n[{}] 总时限已到，未执行\n", command.display));
            continue;
        }
        let remaining = TOTAL_BUDGET.saturating_sub(elapsed).min(timeout);
        let result = crate::sandbox::capture::run_capture(
            sandbox,
            workspace_root,
            &command.display,
            &command.cwd,
            remaining,
            cancel,
        )
        .await;
        match result {
            Ok(run) => {
                duration_ms += run.duration_ms;
                let baseline_hit = baseline
                    .is_some_and(|cache| cache.hit(&command.display, &run.output));
                let current = classify(&run, baseline_hit);
                state = merge_state(state, current);
                if !run.output.trim().is_empty() {
                    combined.push_str(&format!("$ {}\n{}\n", command.display, run.output));
                }
            }
            Err(crate::sandbox::capture::CaptureError::Cancelled) => {
                state = merge_state(state, VerifyState::Failed);
                combined.push_str(&format!("\n[{}] 被取消\n", command.display));
            }
            Err(crate::sandbox::capture::CaptureError::Spawn(message))
            | Err(crate::sandbox::capture::CaptureError::Denied(message))
            | Err(crate::sandbox::capture::CaptureError::Wait(message)) => {
                // 沙箱拒绝/进程起不来不是「代码错了」，但也不能当成通过：
                // 归入环境失败这个具名原因，由它承担「不清脏、不否决」的语义。
                state = merge_state(state, VerifyState::Inconclusive(InconclusiveReason::EnvFailure));
                combined.push_str(&format!("\n[{}] {message}\n", command.display));
            }
        }
    }

    let diagnostics = crate::agent_loop::repair::extract_diagnostics(&combined)
        .into_iter()
        .map(|d| format!("{} {}", d.location, d.message))
        .collect::<Vec<_>>();
    VerifyOutcome {
        state,
        commands: commands.iter().map(|c| c.display.clone()).collect(),
        duration_ms,
        summary: tail(&combined),
        diagnostics,
        output: combined,
    }
}

/// 聚合多条命令：`Failed` 永远赢（fail-closed）。
fn merge_state(current: VerifyState, next: VerifyState) -> VerifyState {
    match (current, next) {
        (VerifyState::Failed, _) | (_, VerifyState::Failed) => VerifyState::Failed,
        (VerifyState::Inconclusive(InconclusiveReason::Timeout), _)
        | (_, VerifyState::Inconclusive(InconclusiveReason::Timeout)) => {
            VerifyState::Inconclusive(InconclusiveReason::Timeout)
        }
        (VerifyState::Inconclusive(InconclusiveReason::CommandMissing), _)
        | (_, VerifyState::Inconclusive(InconclusiveReason::CommandMissing)) => {
            VerifyState::Inconclusive(InconclusiveReason::CommandMissing)
        }
        (VerifyState::Inconclusive(InconclusiveReason::PreExistingRed), _)
        | (_, VerifyState::Inconclusive(InconclusiveReason::PreExistingRed)) => {
            VerifyState::Inconclusive(InconclusiveReason::PreExistingRed)
        }
        (VerifyState::Inconclusive(InconclusiveReason::EnvFailure), _)
        | (_, VerifyState::Inconclusive(InconclusiveReason::EnvFailure)) => {
            VerifyState::Inconclusive(InconclusiveReason::EnvFailure)
        }
        (VerifyState::Skipped(_), other) | (other, VerifyState::Skipped(_)) => other,
        (VerifyState::Passed, VerifyState::Passed) => VerifyState::Passed,
    }
}

fn tail(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.len() <= SUMMARY_BYTES {
        return trimmed.to_string();
    }
    let mut start = trimmed.len() - SUMMARY_BYTES;
    while start < trimmed.len() && !trimmed.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &trimmed[start..])
}

/// 否决时注入给模型的提醒。命令原文与诊断都必须带上：
/// 不给命令，模型无从复现；不给诊断，它就只会再猜一遍。
pub fn reminder_text(outcome: &VerifyOutcome, repair_note: Option<&str>) -> String {
    let mut text = String::from("宿主验证未通过（写后闸门）：本回合有源码写入，但收尾前验证失败。");
    if !outcome.commands.is_empty() {
        text.push_str(&format!("\n执行命令：{}", outcome.commands.join("；")));
    }
    if !outcome.diagnostics.is_empty() {
        text.push_str("\n错误定位：");
        for diagnostic in &outcome.diagnostics {
            text.push_str(&format!("\n- {diagnostic}"));
        }
    } else if !outcome.summary.is_empty() {
        text.push_str("\n输出尾部：");
        for line in outcome.summary.lines().rev().take(8).collect::<Vec<_>>().into_iter().rev() {
            text.push_str(&format!("\n- {line}"));
        }
    }
    if let Some(note) = repair_note {
        text.push_str(&format!("\n{note}"));
    }
    text.push_str(
        "\n请修复后重新收尾。不要用 `echo`、`--no-verify` 或扩大断言范围来绕过验证；\
         若确实是环境问题，请如实说明。",
    );
    text
}

/// 从命令类工具的参数里取出命令原文（基线只认逐字相同的命令）。
pub fn tool_command(tool: &str, arguments: &str) -> Option<String> {
    if !crate::agent_loop::repair::is_command_tool(tool) {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(arguments).ok()?;
    let command = value.get("command")?.as_str()?.trim();
    (!command.is_empty()).then(|| command.to_string())
}

/// 基线开关：`off` 时不要采集、不要命中。
pub fn baseline_enabled(cfg: &VerifyConfig) -> bool {
    matches!(cfg.baseline, BaselineMode::Auto)
}

/// 终端里是否已经**按原样**跑过全部配置命令并通过。
///
/// 只收配置命令（探测出来的命令不认）：探测是宿主自己选的，模型不可能「原样」敲过；
/// 而配置命令是用户写死的，模型照着跑一遍是真实证据。
///
/// `epoch` 之后的通过才算数——改了代码就得重新验，见 [`crate::tool_router::VerifyPassLog`]。
pub fn observed_pass(
    cfg: &VerifyConfig,
    log: &crate::tool_router::VerifyPassLog,
    epoch: u64,
) -> Option<Vec<String>> {
    let commands = cfg.normalized_commands();
    if commands.is_empty() {
        return None;
    }
    commands
        .iter()
        .all(|command| log.passed_at(epoch, command))
        .then_some(commands)
}

/// 一次裁决的输入。收成结构体，是为了让「通过计入 → 显式/探测命令 → Skipped」
/// 这套顺序只有一个实现：循环内收工与统一收口两处必须用同一套判定标准，
/// 各拼一遍迟早会把顺序写反（例如先探测再计入，模型刚跑过也会被重跑）。
pub struct VerifyRequest<'a> {
    pub cfg: &'a VerifyConfig,
    pub dirty: &'a DirtyTracker,
    pub passes: &'a crate::tool_router::VerifyPassLog,
    pub baseline: Option<&'a BaselineCache>,
    pub workspace_root: &'a Path,
    /// 脏状态代次：只有在这一代之后跑过的通过才算数。
    pub epoch: u64,
    pub timeout: Duration,
    pub sandbox: crate::config::SandboxMode,
}

/// 按现有机制裁决一次：终端通过计入优先，其次显式/就近探测命令，最后 `Skipped`。
pub async fn evaluate(request: &VerifyRequest<'_>, cancel: &CancellationToken) -> VerifyOutcome {
    if let Some(commands) = observed_pass(request.cfg, request.passes, request.epoch) {
        return VerifyOutcome {
            state: VerifyState::Passed,
            commands,
            duration_ms: 0,
            summary: "终端里已按原样跑过同一条验证命令并通过".into(),
            diagnostics: Vec::new(),
            output: String::new(),
        };
    }
    match select_commands(request.cfg, request.dirty, request.workspace_root) {
        Ok(commands) => {
            run_commands(
                &commands,
                request.timeout,
                request.workspace_root,
                request.sandbox,
                cancel,
                request.baseline,
            )
            .await
        }
        Err(reason) => VerifyOutcome {
            state: VerifyState::Skipped(reason),
            commands: Vec::new(),
            duration_ms: 0,
            summary: String::new(),
            diagnostics: Vec::new(),
            output: String::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_router::FileWrite;
    use std::path::PathBuf;

    fn write(path: &str, changed: bool) -> FileWrite {
        FileWrite {
            path: PathBuf::from(path),
            changed,
            assertion_delta: 0,
            assertions_touched: false,
            weakened: false,
        }
    }

    #[test]
    fn only_source_extensions_mark_dirty() {
        let mut dirty = DirtyTracker::default();
        assert!(!dirty.mark(&[write("a.md", true)]));
        assert!(!dirty.is_dirty());
        assert!(dirty.mark(&[write("a.rs", true)]));
        assert!(dirty.is_dirty());
        // 内容没变 = 空操作，不置脏
        let mut other = DirtyTracker::default();
        assert!(!other.mark(&[write("a.rs", false)]));
        assert!(!other.is_dirty());
    }

    #[test]
    fn explicit_commands_win_and_dedupe() {
        let cfg = VerifyConfig {
            commands: vec!["  cargo check  ".into(), "cargo check".into(), String::new()],
            ..Default::default()
        };
        let dirty = DirtyTracker::default();
        let commands = select_commands(&cfg, &dirty, Path::new(".")).unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].display, "cargo check");
    }

    #[test]
    fn observed_pass_requires_every_command_and_fresh_epoch() {
        let cfg = VerifyConfig {
            commands: vec!["cargo test".into(), "cargo clippy".into()],
            ..Default::default()
        };
        let log = crate::tool_router::VerifyPassLog::new();
        // 只跑了一条：不认。
        log.record_pass("cargo test");
        assert_eq!(observed_pass(&cfg, &log, 0), None);
        // 两条都跑过：认。
        log.record_pass("  cargo clippy  ");
        assert_eq!(observed_pass(&cfg, &log, 0), Some(cfg.commands.clone()));
        // 之后又改了代码（代次推进）：旧通过失效。
        log.advance();
        assert_eq!(observed_pass(&cfg, &log, 1), None);
        // 没有配置命令时一律不认（探测命令不算模型跑过）。
        assert_eq!(observed_pass(&VerifyConfig::default(), &log, 0), None);
    }

    #[test]
    fn no_command_is_skipped_not_guessed() {
        let mut dirty = DirtyTracker::default();
        dirty.mark(&[write("no-manifest/x.rs", true)]);
        let result = select_commands(&VerifyConfig::default(), &dirty, Path::new("/definitely/not/here"));
        assert_eq!(result, Err(SkipReason::NoCommandConfigured));
    }

    #[test]
    fn classify_is_closed_and_fail_closed() {
        let failed = crate::sandbox::capture::CapturedRun {
            exit_code: 1,
            timed_out: false,
            output: "error[E0308]: mismatched types\n --> src/main.rs:3:5".into(),
            duration_ms: 10,
        };
        assert_eq!(classify(&failed, false), VerifyState::Failed);

        let timed_out = crate::sandbox::capture::CapturedRun {
            exit_code: -1,
            timed_out: true,
            output: String::new(),
            duration_ms: 10,
        };
        assert_eq!(
            classify(&timed_out, false),
            VerifyState::Inconclusive(InconclusiveReason::Timeout)
        );

        let missing = crate::sandbox::capture::CapturedRun {
            exit_code: 1,
            timed_out: false,
            output: "'cargo' 不是内部或外部命令，也不是可运行的程序".into(),
            duration_ms: 10,
        };
        assert_eq!(
            classify(&missing, false),
            VerifyState::Inconclusive(InconclusiveReason::CommandMissing)
        );

        // 未命中任何具名原因的失败 → Failed，不许变成 Inconclusive
        let odd = crate::sandbox::capture::CapturedRun {
            exit_code: 2,
            timed_out: false,
            output: "something odd happened".into(),
            duration_ms: 10,
        };
        assert_eq!(classify(&odd, true), VerifyState::Inconclusive(InconclusiveReason::PreExistingRed));
        assert_eq!(classify(&odd, false), VerifyState::Failed);
    }

    /// 环境问题不是代码失败：沙箱拒绝访问 / 磁盘满 / DNS 故障必须判 `Inconclusive`。
    ///
    /// 反向对照：把 `is_env_failure` 的特征串摘空，本条必须变红——否则闸门会拿模型
    /// 根本修不好的问题去否决，并吃掉唯一一次验证额度。
    #[test]
    fn environment_failures_are_inconclusive_not_failed() {
        for output in [
            "error: 拒绝访问。 (os error 5)",
            "error: No space left on device (os error 28)",
            "error: failed to download: Could not resolve host: crates.io",
        ] {
            let run = crate::sandbox::capture::CapturedRun {
                exit_code: 1,
                timed_out: false,
                output: output.into(),
                duration_ms: 10,
            };
            assert_eq!(
                classify(&run, false),
                VerifyState::Inconclusive(InconclusiveReason::EnvFailure),
                "环境故障必须判为 Inconclusive(EnvFailure)：{output}"
            );
        }
    }

    /// `baseline = off` 必须真的关掉基线判定；否则真回归会被当成「事先就红」而不拦。
    #[test]
    fn baseline_off_switch_disables_baseline() {
        let off = VerifyConfig {
            baseline: BaselineMode::Off,
            ..Default::default()
        };
        assert!(!baseline_enabled(&off), "baseline=off 时不得采集/命中基线");
        assert!(baseline_enabled(&VerifyConfig::default()), "默认 auto 必须启用基线");

        // 开关关上时，同样的失败只能走 Failed，不许落到 PreExistingRed。
        let run = crate::sandbox::capture::CapturedRun {
            exit_code: 2,
            timed_out: false,
            output: "something odd happened".into(),
            duration_ms: 10,
        };
        assert_eq!(classify(&run, baseline_enabled(&off)), VerifyState::Failed);
    }

    #[test]
    fn baseline_requires_exact_command_and_same_signature() {
        let mut baseline = BaselineCache::default();
        let output = "error[E0425]: cannot find value\n --> src/lib.rs:1:1: cannot find value `x`";
        baseline.note_failure("cargo check", output);
        assert!(baseline.hit("cargo check", output));
        assert!(baseline.hit(" cargo check ", output));
        assert!(!baseline.hit("cargo check --all-targets", output));
        assert!(!baseline.hit(
            "cargo check",
            "error[E0308]: mismatched types\n --> src/lib.rs:9:1: mismatched types"
        ));
    }

    #[test]
    fn merge_prefers_failed_over_everything() {
        let failed = VerifyState::Failed;
        let timeout = VerifyState::Inconclusive(InconclusiveReason::Timeout);
        assert_eq!(merge_state(VerifyState::Passed, failed), failed);
        assert_eq!(merge_state(timeout, failed), failed);
        assert_eq!(
            merge_state(VerifyState::Skipped(SkipReason::NoCommandConfigured), timeout),
            timeout
        );
    }

    #[test]
    fn diagnostics_are_carried_into_reminder() {
        let outcome = VerifyOutcome {
            state: VerifyState::Failed,
            commands: vec!["cargo check".into()],
            duration_ms: 12,
            summary: "boom".into(),
            diagnostics: vec!["src/lib.rs:3:5 mismatched types".into()],
            output: "error[E0308]: mismatched types\n --> src/lib.rs:3:5".into(),
        };
        let text = reminder_text(&outcome, Some("自动修复提醒（第 1/2 次）"));
        assert!(text.contains("cargo check"));
        assert!(text.contains("src/lib.rs:3:5"));
        assert!(text.contains("不要用 `echo`"));
    }

    /// 到点收割：命令永不返回时，闸门必须在时限附近返回 `timeout`，而不是把回合挂住。
    ///
    /// 这里量的是**闸门自己的时限**，所以刻意走 `danger-full-access`（不起沙箱）：
    /// 沙箱冷启动/建容器的成本随机器与祖先链授权变化，把它算进「闸门时长」就会得到
    /// 一个随环境漂移的断言。循环级的端到端验证在 `agent_loop::tests` 里，那里只断言
    /// 「不挂住 + 判 timeout + 不否决」，不掺时间账。
    #[tokio::test]
    async fn run_commands_harvests_at_deadline() {
        let root = std::env::temp_dir();
        let cancel = CancellationToken::new();
        let commands = vec![VerifyCommand {
            // 纯 CPU 忙循环：不依赖网络与控制台，也不会自己退出。
            display: "for /L %i in (1,1,2000000000) do @rem".into(),
            cwd: root.clone(),
        }];
        let started = Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_secs(30),
            run_commands(
                &commands,
                Duration::from_millis(1_000),
                &root,
                crate::config::SandboxMode::DangerFullAccess,
                &cancel,
                None,
            ),
        )
        .await
        .expect("1s 时限的命令不得挂住闸门（30s 未返回即失败）");

        assert_eq!(
            outcome.state,
            VerifyState::Inconclusive(InconclusiveReason::Timeout),
            "不返回的命令必须判为超时；判成 Failed 会误否决"
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(10),
            "1s 时限应在时限附近收割（留足进程创建开销），实际 {elapsed:?}"
        );
    }
}
