//! 首连批准的对象是**指纹**，不是 server 自报的 id/name（docs/02 §3.1.4(1)）。
//!
//! 规范输入（排序后做 SHA-256）：
//! 1. 传输方式与 `initialize.protocolVersion`；
//! 2. 解析后的绝对可执行路径 + 文件 SHA-256；
//! 3. 完整 argv（逐项原样）；
//! 4. 工作目录（绝对路径）；
//! 5. 环境变量键名 + 值的摘要（**不落明文**）；
//! 6. server 自报 name/version（只作变更检测信号）；
//! 7. 排序后的分发名清单；
//! 8. 解释器型启动（`python -m <module>`）的模块代码树摘要（见 [`super::module_tree`]）。
//!    非解释器型启动没有这一项，规范输入与旧版本完全一致，不制造无意义的重新批准。
//!
//! **工具描述 / Schema / 注解不进指纹**（用户 2026-11-08 口径，覆盖 docs/02 §3.1.4(3)
//! 的「描述变更也重批」）：改描述不重批，但工具清单增删必须重批。内容变更的检测与
//! 展示由 [`super::trust`] 另存摘要承担。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 算法版本写进规范输入：换算法等于换指纹，旧批准自动失效（fail-closed 为 review）。
pub const FINGERPRINT_ALGO: &str = "shaniu-mcp-fingerprint-v1";
/// env 值摘要的域分隔符（防止与其它哈希用途撞同一个输入）。
const ENV_DIGEST_DOMAIN: &str = "shaniu-mcp-env-digest-v1";

/// 一项环境变量的摘要。键名保留供界面展示，值只有摘要（§3.1.4(1) 第 5 条）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvDigest {
    pub key: String,
    pub value_sha256: String,
}

/// 已覆盖的模块代码树（解释器型启动才有）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleTreeDigest {
    pub module: String,
    pub sha256: String,
    pub files: usize,
    pub bytes: u64,
}

/// 指纹规范输入。字段声明顺序就是序列化顺序，`env`/`tools` 进构造时已排序，
/// 因此 [`canonical`](Self::canonical) 的输出对同一组身份项是稳定的。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FingerprintInput {
    pub algo: String,
    pub transport: String,
    pub protocol_version: String,
    /// 解析后的绝对路径（批准时固化，之后不再走 PATH）。
    pub executable: String,
    pub executable_sha256: String,
    pub argv: Vec<String>,
    pub cwd: String,
    pub env: Vec<EnvDigest>,
    pub server_name: String,
    pub server_version: String,
    /// 按分发名排序的完整工具清单。**只放名字**：描述/Schema/注解变更不在指纹里。
    pub tools: Vec<String>,
    /// 模块代码树摘要。`None`（非解释器型）时整个字段不进规范输入，旧指纹保持不变。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module_tree: Option<ModuleTreeDigest>,
}

impl FingerprintInput {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        transport: &str,
        protocol_version: &str,
        executable: &str,
        executable_sha256: &str,
        argv: &[String],
        cwd: &str,
        env: &BTreeMap<String, String>,
        server_name: &str,
        server_version: &str,
        tools: &[String],
        module_tree: Option<ModuleTreeDigest>,
    ) -> Self {
        let mut env_digests: Vec<EnvDigest> = env
            .iter()
            .map(|(key, value)| EnvDigest {
                key: key.clone(),
                value_sha256: env_value_digest(key, value),
            })
            .collect();
        env_digests.sort_by(|a, b| a.key.cmp(&b.key));

        let mut tool_names: Vec<String> = tools.to_vec();
        tool_names.sort();
        tool_names.dedup();

        Self {
            algo: FINGERPRINT_ALGO.to_string(),
            transport: transport.to_string(),
            protocol_version: protocol_version.to_string(),
            executable: executable.to_string(),
            executable_sha256: executable_sha256.to_string(),
            argv: argv.to_vec(),
            cwd: cwd.to_string(),
            env: env_digests,
            server_name: server_name.to_string(),
            server_version: server_version.to_string(),
            tools: tool_names,
            module_tree,
        }
    }

    /// 规范 JSON：结构体字段顺序固定，`env` 与 `tools` 已排序，serde_json 的对象键
    /// 默认按 BTreeMap 有序输出——同一组身份项必然得到同一串字节。
    pub fn canonical(&self) -> String {
        serde_json::to_string(self).expect("指纹规范输入必定可序列化")
    }

    /// SHA-256 小写十六进制。
    pub fn fingerprint(&self) -> String {
        sha256_hex(self.canonical().as_bytes())
    }

    /// 界面展示用：`KEY = sha256:xxxx…`（值永远不出现）。
    pub fn masked_env(&self) -> Vec<String> {
        self.env
            .iter()
            .map(|entry| format!("{} = sha256:{}…", entry.key, &entry.value_sha256[..8.min(entry.value_sha256.len())]))
            .collect()
    }
}

/// env 值摘要：域分隔 + 键名 + 值。**不是加密**——低熵值仍可被暴力枚举；
/// 这里的目标是「不落明文、不能一眼读走」，与 §3.1.4(1) 的「HMAC 更佳」相比是 v1 的
/// 已知降级项（HMAC 需要一台机器的密钥，v1 不引入密钥管理）。
pub fn env_value_digest(key: &str, value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(ENV_DIGEST_DOMAIN.as_bytes());
    hasher.update([0u8]);
    hasher.update(key.as_bytes());
    hasher.update([0u8]);
    hasher.update(value.as_bytes());
    hex(&hasher.finalize())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex(&digest)
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn input(env_map: &BTreeMap<String, String>, tools: &[&str]) -> FingerprintInput {
        let owned: Vec<String> = tools.iter().map(|t| t.to_string()).collect();
        FingerprintInput::new(
            "stdio",
            "2024-11-05",
            r"C:\tools\server.exe",
            &"a".repeat(64),
            &["--flag".to_string(), "value".to_string()],
            r"C:\workspace",
            env_map,
            "self-reported-name",
            "1.2.3",
            &owned,
            None,
        )
    }

    #[test]
    fn fingerprint_is_stable_and_canonical_order_independent() {
        let first = input(&env(&[("B", "2"), ("A", "1")]), &["mcp__srv__b", "mcp__srv__a"]);
        let second = input(&env(&[("A", "1"), ("B", "2")]), &["mcp__srv__a", "mcp__srv__b"]);
        assert_eq!(first.canonical(), second.canonical(), "顺序不同不能改变身份");
        assert_eq!(first.fingerprint(), second.fingerprint());
        assert_eq!(first.fingerprint().len(), 64);
    }

    #[test]
    fn env_values_never_appear_in_plaintext_and_changes_change_the_fingerprint() {
        let secret = "super-secret-token";
        let first = input(&env(&[("TOKEN", secret)]), &["mcp__srv__a"]);
        let canonical = first.canonical();
        assert!(!canonical.contains(secret), "env 明文不得进入指纹规范输入");
        assert!(first.env[0].value_sha256.len() == 64);

        let changed = input(&env(&[("TOKEN", "another")]), &["mcp__srv__a"]);
        assert_ne!(first.fingerprint(), changed.fingerprint());
        // 键名变化同样改变身份
        let renamed = input(&env(&[("TOKEN2", secret)]), &["mcp__srv__a"]);
        assert_ne!(first.fingerprint(), renamed.fingerprint());
    }

    #[test]
    fn tool_list_add_remove_changes_the_fingerprint() {
        let one = input(&BTreeMap::new(), &["mcp__srv__a"]);
        let two = input(&BTreeMap::new(), &["mcp__srv__a", "mcp__srv__b"]);
        assert_ne!(one.fingerprint(), two.fingerprint(), "工具清单增删必须重批");
    }

    #[test]
    fn description_and_schema_are_deliberately_outside_the_fingerprint() {
        // 指纹输入里根本没有描述/Schema 字段：server 改描述不改指纹（用户口径）。
        // 这条测试锁的是「构造参数里没有它们」，而不是靠字符串搜索。
        let canonical = input(&BTreeMap::new(), &["mcp__srv__a"]).canonical();
        assert!(!canonical.contains("description"));
        assert!(!canonical.contains("inputSchema"));
    }

    #[test]
    fn executable_path_and_argv_and_cwd_and_identity_all_matter() {
        let base = input(&BTreeMap::new(), &["mcp__srv__a"]);
        let mut other_cwd = base.clone();
        other_cwd.cwd = r"C:\elsewhere".into();
        assert_ne!(base.fingerprint(), other_cwd.fingerprint());

        let mut other_argv = base.clone();
        other_argv.argv.push("--extra".into());
        assert_ne!(base.fingerprint(), other_argv.fingerprint());

        let mut other_exe = base.clone();
        other_exe.executable_sha256 = "b".repeat(64);
        assert_ne!(base.fingerprint(), other_exe.fingerprint());

        let mut other_name = base.clone();
        other_name.server_name = "renamed".into();
        assert_ne!(base.fingerprint(), other_name.fingerprint());

        let mut other_protocol = base.clone();
        other_protocol.protocol_version = "2025-06-18".into();
        assert_ne!(base.fingerprint(), other_protocol.fingerprint());
    }

    #[test]
    fn module_tree_digest_is_part_of_identity_and_absent_for_direct_executables() {
        let base = input(&BTreeMap::new(), &["mcp__srv__a"]);
        assert!(
            !base.canonical().contains("module_tree"),
            "非解释器型（None）不得在规范输入里多出字段，否则旧批准会无故失效"
        );

        let mut covered = base.clone();
        covered.module_tree = Some(ModuleTreeDigest {
            module: "pkg".into(),
            sha256: "c".repeat(64),
            files: 3,
            bytes: 1234,
        });
        assert_ne!(
            base.fingerprint(),
            covered.fingerprint(),
            "模块代码树变化必须改变指纹"
        );
    }

    #[test]
    fn masked_env_hides_values() {
        let fingerprint = input(&env(&[("TOKEN", "secret")]), &["mcp__srv__a"]);
        let masked = fingerprint.masked_env();
        assert_eq!(masked.len(), 1);
        assert!(masked[0].starts_with("TOKEN = sha256:"));
        assert!(!masked[0].contains("secret"));
    }
}
