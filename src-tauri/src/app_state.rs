//! 全局状态（tauri::State 管理）：配置 + 会话运行时（活动任务取消令牌）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use tokio_util::sync::CancellationToken;

use crate::config::{new_session_id, AppConfig};
use crate::permission::ApprovalBroker;
use crate::tool_router::PlanState;

pub struct AppState {
    config: RwLock<AppConfig>,
    config_path: RwLock<PathBuf>,
    /// 数据根目录（§9.3）：会话档案 rollout 与日志写在这里。
    /// 与 config_root 分开——安装模式下前者是 %LOCALAPPDATA%\SHANIU、后者是 %APPDATA%\SHANIU，
    /// 从 config_path 反推会把档案写错地方。
    data_root: PathBuf,
    session_seq: AtomicU32,
    /// 当前活动任务（None = 空闲）：(会话 id, 取消令牌)。会话 id 供归档/置顶
    /// 等操作判断"目标会话是否正在跑"（SH-100）——单任务闸下它就是唯一的那个。
    active_task: Mutex<Option<(String, CancellationToken)>>,
    /// 审批 broker（approve 命令回执入口）。
    pub broker: Arc<ApprovalBroker>,
    /// 计划态标志（Plan Mode，跨任务保留：同一会话内后续任务仍在计划态时继续只读）。
    pub plan_state: Arc<PlanState>,
    /// 进程级后台作业注册表（SH-089）：跨任务保留，进程退出即清空。
    pub jobs: Arc<crate::jobs::JobRegistry>,
    /// 活动任务的子代理注册表（SH-053 批 3）：UI steering/中断的命令面入口。
    /// 与任务同生命周期——`end_task` 清空，避免把消息投进已收口的僵尸任务。
    active_subagents: Mutex<Option<Arc<crate::subagent_registry::SubagentRegistry>>>,
    /// MCP 运行时（SH-063）：信任记录、首连审查、已批准执行器。
    pub mcp: Arc<crate::mcp_client::runtime::McpRuntime>,
}

impl AppState {
    pub fn new(config: AppConfig, config_path: PathBuf, data_root: PathBuf) -> Self {
        let config_root = config_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        Self {
            config: RwLock::new(config),
            config_path: RwLock::new(config_path),
            data_root,
            session_seq: AtomicU32::new(1),
            active_task: Mutex::new(None),
            broker: Arc::new(ApprovalBroker::new()),
            plan_state: Arc::new(PlanState::new()),
            jobs: Arc::new(crate::jobs::JobRegistry::new()),
            active_subagents: Mutex::new(None),
            mcp: Arc::new(crate::mcp_client::runtime::McpRuntime::new(&config_root)),
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
    pub fn config_root(&self) -> PathBuf {        self.config_path()
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
    }

    /// 数据根目录（rollout 档案与日志；§9.3 安装模式 = %LOCALAPPDATA%\SHANIU）。
    pub fn data_root(&self) -> PathBuf {
        self.data_root.clone()
    }

    /// 工作区根目录：配置优先，空则回落用户主目录（docs §9.1）。
    pub fn workspace_root(&self) -> PathBuf {
        let configured = self.config().workspace.root.trim().to_string();
        if !configured.is_empty() {
            return PathBuf::from(configured);
        }
        // 与 `get_product_dirs` 回给界面的 homeDir 同一个来源：两处各读各的环境变量，
        // 就会出现"工作区建在 A、界面按 B 缩写路径"。
        crate::config::home_dir()
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

    /// 登记活动任务（带会话 id）；已有任务在跑时返回 false（防重入）。
    pub fn begin_task(&self, agent_id: &str, cancel: CancellationToken) -> bool {
        let mut guard = self.active_task.lock().expect("active_task 锁中毒");
        if guard.is_some() {
            return false;
        }
        *guard = Some((agent_id.to_string(), cancel));
        true
    }

    /// 结束活动任务（任务收口时调用）。
    pub fn end_task(&self) {
        *self.active_task.lock().expect("active_task 锁中毒") = None;
        *self.active_subagents.lock().expect("active_subagents 锁中毒") = None;
    }

    /// 挂载活动任务的子代理注册表（任务启动、begin_task 成功后调用）。
    pub fn set_subagents(&self, registry: Option<Arc<crate::subagent_registry::SubagentRegistry>>) {
        *self.active_subagents.lock().expect("active_subagents 锁中毒") = registry;
    }

    /// 活动任务的子代理注册表（None = 空闲，steering 不可投递）。
    pub fn subagents(&self) -> Option<Arc<crate::subagent_registry::SubagentRegistry>> {
        self.active_subagents.lock().expect("active_subagents 锁中毒").clone()
    }

    /// 正在跑任务的会话 id（None = 空闲）。
    pub fn active_agent(&self) -> Option<String> {
        self.active_task
            .lock()
            .expect("active_task 锁中毒")
            .as_ref()
            .map(|(id, _)| id.clone())
    }

    /// 取消活动任务；无任务返回 false。
    pub fn cancel_active(&self) -> bool {
        match self.active_task.lock().expect("active_task 锁中毒").clone() {
            Some((_, token)) => {
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
            std::env::temp_dir().join(format!("shaniu-state-data-{}-{n}", std::process::id())),
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
        assert!(s.begin_task("sess-1", token.clone()));
        assert!(!s.begin_task("sess-2", CancellationToken::new()), "重入必须被拒");
        assert!(s.has_active_task());
        assert_eq!(s.active_agent().as_deref(), Some("sess-1"));
        assert!(s.cancel_active());
        assert!(token.is_cancelled());
        s.end_task();
        assert!(!s.has_active_task());
        assert_eq!(s.active_agent(), None);
        assert!(!s.cancel_active());
    }

    #[test]
    fn subagent_registry_lives_only_for_the_active_task() {
        let s = state();
        assert!(s.subagents().is_none(), "空闲态没有可 steering 的注册表");
        let registry = Arc::new(crate::subagent_registry::SubagentRegistry::new(2));
        assert!(s.begin_task("sess-1", CancellationToken::new()));
        s.set_subagents(Some(registry.clone()));
        assert!(Arc::ptr_eq(&s.subagents().expect("任务期应能取到注册表"), &registry));
        s.end_task();
        assert!(s.subagents().is_none(), "任务收口必须释放注册表，否则 steering 会投进僵尸任务");
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
