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
    /// 配置只读原因（启动时无法安全修复的坏配置）：Some = 禁止任何写回。
    /// 写入前不拦的话，一次保存就会把用户修不好的原文件覆盖掉。
    config_read_only: RwLock<Option<String>>,
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
    /// 会话策略句柄（SH-120）：任务运行中切换审批档，下一 turn 生效。
    /// 任务启动时由 `reset_policy` 从 config 重新装载，避免上一任务的选择泄漏。
    pub policy: Arc<crate::permission::SessionPolicyHandle>,
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
        let policy = Arc::new(crate::permission::SessionPolicyHandle::new(
            crate::permission::SessionPolicy {
                approval: config.approval.mode,
                sandbox: config.sandbox.mode,
            },
        ));
        let broker = Arc::new(ApprovalBroker::new());
        let state = Self {
            config: RwLock::new(config),
            config_path: RwLock::new(config_path),
            config_read_only: RwLock::new(None),
            data_root,
            session_seq: AtomicU32::new(1),
            active_task: Mutex::new(None),
            broker,
            policy,
            plan_state: Arc::new(PlanState::new()),
            jobs: Arc::new(crate::jobs::JobRegistry::new()),
            active_subagents: Mutex::new(None),
            mcp: Arc::new(crate::mcp_client::runtime::McpRuntime::new(&config_root)),
        };
        state.refresh_derived_policy();
        state
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

    /// 标记配置只读（启动时坏配置无法安全修复）。之后所有写回都被拒绝，
    /// 原因里带配置文件路径，直接回给设置页展示。
    pub fn mark_config_read_only(&self, reason: impl Into<String>) {
        let reason = reason.into();
        tracing::warn!(%reason, "配置进入只读模式：不会写回任何保存");
        *self.config_read_only.write().expect("config 只读锁中毒") = Some(reason);
    }

    /// 当前只读原因（None = 可写）。
    pub fn config_read_only_reason(&self) -> Option<String> {
        self.config_read_only.read().expect("config 只读锁中毒").clone()
    }

    /// 更新并持久化配置（M1 的 set_model_config / set_workspace 命令复用）。
    ///
    /// **先落盘、成功后才改内存**：`save` 的写前自检会拒绝串段/丢字段的配置，
    /// 若先改内存，拒绝后进程内看到的是新值、磁盘还是旧值——用户以为改好了，
    /// 而这次运行的所有请求已经用上了那份没落盘的配置。
    pub fn update_config(&self, next: AppConfig) -> Result<(), String> {
        if let Some(reason) = self.config_read_only_reason() {
            return Err(format!("配置处于只读模式，已拒绝写入：{reason}"));
        }
        let path = self.config_path();
        next.save(&path)?;
        {
            let mut guard = self.config.write().expect("config 锁中毒");
            *guard = next;
        }
        self.refresh_derived_policy();
        Ok(())
    }

    /// 任务启动时重置会话策略（SH-120）：审批档/沙箱档回到 config，
    /// 并重新装载前缀白名单——运行中切换过的档位不跨任务泄漏。
    pub fn reset_policy(&self) {
        self.refresh_derived_policy();
    }

    /// config → 运行时策略的**唯一**同步点（构造、配置变更、任务启动都走这里）。
    fn refresh_derived_policy(&self) {
        let cfg = self.config();
        self.policy.reset(crate::permission::SessionPolicy {
            approval: cfg.approval.mode,
            sandbox: cfg.sandbox.mode,
        });
        let (rules, rejected) =
            crate::permission::PrefixAllowlist::from_config(&cfg.approval.allow_prefixes);
        for reason in rejected {
            // 静默丢弃会让用户以为规则生效了；日志里逐条说明
            tracing::warn!("前缀白名单规则被忽略: {reason}");
        }
        self.broker.set_prefix_rules(rules);
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

    /// 返回 `(临时目录守卫, AppState)`：守卫必须绑定到变量活到测试结束，
    /// 否则临时目录会被立即删掉（2026-11-09 夹具规矩）。
    fn state() -> (crate::test_support::TestTempDir, AppState) {
        let dir = crate::test_support::TestTempDir::new("state");
        let state = AppState::new(
            AppConfig::default(),
            dir.join("config.toml"),
            dir.join("data"),
        );
        (dir, state)
    }

    #[test]
    fn session_ids_increase_uniquely() {
        let (_dir, s) = state();
        let a = s.next_session_id();
        let b = s.next_session_id();
        assert_ne!(a, b);
    }

    #[test]
    fn config_update_is_visible_and_persisted() {
        let (_dir, s) = state();
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
    fn config_read_only_blocks_every_write() {
        let (_dir, s) = state();
        s.mark_config_read_only("解析配置失败: bad value（文件未被改动：C:\\cfg\\config.toml）");
        let err = s.update_config(AppConfig::default()).unwrap_err();
        assert!(err.contains("只读") && err.contains("config.toml"), "只读原因必须带路径: {err}");
        assert!(!s.config_path().exists(), "只读模式必须一个字节都不写");
        assert!(
            s.config_read_only_reason().unwrap().contains("未被改动"),
            "只读原因要能原样展示给用户"
        );
    }

    #[test]
    fn rejected_save_keeps_memory_and_disk_unchanged() {
        let (_dir, s) = state();
        let mut good = s.config();
        good.model.max_turns = 9;
        s.update_config(good).unwrap();
        let before = std::fs::read_to_string(s.config_path()).unwrap();

        // 串段形态的档位表：写前自检必须拒绝，且**先落盘后改内存**意味着
        // 拒绝后内存里也不能留下这份坏配置
        let mut bad = s.config();
        let row = crate::config::ModelProfile {
            id: "m".into(),
            reasoning_efforts: [("path".to_string(), r"\\?\D:\tmp\ws2".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let provider = crate::config::ProviderProfile {
            id: "longcat".into(),
            models: vec![row],
            ..Default::default()
        };
        bad.providers = vec![provider];

        assert!(s.update_config(bad).is_err());
        assert!(s.config().providers.is_empty(), "拒绝落盘后内存不得留下坏配置");
        assert_eq!(s.config().model.max_turns, 9);
        assert_eq!(std::fs::read_to_string(s.config_path()).unwrap(), before);
        std::fs::remove_file(s.config_path()).ok();
    }

    #[test]
    fn task_slot_is_exclusive_and_cancellable() {
        let (_dir, s) = state();
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
        let (_dir, s) = state();
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
        let (_dir, s) = state();
        assert!(s.workspace_root().starts_with(std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default()));
        let mut cfg = s.config();
        cfg.workspace.root = "D:/tmp/ws".into();
        s.update_config(cfg).unwrap();
        assert_eq!(s.workspace_root(), PathBuf::from("D:/tmp/ws"));
    }
}
