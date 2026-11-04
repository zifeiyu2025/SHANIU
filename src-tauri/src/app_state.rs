//! 全局状态（tauri::State 管理）：配置 + 会话注册表雏形（M1 接入 agent_loop 后扩展）。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::RwLock;

use crate::config::{new_session_id, AppConfig};

pub struct AppState {
    config: RwLock<AppConfig>,
    config_path: RwLock<std::path::PathBuf>,
    session_seq: AtomicU32,
}

impl AppState {
    pub fn new(config: AppConfig, config_path: std::path::PathBuf) -> Self {
        Self {
            config: RwLock::new(config),
            config_path: RwLock::new(config_path),
            session_seq: AtomicU32::new(1),
        }
    }

    /// 克隆当前配置（AppConfig 小对象，克隆廉价且避免持锁跨 await）。
    pub fn config(&self) -> AppConfig {
        self.config.read().expect("config 锁中毒").clone()
    }

    pub fn config_path(&self) -> std::path::PathBuf {
        self.config_path.read().expect("config path 锁中毒").clone()
    }

    /// 更新并持久化配置（M1 的 set_config 命令复用）。
    pub fn update_config(&self, next: AppConfig) -> Result<(), String> {
        {
            let mut guard = self.config.write().expect("config 锁中毒");
            *guard = next;
        }
        let path = self.config_path();
        self.config().save(&path)
    }

    /// 注册新会话，返回唯一 ID。
    pub fn next_session_id(&self) -> String {
        let seq = self.session_seq.fetch_add(1, Ordering::SeqCst);
        new_session_id(seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AppState {
        AppState::new(AppConfig::default(), std::env::temp_dir().join("shaniu-state-test.toml"))
    }

    #[test]
    fn session_ids_increase_uniquely() {
        let s = state();
        let a = s.next_session_id();
        let b = s.next_session_id();
        assert_ne!(a, b);
    }

    #[test]
    fn config_update_is_visible_and_persisted() {
        let s = state();
        let mut cfg = s.config();
        cfg.model.max_turns = 8;
        s.update_config(cfg).unwrap();
        assert_eq!(s.config().model.max_turns, 8);
        // 持久化落盘可回读
        let raw = std::fs::read_to_string(s.config_path()).unwrap();
        assert!(raw.contains("max_turns = 8"));
        std::fs::remove_file(s.config_path()).ok();
    }
}
