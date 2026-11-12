//! TOML 配置与数据目录解析（docs §9.3）。
//!
//! 解析优先级：`SHANIU_DATA_DIR` 环境变量 → exe 同目录 `portable.marker`（便携模式）
//! → `%APPDATA%\SHANIU` + `%LOCALAPPDATA%\SHANIU`（安装模式）。
//! 便携模式两种形态共用同一代码路径，差异仅在数据根目录。
//!
//! 数据边界（AGENTS §3）：本模块是未来 `secrets\`（DPAPI 加密）的唯一读写入口；
//! DPAPI 存取随 M2 权限模块接入（M0 无模型调用、无密钥使用场景）。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{OnceLock, RwLock};
use std::time::SystemTime;

use regex::Regex;
use serde::{Deserialize, Serialize};
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::fmt::MakeWriter;

/// 内置模型能力表（SH-132）：`config` 层的默认值数据表，不是业务模块。
pub mod model_catalog;
/// 内置服务商表：出厂默认服务商与展示次序的数据层来源。
pub mod builtin_providers;
/// 同 `<data_root>` 单实例保护：命名互斥体按数据目录 hash 区分（2026-11-11）。
pub mod single_instance;
/// 工作区根唯一判据：等于/祖先/数据根内兄弟目录一律拒绝（任务 7，2026-11-11）。
pub mod workspace_gate;
/// Windows 路径规范化原语：剥 verbatim 前缀 / 取最深已存在祖先（FF-1 收敛，2026-11-13）。
pub mod path_norm;

pub const DATA_DIR_ENV: &str = "SHANIU_DATA_DIR";
pub const PORTABLE_MARKER: &str = "portable.marker";
pub const CONFIG_FILE: &str = "config.toml";
const APP_DIR_NAME: &str = "SHANIU";

/// 产品目录布局（docs §9.1/§9.2）：两种发行形态共用同一套子目录，仅根目录不同。
pub const SECRETS_DIR: &str = "secrets";
pub const SETTINGS_BACKUP_DIR: &str = "settings-backup";
pub const SESSIONS_DIR: &str = "sessions";
pub const ROLLOUTS_DIR: &str = "rollouts";
pub const LOGS_DIR: &str = "logs";
pub const CACHE_DIR: &str = "cache";
pub const CRASH_DIR: &str = "crash";
/// 默认工作区：安装版与便携版都放在数据根下（`data\Workspace`），启动时随目录结构一并创建。
pub const WORKSPACE_DIR: &str = "Workspace";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub model: ModelConfig,
    pub sandbox: SandboxConfig,
    pub approval: ApprovalConfig,
    pub workspace: WorkspaceConfig,
    pub agent: AgentConfig,
    /// 写后验证闸门（04 §4 `[verify]`，additive）。缺段 = 默认开启 + enforce。
    pub verify: VerifyConfig,
    /// 会话标题自动生成（04 §4 `[session_title]`）。缺段 = 默认开启。
    pub session_title: SessionTitleConfig,
    /// 可选的服务商列表（04 §4 `[[providers]]`）。**为空就是旧行为**：生效档直接取
    /// `[model]` 段。这是 additive 扩展而不是替换——已有 config.toml 不改一个字也能跑，
    /// 而逼用户迁移配置文件等于让升级即失效。
    pub providers: Vec<ProviderProfile>,
    /// 额外工作区（SH-099，additive `[[workspaces]]`）。默认工作区来自
    /// `[workspace].root`（workspaceId 固定 `default-workspace`，不进本列表），
    /// 这里只存用户通过侧栏「添加工作区」注册的那几家。为空 = 旧单工作区行为。
    pub workspaces: Vec<WorkspaceProfile>,

    /// MCP 客户端（SH-060，additive `[[mcp.servers]]`）。空 = 未启用，行为与旧版逐字一致。
    pub mcp: McpConfig,

    /// 当前生效的服务商 id；空或指不到任何一项时同样回落 `[model]`。
    pub active_provider: String,
    /// 服务商文档的修订号（设置页乐观锁的比对基准），每次写入 +1。
    pub provider_revision: u64,
}

/// 一家额外工作区（SH-099）。`id` 由宿主生成（ULID）；`title` 缺省 = 路径目录名
/// （上游 defaultWorkspaceTitle 同口径）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceProfile {
    pub id: String,
    pub path: String,
    pub title: String,
}

impl WorkspaceProfile {
    /// 展示名：显式 title 优先，否则回落路径目录名（盘根回落根拼写，上游同口径）。
    pub fn display_title(&self) -> String {
        if !self.title.trim().is_empty() {
            return self.title.trim().to_string();
        }
        std::path::Path::new(&self.path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| self.path.clone())
    }
}

/// 推理档位全集（SH-096，上游 llm-pi-ai `THINKING_LEVELS`）。设置页的档位编辑器
/// 与后端校验共用这一份清单——两处各写一份迟早漂移。
pub const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// 推理档位的默认预选档（没选过档位时回落这一档）。
///
/// 这是**唯一**的默认档真源：后端 `effective_model` 的请求侧回落、席位的
/// `defaultEffort`、设置页的档位编辑器都从这里来（经 `get_settings` 的
/// `thinkingLevels`/`defaultReasoningLevel` 下发），不再各处写一个 "high" 字面量。
pub const DEFAULT_REASONING_LEVEL: &str = "high";

/// 能力项来源（SH-132，04 §4 `[providers.models.capabilities.<item>].source`）。
///
/// 声明值的优先级（高 → 低）：**手动 > 端点元数据 > 内置表 > 推测（家族）**。
/// 探针不参与声明排位，它只写 `verified`——两者不能互相覆盖，否则"填错了"会被
/// 一次探测洗成"对的"。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilitySource {
    /// 没人声明过（旧配置、自动获取失败）。
    #[default]
    Unknown,
    /// 端点元数据（`GET /models` 或 `/models/{id}`）。
    Endpoint,
    /// 随仓库发布的内置表精确命中（`resources/model-catalog.json`）。
    Builtin,
    /// 内置表家族正则命中：**推测**，与「内置表」区分；探针结论优先于它。
    Inferred,
    /// 实测探针写入（只作声明外的补充，不覆盖声明值）。
    Probe,
    /// 用户在设置页手填，覆盖一切自动来源。
    Manual,
}

/// 一项能力的声明与实测记录（SH-132）。
///
/// **值只有一份**：`vision` 取模型行的 `input` 含不含 `image`，
/// `context_window`/`max_output` 取 `context_window`/`max_tokens`，`reasoning`
/// 的档位取 `reasoning_efforts`。这里只存"谁说它行、测过没有"，避免同一事实
/// 落两处后各自漂移（SH-096 的串段事故就是这么来的）。
///
/// 唯一例外是 `tools` / `builtin_web_search` 这类**没有专属字段**的布尔项：
/// 它们的声明值存在 `value`。以后新增 `audio` / `embeddings` / `prompt_caching`
/// 同理——map 的键不做白名单，未知项原样保留并透传到设置页，新增能力不需要
/// 改配置结构。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CapabilityRecord {
    /// 无专属字段的布尔项声明值。`false` 也是有效声明（"明确没有"），
    /// 与 `None`（没声明）在界面上必须能区分。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<bool>,
    pub source: CapabilitySource,
    /// 实测结论：`None` = 未验证，`Some(true)` = 实测通过，`Some(false)` = 与声明不符。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified: Option<bool>,
    /// 实测通过的推理档（仅 `reasoning` 项）：档位选择器只列这些档。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub verified_levels: Vec<String>,
    /// 端点声明的可用档位（仅 `reasoning` 项，来自 `effort.supported_levels`）。
    /// 可选档 = 声明 ∩ 端点这份清单——端点明说没这档，界面上就不该给。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub endpoint_levels: Vec<String>,
    /// 最近一次实测时间（Unix 毫秒），供界面显示"上次实测"。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_at_ms: Option<u64>,
    /// 实测证据的人话摘要（"两档 reasoning_tokens 均为 0"这类），写日志与界面都直接用它。
    #[serde(skip_serializing_if = "String::is_empty")]
    pub evidence: String,
}

impl CapabilityRecord {
    /// 端点/手动写入一条布尔声明。`Probe` 来源不允许走这里——探针只写 `verified`。
    pub fn declared(value: bool, source: CapabilitySource) -> Self {
        Self { value: Some(value), source, ..Default::default() }
    }
}

/// 设置页里一行模型档位（04 §4 `[[providers.models]]`）。
///
/// 运行期消费 `id`（`ProviderProfile::model` 就是它当前选中的那一行）与
/// `reasoning_efforts`（SH-096：推理档位声明）；其余字段是设置页写面的原样留存。
/// 存下来但不生效，比"用户加了四行、重载只剩一行"更接近事实——后者的代价是
/// 用户以为自己配好了。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelProfile {
    /// 传给端点的模型 id。
    pub id: String,
    /// 选择器里的显示名。
    pub name: String,
    /// 上下文窗口（token）。compaction 目前用固定预算，这一行只作显示与留存。
    /// `alias` 不是照顾拼写：`save_providers` 收的是 JSON，前端送来 `contextWindow`
    /// 而这里只认 `context_window` 的话，serde 会**静默**按缺省给 0——设置页显示
    /// 配了 128K、后端读到一个空窗口。落盘仍然写 snake_case（与 04 §4 同源）。
    #[serde(alias = "contextWindow")]
    pub context_window: u32,
    /// 单次响应输出上限（token）。
    #[serde(alias = "maxTokens")]
    pub max_tokens: u32,
    /// 该模型接受的输入模态（`text` / `image`）。
    pub input: Vec<String>,
    /// 推理档位声明（SH-096）：档位名 → 发往 `reasoning_effort` 的 wire 值。
    /// 空串值 = 该档不发任何字段（off 档的语义）。空 map = 未声明推理能力，
    /// 席位不出档位选择。上游还有 `false`（显式声明非推理模型）——SHANIU 没有
    /// 可继承的目录，false 与缺席等价，前端写面把 false 归一成空 map。
    #[serde(alias = "reasoningEfforts")]
    pub reasoning_efforts: std::collections::BTreeMap<String, String>,
    /// 能力声明与实测记录（SH-132）：能力项 id → 来源/实测结论。
    /// 键不做白名单（扩展位：`audio`/`embeddings`/`prompt_caching` 以后直接加），
    /// 值本身取本行的 `input`/`context_window`/`max_tokens`/`reasoning_efforts`。
    #[serde(default)]
    pub capabilities: std::collections::BTreeMap<String, CapabilityRecord>,
}

impl ModelProfile {
    /// 内置表命中（精确或家族）。`base_url` 必填：同一个模型 id 在不同端点上
    /// 能力并不相同（`deepseek-flash` 在 DeepSeek 有视觉、在 Sensenova 没有）。
    fn builtin(&self, base_url: &str) -> Option<model_catalog::BuiltinCapabilities> {
        model_catalog::lookup(base_url, &self.id).map(|hit| hit.capabilities())
    }

    /// 声明的视觉能力。优先级：能力项显式值 > `input` 字段 > 内置表。
    ///
    /// 为什么保留三条来源：`input` 是设置页既有的"输入模态"字段（旧配置全靠它），
    /// 能力项是端点/手动覆盖的写入点，内置表是没配过也没抓过元数据时的兜底。
    /// 三者都不成立 = 没声明。
    pub fn declared_vision(&self, base_url: &str) -> bool {
        if let Some(value) = self.capabilities.get("vision").and_then(|cap| cap.value) {
            return value;
        }
        if self.input.iter().any(|m| m == "image") {
            return true;
        }
        self.builtin(base_url)
            .and_then(|builtin| builtin.input)
            .is_some_and(|input| input.iter().any(|m| m == "image"))
    }

    /// 视觉是否**生效**：声明有且实测没否定。未验证不拦（用户可用），实测不符才拦。
    pub fn vision_effective(&self, base_url: &str) -> bool {
        self.declared_vision(base_url)
            && self.capabilities.get("vision").and_then(|cap| cap.verified) != Some(false)
    }

    /// 可选的推理档，按 `THINKING_LEVELS` 顺序。
    ///
    /// 唯一真源仍是 `reasoning_efforts`（档位 → wire 值）；内置表只在**本行没声明**
    /// 时给出档位名。最后与端点 `effort.supported_levels` 取交集：端点明说没这档，
    /// 界面上就不该给，选了也不该发出去被静默忽略。
    pub fn declared_reasoning_levels(&self, base_url: &str) -> Vec<String> {
        let mut levels: Vec<String> = THINKING_LEVELS
            .iter()
            .copied()
            .filter(|level| *level != "off")
            .filter(|level| {
                self.reasoning_efforts
                    .get(*level)
                    .is_some_and(|wire| !wire.trim().is_empty())
            })
            .map(str::to_string)
            .collect();
        if levels.is_empty() {
            if let Some(builtin) = self.builtin(base_url).and_then(|b| b.reasoning_levels) {
                levels = THINKING_LEVELS
                    .iter()
                    .copied()
                    .filter(|level| builtin.iter().any(|l| l == *level))
                    .map(str::to_string)
                    .collect();
            }
        }
        if let Some(endpoint) = self
            .capabilities
            .get("reasoning")
            .map(|cap| &cap.endpoint_levels)
            .filter(|list| !list.is_empty())
        {
            levels.retain(|level| endpoint.iter().any(|l| l == level));
        }
        levels
    }

    /// 某个档位发往 `reasoning_effort` 的 wire 值。`None` = 这一档不发字段。
    ///
    /// 真源是 `reasoning_efforts`（本行声明）；本行没声明任何档位时，用内置表的档位名
    /// 做恒等映射——OpenAI 兼容端点的 `reasoning_effort` 取值就是这些名字。端点
    /// `effort.supported_levels` 明说没这档就不给，避免"界面上选了、请求里没有"。
    pub fn reasoning_wire(&self, base_url: &str, level: &str) -> Option<String> {
        let level = level.trim();
        if level.is_empty() {
            return None;
        }
        if !self.reasoning_efforts.is_empty() {
            return self
                .reasoning_efforts
                .get(level)
                .filter(|wire| !wire.trim().is_empty())
                .cloned();
        }
        if let Some(endpoint) = self
            .capabilities
            .get("reasoning")
            .map(|cap| &cap.endpoint_levels)
            .filter(|list| !list.is_empty())
        {
            if !endpoint.iter().any(|l| l == level) {
                return None;
            }
        }
        self.builtin(base_url)
            .and_then(|builtin| builtin.reasoning_levels)
            .filter(|levels| levels.iter().any(|l| l == level))
            .map(|_| level.to_string())
    }

    /// 上下文窗口：本行填了就用本行的（手动/端点写入），否则用内置表的。
    /// 0 = 未知（不拿 0 当"无限"；压缩阈值由 `agent_loop::compact_token_budget`
    /// 按保守窗口推导并告警，不会退回固定预算）。
    pub fn context_window_effective(&self, base_url: &str) -> u32 {
        if self.context_window > 0 {
            return self.context_window;
        }
        self.builtin(base_url)
            .and_then(|builtin| builtin.context_window)
            .map(|window| window.min(u32::MAX as u64) as u32)
            .unwrap_or(0)
    }

    /// 输出上限：本行填了就用本行的，否则用内置表。与 [`Self::context_window_effective`]
    /// 同口径——视图里「来源：内置表、值：未声明」是自相矛盾的组合，必须同源。
    pub fn max_tokens_effective(&self, base_url: &str) -> u32 {
        if self.max_tokens > 0 {
            return self.max_tokens;
        }
        self.builtin(base_url)
            .and_then(|builtin| builtin.max_output)
            .map(|limit| limit.min(u32::MAX as u64) as u32)
            .unwrap_or(0)
    }

    /// 能力项在界面上的三态：`verified` / `mismatch` / `unverified`。
    /// 只用于展示——**不**参与运行期放行（那用 `vision_effective` 这类显式判据）。
    pub fn capability_state(&self, item: &str) -> &'static str {
        match self.capabilities.get(item).and_then(|cap| cap.verified) {
            Some(true) => "verified",
            Some(false) => "mismatch",
            None => "unverified",
        }
    }
}

/// 一家可切换的服务商（04 §4 `[[providers]]` 的一行）。
///
/// `secret_ref` 是 DPAPI 里的密钥文件名，同时兼作设置页的凭据 ref：
/// 两侧共用同一个字符串，避免"界面显示已配 Key、后端读的是另一个文件"。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderProfile {
    /// 稳定标识；席位与设置页都按它寻址，也用作 secret_ref 缺省时的派生依据。
    pub id: String,
    /// 展示名。
    pub label: String,
    /// 协议族：`openai-responses` | `openai-chat`。
    pub provider: String,
    #[serde(alias = "baseUrl")]
    pub base_url: String,
    pub model: String,
    /// 席位选中的推理档位（SH-096，存**档位名**而非 wire 值——wire 拼写以模型行的
    /// `reasoning_efforts` 为准）。空 = 服务商默认（不发字段）。设置页不编辑它：
    /// 上游的选中档属于席位状态，这里按"选中档随配置持久化"的既有先例（`model`
    /// 字段同此）落进服务商文档，由席位 selectModel 写入。
    #[serde(alias = "reasoningEffort")]
    pub reasoning_effort: String,
    /// 自定义请求头（SH-097，additive）：发往该服务商端点的附加头。上游同字段
    /// 位于服务商 profile（llm-pi-ai config.ts），语义一致：部署头先放，认证头
    /// 与保留名后到者胜。
    ///
    /// **值是凭据，一律按 secret 处理（审计 M8）**：这个字段的语义就是"发给模型端点
    /// 的凭据类头"，用户不会拿它填普通元数据；只按头名（`X-Api-Key` / `Authorization`
    /// …）做匹配是启发式，厂商自定义命名漏一个就等于没做。因此全部值都进
    /// `SECRET_REGISTRY`，走与 API Key 同一条落盘/日志脱敏路径，设置页不回显。
    #[serde(alias = "headers")]
    pub headers: std::collections::BTreeMap<String, String>,
    /// 密钥存放名（DPAPI 文件名 / 设置页 ref）。空 = 按 id 派生。
    #[serde(alias = "secretRef")]
    pub secret_ref: String,
    /// 这一家的模型档位清单（设置页写面）。空 = 只用 `model` 那一行，旧文档不改一字也能跑。
    /// 必须排在最后：TOML 要求标量先于子表，把 `[[providers.models]]` 放前面会让整个
    /// `[[providers]]` 的缩进归属变成 models，写出去的配置直接解析不回来。
    pub models: Vec<ModelProfile>,
}

/// 自定义请求头校验（SH-097）：名字必须是 RFC 7230 token（分隔符与空格都不行），
/// 值不得含 CR/LF/NUL（头注入）。空值放行——空值是合法 HTTP。
/// 上游用 `new Headers(...)` 做同一件事（llm-pi-ai config.ts 的 assertValidHeaders），
/// 这里按同一语义手写，不给 config 层引入 reqwest 类型依赖。
pub fn validate_header_pair(name: &str, value: &str) -> Result<(), String> {
    let is_tchar = |b: u8| matches!(b,
        b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.'
        | b'^' | b'_' | b'`' | b'|' | b'~'
        | b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z');
    if name.is_empty() || !name.bytes().all(is_tchar) {
        return Err(format!("请求头名 {name:?} 不合法：必须是不含空格与冒号的 HTTP 头名"));
    }
    if value.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
        return Err(format!("请求头 {name} 的值含换行或控制字符：头注入防线不允许"));
    }
    Ok(())
}

impl Default for ProviderProfile {
    fn default() -> Self {
        Self {
            id: String::new(),
            label: String::new(),
            provider: "openai-chat".into(),
            base_url: String::new(),
            model: String::new(),
            reasoning_effort: String::new(),
            headers: std::collections::BTreeMap::new(),
            secret_ref: String::new(),
            models: Vec::new(),
        }
    }
}

impl ProviderProfile {
    /// 密钥文件名：显式给了就用给的，否则按 id 派生。派生规则与设置页一致
    /// （大写、非字母数字换成下划线、`_API_KEY` 后缀），所以界面和后端不会
    /// 各自算出一个不同的名字。
    pub fn secret_name(&self) -> String {
        if !self.secret_ref.trim().is_empty() {
            return self.secret_ref.trim().to_string();
        }
        let derived = self
            .id
            .to_uppercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect::<String>();
        format!("{derived}_API_KEY")
    }
}

/// 一次运行真正使用的模型端点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModel {
    pub id: String,
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub max_turns: u32,
    /// 密钥文件名。回落路径上等于 `[model].provider`，
    /// 这样升级前存的那份 DPAPI 密钥仍然读得回来。
    pub secret_name: String,
    /// 生效的推理档 wire 值（SH-096）：选中档位在该模型 `reasoning_efforts` 里
    /// 查到的字符串，None = 不发字段（未声明 / 选了 off / 档位未命中）。
    pub reasoning_effort: Option<String>,
    /// 该服务商的自定义请求头（SH-097），原样带到模型请求。
    pub headers: std::collections::BTreeMap<String, String>,
    /// 视觉能力是否生效（SH-132）：声明有视觉（`input` 含 `image`，或能力项显式
    /// 覆盖）且没有被实测否定。`view_image` 只在这个值为真时注册——模型看不到
    /// 工具，就不会乱调，也不会幻觉"我看过截图了"。
    pub vision: bool,
    /// 上下文窗口（SH-132）：0 = 未知。已知时压缩触发点取
    /// `min(token_budget, context_window * 4/5)`，不再一个固定预算吃到底。
    pub context_window: u32,
}

impl AppConfig {
    /// 全新安装的出厂配置：内置服务商（DeepSeek 置顶）直接进 `[[providers]]`
    /// 并由它生效。只在配置文件**不存在**时使用；已有配置（哪怕 providers 为空）
    /// 一个字节都不改——用户删光了服务商是用户的决定，不能被"默认值"复活。
    pub fn fresh_install() -> Self {
        let mut cfg = Self {
            providers: builtin_providers::default_profiles(),
            active_provider: builtin_providers::default_active_id().to_string(),
            ..Self::default()
        };
        // 全新安装写出的 `os` 就是本版本默认值，不需要再迁移；标成「已定过」，
        // 这样用户以后手动改回 app_container 不会被迁移逻辑改回去。
        cfg.sandbox.os_explicit = true;
        cfg
    }

    /// 校验额外工作区清单（SH-099）：id 唯一非空、路径唯一非空且是绝对路径。
    /// 存在性/目录类型在创建命令里即时校验（配置里存的是用户意图，目录后来被删
    /// 不该让整个 config.toml 拒载）。
    pub fn validate_workspaces(workspaces: &[WorkspaceProfile]) -> Result<(), String> {
        let mut seen_ids: Vec<&str> = Vec::with_capacity(workspaces.len());
        let mut seen_paths: Vec<String> = Vec::with_capacity(workspaces.len());
        for ws in workspaces {
            if ws.id.trim().is_empty() {
                return Err("工作区缺 id：侧栏按它寻址，空 id 选不中".into());
            }
            if seen_ids.contains(&ws.id.as_str()) {
                return Err(format!("工作区 id 重复：{}", ws.id));
            }
            seen_ids.push(ws.id.as_str());
            let path = ws.path.trim();
            if path.is_empty() {
                return Err(format!("工作区 {} 缺路径", ws.id));
            }
            if !std::path::Path::new(path).is_absolute() {
                return Err(format!("工作区 {} 的路径 {path} 不是绝对路径", ws.id));
            }
            let canonical = std::path::Path::new(path)
                .canonicalize()
                .map_err(|e| format!("工作区 {} 的路径 {path} 不可用: {e}", ws.id))?;
            let key = canonical.to_string_lossy().to_string();
            if seen_paths.iter().any(|p| p == &key) {
                return Err(format!("工作区路径重复（含符号链接指向同一目录）: {path}"));
            }
            seen_paths.push(key);
        }
        Ok(())
    }

    /// 校验 MCP 配置（SH-060，docs/02 §3.1）。fail-closed：任一项不合法即拒载整份配置，
    /// 而不是跳过坏的那家继续启动——「跳过」等于用户以为自己配了三家、实际只跑了两家。
    ///
    /// 只做**语法与不变式**检查；可执行文件是否存在、SHA-256、工具清单属于连接期指纹，
    /// 不在这里做（配置里存的是用户意图，二进制后来被替换必须由指纹发现）。
    pub fn validate_mcp(mcp: &McpConfig) -> Result<(), String> {
        let mut seen: Vec<&str> = Vec::with_capacity(mcp.servers.len());
        for server in &mcp.servers {
            if !is_valid_mcp_server_id(&server.id) {
                return Err(format!(
                    "MCP server id 非法：「{}」（要求 ^[a-z0-9][a-z0-9-]{{0,31}}$，取自用户配置而非 server 自报名）",
                    server.id
                ));
            }
            if seen.contains(&server.id.as_str()) {
                // 重复 id = 同一命名空间由两家进程声明，分发名会互相覆盖；
                // 拒绝整份配置，避免「后写者获胜」。
                return Err(format!("MCP server id 重复：{}", server.id));
            }
            seen.push(server.id.as_str());

            if server.command.trim().is_empty() {
                return Err(format!("MCP server {} 缺 command", server.id));
            }
            if server.command.contains('\0') || server.args.iter().any(|a| a.contains('\0')) {
                return Err(format!("MCP server {} 的 command/args 含 NUL 字符", server.id));
            }
            let cwd = server.cwd.trim();
            if !cwd.is_empty() && !std::path::Path::new(cwd).is_absolute() {
                return Err(format!(
                    "MCP server {} 的 cwd {cwd} 不是绝对路径（指纹与进程 cwd 都要求确定的绝对路径）",
                    server.id
                ));
            }
            if server.unsandboxed && server.unsandboxed_reason.trim().is_empty() {
                // X10：豁免必须用户主动、记录在案、界面可见；没有原因等于静默放行。
                return Err(format!(
                    "MCP server {} 声明 unsandboxed = true，但未填写 unsandboxed_reason——\
                     豁免必须留下可追溯的原因",
                    server.id
                ));
            }
            if server.allowed_roots.len() > MAX_ALLOWED_ROOTS {
                return Err(format!(
                    "MCP server {} 的 allowed_roots 超过上限 {MAX_ALLOWED_ROOTS} 条（拒绝无限扩张）",
                    server.id
                ));
            }
            for root in &server.allowed_roots {
                validate_allowed_root(&root.path)
                    .map_err(|error| format!("MCP server {} 的 allowed_roots 非法：{error}", server.id))?;
            }
            for (key, value) in &server.env {
                if key.trim().is_empty() || key.contains('=') || key.contains('\0') {
                    return Err(format!("MCP server {} 的环境变量名非法：{:?}", server.id, key));
                }
                if value.contains('\0') {
                    return Err(format!("MCP server {} 的环境变量 {} 含 NUL 字符", server.id, key));
                }
            }
        }
        Ok(())
    }

    /// 启动配置的完整性校验（解析成功后立即调用）。
    fn validate_loaded(&self) -> Result<(), String> {
        // 头值也是凭据：装载即登记，任何一次日志都晚于这次调用（审计 M8）
        register_header_secrets(&self.providers);
        Self::validate_verify(&self.verify)?;
        Self::validate_mcp(&self.mcp)
    }

    /// `[verify].commands` 的严格校验：空命令、超条数都直接报错。
    ///
    /// 静默截断/跳过会让「我明明配了 4 条」与「实际只跑了 3 条」对不上，
    /// 而验证闸门最不能容忍的就是「看起来配了、实际没跑」。
    fn validate_verify(verify: &VerifyConfig) -> Result<(), String> {
        if verify.commands.len() > MAX_VERIFY_COMMANDS {
            return Err(format!(
                "[verify].commands 最多 {MAX_VERIFY_COMMANDS} 条（当前 {} 条）",
                verify.commands.len()
            ));
        }
        for command in &verify.commands {
            let trimmed = command.trim();
            if trimmed.is_empty() {
                return Err("[verify].commands 不允许空命令".to_string());
            }
            if trimmed.contains('\0') {
                return Err("[verify].commands 含 NUL 字符".to_string());
            }
        }
        Ok(())
    }

    /// 按 id 找额外工作区；`default-workspace` 不在这里（它来自 [workspace].root）。
    pub fn workspace_by_id(&self, id: &str) -> Option<&WorkspaceProfile> {
        self.workspaces.iter().find(|ws| ws.id == id)
    }

    /// 生效端点：`active_provider` 命中列表就用那家，否则回落 `[model]`。
    ///
    /// 回落**不是**容错降级，而是文档规定的兼容姿态：只配了 `[model]` 的安装
    /// 必须和不配列表时行为逐字一致。
    pub fn effective_model(&self) -> ResolvedModel {
        match self.providers.iter().find(|p| p.id == self.active_provider) {
            Some(p) => {
                // 档位 → wire 值在这里一次解析完，model_client 只看最终字符串：
                // 查表规则（off 空值不发、未声明不发）不该散进每个请求构造点。
                let level = p.reasoning_effort.trim();
                let reasoning_effort = if level.is_empty() {
                    // 初始默认（用户拍板，2026-11-07）：没选择过档位时回落声明的
                    // 默认档（`DEFAULT_REASONING_LEVEL`），与席位 catalog 的
                    // defaultEffort 预选同一规则；模型没声明这一档（或其 wire 值为空）
                    // 就维持不发，不发明一个档位。显式选过 off 的（reasoning_effort =
                    // "off"）不进这一支。
                    p.models
                        .iter()
                        .find(|m| m.id == p.model)
                        .and_then(|m| m.reasoning_wire(&p.base_url, DEFAULT_REASONING_LEVEL))
                } else {
                    p.models
                        .iter()
                        .find(|m| m.id == p.model)
                        .and_then(|m| m.reasoning_wire(&p.base_url, level))
                };
                let row = p.models.iter().find(|m| m.id == p.model);
                ResolvedModel {
                    id: p.id.clone(),
                    provider: p.provider.clone(),
                    base_url: p.base_url.clone(),
                    model: p.model.clone(),
                    max_turns: self.model.max_turns,
                    secret_name: p.secret_name(),
                    reasoning_effort,
                    headers: p.headers.clone(),
                    vision: row.map(|m| m.vision_effective(&p.base_url)).unwrap_or(false),
                    context_window: row
                        .map(|m| m.context_window_effective(&p.base_url))
                        .unwrap_or(0),
                }
            }
            None => ResolvedModel {
                id: self.model.provider.clone(),
                provider: self.model.provider.clone(),
                base_url: self.model.base_url.clone(),
                model: self.model.model.clone(),
                max_turns: self.model.max_turns,
                secret_name: self.model.provider.clone(),
                // 回落段没有档位清单，推理档无从谈起（04 §4 兼容姿态的一部分）
                reasoning_effort: None,
                headers: Default::default(),
                // 回落段没有能力声明：视觉不生效、窗口未知。宁可少注册一个工具，
                // 也不要在没声明的地方假装能用。
                vision: false,
                context_window: 0,
            },
        }
    }

    /// 校验服务商文档：id 非空且唯一、active 要么空要么命中。
    /// 报错而不是静默修——静默丢掉一个 id 会让用户以为已经配好，实际那家根本不存在。
    pub fn validate_providers(providers: &[ProviderProfile], active: &str) -> Result<(), String> {
        // 校验通过与否都要先登记：写入后紧接着就可能打日志（保存失败的报错里
        // 常带原文），晚一步登记就漏一步（审计 M8）
        register_header_secrets(providers);
        let mut seen: Vec<&str> = Vec::with_capacity(providers.len());
        for p in providers {
            if p.id.trim().is_empty() {
                return Err("服务商缺 id：id 既是席位寻址用的键，也是密钥文件名派生依据".into());
            }
            if seen.contains(&p.id.as_str()) {
                return Err(format!("服务商 id 重复：{}", p.id));
            }
            seen.push(p.id.as_str());
            // 密钥名要过 DPAPI 文件名的 sanitize。那里是**剔除**非法字符而不是报错，
            // 所以名字被改动必须在这里就拦下：一旦静默改写，设置页按原名报告"已配 Key"、
            // 后端按改名后的文件读，第一次运行就变成"配了却说没配"。
            let name = p.secret_name();
            let sanitized = SecretStore::sanitize_provider(&name)?;
            if sanitized != name {
                return Err(format!(
                    "服务商 {} 的密钥名 {name} 含非法字符，会被清洗成 {sanitized}——请直接用一个合法名字",
                    p.id
                ));
            }
            let mut rows: Vec<&str> = Vec::with_capacity(p.models.len());
            for row in &p.models {
                if row.id.trim().is_empty() {
                    return Err(format!("服务商 {} 有模型行缺 id：席位按 id 选中一行，空 id 选不中", p.id));
                }
                if rows.contains(&row.id.as_str()) {
                    return Err(format!("服务商 {} 的模型 id 重复：{}", p.id, row.id));
                }
                rows.push(row.id.as_str());
                // 推理档声明（SH-096）：档位名必须来自全集；只有 off 允许空 wire 值；
                // 全表只声明 off 没有意义（席位会列出一个"永远不发字段"的档位）。
                for (level, wire) in &row.reasoning_efforts {
                    if !THINKING_LEVELS.contains(&level.as_str()) {
                        return Err(format!(
                            "服务商 {} 的模型 {} 有未知推理档位 {level}（可用：{}）",
                            p.id,
                            row.id,
                            THINKING_LEVELS.join("/")
                        ));
                    }
                    if level != "off" && wire.trim().is_empty() {
                        return Err(format!(
                            "服务商 {} 的模型 {} 的推理档位 {level} 缺 wire 值（只有 off 允许为空）",
                            p.id, row.id
                        ));
                    }
                }
                if !row.reasoning_efforts.is_empty()
                    && row.reasoning_efforts.values().all(|wire| wire.trim().is_empty())
                {
                    return Err(format!(
                        "服务商 {} 的模型 {} 的推理档位只声明了 off：至少给一个非 off 档 wire 值",
                        p.id, row.id
                    ));
                }
                // 能力项（SH-132）：键不设白名单（扩展位），但已识别的项要守住语义。
                // `verified_levels` 是探针写入的实测档，出现未知档位说明来源可疑或
                // 手改坏了——按拒绝处理，而不是让档位选择器列出一个不存在的档。
                for (item, cap) in &row.capabilities {
                    for level in &cap.verified_levels {
                        if !THINKING_LEVELS.contains(&level.as_str()) {
                            return Err(format!(
                                "服务商 {} 的模型 {} 的能力项 {item} 含未知实测档位 {level}（可用：{}）",
                                p.id,
                                row.id,
                                THINKING_LEVELS.join("/")
                            ));
                        }
                    }
                    if item == "reasoning" && cap.value.is_some() {
                        return Err(format!(
                            "服务商 {} 的模型 {} 的能力项 reasoning 不能写布尔值：档位以 reasoning_efforts 为准",
                            p.id, row.id
                        ));
                    }
                }
            }
            // 清单非空时当前生效行必须在里面：不在的话席位下拉里没有它，用户看不到
            // 自己正在跑的模型，随手点一下别的行就把运行档换掉了。
            if !p.models.is_empty() && !p.model.trim().is_empty() && !rows.contains(&p.model.as_str()) {
                return Err(format!(
                    "服务商 {} 的当前模型 {} 不在模型清单里，请先把它加进清单",
                    p.id, p.model
                ));
            }
            // 选中档位必须命中生效模型声明的档位：不在清单里的档位查不到 wire 值，
            // 静默不发字段的话，用户选了 high 实际跑的还是默认档——比报错难发现得多。
            let level = p.reasoning_effort.trim();
            if !level.is_empty() {
                let declared = p
                    .models
                    .iter()
                    .find(|m| m.id == p.model)
                    .is_some_and(|m| m.reasoning_wire(&p.base_url, level).is_some());
                if !declared {
                    return Err(format!(
                        "服务商 {} 的选中推理档位 {level} 不在生效模型 {} 的档位声明里",
                        p.id, p.model
                    ));
                }
            }
            // 自定义请求头（SH-097）：坏名字/头注入必须在写入时拦下，运行期只管发
            for (name, value) in &p.headers {
                validate_header_pair(name, value)
                    .map_err(|e| format!("服务商 {} 的自定义请求头：{e}", p.id))?;
            }
        }
        if !active.is_empty() && !providers.iter().any(|p| p.id == active) {
            return Err(format!("active_provider 指不到任何服务商：{active}"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    /// 协议族：`openai-responses` | `openai-chat`（ADR：仅对接兼容端点）。
    pub provider: String,
    pub base_url: String,
    pub model: String,
    /// 每任务最大轮数。默认 0 = 不限：只要任务没解决就继续，由模型自己收口；
    /// 只有显式配置正整数时才在上限处停（风险 R3 的兜底从「默认值」改为「用户显式设置」）。
    pub max_turns: u32,
    /// 单个采样步最多重试几次模型请求（0 = 关闭重试）。默认 5，装配时封顶 8。
    /// 只对瞬态失败（限流 / 超时 / 网络 / 5xx / 空响应）生效，且只在首个可见增量前。
    pub max_retries: u32,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            provider: "openai-responses".into(),
            base_url: String::new(),
            model: String::new(),
            max_turns: 0,
            max_retries: 5,
        }
    }
}

/// Agent 运行预算（04 §4 的 `[agent]` 段）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    /// 单会话累计 token 预算。达到它就触发压缩（SH-032），
    /// 默认值取 04 §4 规定的 `token_budget = 500000`。
    ///
    /// 这是**预算**而不是硬顶：硬顶是 `model.max_turns`（轮数维度，SH-034 才补 token
    /// 维度的硬顶）。预算的语义是"该收口了"，不是"必须停下"。
    pub token_budget: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self { token_budget: 500_000 }
    }
}

/// 写后验证闸门（04 §4 `[verify]`）。
///
/// 默认 `enforce`：脏状态在收尾时验证失败会否决一次。`mode` / `baseline` 都是
/// **封闭枚举**——未知字面量在解析期报错，不做容错回落。配置写错必须立刻暴露，
/// 否则「以为开了闸门，其实静默关掉」比没有闸门更危险。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VerifyConfig {
    /// 总开关。`SHANIU_EVAL_ABLATE_VERIFY=1` 是评测消融，优先于本项。
    pub enabled: bool,
    pub mode: VerifyMode,
    pub baseline: BaselineMode,
    /// 单条命令超时；上限 `MAX_VERIFY_TIMEOUT_MS`。
    pub timeout_ms: u64,
    /// 显式命令（最高优先）；为空才按脏文件就近探测。
    pub commands: Vec<String>,
}

/// 验证失败时的处置方式。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyMode {
    /// 失败否决一次（默认）。
    #[default]
    Enforce,
    /// 只发事件、不否决——运行期泄压阀，改配置即生效，不回滚代码。
    Warn,
}

/// 「任务开始前就已经红」的基线采集策略。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaselineMode {
    /// 首次置脏前观测到同命令失败，就把它当作既有失败（不清脏、不否决）。
    #[default]
    Auto,
    /// 不做基线判定。
    Off,
}

/// 单条验证命令超时上限：再长就不是「验证」，是「跑构建」。
pub const MAX_VERIFY_TIMEOUT_MS: u64 = 300_000;
/// 一次验证最多执行的命令条数（确定性：同任务两次验证必须同组同序）。
pub const MAX_VERIFY_COMMANDS: usize = 3;

impl Default for VerifyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: VerifyMode::default(),
            baseline: BaselineMode::default(),
            timeout_ms: 120_000,
            commands: Vec::new(),
        }
    }
}

impl VerifyConfig {
    /// 生效模式：进程级 `SHANIU_VERIFY_MODE` 覆盖配置（只认 `enforce|warn`，其它值忽略）。
    pub fn effective_mode(&self) -> VerifyMode {
        match std::env::var("SHANIU_VERIFY_MODE").ok().as_deref().map(str::trim) {
            Some("warn") => VerifyMode::Warn,
            Some("enforce") => VerifyMode::Enforce,
            _ => self.mode,
        }
    }

    /// 归一化后的命令清单：去空白、去重、保序、截断到 [`MAX_VERIFY_COMMANDS`]。
    pub fn normalized_commands(&self) -> Vec<String> {
        let mut result: Vec<String> = Vec::new();
        for command in &self.commands {
            let command = command.trim();
            if command.is_empty() || result.iter().any(|kept| kept == command) {
                continue;
            }
            result.push(command.to_string());
            if result.len() >= MAX_VERIFY_COMMANDS {
                break;
            }
        }
        result
    }

    pub fn clamped_timeout_ms(&self) -> u64 {
        self.timeout_ms.clamp(1_000, MAX_VERIFY_TIMEOUT_MS)
    }
}

/// 会话标题自动生成（04 §4 `[session_title]`，对齐上游 `dsh-session-title-first-prompt-llm`
/// 在 base bundle 的开箱默认）。
///
/// 语义：全新会话（无父、非分叉）收到首条用户消息后，用**生效路由**发一次辅助请求
/// 总结该消息生成标题；截断派生降级为 fallback（生成失败保持 fallback，不自动重试）；
/// 用户手改（`titles.json`）优先且不被生成覆盖。数值照抄上游 base bundle。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionTitleConfig {
    /// 开箱默认开（上游正式产品即默认挂载）。关掉 = 只用截断 fallback。
    pub enabled: bool,
    /// 非 CJK 语言的目标词数。
    pub target_words: u32,
    /// CJK 语言的目标字数。
    pub target_cjk_characters: u32,
    /// 框架后的输入字节上限：超限**拒绝**而不是截断（上游语义——截断会让标题基于
    /// 被切开的文本编造）。超限保持 fallback。
    pub max_input_bytes: u64,
    /// 辅助请求的输出 token 上限：防失控的兜底而不是长度目标（系统提示已约束短标题；
    /// 推理模型关不掉的思考也计入输出）。独立于会话主请求的输出上限。
    pub max_output_tokens: u32,
    /// 辅助请求端到端超时。迟到的成功结果不采纳（流被超时掐断）。
    pub timeout_ms: u64,
}

impl Default for SessionTitleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            target_words: 5,
            target_cjk_characters: 10,
            max_input_bytes: 4096,
            max_output_tokens: 4096,
            timeout_ms: 60_000,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxConfig {
    pub mode: SandboxMode,
    /// OS 级沙箱机制（SH-021/022）。默认受限令牌（只约束写）；AppContainer 是
    /// 可选的更严格机制，回退到 logical 必须显式选择。
    pub os: OsSandbox,
    /// `os` 这一项是否已经被定过（用户亲手选过，或由旧默认迁移过）。
    ///
    /// 存在的唯一原因：旧版本会把默认值一起写进 config.toml，于是「文件里有
    /// `app_container`」并不等于「用户选过 AppContainer」。本字段缺失（旧文件）
    /// 时按旧默认处理——见 `load_or_create_diagnosed` 的一次性迁移。
    /// 想主动留在 AppContainer：同时写 `os = "app_container"` 与 `os_explicit = true`。
    pub os_explicit: bool,
}

/// OS 级沙箱机制。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OsSandbox {
    /// 写约束（默认）：`WRITE_RESTRICTED` 受限令牌，只约束写，读与网络不约束。
    /// 对齐 codex / dsh 在 Windows 上的机制（docs/16 §5）。
    #[default]
    RestrictedToken,
    /// **可选**的 AppContainer：路径/读隔离。比参考实现更严，只在显式选择时启用。
    AppContainer,
    /// **显式回退**：只有受限令牌 + Job 的逻辑沙箱，没有路径级边界。
    /// 选中后 UI 与日志必须标注「当前未启用 OS 沙箱」，禁止静默降级。
    Logical,
}

/// 三档文件效果（对齐 DSH `SandboxMode`，docs/04 §4 `[sandbox] mode`）。
///
/// 默认 `ReadOnly`，与 DSH 一致（2026-11-10 产品决策，此前曾短暂默认
/// `workspace_write`）。解析优先级（对齐 DSH `SandboxPolicyService.resolve`）：
/// **本次调用显式指定 > 会话设置 > 部署默认**——显式指定只有 on-failure 提权那一条
/// （单次调用），会话设置是运行中切换的 `SessionPolicyHandle`，部署默认是
/// config.toml。fail-closed 语义见 ADR-002。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    #[default]
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl SandboxMode {
    /// wire 值（与 `serde(rename_all = "snake_case")` 一致，供状态条/会话事件复用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::WorkspaceWrite => "workspace_write",
            Self::DangerFullAccess => "danger_full_access",
        }
    }

    /// 解析 wire 值；未知值返回 None（调用方决定拒绝还是回落，不在这里猜）。
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "read_only" => Some(Self::ReadOnly),
            "workspace_write" => Some(Self::WorkspaceWrite),
            "danger_full_access" => Some(Self::DangerFullAccess),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ApprovalConfig {
    pub mode: ApprovalMode,
    /// 命令前缀白名单（ADR-007③ / ADR-009②）：命中即放行、不弹窗，每条命中写审计。
    ///
    /// **不做全局落盘规则文件**（ADR-008）：这里只接受用户手写 config.toml，运行时不提供
    /// 任何改写入口——代理没有路径自行扩权，一次局部放行也不会被后续会话继承。
    /// 默认集只含只读/构建类；写文件、删除、网络操作不得进入默认前缀（ADR-007 代价节）。
    pub allow_prefixes: Vec<String>,
}

impl Default for ApprovalConfig {
    fn default() -> Self {
        Self {
            mode: ApprovalMode::OnRequest,
            allow_prefixes: default_allow_prefixes(),
        }
    }
}

/// 前缀白名单默认集：只读查看与构建/测试。刻意不含 `cargo run`（执行产物）、
/// `git commit`/`git push`（写远端）、任何删除/移动/网络命令。
pub fn default_allow_prefixes() -> Vec<String> {
    [
        "cargo build",
        "cargo check",
        "cargo clippy",
        "cargo test",
        "cargo metadata",
        "git status",
        "git diff",
        "git log",
        "git show",
        "git blame",
        "dir",
        "type",
        // 只读搜索：参数级守卫在 `permission::policy::readonly_guard_allows`，
        // 带盘符/UNC/`..`/绝对路径的组合一律走审批，`findstr` 开关另有白名单。
        "findstr",
    ]
    .iter()
    .map(|rule| (*rule).to_string())
    .collect()
}

/// 审批策略分层（ADR-007：随 turn 生效，不是全局开关）。
///
/// 语义落在**每条命令/每次工具调用**上，由 `permission::policy::decide` 消费：
/// - `Never`：从不询问。沙箱允许的写入/执行照常放行；沙箱拒绝、破坏性操作**直接失败**，
///   不产生审批卡。它不等于"全部允许"——未知工具仍按破坏性拒绝（fail-closed）。
/// - `OnFailure`：沙箱允许的写入/执行不问；只有**沙箱拒绝**导致命令失败时才询问一次
///   是否以 full-access 重试（提权必须用户点头，不静默降级）。
/// - `OnRequest`：写入/执行按风险逐次询问（默认档）。
/// - `FullAccess`：全部放行且不询问。**前提**是沙箱已是 `danger_full_access`；
///   前提不满足时回落 `OnRequest`（允许面收敛，不是静默放宽）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    Never,
    #[serde(rename = "on_failure", alias = "on-failure")]
    OnFailure,
    /// 旧值 `ask` / `auto_read_only` 仍能读入（行为本来就是"按风险询问"）。
    #[default]
    #[serde(rename = "on_request", alias = "on-request", alias = "ask", alias = "auto_read_only")]
    OnRequest,
    #[serde(rename = "full_access", alias = "full-access")]
    FullAccess,
}

impl ApprovalMode {
    /// wire 值（与 `serde` 重命名一致，供审计与状态条复用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::OnFailure => "on_failure",
            Self::OnRequest => "on_request",
            Self::FullAccess => "full_access",
        }
    }

    /// 生效档：`full_access` 需要 `danger_full_access` 沙箱作为前提，
    /// 否则回落到 `on_request` 并返回回落说明（调用方负责播报，不静默）。
    pub fn effective(self, sandbox: SandboxMode) -> (Self, Option<&'static str>) {
        if self == Self::FullAccess && sandbox != SandboxMode::DangerFullAccess {
            (
                Self::OnRequest,
                Some("full_access 需要 danger_full_access 沙箱，当前沙箱档不满足，已回落 on_request"),
            )
        } else {
            (self, None)
        }
    }
}

/// MCP 客户端配置（docs/02 §3.1，additive `[mcp]` / `[[mcp.servers]]`）。
///
/// 空列表 = 未启用 MCP，旧 config.toml 不改一个字也能跑。
/// v1 仅支持 stdio 传输（本地子进程），不提供托管市场/自动安装。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    pub servers: Vec<McpServerConfig>,
}

/// MCP 配置来源：把「首审可以 spawn 二进制」的正当性变成**被检查的事实**，
/// 而不是默认假设。
///
/// v1 的唯一来源是应用级 `config.toml`（用户本人撰写）。一旦将来引入工作区/
/// 项目级 `mcp.servers`（从仓库读到配置），那等同于执行仓库提供的二进制，
/// 必须先完成「不 spawn 的两阶段审查」改造；在那之前运行时会在 spawn 前直接
/// 拒绝 `Workspace` 来源（`mcp_client::runtime::connect_and_review_with`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpConfigSource {
    /// 应用级 `config.toml`，由用户本人撰写。
    #[default]
    User,
    /// 工作区/项目级配置（**当前没有写入路径**，为将来的改造预留）。
    Workspace,
}

/// 一家 stdio MCP server。
///
/// **不可信第三方进程**（docs/02 §3.1.1）：这里只存用户自己填的启动参数，
/// server 自报的名称/版本/工具描述一律不写回配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
    /// 配置来源。`#[serde(skip)]`：**不可由 TOML 指定**——来源只能由加载代码
    /// 设置，用户不能自称「这是用户配置」。默认 `User` 对应 v1 唯一来源
    /// 应用级 `config.toml`；工作区加载器将来必须显式置 `Workspace`。
    #[serde(skip)]
    pub source: McpConfigSource,
    /// 稳定标识，同时是分发名 `mcp__<id>__<slug>` 的命名空间来源。
    /// 语法 `^[a-z0-9][a-z0-9-]{0,31}$`——**不采用 server 自报名称**（B-R1）。
    pub id: String,
    /// 可执行文件（命令名或绝对路径）。连接时解析成绝对路径并固化进指纹，
    /// 之后不再走 PATH 解析（C-R5：不做 shell 解释）。
    pub command: String,
    /// 完整 argv（逐项原样传入，不做 shell 拼接）。
    pub args: Vec<String>,
    /// 环境变量白名单：server 环境 = **仅**这些键值（`env_clear` 后逐项注入，C-R4）。
    /// 字典序序列化，保证指纹稳定。
    pub env: std::collections::BTreeMap<String, String>,
    /// 工作目录（绝对路径）。空 = 落回数据根下的默认工作区。
    pub cwd: String,
    /// 是否启用；false = 不连接、工具不注册。默认 true（写了配置就是要用）。
    pub enabled: bool,
    /// **逐服务器显式豁免沙箱**（docs/02 §3.1.5 X10）。默认 false = 受限令牌 + Job Object。
    /// 置 true 必须同时填写 `unsandboxed_reason`，界面须常驻可见——不允许静默放行。
    pub unsandboxed: bool,
    /// 豁免原因（记录在案，供界面与审计展示）。非空才允许 `unsandboxed = true`。
    pub unsandboxed_reason: String,
    /// 是否允许该 server 访问网络。**默认 `true`（允许）**，显式 `allow_network = false`
    /// 才关断（容器不挂 `internetClient`）。与「网络不是策略维度」一致：命令与终端一律可出网，
    /// MCP 不该是唯一例外；per-server 关断能力保留，但不作默认。
    /// `sandbox.os = "logical"` 下该开关不生效，状态面按 `not_isolated` 如实显示。
    pub allow_network: bool,
    /// 目录级授权清单（Q2）：默认空 = 只能读工作区。
    /// 每一项都要经过 [`validate_allowed_root`]；拒绝名单内的路径不许写进配置。
    pub allowed_roots: Vec<AllowedRoot>,
    /// 该 server 对工作区的访问档（Q3）：默认 `read`。**进指纹**，
    /// 改成 `write` 会得到新指纹并要求重新批准。
    pub workspace_access: WorkspaceAccess,
}

/// 目录级授权档位（Q2：默认只读，显式升级为可写）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceAccess {
    #[default]
    Read,
    Write,
}

impl WorkspaceAccess {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkspaceAccess::Read => "read",
            WorkspaceAccess::Write => "write",
        }
    }
}

/// 一条用户勾选的目录授权。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowedRoot {
    pub path: String,
    #[serde(default)]
    pub access: WorkspaceAccess,
}

/// 单个 server 的目录授权条数上限（Q1：超出拒绝新增，不无限建）。
pub const MAX_ALLOWED_ROOTS: usize = 16;

/// 校验并规范化用户勾选的目录（Q2 三道硬约束）：
/// 1. 必须是**规范化绝对路径**，含 `..` 一律拒绝（不做「解析后等价」的宽容判断）；
/// 2. 驱动器根、`%SystemRoot%`、`Program Files*`、用户 profile 根直接进拒绝名单；
/// 3. 目录必须真实存在。
pub fn validate_allowed_root(raw: &str) -> Result<std::path::PathBuf, String> {
    use std::path::{Component, PathBuf};

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("目录路径不能为空".into());
    }
    if trimmed.contains('\0') {
        return Err("目录路径含 NUL 字符".into());
    }
    let path = PathBuf::from(trimmed);
    if !path.is_absolute() {
        return Err(format!("授权目录必须是绝对路径：{trimmed}"));
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!("授权目录不能包含 `..`：{trimmed}"));
    }
    let canonical = std::fs::canonicalize(&path)
        .map_err(|error| format!("授权目录无法解析（{trimmed}）：{error}"))?;
    let text = canonical.to_string_lossy();
    let canonical = PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text));
    if !canonical.is_dir() {
        return Err(format!("授权路径不是目录：{}", canonical.display()));
    }
    if let Some(reason) = denied_root_reason(&canonical) {
        return Err(format!("该路径在拒绝名单内（{reason}），不能授权：{}", canonical.display()));
    }
    Ok(canonical)
}

/// 拒绝名单判定（返回拒绝原因；`None` = 允许）。
fn denied_root_reason(path: &std::path::Path) -> Option<&'static str> {
    if path.parent().is_none() {
        return Some("驱动器根目录");
    }
    let text = path.to_string_lossy().to_lowercase();
    if let Some(root) = std::env::var_os("SystemRoot") {
        let root = std::path::PathBuf::from(root).to_string_lossy().to_lowercase();
        if text == root || text.starts_with(&format!("{root}\\")) {
            return Some("系统目录 %SystemRoot%");
        }
    }
    if let Some(home) = std::env::var_os("USERPROFILE") {
        let home = std::path::PathBuf::from(home).to_string_lossy().to_lowercase();
        if text == home {
            return Some("用户 profile 根目录");
        }
    }
    let segments: Vec<&str> = text.split('\\').collect();
    if segments
        .iter()
        .any(|segment| *segment == "program files" || *segment == "program files (x86)")
    {
        return Some("Program Files");
    }
    None
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            source: McpConfigSource::User,
            id: String::new(),
            command: String::new(),
            args: Vec::new(),
            env: Default::default(),
            cwd: String::new(),
            enabled: true,
            unsandboxed: false,
            unsandboxed_reason: String::new(),
            // 方案 3（2026-11-10）：默认允许，显式 false 才关。容器级 `#[serde(default)]`
            // 使「字段缺失」也落到这里，因此三态 = 缺失/true → 允许，false → 拒绝。
            allow_network: true,
            allowed_roots: Vec::new(),
            workspace_access: WorkspaceAccess::Read,
        }
    }
}

/// id 语法（B-R1）：小写字母/数字开头，仅小写字母、数字与连字符，总长 1~32。
pub fn is_valid_mcp_server_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.is_empty() || bytes.len() > 32 {
        return false;
    }
    let first = bytes[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// 工作区配置（docs §9.1：Agent 操作的项目目录，用户指定）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceConfig {
    /// 工作区根目录；空 = 用户主目录（%USERPROFILE%）。
    pub root: String,
    /// 默认工作区的显示名（上游 registry 的可写 title 同构）：空 = 上游首启拼写
    /// `default-workspace`，前端 `workspaceDisplayTitle` 据此渲染成「默认工作区」；
    /// 改过名 = 用户写的标题逐字显示。改回（或重新注册成）`default-workspace`
    /// 就回到本地化名——与上游的自动标题判定同一份规则。
    pub title: String,
    /// 默认工作区的删除标记（上游 registry delete 同构）：true = 不进工作区清单，
    /// 目录与会话档案全保留（会话显示在「未分组」）。`[workspace].root` 是运行期
    /// 依赖的全局根，物理删除等于停用 Agent 操作目录，所以落标记而不是删字段。
    /// 标记**不因重新添加同路径而清除**——上游 `deleteKnown` 同样不清
    /// `defaultWorkspaceId`，重新添加走全新注册（新 id），启动初始化也不复活它。
    pub hidden: bool,
    /// 工作区展示顺序（SH-111，上游 registry `state.workspaceIds` 的等价物）：
    /// 完整 id 列表，含 `default-workspace`。空 = 未拖拽过，按派生序
    /// （默认项若可见则居首，其后 `[[workspaces]]` 数组序）。写过之后本字段是
    /// 权威顺序：已不在注册表的 id 丢弃，新注册未列出的 id 按派生序补尾。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub order: Vec<String>,
}

/// 解析出的数据目录（§9.3：解析结果写入日志第一行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDirs {
    /// config.toml 与 secrets（安装版 = %APPDATA%\SHANIU；便携版 = <exe>\data）。
    pub config_root: PathBuf,
    /// 会话/rollout/日志（安装版 = %LOCALAPPDATA%\SHANIU；便携版 = <exe>\data）。
    pub data_root: PathBuf,
}

impl DataDirs {
    /// 默认工作区（Agent 操作目录）：数据根下的 `Workspace`（docs §9.3）。
    pub fn workspace_root(&self) -> PathBuf {
        self.data_root.join(WORKSPACE_DIR)
    }

    /// 前端视图（camelCase）：`get_product_dirs` 命令把它交给界面做工作区基线。
    pub fn view(&self) -> ProductDirsView {
        ProductDirsView {
            config_root: self.config_root.display().to_string(),
            data_root: self.data_root.display().to_string(),
            workspace_root: self.workspace_root().display().to_string(),
            // 界面"显示路径缩写"用的家目录。回落工作区已改为数据根下的 Workspace，
            // 不再与家目录同源；这个字段只影响 `~` 缩写的来源，两处不要混用。
            home_dir: home_dir().display().to_string(),
        }
    }
}

/// 用户主目录：`%USERPROFILE%`（Windows）→ `%HOME%`（POSIX）→ 临时目录。
/// 空字符串按"没设"处理：设了空值的变量不能当目录用。
pub fn home_dir() -> PathBuf {
    home_dir_from(
        std::env::var_os("USERPROFILE").as_deref(),
        std::env::var_os("HOME").as_deref(),
        &std::env::temp_dir(),
    )
}

/// `home_dir` 的纯函数部分：环境变量与回退目录由调用方给出，便于测试不动全局环境。
fn home_dir_from(
    profile: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
    fallback: &Path,
) -> PathBuf {
    if let Some(value) = profile.filter(|value| !value.is_empty()) {
        return PathBuf::from(value);
    }
    if let Some(value) = home.filter(|value| !value.is_empty()) {
        return PathBuf::from(value);
    }
    fallback.to_path_buf()
}

/// 产品目录的前端视图（字段名与前端 `get_product_dirs` 的读法一致）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProductDirsView {
    pub config_root: String,
    pub data_root: String,
    pub workspace_root: String,
    /// 用户主目录：界面用它把显示路径缩写成 `~` 形式（04 §1.3）。
    pub home_dir: String,
}

/// 启动时提前创建产品目录结构（幂等，docs §9.1/§9.2）。
///
/// 配置侧：`secrets\` / `settings-backup\`；数据侧：`sessions\` / `rollouts\` /
/// `logs\` / `cache\` / `crash\`。便携模式下两侧根相同，逐名创建互不影响。
/// 失败即报错：后续写路径不该带着一个建不出来的根继续跑。
///
/// **默认工作区 `Workspace\` 不在这里建**：用户配置了别的根时它根本不会被使用，
/// 无条件创建会在数据根里留一个空目录（还容易被误认成"正在用的工作区"）。
/// 由启动层在"确实以默认根工作"时创建（`run()` 里按 `AppState::workspace_root`
/// 判定），失败同样报错。
pub fn ensure_product_dirs(dirs: &DataDirs) -> Result<(), String> {
    for name in [SECRETS_DIR, SETTINGS_BACKUP_DIR] {
        create_subdir(&dirs.config_root, name)?;
    }
    for name in [SESSIONS_DIR, ROLLOUTS_DIR, LOGS_DIR, CACHE_DIR, CRASH_DIR] {
        create_subdir(&dirs.data_root, name)?;
    }
    Ok(())
}

fn create_subdir(root: &Path, name: &str) -> Result<(), String> {
    let path = root.join(name);
    fs::create_dir_all(&path).map_err(|e| format!("创建目录失败 {}: {e}", path.display()))
}

/// 纯函数解析（输入显式注入，可单测各优先级分支）。
///
/// **不做首次判定**：没有 `portable.marker` 时直接回落安装版，不探测可写性、不写记录。
/// 真实启动走 [`init_root_resolution`]——它先读记录、再在确实没有记录时判定一次。
pub fn resolve_dirs(
    env_data_dir: Option<&str>,
    exe_dir: Option<&Path>,
    appdata: Option<&Path>,
    localdata: Option<&Path>,
) -> DataDirs {
    // 优先级 1：环境变量（测试/CI/多实例隔离）
    if let Some(dir) = env_data_dir {
        if !dir.trim().is_empty() {
            let root = PathBuf::from(dir);
            return DataDirs { config_root: root.clone(), data_root: root };
        }
    }
    // 优先级 2：便携模式（exe 同目录存在 portable.marker）
    if let Some(exe) = exe_dir {
        if exe.join(PORTABLE_MARKER).exists() {
            let root = exe.join("data");
            return DataDirs { config_root: root.clone(), data_root: root };
        }
    }
    installed_dirs(appdata, localdata)
}

/// 安装模式默认根：配置侧 `%APPDATA%\SHANIU`，数据侧 `%LOCALAPPDATA%\SHANIU`。
fn installed_dirs(appdata: Option<&Path>, localdata: Option<&Path>) -> DataDirs {
    let fallback = std::env::temp_dir().join(APP_DIR_NAME);
    let config_root = appdata.map(|p| p.join(APP_DIR_NAME)).unwrap_or_else(|| fallback.clone());
    let data_root = localdata.map(|p| p.join(APP_DIR_NAME)).unwrap_or(config_root.clone());
    DataDirs { config_root, data_root }
}

// ==================== 数据根首次判定（2026-11-11） ====================
//
// 问题：便携与否原来只看 `portable.marker`。用户把单文件 exe 单独取出（无 marker）
// 启动时，数据落进 `%LOCALAPPDATA%\SHANIU`、exe 旁边不生成 `data\`——"单文件 exe
// 发群、数据在 exe 同目录"的发放形态因此不成立。
//
// 现在的规则：**首次启动判定一次，之后一律按记录走，绝不每次重新探测**。
// 为什么不能每次探测：U 盘只读开关、目录权限变化、杀软临时拦截都会让两次启动落到
// 不同位置；数据根一旦漂移，用户看到的是"历史没了"——那比"路径不理想"严重一个量级。
// 路径不理想最多是用户手动搬一次目录；数据分裂是两份历史、无法自动合并。

/// 数据根来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataRootKind {
    /// 数据在程序旁边（`<exe 目录>\data`）。
    Portable,
    /// 数据在用户目录（`%APPDATA%\SHANIU` / `%LOCALAPPDATA%\SHANIU`）。
    Installed,
}

impl DataRootKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Portable => "portable",
            Self::Installed => "installed",
        }
    }
}

/// 数据根记录：`<用户目录>\SHANIU\data-root\<exe 目录 hash>.json`。
///
/// **按 exe 目录分文件，不是全机一份**：同一台机器可以有两份便携副本，各自 `data\`
/// 互不相干、也允许同时运行。全机一份记录会把它们强行指向同一个根——第二份要么被
/// 单实例锁拦下，要么更糟：两份历史混进同一个目录。
pub const DATA_ROOT_RECORD_VERSION: u32 = 1;
pub const DATA_ROOT_RECORD_DIR: &str = "data-root";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataRootRecord {
    pub version: u32,
    pub kind: DataRootKind,
    /// 判定当时的数据根绝对路径。
    pub path: String,
    /// 判定当时的 exe 目录（排障用，不参与判定；exe 移动后靠它一眼看出记录是旧的）。
    pub exe_dir: String,
}

/// 一次数据根解析的完整结果。
#[derive(Debug, Clone)]
pub struct RootResolution {
    pub dirs: DataDirs,
    /// `None` = `SHANIU_DATA_DIR` 显式覆盖（不落记录，也不算"首次判定"）。
    pub kind: Option<DataRootKind>,
    /// true = 本次做了首次判定，需要把记录落盘。
    pub decided_now: bool,
    pub record_path: PathBuf,
    /// 必须明确告诉用户的问题（记录与实际不符、记录写不下）；`None` = 一致。
    pub warning: Option<String>,
    /// 记录/旧目录说便携、但当前不可写：给用户一条明确出口（改用安装模式）。
    /// `Some` 时由启动层询问用户；**未确认不得自动切换**（数据分裂）。
    pub fallback_to_installed: Option<DataDirs>,
    /// 用户在出口选择"退出"：启动层据此中止，不许继续用不可写目录。
    pub aborted: bool,
    /// 走出口切换到安装模式时原来的数据位置（只为日志/提示，不参与判定）。
    pub switched_from: Option<PathBuf>,
}

/// 记录文件路径：按 exe 目录 hash 分文件，放在用户目录（而不是 exe 旁边）——
/// exe 旁边恰恰可能是只读的，记录写不下去就退化成"每次重新判定"，正是要防的。
pub fn data_root_record_path(
    exe_dir: Option<&Path>,
    appdata: Option<&Path>,
    localdata: Option<&Path>,
) -> PathBuf {
    let base = localdata
        .or(appdata)
        .map(|root| root.join(APP_DIR_NAME))
        .unwrap_or_else(|| std::env::temp_dir().join(APP_DIR_NAME));
    let key = exe_dir.map(path_hash).unwrap_or(0);
    base.join(DATA_ROOT_RECORD_DIR).join(format!("{key:016x}.json"))
}

/// 路径的稳定 hash：canonicalize 小写后按 UTF-16 取字节。
/// `DefaultHasher` 不保证跨 Rust 版本稳定——升级工具链后记录文件名会变，
/// 等于把"之后固定"作废。
pub(crate) fn path_hash(path: &Path) -> u64 {
    let full = fs::canonicalize(path)
        .or_else(|_| std::path::absolute(path))
        .unwrap_or_else(|_| path.to_path_buf());
    let lowered = full.to_string_lossy().to_lowercase();
    let mut bytes = Vec::with_capacity(lowered.len() * 2);
    for unit in lowered.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    single_instance::fnv1a_64(&bytes)
}

/// 真实探测"能建目录**并且**能写文件"。
///
/// 只判 `create_dir_all` 不够：目录可能早就存在（只读介质、权限被收回），mkdir 成功
/// 但一写就失败——那正是数据分裂最典型的触发条件。
fn probe_data_dir_writable(dir: &Path) -> bool {
    if fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(format!(".shaniu-write-probe-{}", std::process::id()));
    let ok = fs::write(&probe, b"ok").is_ok();
    let _ = fs::remove_file(&probe);
    ok
}

fn same_path(left: &Path, right: &Path) -> bool {
    let norm = |path: &Path| {
        path.to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_lowercase()
    };
    norm(left) == norm(right)
}

fn unwritable_warning(dirs: &DataDirs, record_path: &Path) -> String {
    format!(
        "数据目录现在写不进去：{}\n\n\
         程序不会自动换到别的位置（换了会把已有历史分成两处，看起来就像全丢了），\
         因此仍按原位置启动，但这次可能存不下新内容。\n\n\
         请恢复该目录的写权限。要重新选择位置，删除记录文件后重启：\n{}",
        dirs.data_root.display(),
        record_path.display()
    )
}

/// 便携目录不可写时的出口提示：用户点「是」才切到安装模式，原目录原样保留。
/// 没有这条出口，只读介质/权限变化会把用户卡在"只能退出"的状态。
fn portable_unwritable_prompt(dirs: &DataDirs, installed: &DataDirs, record_path: &Path) -> String {
    format!(
        "你的数据本来在：\n{}\n\n\
         这个目录现在写不进去（可能是 U 盘只读、权限变化或安全软件拦截）。\n\n\
         选择「是」：改用安装模式，数据放到\n{}\n\
         你现有的数据仍在 {}，**不会被删除或移动**。\n\n\
         选择「否」：退出，什么都不改。\n\n\
         记录文件：{}（要改回便携，删除它后重启）",
        dirs.data_root.display(),
        installed.data_root.display(),
        dirs.data_root.display(),
        record_path.display()
    )
}

/// 纯判定：输入显式注入（含"可写探测"这个可替换的副作用点），可单测每一条分支。
///
/// 优先级：`SHANIU_DATA_DIR` > 已有记录（**不再探测**）> 旧 `portable.marker` /
/// 已存在的 `<exe>\data` > 可写性探测 > 安装版默认。
pub fn decide_root(
    env_data_dir: Option<&str>,
    exe_dir: Option<&Path>,
    appdata: Option<&Path>,
    localdata: Option<&Path>,
    record: Option<&DataRootRecord>,
    writable: impl Fn(&Path) -> bool,
) -> RootResolution {
    let record_path = data_root_record_path(exe_dir, appdata, localdata);

    // 0）显式覆盖：测试/CI/多实例隔离用，不读不写记录。
    if let Some(dir) = env_data_dir.map(str::trim).filter(|dir| !dir.is_empty()) {
        let root = PathBuf::from(dir);
        return RootResolution {
            dirs: DataDirs { config_root: root.clone(), data_root: root },
            kind: None,
            decided_now: false,
            record_path,
            warning: None,
            fallback_to_installed: None,
            aborted: false,
            switched_from: None,
        };
    }

    // 1）有记录：一律按记录。探测只用于**发现不一致并告知**，绝不用于换位置。
    if let Some(record) = record {
        let root = PathBuf::from(&record.path);
        let dirs = DataDirs { config_root: root.clone(), data_root: root.clone() };
        let expected = match record.kind {
            DataRootKind::Portable => exe_dir.map(|dir| dir.join("data")),
            DataRootKind::Installed => Some(installed_dirs(appdata, localdata).data_root),
        };
        // exe 被挪走 / 系统用户目录变了：记录仍有效，但它已经和"当前该在的位置"不同了。
        let moved = expected
            .as_deref()
            .map(|expected| !same_path(expected, &root))
            .unwrap_or(false);
        let is_writable = writable(&root);
        // 只有"位置没变、只是这次写不进去"才给安装模式出口；位置已经变了（moved）
        // 说明用户挪过 exe，切换会把两处历史分开，不能拿这个当出口。
        let fallback = if !moved && !is_writable && record.kind == DataRootKind::Portable {
            Some(installed_dirs(appdata, localdata))
        } else {
            None
        };
        let warning = if moved {
            Some(format!(
                "数据位置记录显示数据在：{}\n\
                 但当前程序/系统默认位置指向：{}\n\n\
                 程序按记录继续使用原数据位置（不会自动切换，否则历史会分成两处）。\n\
                 要改用当前位置：把原数据目录整体移到新位置后，删除记录文件再重启：\n{}",
                root.display(),
                expected.as_deref().map(|p| p.display().to_string()).unwrap_or_else(|| "（未知）".into()),
                record_path.display()
            ))
        } else if let Some(installed) = &fallback {
            Some(portable_unwritable_prompt(&dirs, installed, &record_path))
        } else if !is_writable {
            Some(unwritable_warning(&dirs, &record_path))
        } else {
            None
        };
        return RootResolution {
            dirs,
            kind: Some(record.kind),
            decided_now: false,
            record_path,
            warning,
            fallback_to_installed: fallback,
            aborted: false,
            switched_from: None,
        };
    }

    // 2）首次判定。旧 marker 与"已经存在的 data 目录"优先于可写性探测：
    //    已存在的数据即便当前不可写也不能丢下——那等于让用户"历史没了"。
    if let Some(exe) = exe_dir {
        let portable_root = exe.join("data");
        if exe.join(PORTABLE_MARKER).exists() || portable_root.is_dir() {
            let dirs = DataDirs { config_root: portable_root.clone(), data_root: portable_root.clone() };
            let is_writable = writable(&portable_root);
            let fallback = if is_writable {
                None
            } else {
                Some(installed_dirs(appdata, localdata))
            };
            let warning = fallback
                .as_ref()
                .map(|installed| portable_unwritable_prompt(&dirs, installed, &record_path));
            return RootResolution {
                dirs,
                kind: Some(DataRootKind::Portable),
                decided_now: true,
                record_path,
                warning,
                fallback_to_installed: fallback,
                aborted: false,
                switched_from: None,
            };
        }
        if writable(&portable_root) {
            let dirs = DataDirs { config_root: portable_root.clone(), data_root: portable_root };
            return RootResolution {
                dirs,
                kind: Some(DataRootKind::Portable),
                decided_now: true,
                record_path,
                warning: None,
                fallback_to_installed: None,
                aborted: false,
                switched_from: None,
            };
        }
    }

    // 3）程序旁边写不进去（Program Files、只读介质）→ 安装版默认。
    let dirs = installed_dirs(appdata, localdata);
    let warning = if writable(&dirs.data_root) {
        None
    } else {
        Some(unwritable_warning(&dirs, &record_path))
    };
    RootResolution {
        dirs,
        kind: Some(DataRootKind::Installed),
        decided_now: true,
        record_path,
        warning,
        fallback_to_installed: None,
        aborted: false,
        switched_from: None,
    }
}

enum RecordRead {
    Missing,
    Valid(DataRootRecord),
    Corrupt(String),
}

fn read_record(path: &Path) -> RecordRead {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return RecordRead::Missing,
        Err(e) => return RecordRead::Corrupt(format!("读不到: {e}")),
    };
    match serde_json::from_str::<DataRootRecord>(&raw) {
        Ok(record) if record.version == DATA_ROOT_RECORD_VERSION => RecordRead::Valid(record),
        Ok(record) => RecordRead::Corrupt(format!(
            "版本 {} 不认识（期望 {}）",
            record.version, DATA_ROOT_RECORD_VERSION
        )),
        Err(e) => RecordRead::Corrupt(format!("内容不是有效的 JSON: {e}")),
    }
}

fn write_record(path: &Path, record: &DataRootRecord) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("创建记录目录失败 {}: {e}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(record).map_err(|e| format!("序列化记录失败: {e}"))?;
    let tmp = temp_sibling(path);
    if let Err(e) = fs::write(&tmp, body).and_then(|()| fs::rename(&tmp, path)) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("写数据位置记录失败 {}: {e}", path.display()));
    }
    Ok(())
}

/// 进程内唯一的数据根解析结果（`run()` 第一步初始化；之后的调用都读这一份）。
static ROOT_RESOLUTION: OnceLock<RootResolution> = OnceLock::new();

/// 当前进程的数据根（未初始化时 `None`）。
///
/// 给"不持有 `AppState`、又必须判断路径是否落在数据根内"的地方用（工作区判据、
/// 沙箱 ACL 兜底）。生产启动路径一定先调用 `init_root_resolution*`，所以拿到的
/// 就是本次实际使用的目录；单元测试直接调判据时走显式传参的 `check_against`。
pub fn current_data_root() -> Option<&'static Path> {
    ROOT_RESOLUTION.get().map(|resolution| resolution.dirs.data_root.as_path())
}

/// 真实启动入口（不带交互）：读记录 → 只在没有记录时判定一次 → 需要时落记录。
/// 便携目录不可写时只返回提示（`fallback_to_installed`），由调用方决定是否询问用户。
pub fn init_root_resolution() -> &'static RootResolution {
    ROOT_RESOLUTION.get_or_init(build_root_resolution)
}

/// 带用户确认的启动入口：便携目录不可写时询问一次。用户同意 → 落"安装模式"记录并
/// 改用 `%LOCALAPPDATA%`（原 `data\` 原样保留）；不同意 → `aborted = true`，启动层退出。
/// 判定只发生一次，之后不受影响。
pub fn init_root_resolution_with(
    confirm: impl Fn(&RootResolution) -> bool + 'static,
) -> &'static RootResolution {
    ROOT_RESOLUTION.get_or_init(|| {
        let mut resolution = build_root_resolution();
        switch_to_installed_if_confirmed(&mut resolution, &confirm);
        resolution
    })
}

fn switch_to_installed_if_confirmed(
    resolution: &mut RootResolution,
    confirm: &dyn Fn(&RootResolution) -> bool,
) {
    let Some(installed) = resolution.fallback_to_installed.clone() else {
        return;
    };
    if !confirm(resolution) {
        // 没有确认就不切换，也不许继续用不可写目录：交给启动层退出。
        resolution.aborted = true;
        return;
    }
    let from = resolution.dirs.data_root.clone();
    let record = DataRootRecord {
        version: DATA_ROOT_RECORD_VERSION,
        kind: DataRootKind::Installed,
        path: installed.data_root.display().to_string(),
        exe_dir: std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(|dir| dir.display().to_string()))
            .unwrap_or_default(),
    };
    let mut warning = None;
    if let Err(error) = write_record(&resolution.record_path, &record) {
        // 记录写不下 = 下次启动还会再问一遍；不拦启动，但必须留痕。
        warning = Some(error);
    }
    resolution.dirs = installed;
    resolution.kind = Some(DataRootKind::Installed);
    resolution.decided_now = false;
    resolution.fallback_to_installed = None;
    resolution.switched_from = Some(from);
    resolution.warning = warning;
}

fn build_root_resolution() -> RootResolution {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf));
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from);
    let localdata = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let env_data_dir = std::env::var(DATA_DIR_ENV).ok();

    let record_path = data_root_record_path(
        exe_dir.as_deref(),
        appdata.as_deref(),
        localdata.as_deref(),
    );
    let (record, mut warning) = match read_record(&record_path) {
        RecordRead::Missing => (None, None),
        RecordRead::Valid(record) => (Some(record), None),
        // 记录坏了也不能静默重新判定（那可能把历史分到第二处）。这里按现有目录判定，
        // 但必须把"记录坏了、怎么修"说清楚。
        RecordRead::Corrupt(reason) => (
            None,
            Some(format!(
                "数据位置记录损坏（{}）：{reason}\n\n\
                 本次按现有目录重新判定。如果之前的数据在别处，请先用 SHANIU_DATA_DIR \
                 指过去再启动，或修复/删除这个记录文件后重启。",
                record_path.display()
            )),
        ),
    };

    let mut resolution = decide_root(
        env_data_dir.as_deref(),
        exe_dir.as_deref(),
        appdata.as_deref(),
        localdata.as_deref(),
        record.as_ref(),
        probe_data_dir_writable,
    );

    if resolution.decided_now {
        let record = DataRootRecord {
            version: DATA_ROOT_RECORD_VERSION,
            kind: resolution.kind.unwrap_or(DataRootKind::Installed),
            path: resolution.dirs.data_root.display().to_string(),
            exe_dir: exe_dir
                .as_deref()
                .map(|dir| dir.display().to_string())
                .unwrap_or_default(),
        };
        if let Err(error) = write_record(&resolution.record_path, &record) {
            // 记录落不下 = 下次启动会重新判定 = 可能数据分裂，必须让用户知道。
            warning = Some(match warning {
                Some(prev) => format!("{prev}\n\n{error}"),
                None => error,
            });
        }
    }

    if let Some(extra) = resolution.warning.take() {
        warning = Some(match warning {
            Some(prev) => format!("{prev}\n\n{extra}"),
            None => extra,
        });
    }
    resolution.warning = warning;
    resolution
}

/// 读取真实环境后的解析入口。
///
/// 生产启动第一步就是 [`init_root_resolution`]，所以这里直接返回那一份结果；只有
/// 测试/工具在启动前直接调用时才走不探测、不落盘的旧口径。
pub fn resolve_dirs_from_env() -> DataDirs {
    if let Some(resolution) = ROOT_RESOLUTION.get() {
        return resolution.dirs.clone();
    }
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    resolve_dirs(
        std::env::var(DATA_DIR_ENV).ok().as_deref(),
        exe_dir.as_deref(),
        std::env::var_os("APPDATA").map(PathBuf::from).as_deref(),
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from).as_deref(),
    )
}

/// 启动加载的处置结果：调用方据此决定是否进入「配置只读」模式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigLoadOutcome {
    /// 配置干净，无需动作。
    Clean,
    /// 已剔除写串键并写回；`backup` 是修复前的原文件快照。
    Repaired { backup: PathBuf, removed: Vec<String> },
    /// 无法安全修复：**磁盘文件一个字节未动**，调用方必须禁止再写回，
    /// 并把 `reason`（含路径）告诉用户。
    ReadOnly { reason: String },
}

/// 一次启动加载的完整结果（配置 + 路径 + 处置）。
pub struct LoadedConfig {
    pub config: AppConfig,
    pub path: PathBuf,
    pub outcome: ConfigLoadOutcome,
}

impl AppConfig {
    /// 加载 `config.toml`；不存在则写入默认配置并返回（fail-closed：解析失败即报错）。
    pub fn load_or_create(config_root: &Path) -> Result<(Self, PathBuf), String> {
        let loaded = Self::load_or_create_diagnosed(config_root);
        match loaded.outcome {
            ConfigLoadOutcome::ReadOnly { reason } => Err(reason),
            _ => Ok((loaded.config, loaded.path)),
        }
    }

    /// 启动加载 + **一次性修复**已写坏的配置（串段键）。
    ///
    /// 只做一种自动修复：`reasoning_efforts` 里出现非档位键（`path` / `title` 这类
    /// 别的 section 的键）。流程固定为「备份 → 剔除 → 写回 → 记日志」，任一步失败
    /// 都退回只读，**绝不覆盖无法确认可用的文件**。
    ///
    /// 幂等：修完的配置不再含写串键，下一次启动命中 `Clean`、不再产生备份。
    pub fn load_or_create_diagnosed(config_root: &Path) -> LoadedConfig {
        let path = config_root.join(CONFIG_FILE);
        let read_only = |reason: String| LoadedConfig {
            config: Self::default(),
            path: path.clone(),
            outcome: ConfigLoadOutcome::ReadOnly { reason },
        };

        if let Some(parent) = path.parent() {
            if let Err(error) = fs::create_dir_all(parent) {
                return read_only(format!(
                    "创建配置目录失败: {error}（配置文件：{}）",
                    path.display()
                ));
            }
        }
        if !path.exists() {
            let default = Self::fresh_install();
            return match default.save(&path) {
                Ok(()) => LoadedConfig { config: default, path, outcome: ConfigLoadOutcome::Clean },
                Err(error) => LoadedConfig {
                    config: Self::fresh_install(),
                    path,
                    outcome: ConfigLoadOutcome::ReadOnly { reason: error },
                },
            };
        }

        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) => {
                return read_only(format!("读取配置失败: {error}（配置文件：{}）", path.display()))
            }
        };
        let mut cfg: Self = match toml::from_str(&raw) {
            Ok(cfg) => cfg,
            Err(error) => {
                return read_only(format!(
                    "解析配置失败: {error}（文件未被改动：{}）",
                    path.display()
                ))
            }
        };
        if let Err(error) = cfg.validate_loaded() {
            return read_only(format!("{error}（文件未被改动：{}）", path.display()));
        }
        cfg.migrate_legacy_sandbox_os(&path);

        let stray = cfg.stray_reasoning_keys();
        if stray.is_empty() {
            return LoadedConfig { config: cfg, path, outcome: ConfigLoadOutcome::Clean };
        }

        // 修复前先备份；备份失败就不改文件（没有备份更不该写）。
        let Some(backup) = backup_original(&path) else {
            return read_only(format!(
                "配置里有写串键（{}），但备份原文件失败，已拒绝自动修复（文件未被改动：{}）",
                stray.join("；"),
                path.display()
            ));
        };
        cfg.strip_stray_reasoning_keys();
        match cfg.save(&path) {
            Ok(()) => {
                tracing::warn!(
                    config = %path.display(),
                    backup = %backup.display(),
                    removed = %stray.join("；"),
                    "配置已自动修复：剔除了不属于 [providers.models.reasoning_efforts] 的键（原文件已备份）"
                );
                LoadedConfig {
                    config: cfg,
                    path,
                    outcome: ConfigLoadOutcome::Repaired { backup, removed: stray },
                }
            }
            Err(error) => read_only(format!(
                "剔除写串键（{}）后仍无法写回：{error}；配置文件：{}",
                stray.join("；"),
                path.display()
            )),
        }
    }

    /// 一次性迁移：把旧版写死的默认机制换成新默认（写约束）。
    ///
    /// 上下文：旧版本启动时会把**默认值**一起序列化进 config.toml，所以老文件里
    /// 有 `os = "app_container"` 并不代表用户选过 AppContainer。`os_explicit`
    /// 缺失（旧文件）时按旧默认迁移；用户想留在 AppContainer 就写
    /// `os_explicit = true`（迁移后写回的文件也带这个标记，不会被反复改）。
    ///
    /// 纪律：**先备份再写回**；写不回去只在本进程内按新默认运行并留 warn，
    /// 下次启动重试（幂等）。绝不静默——日志与状态面都会显示生效机制。
    fn migrate_legacy_sandbox_os(&mut self, path: &Path) {
        if self.sandbox.os != OsSandbox::AppContainer || self.sandbox.os_explicit {
            return;
        }
        let backup = backup_original(path);
        self.sandbox.os = OsSandbox::RestrictedToken;
        self.sandbox.os_explicit = true;
        match self.save(path) {
            Ok(()) => tracing::warn!(
                config = %path.display(),
                backup = %backup.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
                "沙箱默认机制已迁移：app_container → restricted_token（只约束写；\
                 要留在 AppContainer 请写 os_explicit = true，原文件已备份）"
            ),
            Err(error) => tracing::warn!(
                config = %path.display(),
                %error,
                "沙箱默认机制迁移未能写回：本次按 restricted_token 运行，下次启动重试"
            ),
        }
    }

    /// 写盘。**先自检、再原子替换**：配置写坏会静默覆盖用户配置，比写失败严重得多。
    ///
    /// 两道写前检查：
    ///  1. `[providers.models.reasoning_efforts]` 的键必须全部是推理档位名。实测过
    ///     一起真实事故：该表里出现了 `path` / `title`（`[[workspaces]]` 的键），
    ///     而模型档位只剩这两个键——用户的推理档声明被覆盖，界面上看着"没配档位"。
    ///  2. 序列化结果反序列化回来必须与内存逐字段相等。section 归属错、丢键、值
    ///     类型漂移都会让这一条不成立。
    ///
    /// 任一条不过就**拒绝落盘**，并把磁盘上的原文件备份成 `config.toml.bak-<ms>`
    /// 供取证；原文件本身一个字节都不动。
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let stray = self.stray_reasoning_keys();
        if !stray.is_empty() {
            return Err(self.refuse_write(
                path,
                format!(
                    "[providers.models.reasoning_efforts] 里出现了非推理档位的键：{}。\
                     该表只允许 {} / {}。这是「别的 section 被写串」的典型形态，\
                     继续写盘会把用户的推理档声明抹掉",
                    stray.join("、"),
                    THINKING_LEVELS.join("/"),
                    "（推荐先删掉这些键再保存）",
                ),
            ));
        }

        let body = toml::to_string_pretty(self).map_err(|e| format!("序列化配置失败: {e}"))?;
        let back: Self = toml::from_str(&body).map_err(|e| {
            self.refuse_write(path, format!("序列化结果无法解析回 AppConfig：{e}（拒绝落盘，否则下次启动直接读不回来）"))
        })?;
        if back != *self {
            return Err(self.refuse_write(
                path,
                "序列化结果与内存配置不一致：section 归属或字段丢失（拒绝落盘）".to_string(),
            ));
        }

        // 原子替换：先写同目录临时文件再 rename。直接 fs::write 中途崩溃会留下
        // 半截 config.toml——下次启动读不回来，用户看到的就是"配置全没了"。
        // 临时名唯一（pid + 序号）：并发写不同目标时固定名会互相抢同一个临时文件。
        let tmp = temp_sibling(path);
        fs::write(&tmp, &body).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            format!("写入配置失败: {e}")
        })?;
        if let Err(error) = fs::rename(&tmp, path) {
            let _ = fs::remove_file(&tmp);
            return Err(format!("替换配置失败: {error}"));
        }
        Ok(())
    }

    /// 档位表里不属于 `THINKING_LEVELS` 的键（`服务商/模型: 键` 形态，供报错与取证）。
    fn stray_reasoning_keys(&self) -> Vec<String> {
        let mut out = Vec::new();
        for provider in &self.providers {
            for row in &provider.models {
                let keys: Vec<&str> = row
                    .reasoning_efforts
                    .keys()
                    .map(String::as_str)
                    .filter(|key| !THINKING_LEVELS.contains(key))
                    .collect();
                if !keys.is_empty() {
                    out.push(format!("{}/{}: {}", provider.id, row.id, keys.join(", ")));
                }
            }
        }
        out
    }

    /// 剔除档位表里不属于 `THINKING_LEVELS` 的键（一次性修复用；返回剔除条数）。
    fn strip_stray_reasoning_keys(&mut self) -> usize {
        let mut removed = 0usize;
        for provider in &mut self.providers {
            for row in &mut provider.models {
                let before = row.reasoning_efforts.len();
                row.reasoning_efforts
                    .retain(|key, _| THINKING_LEVELS.contains(&key.as_str()));
                removed += before - row.reasoning_efforts.len();
            }
        }
        removed
    }

    /// 拒绝落盘时的统一处理：备份磁盘原文件（存在才备份），拼出可操作的错误。
    /// 备份失败也照样拒绝——**没有备份就更不能写**。
    fn refuse_write(&self, path: &Path, reason: String) -> String {
        match backup_original(path) {
            Some(backup) => format!(
                "{reason}。已拒绝写入，原文件备份在 {}",
                backup.display()
            ),
            None => format!("{reason}。已拒绝写入（原文件不存在或备份失败，因此一个字节都没动）"),
        }
    }
}

/// 配置原子写的临时文件序号：同一毫秒内并发 save 也不互相抢同一个临时文件。
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// 同目录唯一临时文件名：`<目标名>.tmp-<pid>-<序号>`。
///
/// 原子替换（写临时文件 → rename）必须用唯一名：固定成 `xxx.json.tmp` 时，两个写者
/// 会踩同一个临时文件——rename 目标被占用直接失败，或者后写者把前写者的内容覆盖成
/// last-write-wins。pid 区分进程、序号区分同进程内的并发写（2026-11-11）。
pub(crate) fn temp_sibling(target: &Path) -> PathBuf {
    let base = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("shaniu");
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    target.with_file_name(format!("{base}.tmp-{}-{seq}", std::process::id()))
}

/// 拒绝落盘前把磁盘上的原文件复制一份（`config.toml.bak-<毫秒>`，重名则加序号）。
/// 备份只为取证：原文件本身不会被改写。
fn backup_original(path: &Path) -> Option<PathBuf> {
    if !path.is_file() {
        return None;
    }
    let ms = crate::clock::now_ms();
    for attempt in 0..10 {
        let name = if attempt == 0 {
            format!("{CONFIG_FILE}.bak-{ms}")
        } else {
            format!("{CONFIG_FILE}.bak-{ms}-{attempt}")
        };
        let candidate = path.with_file_name(name);
        if candidate.exists() {
            continue;
        }
        return fs::copy(path, &candidate).ok().map(|_| candidate);
    }
    None
}

/// 单调会话 ID（M3 rollout 接入后换正式 ULID/UUID）。
pub fn new_session_id(seq: u32) -> String {
    let ms = crate::clock::now_ms();
    format!("sess-{ms}-{seq:04}")
}

// ============================== DPAPI 密钥存取（docs §9.1 secrets\） ==============================
//
// 边界（AGENTS §3 / docs §9.3）：Key 永不进代码/提示词/日志；仅本模块读写；
// DPAPI 加密绑定当前机器当前用户——跨机器解密失败一律 Err（fail-closed，不回退明文）。

use windows::core::PCWSTR;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB,
};

/// DPAPI 加密（用户作用域）。输出 blob 需 LocalFree，务必走完释放路径。
fn dpapi_protect(plaintext: &[u8]) -> Result<Vec<u8>, String> {
    unsafe {
        let input = CRYPT_INTEGER_BLOB {
            cbData: plaintext.len() as u32,
            pbData: plaintext.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB::default();
        CryptProtectData(
            &input,
            PCWSTR::null(),
            None,
            None,
            None,
            0,
            &mut out,
        )
        .map_err(|e| format!("DPAPI 加密失败: {e}"))?;
        let cipher = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out.pbData.cast())));
        Ok(cipher)
    }
}

/// DPAPI 解密。任何失败都返回 Err——绝不降级输出半截数据。
fn dpapi_unprotect(cipher: &[u8]) -> Result<Vec<u8>, String> {
    unsafe {
        let input = CRYPT_INTEGER_BLOB {
            cbData: cipher.len() as u32,
            pbData: cipher.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB::default();
        CryptUnprotectData(&input, None, None, None, None, 0, &mut out)
            .map_err(|_| "DPAPI 解密失败（密文损坏或跨机器迁移）".to_string())?;
        let plain = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out.pbData.cast())));
        Ok(plain)
    }
}

/// 密钥存储：`<config_root>/secrets/<provider>.key`（DPAPI 密文，仅当前用户可解）。
#[derive(Clone)]
pub struct SecretStore {
    root: std::path::PathBuf,
}

impl SecretStore {
    pub fn new(config_root: &Path) -> Self {
        Self { root: config_root.to_path_buf() }
    }

    /// provider 名清洗为文件系统安全 slug，阻断路径分隔符注入。
    fn sanitize_provider(provider: &str) -> Result<String, String> {
        let safe: String = provider
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if safe.is_empty() {
            return Err("provider 名非法".into());
        }
        Ok(safe)
    }

    fn key_path(&self, provider: &str) -> Result<PathBuf, String> {
        Ok(self.root.join(SECRETS_DIR).join(format!(
            "{}.key",
            Self::sanitize_provider(provider)?
        )))
    }

    /// 存储并加密。空密钥拒绝。明文随手登记进脱敏表（本会话刚存过的 key 同样不能出现在日志里）。
    pub fn store_key(&self, provider: &str, plaintext: &str) -> Result<(), String> {
        if plaintext.trim().is_empty() {
            return Err("密钥为空".into());
        }
        register_secret(plaintext);
        let cipher = dpapi_protect(plaintext.as_bytes())?;
        let path = self.key_path(provider)?;
        fs::create_dir_all(path.parent().expect("密钥路径必有父目录"))
            .map_err(|e| format!("创建 secrets 目录失败: {e}"))?;
        fs::write(&path, cipher).map_err(|e| format!("写入密钥失败: {e}"))
    }

    /// 读取并解密；不存在返回 Ok(None)。解密失败（损坏/跨机器）返回 Err——fail-closed。
    /// 解出的明文随手登记进脱敏表：这是唯一知道"当前密钥长什么样"的时刻。
    pub fn load_key(&self, provider: &str) -> Result<Option<String>, String> {
        let path = self.key_path(provider)?;
        if !path.exists() {
            return Ok(None);
        }
        let cipher = fs::read(&path).map_err(|e| format!("读取密钥失败: {e}"))?;
        let plain = dpapi_unprotect(&cipher)?;
        let key = String::from_utf8(plain).map_err(|_| "密钥编码非法".to_string())?;
        register_secret(&key);
        Ok(Some(key))
    }

    pub fn delete_key(&self, provider: &str) -> Result<(), String> {
        let path = self.key_path(provider)?;
        if path.exists() {
            fs::remove_file(&path).map_err(|e| format!("删除密钥失败: {e}"))?;
        }
        Ok(())
    }
}

// ===== SH-042：落盘前统一脱敏（docs/01 §风险登记册 R2、§9.3 目录表 logs\ 行）=====
// 放在本模块的理由：密钥的唯一出入口是 SecretStore，脱敏要登记"当前密钥长什么样"，
// 两件事拆开就必然出现"存了 key 却没人登记"的缝隙。SH-042 卡面写的就是 config.rs+日志。

/// 落盘脱敏标记。不回显任何密钥片段——连前缀和长度都不给。
pub const REDACTED: &str = "[REDACTED]";

/// 已知密钥原文登记表（进程级）。正则只能认"长得像 key"的串，而 DPAPI 解出的
/// 密钥可以是任意形状，唯一可靠的办法是把原文登记下来、落盘前整串替换。
static SECRET_REGISTRY: RwLock<Vec<String>> = RwLock::new(Vec::new());

/// 登记一个密钥原文。空串与纯空白忽略：空串会匹配任何位置，等于把整篇文本都抹掉。
pub fn register_secret(secret: &str) {
    let s = secret.trim();
    if s.is_empty() {
        return;
    }
    // 测试下所有写入方共用一把锁（见 `secret_registry_test_lock`）。
    // 锁在这里，而不是让每个用例各自加：`register_header_secrets` 这类间接写入
    // 散落在别的用例里，逐个补是补不完的——漏一个就还是那条偶发红。
    #[cfg(test)]
    let _serialize = secret_registry_test_lock();
    let mut reg = SECRET_REGISTRY.write().unwrap_or_else(|e| e.into_inner());
    if !reg.iter().any(|x| x == s) {
        reg.push(s.to_string());
    }
}

/// 测试专用：**所有**登记密钥的用例共用这一把锁。
///
/// 崩溃路径用 `try_read` 是**产品行为，不是测试缺陷**——panic hook 里再阻塞会把
/// "崩溃退出"变成"卡死"，这条不能为了让用例变绿而改成 `read`。代价是并行跑时，
/// 只要别的用例正持写锁，`write_crash_report` 就退化成纯模式脱敏，断言"登记密钥
/// 必须整串替换"偶发失败。因此写入方（[`register_secret`] 内部）一律先过这把锁；
/// 崩溃用例则先登记、再持锁跑完整条写报告流程，保证那段时间没有新写入。
///
/// **持锁期间不得再调 `register_secret`**（`std::sync::Mutex` 不可重入，会自锁）。
#[cfg(test)]
pub(crate) fn secret_registry_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// 自定义请求头值能被整串替换的最短长度。
///
/// **这是长度启发式，不是确定性判断**：它不保证短凭据不泄漏，只保证常见形态
/// （签名、令牌、base64 串）不漏。低于该值不做原文登记：`redact` 是**逐字面量
/// 替换**，把 `X-Debug: 1` 的 `1` 登记进去，日志与 events.jsonl 里每个 `1` 都会
/// 变成 `[REDACTED]`——那会毁掉会话回放（模型看到的正文被抹穿），比漏脱敏一个
/// 短值更糟。8 字符与上面 "Bearer ≥8" 的既有阈值同口径。
/// 已知代价见 `docs/04` §4「已知边界」：7 字符及以下的头值会明文落盘。
const HEADER_SECRET_MIN_CHARS: usize = 8;

/// 把服务商自定义请求头的**全部值**登记为密钥（审计 M8）。
///
/// 不做头名匹配：`X-Api-Key` / `X-Auth-Token` / 厂商自造名都可能漏，漏一个等于没做。
/// 只在配置装载与写入两个入口调用，保证任何一次日志/事件落盘之前登记表已经就绪。
pub fn register_header_secrets(providers: &[ProviderProfile]) {
    for p in providers {
        for value in p.headers.values() {
            let v = value.trim();
            if v.chars().count() >= HEADER_SECRET_MIN_CHARS {
                register_secret(v);
            }
        }
    }
}

struct Pattern {
    re: Regex,
    /// 替换模板：无捕获组用全匹配替换；有捕获组保留键名/方案名只抹值。
    template: &'static str,
}

fn patterns() -> &'static [Pattern] {
    static PATTERNS: OnceLock<Vec<Pattern>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        vec![
            // OpenAI 风格 key：sk- 前缀 + 至少 8 个 key 字符（未登记的陌生 key 也拦得住）
            Pattern { re: Regex::new(r"sk-[A-Za-z0-9_-]{8,}").expect("内建正则合法"), template: REDACTED },
            // HTTP 认证头：Bearer xxx / Basic xxx
            Pattern {
                re: Regex::new(r#"(?i)\b(bearer|basic)\s+[A-Za-z0-9._~+/=-]{8,}"#).expect("内建正则合法"),
                template: "${1} [REDACTED]",
            },
            // 键值形态：api_key=xxx / "apiKey": "xxx" / token: xxx
            Pattern {
                re: Regex::new(r#"(?i)((?:api[_-]?key|secret|access[_-]?token|password)"?\s*[:=]\s*"?)[^\s",}]{6,}"#)
                    .expect("内建正则合法"),
                template: "${1}[REDACTED]",
            },
        ]
    })
}

/// 崩溃报告序号：同一毫秒内两次 panic 也不互相覆盖。
static CRASH_SEQ: AtomicU64 = AtomicU64::new(0);

/// 脱敏正文：先整串替换已登记密钥，再按模式擦掉形似密钥的串。
///
/// 两条调用路径**只差取锁方式**（崩溃路径不能阻塞），正文必须共用这一份——
/// 复制粘贴出来的「几乎一样」终会漂移，而这里是密钥落盘前的最后一道闸。
fn scrub<'a>(text: &str, secrets: impl IntoIterator<Item = &'a str>) -> String {
    let mut out = text.to_string();
    for secret in secrets {
        if out.contains(secret) {
            out = out.replace(secret, REDACTED);
        }
    }
    for p in patterns() {
        out = p.re.replace_all(&out, p.template).into_owned();
    }
    out
}

/// 崩溃路径专用的 best-effort 脱敏：注册表锁被 panic 持有时退化为纯模式脱敏，
/// 绝不在 panic hook 里再次阻塞（那会把"崩溃退出"变成"卡死"）。
fn redact_for_crash(text: &str) -> String {
    match SECRET_REGISTRY.try_read() {
        Ok(reg) => scrub(text, reg.iter().map(String::as_str)),
        Err(_) => scrub(text, std::iter::empty()),
    }
}

/// 写一份本地崩溃报告到 `<data_root>\crash\panic-<ts>-<pid>-<seq>.log`。
/// **只写本地、不上报**（docs/01 §9.3「待用户确认后才可上报」）；正文过脱敏。
pub fn write_crash_report(
    crash_dir: &Path,
    message: &str,
    location: &str,
    thread: &str,
) -> Result<PathBuf, String> {
    fs::create_dir_all(crash_dir).map_err(|e| format!("创建 crash 目录失败: {e}"))?;
    let stamp = crate::clock::now_ms();
    let seq = CRASH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = crash_dir.join(format!("panic-{stamp}-{}-{seq}.log", std::process::id()));
    let body = format!(
        "time_unix_ms={stamp}\nthread={thread}\nlocation={location}\nmessage={}\n",
        redact_for_crash(message)
    );
    fs::write(&path, body).map_err(|e| format!("写崩溃报告失败（{}）: {e}", path.display()))?;
    Ok(path)
}

/// 安装进程级 panic hook：先落一份本地崩溃报告，再交给原 hook（保留 stderr 行为）。
/// 报告内容不进入日志/提示词，也不上传；失败静默（崩溃路径不能再抛）。
pub fn install_crash_hook(crash_dir: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = if let Some(text) = info.payload().downcast_ref::<&str>() {
            (*text).to_string()
        } else if let Some(text) = info.payload().downcast_ref::<String>() {
            text.clone()
        } else {
            "<non-string panic payload>".to_string()
        };
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let thread = std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_string();
        let _ = write_crash_report(&crash_dir, &message, &location, &thread);
        previous(info);
    }));
}

/// 对任意将要落盘的文本做统一脱敏：先整串替换已登记密钥，再按模式擦掉形似密钥的串。
/// logs 与 rollouts 的所有写入都必须经过这里（docs/01 §438「落盘前统一过脱敏过滤器」）。
pub fn redact(text: &str) -> String {
    // 中毒后照常读：锁里是要保密的清单，panic 掉整个进程比少脱敏一次更糟
    let reg = SECRET_REGISTRY.read().unwrap_or_else(|e| e.into_inner());
    scrub(text, reg.iter().map(String::as_str))
}

/// 按行缓冲并脱敏的 writer：tracing 的一条事件分多次 write 到达（时间戳/级别/正文
/// 各一次），必须攒到换行才算一行完整文本，否则密钥被写调用劈在两段里就匹配不上了。
pub struct RedactingWriter<W: io::Write> {
    inner: W,
    line: Vec<u8>,
}

impl<W: io::Write> RedactingWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner, line: Vec::new() }
    }
}

impl<W: io::Write> io::Write for RedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.line.extend_from_slice(buf);
        while let Some(pos) = self.line.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.line.drain(..=pos).collect();
            let text = String::from_utf8_lossy(&line[..line.len() - 1]);
            self.inner.write_all(redact(&text).as_bytes())?;
            self.inner.write_all(b"\n")?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.line.is_empty() {
            let text = String::from_utf8_lossy(&self.line);
            self.inner.write_all(redact(&text).as_bytes())?;
            self.line.clear();
        }
        self.inner.flush()
    }
}

/// MakeWriter 适配：把 tracing 的落盘路径全部导进 RedactingWriter。
struct RedactingMaker(NonBlocking);

impl<'a> MakeWriter<'a> for RedactingMaker {
    type Writer = RedactingWriter<<NonBlocking as MakeWriter<'a>>::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter { inner: self.0.make_writer(), line: Vec::new() }
    }
}

pub const LOG_RETENTION_DAYS: u64 = 14;
pub const LOG_FILE_PREFIX: &str = "shaniu.log";

/// 组装订阅器（与全局注册拆开：集成测试用 `with_default` 挂线程级订阅，
/// 只有生产入口走 set_global_default——全局订阅器一个进程只能设一次）。
pub fn build_logging(
    data_root: &Path,
) -> Result<(impl tracing::Subscriber + Send + Sync, WorkerGuard), String> {
    let dir = data_root.join(LOGS_DIR);
    fs::create_dir_all(&dir).map_err(|e| format!("创建日志目录失败: {e}"))?;
    prune_expired_logs(&dir, LOG_RETENTION_DAYS);

    let (non_blocking, guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::daily(&dir, LOG_FILE_PREFIX));
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .with_writer(RedactingMaker(non_blocking))
        .finish();
    Ok((subscriber, guard))
}

/// 初始化应用日志：`<data_root>\logs\` 按天滚动、落盘前统一脱敏、过期文件清理
/// （docs/01 §9.3「写入前经脱敏过滤器，按天滚动，保留 14 天」）。
/// 返回的 WorkerGuard 必须在进程生命周期内存活——non_blocking 的后台线程随 guard 关闭。
pub fn init_logging(data_root: &Path) -> Result<WorkerGuard, String> {
    let (subscriber, guard) = build_logging(data_root)?;
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|e| format!("日志初始化失败: {e}"))?;
    Ok(guard)
}

/// 删除 mtime 早于保留期的日志文件。按 mtime 而不是文件名日期判据：滚动文件名的
/// 格式归 tracing-appender 管，mtime 与命名解耦，也省一个日期库依赖。
/// 单个文件删不掉就留到下次启动再试，不影响应用运行。
pub fn prune_expired_logs(dir: &Path, retain_days: u64) {
    let cutoff = SystemTime::now() - std::time::Duration::from_secs(retain_days * 24 * 3600);
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        if meta.modified().map(|m| m < cutoff).unwrap_or(false) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod secrets_tests {
    use super::*;

    fn store() -> (crate::test_support::TestTempDir, SecretStore) {
        let base = crate::test_support::TestTempDir::new("secrets");
        let store = SecretStore::new(&base);
        (base, store)
    }

    #[test]
    fn round_trip_store_load() {
        let (_dir, s) = store();
        s.store_key("openai-responses", "sk-test-123").unwrap();
        assert_eq!(s.load_key("openai-responses").unwrap().as_deref(), Some("sk-test-123"));
        // 落盘的是 DPAPI 密文而非明文
        let raw = std::fs::read(s.key_path("openai-responses").unwrap()).unwrap();
        assert!(!raw.windows(12).any(|w| w == b"sk-test-123"));
        s.delete_key("openai-responses").unwrap();
    }

    #[test]
    fn missing_key_is_none() {
        let (_dir, s) = store();
        assert_eq!(s.load_key("nonexistent").unwrap(), None);
    }

    #[test]
    fn delete_then_load_is_none() {
        let (_dir, s) = store();
        s.store_key("p1", "secret").unwrap();
        s.delete_key("p1").unwrap();
        assert_eq!(s.load_key("p1").unwrap(), None);
    }

    #[test]
    fn tampered_cipher_fails_closed() {
        let (_dir, s) = store();
        s.store_key("p2", "secret-value").unwrap();
        let path = s.key_path("p2").unwrap();
        let mut raw = std::fs::read(&path).unwrap();
        let mid = raw.len() / 2;
        raw[mid] = raw[mid].wrapping_add(0xFF);
        std::fs::write(&path, raw).unwrap();
        // 损坏密文必须 Err，绝不输出明文
        assert!(s.load_key("p2").is_err());
        s.delete_key("p2").unwrap();
    }

    #[test]
    fn provider_name_is_sanitized_against_path_injection() {
        let (_dir, s) = store();
        let path = s.key_path("../evil").unwrap();
        // 分隔符被清洗：不逃逸 secrets 目录
        assert!(path.file_name().unwrap() == "evil.key");
    }

    #[test]
    fn empty_key_rejected() {
        let (_dir, s) = store();
        assert!(s.store_key("p3", "   ").is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_var_takes_highest_priority() {
        let dirs = resolve_dirs(
            Some("D:/tmp/shaniu-test"),
            Some(Path::new("C:/portable")), // 即使有 marker 也忽略
            Some(Path::new("C:/appdata")),
            Some(Path::new("C:/localdata")),
        );
        // 注：无 marker 时才走默认；此处无 marker，env 优先
        assert_eq!(dirs.config_root, PathBuf::from("D:/tmp/shaniu-test"));
        assert_eq!(dirs.data_root, PathBuf::from("D:/tmp/shaniu-test"));
    }

    #[test]
    fn portable_marker_routes_to_exe_data_dir() {
        let base = crate::test_support::TestTempDir::new("test");
        let exe = base.join("app");
        fs::create_dir_all(&exe).unwrap();
        fs::write(exe.join(PORTABLE_MARKER), b"").unwrap();
        let dirs = resolve_dirs(None, Some(&exe), Some(Path::new("C:/appdata")), None);
        let expected = exe.join("data");
        assert_eq!(dirs.config_root, expected);
        assert_eq!(dirs.data_root, expected);
    }

    /// 首次判定：exe 旁边能建能写 → 用 `<exe>\data` 并落记录；写不进去 → 安装版默认。
    #[test]
    fn first_launch_prefers_writable_data_next_to_exe() {
        let base = crate::test_support::TestTempDir::new("root-decide-portable");
        let exe = base.join("app");
        fs::create_dir_all(&exe).unwrap();
        let portable_root = exe.join("data");

        let resolution = decide_root(
            None,
            Some(exe.as_path()),
            Some(base.join("appdata").as_path()),
            Some(base.join("localdata").as_path()),
            None,
            |_| true,
        );
        assert_eq!(resolution.kind, Some(DataRootKind::Portable));
        assert_eq!(resolution.dirs.data_root, portable_root);
        assert!(resolution.decided_now, "首次判定必须落记录，否则下次又得探测");
        assert!(resolution.warning.is_none());

        let installed = decide_root(
            None,
            Some(exe.as_path()),
            Some(base.join("appdata").as_path()),
            Some(base.join("localdata").as_path()),
            None,
            |dir| dir != portable_root.as_path(),
        );
        assert_eq!(installed.kind, Some(DataRootKind::Installed));
        assert_eq!(installed.dirs.data_root, base.join("localdata").join(APP_DIR_NAME));
        assert!(installed.decided_now);
    }

    /// "首次判定之后固定"的核心：记录存在时，**即使这次探测说不可写也不许换位置**。
    /// 反向对照：改成每次探测判定 → 这里会得到安装版路径 → 断言红。
    #[test]
    fn recorded_root_wins_even_when_probe_now_fails() {
        let base = crate::test_support::TestTempDir::new("root-decide-fixed");
        let exe = base.join("app");
        fs::create_dir_all(&exe).unwrap();
        let portable_root = exe.join("data");
        let record = DataRootRecord {
            version: DATA_ROOT_RECORD_VERSION,
            kind: DataRootKind::Portable,
            path: portable_root.display().to_string(),
            exe_dir: exe.display().to_string(),
        };

        let resolution = decide_root(
            None,
            Some(exe.as_path()),
            Some(base.join("appdata").as_path()),
            Some(base.join("localdata").as_path()),
            Some(&record),
            |_| false,
        );
        assert_eq!(
            resolution.dirs.data_root, portable_root,
            "数据根不许因为一次探测失败就漂移（那就是数据分裂）"
        );
        assert!(!resolution.decided_now, "已有记录时不该再判定/写记录");
        let warning = resolution.warning.expect("记录与实际不符必须明确提示");
        assert!(warning.contains("写不进去"), "提示要说清原因：{warning}");
        assert!(
            warning.contains("要改回便携") && warning.contains("删除它后重启"),
            "提示要说清怎么改回去：{warning}"
        );
    }

    /// 旧便携目录兼容：`portable.marker` 必须继续被识别，哪怕目录这次不可写。
    /// 这里刻意**只放 marker、不预建 `data\`**：两种信号同时存在时无法证明 marker
    /// 真的被读到（老目录通常两者都有，但那样用例就没有分辨力）。
    /// 反向对照：去掉 marker 兼容 → 会回落安装版 → 断言红。
    #[test]
    fn legacy_portable_marker_still_wins_over_writability_probe() {
        let base = crate::test_support::TestTempDir::new("root-decide-marker");
        let exe = base.join("app");
        fs::create_dir_all(&exe).unwrap();
        fs::write(exe.join(PORTABLE_MARKER), b"").unwrap();

        let resolution = decide_root(
            None,
            Some(exe.as_path()),
            Some(base.join("appdata").as_path()),
            Some(base.join("localdata").as_path()),
            None,
            |_| false,
        );
        assert_eq!(resolution.kind, Some(DataRootKind::Portable));
        assert_eq!(resolution.dirs.data_root, exe.join("data"));
        assert!(resolution.warning.is_some(), "不可写要提示，但不能换位置");
    }

    /// 没有 marker 但 `<exe>\data` 已经在（老版本 / 手工放的数据）：同样按便携。
    #[test]
    fn existing_data_dir_next_to_exe_is_kept() {
        let base = crate::test_support::TestTempDir::new("root-decide-existing");
        let exe = base.join("app");
        fs::create_dir_all(exe.join("data").join(ROLLOUTS_DIR)).unwrap();

        let resolution = decide_root(
            None,
            Some(exe.as_path()),
            Some(base.join("appdata").as_path()),
            Some(base.join("localdata").as_path()),
            None,
            |_| false,
        );
        assert_eq!(resolution.dirs.data_root, exe.join("data"));
        assert_eq!(resolution.kind, Some(DataRootKind::Portable));
    }

    /// ACL 禁写不能只报错：必须给一条用户可以走的路（改用安装模式），
    /// 否则只读介质/权限变化就把用户卡在"只能退出"。
    #[test]
    fn unwritable_portable_offers_installed_exit() {
        let base = crate::test_support::TestTempDir::new("root-exit-offer");
        let exe = base.join("app");
        fs::create_dir_all(&exe).unwrap();
        let portable_root = exe.join("data");
        fs::create_dir_all(&portable_root).unwrap();
        fs::write(portable_root.join("keep.txt"), b"old").unwrap();
        let record = DataRootRecord {
            version: DATA_ROOT_RECORD_VERSION,
            kind: DataRootKind::Portable,
            path: portable_root.display().to_string(),
            exe_dir: exe.display().to_string(),
        };

        let resolution = decide_root(
            None,
            Some(exe.as_path()),
            Some(base.join("appdata").as_path()),
            Some(base.join("localdata").as_path()),
            Some(&record),
            |_| false,
        );
        let installed = resolution
            .fallback_to_installed
            .as_ref()
            .expect("不可写必须给出口，不能只有一句报错");
        assert_eq!(
            installed.data_root,
            base.join("localdata").join(APP_DIR_NAME),
            "出口就是安装模式的数据根"
        );
        let warning = resolution.warning.expect("出口提示必须存在");
        for needle in [
            "改用安装模式",
            "不会被删除或移动",
            "选择「否」：退出",
            &portable_root.display().to_string(),
        ] {
            assert!(warning.contains(needle), "出口提示缺少「{needle}」：{warning}");
        }
        // 只是"给出出口"：未经确认，数据根与记录都不许动。
        assert_eq!(resolution.dirs.data_root, portable_root);
        assert!(resolution.switched_from.is_none());
        assert!(!resolution.decided_now);
    }

    /// 出口必须由用户点确认才生效；未确认时既不切换也不落记录。
    /// 反向对照：把 `switch_to_installed_if_confirmed` 改成"直接切换"，
    /// 「否」那一段的 `aborted` / 数据根 / 记录文件三条断言会同时变红。
    #[test]
    fn installed_switch_needs_explicit_confirmation() {
        let base = crate::test_support::TestTempDir::new("root-exit-confirm");
        let exe = base.join("app");
        fs::create_dir_all(&exe).unwrap();
        let portable_root = exe.join("data");
        fs::create_dir_all(&portable_root).unwrap();
        fs::write(portable_root.join("keep.txt"), b"old").unwrap();
        let record = DataRootRecord {
            version: DATA_ROOT_RECORD_VERSION,
            kind: DataRootKind::Portable,
            path: portable_root.display().to_string(),
            exe_dir: exe.display().to_string(),
        };
        let make = || {
            decide_root(
                None,
                Some(exe.as_path()),
                Some(base.join("appdata").as_path()),
                Some(base.join("localdata").as_path()),
                Some(&record),
                |_| false,
            )
        };

        // ① 用户选「否」（含直接关窗）：不切换、不落记录，交给启动层退出。
        let mut declined = make();
        switch_to_installed_if_confirmed(&mut declined, &|_| false);
        assert!(declined.aborted, "用户拒绝后必须中止，不许继续用不可写目录");
        assert_eq!(declined.dirs.data_root, portable_root, "未确认就不许换位置");
        assert!(declined.switched_from.is_none());
        assert!(
            !declined.record_path.exists(),
            "未确认不许写「已改用安装模式」记录，否则下次启动再也不会问"
        );

        // ② 用户选「是」：切到安装模式，原目录原样保留（不删、不搬）。
        let mut accepted = make();
        switch_to_installed_if_confirmed(&mut accepted, &|_| true);
        assert!(!accepted.aborted);
        assert_eq!(
            accepted.dirs.data_root,
            base.join("localdata").join(APP_DIR_NAME)
        );
        assert_eq!(accepted.kind, Some(DataRootKind::Installed));
        assert_eq!(
            accepted.switched_from.as_deref(),
            Some(portable_root.as_path()),
            "切换后要留下「从哪来」的痕迹供日志与提示"
        );
        let written = fs::read_to_string(&accepted.record_path).expect("切换后必须落记录");
        assert!(written.contains("\"installed\""), "记录要写明新形态：{written}");
        assert!(
            portable_root.join("keep.txt").is_file(),
            "原数据必须原样保留，不许删除或移动"
        );
    }

    /// 记录往返 + 损坏识别：坏记录必须能被识别出来（走"提示后重新判定"，
    /// 而不是当成"没有记录"悄悄判定）。
    #[test]
    fn record_round_trips_and_corruption_is_detected() {
        let base = crate::test_support::TestTempDir::new("root-record");
        let path = base.join(DATA_ROOT_RECORD_DIR).join("k.json");
        let record = DataRootRecord {
            version: DATA_ROOT_RECORD_VERSION,
            kind: DataRootKind::Installed,
            path: base.join("localdata").display().to_string(),
            exe_dir: base.join("app").display().to_string(),
        };
        write_record(&path, &record).unwrap();
        match read_record(&path) {
            RecordRead::Valid(back) => assert_eq!(back, record),
            _ => panic!("记录必须能读回"),
        }

        fs::write(&path, b"not-json").unwrap();
        assert!(matches!(read_record(&path), RecordRead::Corrupt(_)));
        assert!(matches!(
            read_record(&base.join("missing.json")),
            RecordRead::Missing
        ));
    }

    /// 环境变量覆盖：不判定、不落记录（测试/CI/多实例隔离都靠它）。
    #[test]
    fn env_override_skips_decision() {
        let base = crate::test_support::TestTempDir::new("root-decide-env");
        let resolution = decide_root(
            Some("D:/tmp/shaniu-env"),
            Some(base.join("app").as_path()),
            Some(base.join("appdata").as_path()),
            Some(base.join("localdata").as_path()),
            None,
            |_| true,
        );
        assert_eq!(resolution.kind, None);
        assert!(!resolution.decided_now, "环境变量覆盖不该写记录");
        assert_eq!(resolution.dirs.data_root, PathBuf::from("D:/tmp/shaniu-env"));
    }

    /// 记录按 exe 目录分文件：两份便携副本各自判定，互不覆盖。
    #[test]
    fn record_path_is_per_exe_directory() {
        let base = crate::test_support::TestTempDir::new("root-record-path");
        let first = base.join("copy-a");
        let second = base.join("copy-b");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        let a = data_root_record_path(Some(first.as_path()), None, Some(base.path()));
        let b = data_root_record_path(Some(second.as_path()), None, Some(base.path()));
        assert_ne!(a, b, "两份副本不能共用一份记录，否则会被指向同一个数据根");
    }

    #[test]
    fn installed_mode_uses_appdata_and_localappdata() {
        let dirs = resolve_dirs(
            None,
            Some(Path::new("C:/Program Files/SHANIU")),
            Some(Path::new("C:/appdata")),
            Some(Path::new("C:/localdata")),
        );
        assert_eq!(dirs.config_root, PathBuf::from("C:/appdata/SHANIU"));
        assert_eq!(dirs.data_root, PathBuf::from("C:/localdata/SHANIU"));
    }

    #[test]
    fn product_dirs_are_created_for_installed_and_portable() {
        let base = crate::test_support::TestTempDir::new("layout");
        // 安装版：配置根与数据根分离（%APPDATA% / %LOCALAPPDATA%）
        let installed = DataDirs {
            config_root: base.join("appdata"),
            data_root: base.join("localdata"),
        };
        ensure_product_dirs(&installed).unwrap();
        for name in [SECRETS_DIR, SETTINGS_BACKUP_DIR] {
            assert!(installed.config_root.join(name).is_dir(), "缺少配置侧目录 {name}");
        }
        for name in [SESSIONS_DIR, ROLLOUTS_DIR, LOGS_DIR, CACHE_DIR, CRASH_DIR] {
            assert!(installed.data_root.join(name).is_dir(), "缺少数据侧目录 {name}");
        }
        // 默认工作区**不再**随目录结构无条件创建：配置了别的根时它不会被使用，
        // 留着空目录只会被误认成"正在用的工作区"。由启动层在确实以默认根工作时创建。
        assert!(
            !installed.data_root.join(WORKSPACE_DIR).exists(),
            "ensure_product_dirs 不该创建默认工作区"
        );
        // 默认工作区落在数据根下的 Workspace
        assert_eq!(installed.workspace_root(), installed.data_root.join(WORKSPACE_DIR));

        // 便携版：同一根（<exe>\data）；重复调用幂等
        let portable = DataDirs {
            config_root: base.join("data"),
            data_root: base.join("data"),
        };
        ensure_product_dirs(&portable).unwrap();
        ensure_product_dirs(&portable).unwrap();
        assert!(!portable.workspace_root().exists(), "默认工作区同样不在这一步创建");
        assert!(portable.config_root.join(SECRETS_DIR).is_dir());
    }

    #[test]
    fn unwritable_root_reports_error_instead_of_silently_skipping() {
        let base = crate::test_support::TestTempDir::new("layout-bad");
        // 数据根是一条文件路径：建子目录必然失败，必须如实报错而不是当作已建好
        let blocked = base.join("blocked");
        fs::write(&blocked, b"file").unwrap();
        let dirs = DataDirs { config_root: blocked.clone(), data_root: blocked };
        assert!(ensure_product_dirs(&dirs).is_err());
    }

    #[test]
    fn product_dirs_view_is_camel_case_for_the_frontend() {
        let dirs = DataDirs {
            config_root: PathBuf::from("C:/cfg"),
            data_root: PathBuf::from("C:/data"),
        };
        let json = serde_json::to_value(dirs.view()).unwrap();
        // 前端按 camelCase 读，工作区路径 = 数据根下的 Workspace
        assert_eq!(json["workspaceRoot"], dirs.workspace_root().display().to_string());
        assert!(json.get("workspace_root").is_none());
    }

    #[test]
    fn config_round_trips_and_defaults_fill_missing_fields() {
        let base = crate::test_support::TestTempDir::new("cfg");
        let root = base.join("cfg");
        let (cfg, path) = AppConfig::load_or_create(&root).unwrap();
        assert_eq!(cfg.sandbox.mode, SandboxMode::ReadOnly, "默认档必须与 DSH 一致（只读）");
        assert_eq!(cfg.approval.mode, ApprovalMode::OnRequest);
        assert_eq!(cfg.model.max_turns, 0, "轮数默认不限（0 = 不限）");

        // 缺字段 → serde default 兜底（前向兼容旧配置文件）
        let partial = "[model]\nprovider = \"openai-chat\"\n";
        fs::write(&path, partial).unwrap();
        let (cfg2, _) = AppConfig::load_or_create(&root).unwrap();
        assert_eq!(cfg2.model.provider, "openai-chat");
        assert_eq!(cfg2.model.max_turns, 0, "旧配置缺字段时也必须落到「不限」");
        assert_eq!(cfg2.sandbox.mode, SandboxMode::ReadOnly);
        assert_eq!(
            cfg2.sandbox.os,
            OsSandbox::RestrictedToken,
            "OS 沙箱默认必须是只约束写的受限令牌（对齐 codex/dsh）"
        );
    }

    #[test]
    fn sandbox_os_must_be_explicit_to_fall_back_to_logical() {
        // 默认值不能是 logical：回退必须由用户写进配置，不能自动发生。
        let default: SandboxConfig = toml::from_str("").unwrap();
        assert_eq!(default.os, OsSandbox::RestrictedToken);
        assert!(!default.os_explicit, "字段缺失 = 旧文件，由迁移逻辑处理");
        let logical: SandboxConfig = toml::from_str("os = \"logical\"").unwrap();
        assert_eq!(logical.os, OsSandbox::Logical);
        assert!(
            toml::from_str::<SandboxConfig>("os = \"auto\"").is_err(),
            "未知 OS 沙箱取值必须解析失败，不能悄悄落到默认"
        );
    }

    #[test]
    fn corrupt_config_fails_closed() {
        let base = crate::test_support::TestTempDir::new("bad");
        let root = base.join("cfg");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(CONFIG_FILE), "???not toml???[").unwrap();
        assert!(AppConfig::load_or_create(&root).is_err());
    }

    #[test]
    fn session_ids_are_unique_and_prefixed() {
        let a = new_session_id(1);
        let b = new_session_id(2);
        assert!(a.starts_with("sess-") && b.starts_with("sess-"));
        assert_ne!(a, b);
    }

    /// 家目录的取值顺序必须固定：`%USERPROFILE%` 优先，其次 `%HOME%`，都没有才回落。
    /// 空串算"没设"——把空值当目录用会指向当前工作目录，界面缩写路径时就错了根。
    #[test]
    fn home_dir_prefers_profile_then_home_then_fallback() {
        use std::ffi::OsStr;
        let temp = Path::new("T:/tmp");
        assert_eq!(
            home_dir_from(Some(OsStr::new("C:/Users/a")), Some(OsStr::new("/home/a")), temp),
            PathBuf::from("C:/Users/a")
        );
        assert_eq!(
            home_dir_from(Some(OsStr::new("")), Some(OsStr::new("/home/a")), temp),
            PathBuf::from("/home/a"),
            "设成空串的 USERPROFILE 不能当目录用"
        );
        assert_eq!(home_dir_from(None, None, temp), PathBuf::from("T:/tmp"));
        // 真实环境下不能给出空目录：界面拿它缩写显示路径
        assert!(!home_dir().as_os_str().is_empty());
        assert_eq!(
            resolve_dirs(None, None, Some(Path::new("C:/appdata")), Some(Path::new("L:/local")))
                .view()
                .home_dir,
            home_dir().display().to_string(),
            "界面读的家目录必须与运行时的默认工作区回落同源"
        );
    }
}

#[cfg(test)]
mod provider_tests {
    use super::*;

    fn profile(id: &str, base: &str, model: &str) -> ProviderProfile {
        ProviderProfile {
            id: id.into(),
            label: id.into(),
            provider: "openai-chat".into(),
            base_url: base.into(),
            model: model.into(),
            reasoning_effort: String::new(),
            headers: std::collections::BTreeMap::new(),
            secret_ref: String::new(),
            models: Vec::new(),
        }
    }

    fn row(id: &str) -> ModelProfile {
        ModelProfile {
            id: id.into(),
            name: id.into(),
            context_window: 128_000,
            max_tokens: 8_192,
            input: vec!["text".into()],
            reasoning_efforts: std::collections::BTreeMap::new(),
            capabilities: std::collections::BTreeMap::new(),
        }
    }

    /// 旧 config.toml（没有 `[[providers]]`）必须与"不启用列表"逐字等价：
    /// 升级不该让已经配好的端点和已经存下的密钥突然失效。
    #[test]
    fn config_without_providers_falls_back_to_model_section() {
        let raw = "[model]\nprovider = \"openai-chat\"\nbase_url = \"https://a/v1\"\nmodel = \"m1\"\nmax_turns = 7\n";
        let cfg: AppConfig = toml::from_str(raw).expect("旧格式配置必须能解析");
        assert!(cfg.providers.is_empty());
        let active = cfg.effective_model();
        assert_eq!((active.provider.as_str(), active.base_url.as_str(), active.model.as_str()), ("openai-chat", "https://a/v1", "m1"));
        assert_eq!(active.max_turns, 7);
        // 回落路径的密钥名沿用 [model].provider——这正是改动前 build_agent 读的那个文件
        assert_eq!(active.secret_name, "openai-chat");
        // 缺 `[session_title]` 段 = 开箱默认开（上游 base bundle 即默认挂载），不是关
        assert!(cfg.session_title.enabled);
        assert_eq!(cfg.session_title.target_words, 5);
        assert_eq!(cfg.session_title.max_input_bytes, 4096);
    }

    #[test]
    fn active_profile_wins_over_model_section() {
        let mut cfg = AppConfig::default();
        cfg.model.base_url = "https://stale/v1".into();
        cfg.model.model = "stale".into();
        cfg.providers = vec![profile("longcat", "https://lc/v1", "LongCat-2.5"), profile("deepseek", "https://ds/v1", "deepseek-chat")];
        cfg.active_provider = "deepseek".into();
        let active = cfg.effective_model();
        assert_eq!((active.id.as_str(), active.base_url.as_str(), active.model.as_str()), ("deepseek", "https://ds/v1", "deepseek-chat"));
        assert_eq!(active.secret_name, "DEEPSEEK_API_KEY", "密钥名必须按生效档派生，否则切了服务商还在读同一份密钥");
    }

    #[test]
    fn unknown_active_provider_falls_back_instead_of_erroring_at_runtime() {
        // 手工编辑过的 config.toml 可能把 active 指向不存在的 id。运行期回落到 [model]
        // 而不是 panic：读配置失败会让整个应用起不来，而回落至少还能用旧端点。
        // 写入路径则由 validate 拦住（下一条测试）。
        let mut cfg = AppConfig::default();
        cfg.model.model = "fallback".into();
        cfg.providers = vec![profile("longcat", "https://lc/v1", "LongCat-2.5")];
        cfg.active_provider = "ghost".into();
        assert_eq!(cfg.effective_model().model, "fallback");
        assert!(AppConfig::validate_providers(&cfg.providers, &cfg.active_provider).is_err());
    }

    #[test]
    fn provider_document_validation_rejects_empty_duplicate_and_bad_secret_names() {
        assert!(AppConfig::validate_providers(&[profile("", "b", "m")], "").is_err());
        let dup = vec![profile("same", "b1", "m1"), profile("same", "b2", "m2")];
        let err = AppConfig::validate_providers(&dup, "").unwrap_err();
        assert!(err.contains("重复"), "{err}");

        // id 里的路径分隔符在派生阶段就被换成下划线，所以派生名本身是安全的：
        // 这种情况必须放行，否则正常用户改个带空格的 id 就写不进去。
        let derived_evil = vec![profile(r"..\secrets\evil", "b", "m")];
        assert!(AppConfig::validate_providers(&derived_evil, "").is_ok());
        assert_eq!(derived_evil[0].secret_name(), "___SECRETS_EVIL_API_KEY");

        // 显式给了一个会被 sanitize **静默改写**的名字才是真危险：设置页按原名报
        // "已配 Key"，后端按改写后的文件读，第一次运行就变成"配了却说没配"。
        let mut explicit = profile("longcat", "b", "m");
        explicit.secret_ref = r"..\evil".into();
        let err = AppConfig::validate_providers(&[explicit], "").unwrap_err();
        assert!(err.contains("非法字符"), "{err}");
        assert!(AppConfig::validate_providers(&[profile("longcat", "b", "m")], "longcat").is_ok());
    }

    #[test]
    fn secret_name_prefers_explicit_ref_and_derives_otherwise() {
        let mut p = profile("minimax-cn", "b", "m");
        assert_eq!(p.secret_name(), "MINIMAX_CN_API_KEY", "派生规则要与设置页的 deriveKeyRef 同形");
        p.secret_ref = "MY_KEY".into();
        assert_eq!(p.secret_name(), "MY_KEY");
        p.secret_ref = "  ".into();
        assert_eq!(p.secret_name(), "MINIMAX_CN_API_KEY", "全空白等于没给，不能存出一个空文件名");
    }

    #[test]
    fn provider_document_round_trips_through_toml() {
        let mut listed = profile("longcat", "https://lc/v1", "LongCat-2.5");
        listed.models = vec![row("LongCat-2.5"), row("LongCat-mini")];
        let cfg = AppConfig {
            providers: vec![listed],
            active_provider: "longcat".into(),
            provider_revision: 3,
            ..Default::default()
        };
        let dir = crate::test_support::TestTempDir::new("providers");
        let path = dir.join("providers.toml");
        cfg.save(&path).unwrap();
        let back: AppConfig = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back, cfg, "服务商文档必须原样往返：半途丢字段就等于用户配的 Key 名变了");
    }

    /// 设置页写的是 JSON，字段名却是 camelCase。serde 对结构体字段**不做**大小写转换，
    /// 认不到的键会被 `#[serde(default)]` 静默填成缺省值——写出去就是"界面显示 128K
    /// 上下文、文档里是 0"。这条测试钉住 alias 生效。
    #[test]
    fn camel_case_json_from_frontend_deserializes_into_the_same_document() {
        let parsed: Vec<ProviderProfile> = serde_json::from_value(serde_json::json!([
            {
                "id": "longcat",
                "label": "LongCat",
                "provider": "openai-chat",
                "baseUrl": "https://api.longcat.chat/openai/v1",
                "model": "LongCat-2.5",
                "secretRef": "LONGCAT_API_KEY",
                "models": [
                    { "id": "LongCat-2.5", "name": "LongCat 2.5", "contextWindow": 128_000, "maxTokens": 8_192, "input": ["text"] }
                ]
            }
        ]))
        .expect("设置页送来的 camelCase 文档必须解析成同一份结构");
        assert_eq!(parsed[0].base_url, "https://api.longcat.chat/openai/v1");
        assert_eq!(parsed[0].secret_ref, "LONGCAT_API_KEY");
        assert_eq!(parsed[0].models[0].context_window, 128_000);
        assert_eq!(parsed[0].models[0].input, vec!["text".to_string()]);
    }

    #[test]
    fn model_roster_rejects_blank_duplicate_and_unlisted_active_rows() {
        let mut blank = profile("longcat", "b", "m");
        blank.models = vec![row("")];
        let err = AppConfig::validate_providers(&[blank.clone()], "").unwrap_err();
        assert!(err.contains("缺 id"), "{err}");

        let mut dup = profile("longcat", "b", "m");
        dup.models = vec![row("a"), row("a")];
        let err = AppConfig::validate_providers(&[dup], "").unwrap_err();
        assert!(err.contains("模型 id 重复"), "{err}");

        // 生效行不在清单里：席位下拉里没有正在跑的模型，用户下一次点选就把档位换走了
        let mut unlisted = profile("longcat", "b", "ghost");
        unlisted.models = vec![row("a")];
        let err = AppConfig::validate_providers(&[unlisted], "").unwrap_err();
        assert!(err.contains("不在模型清单里"), "{err}");

        // 空清单仍按 model 单行跑：这是升级前文档的原样姿态
        let listed = profile("longcat", "b", "m");
        assert!(AppConfig::validate_providers(&[listed], "longcat").is_ok());
        let mut legacy = profile("longcat", "b", "m");
        legacy.models = vec![row("m")];
        assert!(AppConfig::validate_providers(&[legacy], "longcat").is_ok());
    }

    /// 空 `models` 必须**不产出** `[[providers.models]]` 这一节：写出一张空表会让
    /// 旧版后端读到多余的一节，而往返测试比对的是整份 AppConfig 的相等性。
    #[test]
    fn empty_roster_writes_no_models_section() {
        let cfg = AppConfig {
            providers: vec![profile("longcat", "https://lc/v1", "LongCat-2.5")],
            ..Default::default()
        };
        let body = toml::to_string_pretty(&cfg).unwrap();
        assert!(!body.contains("providers.models"), "{body}");
        assert_eq!(toml::from_str::<AppConfig>(&body).unwrap(), cfg);
    }

    // ---- 推理档（SH-096）----

    /// 串段守卫：真实事故是 `[[workspaces]]` 的 `path` / `title` 落进了
    /// `[providers.models.reasoning_efforts]`，用户的推理档声明被这两个键顶掉。
    /// 写盘前必须拦下、原文件一个字节不动，并留一份备份用于取证。
    #[test]
    fn stray_reasoning_keys_are_refused_and_original_is_backed_up() {
        let base = crate::test_support::TestTempDir::new("stray");
        let path = base.join(CONFIG_FILE);
        let mut listed = profile("longcat", "https://lc/v1", "LongCat-2.5");
        listed.models = vec![effort_row("LongCat-2.5")];
        let good = AppConfig {
            providers: vec![listed],
            active_provider: "longcat".into(),
            ..Default::default()
        };
        good.save(&path).unwrap();
        // 原子替换：成功路径不许留下临时文件
        assert!(
            !fs::read_dir(&base).unwrap().any(|e| e
                .map(|e| e.file_name().to_string_lossy().contains(".tmp-"))
                .unwrap_or(false)),
            "成功落盘后不应残留临时文件"
        );
        let before = fs::read_to_string(&path).unwrap();

        let mut polluted = good.clone();
        polluted.providers[0].models[0].reasoning_efforts = [
            ("path".to_string(), r"\\?\D:\tmp\ws2".to_string()),
            ("title".to_string(), "ws2".to_string()),
        ]
        .into_iter()
        .collect();
        let err = polluted.save(&path).unwrap_err();
        assert!(err.contains("path") && err.contains("title"), "报错要点名串进来的键: {err}");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            before,
            "拒绝落盘时原文件必须一个字节都不变"
        );

        let backups: Vec<String> = fs::read_dir(&base)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.contains(".bak-"))
            .collect();
        assert_eq!(backups.len(), 1, "拒绝落盘必须留一份原文件备份: {backups:?}");
        assert_eq!(fs::read_to_string(base.join(&backups[0])).unwrap(), before);
    }

    fn effort_row(id: &str) -> ModelProfile {
        let mut r = row(id);
        // off 的 wire 值是空串（不发字段）；其余档位给非空 wire 值
        r.reasoning_efforts = [
            ("off".to_string(), String::new()),
            ("low".to_string(), "low".to_string()),
            ("high".to_string(), "high".to_string()),
        ]
        .into_iter()
        .collect();
        r
    }

    // ---- 启动一次性修复（2026-11-08）----

    /// 盘上已经坏的配置必须能自愈一次：备份 → 剔除 → 写回；第二次启动不再动作。
    #[test]
    fn stray_keys_on_disk_are_repaired_once_and_backed_up() {
        let base = crate::test_support::TestTempDir::new("repair");
        let path = base.join(CONFIG_FILE);

        let mut listed = profile("longcat", "https://lc/v1", "LongCat-2.5");
        listed.models = vec![effort_row("LongCat-2.5")];
        let mut cfg = AppConfig {
            providers: vec![listed],
            active_provider: "longcat".into(),
            ..Default::default()
        };
        cfg.providers[0].models[0].reasoning_efforts.insert("path".into(), r"\\?\D:\tmp\ws2".into());
        cfg.providers[0].models[0].reasoning_efforts.insert("title".into(), "ws2".into());
        // 模拟"上一版写坏的盘上文件"：绕过 save 的自检，直接序列化落盘
        fs::write(&path, toml::to_string_pretty(&cfg).unwrap()).unwrap();

        let first = AppConfig::load_or_create_diagnosed(&base);
        let (backup, removed) = match &first.outcome {
            ConfigLoadOutcome::Repaired { backup, removed } => (backup.clone(), removed.clone()),
            other => panic!("坏配置必须被修复，实际: {other:?}"),
        };
        assert_eq!(removed.len(), 1, "一个模型行里的写串键聚合为一条: {removed:?}");
        assert!(
            removed[0].contains("path") && removed[0].contains("title"),
            "必须点名剔掉的键: {removed:?}"
        );
        assert!(backup.exists(), "修复前必须先备份原文件");
        assert!(
            fs::read_to_string(&backup).unwrap().contains("ws2"),
            "备份必须是修复前的原样"
        );

        let repaired = &first.config.providers[0].models[0].reasoning_efforts;
        assert_eq!(
            repaired.keys().cloned().collect::<Vec<_>>(),
            vec!["high".to_string(), "low".to_string(), "off".to_string()],
            "只保留合法档位键"
        );
        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains(r"\\?\D:\tmp\ws2") && !raw.contains("title = \"ws2\""), "写回后不许再带写串键: {raw}");

        // 幂等：第二次启动不再修复、不再产生新备份
        let second = AppConfig::load_or_create_diagnosed(&base);
        assert_eq!(second.outcome, ConfigLoadOutcome::Clean, "二次启动不应再触发修复");
        let backups = fs::read_dir(&base)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".bak-"))
            .count();
        assert_eq!(backups, 1, "二次启动不应新增备份文件");
    }

    /// 旧配置兼容（2026-11-10 删网络维度）：历史 config.toml 可能带 `[sandbox] network`
    /// 这类已废弃键。删能力不许把旧配置打废——加载必须成功（未知键忽略，不进只读），
    /// 且**加载不改写文件字节**（不许自动删改旧字段）。
    #[test]
    fn deprecated_sandbox_network_key_loads_and_file_bytes_stay_untouched() {
        let base = crate::test_support::TestTempDir::new("legacy-key");
        let path = base.join(CONFIG_FILE);
        let legacy = "[sandbox]\n\
mode = \"workspace_write\"\n\
os = \"app_container\"\n\
os_explicit = true\n\
network = \"restricted\"\n";
        fs::write(&path, legacy).unwrap();
        let before = fs::read(&path).unwrap();

        let loaded = AppConfig::load_or_create_diagnosed(&base);
        assert_eq!(
            loaded.outcome,
            ConfigLoadOutcome::Clean,
            "旧字段必须被忽略，而不是让整份配置进只读: {:?}",
            loaded.outcome
        );
        // 能力确实未生效：档位仍按文件里的 mode 解析，且没有任何网络开关被读出来
        assert_eq!(loaded.config.sandbox.mode, SandboxMode::WorkspaceWrite);
        assert_eq!(loaded.config.sandbox.os, OsSandbox::AppContainer);
        assert_eq!(fs::read(&path).unwrap(), before, "加载不得改写配置文件字节");
    }

    /// 旧版本的默认值也会被写进 config.toml：`app_container` 且没有 `os_explicit`
    /// 视为「旧默认」，一次性迁移到 `restricted_token`（先备份、只迁一次）。
    #[test]
    fn legacy_default_sandbox_os_is_migrated_once_with_backup() {
        let base = crate::test_support::TestTempDir::new("os-migrate");
        let path = base.join(CONFIG_FILE);
        fs::write(&path, "[sandbox]\nmode = \"workspace_write\"\nos = \"app_container\"\n").unwrap();

        let first = AppConfig::load_or_create_diagnosed(&base);
        assert_eq!(first.outcome, ConfigLoadOutcome::Clean);
        assert_eq!(first.config.sandbox.os, OsSandbox::RestrictedToken, "旧默认必须迁到写约束");
        assert!(first.config.sandbox.os_explicit, "迁移后要标记为已定过");
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("os = \"restricted_token\""), "迁移必须写回文件: {raw}");
        assert!(raw.contains("os_explicit = true"), "迁移必须落标记，避免反复迁移: {raw}");
        let backups = fs::read_dir(&base)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".bak-"))
            .count();
        assert_eq!(backups, 1, "迁移前必须先备份原文件");

        let second = AppConfig::load_or_create_diagnosed(&base);
        assert_eq!(second.config.sandbox.os, OsSandbox::RestrictedToken);
        let backups2 = fs::read_dir(&base)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".bak-"))
            .count();
        assert_eq!(backups2, 1, "迁移必须幂等：二次启动不再备份");
    }

    /// 用户显式留在 AppContainer：带 `os_explicit = true` 的文件不许被迁移。
    #[test]
    fn explicit_app_container_is_never_migrated() {
        let base = crate::test_support::TestTempDir::new("os-explicit");
        let path = base.join(CONFIG_FILE);
        let raw = "[sandbox]\nmode = \"workspace_write\"\nos = \"app_container\"\nos_explicit = true\n";
        fs::write(&path, raw).unwrap();

        let loaded = AppConfig::load_or_create_diagnosed(&base);
        assert_eq!(loaded.config.sandbox.os, OsSandbox::AppContainer);
        assert_eq!(fs::read_to_string(&path).unwrap(), raw, "显式选择不得被改写");
    }

    /// 修不了（解析就失败）必须进只读：原文件一个字节不动，并给出路径。
    #[test]
    fn unparseable_config_is_left_untouched_and_reported_read_only() {
        let base = crate::test_support::TestTempDir::new("readonly");
        let path = base.join(CONFIG_FILE);
        let broken = "[model]\nbase_url = 123\n";
        fs::write(&path, broken).unwrap();

        let loaded = AppConfig::load_or_create_diagnosed(&base);
        match &loaded.outcome {
            ConfigLoadOutcome::ReadOnly { reason } => {
                assert!(reason.contains(&path.display().to_string()), "只读原因必须带路径: {reason}");
                assert!(reason.contains("未被改动"), "必须说明文件未被改动: {reason}");
            }
            other => panic!("解析失败必须进只读，实际: {other:?}"),
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), broken, "不可修复时原文件必须保持不变");
        assert!(
            !fs::read_dir(&base)
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().contains(".bak-")),
            "没有修复动作就不该产生备份"
        );
    }

    #[test]
    fn reasoning_efforts_round_trip_through_toml_and_json() {
        let mut listed = profile("longcat", "https://lc/v1", "LongCat-2.5");
        listed.reasoning_effort = "high".into();
        listed.models = vec![effort_row("LongCat-2.5")];
        let cfg = AppConfig {
            providers: vec![listed.clone()],
            active_provider: "longcat".into(),
            ..Default::default()
        };
        // TOML 往返：档位表住在 [[providers.models]] 元素里，写出/读回必须无损
        let body = toml::to_string_pretty(&cfg).unwrap();
        assert_eq!(toml::from_str::<AppConfig>(&body).unwrap(), cfg, "TOML 往返丢档位表 = 用户配的推理档重载即失");
        // JSON 往返：serde 序列化用落盘名（snake_case）；camelCase 只在
        // get_settings 的手工拼装里出现，由 alias 保证反序列化两侧都认
        let json = serde_json::to_value(&cfg.providers).unwrap();
        assert_eq!(json[0]["reasoning_effort"], "high");
        assert_eq!(json[0]["models"][0]["reasoning_efforts"]["off"], "");
        let back: Vec<ProviderProfile> = serde_json::from_value(json).unwrap();
        assert_eq!(back, cfg.providers);
    }

    #[test]
    fn effective_model_resolves_effort_to_wire_value() {
        let mut listed = profile("longcat", "https://lc/v1", "LongCat-2.5");
        listed.models = vec![effort_row("LongCat-2.5")];
        let mut cfg = AppConfig {
            providers: vec![listed.clone()],
            active_provider: "longcat".into(),
            ..Default::default()
        };
        // 选中 high → wire 值原样下发
        listed.reasoning_effort = "high".into();
        cfg.providers = vec![listed.clone()];
        assert_eq!(cfg.effective_model().reasoning_effort.as_deref(), Some("high"));

        // 选中 off → 不发字段
        listed.reasoning_effort = "off".into();
        cfg.providers = vec![listed.clone()];
        assert_eq!(cfg.effective_model().reasoning_effort, None);

        // 选中档位不在模型声明里（防御；validate 会拦）→ 不发而不是猜
        listed.reasoning_effort = "xhigh".into();
        cfg.providers = vec![listed.clone()];
        assert_eq!(cfg.effective_model().reasoning_effort, None);

        // 未选（空）→ 初始默认回落声明的 high（用户拍板 2026-11-07）
        listed.reasoning_effort = String::new();
        cfg.providers = vec![listed.clone()];
        assert_eq!(cfg.effective_model().reasoning_effort.as_deref(), Some("high"));

        // 声明里没有 high → 初始默认无从谈起，维持不发
        let mut partial = profile("longcat", "https://lc/v1", "LongCat-2.5");
        partial.models = vec![{
            let mut r = row("LongCat-2.5");
            r.reasoning_efforts = [("low".to_string(), "low".to_string())].into_iter().collect();
            r
        }];
        cfg.providers = vec![partial];
        assert_eq!(cfg.effective_model().reasoning_effort, None);
    }

    /// 设置页的「生效档位」与请求侧必须是同一个判据：本行没声明时由内置表补出
    /// 实际会被 `reasoning_wire` 接受的档；内置表也没有就不给，不猜。
    #[test]
    fn declared_levels_fall_back_to_builtin_and_stay_empty_when_unknown() {
        let base = "https://api.deepseek.com";
        let flash = row("deepseek-flash");
        assert_eq!(
            flash.declared_reasoning_levels(base),
            ["low", "high", "max"],
            "内置表声明了 low/high/max，设置页必须与请求侧看到同一组档"
        );
        // 两侧同源：界面列出的每一档，请求侧都必须查得到 wire 值
        for level in flash.declared_reasoning_levels(base) {
            assert!(
                flash.reasoning_wire(base, &level).is_some(),
                "界面给出的档 {level} 在请求侧没有 wire 值 = 两侧漂移"
            );
        }
        assert_eq!(flash.reasoning_wire(base, "high").as_deref(), Some("high"));
        // LongCat 端点实测静默忽略 reasoning_effort（thinking 才是真开关），
        // 内置表不声明档位 → 空清单，设置页就不给档位选择。
        let longcat_base = "https://api.longcat.chat/openai/v1";
        assert!(
            row("LongCat-2.5").declared_reasoning_levels(longcat_base).is_empty(),
            "内置表没声明的模型不得凭空出档位"
        );
        // 本行声明优先于内置表（同 id 换端点能力可能不同）
        let declared = effort_row("deepseek-flash");
        assert_eq!(declared.declared_reasoning_levels(base), ["low", "high"]);
        // 端点明说没这档 → 从清单里剔掉（selected 与 wire 不能错位）
        let mut constrained = row("deepseek-flash");
        constrained.capabilities.insert(
            "reasoning".into(),
            CapabilityRecord { endpoint_levels: vec!["low".into()], ..Default::default() },
        );
        assert_eq!(constrained.declared_reasoning_levels(base), ["low"]);
    }

    #[test]
    fn validate_providers_rejects_bad_reasoning_declarations() {
        // 未知档位名：席位会列出后端查不到 wire 值的档
        let mut unknown = profile("longcat", "b", "m1");
        let mut r = row("m1");
        r.reasoning_efforts = [("ultra".to_string(), "ultra".to_string())].into_iter().collect();
        unknown.models = vec![r];
        let err = AppConfig::validate_providers(&[unknown], "").unwrap_err();
        assert!(err.contains("未知推理档位"), "{err}");

        // 非 off 档缺 wire 值：等于让模型"选了 low 却什么都不发"
        let mut blank = profile("longcat", "b", "m1");
        let mut r = row("m1");
        r.reasoning_efforts = [("low".to_string(), String::new())].into_iter().collect();
        blank.models = vec![r];
        let err = AppConfig::validate_providers(&[blank], "").unwrap_err();
        assert!(err.contains("缺 wire 值"), "{err}");

        // 只声明 off：席位列出一个"永远不发字段"的档位，没有意义
        let mut all_off = profile("longcat", "b", "m1");
        let mut r = row("m1");
        r.reasoning_efforts = [("off".to_string(), String::new())].into_iter().collect();
        all_off.models = vec![r];
        let err = AppConfig::validate_providers(&[all_off], "").unwrap_err();
        assert!(err.contains("只声明了 off"), "{err}");

        // 选中档位不在生效模型的声明里
        let mut undeclared = profile("longcat", "b", "m1");
        undeclared.reasoning_effort = "high".into();
        undeclared.models = vec![row("m1")];
        let err = AppConfig::validate_providers(&[undeclared], "").unwrap_err();
        assert!(err.contains("不在生效模型"), "{err}");

        // 合法声明 + 命中的选中档位放行
        let mut ok = profile("longcat", "b", "m1");
        ok.reasoning_effort = "low".into();
        ok.models = vec![effort_row("m1")];
        assert!(AppConfig::validate_providers(&[ok], "").is_ok());
    }

    // ---- 自定义请求头（SH-097）----

    #[test]
    fn headers_round_trip_through_toml_and_reach_resolved_model() {
        let mut listed = profile("longcat", "https://lc/v1", "m1");
        listed.headers = [("X-Gateway-Sign".to_string(), "abc".to_string())].into_iter().collect();
        let cfg = AppConfig {
            providers: vec![listed.clone()],
            active_provider: "longcat".into(),
            ..Default::default()
        };
        let body = toml::to_string_pretty(&cfg).unwrap();
        assert_eq!(toml::from_str::<AppConfig>(&body).unwrap(), cfg, "头表往返无损");
        let resolved = cfg.effective_model();
        assert_eq!(resolved.headers.get("X-Gateway-Sign").map(String::as_str), Some("abc"));
    }

    #[test]
    fn validate_providers_rejects_bad_header_names_and_injection() {
        // 名字带空格：不是合法 HTTP 头名，reqwest 在发送期才会炸，写入期就该拦
        let mut space = profile("longcat", "b", "m1");
        space.headers = [("bad name".to_string(), "v".to_string())].into_iter().collect();
        let err = AppConfig::validate_providers(&[space], "").unwrap_err();
        assert!(err.contains("不合法"), "{err}");

        // 值带换行 = 头注入防线，绝对不允许
        let mut injection = profile("longcat", "b", "m1");
        injection.headers = [("X-A".to_string(), "v\r\nX-Evil: 1".to_string())].into_iter().collect();
        let err = AppConfig::validate_providers(&[injection], "").unwrap_err();
        assert!(err.contains("头注入"), "{err}");

        // 合法头放行（空值也合法）
        let mut ok = profile("longcat", "b", "m1");
        ok.headers = [("X-Gateway-Sign".to_string(), String::new())].into_iter().collect();
        assert!(AppConfig::validate_providers(&[ok], "").is_ok());
    }

    #[test]
    fn custom_header_values_are_registered_as_secrets() {
        // 审计 M8：头名不可预测（X-Gateway-Sign 这类），只按名字匹配必然漏，
        // 所以全部值都进登记表，走与 API Key 同一条 redact 路径。
        let value = "opaque-gateway-signature-m8";
        let mut p = profile("longcat", "b", "m1");
        p.headers = [("X-Gateway-Sign".to_string(), value.to_string())].into_iter().collect();
        assert!(redact(value) == value, "构造前提：登记前这串不是已知密钥");

        // 保存/装载入口都会登记（反向对照：把 validate_providers 里的
        // register_header_secrets 调用去掉，本断言立刻红）
        AppConfig::validate_providers(std::slice::from_ref(&p), "longcat").unwrap();
        assert_eq!(redact(&format!("hdr={value}")), "hdr=[REDACTED]");

        // 短值不登记：把 `X-Debug: 1` 的 "1" 登记进去会把日志里每个 1 都抹掉
        let mut short = profile("longcat", "b", "m1");
        short.headers = [("X-Debug".to_string(), "1".to_string())].into_iter().collect();
        register_header_secrets(&[short]);
        assert_eq!(redact("turn=1 ok"), "turn=1 ok", "短值不得进入整串替换");

        // 装载路径：config.toml 读进内存时同样先登记，早于任何一次日志
        let from_disk = "opaque-from-disk-m8-xyz";
        let mut loaded = profile("longcat", "b", "m1");
        loaded.headers =
            [("X-From-Disk".to_string(), from_disk.to_string())].into_iter().collect();
        let cfg = AppConfig { providers: vec![loaded], ..AppConfig::default() };
        cfg.validate_loaded().expect("装载校验");
        assert_eq!(redact(from_disk), REDACTED);
    }

    #[test]
    fn custom_header_still_reaches_the_request() {
        // 别把功能改哑了：脱敏只影响落盘/日志，不影响实际发出去的请求头
        let mut p = profile("longcat", "b", "m1");
        p.headers = [("X-Gateway-Sign".to_string(), "sig-0123456789".to_string())]
            .into_iter()
            .collect();
        let cfg = AppConfig {
            providers: vec![p],
            active_provider: "longcat".into(),
            ..AppConfig::default()
        };
        AppConfig::validate_providers(&cfg.providers, "longcat").unwrap();
        let resolved = cfg.effective_model();
        assert_eq!(
            resolved.headers.get("X-Gateway-Sign").map(String::as_str),
            Some("sig-0123456789"),
            "登记为 secret 不能把头发送值改掉"
        );
    }

    #[test]
    fn crash_report_is_redacted_and_unique() {
        // 先登记（内部已过测试锁），再持锁跑完整条写报告流程：崩溃路径 `try_read`
        // 撞上写锁会退化成纯模式脱敏（产品行为），不把这段时间隔出来就会偶发红。
        register_secret("opaque-crash-secret-abc");
        let _secret_lock = secret_registry_test_lock();
        let dir = crate::test_support::TestTempDir::new("crash");
        let first = write_crash_report(
            &dir,
            "boom sk-abcdefgh12345678 opaque-crash-secret-abc",
            "src-tauri/src/lib.rs:42:7",
            "worker",
        )
        .expect("写崩溃报告");
        let text = fs::read_to_string(&first).expect("读崩溃报告");
        assert!(text.contains("location=src-tauri/src/lib.rs:42:7"));
        assert!(text.contains("thread=worker"));
        assert!(text.contains(REDACTED));
        assert!(!text.contains("sk-abcdefgh12345678"), "模式脱敏必须生效");
        assert!(!text.contains("opaque-crash-secret-abc"), "登记密钥必须整串替换");

        let second = write_crash_report(&dir, "second", "loc", "main").expect("第二份");
        assert_ne!(first, second, "同一毫秒的两次 panic 也不能互相覆盖");
    }
}

// ============================== MCP 配置（SH-060） ==============================

#[cfg(test)]
mod mcp_config_tests {
    use super::*;
    use std::fs;

    fn server(id: &str) -> McpServerConfig {
        McpServerConfig { id: id.into(), command: "node".into(), ..Default::default() }
    }

    fn temp_root(tag: &str) -> crate::test_support::TestTempDir {
        crate::test_support::TestTempDir::new(&format!("mcp-cfg-{tag}"))
    }

    #[test]
    fn mcp_defaults_are_empty_enabled_and_sandboxed() {
        let cfg = AppConfig::default();
        assert!(cfg.mcp.servers.is_empty(), "不配 MCP = 旧行为");
        let d = McpServerConfig::default();
        assert!(d.enabled, "写了配置即启用");
        assert!(!d.unsandboxed, "X10：默认必须走沙箱");
        assert!(d.allow_network, "方案 3：网络默认允许（显式 false 才关）");
        assert!(d.unsandboxed_reason.is_empty());
        assert!(d.allowed_roots.is_empty(), "Q2：默认没有任何目录授权");
        assert_eq!(d.workspace_access, WorkspaceAccess::Read, "Q3：工作区默认只读");
    }

    /// 网络三态契约（方案 3，2026-11-10）：字段缺失 → 允许；显式 true → 允许；
    /// 显式 false → 拒绝。前两态与第三态成对断言——反转的只是「字段缺失」的默认值，
    /// 不许推翻用户显式写的 false。同时钉住旧配置（没有该字段）加载不改写文件字节。
    #[test]
    fn mcp_network_three_states_and_legacy_file_bytes_stay_untouched() {
        let base = crate::test_support::TestTempDir::new("mcp-net-3state");
        let path = base.join(CONFIG_FILE);
        let legacy = "[[mcp.servers]]\nid = \"srv\"\ncommand = \"node\"\n";
        fs::write(&path, legacy).unwrap();
        let before = fs::read(&path).unwrap();

        let loaded = AppConfig::load_or_create_diagnosed(&base);
        assert_eq!(loaded.outcome, ConfigLoadOutcome::Clean);
        assert_eq!(loaded.config.mcp.servers.len(), 1);
        assert!(
            loaded.config.mcp.servers[0].allow_network,
            "字段缺失必须默认允许（方案 3）"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "加载不得把默认值写回文件（旧字段/缺字段都原样保留）"
        );

        let explicit_true: AppConfig = toml::from_str(
            "[[mcp.servers]]\nid = \"srv\"\ncommand = \"node\"\nallow_network = true\n",
        )
        .unwrap();
        assert!(explicit_true.mcp.servers[0].allow_network, "显式 true = 允许");

        let explicit_false: AppConfig = toml::from_str(
            "[[mcp.servers]]\nid = \"srv\"\ncommand = \"node\"\nallow_network = false\n",
        )
        .unwrap();
        assert!(
            !explicit_false.mcp.servers[0].allow_network,
            "显式 false 必须仍然是关——这是「缺失 → 允许」的反向对照"
        );
    }

    #[test]
    fn allowed_root_validation_rejects_relative_parent_and_denied_paths() {
        assert!(validate_allowed_root("relative\\dir").is_err(), "相对路径必须拒绝");
        assert!(validate_allowed_root(r"C:\a\..\b").is_err(), "含 `..` 一律拒绝");

        let drive_root = format!("{}\\", std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into()));
        assert!(validate_allowed_root(&drive_root).is_err(), "驱动器根必须拒绝");

        if let Ok(system_root) = std::env::var("SystemRoot") {
            assert!(validate_allowed_root(&system_root).is_err(), "系统目录必须拒绝");
        }
        if let Ok(home) = std::env::var("USERPROFILE") {
            assert!(validate_allowed_root(&home).is_err(), "用户 profile 根必须拒绝");
        }
        assert!(
            validate_allowed_root(r"C:\Program Files").is_err(),
            "Program Files 必须拒绝"
        );

        let allowed = std::env::temp_dir();
        assert!(
            validate_allowed_root(&allowed.to_string_lossy()).is_ok(),
            "普通目录应允许：{}",
            allowed.display()
        );
    }

    #[test]
    fn mcp_section_parses_from_toml_and_round_trips() {
        let body = r#"
[[mcp.servers]]
id = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", 'D:\work']
env = { LOG_LEVEL = "error" }
cwd = 'D:\work'

[[mcp.servers]]
id = "legacy"
command = "node"
unsandboxed = true
unsandboxed_reason = "旧版 server 在受限令牌下无法启动"
"#;
        let cfg: AppConfig = toml::from_str(body).unwrap();
        AppConfig::validate_mcp(&cfg.mcp).expect("合法配置");
        assert_eq!(cfg.mcp.servers.len(), 2);
        let fs_srv = &cfg.mcp.servers[0];
        assert_eq!(fs_srv.args.len(), 3);
        assert_eq!(fs_srv.env.get("LOG_LEVEL").map(String::as_str), Some("error"));
        assert!(fs_srv.enabled, "缺省 enabled = true");
        assert!(!fs_srv.unsandboxed);
        assert!(cfg.mcp.servers[1].unsandboxed);
        // 往返无损：否则用户改一次别处的设置就会把 mcp 段写丢
        let round = toml::to_string_pretty(&cfg).unwrap();
        assert_eq!(toml::from_str::<AppConfig>(&round).unwrap(), cfg);
    }

    #[test]
    fn mcp_source_cannot_be_set_from_toml_and_defaults_to_user() {
        // 来源是安全判据，不能由配置自称：TOML 里写 source 也不会生效。
        let body = r#"
[[mcp.servers]]
id = "sneaky"
command = "node"
source = "workspace"
"#;
        let cfg: AppConfig = toml::from_str(body).unwrap();
        assert_eq!(
            cfg.mcp.servers[0].source,
            McpConfigSource::User,
            "用户不能通过配置文件自称来源"
        );
        // 序列化不回写：改别的设置不会把来源带出去，更不会把 User 洗成别的值。
        let round = toml::to_string_pretty(&cfg).unwrap();
        assert!(!round.contains("source"), "source 不得进 config.toml: {round}");
        // 只有加载代码能显式设置非 User 来源（将来工作区加载器的入口）。
        let mut workspace_sourced = server("ws");
        workspace_sourced.source = McpConfigSource::Workspace;
        assert_eq!(workspace_sourced.source, McpConfigSource::Workspace);
    }

    #[test]
    fn validate_mcp_rejects_illegal_ids_and_duplicates() {
        for bad in ["File", "file_system", "-fs", &"a".repeat(33)] {
            let err = AppConfig::validate_mcp(&McpConfig { servers: vec![server(bad)] })
                .unwrap_err();
            assert!(err.contains("id 非法"), "{bad} 应被拒绝: {err}");
        }
        // 合法边界：单字符 / 32 字符 / 数字开头 / 含连字符
        for ok in ["a", "0", "fs-1", "fs-", &"a".repeat(32)] {
            assert!(is_valid_mcp_server_id(ok), "{ok} 是合法 id");
        }
        // 重复 id = 同一命名空间两家进程，拒绝整份配置而不是后者覆盖前者
        let err = AppConfig::validate_mcp(&McpConfig {
            servers: vec![server("dup"), server("dup")],
        })
        .unwrap_err();
        assert!(err.contains("重复"), "{err}");
    }

    #[test]
    fn validate_mcp_rejects_missing_command_bad_cwd_and_bad_env() {
        let mut no_cmd = server("a");
        no_cmd.command = "  ".into();
        assert!(AppConfig::validate_mcp(&McpConfig { servers: vec![no_cmd] })
            .unwrap_err()
            .contains("缺 command"));

        let mut relative = server("a");
        relative.cwd = "relative/dir".into();
        assert!(AppConfig::validate_mcp(&McpConfig { servers: vec![relative] })
            .unwrap_err()
            .contains("不是绝对路径"));

        let mut bad_env = server("a");
        bad_env.env.insert("BAD=KEY".into(), "v".into());
        assert!(AppConfig::validate_mcp(&McpConfig { servers: vec![bad_env] })
            .unwrap_err()
            .contains("环境变量名非法"));

        let mut nul_arg = server("a");
        nul_arg.args.push("x\0y".into());
        assert!(AppConfig::validate_mcp(&McpConfig { servers: vec![nul_arg] })
            .unwrap_err()
            .contains("NUL"));
    }

    #[test]
    fn unsandboxed_requires_a_recorded_reason() {
        let mut exempt = server("a");
        exempt.unsandboxed = true;
        let err = AppConfig::validate_mcp(&McpConfig { servers: vec![exempt.clone()] })
            .unwrap_err();
        assert!(err.contains("unsandboxed_reason"), "{err}");

        exempt.unsandboxed_reason = "旧版 server 需要交互式桌面".into();
        assert!(AppConfig::validate_mcp(&McpConfig { servers: vec![exempt] }).is_ok());
    }

    #[test]
    fn invalid_mcp_config_fails_closed_on_load() {
        let root = temp_root("load");
        let body = r#"
[[mcp.servers]]
id = "dup"
command = "node"

[[mcp.servers]]
id = "dup"
command = "node"
"#;
        fs::write(root.join(CONFIG_FILE), body).unwrap();
        let err = AppConfig::load_or_create(&root).unwrap_err();
        assert!(err.contains("id 重复"), "坏配置必须拒载而不是跳过后继续: {err}");
    }

    #[test]
    fn valid_mcp_config_survives_load() {
        let root = temp_root("ok");
        let body = r#"
[[mcp.servers]]
id = "fs"
command = "node"
args = ["server.js"]
"#;
        fs::write(root.join(CONFIG_FILE), body).unwrap();
        let (cfg, _) = AppConfig::load_or_create(&root).unwrap();
        assert_eq!(cfg.mcp.servers.len(), 1);
        assert_eq!(cfg.mcp.servers[0].id, "fs");
        assert!(cfg.mcp.servers[0].enabled);
    }
}
