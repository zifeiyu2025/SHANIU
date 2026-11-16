//! 审批请求/裁决的档案留痕（2026-11-17，真机冒烟 N2）。
//!
//! 为什么要有这一层：审批卡是 `exec_approval_request` 事件驱动的，档案里原本
//! **一个字都没有**。审批卡挂起时进程被强杀，事件日志之外什么都不剩，重开后那张卡
//! 凭空消失，用户看不出「曾经有一个审批在等」。请求一发出就在档案里落一条，真有人
//! 作答时再落一条裁决；「有请求无裁决」= 关闭前没作答，回放据此把那张卡显式标成
//! 已失效，而不是伪造一张能点的卡（关闭后已没有回执通道，点了只会给出一个没人等的
//! request_id）。
//!
//! 为什么包在出口层而不是审批器里：档案写者只在回合运行这一层才有；把写者塞进
//! `permission/` 会让权限层反向依赖会话档案，而审批路径是安全语义最不该被动的地方。
//! 包出口则零侵入——审批器照旧只认识「问」和「答」，留痕只是旁路。
//!
//! 落档失败**不阻断审批**：审批是安全路径，审计是旁路。失败只 warn，前端照常看到
//! 卡片；代价是那次审批在重开后没有「已失效」记录，而不是审批本身卡住。

use std::sync::Arc;

use crate::events::{EventMsg, EventSink, ResponderState};
use crate::rollout::{RolloutItem, RolloutWriter};

/// 在原有出口外面套一层：审批请求/裁决顺手写进会话档案。
pub fn wrap(inner: Arc<dyn EventSink>, writer: Arc<RolloutWriter>) -> Arc<dyn EventSink> {
    Arc::new(ApprovalArchiveSink { inner, writer })
}

struct ApprovalArchiveSink {
    inner: Arc<dyn EventSink>,
    writer: Arc<RolloutWriter>,
}

impl EventSink for ApprovalArchiveSink {
    fn emit(&self, msg: &EventMsg) -> Result<(), String> {
        if let EventMsg::ExecApprovalRequest { request_id, tool, risk, call_id, .. } = msg {
            let item = RolloutItem::ApprovalRequest {
                request_id: request_id.clone(),
                call_id: call_id.clone(),
                tool: tool.clone(),
                risk: risk.as_str().to_owned(),
                at_ms: crate::clock::now_ms(),
            };
            if let Err(error) = self.writer.append(&item) {
                tracing::warn!(target: "rollout", "审批请求落档失败（不影响审批本身）：{error}");
            }
        }
        self.inner.emit(msg)
    }

    /// 子代理换的是信封（`agent_id`），不是会话也不是档案：同一个 run 的审批
    /// 都属于同一条时间线，写者照旧。
    fn scoped(&self, agent_id: &str, parent_id: Option<&str>) -> Option<Arc<dyn EventSink>> {
        let inner = self.inner.scoped(agent_id, parent_id)?;
        Some(Arc::new(ApprovalArchiveSink { inner, writer: self.writer.clone() }))
    }

    fn session_id(&self) -> &str {
        self.inner.session_id()
    }

    fn responder(&self) -> ResponderState {
        self.inner.responder()
    }
}
