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
use std::sync::{OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde::{Deserialize, Serialize};
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::fmt::MakeWriter;

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
    /// 会话标题自动生成（04 §4 `[session_title]`）。缺段 = 默认开启。
    pub session_title: SessionTitleConfig,
    /// 可选的服务商列表（04 §4 `[[providers]]`）。**为空就是旧行为**：生效档直接取
    /// `[model]` 段。这是 additive 扩展而不是替换——已有 config.toml 不改一个字也能跑，
    /// 而逼用户迁移配置文件等于让升级即失效。
    pub providers: Vec<ProviderProfile>,
    /// 当前生效的服务商 id；空或指不到任何一项时同样回落 `[model]`。
    pub active_provider: String,
    /// 服务商文档的修订号（设置页乐观锁的比对基准），每次写入 +1。
    pub provider_revision: u64,
}

/// 设置页里一行模型档位（04 §4 `[[providers.models]]`）。
///
/// 运行期只消费 `id`（`ProviderProfile::model` 就是它当前选中的那一行）；其余四个
/// 字段是设置页写面的原样留存。存下来但不生效，比"用户加了四行、重载只剩一行"
/// 更接近事实——后者的代价是用户以为自己配好了。
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
    /// 密钥存放名（DPAPI 文件名 / 设置页 ref）。空 = 按 id 派生。
    #[serde(alias = "secretRef")]
    pub secret_ref: String,
    /// 这一家的模型档位清单（设置页写面）。空 = 只用 `model` 那一行，旧文档不改一字也能跑。
    /// 必须排在最后：TOML 要求标量先于子表，把 `[[providers.models]]` 放前面会让整个
    /// `[[providers]]` 的缩进归属变成 models，写出去的配置直接解析不回来。
    pub models: Vec<ModelProfile>,
}

impl Default for ProviderProfile {
    fn default() -> Self {
        Self {
            id: String::new(),
            label: String::new(),
            provider: "openai-chat".into(),
            base_url: String::new(),
            model: String::new(),
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
}

impl AppConfig {
    /// 生效端点：`active_provider` 命中列表就用那家，否则回落 `[model]`。
    ///
    /// 回落**不是**容错降级，而是文档规定的兼容姿态：只配了 `[model]` 的安装
    /// 必须和不配列表时行为逐字一致。
    pub fn effective_model(&self) -> ResolvedModel {
        match self.providers.iter().find(|p| p.id == self.active_provider) {
            Some(p) => ResolvedModel {
                id: p.id.clone(),
                provider: p.provider.clone(),
                base_url: p.base_url.clone(),
                model: p.model.clone(),
                max_turns: self.model.max_turns,
                secret_name: p.secret_name(),
            },
            None => ResolvedModel {
                id: self.model.provider.clone(),
                provider: self.model.provider.clone(),
                base_url: self.model.base_url.clone(),
                model: self.model.model.clone(),
                max_turns: self.model.max_turns,
                secret_name: self.model.provider.clone(),
            },
        }
    }

    /// 校验服务商文档：id 非空且唯一、active 要么空要么命中。
    /// 报错而不是静默修——静默丢掉一个 id 会让用户以为已经配好，实际那家根本不存在。
    pub fn validate_providers(providers: &[ProviderProfile], active: &str) -> Result<(), String> {
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
            }
            // 清单非空时当前生效行必须在里面：不在的话席位下拉里没有它，用户看不到
            // 自己正在跑的模型，随手点一下别的行就把运行档换掉了。
            if !p.models.is_empty() && !p.model.trim().is_empty() && !rows.contains(&p.model.as_str()) {
                return Err(format!(
                    "服务商 {} 的当前模型 {} 不在模型清单里，请先把它加进清单",
                    p.id, p.model
                ));
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
    /// 每任务最大轮数（风险 R3：无限循环烧 token）。
    pub max_turns: u32,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            provider: "openai-responses".into(),
            base_url: String::new(),
            model: String::new(),
            max_turns: 32,
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
}

/// M0 默认只读：真实沙箱 M2 落地，fail-closed 语义见 ADR-002。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    #[default]
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ApprovalConfig {
    pub mode: ApprovalMode,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    /// 写入/命令逐次询问（默认）。
    #[default]
    Ask,
    /// 只读自动放行，其余仍询问（M2 起 broker 裁决）。
    AutoReadOnly,
}

/// 工作区配置（docs §9.1：Agent 操作的项目目录，用户指定）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceConfig {
    /// 工作区根目录；空 = 用户主目录（%USERPROFILE%）。
    pub root: String,
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
            // 界面上"显示路径缩写"用的家目录必须与运行时的回落工作区同源，
            // 否则同一份文档在界面里会被缩成另一个前缀。
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
/// `logs\` / `cache\` / `crash\` 与默认工作区 `Workspace\`。便携模式下两侧根相同，
/// 逐名创建互不影响。失败即报错：后续写路径不该带着一个建不出来的根继续跑。
pub fn ensure_product_dirs(dirs: &DataDirs) -> Result<(), String> {
    for name in [SECRETS_DIR, SETTINGS_BACKUP_DIR] {
        create_subdir(&dirs.config_root, name)?;
    }
    for name in [SESSIONS_DIR, ROLLOUTS_DIR, LOGS_DIR, CACHE_DIR, CRASH_DIR, WORKSPACE_DIR] {
        create_subdir(&dirs.data_root, name)?;
    }
    Ok(())
}

fn create_subdir(root: &Path, name: &str) -> Result<(), String> {
    let path = root.join(name);
    fs::create_dir_all(&path).map_err(|e| format!("创建目录失败 {}: {e}", path.display()))
}

/// 纯函数解析（输入显式注入，可单测各优先级分支）。
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
    // 优先级 3：安装模式默认
    let fallback = std::env::temp_dir().join(APP_DIR_NAME);
    let config_root = appdata.map(|p| p.join(APP_DIR_NAME)).unwrap_or_else(|| fallback.clone());
    let data_root = localdata.map(|p| p.join(APP_DIR_NAME)).unwrap_or(config_root.clone());
    DataDirs { config_root, data_root }
}

/// 读取真实环境后的解析入口。
pub fn resolve_dirs_from_env() -> DataDirs {
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    resolve_dirs(
        std::env::var(DATA_DIR_ENV).ok().as_deref(),
        exe_dir.as_deref(),
        std::env::var_os("APPDATA").map(PathBuf::from).as_deref(),
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from).as_deref(),
    )
}

impl AppConfig {
    /// 加载 `config.toml`；不存在则写入默认配置并返回（fail-closed：解析失败即报错）。
    pub fn load_or_create(config_root: &Path) -> Result<(Self, PathBuf), String> {
        let path = config_root.join(CONFIG_FILE);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败: {e}"))?;
        }
        if !path.exists() {
            let default = Self::default();
            default.save(&path)?;
            return Ok((default, path));
        }
        let raw = fs::read_to_string(&path).map_err(|e| format!("读取配置失败: {e}"))?;
        let cfg: Self = toml::from_str(&raw).map_err(|e| format!("解析配置失败: {e}"))?;
        Ok((cfg, path))
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let body = toml::to_string_pretty(self).map_err(|e| format!("序列化配置失败: {e}"))?;
        fs::write(path, body).map_err(|e| format!("写入配置失败: {e}"))
    }
}

/// 单调会话 ID（M3 rollout 接入后换正式 ULID/UUID）。
pub fn new_session_id(seq: u32) -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
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
    let mut reg = SECRET_REGISTRY.write().unwrap_or_else(|e| e.into_inner());
    if !reg.iter().any(|x| x == s) {
        reg.push(s.to_string());
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

/// 对任意将要落盘的文本做统一脱敏：先整串替换已登记密钥，再按模式擦掉形似密钥的串。
/// logs 与 rollouts 的所有写入都必须经过这里（docs/01 §438「落盘前统一过脱敏过滤器」）。
pub fn redact(text: &str) -> String {
    let mut out = text.to_string();
    {
        // 中毒后照常读：锁里是要保密的清单，panic 掉整个进程比少脱敏一次更糟
        let reg = SECRET_REGISTRY.read().unwrap_or_else(|e| e.into_inner());
        for secret in reg.iter() {
            if out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), REDACTED);
            }
        }
    }
    for p in patterns() {
        out = p.re.replace_all(&out, p.template).into_owned();
    }
    out
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

    fn store() -> SecretStore {
        let base = std::env::temp_dir().join(format!("shaniu-secrets-{}", std::process::id()));
        SecretStore::new(&base)
    }

    #[test]
    fn round_trip_store_load() {
        let s = store();
        s.store_key("openai-responses", "sk-test-123").unwrap();
        assert_eq!(s.load_key("openai-responses").unwrap().as_deref(), Some("sk-test-123"));
        // 落盘的是 DPAPI 密文而非明文
        let raw = std::fs::read(s.key_path("openai-responses").unwrap()).unwrap();
        assert!(!raw.windows(12).any(|w| w == b"sk-test-123"));
        s.delete_key("openai-responses").unwrap();
    }

    #[test]
    fn missing_key_is_none() {
        let s = store();
        assert_eq!(s.load_key("nonexistent").unwrap(), None);
    }

    #[test]
    fn delete_then_load_is_none() {
        let s = store();
        s.store_key("p1", "secret").unwrap();
        s.delete_key("p1").unwrap();
        assert_eq!(s.load_key("p1").unwrap(), None);
    }

    #[test]
    fn tampered_cipher_fails_closed() {
        let s = store();
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
        let s = store();
        let path = s.key_path("../evil").unwrap();
        // 分隔符被清洗：不逃逸 secrets 目录
        assert!(path.file_name().unwrap() == "evil.key");
    }

    #[test]
    fn empty_key_rejected() {
        let s = store();
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
        let base = std::env::temp_dir().join(format!("shaniu-test-{}", std::process::id()));
        let exe = base.join("app");
        fs::create_dir_all(&exe).unwrap();
        fs::write(exe.join(PORTABLE_MARKER), b"").unwrap();
        let dirs = resolve_dirs(None, Some(&exe), Some(Path::new("C:/appdata")), None);
        let expected = exe.join("data");
        assert_eq!(dirs.config_root, expected);
        assert_eq!(dirs.data_root, expected);
        fs::remove_dir_all(&base).ok();
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
        let base = std::env::temp_dir().join(format!("shaniu-layout-{}", std::process::id()));
        // 安装版：配置根与数据根分离（%APPDATA% / %LOCALAPPDATA%）
        let installed = DataDirs {
            config_root: base.join("appdata"),
            data_root: base.join("localdata"),
        };
        ensure_product_dirs(&installed).unwrap();
        for name in [SECRETS_DIR, SETTINGS_BACKUP_DIR] {
            assert!(installed.config_root.join(name).is_dir(), "缺少配置侧目录 {name}");
        }
        for name in [SESSIONS_DIR, ROLLOUTS_DIR, LOGS_DIR, CACHE_DIR, CRASH_DIR, WORKSPACE_DIR] {
            assert!(installed.data_root.join(name).is_dir(), "缺少数据侧目录 {name}");
        }
        // 默认工作区落在数据根下的 Workspace
        assert_eq!(installed.workspace_root(), installed.data_root.join(WORKSPACE_DIR));

        // 便携版：同一根（<exe>\data）；重复调用幂等
        let portable = DataDirs {
            config_root: base.join("data"),
            data_root: base.join("data"),
        };
        ensure_product_dirs(&portable).unwrap();
        ensure_product_dirs(&portable).unwrap();
        assert!(portable.workspace_root().is_dir());
        assert!(portable.config_root.join(SECRETS_DIR).is_dir());
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn unwritable_root_reports_error_instead_of_silently_skipping() {
        let base = std::env::temp_dir().join(format!("shaniu-layout-bad-{}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        // 数据根是一条文件路径：建子目录必然失败，必须如实报错而不是当作已建好
        let blocked = base.join("blocked");
        fs::write(&blocked, b"file").unwrap();
        let dirs = DataDirs { config_root: blocked.clone(), data_root: blocked };
        assert!(ensure_product_dirs(&dirs).is_err());
        fs::remove_dir_all(&base).ok();
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
        let base = std::env::temp_dir().join(format!("shaniu-cfg-{}", std::process::id()));
        let root = base.join("cfg");
        let (cfg, path) = AppConfig::load_or_create(&root).unwrap();
        assert_eq!(cfg.sandbox.mode, SandboxMode::ReadOnly);
        assert_eq!(cfg.approval.mode, ApprovalMode::Ask);
        assert_eq!(cfg.model.max_turns, 32);

        // 缺字段 → serde default 兜底（前向兼容旧配置文件）
        let partial = "[model]\nprovider = \"openai-chat\"\n";
        fs::write(&path, partial).unwrap();
        let (cfg2, _) = AppConfig::load_or_create(&root).unwrap();
        assert_eq!(cfg2.model.provider, "openai-chat");
        assert_eq!(cfg2.model.max_turns, 32);
        assert_eq!(cfg2.sandbox.mode, SandboxMode::ReadOnly);
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn corrupt_config_fails_closed() {
        let base = std::env::temp_dir().join(format!("shaniu-bad-{}", std::process::id()));
        let root = base.join("cfg");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(CONFIG_FILE), "???not toml???[").unwrap();
        assert!(AppConfig::load_or_create(&root).is_err());
        fs::remove_dir_all(&base).ok();
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
        let path = std::env::temp_dir().join(format!("shaniu-providers-{}.toml", std::process::id()));
        cfg.save(&path).unwrap();
        let back: AppConfig = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back, cfg, "服务商文档必须原样往返：半途丢字段就等于用户配的 Key 名变了");
        std::fs::remove_file(&path).ok();
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
}
