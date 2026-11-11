//! 审批 broker（docs §8 permission/mod.rs，ADR-003：集中裁决、审批不可绕过）。
//!
//! 职责：工具执行前的门禁（gate）——按 policy 裁决；需要询问时发
//! `ExecApprovalRequest` 并挂起等待前端回执（approve 命令 → resolve）。
//! 审批 UI 不可伪造：裁决只接受后端校验过的 request_id 回执。

pub mod policy;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::config::SandboxMode;
use crate::events::{ApprovalDecision, EventMsg, EventSink, RiskLevel};

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
    pending: Mutex<HashMap<String, tokio::sync::oneshot::Sender<ApprovalDecision>>>,
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
    pub fn new() -> Self {
        Self::default()
    }

    /// 装配层把 config.toml 里的前缀规则装进来（空 = 全部按风险裁决）。
    pub fn set_prefix_rules(&self, rules: PrefixAllowlist) {
        *self.prefix.write().expect("prefix 锁中毒") = rules;
    }

    pub fn prefix_rule_count(&self) -> usize {
        self.prefix.read().expect("prefix 锁中毒").rule_count()
    }

    /// 关闭批量确认（评估对照用）。只影响 `gate` 的批量短路，不改记忆与风险评估。
    pub fn disable_batch(&self) {
        self.batch_disabled
            .store(true, std::sync::atomic::Ordering::SeqCst);
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
                    let (effective, _) = policy.approval.effective(policy.sandbox);
                    let _ = env.sink.emit(&EventMsg::AllowlistHit {
                        tool: tool.to_string(),
                        command: format!(
                            "sandbox={} approval={}",
                            policy.sandbox.as_str(),
                            effective.as_str()
                        ),
                        rule_id: "plan-review:pre-authorized".to_string(),
                    });
                }
                GateOutcome::Allowed
            }
            Decision::Deny => GateOutcome::Forbidden,
            Decision::Prompt if batched => GateOutcome::Allowed,
            Decision::Prompt => {
                self.request_approval(
                    tool,
                    args,
                    call_id,
                    ApprovalPrompt { risk, reason: Some(prompt_reason(risk)), remember: true, escalation: false },
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
        let (tx, rx) = tokio::sync::oneshot::channel::<ApprovalDecision>();
        self.pending
            .lock()
            .expect("pending 锁中毒")
            .insert(request_id.clone(), tx);
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
            batch_scope: (remember
                && !self.batch_disabled.load(std::sync::atomic::Ordering::SeqCst)
                && Self::batch_key(risk, args).is_some())
            .then(|| "same-args".to_string()),
        }) {
            // M2：卡片送不到 = 用户不可能看到、也就不可能批准 = 唯一安全的裁决是拒绝。
            // 不能等：没有回执来源，下面的 `select!` 会永久挂起整个回合。
            // 也不加超时——「用户暂时离开」是正常场景，超时默认拒绝会误伤真实用户；
            // 要区分的是「人没在看」（等）和「卡片根本没送到」（拒），这里是后者。
            self.pending.lock().expect("pending 锁中毒").remove(&request_id);
            tracing::warn!(
                "审批卡未能送达（request_id={request_id}，tool={tool}，agent={}）：{error}；\
                 卡片送不到则用户无法批准，按 Denied 裁决，不挂起等待回执",
                subject.agent_id
            );
            return GateOutcome::Denied;
        }
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
            // 未放行的请求撤单（已 resolve 的 remove 无副作用）
            self.pending.lock().expect("pending 锁中毒").remove(&request_id);
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
    pub fn resolve(&self, request_id: &str, decision: ApprovalDecision) -> bool {
        match self
            .pending
            .lock()
            .expect("pending 锁中毒")
            .remove(request_id)
        {
            Some(tx) => tx.send(decision).is_ok(),
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
        let mut guard = self.pending.lock().expect("pending 锁中毒");
        let count = guard.len();
        guard.clear();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ApprovalMode, SandboxMode};
    use crate::events::NullEventSink;
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
}
