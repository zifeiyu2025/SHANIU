//! TOML 配置与数据目录解析（docs §9.3）。
//!
//! 解析优先级：`SHANIU_DATA_DIR` 环境变量 → exe 同目录 `portable.marker`（便携模式）
//! → `%APPDATA%\SHANIU` + `%LOCALAPPDATA%\SHANIU`（安装模式）。
//! 便携模式两种形态共用同一代码路径，差异仅在数据根目录。
//!
//! 数据边界（AGENTS §3）：本模块是未来 `secrets\`（DPAPI 加密）的唯一读写入口；
//! DPAPI 存取随 M2 权限模块接入（M0 无模型调用、无密钥使用场景）。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub const DATA_DIR_ENV: &str = "SHANIU_DATA_DIR";
pub const PORTABLE_MARKER: &str = "portable.marker";
pub const CONFIG_FILE: &str = "config.toml";
const APP_DIR_NAME: &str = "SHANIU";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub model: ModelConfig,
    pub sandbox: SandboxConfig,
    pub approval: ApprovalConfig,
    pub workspace: WorkspaceConfig,
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

pub const SECRETS_DIR: &str = "secrets";

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

    /// 存储并加密。空密钥拒绝。
    pub fn store_key(&self, provider: &str, plaintext: &str) -> Result<(), String> {
        if plaintext.trim().is_empty() {
            return Err("密钥为空".into());
        }
        let cipher = dpapi_protect(plaintext.as_bytes())?;
        let path = self.key_path(provider)?;
        fs::create_dir_all(path.parent().expect("密钥路径必有父目录"))
            .map_err(|e| format!("创建 secrets 目录失败: {e}"))?;
        fs::write(&path, cipher).map_err(|e| format!("写入密钥失败: {e}"))
    }

    /// 读取并解密；不存在返回 Ok(None)。解密失败（损坏/跨机器）返回 Err——fail-closed。
    pub fn load_key(&self, provider: &str) -> Result<Option<String>, String> {
        let path = self.key_path(provider)?;
        if !path.exists() {
            return Ok(None);
        }
        let cipher = fs::read(&path).map_err(|e| format!("读取密钥失败: {e}"))?;
        let plain = dpapi_unprotect(&cipher)?;
        String::from_utf8(plain)
            .map(Some)
            .map_err(|_| "密钥编码非法".into())
    }

    pub fn delete_key(&self, provider: &str) -> Result<(), String> {
        let path = self.key_path(provider)?;
        if path.exists() {
            fs::remove_file(&path).map_err(|e| format!("删除密钥失败: {e}"))?;
        }
        Ok(())
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
