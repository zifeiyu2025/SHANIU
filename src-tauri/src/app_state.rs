//! 全局状态（tauri::State 管理）：配置 + 会话运行时（活动任务取消令牌）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, RwLock};

use tokio_util::sync::CancellationToken;

use crate::config::{new_session_id, AppConfig};

pub struct AppState {
    config: RwLock<AppConfig>,
    config_path: RwLock<PathBuf>,
    session_seq: AtomicU32,
    /// 当前活动任务的取消令牌（None = 空闲）。
    active_task: Mutex<Option<CancellationToken>>,
}

impl AppState {
    pub fn new(config: AppConfig, config_path: PathBuf) -> Self {
        Self {
            config: RwLock::new(config),
            config_path: RwLock::new(config_path),
            session_seq: AtomicU32::new(1),
            active_task: Mutex::new(None),
        }
    }

    /// 克隆当前配置（AppConfig 小对象，克隆廉价且避免持锁跨 await）。
    pub fn config(&self) -> AppConfig {
        self.config.read().expect("config 锁中毒").clone()
    }

    pub fn config_path(&self) -> PathBuf {
        self.config_path.read().expect("config path 锁中毒").clone()
    }

    /// 配置根目录（config.toml 与 secrets\ 的父目录）。
    pub fn config_root(&self) -> PathBuf {
        self.config_path()
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
    }

    /// 工作区根目录：配置优先，空则回落用户主目录（docs §9.1）。
    pub fn workspace_root(&self) -> PathBuf {
        let configured = self.config().workspace.root.trim().to_string();
        if !configured.is_empty() {
            return PathBuf::from(configured);
        }
        std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
    }

    /// 更新并持久化配置（M1 的 set_model_config / set_workspace 命令复用）。
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

    /// 登记活动任务；已有任务在跑时返回 false（防重入）。
    pub fn begin_task(&self, cancel: CancellationToken) -> bool {
        let mut guard = self.active_task.lock().expect("active_task 锁中毒");
        if guard.is_some() {
            return false;
        }
        *guard = Some(cancel);
        true
    }

    /// 结束活动任务（任务收口时调用）。
    pub fn end_task(&self) {
        *self.active_task.lock().expect("active_task 锁中毒") = None;
    }

    /// 取消活动任务；无任务返回 false。
    pub fn cancel_active(&self) -> bool {
        match self.active_task.lock().expect("active_task 锁中毒").clone() {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    pub fn has_active_task(&self) -> bool {
        self.active_task.lock().expect("active_task 锁中毒").is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    fn state() -> AppState {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        AppState::new(
            AppConfig::default(),
            std::env::temp_dir().join(format!("shaniu-state-{}-{n}.toml", std::process::id())),
        )
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

    #[test]
    fn task_slot_is_exclusive_and_cancellable() {
        let s = state();
        assert!(!s.has_active_task());
        let token = CancellationToken::new();
        assert!(s.begin_task(token.clone()));
        assert!(!s.begin_task(CancellationToken::new()), "重入必须被拒");
        assert!(s.has_active_task());
        assert!(s.cancel_active());
        assert!(token.is_cancelled());
        s.end_task();
        assert!(!s.has_active_task());
        assert!(!s.cancel_active());
    }

    #[test]
    fn workspace_root_falls_back_to_user_profile() {
        let s = state();
        assert!(s.workspace_root().starts_with(std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default()));
        let mut cfg = s.config();
        cfg.workspace.root = "D:/tmp/ws".into();
        s.update_config(cfg).unwrap();
        assert_eq!(s.workspace_root(), PathBuf::from("D:/tmp/ws"));
    }
}
