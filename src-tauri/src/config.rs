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
}
