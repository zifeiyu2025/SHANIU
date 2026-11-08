//! 工具路由（docs §8）：ToolSpec / ToolExecutor trait / 注册表。
//!
//! 职责：注册表管理 + 按名分发执行 + 向 model_client 下发工具声明。
//! 执行逻辑在各 `tools/*` 模块；风险分级与审批在 M2 permission 模块接入
//! （届时 execute 前置裁决，本模块保持纯分发）。

pub mod orchestration;
pub mod spec;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::events::EventSink;
use crate::model_client::ToolDeclaration;

pub use spec::ToolSpec;

/// 计划态共享标志。
///
/// 放在共享内核（与 `ToolSpec` 同层）而非 `tools/plan`：`agent_loop` 要读它做
/// 计划态门控，若留在 `tools/*` 会逼出 `agent_loop → tools` 的反向依赖（docs §2.3）。
#[derive(Debug, Default)]
pub struct PlanState {
    active: AtomicBool,
}

impl PlanState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    fn set(&self, active: bool) {
        self.active.store(active, Ordering::SeqCst);
    }

    /// 激活计划态（enter_plan_mode 调用；测试与恢复路径同样走此入口）。
    pub fn activate(&self) {
        self.set(true);
    }

    /// 退出计划态（exit_plan_mode 获批准后调用）。
    pub fn deactivate(&self) {
        self.set(false);
    }
}

// ============================== 读状态（SH-124） ==============================

/// 文件读取戳：mtime + 内容哈希双判（SH-124②）。
///
/// `hashed_len` 与 `len` 分离是为了兼容 `read_file` 的截断读取——记录时只哈希了
/// 读到的前缀，校验时也只比对同样长度的前缀。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    pub modified: Option<std::time::SystemTime>,
    pub len: u64,
    pub hashed_len: u64,
    pub hash: u64,
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

impl FileStamp {
    /// 用「已读到的字节 + 文件元数据」构造（`bytes` 可能是截断后的前缀）。
    pub fn of_read(bytes: &[u8], meta: &std::fs::Metadata) -> Self {
        Self {
            modified: meta.modified().ok(),
            len: meta.len(),
            hashed_len: bytes.len() as u64,
            hash: hash_bytes(bytes),
        }
    }

    /// 全量读取构造（写工具在自己刚写完/刚读完整文件时使用）。
    pub fn of_full_file(path: &Path) -> std::io::Result<Self> {
        let bytes = std::fs::read(path)?;
        Ok(Self {
            modified: std::fs::metadata(path).ok().and_then(|m| m.modified().ok()),
            len: bytes.len() as u64,
            hashed_len: bytes.len() as u64,
            hash: hash_bytes(&bytes),
        })
    }
}

/// 当前内容是否仍与读取戳一致：mtime 相同走快路径，不同再比长度与哈希。
pub fn content_matches(recorded: &FileStamp, current: &[u8], meta: &std::fs::Metadata) -> bool {
    if current.len() as u64 != recorded.len {
        return false;
    }
    if meta.modified().ok() == recorded.modified {
        return true;
    }
    let limit = recorded.hashed_len.min(current.len() as u64) as usize;
    limit as u64 == recorded.hashed_len && hash_bytes(&current[..limit]) == recorded.hash
}

/// 读状态登记表的路径键：canonicalize 后统一剥掉 Windows verbatim 前缀。
///
/// `resolve_in_workspace` 已经过 `shell_usable`，而写工具手里的路径是原样 join 的，
/// 两边必须归一化到同一把键，否则「读过的文件」在写侧永远查不到。
pub fn canonical_key(path: &Path) -> PathBuf {
    shell_usable(&path.canonicalize().unwrap_or_else(|_| path.to_path_buf()))
}

/// 读状态登记表（会话 + 路径）——写工具在落盘前查询，防止「没读就改」与覆盖外部改动。
///
/// 放在共享内核而不是 `tools/*`：`read_file`（记录）与各类写工具（校验）都要用它，
/// 未来新增写工具接同一份登记表即可（SH-124④），不需要各自造状态。
#[derive(Debug, Default)]
pub struct ReadRegistry {
    entries: std::sync::Mutex<HashMap<(String, PathBuf), FileStamp>>,
}

const DEFAULT_SESSION_KEY: &str = "__no_session__";

impl ReadRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn key(session: Option<&str>, path: &Path) -> (String, PathBuf) {
        (
            session.unwrap_or(DEFAULT_SESSION_KEY).to_string(),
            path.to_path_buf(),
        )
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(String, PathBuf), FileStamp>> {
        // 登记表只存观测值；锁中毒不该让整个写路径失败。
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn record(&self, session: Option<&str>, path: &Path, stamp: FileStamp) {
        self.lock().insert(Self::key(session, path), stamp);
    }

    pub fn get(&self, session: Option<&str>, path: &Path) -> Option<FileStamp> {
        self.lock().get(&Self::key(session, path)).copied()
    }

    pub fn forget(&self, session: Option<&str>, path: &Path) {
        self.lock().remove(&Self::key(session, path));
    }
}

// ============================== 写日志（SH-126） ==============================

/// 断言类语句的行首前缀（trim 之后比对）。
///
/// 只认「行首」是刻意的：Rust 里 `.expect(` 遍地都是，按子串匹配会把普通代码全算成断言；
/// 而 JS/TS 的 `expect(x)` 与 Python 的 `assert x` 都在行首。
const ASSERTION_PREFIXES: &[&str] = &[
    "assert!(",
    "assert_eq!(",
    "assert_ne!(",
    "debug_assert!(",
    "debug_assert_eq!(",
    "debug_assert_ne!(",
    "assert(",
    "assert.equal(",
    "assert.strictEqual(",
    "assert.deepEqual(",
    "assert.deepStrictEqual(",
    "assert.ok(",
    "expect(",
];

/// 「强断言」前缀：`assert_eq!` / `assert_ne!` 这类同时校验两边的形态。
/// 它们减少、而普通 `assert!` 增加 = 断言被放宽（SH-126 判定依据之一）。
const STRICT_ASSERTION_PREFIXES: &[&str] = &[
    "assert_eq!(",
    "assert_ne!(",
    "debug_assert_eq!(",
    "debug_assert_ne!(",
    "assert.equal(",
    "assert.strictEqual(",
    "assert.deepEqual(",
    "assert.deepStrictEqual(",
];

fn is_assertion_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    // 只认行首：这是有意的。注释掉的断言（`// assert_eq!`）行首是 `//`，因此**不算**断言，
    // 它在「改前有、改后没有」的比对里会体现成删减——注释掉断言本来就该按删减处理。
    if ASSERTION_PREFIXES
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
        || trimmed.starts_with("assert ")
    {
        return true;
    }
    // 行内断言也算：`fn adds() { assert_eq!(add(2, 2), 4); }` 这种一行写法很常见，
    // 只认行首会整条漏掉。Rust 的断言宏字面量特征足够强，可以按子串认；
    // JS 的 `expect(` 不行（Rust 的 `.expect(` 遍地都是），所以它仍然只在行首生效。
    ["assert!(", "assert_eq!(", "assert_ne!(", "debug_assert!(", "debug_assert_eq!(", "debug_assert_ne!("]
        .iter()
        .any(|needle| line.contains(needle))
}

fn assertion_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| is_assertion_line(line))
        .map(String::from)
        .collect()
}

fn strict_assertion_count(lines: &[String]) -> usize {
    lines
        .iter()
        .filter(|line| {
            // 用 contains 而不是 starts_with：一行写法 `fn f() { assert_eq!(..) }` 的行首是 `fn`。
            STRICT_ASSERTION_PREFIXES
                .iter()
                .any(|prefix| line.contains(prefix))
        })
        .count()
}

fn ignore_attr_count(text: &str) -> usize {
    text.lines()
        .filter(|line| line.trim().starts_with("#[ignore"))
        .count()
}

/// 一次文件写入的摘要（写工具落盘后登记，自愈闭环消费）。
///
/// 为什么需要它：自愈闭环 + 能改文件 = 存在「把断言删掉让测试通过」的路径。
/// 宿主看不到模型意图，但看得到「哪些文件被改了、断言有没有变」——
/// 这就是把「不可见」变成「可见」的最小做法（SH-126）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileWrite {
    /// 写入后的绝对路径（已归一化，可直接与诊断路径/读登记表比对）。
    pub path: PathBuf,
    /// 内容是否真的变了（新文件/删除也算变）。
    pub changed: bool,
    /// 断言条数净变化：>0 新增，<0 删减。
    pub assertion_delta: i64,
    /// 断言语句集合是否变化（覆盖「条数不变、只改期望值」的改宽）。
    pub assertions_touched: bool,
    /// 是否检测到放宽：删减断言 / 强断言降级 / 新增 `#[ignore]`。
    pub weakened: bool,
}

impl FileWrite {
    /// `before` / `after` 为 `None` 表示文件分别不存在（新建 / 删除）。
    pub fn between(path: PathBuf, before: Option<&str>, after: Option<&str>) -> Self {
        let before_text = before.unwrap_or("");
        let after_text = after.unwrap_or("");
        let before_assertions = assertion_lines(before_text);
        let after_assertions = assertion_lines(after_text);
        let strict_before = strict_assertion_count(&before_assertions);
        let strict_after = strict_assertion_count(&after_assertions);
        let plain_before = before_assertions.len() - strict_before;
        let plain_after = after_assertions.len() - strict_after;
        let weakened = after_assertions.len() < before_assertions.len()
            // 强断言变普通断言：`assert_eq!(a, b)` → `assert!(a == b)` 就是放宽。
            || (strict_after < strict_before && plain_after > plain_before)
            // 挂 `#[ignore]` = 让测试不再执行，等价于删除断言。
            || ignore_attr_count(after_text) > ignore_attr_count(before_text);
        let assertions_touched = match (before, after) {
            // 新建文件不算「动了断言」：没有可被删减的原断言。
            (None, _) => false,
            (Some(_), None) => !before_assertions.is_empty(),
            (Some(_), Some(_)) => before_assertions != after_assertions,
        };
        Self {
            path,
            changed: before_text != after_text,
            assertion_delta: after_assertions.len() as i64 - before_assertions.len() as i64,
            assertions_touched,
            weakened,
        }
    }
}

/// 写日志：写工具登记「这次改了什么文件、断言有没有变」，自愈闭环在命令重新通过时消费。
///
/// 放在共享内核的原因与 [`ReadRegistry`] 相同：`apply_patch` 是生产者，
/// `agent_loop` 是消费者，两边不能各造一份状态，否则永远对不上。
#[derive(Debug, Default)]
pub struct WriteJournal {
    entries: std::sync::Mutex<Vec<FileWrite>>,
}

impl WriteJournal {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<FileWrite>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 登记一次写入；同一文件的多次写入按路径合并，避免一次修复产出多条噪音。
    pub fn record(&self, path: &Path, before: Option<&str>, after: Option<&str>) {
        let entry = FileWrite::between(canonical_key(path), before, after);
        if !entry.changed {
            return;
        }
        let mut entries = self.lock();
        if let Some(existing) = entries.iter_mut().find(|e| e.path == entry.path) {
            existing.changed |= entry.changed;
            existing.assertion_delta += entry.assertion_delta;
            existing.assertions_touched |= entry.assertions_touched;
            existing.weakened |= entry.weakened;
        } else {
            entries.push(entry);
        }
    }

    /// 取走并清空（消费语义：每次命令结果结算一次，不重复判定）。
    pub fn take(&self) -> Vec<FileWrite> {
        std::mem::take(&mut *self.lock())
    }
}

// ============================== 执行契约 ==============================

/// 工具执行错误。`Execution` 的文本会回填给模型（风险登记册：失败自动回填）。
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("参数非法: {0}")]
    InvalidArgs(String),
    #[error("{0}")]
    Execution(String),
    #[error("已取消")]
    Cancelled,
}

/// 工具输出（回填给模型的文本 + 成功标记）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub output: String,
    pub success: bool,
}

impl ToolOutput {
    pub fn ok(output: impl Into<String>) -> Self {
        Self { output: output.into(), success: true }
    }
    pub fn fail(output: impl Into<String>) -> Self {
        Self { output: output.into(), success: false }
    }
}

/// 工具执行上下文：工作区边界、取消令牌、流式输出通道、沙箱策略。
pub struct ToolContext {
    /// 工作区根目录（沙箱 workspace-write 的作用域，docs §9.3）。
    pub workspace_root: PathBuf,
    pub cancel: CancellationToken,
    /// 本次调用的 call_id（unified_exec 等流式工具经 sink 发 ToolCallOutputDelta）。
    pub call_id: String,
    pub sink: Arc<dyn EventSink>,
    /// 沙箱策略（unified_exec 启动子进程时生效）。
    pub sandbox: crate::config::SandboxMode,
    /// 会话作用域（SH-089）：后台作业的属主。测试与无会话路径为 None。
    pub session_id: Option<String>,
    /// 进程级作业注册表：`run_in_background` 与 job 工具从这里读写。
    pub jobs: Option<Arc<crate::jobs::JobRegistry>>,
    /// 调用者身份（SH-052）：root 或子代理；None = 无代理上下文的测试路径。
    pub scope: Option<orchestration::AgentScope>,
    /// 编排能力（SH-052）：只有 depth=0 的 root 实例注入；子代理为 None。
    pub orchestrator: Option<Arc<dyn orchestration::AgentOrchestrator>>,
    /// 读状态登记表（SH-124）：`read_file` 记录、写工具校验。
    /// None = 该路径不启用写前读校验（无会话的测试装配）。
    pub reads: Option<Arc<ReadRegistry>>,
    /// 写日志（SH-126）：写工具登记、自愈闭环判定「是否靠改测试变绿」。
    pub writes: Option<Arc<WriteJournal>>,
}

impl ToolContext {
    pub fn new(
        workspace_root: impl Into<PathBuf>,
        cancel: CancellationToken,
        call_id: impl Into<String>,
        sink: Arc<dyn EventSink>,
        sandbox: crate::config::SandboxMode,
    ) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            cancel,
            call_id: call_id.into(),
            sink,
            sandbox,
            session_id: None,
            jobs: None,
            scope: None,
            orchestrator: None,
            reads: None,
            writes: None,
        }
    }

    /// 只注入代理身份（子代理路径）：用于深度门控，不提供编排能力。
    pub fn with_scope(mut self, scope: orchestration::AgentScope) -> Self {
        self.scope = Some(scope);
        self
    }

    /// 注入代理身份与编排能力（SH-052，仅 root）。两者要么同时给，要么都不给：
    /// 只有 scope 没有 orchestrator 时，编排工具必然失败，不如在声明层就隐藏。
    pub fn with_orchestration(
        mut self,
        scope: orchestration::AgentScope,
        orchestrator: Arc<dyn orchestration::AgentOrchestrator>,
    ) -> Self {
        self.scope = Some(scope);
        self.orchestrator = Some(orchestrator);
        self
    }

    /// 注入会话与作业注册表（agent_loop 在每个 turn 的工具上下文上调用）。
    pub fn with_jobs(
        mut self,
        session_id: Option<String>,
        jobs: Option<Arc<crate::jobs::JobRegistry>>,
    ) -> Self {
        self.session_id = session_id;
        self.jobs = jobs;
        self
    }

    /// 注入读状态登记表（SH-124）：`read_file` 与写工具共用同一份。
    pub fn with_reads(mut self, reads: Arc<ReadRegistry>) -> Self {
        self.reads = Some(reads);
        self
    }

    /// 注入写日志（SH-126）：写工具登记、自愈闭环消费。
    pub fn with_writes(mut self, writes: Arc<WriteJournal>) -> Self {
        self.writes = Some(writes);
        self
    }
}

/// 工具执行器：一个实现 = 一个工具。
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    fn spec(&self) -> &ToolSpec;
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError>;
}

// ============================== 注册表 ==============================

/// 注册失败原因（docs/02 §3.1.3 B-R3/B-R4）。
///
/// 动态注册面（MCP）必须用 [`ToolRouter::try_register`] / [`ToolRouter::try_register_all`]：
/// 名称冲突返回错误而不是覆盖，未提交前不改动注册表。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RegisterError {
    #[error("工具名不能为空")]
    EmptyName,
    /// 已存在同名工具，或仅大小写不同的同名工具（避免模型/路由选错对象）。
    #[error("工具名冲突: {0}（已存在同名或仅大小写不同的工具）")]
    Duplicate(String),
}

#[derive(Default)]
pub struct ToolRouter {
    tools: HashMap<String, Arc<dyn ToolExecutor>>,
}

impl ToolRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 内置工具注册入口。命名冲突属于编程错误（内置清单是静态的），
    /// 因此这里 fail-fast 而不是静默覆盖——静默覆盖在动态注册面就是工具冒充入口。
    /// 动态注册（MCP）一律用 [`try_register`](Self::try_register)。
    pub fn register(&mut self, executor: Arc<dyn ToolExecutor>) {
        let name = executor.spec().name.clone();
        if let Err(e) = self.try_register(executor) {
            panic!("内置工具注册失败（{name}）: {e}");
        }
    }

    /// 单条注册：名称冲突返回错误，失败时注册表保持原样。
    pub fn try_register(&mut self, executor: Arc<dyn ToolExecutor>) -> Result<(), RegisterError> {
        let name = executor.spec().name.clone();
        let existing: Vec<&str> = self.tools.keys().map(String::as_str).collect();
        check_name(&name, &existing)?;
        self.tools.insert(name, executor);
        Ok(())
    }

    /// 事务化批量注册：先全量校验（含批次内互相冲突），全部通过才一次性提交。
    /// 任一失败 → 一个都不注册，调用方无需回滚半套工具（B-R3）。
    pub fn try_register_all(
        &mut self,
        executors: Vec<Arc<dyn ToolExecutor>>,
    ) -> Result<(), RegisterError> {
        {
            let mut seen: Vec<&str> = self.tools.keys().map(String::as_str).collect();
            for executor in &executors {
                let name = executor.spec().name.as_str();
                check_name(name, &seen)?;
                seen.push(name);
            }
        }
        for executor in executors {
            self.tools.insert(executor.spec().name.clone(), executor);
        }
        Ok(())
    }

    /// 注销工具（MCP 服务器断开/清单删除时使用）。返回被移除的执行器。
    pub fn remove(&mut self, name: &str) -> Option<Arc<dyn ToolExecutor>> {
        self.tools.remove(name)
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn ToolExecutor>> {
        self.tools.get(name)
    }

    /// 全部工具契约（下发模型用）。
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self.tools.values().map(|t| t.spec().clone()).collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    }

    pub fn declarations(&self) -> Vec<ToolDeclaration> {
        self.specs().iter().map(|s| s.to_declaration()).collect()
    }

    /// 按名分发执行；未知工具返回 InvalidArgs（回填模型可自我纠正）。
    pub async fn execute(
        &self,
        name: &str,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        // 第二层深度门控（ADR-101/FF-106）：即使模型伪造工具名，子代理也无法调用编排工具。
        // 第一层在 `agent_loop::declarations_for`——模型根本看不到这些声明。
        if let Some(scope) = &ctx.scope {
            if scope.depth > 0 && orchestration::is_agent_tool(name) {
                return Err(ToolError::InvalidArgs(format!(
                    "当前代理深度 {} 不允许调用编排工具 {name}（只有 root 可以 spawn）",
                    scope.depth
                )));
            }
        } else if orchestration::is_agent_tool(name) {
            return Err(ToolError::InvalidArgs(format!(
                "没有代理上下文，无法调用编排工具 {name}"
            )));
        }
        let executor = self
            .tools
            .get(name)
            .ok_or_else(|| ToolError::InvalidArgs(format!("未知工具: {name}")))?;
        executor.execute(args, ctx).await
    }
}

/// 名称合法性检查：非空 + 不与既有名称（或仅 ASCII 大小写不同）冲突。
fn check_name(name: &str, existing: &[&str]) -> Result<(), RegisterError> {
    if name.trim().is_empty() {
        return Err(RegisterError::EmptyName);
    }
    if existing.iter().any(|e| e.eq_ignore_ascii_case(name)) {
        return Err(RegisterError::Duplicate(name.to_string()));
    }
    Ok(())
}

// ============================== 工作区路径边界（共享助手） ==============================

/// 去掉 Windows 的 `\\?\` verbatim 前缀，还原成 shell 可用的等价路径。
///
/// 为什么必须还原：`canonicalize()` 在 Windows 上**一定**产出 `\\?\C:\...` 形态，
/// 而 cmd.exe 认不了它——`cd /d "\\?\C:\..."` 失败、`dir /b > out.txt` 静默不产出文件。
/// 于是模型看到的 cwd 与它能用的 cwd 不是同一个东西，只能反复重试直到耗尽轮数预算
/// （SH-085 的真实成因，基线里那条任务就是这么死的）。
/// 只在**安全校验之后**调用：越界判定仍用 canonical 路径做，还原不改变指向的位置。
/// 非 verbatim（普通绝对/相对路径）原样返回。
pub fn shell_usable(path: &Path) -> PathBuf {
    let mut components = path.components();
    match components.next() {
        Some(std::path::Component::Prefix(prefix)) => match prefix.kind() {
            // drive 是 ASCII 码（u8），必须转成字符；根已在字符串里给出，
            // 后面的 RootDir 组件必须跳过，否则拼出 "D:\\\x"
            std::path::Prefix::VerbatimDisk(drive) => {
                let mut out = PathBuf::from(format!("{}:\\", char::from(drive)));
                out.extend(components.filter(|c| !matches!(c, std::path::Component::RootDir)));
                out
            }
            std::path::Prefix::VerbatimUNC(host, share) => {
                // host/share 是 OsStr，不能进 format!；用 push 逐段拼
                let mut out = PathBuf::from(r"\\");
                out.push(host);
                out.push(share);
                out.extend(components.filter(|c| !matches!(c, std::path::Component::RootDir)));
                out
            }
            // 其它前缀（\\.\、\?\ 设备路径等）不是 canonicalize 的产物，原样交出
            _ => path.to_path_buf(),
        },
        _ => path.to_path_buf(),
    }
}

/// 解析工作区内路径：绝对路径与相对路径统一，canonicalize 后校验不越界。
/// 要求目标**已存在**（读类工具用）。这是 M1 的逻辑边界；M2 sandbox 模块落地后
/// 由 deny-ACE 接管强制力（ADR-002）。返回 shell 可用形态（见 [`shell_usable`]）。
pub fn resolve_in_workspace(workspace_root: &Path, path: &str) -> Result<PathBuf, ToolError> {
    if path.trim().is_empty() {
        return Err(ToolError::InvalidArgs("路径为空".into()));
    }
    let raw = PathBuf::from(path);
    let joined = if raw.is_absolute() { raw } else { workspace_root.join(raw) };
    let canonical = joined
        .canonicalize()
        .map_err(|_| ToolError::InvalidArgs(format!("路径不存在: {path}")))?;
    let root = workspace_root
        .canonicalize()
        .map_err(|e| ToolError::Execution(format!("工作区根目录不可用: {e}")))?;
    if !canonical.starts_with(&root) {
        return Err(ToolError::Execution(format!("路径越界（工作区之外）: {path}")));
    }
    Ok(shell_usable(&canonical))
}

/// 写类操作的工作区路径校验：允许目标**尚不存在**（如 Add File）。
/// 规则：拒绝包含 `..` 组件；对最深存在的祖先做 canonicalize 并校验不越界，
/// 尾部组件原样拼接（不存在 `..` 故不可能向上逃逸）。
pub fn resolve_for_write(workspace_root: &Path, path: &str) -> Result<PathBuf, ToolError> {
    if path.trim().is_empty() {
        return Err(ToolError::InvalidArgs("路径为空".into()));
    }
    let raw = PathBuf::from(path);
    let joined = if raw.is_absolute() { raw } else { workspace_root.join(raw) };
    if joined
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(ToolError::Execution(format!(
            "路径越界（包含 ..）: {path}"
        )));
    }
    // 自底向上找最深存在祖先
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut probe = joined.clone();
    while !probe.exists() {
        match probe.parent() {
            Some(parent) if parent != probe => {
                tail.push(probe.file_name().map(|n| n.to_os_string()).unwrap_or_default());
                probe = parent.to_path_buf();
            }
            _ => return Err(ToolError::InvalidArgs(format!("路径无法定位: {path}"))),
        }
    }
    let canonical_base = probe
        .canonicalize()
        .map_err(|e| ToolError::Execution(format!("工作区根目录不可用: {e}")))?;
    let root = workspace_root
        .canonicalize()
        .map_err(|e| ToolError::Execution(format!("工作区根目录不可用: {e}")))?;
    if !canonical_base.starts_with(&root) {
        return Err(ToolError::Execution(format!("路径越界（工作区之外）: {path}")));
    }
    let mut result = canonical_base;
    for name in tail.iter().rev() {
        result.push(name);
    }
    if !result.starts_with(&root) {
        return Err(ToolError::Execution(format!("路径越界（工作区之外）: {path}")));
    }
    Ok(shell_usable(&result))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 工具级单测的最小上下文（sink 丢弃事件）
    pub fn test_context(workspace_root: &Path) -> ToolContext {
        static SINK: std::sync::OnceLock<Arc<dyn EventSink>> = std::sync::OnceLock::new();
        let sink = SINK.get_or_init(|| -> Arc<dyn EventSink> {
            Arc::new(crate::events::NullEventSink)
        })
        .clone();
        ToolContext::new(workspace_root, CancellationToken::new(), "call-test", sink, crate::config::SandboxMode::WorkspaceWrite)
    }

    fn router_with_noop() -> ToolRouter {
        struct Owned {
            spec: ToolSpec,
        }
        #[async_trait]
        impl ToolExecutor for Owned {
            fn spec(&self) -> &ToolSpec {
                &self.spec
            }
            async fn execute(&self, _a: Value, _c: &ToolContext) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput::ok("done"))
            }
        }
        let mut router = ToolRouter::new();
        router.register(Arc::new(Owned {
            spec: ToolSpec::new("noop", "no-op", serde_json::json!({"type":"object"})),
        }));
        router
    }

    #[tokio::test]
    async fn executes_registered_tool_by_name() {
        let router = router_with_noop();
        let out = router
            .execute("noop", serde_json::json!({}), &test_context(Path::new(".")))
            .await
            .unwrap();
        assert!(out.success);
        assert_eq!(out.output, "done");
    }

    #[tokio::test]
    async fn unknown_tool_is_invalid_args() {
        let router = router_with_noop();
        let err = router
            .execute("missing", serde_json::json!({}), &test_context(Path::new(".")))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    #[tokio::test]
    async fn cancelled_context_short_circuits() {
        let router = router_with_noop();
        let ctx = test_context(Path::new("."));
        ctx.cancel.cancel();
        let err = router.execute("noop", serde_json::json!({}), &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::Cancelled));
    }

    #[test]
    fn specs_and_declarations_are_sorted_and_complete() {
        let router = router_with_noop();
        let specs = router.specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "noop");
        assert_eq!(router.declarations()[0].name, "noop");
    }

    // ---------------- 注册表事务化契约（docs/02 §3.1.3 B-R3/B-R4） ----------------

    struct StubTool {
        spec: ToolSpec,
        output: &'static str,
    }

    #[async_trait]
    impl ToolExecutor for StubTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }
        async fn execute(&self, _a: Value, _c: &ToolContext) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::ok(self.output))
        }
    }

    fn stub(name: &str, output: &'static str) -> Arc<dyn ToolExecutor> {
        Arc::new(StubTool {
            spec: ToolSpec::new(name, "stub", serde_json::json!({"type":"object"})),
            output,
        })
    }

    #[tokio::test]
    async fn try_register_rejects_duplicate_and_keeps_original() {
        let mut router = ToolRouter::new();
        router.try_register(stub("noop", "original")).unwrap();
        let err = router.try_register(stub("noop", "hijacked")).unwrap_err();
        assert_eq!(err, RegisterError::Duplicate("noop".into()));
        // 冲突不得覆盖：原执行器仍在且行为未变
        let out = router
            .execute("noop", serde_json::json!({}), &test_context(Path::new(".")))
            .await
            .unwrap();
        assert_eq!(out.output, "original");
    }

    #[test]
    fn try_register_rejects_case_insensitive_duplicate() {
        let mut router = ToolRouter::new();
        router.try_register(stub("unified_exec", "builtin")).unwrap();
        let err = router.try_register(stub("Unified_Exec", "evil")).unwrap_err();
        assert_eq!(err, RegisterError::Duplicate("Unified_Exec".into()));
        assert!(router.get("Unified_Exec").is_none());
    }

    #[test]
    fn try_register_rejects_empty_name() {
        let mut router = ToolRouter::new();
        let err = router.try_register(stub("   ", "x")).unwrap_err();
        assert_eq!(err, RegisterError::EmptyName);
        assert!(router.specs().is_empty());
    }

    #[test]
    fn try_register_all_is_atomic() {
        let mut router = ToolRouter::new();
        router.try_register(stub("exists", "keep")).unwrap();
        // 批次里第二条与既有工具冲突 → 整批不提交，第一条也不能留下
        let err = router
            .try_register_all(vec![stub("fresh", "a"), stub("exists", "b")])
            .unwrap_err();
        assert_eq!(err, RegisterError::Duplicate("exists".into()));
        assert!(router.get("fresh").is_none(), "失败批次不得留下半套工具");
        assert_eq!(router.specs().len(), 1);
    }

    #[test]
    fn try_register_all_rejects_intra_batch_duplicate() {
        let mut router = ToolRouter::new();
        let err = router
            .try_register_all(vec![stub("dup", "a"), stub("dup", "b")])
            .unwrap_err();
        assert_eq!(err, RegisterError::Duplicate("dup".into()));
        assert!(router.specs().is_empty());
    }

    #[test]
    fn try_register_all_commits_when_clean() {
        let mut router = ToolRouter::new();
        router
            .try_register_all(vec![stub("a", "1"), stub("b", "2")])
            .unwrap();
        let names: Vec<String> = router.specs().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
    }

    #[tokio::test]
    async fn remove_unregisters_tool() {
        let mut router = ToolRouter::new();
        router.try_register(stub("noop", "done")).unwrap();
        assert!(router.remove("noop").is_some());
        assert!(router.remove("noop").is_none());
        let err = router
            .execute("noop", serde_json::json!({}), &test_context(Path::new(".")))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    #[test]
    fn builtin_tools_never_use_mcp_prefix() {
        // B-R1：`mcp__` 前缀是 MCP 分发名的专属命名空间，内置工具不得占用，
        // 否则恶意 server 可借前缀冒充内置工具。
        let mut router = ToolRouter::new();
        crate::tools::register_builtin(&mut router, Arc::new(PlanState::new()));
        for spec in router.specs() {
            assert!(
                !spec.name.starts_with("mcp__"),
                "内置工具名不得以 mcp__ 开头: {}",
                spec.name
            );
        }
    }

    // resolve_in_workspace 的行为契约
    fn temp_workspace() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("shaniu-ws-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        dir
    }

    #[test]
    fn resolves_relative_and_absolute_paths() {
        let ws = temp_workspace();
        let rel = resolve_in_workspace(&ws, "a.txt").unwrap();
        assert!(rel.ends_with("a.txt"));
        let abs = resolve_in_workspace(&ws, &ws.join("a.txt").to_string_lossy()).unwrap();
        assert!(abs.ends_with("a.txt"));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn rejects_out_of_workspace_paths() {
        let ws = temp_workspace();
        // ".." 真实存在（temp 父目录）：canonicalize 成功后触发越界判定
        let err = resolve_in_workspace(&ws, "..").unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn write_resolver_allows_missing_targets_but_rejects_escape() {
        let ws = temp_workspace();
        // 允许尚不存在的目标（Add File 场景），含多级新目录
        let ok = resolve_for_write(&ws, "new/deep/file.txt").unwrap();
        // 契约（SH-085）：返回 shell 可用形态，故与"还原后的根"比较，而不是 canonicalize 原值
        assert!(ok.starts_with(shell_usable(&ws.canonicalize().unwrap()).as_path()));
        assert_eq!(ok.file_name().unwrap(), "file.txt");
        // 含 .. 的路径直接越界拒绝
        let err = resolve_for_write(&ws, "../evil.txt").unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
        std::fs::remove_dir_all(&ws).ok();
    }

    /// SH-085 的回归锁：canonicalize 在 Windows 上必然产出 `\\?\` verbatim 路径，
    /// 而 cmd.exe 拿它当 cwd 时 `dir /b > out.txt` 静默不产出文件、`cd /d "\\?\..."` 失败——
    /// 模型看到的 cwd 与它能用的 cwd 不是同一个东西，只能反复重试到轮数预算耗尽。
    #[cfg(windows)]
    #[test]
    fn resolvers_never_hand_verbatim_paths_to_shell_tools() {
        let ws = temp_workspace();
        std::fs::write(ws.join("a.txt"), "x").unwrap();
        for resolved in [
            resolve_in_workspace(&ws, "a.txt").unwrap(),
            resolve_in_workspace(&ws, ".").unwrap(),
            resolve_for_write(&ws, "new/deep/file.txt").unwrap(),
        ] {
            let text = resolved.display().to_string();
            assert!(
                !text.contains(r"\\?\"),
                "交给 shell/模型的路径不该带 verbatim 前缀：{text}"
            );
        }
        // 还原不改变指向：已存在的目标仍能在还原后的路径上命中
        assert!(
            resolve_in_workspace(&ws, "a.txt").unwrap().is_file(),
            "还原后的路径不能再变成不可用"
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    #[cfg(windows)]
    #[test]
    fn shell_usable_strips_verbatim_and_leaves_normal_paths() {
        assert_eq!(
            shell_usable(Path::new(r"\\?\C:\Users\jun\a")),
            PathBuf::from(r"C:\Users\jun\a")
        );
        assert_eq!(
            shell_usable(Path::new(r"\\?\UNC\fileserver\share\x")),
            PathBuf::from(r"\\fileserver\share\x")
        );
        // 非 verbatim 原样交出（drive 是 u8 ASCII 码，转错就会产出 "68:\" 这种废路径）
        assert_eq!(
            shell_usable(Path::new(r"C:\Users\jun\a")),
            PathBuf::from(r"C:\Users\jun\a")
        );
    }

    #[test]
    fn rejects_missing_paths_as_invalid_args() {
        let ws = temp_workspace();
        let err = resolve_in_workspace(&ws, "no-such-file.txt").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn file_stamp_detects_same_length_content_change() {
        let ws = temp_workspace();
        let path = ws.join("README.md");
        std::fs::write(&path, "abc").unwrap();
        // 人为把 mtime 置为不同值，强制走哈希比对路径
        let reported = FileStamp {
            modified: Some(std::time::SystemTime::UNIX_EPOCH),
            len: 3,
            hashed_len: 3,
            hash: {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                b"abc".hash(&mut h);
                h.finish()
            },
        };
        let meta = std::fs::metadata(&path).unwrap();
        assert!(content_matches(&reported, b"abc", &meta), "内容相同应判定新鲜");
        assert!(!content_matches(&reported, b"abd", &meta), "同长度不同内容必须判 stale");
        assert!(!content_matches(&reported, b"abcd", &meta), "长度变化必须判 stale");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn read_registry_keys_by_session_and_path() {
        let ws = temp_workspace();
        let path = ws.join("README.md");
        std::fs::write(&path, "hello").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let stamp = FileStamp::of_read(b"hello", &meta);
        let registry = ReadRegistry::new();
        registry.record(Some("s1"), &path, stamp);
        assert!(registry.get(Some("s1"), &path).is_some());
        assert!(registry.get(Some("s2"), &path).is_none(), "读状态必须按会话隔离");
        assert!(registry.get(None, &path).is_none());
        registry.forget(Some("s1"), &path);
        assert!(registry.get(Some("s1"), &path).is_none());
        std::fs::remove_dir_all(&ws).ok();
    }

    // ---------------- 写日志：断言变化判定（SH-126） ----------------

    const TEST_BEFORE: &str = "fn add(a: i32, b: i32) -> i32 { a - b }\n\
        #[test]\n\
        fn adds() {\n\
            assert_eq!(add(2, 2), 4);\n\
        }\n";

    #[test]
    fn deleting_an_assertion_is_weakened() {
        let after = "fn add(a: i32, b: i32) -> i32 { a + b }\n#[test]\nfn adds() {\n}\n";
        let write = FileWrite::between(PathBuf::from("x.rs"), Some(TEST_BEFORE), Some(after));
        assert!(write.weakened, "删掉断言必须判为放宽: {write:?}");
        assert!(write.assertions_touched);
        assert_eq!(write.assertion_delta, -1);
    }

    #[test]
    fn downgrading_strict_assertion_is_weakened() {
        let after = TEST_BEFORE.replace("assert_eq!(add(2, 2), 4);", "assert!(true);");
        let write = FileWrite::between(PathBuf::from("x.rs"), Some(TEST_BEFORE), Some(&after));
        assert!(write.weakened, "assert_eq! → assert! 必须判为放宽: {write:?}");
    }

    #[test]
    fn adding_ignore_attribute_is_weakened() {
        let after = TEST_BEFORE.replace("#[test]", "#[test]\n#[ignore]");
        let write = FileWrite::between(PathBuf::from("x.rs"), Some(TEST_BEFORE), Some(&after));
        assert!(write.weakened, "挂 #[ignore] 等价于删断言: {write:?}");
    }

    #[test]
    fn changing_expected_value_is_touched_but_not_weakened() {
        // 条数没变、强断言没降级，只是把期望值改成能通过的值——这是最隐蔽的改宽，
        // 交给「测试文件被改动」这层判可疑，而不是在这里判删减。
        let after = TEST_BEFORE.replace("assert_eq!(add(2, 2), 4);", "assert_eq!(add(2, 2), 0);");
        let write = FileWrite::between(PathBuf::from("x.rs"), Some(TEST_BEFORE), Some(&after));
        assert!(write.assertions_touched, "{write:?}");
        assert!(!write.weakened, "只改期望值不算删减/放宽: {write:?}");
    }

    #[test]
    fn implementation_only_fix_does_not_touch_assertions() {
        let after = TEST_BEFORE.replace("a - b", "a + b");
        let write = FileWrite::between(PathBuf::from("x.rs"), Some(TEST_BEFORE), Some(&after));
        assert!(write.changed);
        assert!(!write.assertions_touched, "只改实现不得被算作动了断言: {write:?}");
        assert!(!write.weakened);
    }

    #[test]
    fn creating_a_new_file_never_counts_as_touching_assertions() {
        let write = FileWrite::between(
            PathBuf::from("tests/new_test.rs"),
            None,
            Some("#[test]\nfn t() { assert_eq!(1, 1); }\n"),
        );
        assert!(write.changed);
        assert!(!write.assertions_touched, "新建文件没有可被删减的原断言");
        assert!(!write.weakened);
    }

    #[test]
    fn delete_is_judged_against_the_removed_content() {
        let write = FileWrite::between(PathBuf::from("tests/x.rs"), Some(TEST_BEFORE), None);
        assert!(write.changed);
        assert!(write.weakened, "删掉整个含断言的测试文件必须判放宽: {write:?}");
    }

    #[test]
    fn journal_merges_same_path_and_drains_once() {
        let ws = temp_workspace();
        let path = ws.join("a.rs");
        std::fs::write(&path, TEST_BEFORE).unwrap();
        let journal = WriteJournal::new();
        // 第一次：把实现改对（不动断言）
        journal.record(&path, Some(TEST_BEFORE), Some(&TEST_BEFORE.replace("a - b", "a + b")));
        let once = journal.take();
        assert_eq!(once.len(), 1);
        assert!(!once[0].weakened);
        assert!(journal.take().is_empty(), "take 之后必须清空");
        // 同一路径的两次写入合并为一条
        journal.record(&path, Some(TEST_BEFORE), Some(&TEST_BEFORE.replace("a - b", "a + b")));
        journal.record(&path, Some(TEST_BEFORE), Some("fn add() {}\n"));
        let merged = journal.take();
        assert_eq!(merged.len(), 1, "同路径写入必须合并: {merged:?}");
        assert!(merged[0].weakened);
        std::fs::remove_dir_all(&ws).ok();
    }
}
