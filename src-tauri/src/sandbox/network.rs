//! 会话级网络授权（SH-021 网络决定，2026-11-08）。
//!
//! 规则：
//!  - v1 **默认断网**：AppContainer 进程不携带 `internetClient` 能力即无法出站；
//!  - 一个会话内批准一次「允许网络」，该会话内子进程继承；新会话重新要；
//!  - 每次授权/撤销都写审计（谁 / 哪个会话 / 什么时候 / 批的哪条），审计写不进去就拒绝授权；
//!  - 不落盘全局规则文件（ADR-008 已否 codex 的 `default.rules` 路线），
//!    进程重启后所有授权清零。
//!
//! 作用域键是 `session_id`；子代理沿用父会话 id（见 orchestration 的 session 归属），
//! 因此「父批准、子继承」成立，兄弟会话不共享。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkGrant {
    pub session_id: String,
    pub granted_by: String,
    pub granted_at_ms: u64,
    pub scope: String,
}

pub struct NetworkGrants {
    state: Mutex<HashMap<String, NetworkGrant>>,
    audit_path: Option<PathBuf>,
}

impl NetworkGrants {
    pub fn new(audit_path: Option<PathBuf>) -> Self {
        Self { state: Mutex::new(HashMap::new()), audit_path }
    }

    /// 批准一个会话的网络访问。审计写失败 → 不批准（fail-closed）。
    pub fn grant(&self, session_id: &str, scope: &str) -> Result<NetworkGrant, String> {
        if session_id.trim().is_empty() {
            return Err("网络授权需要会话作用域（session_id 为空）".into());
        }
        let grant = NetworkGrant {
            session_id: session_id.to_string(),
            granted_by: current_user(),
            granted_at_ms: now_ms(),
            scope: scope.to_string(),
        };
        self.append_audit("grant", &grant)
            .map_err(|error| format!("网络授权审计写入失败，已拒绝授权：{error}"))?;
        let mut state = self.state.lock().map_err(|_| "网络授权状态锁已中毒".to_string())?;
        state.insert(session_id.to_string(), grant.clone());
        Ok(grant)
    }

    /// 撤销一个会话的网络访问。返回是否本来存在。
    pub fn revoke(&self, session_id: &str) -> Result<bool, String> {
        let mut state = self.state.lock().map_err(|_| "网络授权状态锁已中毒".to_string())?;
        let removed = state.remove(session_id);
        drop(state);
        if let Some(grant) = &removed {
            self.append_audit("revoke", grant)
                .map_err(|error| format!("网络授权已撤销，但审计写入失败：{error}"))?;
        }
        Ok(removed.is_some())
    }

    pub fn is_granted(&self, session_id: Option<&str>) -> bool {
        let Some(session_id) = session_id else {
            return false;
        };
        self.state
            .lock()
            .map(|state| state.contains_key(session_id))
            .unwrap_or(false)
    }

    pub fn list(&self) -> Vec<NetworkGrant> {
        self.state
            .lock()
            .map(|state| state.values().cloned().collect())
            .unwrap_or_default()
    }

    fn append_audit(&self, event: &str, grant: &NetworkGrant) -> Result<(), String> {
        let Some(path) = &self.audit_path else {
            // 没有配置审计落盘（仅测试装配）：内存记录仍在，调用方负责不静默。
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建审计目录失败（{}）: {e}", parent.display()))?;
        }
        let line = serde_json::json!({
            "event": event,
            "session_id": grant.session_id,
            "granted_by": grant.granted_by,
            "granted_at_ms": grant.granted_at_ms,
            "scope": grant.scope,
        });
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| format!("打开审计文件失败（{}）: {e}", path.display()))?;
        use std::io::Write as _;
        writeln!(file, "{line}").map_err(|e| format!("写审计文件失败: {e}"))
    }
}

static GLOBAL: OnceLock<NetworkGrants> = OnceLock::new();

/// 进程级授权表。`init` 未调用时为内存表（无审计文件），仅用于测试。
pub fn init(audit_path: Option<PathBuf>) {
    let _ = GLOBAL.set(NetworkGrants::new(audit_path));
}

pub fn global() -> &'static NetworkGrants {
    GLOBAL.get_or_init(|| NetworkGrants::new(None))
}

fn current_user() -> String {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "unknown".into());
    match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.is_empty() => format!("{domain}\\{user}"),
        _ => user,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> crate::test_support::TestTempDir {
        crate::test_support::TestTempDir::new("net")
    }

    #[test]
    fn grant_is_session_scoped_and_revocable() {
        let dir = dir();
        let grants = NetworkGrants::new(Some(dir.join("audit.jsonl")));
        assert!(!grants.is_granted(Some("s1")));
        grants.grant("s1", "用户批准：本会话允许网络").unwrap();
        assert!(grants.is_granted(Some("s1")));
        assert!(!grants.is_granted(Some("s2")), "新会话不得继承同一授权");
        assert!(!grants.is_granted(None));
        assert!(grants.revoke("s1").unwrap());
        assert!(!grants.is_granted(Some("s1")));
    }

    #[test]
    fn audit_records_who_session_time_and_scope() {
        let dir = dir();
        let path = dir.join("audit.jsonl");
        let grants = NetworkGrants::new(Some(path.clone()));
        grants.grant("s9", "echo 需要联网").unwrap();
        grants.revoke("s9").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "grant / revoke 各一条审计：{text}");
        let grant: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(grant["event"], "grant");
        assert_eq!(grant["session_id"], "s9");
        assert_eq!(grant["scope"], "echo 需要联网");
        assert!(grant["granted_at_ms"].as_u64().unwrap() > 0);
        assert!(!grant["granted_by"].as_str().unwrap().is_empty());
        let revoke: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(revoke["event"], "revoke");
    }

    #[test]
    fn grant_is_refused_when_audit_is_unwritable() {
        let dir = dir();
        let blocked = dir.join("audit-dir");
        std::fs::write(&blocked, b"x").unwrap();
        let grants = NetworkGrants::new(Some(blocked.join("audit.jsonl")));
        let error = grants.grant("s1", "x").expect_err("审计写不进去时不得放行");
        assert!(error.contains("审计"), "{error}");
        assert!(!grants.is_granted(Some("s1")));
    }
}
