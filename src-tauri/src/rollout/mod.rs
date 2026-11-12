//! rollout JSONL 持久化（docs/04 §3.1，SH-030）。
//!
//! 一行一个 `RolloutItem`，首行必须是 `SessionMeta`。三条刻意的设计选择：
//!
//! 1. **逐行 flush**：进程被杀/断电时，已写行必须可读。攒批写快，但崩溃点之后的历史全丢，
//!    resume 就会拿到一段"看起来完整、实际缺尾"的历史——比没存档更糟。
//! 2. **容忍中间坏行，但必须看得见**：跳过无法解析的行继续读，坏行行号随
//!    [`Rollout::damaged_lines`] 返回，并在 `read` 内打一条 warn。**不允许静默跳过**——
//!    跳过且不告诉任何人，等于把"可恢复的损坏"变成"悄悄丢历史"。首行坏仍报错
//!    （没有 meta 就没有会话，跳过它等于编造一份无头档案）；末行撕裂照旧丢弃并计数。
//! 3. **载荷直接复用 `model_client::HistoryItem`**：04 §3.1 的 `ResponseItem` 就是
//!    "模型请求/响应/工具调用的完整记录"，而 HistoryItem 正是循环里那份记录。再造一个
//!    同构类型 = 两份真相，resume 时必然与运行时历史漂移。
//!
//! 边界（AGENTS §3）：rollout 含用户代码与对话，属敏感数据。本模块**不**写入任何密钥；
//! `config_snapshot` 只存端点/模型/模式等非凭据字段，`base_url` 允许入库但禁止含 query 凭据。

pub mod resume;

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::model_client::HistoryItem;

/// 会话文件目录名（04 §3.1：`<data_root>/rollouts/<agent_id>.jsonl`）。
pub const ROLLOUT_DIR: &str = "rollouts";

/// 会话建档时固化的配置快照。字段全是字符串以保证**跨版本可读**：
/// 枚举改名/拆分不该让历史档案解析失败（resume 依赖它）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", default)]
pub struct ConfigSnapshot {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub sandbox: String,
    pub approval: String,
    pub workspace_root: String,
}

/// rollout 的一行。判别标签 `type` 用 snake_case，与 `EventMsg` 同一套约定（04 §1）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RolloutItem {
    /// 文件首行，必须存在（04 §3.1）。
    SessionMeta {
        session_id: String,
        agent_id: String,
        /// 根代理为 null；子代理填父 agent_id（M5 用）。
        #[serde(default)]
        parent_id: Option<String>,
        /// Unix 毫秒。
        created_at: u64,
        config_snapshot: ConfigSnapshot,
    },
    /// 模型侧的一条完整记录。刻意用命名载荷而不是 newtype：
    /// `HistoryItem` 自身是内部标签枚举，newtype 变体与之嵌套会产生 `type` 键冲突。
    ResponseItem {
        item: HistoryItem,
        /// 事件落档时刻（Unix 毫秒）。
        ///
        /// `0` = 旧档案没有记录时刻（A0-4）：读侧必须标"时刻不可信"，
        /// 绝不能用导出/回放那一刻冒充事件发生时刻。
        #[serde(default)]
        at_ms: u64,
    },
    /// 压缩锚点：`[range_start, range_end)` 区间的历史被 summary 取代（SH-032 用）。
    Compacted {
        range_start: usize,
        range_end: usize,
        summary: String,
    },
    /// 父文件中指向子代理 rollout 的引用（SH-056 用）。
    SubagentRef { child_id: String, rollout_path: String },
    /// 会话内权限档位切换（对齐 DSH 的 `sandbox/mode` 会话事件）。
    ///
    /// **只落档案，不碰 `config.toml`**：会话内切档只影响本会话，重进会话靠这条
    /// 事件回放当前档位（进程在不在都不影响）。部署默认仍由设置页写 config。
    /// 最后一条生效——切多次就是多条事件，不做原地改写（档案 append-only）。
    SandboxMode { mode: String, at_ms: u64 },
    /// 会话内权限**预设**切换（`/permission <preset>` 的宿主面，2026-11-11）。
    ///
    /// 与 [`RolloutItem::SandboxMode`] 的关系：预设绑定 `(sandbox, approval)` 两维，
    /// 新档案只写这一条；`sandbox_mode` 保留为旧档案的读取兼容（`last_permission_preset`
    /// 在没有预设事件时回落到它）。`preset` 存 id（如 `danger-full-access`），
    /// 不存两维——id 才是用户选的那个东西，两维展开会让「自定义组合」无法表达。
    PermissionPreset { preset: String, at_ms: u64 },
    /// 退出流程里的“强制中止”留痕：取消信号已发，但等待超时后进程仍要退出。
    ///
    /// 为什么必须有这一条：直接 kill 会在档案里留下一条没人能解释的中断；
    /// 这一行把“为什么断”写清楚，resume 与审计才有据可查。
    /// **只在等待超时、被迫退出时写**；正常取消走 `TurnEnd`，不写这里。
    AbortNote { reason: String, at_ms: u64 },
    /// 回合结局（A0-3）：档案必须如实记录成功/失败/取消/超时，导出与统计都靠它。
    ///
    /// 旧档案没有这一行——那是历史事实，读侧只能标 `unknown`，不许默认成成功。
    TurnEnd {
        turn: u32,
        /// `completed` / `cancelled` / `failed` / `budget_exhausted`。
        reason: String,
        at_ms: u64,
        /// 失败时的结构化原因（分类码 + 脱敏后的端点原文）。
        #[serde(default)]
        error: Option<crate::events::TurnErrorInfo>,
    },
    /// 本次任务实例实际下发的系统提示词（原文；落盘前由 [`RolloutWriter::append`] 统一脱敏）。
    ///
    /// 只在**内容指纹变化**时追加，因此同一 run 的多轮不会重复落盘。
    /// `sha256` 对**落盘后的文本**计算：落盘文本经过脱敏过滤器，若对原文计算，
    /// resume 自洽校验会把含密钥的正常档案误判为损坏。
    SystemPrompt {
        /// 实际发给模型的文本（内存态原文）。
        text: String,
        /// 落盘文本的 sha256（小写十六进制）。
        sha256: String,
        /// 落档时刻（Unix 毫秒）。与 `turn` 一起定位跨 run 的记录。
        at_ms: u64,
        /// 记录时的轮次（本次 run 内从 1 起）。
        #[serde(default)]
        turn: u32,
    },
}

/// 系统提示词落盘文本的 sha256。
///
/// 调用方必须传入**将要落盘的那份文本**（即经 `config::redact` 之后的内容），
/// 否则 resume 侧 `sha256(存档文本) == sha256 字段` 会失败。
pub fn sha256_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

#[derive(Debug, thiserror::Error)]
pub enum RolloutError {
    #[error("rollout 目录不可用: {0}")]
    Io(String),
    #[error("rollout 文件已存在，拒绝覆盖（agent_id 必须唯一）: {0}")]
    AlreadyExists(String),
    #[error("首行不是 session_meta: {0}")]
    NoSessionMeta(String),
    /// 档案不可用：首行不是/读不出 `session_meta`，或重放期发现结构性矛盾（如压缩锚点越界）。
    /// 中间坏行不再走这个变体——它们被跳过并记入 [`Rollout::damaged_lines`]。
    #[error("rollout 第 {line} 行不可用: {message}")]
    Corrupted { line: usize, message: String },
}

/// 已打开的 rollout 追加写入器。`Mutex<File>` 让循环里的多处写入点能共用同一个 writer
/// 而不必关心调用方在哪个线程——工具执行与事件广播都可能落在不同任务上。
#[derive(Debug)]
pub struct RolloutWriter {
    path: PathBuf,
    file: Mutex<File>,
}

impl RolloutWriter {
    /// 建档：创建 `<dir>/<agent_id>.jsonl` 并写入首行 SessionMeta。
    /// 已存在则拒绝——覆盖历史档案是不可逆的数据丢失，绝不静默进行。
    pub fn create(dir: &Path, meta: &RolloutItem) -> Result<Self, RolloutError> {
        let (_, file_name) = match meta {
            RolloutItem::SessionMeta { agent_id, .. } => (agent_id.clone(), format!("{agent_id}.jsonl")),
            other => return Err(RolloutError::NoSessionMeta(format!("建档首行必须是 session_meta，收到 {:?}", other))),
        };
        std::fs::create_dir_all(dir).map_err(|e| RolloutError::Io(format!("{dir:?}: {e}")))?;
        let path = dir.join(&file_name);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    RolloutError::AlreadyExists(format!("{}", path.display()))
                } else {
                    RolloutError::Io(format!("{path:?}: {e}"))
                }
            })?;
        let writer = Self { path, file: Mutex::new(file) };
        writer.append(meta)?;
        Ok(writer)
    }

    /// 以追加方式重新打开已有档案（resume 用，SH-031）。
    ///
    /// 校验首行确实是 `session_meta` 才允许续写：往一个没有建档的文件里追加，
    /// 产出的是一份 resume 时才发现读不了的档案，错误被推迟到最难定位的时刻。
    pub fn open_existing(path: &Path) -> Result<Self, RolloutError> {
        let head = File::open(path)
            .map_err(|e| RolloutError::Io(format!("{path:?}: {e}")))?;
        let first_line = BufReader::new(head)
            .lines()
            .next()
            .ok_or_else(|| RolloutError::NoSessionMeta(format!("{}", path.display())))?
            .map_err(|e| RolloutError::Io(format!("{}", e)))?;
        let first: RolloutItem = serde_json::from_str(&first_line)
            .map_err(|e| RolloutError::Corrupted { line: 1, message: e.to_string() })?;
        if !matches!(first, RolloutItem::SessionMeta { .. }) {
            return Err(RolloutError::NoSessionMeta(format!("{}", path.display())));
        }
        let file = OpenOptions::new()
            .append(true)
            .open(path)
            .map_err(|e| RolloutError::Io(format!("{path:?}: {e}")))?;
        Ok(Self { path: path.to_path_buf(), file: Mutex::new(file) })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 追加一行并立即 flush（设计选择 1）。
    /// 落盘前过脱敏过滤器（docs/01 §438）：rollout 与日志同级敏感，用户把密钥
    /// 粘进任务文本这类情况只能在这里兜住。
    pub fn append(&self, item: &RolloutItem) -> Result<(), RolloutError> {
        let line = serde_json::to_string(item).map_err(|e| RolloutError::Io(format!("序列化失败: {e}")))?;
        let line = crate::config::redact(&line);
        let mut file = self.file.lock().map_err(|_| RolloutError::Io("rollout 写入锁中毒".into()))?;
        file.write_all(line.as_bytes()).map_err(|e| RolloutError::Io(format!("{}", e)))?;
        file.write_all(b"\n").map_err(|e| RolloutError::Io(format!("{}", e)))?;
        file.flush().map_err(|e| RolloutError::Io(format!("flush 失败: {e}")))
    }
}

/// 读档案里**最后一条**档位事件的 wire 值；没有该事件（旧档案 / 从没切过）返回 None。
///
/// 用子串预筛再解析：长会话档案里逐行 `serde_json` 全量解析的代价是 O(全部历史)，
/// 而这个查询在每次打开会话/读投影时都会发生。
pub fn last_sandbox_mode(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut last = None;
    for line in BufReader::new(file).lines() {
        let line = line.ok()?;
        if !line.contains("\"sandbox_mode\"") {
            continue;
        }
        if let Ok(RolloutItem::SandboxMode { mode, .. }) = serde_json::from_str::<RolloutItem>(&line) {
            last = Some(mode);
        }
    }
    last
}

/// 读档案里**最后一条**权限预设事件；没有该事件（旧档案 / 从没切过）返回 None。
///
/// 旧档案只有 `sandbox_mode` 事件：调用方拿到 `None` 后再用 [`last_sandbox_mode`] +
/// 当前 config 的审批档合成一个「自定义」预设。回落不在这里做，因为本函数只回答
/// 「档案里写了什么」，合成需要 config，属于装配层。
pub fn last_permission_preset(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut last = None;
    for line in BufReader::new(file).lines() {
        let line = line.ok()?;
        if !line.contains("\"permission_preset\"") {
            continue;
        }
        if let Ok(RolloutItem::PermissionPreset { preset, .. }) = serde_json::from_str::<RolloutItem>(&line) {
            last = Some(preset);
        }
    }
    last
}

/// 读档案里**最后一条**系统提示词记录的指纹；没有记录（旧档案 / 从未落档）返回 None。
///
/// 用于 resume 后的版本漂移检测（R2）：漂移只告警，不拒绝启动——换模型、换档位、
/// 工作区变化都会合法地产生新提示词。子串预筛的理由同 [`last_sandbox_mode`]。
pub fn last_prompt_sha(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut last = None;
    for line in BufReader::new(file).lines() {
        let line = line.ok()?;
        if !line.contains("\"type\":\"system_prompt\"") {
            continue;
        }
        if let Ok(RolloutItem::SystemPrompt { sha256, .. }) =
            serde_json::from_str::<RolloutItem>(&line)
        {
            last = Some(sha256);
        }
    }
    last
}

/// 读回来的档案：`items` 含首行 meta 之后的全部条目，`torn_tail_lines` 记录被丢弃的半截写行数。
#[derive(Debug, Clone, PartialEq)]
pub struct Rollout {
    pub meta: RolloutItem,
    pub items: Vec<RolloutItem>,
    /// 末行不完整（崩溃时正在写）时被丢弃的行数。正常应为 0 或 1。
    pub torn_tail_lines: usize,
    /// 中间坏行（JSON 解析失败或单行非法 UTF-8）的 1-based 行号；空 = 档案完好。
    /// 条数就是 `len()`——调用方据此在界面/日志说明"这份历史不完整"。
    pub damaged_lines: Vec<usize>,
}

/// 读取档案。规则见文件头设计选择 2：中间坏行跳过但**记录并告警**，首行坏与末行撕裂
/// 分别按"没有 meta"与"半截写"处理。
pub fn read(path: &Path) -> Result<Rollout, RolloutError> {
    let file = File::open(path).map_err(|e| RolloutError::Io(format!("{path:?}: {e}")))?;
    let reader = BufReader::new(file);
    // 单行编码损坏（InvalidData）与"这一行 JSON 写坏了"同属可跳过的数据损坏，记为
    // 损坏行；其余 IO 错误是磁盘/句柄故障，跳过它会把后续行一起读歪，必须整份上报。
    // （审计 M6 防的是 `unwrap_or_default()` 把读错误变成空行再**悄悄**略过；这里的
    // 跳过是带行号计数的显式损坏，不是静默。）
    let mut raw_lines: Vec<Option<String>> = Vec::new();
    for line in reader.lines() {
        match line {
            Ok(text) => raw_lines.push(Some(text)),
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => raw_lines.push(None),
            Err(e) => return Err(RolloutError::Io(format!("{path:?}: {e}"))),
        }
    }

    // 半截写判定看**文件末尾有没有换行**，而不是看 JSON 能不能解析：
    // 崩溃时最后一段可能恰好是合法 JSON 的前缀，也可能碰巧完整——只有换行位置是可靠信号。
    let mut torn = 0usize;
    if !ends_newline(path) {
        match raw_lines.last() {
            Some(Some(text)) if text.trim().is_empty() => {
                raw_lines.pop();
            }
            Some(_) => {
                torn = 1;
                raw_lines.pop();
            }
            None => {}
        }
    }

    let mut parsed: Vec<(usize, RolloutItem)> = Vec::new();
    let mut damaged: Vec<usize> = Vec::new();
    for (idx, line) in raw_lines.iter().enumerate() {
        let number = idx + 1;
        let Some(text) = line else {
            damaged.push(number);
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<RolloutItem>(text) {
            Ok(item) => parsed.push((number, item)),
            Err(e) => {
                // 首行坏 = 没有 meta。跳过它只会把后面条目伪装成"一份没有开头的档案"，
                // 读出来仍然不能用，所以这里保持硬失败。
                if parsed.is_empty() {
                    return Err(RolloutError::Corrupted { line: number, message: e.to_string() });
                }
                damaged.push(number);
            }
        }
    }

    let (first_line, meta) = parsed
        .first()
        .cloned()
        .ok_or_else(|| RolloutError::NoSessionMeta("文件为空".into()))?;
    if !matches!(meta, RolloutItem::SessionMeta { .. }) {
        return Err(RolloutError::NoSessionMeta(format!("第 {} 行", first_line)));
    }
    let items = parsed.into_iter().skip(1).map(|(_, item)| item).collect();
    if !damaged.is_empty() {
        // 唯一收口：所有调用方（打开会话、搜索对账、resume、fork）都经这里，
        // 不会出现"某个入口悄悄少了几条历史却没人知道"。
        tracing::warn!(
            path = %path.display(),
            count = damaged.len(),
            lines = %damaged.iter().map(usize::to_string).collect::<Vec<_>>().join(","),
            "rollout 含无法解析的行：已跳过并保留其余条目，这份历史可能不完整"
        );
    }
    Ok(Rollout { meta, items, torn_tail_lines: torn, damaged_lines: damaged })
}

/// 会话列表的一行摘要（SH-033）。字段用 camelCase 出到前端，与 `get_settings` 同一风格。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub agent_id: String,
    /// 根会话为 None；fork 出来的会话带父 agent_id，列表要能显示血缘。
    pub parent_id: Option<String>,
    pub created_at: u64,
    /// 档案文件的修改时间（毫秒）。列表按它倒序——比 created_at 更贴近"最近在用"。
    pub updated_at: u64,
    pub model: String,
    pub workspace_root: String,
    /// 首条用户消息压成单行并截断；没有用户消息时为空串（不编造占位标题）。
    pub title: String,
    /// meta 之外的条目行数，前端用来区分"空会话"。
    pub item_count: usize,
    /// 空白会话（SH-1099 对齐上游 blank 语义：折叠前缀无 turn）：meta 之外没有任何
    /// 条目 = 用户还没发过首条任务。新建会话流程据此复用已有空白而不是无限新造
    /// （上游 createInWorkspace→sessions.create{sessionId: blank} 同口径）。
    pub blank: bool,
}

/// 标题口径与前端 `ChatView::sessionTitle` 一致：空白压成单空格、按字符数取 40。
fn summarize_title(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 40 {
        format!("{}…", flat.chars().take(40).collect::<String>())
    } else {
        flat
    }
}

/// 列出目录下的会话摘要，按更新时间倒序。
///
/// **这里的容错口径与 `read()` 刻意不同**，不是自相矛盾：`read()` 重建的是要继续跑的
/// 历史，中间坏行会被跳过并**计数上报**、尾部配对不完整的条目由 `resume` 截断；
/// 而列表是**摘要视图**，某一行解析不出来最多是标题缺一截，不影响任何后续语义。
/// 一个坏档案不该让整页列表打不开，所以这里逐行尽力、坏文件跳过。真正打开会话时仍走
/// `read()`，损坏照样被计数并被上层呈现给用户。
pub fn list(dir: &Path) -> Vec<SessionSummary> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let titles = read_titles(dir);
    let mut out: Vec<SessionSummary> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                return None;
            }
            summarize(&path).map(|mut summary| {
                // 用户改过的标题优先于派生标题：派生只该在没人起名字时兜底
                if let Some(custom) = titles.get(&summary.agent_id) {
                    summary.title = custom.clone();
                }
                summary
            })
        })
        .collect();
    out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| a.agent_id.cmp(&b.agent_id)));
    out
}

/// 用户改过的会话标题（`<rollouts>/titles.json`）。
///
/// 为什么不写进档案首行：rollout 是**只追加**的模型转录，`read()` 的准入条件就是首行
/// 必须为 `session_meta`，而且写入方可能正持有文件句柄——为一个显示标题去原子重写首行，
/// 等于拿转录的完整性冒险。标题是界面元数据，旁边存一份即可；`list()` 只扫 `.jsonl`，
/// 这份文件不会被当成会话收进列表。
pub const TITLES_FILE: &str = "titles.json";

/// 标题长度上限（按字符数）。超界报错而不是截断：截断会让用户以为侧栏显示的是他打的
/// 那串，实际是另一串。
pub const TITLE_MAX_CHARS: usize = 120;

/// 读自定义标题。文件缺失或 JSON 坏了都当空表处理：列表是摘要视图，不该被一份元数据
/// 文件挡住——那会让"改标题失败"升级成"整个会话列表打不开"。
pub fn read_titles(dir: &Path) -> std::collections::BTreeMap<String, String> {
    let Ok(raw) = std::fs::read(dir.join(TITLES_FILE)) else {
        return Default::default();
    };
    serde_json::from_str(&String::from_utf8_lossy(&raw)).unwrap_or_default()
}

/// 会话归档/置顶/手动排序的侧车状态（SH-100，`<rollouts>/session-order.json`）。
///
/// 为什么像 titles.json 一样放侧车而不是动档案：归档/置顶是**界面元数据**——
/// 归档会话的档案必须原样可读（unarchive 后回原位），往只追加的转录里写状态
/// 等于拿档案完整性换一个过滤器。集合是 registry-global（上游 workspace 域同
/// 口径），`session_order` 是单工作区的展示顺序（上游 per-workspace `sessionIds`
/// 的 SHANIU 单工作区等价物，缺省按更新时间倒序补位）。
pub const SESSION_ORDER_FILE: &str = "session-order.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionOrderState {
    pub archived_session_ids: Vec<String>,
    pub pinned_session_ids: Vec<String>,
    pub session_order: Vec<String>,
}

/// 读归档/置顶/顺序状态。文件缺失或 JSON 坏了都当空表：这是界面过滤器，
/// 不该挡住会话列表本身（与 titles.json 同一容错口径）。
pub fn read_session_order(dir: &Path) -> SessionOrderState {
    let Ok(raw) = std::fs::read(dir.join(SESSION_ORDER_FILE)) else {
        return SessionOrderState::default();
    };
    serde_json::from_str(&String::from_utf8_lossy(&raw)).unwrap_or_default()
}

/// 原子替换侧车（半截文件会被当成空表，用户的归档/置顶一起丢——同 titles）。
pub fn write_session_order(dir: &Path, state: &SessionOrderState) -> Result<(), String> {
    // 唯一临时名：固定的 `.tmp` 会让两个写者踩同一个文件（rename 失败 / last-write-wins）。
    let tmp = crate::config::temp_sibling(&dir.join(SESSION_ORDER_FILE));
    let body = serde_json::to_string_pretty(state).map_err(|e| format!("序列化失败: {e}"))?;
    std::fs::write(&tmp, body).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("写临时文件失败: {e}")
    })?;
    if let Err(e) = std::fs::rename(&tmp, dir.join(SESSION_ORDER_FILE)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("替换文件失败: {e}"));
    }
    Ok(())
}

/// `set_title` 的失败类型：`code` 给前端按类分派（`title-invalid` → DSH wire 的
/// `session/title-invalid`），`message` 给界面直接显示。不靠匹配中文措辞——那是把
/// 契约挂在文案上，改个字就断。
#[derive(Debug, serde::Serialize)]
#[serde(tag = "code", content = "message", rename_all = "kebab-case")]
pub enum RenameError {
    /// 标题本身不合法（超长等）。
    TitleInvalid(String),
    /// 会话档案不存在。
    NotFound(String),
    /// 其余失败（空 id、落盘 I/O）：前端按网关内部错误回显文案。
    Rejected(String),
}

/// 写自定义标题；`title` 去掉首尾空白后为空 = 清除自定义，回落到首条用户消息派生的标题。
/// 返回真正生效的标题（`None` = 已清除）。
///
/// 档案不存在时拒绝：给一个不存在的会话改名会留下一条没人认领的标题，将来真出现同
/// `agent_id` 的档案就莫名其妙带上了它——而那种档案是 `fork`/`resume` 造出来的，用户
/// 完全看不出标题不是自己起的。
pub fn set_title(dir: &Path, agent_id: &str, title: &str) -> Result<Option<String>, RenameError> {
    let id = agent_id.trim();
    if id.is_empty() {
        return Err(RenameError::Rejected("会话 id 为空，无法改名".into()));
    }
    if !dir.join(format!("{id}.jsonl")).is_file() {
        return Err(RenameError::NotFound(format!("会话档案不存在：{id}")));
    }
    let trimmed = title.trim();
    if trimmed.chars().count() > TITLE_MAX_CHARS {
        return Err(RenameError::TitleInvalid(format!(
            "标题最长 {TITLE_MAX_CHARS} 字，当前 {} 字",
            trimmed.chars().count()
        )));
    }
    let mut titles = read_titles(dir);
    let accepted = if trimmed.is_empty() {
        titles.remove(id);
        None
    } else {
        titles.insert(id.to_string(), trimmed.to_string());
        Some(trimmed.to_string())
    };
    // 原子替换：半截的 titles.json 会被当成空表，用户改过的所有标题一起丢。
    // 临时名唯一（pid + 序号）：固定的 `.tmp` 会让同刻的两次改名互相踩。
    let tmp = crate::config::temp_sibling(&dir.join(TITLES_FILE));
    let body = serde_json::to_string_pretty(&titles)
        .map_err(|e| RenameError::Rejected(format!("序列化标题失败: {e}")))?;
    std::fs::write(&tmp, body).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        RenameError::Rejected(format!("写标题临时文件失败: {e}"))
    })?;
    if let Err(e) = std::fs::rename(&tmp, dir.join(TITLES_FILE)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(RenameError::Rejected(format!("替换标题文件失败: {e}")));
    }
    Ok(accepted)
}

fn summarize(path: &Path) -> Option<SessionSummary> {
    let file = File::open(path).ok()?;
    let mut lines = BufReader::new(file).lines();
    let first = lines.next()?.ok()?;
    // 首行必须是 session_meta，否则这份档案不可用（与建档契约一致）。
    let RolloutItem::SessionMeta { agent_id, parent_id, created_at, config_snapshot, .. } =
        serde_json::from_str(&first).ok()?
    else {
        return None;
    };

    let mut title = String::new();
    let mut item_count = 0usize;
    for line in lines {
        let Ok(line) = line else { continue };
        if line.trim().is_empty() {
            continue;
        }
        item_count += 1;
        if !title.is_empty() {
            continue;
        }
        if let Ok(RolloutItem::ResponseItem { item: HistoryItem::Message { role, text, .. }, .. }) =
            serde_json::from_str(&line)
        {
            if role == crate::model_client::Role::User {
                title = summarize_title(&text);
            }
        }
    }

    let updated_at = path
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(created_at);

    Some(SessionSummary {
        agent_id,
        parent_id,
        created_at,
        updated_at,
        model: config_snapshot.model,
        workspace_root: config_snapshot.workspace_root,
        title,
        item_count,
        blank: item_count == 0,
    })
}

fn ends_newline(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = File::open(path) else { return true };
    let Ok(len) = f.metadata().map(|m| m.len()) else { return true };
    if len == 0 {
        return true;
    }
    if f.seek(SeekFrom::End(-1)).is_err() {
        return true;
    }
    let mut buf = [0u8; 1];
    f.read_exact(&mut buf).map(|_| buf[0] == b'\n').unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_client::{HistoryItem, Role};

    fn temp_dir() -> crate::test_support::TestTempDir {
        crate::test_support::TestTempDir::new("rollout")
    }

    fn meta(agent: &str) -> RolloutItem {
        RolloutItem::SessionMeta {
            session_id: "sess-1".into(),
            agent_id: agent.into(),
            parent_id: None,
            created_at: 1_700_000_000_000,
            config_snapshot: ConfigSnapshot {
                model: "m".into(),
                base_url: "https://x/v1".into(),
                ..Default::default()
            },
        }
    }

    /// 旧档案缺 `config_snapshot` 字段时必须照常读出（空串 = 未记录），不能判整份档案损坏：
    /// 这个结构的存在意义就是「跨版本可读」，缺字段按 `Default` 补是它成立的唯一方式。
    #[test]
    fn config_snapshot_missing_fields_read_as_empty_strings() {
        let line = r#"{"type":"session_meta","session_id":"s","agent_id":"a","created_at":1,"config_snapshot":{"model":"m"}}"#;
        let item: RolloutItem = serde_json::from_str(line).expect("旧档案缺字段必须仍可读");
        let RolloutItem::SessionMeta { config_snapshot, .. } = item else {
            panic!("首行应是 session_meta");
        };
        assert_eq!(config_snapshot.model, "m", "已写入的字段不能被默认值覆盖");
        assert_eq!(config_snapshot.provider, "");
        assert_eq!(config_snapshot.workspace_root, "");
    }

    #[test]
    fn create_writes_session_meta_as_first_line() {
        let dir = temp_dir();
        let agent = "agent-first";
        let w = RolloutWriter::create(&dir, &meta(agent)).unwrap();
        let raw = std::fs::read_to_string(w.path()).unwrap();
        let first = raw.lines().next().unwrap();
        let v: serde_json::Value = serde_json::from_str(first).unwrap();
        assert_eq!(v["type"], "session_meta", "首行必须是 session_meta 判别标签");
        assert_eq!(v["agent_id"], agent);
        assert_eq!(v["config_snapshot"]["base_url"], "https://x/v1");
    }

    #[test]
    fn refuses_to_overwrite_existing_agent_file() {
        let dir = temp_dir();
        let _w = RolloutWriter::create(&dir, &meta("dup")).unwrap();
        let err = RolloutWriter::create(&dir, &meta("dup")).unwrap_err();
        assert!(matches!(err, RolloutError::AlreadyExists(_)), "覆盖历史档案不可逆，必须拒绝：{err}");
    }

    /// 会话内切档**只落档案**：配置文件一个字节都不许动。
    /// 反向对照就是这个断言——没有它，"界面看起来变了"不能证明配置没被写。
    #[test]
    fn session_sandbox_event_never_touches_the_config_file() {
        let dir = temp_dir();
        let config = dir.join("config.toml");
        let original = b"[sandbox]\nmode = \"workspace_write\"\n";
        std::fs::write(&config, original).unwrap();

        let w = RolloutWriter::create(&dir, &meta("agent-mode")).unwrap();
        let path = w.path().to_path_buf();
        drop(w);
        let w = RolloutWriter::open_existing(&path).unwrap();
        w.append(&RolloutItem::SandboxMode { mode: "danger_full_access".into(), at_ms: 7 }).unwrap();
        w.append(&RolloutItem::SandboxMode { mode: "read_only".into(), at_ms: 8 }).unwrap();
        drop(w);

        assert_eq!(std::fs::read(&config).unwrap(), original, "切档写到了配置文件");
        // 最后一条生效（append-only，不做原地改写）
        assert_eq!(last_sandbox_mode(&path).as_deref(), Some("read_only"));
    }

    /// 旧档案里没有档位事件 → None（调用方回落部署默认），不是"读失败"。
    #[test]
    fn last_sandbox_mode_is_none_for_archives_without_the_event() {
        let dir = temp_dir();
        let w = RolloutWriter::create(&dir, &meta("agent-old")).unwrap();
        w.append(&RolloutItem::ResponseItem {
            item: crate::model_client::HistoryItem::Message {
                role: crate::model_client::Role::User, text: "任务".into(), reasoning: None,
            },
            at_ms: 0,
        })
        .unwrap();
        let path = w.path().to_path_buf();
        drop(w);
        assert_eq!(last_sandbox_mode(&path), None);
        assert_eq!(last_sandbox_mode(&dir.join("missing.jsonl")), None);
    }

    #[test]
    fn round_trip_preserves_items_in_order() {
        let dir = temp_dir();
        let w = RolloutWriter::create(&dir, &meta("agent-rt")).unwrap();
        let items = vec![
            RolloutItem::ResponseItem { item: HistoryItem::Message { role: Role::User, text: "任务".into(), reasoning: None }, at_ms: 1_000 },
            RolloutItem::ResponseItem { item: HistoryItem::FunctionCall { name: "read_file".into(), arguments: "{}".into(), call_id: "c1".into() }, at_ms: 1_500 },
            RolloutItem::ResponseItem { item: HistoryItem::FunctionCallOutput { call_id: "c1".into(), output: "内容".into() }, at_ms: 2_000 },
            RolloutItem::Compacted { range_start: 0, range_end: 2, summary: "前两轮摘要".into() },
            RolloutItem::SubagentRef { child_id: "child-1".into(), rollout_path: "rollouts/child-1.jsonl".into() },
        ];
        for it in &items {
            w.append(it).unwrap();
        }
        let back = read(&dir.join("agent-rt.jsonl")).unwrap();
        assert_eq!(back.meta, meta("agent-rt"));
        assert_eq!(back.items, items, "读回来的条目序列必须与写入完全一致");
        assert_eq!(back.torn_tail_lines, 0);
    }

    #[test]
    fn tolerates_torn_last_line_and_reports_middle_corruption() {
        let dir = temp_dir();
        let path = dir.join("agent-torn.jsonl");
        // 崩溃在写第三行的中途：末行没有换行且不是合法 JSON
        std::fs::write(
            &path,
            format!("{}\n{}\n{{\"type\":\"response_item\"",
                serde_json::to_string(&meta("t")).unwrap(),
                serde_json::to_string(&RolloutItem::ResponseItem {
                    item: HistoryItem::Message { role: Role::Assistant, text: "答".into(), reasoning: None },
                    at_ms: 0,
                })
                .unwrap(),
            ),
        )
        .unwrap();
        let back = read(&path).unwrap();
        assert_eq!(back.items.len(), 1, "半截写的那行应被丢弃");
        assert_eq!(back.torn_tail_lines, 1);
        assert!(back.damaged_lines.is_empty(), "末行撕裂不是中间坏行");

        // 同样的坏内容出现在中间（后面还有完整行）→ 跳过、其余读回、坏行行号必须可见。
        // 反向对照：修复前这里返回 Err，断言立刻红。
        std::fs::write(
            &path,
            format!("{}\n{{\"type\":\"bogus\"\n{}\n",
                serde_json::to_string(&meta("t")).unwrap(),
                serde_json::to_string(&RolloutItem::ResponseItem {
                    item: HistoryItem::Message { role: Role::Assistant, text: "答".into(), reasoning: None },
                    at_ms: 0,
                })
                .unwrap(),
            ),
        )
        .unwrap();
        let back = read(&path).unwrap();
        assert_eq!(back.items.len(), 1, "坏行之后的有效条目必须照常读回");
        assert_eq!(back.damaged_lines, vec![2], "坏行行号（1-based）必须报出来");
        assert_eq!(back.torn_tail_lines, 0);
    }

    /// 中间坏行不能变成"悄悄丢历史"：条数与行号都要能从返回值看到。
    #[test]
    fn multiple_damaged_lines_are_counted_and_located() {
        let dir = temp_dir();
        let path = dir.join("agent-damaged.jsonl");
        let good = serde_json::to_string(&RolloutItem::ResponseItem {
            item: HistoryItem::Message { role: Role::User, text: "有效".into(), reasoning: None },
            at_ms: 0,
        })
        .unwrap();
        std::fs::write(
            &path,
            format!(
                "{}\nnot-json\n{}\n{{\"type\":\"response_item\"}}\n{}\n",
                serde_json::to_string(&meta("d")).unwrap(),
                good,
                good,
            ),
        )
        .unwrap();
        let back = read(&path).unwrap();
        assert_eq!(back.items.len(), 2, "两条有效条目都要读回");
        assert_eq!(back.damaged_lines, vec![2, 4], "两处坏行的行号都要报");
    }

    /// 首行坏 = 没有 meta：跳过它只会产出一份无头档案，仍然不可用，必须硬失败。
    #[test]
    fn damaged_first_line_is_still_rejected() {
        let dir = temp_dir();
        let path = dir.join("agent-badhead.jsonl");
        std::fs::write(
            &path,
            format!(
                "not-json\n{}\n",
                serde_json::to_string(&RolloutItem::ResponseItem {
                    item: HistoryItem::Message { role: Role::User, text: "x".into(), reasoning: None },
                    at_ms: 0,
                })
                .unwrap()
            ),
        )
        .unwrap();
        assert!(matches!(read(&path).unwrap_err(), RolloutError::Corrupted { line: 1, .. }));
    }

    /// 单行非法 UTF-8 与"这一行 JSON 写坏"同类：跳过并计入损坏行，而不是把整份档案判死。
    /// 反向对照：修复前这里返回 `Io`，断言红。
    #[test]
    fn undecodable_line_is_reported_as_damage_not_swallowed() {
        let dir = temp_dir();
        let path = dir.join("agent-io-error.jsonl");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(serde_json::to_string(&meta("io")).unwrap().as_bytes());
        bytes.push(b'\n');
        // 非法 UTF-8：`lines()` 读取该行时返回 InvalidData
        bytes.extend_from_slice(&[0xff, 0xfe]);
        bytes.push(b'\n');
        bytes.extend_from_slice(
            serde_json::to_string(&RolloutItem::ResponseItem {
                item: HistoryItem::Message {
                    role: Role::Assistant,
                    text: "答".into(),
                    reasoning: None,
                },
                at_ms: 0,
            })
            .unwrap()
            .as_bytes(),
        );
        bytes.push(b'\n');
        std::fs::write(&path, bytes).unwrap();

        let back = read(&path).unwrap();
        assert_eq!(back.items.len(), 1, "解码失败行之后的有效条目必须读回");
        assert_eq!(back.damaged_lines, vec![2], "解码失败行必须显式报出来");
    }

    #[test]
    fn missing_or_wrong_first_line_is_rejected() {
        let dir = temp_dir();
        let empty = dir.join("agent-empty.jsonl");
        std::fs::write(&empty, "").unwrap();
        assert!(matches!(read(&empty).unwrap_err(), RolloutError::NoSessionMeta(_)));

        let no_meta = dir.join("agent-nometa.jsonl");
        std::fs::write(
            &no_meta,
            format!("{}\n",
                serde_json::to_string(&RolloutItem::ResponseItem {
                    item: HistoryItem::Message { role: Role::User, text: "x".into(), reasoning: None },
                    at_ms: 0,
                })
                .unwrap()),
        )
        .unwrap();
        assert!(matches!(read(&no_meta).unwrap_err(), RolloutError::NoSessionMeta(_)));
    }

    #[test]
    fn create_rejects_non_meta_first_item() {
        let dir = temp_dir();
        let err = RolloutWriter::create(
            &dir,
            &RolloutItem::ResponseItem {
                item: HistoryItem::Message { role: Role::User, text: "x".into(), reasoning: None },
                at_ms: 0,
            },
        )
        .unwrap_err();
        assert!(matches!(err, RolloutError::NoSessionMeta(_)), "建档只接受 session_meta：{err}");
    }

    /// 把档案的 mtime 钉成确定值：`list` 的排序判据是更新时间，靠写入先后碰运气会让测试变脆。
    fn set_mtime(path: &Path, millis: u64) {
        let times = std::fs::FileTimes::new()
            .set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(millis));
        let file = OpenOptions::new().write(true).open(path).unwrap();
        file.set_times(times).unwrap();
    }

    fn write_session(dir: &Path, agent: &str, first_task: &str) -> PathBuf {
        let writer = RolloutWriter::create(dir, &meta(agent)).unwrap();
        writer
            .append(&RolloutItem::ResponseItem {
                item: HistoryItem::Message { role: Role::User, text: first_task.into(), reasoning: None },
                at_ms: 0,
            })
            .unwrap();
        writer
            .append(&RolloutItem::ResponseItem {
                item: HistoryItem::Message { role: Role::Assistant, text: "收到".into(), reasoning: None },
                at_ms: 0,
            })
            .unwrap();
        writer.path().to_path_buf()
    }

    #[test]
    fn list_summarizes_meta_and_first_user_message() {
        let dir = temp_dir();
        write_session(&dir, "a-1", "把  端口\n改成 9090");
        let items = list(&dir);
        assert_eq!(items.len(), 1, "目录里只有一个档案：{items:?}");
        let one = &items[0];
        assert_eq!(one.agent_id, "a-1");
        assert_eq!(one.parent_id, None);
        assert_eq!(one.model, "m");
        // 标题：空白压成单空格，不是原样带换行
        assert_eq!(one.title, "把 端口 改成 9090");
        // item_count 不含首行 meta
        assert_eq!(one.item_count, 2);
    }

    #[test]
    fn list_orders_by_updated_at_descending() {
        let dir = temp_dir();
        let older = write_session(&dir, "old", "早先的任务");
        let newer = write_session(&dir, "new", "后来的任务");
        set_mtime(&older, 1_000);
        set_mtime(&newer, 2_000);
        let ids: Vec<_> = list(&dir).into_iter().map(|s| s.agent_id).collect();
        assert_eq!(ids, vec!["new".to_string(), "old".to_string()], "最近在用的排最前");
    }

    #[test]
    fn list_skips_file_without_session_meta_first_line() {
        let dir = temp_dir();
        write_session(&dir, "good", "正常的会话");
        // 首行不是 meta 的档案：列表跳过它，而不是让整页失败
        std::fs::write(dir.join("broken.jsonl"), "{\"type\":\"response_item\"}\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "不是档案").unwrap();
        let ids: Vec<_> = list(&dir).into_iter().map(|s| s.agent_id).collect();
        assert_eq!(ids, vec!["good".to_string()], "坏档案与非档案文件都不该拖垮列表");
    }

    #[test]
    fn list_survives_corrupt_middle_line_but_still_counts_it() {
        let dir = temp_dir();
        let path = write_session(&dir, "a", "第一条任务");
        // 摘要视图容错：中间坏行不报错（read() 才严格），但行数仍计入 item_count
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.push_str("this is not json\n");
        std::fs::write(&path, raw).unwrap();
        let one = &list(&dir)[0];
        assert_eq!(one.title, "第一条任务", "坏行不该影响已经取到的标题");
        assert_eq!(one.item_count, 3, "行数按物理行计，坏行留给 read() 去报");
        let archive = read(&path).expect("中间坏行不再判死整份档案");
        assert_eq!(archive.items.len(), 2, "两条有效条目照常读回");
        assert_eq!(archive.damaged_lines, vec![4], "坏行必须可见，不能悄悄丢");
    }

    #[test]
    fn list_title_truncates_by_chars_not_bytes() {
        let dir = temp_dir();
        let long = "中".repeat(60);
        write_session(&dir, "a", &long);
        let one = &list(&dir)[0];
        assert_eq!(one.title.chars().count(), 41, "40 字 + 省略号，按字符数截断");
        assert!(one.title.ends_with('…'));
    }

    #[test]
    fn custom_title_overrides_derived_and_clearing_falls_back() {
        let dir = temp_dir();
        write_session(&dir, "a", "第一条用户消息");
        assert_eq!(list(&dir)[0].title, "第一条用户消息", "没人改名时用派生标题");

        assert_eq!(set_title(&dir, "a", "  我的会话  ").unwrap().as_deref(), Some("我的会话"), "首尾空白不该进标题");
        assert_eq!(list(&dir)[0].title, "我的会话");

        // 空标题 = 清除自定义，回落到派生标题：界面"清空名字"不该留下一个空标题行
        assert_eq!(set_title(&dir, "a", "   ").unwrap(), None);
        assert_eq!(list(&dir)[0].title, "第一条用户消息");
    }

    #[test]
    fn set_title_rejects_unknown_agent_and_overlong_title() {
        let dir = temp_dir();
        write_session(&dir, "a", "正文");
        // 给不存在的会话改名会留下一条没人认领的标题：将来 fork/resume 造出同 id 的
        // 档案时，它会莫名其妙带上这个标题，而用户看不出标题不是自己起的。
        let err = set_title(&dir, "ghost", "标题").unwrap_err();
        assert!(matches!(err, RenameError::NotFound(ref m) if m.contains("不存在")), "{err:?}");
        assert!(!read_titles(&dir).contains_key("ghost"));

        let long = "字".repeat(TITLE_MAX_CHARS + 1);
        let err = set_title(&dir, "a", &long).unwrap_err();
        assert!(matches!(err, RenameError::TitleInvalid(ref m) if m.contains(&TITLE_MAX_CHARS.to_string())), "{err:?}");
        // 拒绝的写入必须一点都没落盘
        assert!(read_titles(&dir).is_empty());
    }

    /// 序列化形状是前端契约：适配层按 `code` 分派（`title-invalid` → DSH wire 的
    /// `session/title-invalid`），措辞怎么改都不该破坏这个形状。
    #[test]
    fn rename_error_serializes_to_tagged_code_and_message() {
        assert_eq!(
            serde_json::to_value(RenameError::TitleInvalid("标题最长 120 字，当前 121 字".into())).unwrap(),
            serde_json::json!({ "code": "title-invalid", "message": "标题最长 120 字，当前 121 字" })
        );
        assert_eq!(
            serde_json::to_value(RenameError::NotFound("会话档案不存在：a".into())).unwrap(),
            serde_json::json!({ "code": "not-found", "message": "会话档案不存在：a" })
        );
        assert_eq!(
            serde_json::to_value(RenameError::Rejected("会话 id 为空，无法改名".into())).unwrap(),
            serde_json::json!({ "code": "rejected", "message": "会话 id 为空，无法改名" })
        );
    }

    /// `titles.json` 不是档案：它既不能被扫成会话，也不能因为坏了就挡住整个列表。
    #[test]
    fn titles_sidecar_is_not_a_session_and_a_broken_one_does_not_break_the_list() {
        let dir = temp_dir();
        write_session(&dir, "a", "正文");
        set_title(&dir, "a", "改名后的标题").unwrap();
        assert_eq!(list(&dir).len(), 1, "侧车文件不该被当成第二个会话");
        assert_eq!(list(&dir)[0].title, "改名后的标题");

        std::fs::write(dir.join(TITLES_FILE), "{ not json").unwrap();
        let rows = list(&dir);
        assert_eq!(rows.len(), 1, "标题文件坏了也不能让列表打不开");
        assert_eq!(rows[0].title, "正文", "读不到自定义标题就回落派生标题，而不是报错");
    }

    #[test]
    fn list_of_missing_dir_is_empty_not_an_error() {
        assert_eq!(list(Path::new("Z:\\definitely-not-here\\shaniu")), Vec::<SessionSummary>::new());
    }

    // ---- 归档/置顶/顺序侧车（SH-100）----

    #[test]
    fn session_order_round_trips_and_tolerates_missing_or_broken_file() {
        let dir = temp_dir();
        assert_eq!(read_session_order(&dir), SessionOrderState::default(), "缺失 = 空状态");

        let state = SessionOrderState {
            archived_session_ids: vec!["a".into()],
            pinned_session_ids: vec!["b".into(), "c".into()],
            session_order: vec!["c".into(), "a".into(), "b".into()],
        };
        write_session_order(&dir, &state).unwrap();
        assert_eq!(read_session_order(&dir), state, "往返必须无损：重启后归档/置顶/顺序全靠它");

        std::fs::write(dir.join(SESSION_ORDER_FILE), "{ not json").unwrap();
        assert_eq!(read_session_order(&dir), SessionOrderState::default(), "坏文件当空表，不挡列表");
    }

    /// 归档与置顶的互斥写是上层命令的职责，这里只钉一件事：同一次写里两个集合
    /// 必须一起落盘（上层"归档即掉 pin"依赖这个原子性）。
    #[test]
    fn archive_and_pin_sets_persist_together() {
        let dir = temp_dir();
        let state = SessionOrderState {
            archived_session_ids: vec!["x".into()],
            pinned_session_ids: vec![],
            session_order: vec![],
        };
        write_session_order(&dir, &state).unwrap();
        let mut back = read_session_order(&dir);
        back.pinned_session_ids.push("y".into());
        write_session_order(&dir, &back).unwrap();
        let final_state = read_session_order(&dir);
        assert_eq!(final_state.archived_session_ids, vec!["x".to_string()]);
        assert_eq!(final_state.pinned_session_ids, vec!["y".to_string()]);
    }

    /// 固定临时名会被另一个写者占住：把旧代码用的 `titles.json.tmp` /
    /// `session-order.json.tmp` 占成**目录**（模拟"另一个写者正持有这个临时名"），
    /// 写侧车仍必须成功。旧代码写固定临时文件会失败 → 本用例红。
    #[test]
    fn sidecar_writes_use_unique_temps_and_survive_occupied_legacy_names() {
        let dir = temp_dir();
        write_session(&dir, "a", "任务");
        std::fs::create_dir(dir.join(format!("{TITLES_FILE}.tmp"))).unwrap();
        std::fs::create_dir(dir.join(format!("{SESSION_ORDER_FILE}.tmp"))).unwrap();

        let accepted = set_title(&dir, "a", "新标题").expect("唯一临时名不该被固定名占用影响");
        assert_eq!(accepted.as_deref(), Some("新标题"));
        assert_eq!(read_titles(&dir).get("a").map(String::as_str), Some("新标题"));

        let state = SessionOrderState {
            archived_session_ids: vec!["a".into()],
            pinned_session_ids: vec![],
            session_order: vec!["a".into()],
        };
        write_session_order(&dir, &state).expect("唯一临时名不该被固定名占用影响");
        assert_eq!(read_session_order(&dir), state);
    }
}
