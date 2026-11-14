//! 审批 broker（docs §8 permission/mod.rs，ADR-003：集中裁决、审批不可绕过）。
//!
//! 职责：工具执行前的门禁（gate）——按 policy 裁决；需要询问时发
//! `ExecApprovalRequest` 并挂起等待前端回执（approve 命令 → resolve）。
//! 审批 UI 不可伪造：裁决只接受后端校验过的 request_id 回执。

pub mod policy;
pub mod preset;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::clock::now_ms;
use crate::config::SandboxMode;
use crate::events::{ApprovalDecision, EventMsg, EventSink, ResponderState, RiskLevel};

/// 审批租约心跳间隔：适配层（页面）每 5s 续约一次；宿主只看 TTL，不看这个值。
pub const APPROVAL_LEASE_HEARTBEAT_MS: u64 = 5_000;
/// 审批租约 TTL：超过这么久没有心跳（页面崩溃 / 刷新 / 被挂起）即视为无人可答。
pub const APPROVAL_LEASE_TTL_MS: u64 = 15_000;
/// 看门狗扫描周期：有 pending 时每秒检查一次租约是否还活着。
pub const APPROVAL_UNATTENDED_SCAN_INTERVAL_MS: u64 = 1_000;

// re-export 便于 use permission::{policy, ...}
pub use policy::{
    decide, risk_of, Decision, PrefixAllowlist, SessionPolicy, SessionPolicyHandle, ToolAllowlist,
};

/// 门禁结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateOutcome {
    Allowed,
    /// 用户明确拒绝（审批卡回执 Deny）。
    Denied,
    /// 策略性拒绝：沙箱不允许或工具不在本代理解效工具集内。
    /// 与 `Denied` 分开是为了让回填给模型的理由准确——把策略拒绝说成
    /// "用户拒绝"会让模型误以为换个说法就能再过一次审批。
    Forbidden,
    Cancelled,
}

/// 一次门禁的代理身份（ADR-103 审批冒泡与记忆作用域）。
#[derive(Debug, Clone, Copy)]
pub struct GateSubject<'a> {
    /// 触发本次调用的代理；审批记忆按它分区。
    pub agent_id: &'a str,
    /// 子代理任务摘要；root 为 None。
    pub task_summary: Option<&'a str>,
    /// 本代理的有效工具白名单；None = 无额外限制。
    pub allowed_tools: Option<&'a ToolAllowlist>,
    /// 工具级风险覆盖（SH-063，ADR-107）：由用户批准记录注入，`None` 时回退 `risk_of`。
    pub risk_override: Option<RiskLevel>,
}

impl<'a> GateSubject<'a> {
    /// root 代理（无任务摘要、无白名单收窄）。
    pub fn root(agent_id: &'a str) -> Self {
        Self { agent_id, task_summary: None, allowed_tools: None, risk_override: None }
    }
}

/// 门禁的执行环境：事件出口与取消令牌成对出现。
/// 打包只为一件事：`gate` 的参数已经承载了全部安全输入（身份/白名单/策略），
/// 执行环境再逐项铺开就没人能一眼看清哪些是"决策依据"、哪些是"投递手段"。
#[derive(Clone, Copy)]
pub struct GateEnv<'a> {
    pub sink: &'a Arc<dyn EventSink>,
    pub cancel: &'a CancellationToken,
}

/// 一次审批询问的元信息：风险档 + 风险原因 + 是否允许 `AllowAlways` 写入会话记忆 + 是否为提权询问。
/// 打包成一个参数是因为这几项一起决定"这张审批卡长什么样、批完记不记"，
/// 铺开 9 个参数会让调用点看不出哪些是身份、哪些是提示。
#[derive(Debug, Clone, Copy)]
struct ApprovalPrompt<'a> {
    risk: RiskLevel,
    reason: Option<&'a str>,
    remember: bool,
    /// 提权询问（on-failure）：只影响请求 id，不允许批量，也不写会话记忆。
    escalation: bool,
}

/// 审批 broker：pending 请求表 + execute 级会话记忆（按 agent_id 分域）+ 命令前缀白名单。
#[derive(Default)]
pub struct ApprovalBroker {
    pending: Mutex<HashMap<String, PendingApproval>>,
    /// 观察者租约：`viewer_id → 最后一次心跳`。页面崩溃/刷新来不及 detach 时，
    /// 靠这里的时间戳与 TTL 判定「还有没有人能点审批卡」。
    leases: Mutex<HashMap<String, ViewerLease>>,
    /// 待审批表的修订号：每次增删都 +1，前端按它丢弃倒序到达的旧快照。
    revision: AtomicU64,
    /// 生产装配开启后，创建审批前必须存在新鲜租约；测试夹具保持 `Responder` 旧行为。
    lease_enforced: AtomicBool,
    /// 待审批表变化时的通知钩子（装配层接 Tauri emit）。锁外调用，允许回调读快照。
    pending_changed: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// `agent_id → 已记住的 execute 工具`。ADR-103：子代理的 AllowAlways 只对它自己
    /// 生效，不能放行父代理或兄弟代理——按 agent 分域是这条语义的唯一实现点。
    remembered_execute: Mutex<HashMap<String, HashSet<String>>>,
    /// 命令前缀白名单（ADR-007③）。来自用户 config.toml，进程内只读装载；
    /// **没有全局规则文件**，也没有任何代理可调用的写入路径（ADR-008）。
    prefix: std::sync::RwLock<PrefixAllowlist>,
    /// 本 turn 的批量确认窗口（SH-118④）：用户批准过一次的 (agent, tool)，
    /// 同一 turn 内后续调用不再逐次弹窗。turn 开始由 `begin_turn` 清空。
    batch: Mutex<Option<BatchGrant>>,
    /// 批量确认的**关闭**开关：默认 false（生产启用）。评估基线用它跑"关闭批量"的
    /// 对照组，否则量不出 SH-119 的打断次数变化（同进程内切换，不是产品开关）。
    batch_disabled: std::sync::atomic::AtomicBool,
}

/// 一条挂起的审批：等待回执的发送端 + 可序列化的身份信息。
struct PendingApproval {
    tx: tokio::sync::oneshot::Sender<ApprovalDecision>,
    view: PendingApprovalView,
}

/// 待审批请求的对外视图：刷新后的页面按它重建审批卡，横幅按它定位会话。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingApprovalView {
    pub request_id: String,
    pub session_id: String,
    pub agent_id: Option<String>,
    pub tool: String,
    pub risk: RiskLevel,
    pub args: Value,
    pub reason: Option<String>,
    pub call_id: Option<String>,
    pub task_summary: Option<String>,
    pub batch_scope: Option<String>,
    pub created_at_ms: u64,
}

/// 待审批表的完整快照：查询命令与 `approval/pending-changed` 事件共用同一形状。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingApprovalsSnapshot {
    pub approvals: Vec<PendingApprovalView>,
    pub revision: u64,
    pub heartbeat_ms: u64,
    pub ttl_ms: u64,
    /// 当前是否至少有一个新鲜租约；false = 页面心跳已中断，TTL 到点会统一拒绝。
    pub attended: bool,
}

/// 一个观察者（页面）的租约。`detached` 只表示不再续期，不立即撤销可达性：
/// 刷新会触发 detach，若立即判死，新页面还没 attach 就把上一页挂着的审批拒掉了。
#[derive(Debug, Clone, Copy)]
struct ViewerLease {
    last_seen: Instant,
    detached: bool,
}

/// 批量确认窗口的内容：身份 + 工具 + **这一次的参数指纹**。
///
/// 旧实现只记 `(agent_id, tool)`，参数一变也照样放行：批准 `git status` 会连带
/// `git push --force`（两者都是 `unified_exec`）。风险随参数变化，批量就必须跟着
/// 参数走；写入类（`WriteInWorkspace`）完全退出批量，每个补丁单独确认。
#[derive(Debug, Clone, PartialEq, Eq)]
struct BatchGrant {
    agent_id: String,
    tool: String,
    /// `batch_key` 的输出：参数规范化后的完整指纹。
    key: String,
}

impl ApprovalBroker {
    /// 生产装配入口。
    ///
    /// `SHANIU_EVAL_ABLATE_BATCH=1` 时关闭同 turn 批量确认，供评估基线跑「降噪前」
    /// 对照组；这是唯一的真实入口，不设代理可调用的开关（见 [`Self::disable_batch`]）。
    pub fn new() -> Self {
        let broker = Self::default();
        if std::env::var_os("SHANIU_EVAL_ABLATE_BATCH").is_some() {
            broker.disable_batch();
        }
        broker
    }

    /// 装配层把 config.toml 里的前缀规则装进来（空 = 全部按风险裁决）。
    pub fn set_prefix_rules(&self, rules: PrefixAllowlist) {
        *self.prefix.write().expect("prefix 锁中毒") = rules;
    }

    /// 关闭批量确认（评估对照用）。只影响 `gate` 的批量短路，不改记忆与风险评估。
    /// 正常入口是环境变量 `SHANIU_EVAL_ABLATE_BATCH`（见 [`Self::new`]）。
    pub fn disable_batch(&self) {
        self.batch_disabled
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// 生产装配启用租约判据：没有新鲜租约时，审批请求在创建前就被拒绝。
    ///
    /// 默认不启用是为了让既有测试夹具与无前端的出口保持「有人等回执」的旧语义；
    /// 生产入口（`lib.rs` setup）必须调用一次。
    pub fn enforce_lease(&self) {
        self.lease_enforced.store(true, Ordering::SeqCst);
    }

    fn lease_is_enforced(&self) -> bool {
        self.lease_enforced.load(Ordering::SeqCst)
    }

    /// 登记/刷新一个观察者（页面）。页面刷新会换新的 `viewer_id`，旧租约按 TTL 自然过期。
    pub fn attach(&self, viewer_id: &str) {
        self.attach_at(viewer_id, Instant::now());
    }

    fn attach_at(&self, viewer_id: &str, now: Instant) {
        self.leases
            .lock()
            .expect("leases 锁中毒")
            .insert(viewer_id.to_string(), ViewerLease { last_seen: now, detached: false });
    }

    /// 续约。返回 false 表示这条租约已不存在或已退租，调用方应重新 attach
    /// （覆盖宿主重启、看门狗清理、页面刷新换 id 三种情况）。
    pub fn heartbeat(&self, viewer_id: &str) -> bool {
        self.heartbeat_at(viewer_id, Instant::now())
    }

    fn heartbeat_at(&self, viewer_id: &str, now: Instant) -> bool {
        let mut leases = self.leases.lock().expect("leases 锁中毒");
        match leases.get_mut(viewer_id) {
            Some(lease) if !lease.detached => {
                lease.last_seen = now;
                true
            }
            _ => false,
        }
    }

    /// 优雅退租：只停止续期，不立刻撤销可达性。
    ///
    /// 刷新会触发 detach；若立即判死，新页面还没 attach 就把上一页挂着的审批拒掉了。
    /// 真正的 fail-closed 由 TTL 与看门狗负责。
    pub fn detach(&self, viewer_id: &str) {
        if let Some(lease) = self.leases.lock().expect("leases 锁中毒").get_mut(viewer_id) {
            lease.detached = true;
        }
    }

    fn has_live_lease(&self, now: Instant) -> bool {
        let ttl = Duration::from_millis(APPROVAL_LEASE_TTL_MS);
        self.leases
            .lock()
            .expect("leases 锁中毒")
            .values()
            .any(|lease| now.saturating_duration_since(lease.last_seen) < ttl)
    }

    fn prune_leases(&self, now: Instant) {
        let ttl = Duration::from_millis(APPROVAL_LEASE_TTL_MS);
        self.leases
            .lock()
            .expect("leases 锁中毒")
            .retain(|_, lease| now.saturating_duration_since(lease.last_seen) < ttl);
    }

    /// 装配层注入「待审批表变了」的广播钩子（Tauri emit）。回调在锁外执行。
    pub fn set_pending_changed_listener(&self, listener: Arc<dyn Fn() + Send + Sync>) {
        *self.pending_changed.lock().expect("pending_changed 锁中毒") = Some(listener);
    }

    fn notify_pending_changed(&self) {
        self.revision.fetch_add(1, Ordering::SeqCst);
        let listener = self
            .pending_changed
            .lock()
            .expect("pending_changed 锁中毒")
            .clone();
        if let Some(listener) = listener {
            listener();
        }
    }

    /// 待审批表的完整快照（查询命令与 `approval/pending-changed` 事件共用）。
    pub fn pending_snapshot(&self) -> PendingApprovalsSnapshot {
        let now = Instant::now();
        let mut approvals: Vec<PendingApprovalView> = self
            .pending
            .lock()
            .expect("pending 锁中毒")
            .values()
            .map(|pending| pending.view.clone())
            .collect();
        approvals.sort_by(|left, right| {
            left.created_at_ms
                .cmp(&right.created_at_ms)
                .then_with(|| left.request_id.cmp(&right.request_id))
        });
        PendingApprovalsSnapshot {
            approvals,
            revision: self.revision.load(Ordering::SeqCst),
            heartbeat_ms: APPROVAL_LEASE_HEARTBEAT_MS,
            ttl_ms: APPROVAL_LEASE_TTL_MS,
            attended: self.has_live_lease(now),
        }
    }

    /// 租约清扫：看门狗每秒调用；返回被裁决的请求，供出口补发 `exec_approval_resolved`。
    ///
    /// 只要还有一条新鲜租约就完全不动 pending——「有人看着但暂时没点」是正常场景，
    /// 不能按空闲时间拒绝。没有活租约时，挂起的请求已经无人可答，全部按 Deny 收口。
    pub fn sweep_unattended(&self, now: Instant) -> Vec<PendingApprovalView> {
        self.prune_leases(now);
        if self.has_live_lease(now) {
            return Vec::new();
        }
        let expired: Vec<PendingApproval> = {
            let mut pending = self.pending.lock().expect("pending 锁中毒");
            if pending.is_empty() {
                return Vec::new();
            }
            pending.drain().map(|(_, pending)| pending).collect()
        };
        let mut views = Vec::with_capacity(expired.len());
        for pending in expired {
            let _ = pending.tx.send(ApprovalDecision::Deny);
            tracing::warn!(
                request_id = %pending.view.request_id,
                tool = %pending.view.tool,
                session_id = %pending.view.session_id,
                "审批租约已过期（最近没有心跳），按「没人可答」fail-closed 拒绝"
            );
            views.push(pending.view);
        }
        self.notify_pending_changed();
        views
    }

    /// 工具执行前的门禁：白名单 → 裁决 → 必要时发审批请求并等待回执。
    ///
    /// 白名单先于风险分级：被收窄掉的工具连询问都不问（问一次就执行一次
    /// 等于把"只降不升"变成了"可以申请提权"，ADR-103 不允许）。
    pub async fn gate(
        &self,
        tool: &str,
        args: &Value,
        call_id: &str,
        policy: policy::SessionPolicy,
        subject: GateSubject<'_>,
        env: GateEnv<'_>,
    ) -> GateOutcome {
        if env.cancel.is_cancelled() {
            return GateOutcome::Cancelled;
        }
        if let Some(allowlist) = subject.allowed_tools {
            if !allowlist.contains(tool) {
                return GateOutcome::Forbidden;
            }
        }
        // 用户批准的工具级档位优先；MCP 未知工具没有覆盖时 risk_of 落 destructive。
        let risk = subject.risk_override.unwrap_or_else(|| risk_of(tool));
        // 命令前缀白名单（ADR-007③）：先由沙箱划边界——只读沙箱连前缀都不看，
        // 否则"命中白名单"会成为绕过只读约束的路径。命中 → 放行 + 写审计（ADR-009②）。
        if risk == RiskLevel::Execute && policy.sandbox != SandboxMode::ReadOnly {
            if let Some(command) = args.get("command").and_then(Value::as_str) {
                let hit = self
                    .prefix
                    .read()
                    .expect("prefix 锁中毒")
                    .match_command(command)
                    .map(|rule| rule.id.clone());
                if let Some(rule_id) = hit {
                    let _ = env.sink.emit(&EventMsg::AllowlistHit {
                        tool: tool.to_string(),
                        command: command.to_string(),
                        rule_id,
                    });
                    return GateOutcome::Allowed;
                }
            }
        }
        let remembered = self
            .remembered_execute
            .lock()
            .expect("remembered 锁中毒")
            .get(subject.agent_id)
            .is_some_and(|tools| tools.contains(tool));
        // 同一 turn 内的批量确认（SH-118④，A0 后收紧）：作用域 = 一个 turn + 一个代理
        // + 一个工具 + **完全相同的参数**。批准 `git status` 不再连带 `git push --force`；
        // 写入类连窗口都不开（`batch_key` 返回 None），每个补丁单独确认。
        let batch_token = Self::batch_key(risk, args);
        let batched = !self
            .batch_disabled
            .load(std::sync::atomic::Ordering::SeqCst)
            && batch_token.as_ref().is_some_and(|key| {
                self.batch
                    .lock()
                    .expect("batch 锁中毒")
                    .as_ref()
                    .is_some_and(|grant| {
                        grant.agent_id == subject.agent_id
                            && grant.tool == tool
                            && &grant.key == key
                    })
            });
        match policy::decide(risk, policy.approval, policy.sandbox, remembered) {
            Decision::Allow => {
                // 计划评审在预授权档被放行时必须留痕：只写 `tracing::warn` 用户事后翻不到。
                // 复用 `AllowlistHit`（事件流已有、不扩协议），rule_id 明说「预授权」。
                if risk == RiskLevel::PlanReview {
                    // 快照已归一化（`full_access` 的沙箱是 danger），直接记录生效两维。
                    let _ = env.sink.emit(&EventMsg::AllowlistHit {
                        tool: tool.to_string(),
                        command: format!(
                            "sandbox={} approval={}",
                            policy.sandbox.as_str(),
                            policy.approval.as_str()
                        ),
                        rule_id: "plan-review:pre-authorized".to_string(),
                    });
                }
                GateOutcome::Allowed
            }
            Decision::Deny => GateOutcome::Forbidden,
            Decision::Prompt if batched => GateOutcome::Allowed,
            Decision::Prompt => {
                let base = prompt_reason(risk);
                // 执行类在写约束档必须把沙箱边界写在卡上：审批只决定「能不能跑」，
                // 不放宽写范围，用户点「允许」后发现写外部被拒会以为按钮没用。
                let reason = if risk == RiskLevel::Execute && policy.sandbox == SandboxMode::WorkspaceWrite {
                    format!("{base}。{WRITE_SCOPE_NOTE}")
                } else {
                    base.to_string()
                };
                self.request_approval(
                    tool,
                    args,
                    call_id,
                    ApprovalPrompt { risk, reason: Some(&reason), remember: true, escalation: false },
                    subject,
                    env,
                )
                .await
            }
        }
    }

    /// 新 turn 开始：清空批量确认窗口（SH-118④）。上一 turn 的批准不延用到下一 turn，
    /// 否则"同 turn 批量"会退化成"整个会话只问一次"。
    pub fn begin_turn(&self, agent_id: &str) {
        let mut batch = self.batch.lock().expect("batch 锁中毒");
        if batch.as_ref().is_some_and(|grant| grant.agent_id == agent_id) {
            *batch = None;
        }
    }

    /// `on-failure` 提权询问（SH-120 / ADR-007②）：沙箱拒绝某条命令后，问用户
    /// 是否**只这一次**以 full-access 重跑。`AllowAlways` 不写会话记忆——
    /// 记忆按工具粒度记的是"要不要审批"，而这里要放的是"沙箱边界"，两者不能混。
    pub async fn request_escalation(
        &self,
        tool: &str,
        args: &Value,
        call_id: &str,
        sandbox_reason: &str,
        subject: GateSubject<'_>,
        env: GateEnv<'_>,
    ) -> GateOutcome {
        let reason = format!("沙箱拒绝了这条命令（{sandbox_reason}）；是否仅本次以 full-access 重试？");
        self.request_approval(
            tool,
            args,
            call_id,
            ApprovalPrompt { risk: RiskLevel::Execute, reason: Some(&reason), remember: false, escalation: true },
            subject,
            env,
        )
        .await
    }

    /// 发审批请求并挂起等待；取消时撤单。
    ///
    /// `escalation` 只影响请求 id 与批量说明：提权询问要能和普通审批区分开，
    /// 但卡片详情仍按原始 `call_id` 去聊天快照取参数（否则提权卡只剩一句话）。
    async fn request_approval(
        &self,
        tool: &str,
        args: &Value,
        call_id: &str,
        prompt: ApprovalPrompt<'_>,
        subject: GateSubject<'_>,
        env: GateEnv<'_>,
    ) -> GateOutcome {
        let ApprovalPrompt { risk, reason, remember, escalation } = prompt;
        let request_id = if escalation {
            format!("appr-{call_id}-escalate")
        } else {
            format!("appr-{call_id}")
        };
        // 可达性判据（生产装配）：没有新鲜租约 = 页面崩溃/刷新后没接上/从未加载，
        // 此刻建 pending 只会挂到租约过期。创建前就 fail-closed，不占表、不 emit。
        // 测试夹具默认不启用该判据，`responder()` 仍是结构性的第二道预检。
        if self.lease_is_enforced() && !self.has_live_lease(Instant::now()) {
            tracing::warn!(
                request_id = %request_id,
                tool = %tool,
                session_id = %env.sink.session_id(),
                agent = %subject.agent_id,
                "审批请求到达但最近没有心跳（无活跃租约），按 fail-closed 拒绝，不挂起等待"
            );
            return GateOutcome::Denied;
        }
        // 照 dsh 的 `unavailable` 语义（dsh `interaction/user-approval/src/index.ts:306-317`：
        // 审批 waterfall 没有应答者时默认返回 `'unavailable'`，即刻 fail closed）。
        // 这里问的是「此刻有没有人能回执」，不是「人有没有在看」；生产上「人在不在看」
        // 由租约 TTL 判定：有租约时用户离开工位是正常场景，不按空闲时间拒绝。
        let responder = env.sink.responder();
        if responder != ResponderState::Ready {
            tracing::warn!(
                "审批请求没有可回执的前端（request_id={request_id}，tool={tool}，agent={}，原因={}）：\
                 无人能批准则不能放行，按 Denied 裁决，不挂起等待回执",
                subject.agent_id,
                responder.reason()
            );
            return GateOutcome::Denied;
        }
        let batch_scope = (remember
            && !self.batch_disabled.load(std::sync::atomic::Ordering::SeqCst)
            && Self::batch_key(risk, args).is_some())
        .then(|| "same-args".to_string());
        let (tx, rx) = tokio::sync::oneshot::channel::<ApprovalDecision>();
        self.pending.lock().expect("pending 锁中毒").insert(
            request_id.clone(),
            PendingApproval {
                tx,
                view: PendingApprovalView {
                    request_id: request_id.clone(),
                    session_id: env.sink.session_id().to_string(),
                    agent_id: Some(subject.agent_id.to_string()),
                    tool: tool.to_string(),
                    risk,
                    args: args.clone(),
                    reason: reason.map(str::to_string),
                    call_id: Some(call_id.to_string()),
                    task_summary: subject.task_summary.map(str::to_string),
                    batch_scope: batch_scope.clone(),
                    created_at_ms: now_ms(),
                },
            },
        );
        if let Err(error) = env.sink.emit(&EventMsg::ExecApprovalRequest {
            request_id: request_id.clone(),
            tool: tool.to_string(),
            args: args.clone(),
            risk,
            // ADR-103 审批冒泡：请求自带来源代理与任务摘要，前端不必从泳道
            // 元数据反推"是谁在要权限"——反推在泳道未建立时必然失败。
            agent_id: Some(subject.agent_id.to_string()),
            task_summary: subject.task_summary.map(str::to_string),
            // SH-118⑤：审批上下文要带风险原因，用户才知道自己在批什么。
            reason: reason.map(str::to_string),
            // 卡片的详情区按它取完整参数（additive）。
            call_id: Some(call_id.to_string()),
            // 批准后批量会覆盖多远：只有 Execute、参数指纹可算、且批量没被关掉时才开窗口。
            // 写入类与提权询问永远是 None——批准一次就只算一次。
            batch_scope,
        }) {
            // M2：卡片送不到 = 用户不可能看到、也就不可能批准 = 唯一安全的裁决是拒绝。
            // 不能等：没有回执来源，下面的 `select!` 会永久挂起整个回合。
            // 这里不按租约超时处理：要区分的是「卡片根本没送到」（立刻拒）与
            // 「卡片送到了一个暂时没人点的页面」（交给租约 TTL 收口）。
            self.pending.lock().expect("pending 锁中毒").remove(&request_id);
            self.notify_pending_changed();
            tracing::warn!(
                "审批卡未能送达（request_id={request_id}，tool={tool}，agent={}）：{error}；\
                 卡片送不到则用户无法批准，按 Denied 裁决，不挂起等待回执",
                subject.agent_id
            );
            return GateOutcome::Denied;
        }
        self.notify_pending_changed();
        let outcome = tokio::select! {
            resp = rx => match resp {
                // 允许一次 = 只放行这一次，**不记忆**。ADR-003 的"本会话记住"只由
                // AllowAlways 触发；把 Allow 也写进 remembered_execute 会让用户点一次
                // 「允许一次」后，同进程后续 execute 全部静默放行（真机复测发现）。
                Ok(ApprovalDecision::Allow) => {
                    if remember {
                        self.grant_batch(tool, args, risk, subject.agent_id);
                    }
                    GateOutcome::Allowed
                }
                Ok(d @ ApprovalDecision::AllowAlways) => {
                    if remember {
                        self.remember_if_execute(tool, risk, subject.agent_id);
                        self.grant_batch(tool, args, risk, subject.agent_id);
                    }
                    GateOutcome::from(d)
                }
                Ok(ApprovalDecision::Deny) => GateOutcome::Denied,
                Err(_) => GateOutcome::Cancelled, // 发送端已被撤单
            },
            _ = env.cancel.cancelled() => GateOutcome::Cancelled,
        };
        if outcome != GateOutcome::Allowed {
            // 未放行的请求撤单（已 resolve / 已被看门狗清掉的 remove 无副作用）
            let removed = self
                .pending
                .lock()
                .expect("pending 锁中毒")
                .remove(&request_id)
                .is_some();
            if removed {
                self.notify_pending_changed();
            }
        }
        outcome
    }

    fn remember_if_execute(&self, tool: &str, risk: RiskLevel, agent_id: &str) {
        if risk == RiskLevel::Execute {
            self.remembered_execute
                .lock()
                .expect("remembered 锁中毒")
                .entry(agent_id.to_string())
                .or_default()
                .insert(tool.to_string());
        }
    }

    /// 用户已批准 → 本 turn 内**参数完全相同**的同类调用免问。
    /// 非 Execute 一律不开窗口：写入类每次必问（D3 拍板），Destructive 本来每次必问。
    fn grant_batch(&self, tool: &str, args: &Value, risk: RiskLevel, agent_id: &str) {
        let Some(key) = Self::batch_key(risk, args) else {
            return;
        };
        *self.batch.lock().expect("batch 锁中毒") = Some(BatchGrant {
            agent_id: agent_id.to_string(),
            tool: tool.to_string(),
            key,
        });
    }

    /// 批量的参数指纹：**整份参数完全一致才算同一类**。
    ///
    /// 为什么不是「可执行文件 + 子命令」：`git commit` 与 `git commit --amend`、
    /// `docker run` 与 `docker run --privileged` 会落在同一个粗粒度键上——风险明明随
    /// 旗标变了却仍被放行。既然没法穷举"哪个旗标改变风险"，就只能退到最保守的判据：
    /// 参数完全相同。代价是重复度下降、多弹卡，这是用户明确选过的交换。
    ///
    /// 返回 None = 不批量：非 Execute（写入类每次必问）、参数不是非空对象、
    /// 或 `command` 存在却不是非空字符串（拿不到"这一次到底是什么"就没法比）。
    fn batch_key(risk: RiskLevel, args: &Value) -> Option<String> {
        if risk != RiskLevel::Execute {
            return None;
        }
        let object = args.as_object()?;
        if object.is_empty() {
            return None;
        }
        if let Some(command) = object.get("command") {
            match command.as_str() {
                Some(text) if !text.trim().is_empty() => {}
                _ => return None,
            }
        }
        Some(Self::canonical_json(args))
    }

    /// 键顺序无关的紧凑 JSON：同一份参数不管字段顺序如何都得到同一个指纹。
    fn canonical_json(value: &Value) -> String {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort_unstable();
                let body = keys
                    .into_iter()
                    .map(|key| {
                        let quoted = Value::String(key.clone()).to_string();
                        format!("{quoted}:{}", Self::canonical_json(&map[key]))
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                format!("{{{body}}}")
            }
            Value::Array(items) => {
                let body = items
                    .iter()
                    .map(Self::canonical_json)
                    .collect::<Vec<_>>()
                    .join(",");
                format!("[{body}]")
            }
            other => other.to_string(),
        }
    }

    /// 前端回执入口（approve 命令）；返回是否命中 pending。
    ///
    /// 命中 = 这个 id 在表里且被本次裁决移除。发送端失败（等待方已取消）仍算命中：
    /// 请求确实被消费了，调用方不需要把它当成「过期 id」重试。
    pub fn resolve(&self, request_id: &str, decision: ApprovalDecision) -> bool {
        let removed = self
            .pending
            .lock()
            .expect("pending 锁中毒")
            .remove(request_id);
        match removed {
            Some(pending) => {
                let _ = pending.tx.send(decision);
                self.notify_pending_changed();
                true
            }
            None => false,
        }
    }

    /// 是否有待审批请求（测试/前端状态查询）。
    pub fn has_pending(&self, request_id: &str) -> bool {
        self.pending.lock().expect("pending 锁中毒").contains_key(request_id)
    }

    /// 当前待审批请求数（退出前“还有几件事”用）。
    pub fn pending_count(&self) -> usize {
        self.pending.lock().expect("pending 锁中毒").len()
    }

    /// 撤掉所有待审批请求（退出路径）。
    ///
    /// 直接 drop 发送端：等待方 `rx` 收到 `Err`，按 `GateOutcome::Cancelled` 处理——
    /// 这与“用户取消任务”是同一条路径，不是“审批被拒绝”。返回值仅用于日志。
    pub fn cancel_all(&self) -> usize {
        let count = {
            let mut guard = self.pending.lock().expect("pending 锁中毒");
            let count = guard.len();
            guard.clear();
            count
        };
        if count > 0 {
            self.notify_pending_changed();
        }
        count
    }
}

impl From<ApprovalDecision> for GateOutcome {
    fn from(d: ApprovalDecision) -> Self {
        match d {
            ApprovalDecision::Allow | ApprovalDecision::AllowAlways => GateOutcome::Allowed,
            ApprovalDecision::Deny => GateOutcome::Denied,
        }
    }
}

/// 审批卡上的风险原因（SH-118⑤）：用户要知道自己在批什么，而不是只看到一个工具名。
fn prompt_reason(risk: RiskLevel) -> &'static str {
    match risk {
        RiskLevel::Read => "只读工具本应直接放行；出现此提示说明风险档被用户覆盖",
        RiskLevel::WriteInWorkspace => "写入工作区需要人工确认（当前审批策略 on_request）",
        RiskLevel::Execute => "执行命令需要人工确认（当前审批策略 on_request）",
        RiskLevel::Destructive => "破坏性操作每次必问，不会被会话记忆放行",
        RiskLevel::PlanReview => "计划评审：退出计划态需要用户确认",
    }
}

/// 写约束档下「允许 ≠ 可写外部」的说明，附在执行类审批卡的原因后面。
///
/// 为什么是说明而不是放宽：写范围不是审批层能改的——受限令牌的 restricting SID 与
/// allow ACE 由 `sandbox::prepare_write_restricted` 按工作区根一次性算好
/// （工作区 / TEMP / 工具链目录），本模块按 FF-1 也不允许依赖 sandbox。
/// 要「只放宽这一条路径」，只能从命令行文本里猜目标路径，而命令可以在运行时拼路径
/// （这正是我们拒绝解析重定向的同一理由），猜不准就又变成一次假承诺。
const WRITE_SCOPE_NOTE: &str = "允许只放行执行，不会放宽沙箱：写入工作区、TEMP、工具链目录之外的路径会被拒绝\
（读取不受影响）；需要写到外部时请改到工作区内，或切换访问模式";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ApprovalMode, SandboxMode};
    use crate::events::{NullEventSink, ResponderState};
    use std::sync::{Arc, Mutex};

    fn sink() -> Arc<dyn EventSink> {
        Arc::new(NullEventSink)
    }

    fn args() -> Value {
        serde_json::json!({"command": "echo x"})
    }

    /// 记录发出的所有事件：前缀放行审计与"never 不发审批卡"都靠它取证。
    #[derive(Default)]
    struct CaptureSink {
        events: Mutex<Vec<EventMsg>>,
    }

    impl EventSink for CaptureSink {
        fn emit(&self, msg: &EventMsg) -> Result<(), String> {
            self.events.lock().unwrap().push(msg.clone());
            Ok(())
        }

        fn session_id(&self) -> &str {
            "sess-capture"
        }
    }

    impl CaptureSink {
        fn take(&self) -> Vec<EventMsg> {
            std::mem::take(&mut *self.events.lock().unwrap())
        }
    }

    /// tracing 测试用捕获器：断言「卡片没送到」这件事在日志里留了可定位的记录。
    #[derive(Clone, Default)]
    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    impl LogCapture {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
        }
    }

    impl std::io::Write for LogCapture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
        type Writer = LogCapture;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// M2：审批卡 emit 失败（前端窗口已销毁 / 通道断开）→ 必须立即 Denied，不能挂起。
    ///
    /// 反向对照：把 `request_approval` 里的 emit 失败分支去掉（回到 `let _ =`），
    /// 本用例会卡在 2s 超时上——没有人能替一个没送到的卡片回执。
    #[tokio::test]
    async fn approval_request_is_denied_when_card_cannot_be_delivered() {
        struct FailingSink;
        impl EventSink for FailingSink {
            fn emit(&self, _msg: &EventMsg) -> Result<(), String> {
                Err("前端通道已断开".into())
            }
        }

        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::WARN)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let broker = ApprovalBroker::new();
        let cancel = CancellationToken::new();
        let sink: Arc<dyn EventSink> = Arc::new(FailingSink);
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            broker.gate(
                "unified_exec",
                &args(),
                "c-fail",
                policy::SessionPolicy {
                    approval: ApprovalMode::OnRequest,
                    sandbox: SandboxMode::WorkspaceWrite,
                },
                GateSubject::root("root"),
                GateEnv { sink: &sink, cancel: &cancel },
            ),
        )
        .await
        .expect("卡片送不到时必须立即裁决；旧实现会挂在这里直到超时");
        assert_eq!(out, GateOutcome::Denied, "送不到 = 用户不可能批准 = 拒绝");
        assert!(
            !broker.has_pending("appr-c-fail"),
            "送不出的请求不得留在 pending 表里"
        );

        let logged = capture.text();
        assert!(logged.contains("审批卡未能送达"), "日志缺少可定位记录: {logged}");
        assert!(
            logged.contains("appr-c-fail") && logged.contains("unified_exec"),
            "日志要能定位到具体请求: {logged}"
        );
    }

    /// dsh 的 `unavailable` 语义（`user-approval/src/index.ts:306-317`）：没有应答者
    /// 就立刻 fail closed，不引入超时。SHANIU 的对应事实是「此刻没有能回执的前端」。
    ///
    /// 反向对照：把 `request_approval` 顶部的探测删掉，本用例会卡在 200ms 超时上——
    /// 没有前端时 `rx` 永远不会来，`env.cancel` 也不会自己动。
    #[tokio::test]
    async fn approval_is_denied_immediately_when_nobody_can_answer() {
        struct NoResponderSink;
        impl EventSink for NoResponderSink {
            fn emit(&self, _msg: &EventMsg) -> Result<(), String> {
                Ok(())
            }

            fn responder(&self) -> ResponderState {
                ResponderState::NoWebview
            }
        }

        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::WARN)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let broker = ApprovalBroker::new();
        let cancel = CancellationToken::new();
        let sink: Arc<dyn EventSink> = Arc::new(NoResponderSink);
        let started = std::time::Instant::now();
        let out = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            broker.gate(
                "unified_exec",
                &args(),
                "c-nobody",
                policy::SessionPolicy {
                    approval: ApprovalMode::OnRequest,
                    sandbox: SandboxMode::WorkspaceWrite,
                },
                GateSubject::root("root"),
                GateEnv { sink: &sink, cancel: &cancel },
            ),
        )
        .await
        .expect("没有可回执方时必须立即裁决；挂到用例超时即失败");
        assert_eq!(out, GateOutcome::Denied, "没人能回执 = 不能批准 = 拒绝");
        assert!(started.elapsed() < std::time::Duration::from_millis(200));
        assert!(
            !broker.has_pending("appr-c-nobody"),
            "不可达的请求不得留在 pending 表里"
        );

        let logged = capture.text();
        assert!(logged.contains("没有可回执的前端"), "日志缺少可定位记录: {logged}");
        assert!(
            logged.contains("appr-c-nobody") && logged.contains("unified_exec"),
            "日志要能定位到具体请求: {logged}"
        );
    }

    /// 有人能回执时行为一字不变：照旧等，300ms 后的回执能正常放行（不是「等到超时」）。
    #[tokio::test]
    async fn approval_waits_when_responder_is_ready_and_allows_after_delay() {
        let broker = Arc::new(ApprovalBroker::new());
        let capture = Arc::new(CaptureSink::default());
        let sink: Arc<dyn EventSink> = capture.clone();
        let cancel = CancellationToken::new();

        let b = broker.clone();
        let s = sink.clone();
        let c = cancel.clone();
        let waiting = tokio::spawn(async move {
            b.gate(
                "unified_exec",
                &args(),
                "c-late",
                policy::SessionPolicy {
                    approval: ApprovalMode::OnRequest,
                    sandbox: SandboxMode::WorkspaceWrite,
                },
                GateSubject::root("root"),
                GateEnv { sink: &s, cancel: &c },
            )
            .await
        });
        for _ in 0..100 {
            if broker.has_pending("appr-c-late") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(broker.has_pending("appr-c-late"), "卡片应已登记，等用户回执");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(broker.resolve("appr-c-late", ApprovalDecision::Allow));
        let out = tokio::time::timeout(std::time::Duration::from_millis(200), waiting)
            .await
            .expect("回执到达后应立刻结束等待")
            .unwrap();
        assert_eq!(out, GateOutcome::Allowed, "迟到的回执仍要能放行");
    }

    /// SH-120 / ADR-007③④：命中前缀白名单 → 放行 + 审计事件（含规则 id）；
    /// 带 shell 组合的同前缀命令不得被放行。
    #[tokio::test]
    async fn prefix_hit_allows_with_audit_and_composition_is_not_allowed() {
        let broker = ApprovalBroker::new();
        let (rules, rejected) = PrefixAllowlist::from_config(&["cargo test".to_string()]);
        assert!(rejected.is_empty());
        broker.set_prefix_rules(rules);
        let capture = Arc::new(CaptureSink::default());
        let sink: Arc<dyn EventSink> = capture.clone();
        let cancel = CancellationToken::new();

        let out = broker
            .gate(
                "unified_exec",
                &serde_json::json!({"command": "cargo test --lib"}),
                "c-prefix",
                policy::SessionPolicy { approval: ApprovalMode::OnRequest, sandbox: SandboxMode::WorkspaceWrite },
                GateSubject::root("root"),
                GateEnv { sink: &sink, cancel: &cancel },
            )
            .await;
        assert_eq!(out, GateOutcome::Allowed, "命中前缀应直接放行");
        let events = capture.take();
        assert!(
            events.iter().any(|e| matches!(
                e,
                EventMsg::AllowlistHit { rule_id, command, .. }
                    if rule_id == "cargo test" && command == "cargo test --lib"
            )),
            "放行必须留审计: {events:?}"
        );

        // 组合命令不能借前缀放行：不能出现审批卡也不应放行（这里会挂起等审批，
        // 因此用一个必然被拒绝的沙箱档先验证"不放行"路径更直接）
        let readonly = broker
            .gate(
                "unified_exec",
                &serde_json::json!({"command": "cargo test"}),
                "c-prefix-ro",
                policy::SessionPolicy { approval: ApprovalMode::OnRequest, sandbox: SandboxMode::ReadOnly },
                GateSubject::root("root"),
                GateEnv { sink: &sink, cancel: &cancel },
            )
            .await;
        assert_eq!(readonly, GateOutcome::Forbidden, "只读沙箱下前缀不得绕过执行禁令");

        let composed = broker
            .gate(
                "unified_exec",
                &serde_json::json!({"command": "cargo test && del important"}),
                "c-composed",
                policy::SessionPolicy { approval: ApprovalMode::Never, sandbox: SandboxMode::WorkspaceWrite },
                GateSubject::root("root"),
                GateEnv { sink: &sink, cancel: &cancel },
            )
            .await;
        // never 档不弹窗：组合命令没有被白名单放行，就按 never 的沙箱内策略放行——
        // 关键是它**没有**产生 AllowlistHit（审计里不能记成"白名单放行"）
        assert_eq!(composed, GateOutcome::Allowed);
        let events = capture.take();
        assert!(
            !events.iter().any(|e| matches!(e, EventMsg::AllowlistHit { .. })),
            "组合命令不得记成白名单放行: {events:?}"
        );
    }

    /// SH-118③：`AllowAlways` 只作用于批准它的那个代理——子代理记住后，
    /// 父代理（以及兄弟）仍必须被拦。ADR-103 的"父→子单向、子→父不回灌"。
    #[tokio::test]
    async fn allow_always_is_scoped_to_the_approving_agent() {
        let broker = Arc::new(ApprovalBroker::new());
        let capture = Arc::new(CaptureSink::default());
        let sink: Arc<dyn EventSink> = capture.clone();
        let cancel = CancellationToken::new();
        let session = policy::SessionPolicy {
            approval: ApprovalMode::OnRequest,
            sandbox: SandboxMode::WorkspaceWrite,
        };

        // 子代理批准 AllowAlways
        let b = broker.clone();
        let s = sink.clone();
        let c = cancel.clone();
        let child = tokio::spawn(async move {
            b.gate(
                "unified_exec",
                &args(),
                "child-1",
                session,
                GateSubject {
                    agent_id: "child",
                    task_summary: Some("子任务"),
                    allowed_tools: None,
                    risk_override: None,
                },
                GateEnv { sink: &s, cancel: &c },
            )
            .await
        });
        for _ in 0..200 {
            if broker.has_pending("appr-child-1") {
                assert!(broker.resolve("appr-child-1", ApprovalDecision::AllowAlways));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(child.await.unwrap(), GateOutcome::Allowed);

        // 父代理再调同一工具：仍要询问（记忆与批量窗口都不跨代理）
        capture.take();
        let b = broker.clone();
        let s = sink.clone();
        let c = cancel.clone();
        let root = tokio::spawn(async move {
            b.gate(
                "unified_exec",
                &args(),
                "root-1",
                session,
                GateSubject::root("root"),
                GateEnv { sink: &s, cancel: &c },
            )
            .await
        });
        for _ in 0..200 {
            if broker.has_pending("appr-root-1") {
                assert!(broker.resolve("appr-root-1", ApprovalDecision::Deny));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(root.await.unwrap(), GateOutcome::Denied, "父代理不得继承子代理的批准");
    }

    /// SH-120 / ADR-007②：`never` 档下执行不回审批卡（不弹窗），破坏性工具直接拒绝。
    #[tokio::test]
    async fn never_mode_never_emits_approval_request() {
        let broker = ApprovalBroker::new();
        let capture = Arc::new(CaptureSink::default());
        let sink: Arc<dyn EventSink> = capture.clone();
        let cancel = CancellationToken::new();

        let out = broker
            .gate(
                "unified_exec",
                &args(),
                "c-never",
                policy::SessionPolicy { approval: ApprovalMode::Never, sandbox: SandboxMode::WorkspaceWrite },
                GateSubject::root("root"),
                GateEnv { sink: &sink, cancel: &cancel },
            )
            .await;
        assert_eq!(out, GateOutcome::Allowed);
        assert!(capture.take().is_empty(), "never 档不得发任何审批事件");

        // 未知工具 = destructive：never 档直接拒绝，不询问
        let out = broker
            .gate(
                "mcp__unknown__tool",
                &serde_json::json!({}),
                "c-never-destructive",
                policy::SessionPolicy { approval: ApprovalMode::Never, sandbox: SandboxMode::WorkspaceWrite },
                GateSubject::root("root"),
                GateEnv { sink: &sink, cancel: &cancel },
            )
            .await;
        assert_eq!(out, GateOutcome::Forbidden);
        assert!(capture.take().is_empty(), "拒绝也不应产生审批卡");
    }

    #[tokio::test]
    async fn read_tool_passes_without_request() {
        let broker = ApprovalBroker::new();
        let cancel = CancellationToken::new();
        let out = broker
            .gate("read_file", &serde_json::json!({}), "c0", policy::SessionPolicy { approval: ApprovalMode::OnRequest, sandbox: SandboxMode::ReadOnly }, GateSubject::root("root"), GateEnv { sink: &sink(), cancel: &cancel })
            .await;
        assert_eq!(out, GateOutcome::Allowed);
    }

    #[tokio::test]
    async fn write_denied_in_readonly_sandbox() {
        let broker = ApprovalBroker::new();
        let cancel = CancellationToken::new();
        let out = broker
            .gate("apply_patch", &serde_json::json!({}), "c1", policy::SessionPolicy { approval: ApprovalMode::OnRequest, sandbox: SandboxMode::ReadOnly }, GateSubject::root("root"), GateEnv { sink: &sink(), cancel: &cancel })
            .await;
        assert_eq!(out, GateOutcome::Forbidden, "沙箱拒绝属于策略拒绝，不是用户拒绝");
    }

    /// 回归（真机现象）：用户允许一条要写工作区外的命令 → OS 报「拒绝访问」，
    /// 卡上却没有任何说明，看起来像按钮失效。卡必须先说清允许不放宽沙箱。
    #[tokio::test]
    async fn execute_prompt_states_write_scope_is_not_widened() {
        let broker = Arc::new(ApprovalBroker::new());
        let cancel = CancellationToken::new();
        let capture = Arc::new(CaptureSink::default());
        let sink: Arc<dyn EventSink> = capture.clone();
        let (b, s, c) = (broker.clone(), sink, cancel.clone());
        let gate = tokio::spawn(async move {
            b.gate(
                "unified_exec",
                &serde_json::json!({"command": "echo hi > C:\\outside\\x.txt"}),
                "c-scope",
                policy::SessionPolicy { approval: ApprovalMode::OnRequest, sandbox: SandboxMode::WorkspaceWrite },
                GateSubject::root("root"),
                GateEnv { sink: &s, cancel: &c },
            )
            .await
        });
        let mut prompted = false;
        for _ in 0..200 {
            if broker.has_pending("appr-c-scope") {
                prompted = true;
                assert!(broker.resolve("appr-c-scope", ApprovalDecision::Deny));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(prompted, "执行类在 on_request 下必须弹卡");
        let _ = gate.await.unwrap();

        let reason = capture
            .take()
            .into_iter()
            .find_map(|event| match event {
                EventMsg::ExecApprovalRequest { reason, .. } => reason,
                _ => None,
            })
            .expect("审批卡必须带原因");
        assert!(reason.contains("不会放宽沙箱"), "卡上必须说明允许不放宽沙箱: {reason}");
        assert!(reason.contains("会被拒绝"), "卡上必须说明外部写入会被拒: {reason}");
        assert!(reason.contains("工作区") && reason.contains("TEMP"), "卡上必须写出可写范围: {reason}");
    }

    /// 非执行类（破坏性）审批卡保持原口径：沙箱写范围与它无关，追加只会变成噪音。
    #[tokio::test]
    async fn destructive_prompt_keeps_plain_reason() {
        let broker = Arc::new(ApprovalBroker::new());
        let cancel = CancellationToken::new();
        let capture = Arc::new(CaptureSink::default());
        let sink: Arc<dyn EventSink> = capture.clone();
        let (b, s, c) = (broker.clone(), sink, cancel.clone());
        let gate = tokio::spawn(async move {
            // 未登记的工具落 destructive 档，on_request 下必问。
            b.gate(
                "some_unknown_tool",
                &serde_json::json!({}),
                "c-plain",
                policy::SessionPolicy { approval: ApprovalMode::OnRequest, sandbox: SandboxMode::WorkspaceWrite },
                GateSubject::root("root"),
                GateEnv { sink: &s, cancel: &c },
            )
            .await
        });
        for _ in 0..200 {
            if broker.has_pending("appr-c-plain") {
                assert!(broker.resolve("appr-c-plain", ApprovalDecision::Deny));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let _ = gate.await.unwrap();
        let reason = capture
            .take()
            .into_iter()
            .find_map(|event| match event {
                EventMsg::ExecApprovalRequest { reason, .. } => reason,
                _ => None,
            })
            .expect("审批卡必须带原因");
        assert_eq!(reason, prompt_reason(RiskLevel::Destructive));
    }

    #[tokio::test]
    async fn prompt_resolves_to_deny_then_allows_after_remember() {
        let broker = Arc::new(ApprovalBroker::new());
        let cancel = CancellationToken::new();
        let sink = sink();

        // execute → Prompt：后台 resolve 为 deny
        let b = broker.clone();
        let s = sink.clone();
        let c = cancel.clone();
        let gate1 = tokio::spawn(async move {
            b.gate("unified_exec", &args(), "c2", policy::SessionPolicy { approval: ApprovalMode::OnRequest, sandbox: SandboxMode::WorkspaceWrite }, GateSubject::root("root"), GateEnv { sink: &s, cancel: &c }).await
        });
        // 等待请求挂起后回执 deny
        for _ in 0..200 {
            if broker.has_pending("appr-c2") {
                assert!(broker.resolve("appr-c2", ApprovalDecision::Deny));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(gate1.await.unwrap(), GateOutcome::Denied);

        // 再问一次 → resolve allow_always → 本会话记忆，后续直接放行
        let b = broker.clone();
        let s2 = sink.clone();
        let c2 = cancel.clone();
        let gate2 = tokio::spawn(async move {
            b.gate("unified_exec", &args(), "c3", policy::SessionPolicy { approval: ApprovalMode::OnRequest, sandbox: SandboxMode::WorkspaceWrite }, GateSubject::root("root"), GateEnv { sink: &s2, cancel: &c2 }).await
        });
        for _ in 0..200 {
            if broker.has_pending("appr-c3") {
                assert!(broker.resolve("appr-c3", ApprovalDecision::AllowAlways));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(gate2.await.unwrap(), GateOutcome::Allowed);

        // 记忆生效：不再询问
        let out = broker
            .gate("unified_exec", &args(), "c4", policy::SessionPolicy { approval: ApprovalMode::OnRequest, sandbox: SandboxMode::WorkspaceWrite }, GateSubject::root("root"), GateEnv { sink: &sink, cancel: &cancel })
            .await;
        assert_eq!(out, GateOutcome::Allowed);
    }

    #[tokio::test]
    async fn allow_once_does_not_remember_execute() {
        let broker = Arc::new(ApprovalBroker::new());
        let cancel = CancellationToken::new();
        let sink = sink();
        // 第一次 execute：允许一次
        let b = broker.clone();
        let s = sink.clone();
        let c1 = cancel.clone();
        let gate1 = tokio::spawn(async move {
            b.gate("unified_exec", &args(), "c6", policy::SessionPolicy { approval: ApprovalMode::OnRequest, sandbox: SandboxMode::WorkspaceWrite }, GateSubject::root("root"), GateEnv { sink: &s, cancel: &c1 }).await
        });
        let mut first_prompted = false;
        for _ in 0..200 {
            if broker.has_pending("appr-c6") {
                first_prompted = true;
                assert!(broker.resolve("appr-c6", ApprovalDecision::Allow));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(first_prompted, "第一次 execute 必须挂起等审批");
        assert_eq!(gate1.await.unwrap(), GateOutcome::Allowed);
        // 下一个 turn 的同类调用必须重新询问：Allow 不写 remembered_execute，
        // 批量确认窗口（SH-118④）也随 turn 清空——"允许一次"不是"允许整个会话"。
        broker.begin_turn("root");
        let b = broker.clone();
        let s2 = sink.clone();
        let c2 = cancel.clone();
        let gate2 = tokio::spawn(async move {
            b.gate("unified_exec", &args(), "c7", policy::SessionPolicy { approval: ApprovalMode::OnRequest, sandbox: SandboxMode::WorkspaceWrite }, GateSubject::root("root"), GateEnv { sink: &s2, cancel: &c2 }).await
        });
        let mut prompted = false;
        for _ in 0..200 {
            if broker.has_pending("appr-c7") {
                prompted = true;
                assert!(broker.resolve("appr-c7", ApprovalDecision::Deny));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(prompted, "「允许一次」不得跨 turn 免问：下一 turn 必须挂起等审批");
        assert_eq!(gate2.await.unwrap(), GateOutcome::Denied);
    }

    #[tokio::test]
    async fn cancel_during_prompt_cancels_and_unregisters() {
        let broker = Arc::new(ApprovalBroker::new());
        let cancel = CancellationToken::new();
        let gate = tokio::spawn({
            let b = broker.clone();
            let s = sink();
            let c = cancel.clone();
            async move { b.gate("unified_exec", &args(), "c5", policy::SessionPolicy { approval: ApprovalMode::OnRequest, sandbox: SandboxMode::WorkspaceWrite }, GateSubject::root("root"), GateEnv { sink: &s, cancel: &c }).await }
        });
        for _ in 0..200 {
            if broker.has_pending("appr-c5") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        cancel.cancel();
        assert_eq!(gate.await.unwrap(), GateOutcome::Cancelled);
        assert!(!broker.has_pending("appr-c5"), "取消后必须撤单");
    }

    /// 收窄后的工具必须直接拒绝，且**不得**发审批请求：问一次就能放行
    /// 等于把「只降不升」变成「可以申请提权」（ADR-103）。
    #[tokio::test]
    async fn allowlist_denies_unlisted_tool_without_prompt() {
        #[derive(Default)]
        struct CapturingSink(Mutex<Vec<EventMsg>>);
        impl EventSink for CapturingSink {
            fn emit(&self, msg: &EventMsg) -> Result<(), String> {
                self.0.lock().unwrap().push(msg.clone());
                Ok(())
            }
        }

        let broker = ApprovalBroker::new();
        let cancel = CancellationToken::new();
        let events = Arc::new(CapturingSink::default());
        let allowlist: ToolAllowlist =
            Arc::new(["read_file".to_string()].into_iter().collect());
        let subject = GateSubject {
            agent_id: "child-1",
            task_summary: Some("只读探索"),
            allowed_tools: Some(&allowlist),
            risk_override: None,
        };
        let out = broker
            .gate(
                "unified_exec",
                &args(),
                "c8",
                policy::SessionPolicy { approval: ApprovalMode::OnRequest, sandbox: SandboxMode::WorkspaceWrite },
                subject,
                GateEnv { sink: &(events.clone() as Arc<dyn EventSink>), cancel: &cancel },
            )
            .await;
        assert_eq!(out, GateOutcome::Forbidden);
        assert!(
            events.0.lock().unwrap().is_empty(),
            "白名单外的工具连审批都不该发，否则等于给模型一次提权申请入口"
        );
    }

    /// ADR-103：AllowAlways 的记忆按 agent_id 分域——子代理放行过的 execute
    /// 不得让父代理或兄弟代理静默放行。
    #[tokio::test]
    async fn allow_always_memory_is_scoped_to_the_requesting_agent() {
        let broker = Arc::new(ApprovalBroker::new());
        let cancel = CancellationToken::new();
        let policy = policy::SessionPolicy { approval: ApprovalMode::OnRequest, sandbox: SandboxMode::WorkspaceWrite };

        // 子代理 A 选择「本会话记住」
        let gate_a = tokio::spawn({
            let broker = broker.clone();
            let cancel = cancel.clone();
            async move {
                broker
                    .gate("unified_exec", &args(), "c9", policy, GateSubject::root("child-a"), GateEnv { sink: &sink(), cancel: &cancel })
                    .await
            }
        });
        for _ in 0..200 {
            if broker.has_pending("appr-c9") {
                assert!(broker.resolve("appr-c9", ApprovalDecision::AllowAlways));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(gate_a.await.unwrap(), GateOutcome::Allowed);

        // 兄弟代理 B 必须重新询问，而不是继承 A 的记忆
        let gate_b = tokio::spawn({
            let broker = broker.clone();
            let cancel = cancel.clone();
            async move {
                broker
                    .gate("unified_exec", &args(), "c10", policy, GateSubject::root("child-b"), GateEnv { sink: &sink(), cancel: &cancel })
                    .await
            }
        });
        let mut prompted = false;
        for _ in 0..200 {
            if broker.has_pending("appr-c10") {
                prompted = true;
                assert!(broker.resolve("appr-c10", ApprovalDecision::Deny));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(prompted, "兄弟代理不得继承另一个子代理的 AllowAlways 记忆");
        assert_eq!(gate_b.await.unwrap(), GateOutcome::Denied);
    }

    /// 审批冒泡（ADR-103）：请求事件必须自带来源代理与任务摘要。
    #[tokio::test]
    async fn approval_request_carries_origin_agent_and_task_summary() {
        #[derive(Default)]
        struct CapturingSink(Mutex<Vec<EventMsg>>);
        impl EventSink for CapturingSink {
            fn emit(&self, msg: &EventMsg) -> Result<(), String> {
                self.0.lock().unwrap().push(msg.clone());
                Ok(())
            }
        }

        let broker = Arc::new(ApprovalBroker::new());
        let cancel = CancellationToken::new();
        let events = Arc::new(CapturingSink::default());
        let gate = tokio::spawn({
            let broker = broker.clone();
            let cancel = cancel.clone();
            let events = events.clone();
            async move {
                broker
                    .gate(
                        "unified_exec",
                        &args(),
                        "c11",
                        policy::SessionPolicy { approval: ApprovalMode::OnRequest, sandbox: SandboxMode::WorkspaceWrite },
                        GateSubject {
                            agent_id: "child-9",
                            task_summary: Some("统计仓库文件数"),
                            allowed_tools: None,
                            risk_override: None,
                        },
                        GateEnv { sink: &(events as Arc<dyn EventSink>), cancel: &cancel },
                    )
                    .await
            }
        });
        for _ in 0..200 {
            if broker.has_pending("appr-c11") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let request = events.0.lock().unwrap().iter().find_map(|event| match event {
            EventMsg::ExecApprovalRequest { agent_id, task_summary, .. } => {
                Some((agent_id.clone(), task_summary.clone()))
            }
            _ => None,
        });
        assert_eq!(
            request,
            Some((Some("child-9".to_string()), Some("统计仓库文件数".to_string())))
        );

        cancel.cancel();
        let _ = gate.await.unwrap();
    }

    /// SH-063 / ADR-107：用户把 MCP 工具标为 read 时，门禁按用户档位放行，
    /// 不再因为分发名不在内置名单里就落 destructive。
    #[tokio::test]
    async fn risk_override_read_allows_mcp_tool_without_prompt() {
        let broker = ApprovalBroker::new();
        let cancel = CancellationToken::new();
        let subject = GateSubject {
            agent_id: "root",
            task_summary: None,
            allowed_tools: None,
            risk_override: Some(RiskLevel::Read),
        };
        let out = broker
            .gate(
                "mcp__srv__read",
                &args(),
                "c12",
                policy::SessionPolicy { approval: ApprovalMode::OnRequest, sandbox: SandboxMode::WorkspaceWrite },
                subject,
                GateEnv { sink: &sink(), cancel: &cancel },
            )
            .await;
        assert_eq!(out, GateOutcome::Allowed);
        assert!(!broker.has_pending("appr-c12"), "read 档不该发审批");
    }

    /// 反向：用户把任何工具标成 destructive，都必须弹审批——档位只紧不松。
    #[tokio::test]
    async fn risk_override_destructive_forces_prompt_even_for_read_named_tool() {
        let broker = Arc::new(ApprovalBroker::new());
        let cancel = CancellationToken::new();
        let subject = GateSubject {
            agent_id: "root",
            task_summary: None,
            allowed_tools: None,
            risk_override: Some(RiskLevel::Destructive),
        };
        let b = broker.clone();
        let c = cancel.clone();
        let s = sink();
        let gate = tokio::spawn(async move {
            b.gate(
                "read_file",
                &args(),
                "c13",
                policy::SessionPolicy { approval: ApprovalMode::OnRequest, sandbox: SandboxMode::WorkspaceWrite },
                subject,
                GateEnv { sink: &s, cancel: &c },
            )
            .await
        });
        let mut prompted = false;
        for _ in 0..200 {
            if broker.has_pending("appr-c13") {
                prompted = true;
                assert!(broker.resolve("appr-c13", ApprovalDecision::Deny));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(prompted, "destructive 档必须弹审批");
        assert_eq!(gate.await.unwrap(), GateOutcome::Denied);
    }

    /// 等挂起请求出现（最多 2s）。新用例统一走这里，不改既有用例的轮询写法。
    async fn wait_pending(broker: &ApprovalBroker, request_id: &str) -> bool {
        for _ in 0..200 {
            if broker.has_pending(request_id) {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        false
    }

    /// 取一次审批卡事件里的 `(call_id, batch_scope)`，用于断言卡片上写了什么。
    fn approval_fields(capture: &CaptureSink, request_id: &str) -> Option<(Option<String>, Option<String>)> {
        capture.events.lock().unwrap().iter().find_map(|event| match event {
            EventMsg::ExecApprovalRequest { request_id: id, call_id, batch_scope, .. } if id == request_id => {
                Some((call_id.clone(), batch_scope.clone()))
            }
            _ => None,
        })
    }

    const ON_REQUEST_WS: policy::SessionPolicy = policy::SessionPolicy {
        approval: ApprovalMode::OnRequest,
        sandbox: SandboxMode::WorkspaceWrite,
    };

    /// ④-1 对齐 dsh：`workspace-write` 下区内写入不弹卡，也不再需要批量窗口。
    ///
    /// 反向对照：把 `decide` 的 `WriteInWorkspace + OnRequest` 改回 `Prompt`，
    /// 本用例会红（多出审批卡事件）。
    #[tokio::test]
    async fn write_tool_passes_without_prompt_in_workspace_write() {
        let broker = Arc::new(ApprovalBroker::new());
        let cancel = CancellationToken::new();
        let capture = Arc::new(CaptureSink::default());
        let sink: Arc<dyn EventSink> = capture.clone();
        let patch = serde_json::json!({"patch": "*** Begin Patch\n*** Update File: a.rs\n@@\n-old\n+new\n*** End Patch"});

        for (call_id, args) in [("w1", patch.clone()), ("w2", serde_json::json!({"patch": "other"}))] {
            let outcome = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                broker.gate("apply_patch", &args, call_id, ON_REQUEST_WS, GateSubject::root("root"), GateEnv { sink: &sink, cancel: &cancel }),
            )
            .await
            .expect("区内写入不得挂起等待审批");
            assert_eq!(outcome, GateOutcome::Allowed, "区内写入不得弹审批");
        }
        assert!(broker.pending.lock().unwrap().is_empty(), "不得产生待审批请求");
        assert!(
            !capture.events.lock().unwrap().iter().any(|event| matches!(event, EventMsg::ExecApprovalRequest { .. })),
            "不得发出审批卡事件"
        );
    }

    /// 预授权档放行计划评审时必须在事件流留痕：用户事后能从时间线查到
    /// 「模型自己批了自己的计划」，而不是只在日志文件里。
    ///
    /// 反向对照：去掉 `gate` 里的 `AllowlistHit` 发射，本用例变红。
    #[tokio::test]
    async fn plan_review_preauthorization_emits_visible_audit_event() {
        let broker = Arc::new(ApprovalBroker::new());
        let cancel = CancellationToken::new();
        let capture = Arc::new(CaptureSink::default());
        let sink: Arc<dyn EventSink> = capture.clone();

        let outcome = broker
            .gate(
                "exit_plan_mode",
                &serde_json::json!({}),
                "p1",
                policy::SessionPolicy { approval: ApprovalMode::Never, sandbox: SandboxMode::WorkspaceWrite },
                GateSubject::root("root"),
                GateEnv { sink: &sink, cancel: &cancel },
            )
            .await;
        assert_eq!(outcome, GateOutcome::Allowed);

        let events = capture.events.lock().unwrap();
        let hit = events.iter().find_map(|event| match event {
            EventMsg::AllowlistHit { tool, command, rule_id } => {
                Some((tool.clone(), command.clone(), rule_id.clone()))
            }
            _ => None,
        });
        assert_eq!(
            hit,
            Some((
                "exit_plan_mode".to_string(),
                "sandbox=workspace_write approval=never".to_string(),
                "plan-review:pre-authorized".to_string()
            )),
            "计划评审的预授权必须在事件流留下可查痕迹"
        );
    }

    /// D3：命令类只在**参数完全相同的同一 turn 内**复用批准，且首次卡片必须声明范围。
    /// 跨 turn 窗口清空，同一命令重新弹卡。
    #[tokio::test]
    async fn identical_exec_command_batches_within_turn_and_declares_scope() {
        let broker = Arc::new(ApprovalBroker::new());
        let cancel = CancellationToken::new();
        let capture = Arc::new(CaptureSink::default());
        let sink: Arc<dyn EventSink> = capture.clone();
        let exec_args = serde_json::json!({"command": "git status"});

        let gate1 = tokio::spawn({
            let broker = broker.clone();
            let cancel = cancel.clone();
            let sink = sink.clone();
            let exec_args = exec_args.clone();
            async move {
                broker
                    .gate("unified_exec", &exec_args, "e1", ON_REQUEST_WS, GateSubject::root("root"), GateEnv { sink: &sink, cancel: &cancel })
                    .await
            }
        });
        assert!(wait_pending(&broker, "appr-e1").await);
        assert_eq!(
            approval_fields(&capture, "appr-e1"),
            Some((Some("e1".to_string()), Some("same-args".to_string()))),
            "命令类批准后确实会开窗口，卡片必须写明范围"
        );
        assert!(broker.resolve("appr-e1", ApprovalDecision::Allow));
        assert_eq!(gate1.await.unwrap(), GateOutcome::Allowed);

        // 同一 turn、同一参数：不再询问
        let same = broker
            .gate("unified_exec", &exec_args, "e2", ON_REQUEST_WS, GateSubject::root("root"), GateEnv { sink: &sink, cancel: &cancel })
            .await;
        assert_eq!(same, GateOutcome::Allowed, "参数完全相同的调用应复用本 turn 的批准");
        assert!(!broker.has_pending("appr-e2"), "批量命中不应产生新的审批卡");

        // 新 turn：窗口清空，必须重新询问
        broker.begin_turn("root");
        let gate3 = tokio::spawn({
            let broker = broker.clone();
            let cancel = cancel.clone();
            let sink = sink.clone();
            let exec_args = exec_args.clone();
            async move {
                broker
                    .gate("unified_exec", &exec_args, "e3", ON_REQUEST_WS, GateSubject::root("root"), GateEnv { sink: &sink, cancel: &cancel })
                    .await
            }
        });
        assert!(wait_pending(&broker, "appr-e3").await, "批量不跨 turn");
        assert!(broker.resolve("appr-e3", ApprovalDecision::Deny));
        assert_eq!(gate3.await.unwrap(), GateOutcome::Denied);
    }

    /// D3 核心反向对照：批准 `git status` 不得连带 `git push --force`。
    /// 旧 `(agent_id, tool)` 批量下第二次会直接放行；改为参数指纹后必须重新弹卡。
    #[tokio::test]
    async fn different_exec_command_in_same_turn_must_prompt_again() {
        let broker = Arc::new(ApprovalBroker::new());
        let cancel = CancellationToken::new();
        let sink = sink();

        let gate1 = tokio::spawn({
            let broker = broker.clone();
            let cancel = cancel.clone();
            let sink = sink.clone();
            async move {
                broker
                    .gate("unified_exec", &serde_json::json!({"command": "git status"}), "d1", ON_REQUEST_WS, GateSubject::root("root"), GateEnv { sink: &sink, cancel: &cancel })
                    .await
            }
        });
        assert!(wait_pending(&broker, "appr-d1").await);
        assert!(broker.resolve("appr-d1", ApprovalDecision::Allow));
        assert_eq!(gate1.await.unwrap(), GateOutcome::Allowed);

        let gate2 = tokio::spawn({
            let broker = broker.clone();
            let cancel = cancel.clone();
            let sink = sink.clone();
            async move {
                broker
                    .gate(
                        "unified_exec",
                        &serde_json::json!({"command": "git push --force"}),
                        "d2",
                        ON_REQUEST_WS,
                        GateSubject::root("root"),
                        GateEnv { sink: &sink, cancel: &cancel },
                    )
                    .await
            }
        });
        assert!(
            wait_pending(&broker, "appr-d2").await,
            "命令变了风险就变了：不得复用 `git status` 的批准"
        );
        assert!(broker.resolve("appr-d2", ApprovalDecision::Deny));
        assert_eq!(gate2.await.unwrap(), GateOutcome::Denied);
    }

    /// 写入不再弹卡之后，`AllowAlways` 在写入面上没有可达路径；同时必须保证
    /// 写入**不得**因历史批准写会话记忆或开批量窗口（否则将来接上第三态就是无声扩权）。
    #[tokio::test]
    async fn write_never_writes_memory_or_batch_window() {
        let broker = Arc::new(ApprovalBroker::new());
        let cancel = CancellationToken::new();
        let sink = sink();
        let patch = serde_json::json!({"patch": "*** Begin Patch\n*** Update File: a.rs\n@@\n-old\n+new\n*** End Patch"});

        for call_id in ["aa1", "aa2"] {
            let outcome = broker
                .gate("apply_patch", &patch, call_id, ON_REQUEST_WS, GateSubject::root("root"), GateEnv { sink: &sink, cancel: &cancel })
                .await;
            assert_eq!(outcome, GateOutcome::Allowed);
        }
        assert!(broker.remembered_execute.lock().unwrap().is_empty(), "写入不得写会话记忆");
        assert!(broker.batch.lock().unwrap().is_none(), "写入不得开批量窗口");
    }

    /// 提权询问（on-failure）：卡片仍要能定位到原始 call_id，但**永不声明批量范围**。
    #[tokio::test]
    async fn escalation_prompt_carries_call_id_but_never_batch_scope() {
        let broker = Arc::new(ApprovalBroker::new());
        let cancel = CancellationToken::new();
        let capture = Arc::new(CaptureSink::default());
        let sink: Arc<dyn EventSink> = capture.clone();

        let gate = tokio::spawn({
            let broker = broker.clone();
            let cancel = cancel.clone();
            let sink = sink.clone();
            async move {
                broker
                    .request_escalation(
                        "unified_exec",
                        &serde_json::json!({"command": "rm -rf build"}),
                        "esc1",
                        "只读沙箱禁止执行",
                        GateSubject::root("root"),
                        GateEnv { sink: &sink, cancel: &cancel },
                    )
                    .await
            }
        });
        assert!(wait_pending(&broker, "appr-esc1-escalate").await);
        assert_eq!(
            approval_fields(&capture, "appr-esc1-escalate"),
            Some((Some("esc1".to_string()), None)),
            "提权卡要用原始 call_id 取详情，但「只这一次」不能声明批量"
        );
        assert!(broker.resolve("appr-esc1-escalate", ApprovalDecision::Deny));
        assert_eq!(gate.await.unwrap(), GateOutcome::Denied);
    }

    /// 阶段 3 主用例：请求挂着、但最近没有心跳 → 按 TTL 转成「没人可答」，不永远等。
    ///
    /// 这里调的是生产看门狗每秒调用的同一个 `sweep_unattended`，不是另写一条捷径。
    /// 反向对照见下一条：没有清扫时这个 gate 会一直挂着。
    ///
    /// 不在这里捕获日志：并行跑测试时 tracing 的调用点兴趣缓存是进程级的，
    /// 别的用例（key_check）用全局订阅器重算后会把这条 warn 在宏快路径上丢掉
    /// （实测单条绿、并行红）。可定位性改用 `sweep_unattended` 的返回视图来断言。
    #[tokio::test]
    async fn lease_ttl_denies_pending_approval_without_heartbeat() {
        let broker = Arc::new(ApprovalBroker::new());
        broker.enforce_lease();
        let t0 = Instant::now();
        broker.attach_at("viewer-1", t0);
        let sink: Arc<dyn EventSink> = Arc::new(CaptureSink::default());
        let cancel = CancellationToken::new();

        let gate = tokio::spawn({
            let broker = broker.clone();
            let cancel = cancel.clone();
            let sink = sink.clone();
            async move {
                broker
                    .gate(
                        "unified_exec",
                        &args(),
                        "c-lease",
                        ON_REQUEST_WS,
                        GateSubject::root("root"),
                        GateEnv { sink: &sink, cancel: &cancel },
                    )
                    .await
            }
        });
        assert!(
            wait_pending(&broker, "appr-c-lease").await,
            "有租约时请求应挂起等待用户回执"
        );

        // 不心跳：推进到 TTL 之后，看门狗的清扫必须把它收口。
        let expired = broker
            .sweep_unattended(t0 + std::time::Duration::from_millis(APPROVAL_LEASE_TTL_MS + 1));
        assert_eq!(expired.len(), 1, "无心跳的挂起请求必须被 TTL 收口");
        assert_eq!(expired[0].request_id, "appr-c-lease");
        assert_eq!(expired[0].tool, "unified_exec", "收口记录要能定位到工具");
        assert_eq!(expired[0].session_id, "sess-capture", "快照要能定位到会话");

        let out = tokio::time::timeout(std::time::Duration::from_secs(1), gate)
            .await
            .expect("租约超时后 gate 必须结束，不能永远等")
            .unwrap();
        assert_eq!(out, GateOutcome::Denied, "没人可答 = 不能放行");
        assert!(!broker.has_pending("appr-c-lease"), "收口后不得残留 pending");
    }

    /// 反向对照：不做租约清扫，同样的 gate 在 TTL 之后仍然挂着。
    ///
    /// 两条用例合起来证明：解除等待的是租约清扫，不是 Ready、不是别的副作用。
    #[tokio::test]
    async fn without_sweep_gate_stays_pending_past_ttl() {
        let broker = Arc::new(ApprovalBroker::new());
        broker.enforce_lease();
        let t0 = Instant::now();
        broker.attach_at("viewer-1", t0);
        let sink: Arc<dyn EventSink> = Arc::new(CaptureSink::default());
        let cancel = CancellationToken::new();

        let mut gate = tokio::spawn({
            let broker = broker.clone();
            let cancel = cancel.clone();
            let sink = sink.clone();
            async move {
                broker
                    .gate(
                        "unified_exec",
                        &args(),
                        "c-no-sweep",
                        ON_REQUEST_WS,
                        GateSubject::root("root"),
                        GateEnv { sink: &sink, cancel: &cancel },
                    )
                    .await
            }
        });
        assert!(wait_pending(&broker, "appr-c-no-sweep").await);

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut gate)
                .await
                .is_err(),
            "不清扫时请求会一直挂着——这是被修复的旧行为"
        );

        // 收尾：补一次清扫，避免留下悬挂任务。
        let expired = broker
            .sweep_unattended(t0 + std::time::Duration::from_millis(APPROVAL_LEASE_TTL_MS + 1));
        assert_eq!(expired.len(), 1);
        assert_eq!(gate.await.unwrap(), GateOutcome::Denied);
    }

    /// 心跳仍在 TTL 内时必须继续等：用户「在看但还没点」是正常场景，不能误杀。
    #[tokio::test]
    async fn fresh_heartbeat_keeps_pending_approval_waiting() {
        let broker = Arc::new(ApprovalBroker::new());
        broker.enforce_lease();
        let t0 = Instant::now();
        broker.attach_at("viewer-1", t0);
        let sink: Arc<dyn EventSink> = Arc::new(CaptureSink::default());
        let cancel = CancellationToken::new();

        let gate = tokio::spawn({
            let broker = broker.clone();
            let cancel = cancel.clone();
            let sink = sink.clone();
            async move {
                broker
                    .gate(
                        "unified_exec",
                        &args(),
                        "c-beat",
                        ON_REQUEST_WS,
                        GateSubject::root("root"),
                        GateEnv { sink: &sink, cancel: &cancel },
                    )
                    .await
            }
        });
        assert!(wait_pending(&broker, "appr-c-beat").await);

        let near_deadline =
            t0 + std::time::Duration::from_millis(APPROVAL_LEASE_TTL_MS - 1);
        assert!(
            broker.sweep_unattended(near_deadline).is_empty(),
            "TTL 内的心跳必须让请求继续等待"
        );
        broker.heartbeat_at("viewer-1", near_deadline);

        let expired = broker.sweep_unattended(
            t0 + std::time::Duration::from_millis(APPROVAL_LEASE_TTL_MS * 2 + 1),
        );
        assert_eq!(expired.len(), 1, "心跳停止超过 TTL 后仍要收口");
        assert_eq!(gate.await.unwrap(), GateOutcome::Denied);
    }

    /// 生产判据：没有新鲜租约时，请求在创建前就被拒绝，不占 pending、不发卡。
    #[tokio::test]
    async fn lease_policy_denies_before_pending_is_created() {
        let broker = ApprovalBroker::new();
        broker.enforce_lease();
        let capture = Arc::new(CaptureSink::default());
        let sink: Arc<dyn EventSink> = capture.clone();
        let cancel = CancellationToken::new();

        let out = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            broker.gate(
                "unified_exec",
                &args(),
                "c-no-lease",
                ON_REQUEST_WS,
                GateSubject::root("root"),
                GateEnv { sink: &sink, cancel: &cancel },
            ),
        )
        .await
        .expect("无租约时必须立即裁决，不能进入等待");
        assert_eq!(out, GateOutcome::Denied);
        assert_eq!(broker.pending_count(), 0, "无租约的请求不得进入 pending 表");
        assert!(
            !capture
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event, EventMsg::ExecApprovalRequest { .. })),
            "没人能看的卡片不得发出"
        );
    }

    /// 快照是横幅与刷新重建的唯一数据源：身份字段、阈值、可达性都必须齐。
    #[tokio::test]
    async fn pending_snapshot_exposes_identity_and_lease_thresholds() {
        let broker = Arc::new(ApprovalBroker::new());
        broker.enforce_lease();
        broker.attach("viewer-snap");
        let sink: Arc<dyn EventSink> = Arc::new(CaptureSink::default());
        let cancel = CancellationToken::new();

        let gate = tokio::spawn({
            let broker = broker.clone();
            let cancel = cancel.clone();
            let sink = sink.clone();
            async move {
                broker
                    .gate(
                        "unified_exec",
                        &args(),
                        "c-snap",
                        ON_REQUEST_WS,
                        GateSubject::root("root"),
                        GateEnv { sink: &sink, cancel: &cancel },
                    )
                    .await
            }
        });
        assert!(wait_pending(&broker, "appr-c-snap").await);

        let snapshot = broker.pending_snapshot();
        assert_eq!(snapshot.heartbeat_ms, APPROVAL_LEASE_HEARTBEAT_MS);
        assert_eq!(snapshot.ttl_ms, APPROVAL_LEASE_TTL_MS);
        assert!(snapshot.attended, "有新鲜租约时快照必须报告可达");
        assert_eq!(snapshot.approvals.len(), 1);
        let view = &snapshot.approvals[0];
        assert_eq!(view.request_id, "appr-c-snap");
        assert_eq!(view.session_id, "sess-capture");
        assert_eq!(view.tool, "unified_exec");
        assert_eq!(view.risk, RiskLevel::Execute);
        assert!(view.call_id.as_deref() == Some("c-snap"));

        assert!(broker.resolve("appr-c-snap", ApprovalDecision::Deny));
        assert_eq!(gate.await.unwrap(), GateOutcome::Denied);
    }

    /// 待审批表每次增删都要推进 revision 并通知广播钩子（前端据此防倒序）。
    #[tokio::test]
    async fn pending_changed_listener_fires_on_insert_and_sweep() {
        let broker = Arc::new(ApprovalBroker::new());
        broker.enforce_lease();
        let t0 = Instant::now();
        broker.attach_at("viewer-1", t0);
        let hits = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let listener_hits = hits.clone();
        broker.set_pending_changed_listener(Arc::new(move || {
            listener_hits.fetch_add(1, Ordering::SeqCst);
        }));
        let sink: Arc<dyn EventSink> = Arc::new(CaptureSink::default());
        let cancel = CancellationToken::new();

        let gate = tokio::spawn({
            let broker = broker.clone();
            let cancel = cancel.clone();
            let sink = sink.clone();
            async move {
                broker
                    .gate(
                        "unified_exec",
                        &args(),
                        "c-hook",
                        ON_REQUEST_WS,
                        GateSubject::root("root"),
                        GateEnv { sink: &sink, cancel: &cancel },
                    )
                    .await
            }
        });
        assert!(wait_pending(&broker, "appr-c-hook").await);
        assert!(hits.load(Ordering::SeqCst) >= 1, "插入 pending 后必须广播");
        let before = broker.pending_snapshot().revision;

        let expired = broker
            .sweep_unattended(t0 + std::time::Duration::from_millis(APPROVAL_LEASE_TTL_MS + 1));
        assert_eq!(expired.len(), 1);
        assert!(hits.load(Ordering::SeqCst) >= 2, "收口后必须再广播一次");
        assert!(
            broker.pending_snapshot().revision > before,
            "每次增删都要推进 revision"
        );
        assert_eq!(gate.await.unwrap(), GateOutcome::Denied);
    }
}
