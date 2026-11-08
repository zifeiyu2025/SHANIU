//! MCP 运行时（SH-063）：配置 → 会话 → 指纹 → 信任裁决 → 注册表执行器的状态机。
//!
//! 三条硬约束：
//! - **批准对象是指纹**：首连只建 `review` 记录，用户看完指纹与逐工具清单后才注册；
//! - **冻结配置**：连接时把 `McpServerConfig`（含 env）快照放进待审记录，之后改配置
//!   不影响这次已展示的批准对象；
//! - **撤销立即生效**：撤销信任或豁免时先翻转执行器的撤销开关，已下发给任务的
//!   执行器随之失败，不等到下一轮任务重建 router。
//!
//! 连接失败不产生任何授权；损坏的信任文件由 [`super::trust`] 回落到空存储（review）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::config::{McpConfigSource, McpServerConfig, SandboxMode};
use crate::events::RiskLevel;
use crate::tool_router::ToolExecutor;

use super::command::{file_sha256, resolve_executable};
use super::fingerprint::FingerprintInput;
use super::guard::{self, Exemption, ExemptionKey, ExemptionStore, GuardVerdict, RuleId};
use super::manifest::{self, RegistrationPlan};
use super::registry::{self, ApprovedTool};
use super::session::{McpSession, RemoteTool, ServerIdentity, DEFAULT_HANDSHAKE_TIMEOUT};
use super::trust::{
    self, ToolDecision, ToolGrant, TrustError, TrustRecord, TrustStatus, TrustStore, TRUST_FILE,
};

/// MCP 应用级事件通道（无会话归属，与 `shaniu://event` 分开）。
pub const MCP_EVENT_CHANNEL: &str = "shaniu://mcp";

/// 服务器状态（界面与审计共用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpStatus {
    /// 未连接或已撤销。
    Idle,
    /// 待用户审查：工具不可见、不可调用。
    Review,
    /// 已批准：按逐工具批准位与档位注册。
    Trusted,
    /// 用户阻止。
    Blocked,
    /// 可执行文件与上次批准记录不一致：**未启动进程**，等用户显式确认新二进制后重新审查。
    Retrust,
    /// 进程退化（崩溃重启后仍失败）。
    Degraded,
    /// 连接/握手失败（不产生授权）。
    Failed,
}

impl McpStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            McpStatus::Idle => "idle",
            McpStatus::Review => "review",
            McpStatus::Trusted => "trusted",
            McpStatus::Blocked => "blocked",
            McpStatus::Retrust => "retrust",
            McpStatus::Degraded => "degraded",
            McpStatus::Failed => "failed",
        }
    }
}

impl From<TrustStatus> for McpStatus {
    fn from(status: TrustStatus) -> Self {
        match status {
            TrustStatus::Review => McpStatus::Review,
            TrustStatus::Trusted => McpStatus::Trusted,
            TrustStatus::Blocked => McpStatus::Blocked,
            TrustStatus::Retrust => McpStatus::Retrust,
        }
    }
}

/// 注入命中（逐字片段，去控制字符后展示；A-R4 可解释性）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardHitView {
    pub rule: String,
    pub fragment: String,
}

/// 审查界面上的一个工具。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewTool {
    pub dispatch_name: String,
    pub raw_name: String,
    pub description: String,
    pub schema: Value,
    #[serde(default)]
    pub annotations: Option<Value>,
    /// server 自称只读——**只作展示**，不参与风险决策（ADR-107）。
    pub server_read_only_hint: bool,
    /// 用户选定的档位（默认 destructive）。
    pub risk: RiskLevel,
    pub approved: bool,
    /// 授权是否作用于当前指纹（界面要能看出「这条授权是给谁的」）。
    pub fingerprint: String,
    pub guard_hits: Vec<GuardHitView>,
    #[serde(default)]
    pub guard_explanation: Option<String>,
    /// 已通过显式启用（A-R4）绕过注入拦截。
    pub guard_exempted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectedToolView {
    pub raw_name: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovedToolView {
    pub dispatch_name: String,
    pub risk: RiskLevel,
}

/// 一次首连审查（也是信任状态查询）的完整载荷。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewPayload {
    pub server_id: String,
    pub status: McpStatus,
    pub fingerprint: String,
    pub transport: String,
    pub executable: String,
    pub executable_sha256: String,
    /// 可执行文件当前修改时间（ms）。只在「需要重新信任」时给出，供用户判断替换时间。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable_mtime_ms: Option<u64>,
    /// 上次批准时记录的可执行文件哈希（只在「需要重新信任」时给出）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_executable_sha256: Option<String>,
    /// 上次批准时间（ms）。
    #[serde(default)]
    pub approved_at_ms: u64,
    /// `tools` 来自旧版本缓存，仅作参考，**不是当前有效清单**（retrust 载荷）。
    #[serde(default)]
    pub tools_stale: bool,
    pub argv: Vec<String>,
    pub cwd: String,
    /// `KEY = sha256:xxxxxxxx…`，值永不明文（§3.1.4(1)）。
    pub env: Vec<String>,
    pub protocol_version: String,
    pub server_name: String,
    pub server_version: String,
    pub allow_network: bool,
    pub unsandboxed: bool,
    pub tools: Vec<ReviewTool>,
    pub rejected_tools: Vec<RejectedToolView>,
    pub approved_tools: Vec<ApprovedToolView>,
    pub exemptions: Vec<Exemption>,
    pub warnings: Vec<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// 审查界面逐工具的裁决输入。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TrustDecisionInput {
    pub dispatch_name: String,
    pub approved: bool,
    #[serde(default)]
    pub risk: Option<RiskLevel>,
    /// 工具被注入规则命中时，用户看完命中片段后的显式启用（A-R4）。
    #[serde(default)]
    pub enable_injection: bool,
}

impl TrustDecisionInput {
    fn risk(&self) -> RiskLevel {
        trust::sanitize_user_risk(self.risk.unwrap_or(RiskLevel::Destructive))
    }
}

/// 应用级 MCP 事件（`shaniu://mcp`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpEvent {
    ServerStatusChanged {
        server_id: String,
        status: McpStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    ManifestReview {
        server_id: String,
        review: Box<ReviewPayload>,
    },
    TrustResolved {
        server_id: String,
        fingerprint: String,
        status: McpStatus,
        tools: Vec<ApprovedToolView>,
    },
    ExemptionChanged {
        server_id: String,
        fingerprint: String,
        exemptions: Vec<Exemption>,
    },
}

/// 事件接收方（Tauri 在组合根实现；测试用记录型实现）。
pub trait McpEventSink: Send + Sync {
    fn emit(&self, event: &McpEvent);
}

pub struct NullMcpSink;

impl McpEventSink for NullMcpSink {
    fn emit(&self, _event: &McpEvent) {}
}

#[derive(Clone)]
struct Pending {
    config: McpServerConfig,
    identity: ServerIdentity,
    input: FingerprintInput,
    plan: RegistrationPlan,
    guard_rejected: Vec<registry::GuardRejection>,
    remote_tools: Vec<RemoteTool>,
    session: Arc<Mutex<McpSession>>,
}

struct Active {
    remote_tools: Vec<RemoteTool>,
    approved: Vec<ApprovedTool>,
    executors: Vec<Arc<dyn ToolExecutor>>,
    revocation: BTreeMap<String, Arc<AtomicBool>>,
    session: Arc<Mutex<McpSession>>,
}

/// 构建 [`ReviewPayload`] 的上下文：状态、来源信任记录与已收集的警告。
struct PayloadContext<'a> {
    status: McpStatus,
    record: Option<&'a TrustRecord>,
    warnings: Vec<String>,
}

/// 进程级 MCP 运行时。
pub struct McpRuntime {
    trust_path: PathBuf,
    trust: Mutex<TrustStore>,
    exemptions: Mutex<ExemptionStore>,
    pending: Mutex<BTreeMap<String, Pending>>,
    /// 已连接服务器：用 std 锁，组合根需要在**同步**构 router 时取执行器副本。
    active: std::sync::Mutex<BTreeMap<String, Active>>,
    sink: RwLock<Arc<dyn McpEventSink>>,
}

impl McpRuntime {
    pub fn new(config_root: &Path) -> Self {
        let trust_path = config_root.join("secrets").join(TRUST_FILE);
        let store = TrustStore::load(&trust_path);
        let exemptions = ExemptionStore::from_entries(store.exemptions());
        Self {
            trust_path,
            trust: Mutex::new(store),
            exemptions: Mutex::new(exemptions),
            pending: Mutex::new(BTreeMap::new()),
            active: std::sync::Mutex::new(BTreeMap::new()),
            sink: RwLock::new(Arc::new(NullMcpSink)),
        }
    }

    pub fn set_sink(&self, sink: Arc<dyn McpEventSink>) {
        *self.sink.write().expect("MCP sink 锁中毒") = sink;
    }

    pub fn trust_path(&self) -> &Path {
        &self.trust_path
    }

    fn emit(&self, event: McpEvent) {
        let sink = self.sink.read().expect("MCP sink 锁中毒").clone();
        sink.emit(&event);
    }

    /// 首连 / 重连：返回审查载荷。已受信任且指纹未变时直接重建执行器并返回 trusted。
    pub async fn connect_and_review(
        &self,
        config: &McpServerConfig,
        workspace_root: &Path,
    ) -> Result<ReviewPayload, String> {
        self.connect_and_review_with(config, workspace_root, false).await
    }

    /// 同 [`connect_and_review`](Self::connect_and_review)，但允许用户显式确认新二进制。
    ///
    /// `accept_new_binary=false` 时，只要可执行文件哈希与批准记录不一致就**不启动进程、
    /// 不注册、不列当前清单**——「两次批准之间二进制自己变了」本身即攻击形态，不能为了
    /// 展示清单先把它跑起来（docs/02 §3.1.4(3)）。用户确认后才 spawn → 握手 → 列清单 →
    /// 走逐工具风险档位。
    pub async fn connect_and_review_with(
        &self,
        config: &McpServerConfig,
        workspace_root: &Path,
        accept_new_binary: bool,
    ) -> Result<ReviewPayload, String> {
        if !crate::config::is_valid_mcp_server_id(&config.id) {
            return Err(format!("非法的 MCP server id: {}", config.id));
        }
        if !config.enabled {
            return Err(format!("MCP server {} 未启用", config.id));
        }
        // 首审 spawn 的正当性只成立于「配置由用户自己撰写」（应用级 config.toml）。
        // 工作区/项目级 mcp.servers 等同于执行仓库提供的二进制，必须先审声明再启动，
        // 不能沿用首审 spawn 路径。这里做成硬拒绝，而不是依赖调用方自觉。
        if config.source != McpConfigSource::User {
            return Err(format!(
                "MCP server {} 来自工作区配置，必须先完成两阶段审查（不 spawn）才能启动；当前拒绝 spawn",
                config.id
            ));
        }
        self.stop_server(&config.id).await;

        let executable = {
            let command = config.command.clone();
            tokio::task::spawn_blocking(move || resolve_executable(&command))
                .await
                .map_err(|error| format!("解析可执行文件的任务失败: {error}"))?
                .map_err(|error| error.to_string())?
        };
        let executable_sha256 = {
            let path = executable.clone();
            tokio::task::spawn_blocking(move || file_sha256(&path))
                .await
                .map_err(|error| format!("计算可执行文件哈希的任务失败: {error}"))?
                .map_err(|error| format!("计算可执行文件哈希失败: {error}"))?
        };

        let cwd = if config.cwd.trim().is_empty() {
            workspace_root.to_path_buf()
        } else {
            PathBuf::from(config.cwd.trim())
        };
        let cwd = cwd.canonicalize().unwrap_or(cwd);
        if !cwd.is_dir() {
            return Err(format!(
                "MCP server {} 的工作目录不存在: {}",
                config.id,
                cwd.display()
            ));
        }

        let existing = self.trust.lock().await.record(&config.id).cloned();
        // `accept_new_binary` 只在**已处于 retrust** 时才生效：它代表「用户在『需要重新
        // 信任』界面上确认了新二进制」。任何自动化流程、重试逻辑或前端默认值直接传 true
        // 都不能绕过这道闸——非 retrust 状态下的 true 一律按 false 处理。
        let confirmed = accept_new_binary
            && matches!(
                existing.as_ref().map(|record| record.status),
                Some(TrustStatus::Retrust)
            );
        // 哈希不一致 = 两次批准之间二进制被换过。没有用户显式确认时**绝不 spawn**：
        // 为了展示清单先把它跑起来，等于把最危险的一步放在最前面。
        if !confirmed {
            if let Some(record) = &existing {
                let approved_before = !matches!(record.status, TrustStatus::Review);
                if approved_before
                    && !record
                        .input
                        .executable_sha256
                        .eq_ignore_ascii_case(&executable_sha256)
                {
                    return Ok(self
                        .retrust_payload(config, record, &executable, &executable_sha256, &cwd)
                        .await);
                }
            }
        }

        let sandbox_mode = if config.unsandboxed {
            SandboxMode::DangerFullAccess
        } else {
            SandboxMode::WorkspaceWrite
        };
        let mut session = McpSession::start(config, sandbox_mode, &cwd, &executable_sha256)
            .await
            .map_err(|error| format!("启动 MCP server 失败: {error}"))?;
        let identity = session
            .initialize(DEFAULT_HANDSHAKE_TIMEOUT)
            .await
            .map_err(|error| format!("MCP 握手失败: {error}"))?;
        let remote_tools = session
            .list_tools(DEFAULT_HANDSHAKE_TIMEOUT)
            .await
            .map_err(|error| format!("读取工具清单失败: {error}"))?;

        let plan = manifest::plan_registration(&config.id, &remote_tools);
        let dispatch_names: Vec<String> = plan
            .accepted
            .iter()
            .map(|tool| tool.dispatch_name.clone())
            .collect();
        let input = FingerprintInput::new(
            "stdio",
            &identity.protocol_version,
            &executable.to_string_lossy(),
            &executable_sha256,
            &config.args,
            &cwd.to_string_lossy(),
            &config.env,
            &identity.name,
            &identity.version,
            &dispatch_names,
        );
        let fingerprint = input.fingerprint();
        let fresh_grants = trust::grants_from_plan(&plan);
        let session = Arc::new(Mutex::new(session));

        let guard_rejected = {
            let exemptions = self.exemptions.lock().await;
            registry::partition_by_guard(&plan, &config.id, &fingerprint, &exemptions).1
        };
        let mut warnings = Vec::new();
        if identity.instructions_dropped {
            warnings.push("server 下发了 initialize.instructions，v1 已丢弃（A-R1）".to_string());
        }
        if config.unsandboxed {
            warnings.push(format!(
                "该 server 以 unsandboxed 启动：{}",
                if config.unsandboxed_reason.trim().is_empty() {
                    "未填写原因"
                } else {
                    config.unsandboxed_reason.trim()
                }
            ));
        }
        if config.allow_network {
            warnings.push("该 server 声明允许网络访问（v1 进程约束不保证网络隔离）".to_string());
        }

        match existing {
            Some(record)
                if record.fingerprint == fingerprint && record.status == TrustStatus::Trusted =>
            {
                let diff = trust::diff_manifest(&record.tools, &fresh_grants);
                if diff.requires_review() {
                    // 指纹里只有工具名，能走到这里的变化只可能是参数 Schema：
                    // 用户批准时看到的工具形态变了，必须重新审查（docs/02 §3.1.4(3)）。
                    let reason = Some(format!(
                        "工具参数 Schema 已变化，需要重新审查：{}",
                        diff.explain()
                    ));
                    return self
                        .enter_review(
                            config,
                            identity,
                            input,
                            plan,
                            guard_rejected,
                            remote_tools,
                            session,
                            fresh_grants,
                            reason,
                            warnings,
                        )
                        .await;
                }
                // 描述 / 注解变化：只更新基线，不重批；注入过滤在每次连接时都已重跑。
                let mut refreshed = record.clone();
                trust::refresh_digests(&mut refreshed, &fresh_grants);
                refreshed.cached_tools = trust::cached_from_plan(&plan);
                if let Err(error) = self.upsert_quiet(&refreshed).await {
                    tracing::warn!("更新 MCP 内容摘要失败: {error}");
                }
                let approved = approved_from_record(&refreshed);
                let built = registry::build_executors(
                    &config.id,
                    &fingerprint,
                    &remote_tools,
                    &approved,
                    session.clone(),
                    &*self.exemptions.lock().await,
                );
                let payload = self
                    .build_payload(
                        PayloadContext {
                            status: McpStatus::Trusted,
                            record: Some(&refreshed),
                            warnings,
                        },
                        config,
                        &identity,
                        &input,
                        &plan,
                    )
                    .await;
                self.active.lock().expect("MCP active 锁中毒").insert(
                    config.id.clone(),
                    Active {
                        remote_tools,
                        approved,
                        executors: built.executors,
                        revocation: built.revocation,
                        session,
                    },
                );
                self.emit(McpEvent::ServerStatusChanged {
                    server_id: config.id.clone(),
                    status: McpStatus::Trusted,
                    reason: None,
                });
                Ok(payload)
            }
            Some(record)
                if record.fingerprint == fingerprint && record.status == TrustStatus::Blocked =>
            {
                session.lock().await.shutdown();
                self.emit(McpEvent::ServerStatusChanged {
                    server_id: config.id.clone(),
                    status: McpStatus::Blocked,
                    reason: record.reason.clone(),
                });
                Ok(self
                    .build_payload(
                        PayloadContext {
                            status: McpStatus::Blocked,
                            record: Some(&record),
                            warnings,
                        },
                        config,
                        &identity,
                        &input,
                        &plan,
                    )
                    .await)
            }
            Some(record) => {
                // 身份或工具清单变化 → 全部重批；差异只用于解释原因。
                let diff = trust::diff_manifest(&record.tools, &fresh_grants);
                let reason = if record.fingerprint == fingerprint {
                    record
                        .reason
                        .clone()
                        .or_else(|| Some("等待首次审查".to_string()))
                } else {
                    Some(format!(
                        "身份或工具清单已变化，需要重新审查：{}",
                        diff.explain()
                    ))
                };
                self.enter_review(
                    config,
                    identity,
                    input,
                    plan,
                    guard_rejected,
                    remote_tools,
                    session,
                    fresh_grants,
                    reason,
                    warnings,
                )
                .await
            }
            None => {
                self.enter_review(
                    config,
                    identity,
                    input,
                    plan,
                    guard_rejected,
                    remote_tools,
                    session,
                    fresh_grants,
                    Some("首次连接：请核对指纹与逐工具风险档位后批准".to_string()),
                    warnings,
                )
                .await
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn enter_review(
        &self,
        config: &McpServerConfig,
        identity: ServerIdentity,
        input: FingerprintInput,
        plan: RegistrationPlan,
        guard_rejected: Vec<registry::GuardRejection>,
        remote_tools: Vec<RemoteTool>,
        session: Arc<Mutex<McpSession>>,
        fresh_grants: Vec<ToolGrant>,
        reason: Option<String>,
        warnings: Vec<String>,
    ) -> Result<ReviewPayload, String> {
        let record = self
            .trust
            .lock()
            .await
            .store_review(
                &config.id,
                input.clone(),
                fresh_grants,
                trust::cached_from_plan(&plan),
                reason,
            )
            .map_err(|error| error.to_string())?;
        self.pending.lock().await.insert(
            config.id.clone(),
            Pending {
                config: config.clone(),
                identity: identity.clone(),
                input: input.clone(),
                plan: plan.clone(),
                guard_rejected,
                remote_tools,
                session,
            },
        );
        let payload = self
            .build_payload(
                PayloadContext {
                    status: McpStatus::Review,
                    record: Some(&record),
                    warnings,
                },
                config,
                &identity,
                &input,
                &plan,
            )
            .await;
        self.emit(McpEvent::ManifestReview {
            server_id: config.id.clone(),
            review: Box::new(payload.clone()),
        });
        self.emit(McpEvent::ServerStatusChanged {
            server_id: config.id.clone(),
            status: McpStatus::Review,
            reason: record.reason.clone(),
        });
        Ok(payload)
    }

    /// 可执行文件哈希变化时的载荷：**没有启动进程**，工具清单来自上次批准的缓存。
    ///
    /// 状态置为 `retrust` 并落盘；界面据此展示「上次批准哈希 vs 当前哈希、路径、时间」，
    /// 旧清单必须标注为参考。只有用户显式确认后才会走完整的 spawn → 握手 → 审查流程。
    async fn retrust_payload(
        &self,
        config: &McpServerConfig,
        record: &TrustRecord,
        executable: &Path,
        executable_sha256: &str,
        cwd: &Path,
    ) -> ReviewPayload {
        let previous = record.input.executable_sha256.clone();
        let reason = format!(
            "可执行文件哈希已变化（上次批准 {}，当前 {}）：未启动进程，需确认新二进制后重新审查",
            short_sha(&previous),
            short_sha(executable_sha256)
        );
        if record.status != TrustStatus::Retrust {
            let mut updated = record.clone();
            updated.status = TrustStatus::Retrust;
            updated.reason = Some(reason.clone());
            if let Err(error) = self.upsert_quiet(&updated).await {
                tracing::warn!("写入 MCP 重新信任状态失败: {error}");
            }
        }

        let tools: Vec<ReviewTool> = record
            .cached_tools
            .iter()
            .map(|tool| {
                let grant = record
                    .tools
                    .iter()
                    .find(|grant| grant.dispatch_name == tool.dispatch_name);
                let (guard_hits, guard_explanation) = match guard::inspect(&tool.description) {
                    GuardVerdict::Clean => (Vec::new(), None),
                    GuardVerdict::Suspicious(hits) => (
                        hits.iter()
                            .map(|hit| GuardHitView {
                                rule: hit.rule.as_str().to_string(),
                                fragment: hit.fragment.clone(),
                            })
                            .collect(),
                        Some(guard::explain(&tool.dispatch_name, &hits)),
                    ),
                };
                ReviewTool {
                    dispatch_name: tool.dispatch_name.clone(),
                    raw_name: tool.raw_name.clone(),
                    description: tool.description.clone(),
                    schema: tool.schema.clone(),
                    annotations: tool.annotations.clone(),
                    server_read_only_hint: tool
                        .annotations
                        .as_ref()
                        .and_then(|value| value.get("readOnlyHint"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    risk: grant.map(|grant| grant.risk).unwrap_or(RiskLevel::Destructive),
                    // 旧批准属于旧指纹：一律显示未批准，绝不冒充当前有效授权。
                    approved: false,
                    fingerprint: record.fingerprint.clone(),
                    guard_hits,
                    guard_explanation,
                    guard_exempted: false,
                }
            })
            .collect();

        let mtime_ms = std::fs::metadata(executable)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis() as u64);

        let payload = ReviewPayload {
            server_id: config.id.clone(),
            status: McpStatus::Retrust,
            fingerprint: record.fingerprint.clone(),
            transport: record.input.transport.clone(),
            executable: executable.to_string_lossy().to_string(),
            executable_sha256: executable_sha256.to_string(),
            executable_mtime_ms: mtime_ms,
            previous_executable_sha256: Some(previous),
            approved_at_ms: record.approved_at_ms,
            tools_stale: true,
            argv: record.input.argv.clone(),
            cwd: cwd.to_string_lossy().to_string(),
            env: record.input.masked_env(),
            protocol_version: record.input.protocol_version.clone(),
            server_name: record.input.server_name.clone(),
            server_version: record.input.server_version.clone(),
            allow_network: config.allow_network,
            unsandboxed: config.unsandboxed,
            tools,
            rejected_tools: Vec::new(),
            approved_tools: Vec::new(),
            exemptions: record.exemptions.clone(),
            warnings: vec![
                "可执行文件在两次批准之间发生了变化：本次没有启动进程，也没有注册任何工具。"
                    .to_string(),
                "下面的工具清单来自上次批准的缓存，仅作参考，不是当前版本。".to_string(),
            ],
            reason: Some(reason),
        };
        self.emit(McpEvent::ServerStatusChanged {
            server_id: config.id.clone(),
            status: McpStatus::Retrust,
            reason: payload.reason.clone(),
        });
        payload
    }

    /// 应用用户裁决：批准位 + 逐工具档位 + 注入工具的显式启用。
    pub async fn resolve_trust(
        &self,
        server_id: &str,
        fingerprint: &str,
        decisions: &[TrustDecisionInput],
    ) -> Result<ReviewPayload, String> {
        let pending = self
            .pending
            .lock()
            .await
            .get(server_id)
            .cloned()
            .ok_or_else(|| format!("服务器 {server_id} 没有待审查会话，请重新连接"))?;
        if pending.input.fingerprint() != fingerprint {
            return Err(TrustError::FingerprintMismatch {
                recorded: pending.input.fingerprint(),
                submitted: fingerprint.to_string(),
            }
            .to_string());
        }

        let known: BTreeMap<&str, &manifest::PlannedTool> = pending
            .plan
            .accepted
            .iter()
            .map(|tool| (tool.dispatch_name.as_str(), tool))
            .collect();
        for decision in decisions {
            if !known.contains_key(decision.dispatch_name.as_str()) {
                return Err(format!(
                    "裁决里出现清单中不存在的工具: {}（拒绝写入信任记录）",
                    decision.dispatch_name
                ));
            }
        }

        // 显式启用注入工具：只在用户勾选且要求 enable_injection 时授予豁免。
        {
            let mut exemptions = self.exemptions.lock().await;
            for decision in decisions {
                if !decision.approved || !decision.enable_injection {
                    continue;
                }
                let Some(rejected) = pending
                    .guard_rejected
                    .iter()
                    .find(|item| item.dispatch_name == decision.dispatch_name)
                else {
                    continue;
                };
                let rule = rejected
                    .hits
                    .first()
                    .map(|hit| hit.rule)
                    .unwrap_or(RuleId::InstructionOverride);
                let fragment = rejected
                    .hits
                    .first()
                    .map(|hit| hit.fragment.clone())
                    .unwrap_or_default();
                exemptions.grant(
                    server_id,
                    fingerprint,
                    &decision.dispatch_name,
                    rule,
                    &fragment,
                );
            }
        }

        // 注入命中且没有「显式启用」的批准不算数：即使勾了批准，工具也不会注册。
        // 这里提前折算成未批准，保证 `trusted/blocked` 与真实注册结果一致，
        // 不出现「状态是 trusted、实际零工具」的自相矛盾。
        let effective: std::collections::BTreeSet<&str> = decisions
            .iter()
            .filter(|decision| decision.approved)
            .filter(|decision| {
                decision.enable_injection
                    || !pending
                        .guard_rejected
                        .iter()
                        .any(|item| item.dispatch_name == decision.dispatch_name)
            })
            .map(|decision| decision.dispatch_name.as_str())
            .collect();

        let tool_decisions: Vec<ToolDecision> = decisions
            .iter()
            .map(|decision| {
                if effective.contains(decision.dispatch_name.as_str()) {
                    ToolDecision::approve(decision.dispatch_name.clone(), decision.risk())
                } else {
                    ToolDecision::skip(decision.dispatch_name.clone())
                }
            })
            .collect();

        let scoped: Vec<Exemption> = self
            .exemptions
            .lock()
            .await
            .list()
            .into_iter()
            .filter(|entry| entry.server_id == server_id && entry.fingerprint == fingerprint)
            .collect();
        let record = {
            let mut store = self.trust.lock().await;
            store
                .resolve(server_id, fingerprint, &tool_decisions)
                .map_err(|error| error.to_string())?;
            store
                .set_exemptions(server_id, scoped.clone())
                .map_err(|error| error.to_string())?;
            // 重新取一次：`resolve` 的返回值快照早于 `set_exemptions`，直接用会漏掉本次豁免。
            store
                .record(server_id)
                .cloned()
                .ok_or_else(|| format!("服务器 {server_id} 的信任记录在裁决后消失"))?
        };

        let approved = approved_from_record(&record);
        let status = McpStatus::from(record.status);
        if status == McpStatus::Trusted {
            let built = registry::build_executors(
                server_id,
                fingerprint,
                &pending.remote_tools,
                &approved,
                pending.session.clone(),
                &*self.exemptions.lock().await,
            );
            self.active.lock().expect("MCP active 锁中毒").insert(
                server_id.to_string(),
                Active {
                    remote_tools: pending.remote_tools.clone(),
                    approved: approved.clone(),
                    executors: built.executors,
                    revocation: built.revocation,
                    session: pending.session.clone(),
                },
            );
            self.pending.lock().await.remove(server_id);
        }

        let payload = self
            .build_payload(
                PayloadContext {
                    status,
                    record: Some(&record),
                    warnings: Vec::new(),
                },
                &pending.config,
                &pending.identity,
                &pending.input,
                &pending.plan,
            )
            .await;
        self.emit(McpEvent::TrustResolved {
            server_id: server_id.to_string(),
            fingerprint: fingerprint.to_string(),
            status,
            tools: payload.approved_tools.clone(),
        });
        self.emit(McpEvent::ServerStatusChanged {
            server_id: server_id.to_string(),
            status,
            reason: Some(if status == McpStatus::Trusted {
                "已按用户裁决注册工具".to_string()
            } else {
                "用户未批准任何工具".to_string()
            }),
        });
        Ok(payload)
    }

    /// 撤销整个服务器的信任：先让在途执行器失效，再删记录与豁免。
    pub async fn revoke_trust(&self, server_id: &str) -> Result<(), String> {
        self.stop_server(server_id).await;
        let removed = self
            .trust
            .lock()
            .await
            .revoke(server_id)
            .map_err(|error| error.to_string())?;
        {
            let mut exemptions = self.exemptions.lock().await;
            let keys: Vec<ExemptionKey> = exemptions
                .list()
                .into_iter()
                .filter(|entry| entry.server_id == server_id)
                .map(|entry| {
                    ExemptionKey::new(entry.server_id, entry.fingerprint, entry.dispatch_name)
                })
                .collect();
            for key in keys {
                exemptions.revoke(&key);
            }
        }
        if removed.is_none() {
            return Err(format!("服务器 {server_id} 没有可撤销的信任记录"));
        }
        self.emit(McpEvent::ServerStatusChanged {
            server_id: server_id.to_string(),
            status: McpStatus::Idle,
            reason: Some("信任与豁免已撤销，下次连接需重新审查".to_string()),
        });
        Ok(())
    }

    /// 全部豁免记录（插件页「豁免」tab 的只读数据源，跨服务器、跨指纹）。
    pub async fn list_exemptions(&self) -> Vec<Exemption> {
        self.exemptions.lock().await.list()
    }

    /// 撤销单个注入豁免：工具立即不可调用，下次连接也不再注册。
    ///
    /// `fingerprint` 用于撤销**旧指纹**留下的记录（插件页豁免台账会列出它们）；
    /// 缺省时作用于当前信任记录的指纹。
    pub async fn revoke_exemption(
        &self,
        server_id: &str,
        dispatch_name: &str,
        fingerprint: Option<&str>,
    ) -> Result<(), String> {
        let current_fingerprint = {
            let trust = self.trust.lock().await;
            let record = trust
                .record(server_id)
                .ok_or_else(|| format!("服务器 {server_id} 没有信任记录"))?;
            record.fingerprint.clone()
        };
        let fingerprint = fingerprint.unwrap_or(&current_fingerprint);
        {
            let mut exemptions = self.exemptions.lock().await;
            exemptions.revoke(&ExemptionKey::new(server_id, fingerprint, dispatch_name));
        }
        let scoped: Vec<Exemption> = self
            .exemptions
            .lock()
            .await
            .list()
            .into_iter()
            .filter(|entry| {
                entry.server_id == server_id && entry.fingerprint == current_fingerprint
            })
            .collect();
        self.trust
            .lock()
            .await
            .set_exemptions(server_id, scoped.clone())
            .map_err(|error| error.to_string())?;

        // 先把旧执行器整体失效，再重建——绝不让撤销窗口里继续可调。
        let active_snapshot = {
            let active = self.active.lock().expect("MCP active 锁中毒");
            active.get(server_id).map(|active| {
                (
                    active.remote_tools.clone(),
                    active.approved.clone(),
                    active.session.clone(),
                    active.revocation.values().cloned().collect::<Vec<_>>(),
                )
            })
        };
        if let Some((remote_tools, approved, session, flags)) = active_snapshot {
            for flag in flags {
                flag.store(false, Ordering::SeqCst);
            }
            let built = registry::build_executors(
                server_id,
                &current_fingerprint,
                &remote_tools,
                &approved,
                session,
                &*self.exemptions.lock().await,
            );
            let mut active_map = self.active.lock().expect("MCP active 锁中毒");
            if let Some(entry) = active_map.get_mut(server_id) {
                entry.executors = built.executors;
                entry.revocation = built.revocation;
            }
        }
        self.emit(McpEvent::ExemptionChanged {
            server_id: server_id.to_string(),
            fingerprint: current_fingerprint,
            exemptions: scoped,
        });
        Ok(())
    }

    /// 当前所有已批准工具的执行器（组合根在每个任务的 router 上注册副本）。
    pub fn executors(&self) -> Vec<Arc<dyn ToolExecutor>> {
        self.active
            .lock()
            .expect("MCP active 锁中毒")
            .values()
            .flat_map(|active| active.executors.iter().cloned())
            .collect()
    }

    /// 已完成连接的服务器状态（未连接的不出现）。
    pub async fn statuses(&self) -> Vec<ServerStatus> {
        let trust = self.trust.lock().await;
        let pending = self.pending.lock().await;
        let active = self.active.lock().expect("MCP active 锁中毒");
        trust
            .records()
            .into_iter()
            .map(|record| {
                let status = if pending.contains_key(&record.server_id) {
                    McpStatus::Review
                } else if active.contains_key(&record.server_id) {
                    McpStatus::Trusted
                } else {
                    McpStatus::from(record.status)
                };
                ServerStatus {
                    server_id: record.server_id.clone(),
                    status,
                    fingerprint: record.fingerprint.clone(),
                    approved_tools: record
                        .approved_tools()
                        .into_iter()
                        .map(|(dispatch_name, risk)| ApprovedToolView {
                            dispatch_name,
                            risk,
                        })
                        .collect(),
                    exemption_count: record.exemptions.len(),
                    reason: record.reason.clone(),
                }
            })
            .collect()
    }

    /// 进程退出前关闭全部子进程。
    pub async fn shutdown_all(&self) {
        let ids: Vec<String> = {
            let mut ids: Vec<String> = self.pending.lock().await.keys().cloned().collect();
            ids.extend(
                self.active
                    .lock()
                    .expect("MCP active 锁中毒")
                    .keys()
                    .cloned(),
            );
            ids
        };
        for id in ids {
            self.stop_server(&id).await;
        }
    }

    async fn upsert_quiet(&self, record: &TrustRecord) -> Result<(), TrustError> {
        let mut store = self.trust.lock().await;
        store.upsert(record.clone())?;
        Ok(())
    }

    async fn stop_server(&self, server_id: &str) {
        let pending = self.pending.lock().await.remove(server_id);
        let active = self
            .active
            .lock()
            .expect("MCP active 锁中毒")
            .remove(server_id);
        if let Some(pending) = pending {
            pending.session.lock().await.shutdown();
        }
        if let Some(active) = active {
            for flag in active.revocation.values() {
                flag.store(false, Ordering::SeqCst);
            }
            active.session.lock().await.shutdown();
        }
    }

    async fn build_payload(
        &self,
        context: PayloadContext<'_>,
        config: &McpServerConfig,
        identity: &ServerIdentity,
        input: &FingerprintInput,
        plan: &RegistrationPlan,
    ) -> ReviewPayload {
        let PayloadContext {
            status,
            record,
            mut warnings,
        } = context;
        let fingerprint = input.fingerprint();
        // 只在本函数的同步判定里持锁；调用方不持锁跨 await。
        let exemptions = self.exemptions.lock().await;
        let is_exempt = |dispatch: &str| exemptions.is_exempt(&config.id, &fingerprint, dispatch);
        let grant_of = |dispatch: &str| {
            record.and_then(|record| {
                record
                    .tools
                    .iter()
                    .find(|grant| grant.dispatch_name == dispatch)
            })
        };

        let tools: Vec<ReviewTool> = plan
            .accepted
            .iter()
            .map(|tool| {
                let (guard_hits, guard_explanation) = match guard::inspect(&tool.raw_description) {
                    GuardVerdict::Clean => (Vec::new(), None),
                    GuardVerdict::Suspicious(hits) => (
                        hits.iter()
                            .map(|hit| GuardHitView {
                                rule: hit.rule.as_str().to_string(),
                                fragment: hit.fragment.clone(),
                            })
                            .collect(),
                        Some(guard::explain(&tool.dispatch_name, &hits)),
                    ),
                };
                let grant = grant_of(&tool.dispatch_name);
                ReviewTool {
                    dispatch_name: tool.dispatch_name.clone(),
                    raw_name: tool.raw_name.clone(),
                    description: tool.spec.description.clone(),
                    schema: tool.spec.parameters.clone(),
                    annotations: tool.annotations.clone(),
                    server_read_only_hint: tool
                        .annotations
                        .as_ref()
                        .and_then(|value| value.get("readOnlyHint"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    risk: grant
                        .map(|grant| grant.risk)
                        .unwrap_or(RiskLevel::Destructive),
                    approved: grant.map(|grant| grant.approved).unwrap_or(false),
                    fingerprint: fingerprint.clone(),
                    guard_hits,
                    guard_explanation,
                    guard_exempted: is_exempt(&tool.dispatch_name),
                }
            })
            .collect();

        let rejected_tools = plan
            .rejected
            .iter()
            .map(|tool| RejectedToolView {
                raw_name: tool.raw_name.clone(),
                reason: tool.reason.explain(),
            })
            .collect();

        let approved_tools = record
            .map(|record| {
                record
                    .approved_tools()
                    .into_iter()
                    .map(|(dispatch_name, risk)| ApprovedToolView {
                        dispatch_name,
                        risk,
                    })
                    .collect()
            })
            .unwrap_or_default();

        let exemptions = record
            .map(|record| record.exemptions.clone())
            .unwrap_or_default();

        for collision in &plan.collisions {
            warnings.push(format!(
                "工具名冲突（归一化后同名）：{} → {}",
                collision.raw_names.join(" / "),
                collision.dispatch_names.join(" / ")
            ));
        }

        ReviewPayload {
            server_id: config.id.clone(),
            status,
            fingerprint,
            transport: input.transport.clone(),
            executable: input.executable.clone(),
            executable_sha256: input.executable_sha256.clone(),
            executable_mtime_ms: None,
            previous_executable_sha256: None,
            approved_at_ms: record.map(|record| record.approved_at_ms).unwrap_or(0),
            tools_stale: false,
            argv: input.argv.clone(),
            cwd: input.cwd.clone(),
            env: input.masked_env(),
            protocol_version: identity.protocol_version.clone(),
            server_name: identity.name.clone(),
            server_version: identity.version.clone(),
            allow_network: config.allow_network,
            unsandboxed: config.unsandboxed,
            tools,
            rejected_tools,
            approved_tools,
            exemptions,
            warnings,
            reason: record.and_then(|record| record.reason.clone()),
        }
    }
}

fn short_sha(value: &str) -> &str {
    &value[..12.min(value.len())]
}

fn approved_from_record(record: &TrustRecord) -> Vec<ApprovedTool> {
    record
        .approved_tools()
        .into_iter()
        .map(|(dispatch_name, risk)| ApprovedTool {
            dispatch_name,
            risk,
        })
        .collect()
}

/// 服务器状态视图（`mcp_list_servers` 用）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerStatus {
    pub server_id: String,
    pub status: McpStatus,
    pub fingerprint: String,
    pub approved_tools: Vec<ApprovedToolView>,
    /// 用户显式启用的注入豁免数量。
    #[serde(default)]
    pub exemption_count: usize,
    #[serde(default)]
    pub reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event_shape(event: &McpEvent) -> Value {
        serde_json::to_value(event).unwrap()
    }

    #[test]
    fn mcp_event_wire_shape_is_stable() {
        let status = McpEvent::ServerStatusChanged {
            server_id: "srv".into(),
            status: McpStatus::Review,
            reason: Some("首次连接".into()),
        };
        let json = event_shape(&status);
        assert_eq!(json["type"], "server_status_changed");
        assert_eq!(json["status"], "review");
        assert_eq!(json["server_id"], "srv");

        let resolved = McpEvent::TrustResolved {
            server_id: "srv".into(),
            fingerprint: "fp".into(),
            status: McpStatus::Trusted,
            tools: vec![ApprovedToolView {
                dispatch_name: "mcp__srv__read".into(),
                risk: RiskLevel::Read,
            }],
        };
        let json = event_shape(&resolved);
        assert_eq!(json["type"], "trust_resolved");
        assert_eq!(json["tools"][0]["risk"], "read");

        let exemption = McpEvent::ExemptionChanged {
            server_id: "srv".into(),
            fingerprint: "fp".into(),
            exemptions: Vec::new(),
        };
        assert_eq!(event_shape(&exemption)["type"], "exemption_changed");
    }

    #[test]
    fn status_serializes_snake_case_for_ts_mirror() {
        for (status, expected) in [
            (McpStatus::Idle, "idle"),
            (McpStatus::Review, "review"),
            (McpStatus::Trusted, "trusted"),
            (McpStatus::Blocked, "blocked"),
            (McpStatus::Retrust, "retrust"),
            (McpStatus::Degraded, "degraded"),
            (McpStatus::Failed, "failed"),
        ] {
            assert_eq!(
                serde_json::to_value(status).unwrap(),
                Value::String(expected.into())
            );
            assert_eq!(status.as_str(), expected);
        }
    }
}
