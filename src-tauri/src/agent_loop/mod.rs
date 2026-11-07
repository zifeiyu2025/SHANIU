//! turn 主循环（docs §8 agent_loop/mod.rs，ADR-001）：
//! 请求模型（流式）→ 收集 tool calls → 执行 → 结果回填 → 再请求。
//!
//! 单一职责边界：
//! - 模型流经 `ModelBackend` 注入（真实 Responses 实现 + 测试脚本实现）；
//! - 工具执行经 `ToolRouter`（M2 起在路由层前置审批裁决）；
//! - 事件全部经 `EventSink` 广播；取消经 `CancellationToken`（drop 模型流断开连接）。
//!
//! 每个任务 = 若干 turn；turn 内无工具调用即任务完成（completed）。

pub mod budget;
pub mod prompt;

use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::config::AppConfig;
use crate::events::{EventMsg, ToolCallResult};
use crate::model_client::{
    self, provider::ProviderConfig, HistoryItem, ModelStreamEvent, ResponseItem, Role, Usage,
};
use crate::permission::{policy, ApprovalBroker};
use crate::tool_router::{PlanState, ToolContext, ToolRouter};

pub use budget::TurnBudget;

// ============================== 模型后端抽象 ==============================

/// 模型流（转发 model_client 的别名）。
pub use crate::model_client::ModelStream;

/// 模型后端：把 (历史, 工具声明, 系统提示) 变成模型事件流。
///
/// `instructions` 每轮由 `prompt::system_instruction` 生成（docs/04 §6.1），
/// 不写入 history，避免逐轮重复累积。
#[async_trait::async_trait]
pub trait ModelBackend: Send + Sync {
    async fn stream(
        &self,
        history: &[HistoryItem],
        tools: &[ToolDeclarationShim],
        instructions: &str,
    ) -> Result<ModelStream, String>;
}

/// 避免 agent_loop 直接耦合 model_client 的声明类型（转发即可，无重复定义）。
pub use crate::model_client::ToolDeclaration as ToolDeclarationShim;

/// 真实后端：Responses SSE（密钥由调用方从 SecretStore 解密后注入）。
pub struct ResponsesBackend {
    pub provider: ProviderConfig,
    pub api_key: String,
}

#[async_trait::async_trait]
impl ModelBackend for ResponsesBackend {
    async fn stream(
        &self,
        history: &[HistoryItem],
        tools: &[ToolDeclarationShim],
        instructions: &str,
    ) -> Result<ModelStream, String> {
        let stream =
            model_client::stream_model(&self.provider, &self.api_key, history, tools, instructions)
                .await?;
        Ok(Box::pin(stream))
    }
}

// ============================== 循环本体 ==============================

pub struct AgentLoop {
    pub backend: Arc<dyn ModelBackend>,
    pub router: Arc<ToolRouter>,
    pub broker: Arc<ApprovalBroker>,
    pub plan_state: Arc<PlanState>,
    pub workspace_root: PathBuf,
    pub max_turns: u32,
    /// 单会话 token 预算（04 §4 `[agent].token_budget`）：每轮采样前据此判定是否压缩。
    /// 0 = 不启用压缩（`compaction::plan_if_needed` 对 0 直接返回 None）。
    pub token_budget: usize,
    pub policy: policy::SessionPolicy,
    /// 会话档案写入器（SH-030）。None = 本次运行不落盘（测试与无目录启动用）。
    pub rollout: Option<Arc<crate::rollout::RolloutWriter>>,
    /// 落盘失败只播报一次：磁盘满时每条历史都报一遍会把事件流刷满，却没有任何新信息。
    /// 与其余字段一样公开——`AgentLoop` 是纯数据结构，测试与装配层都按字面量构造它。
    pub rollout_broke: std::sync::atomic::AtomicBool,
}

impl AgentLoop {
    /// 把 `history` 里尚未落盘的尾部条目补写进档案。
    ///
    /// 失败不中断本轮执行——工具与模型都不依赖档案；但必须**播报**：
    /// 静默降级会让用户以为"会话有存档、可以 resume"，而实际没有。
    fn flush_history(
        &self,
        sink: &Arc<dyn crate::events::EventSink>,
        history: &[HistoryItem],
        recorded: &mut usize,
    ) {
        let Some(writer) = &self.rollout else { *recorded = history.len(); return };
        for item in &history[*recorded..] {
            let entry = crate::rollout::RolloutItem::ResponseItem { item: item.clone() };
            if let Err(e) = writer.append(&entry) {
                if !self.rollout_broke.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    let _ = sink.emit(&EventMsg::Error {
                        message: format!("会话档案写入失败（后续不再重复播报）：{e}"),
                    });
                }
                // 失败后不再重试剩余条目：同一块盘继续试只会刷更多错误
                break;
            }
        }
        *recorded = history.len();
    }

    /// 就地压缩一段历史：发一次不带工具的总结请求 → 写 `Compacted` 锚点 → 用摘要替换区间。
    ///
    /// 三件事的顺序是硬性的：
    /// 1. **先落锚点、再改内存**。锚点的语义是「重放出的 history 里 `[start,end)` 区间被
    ///    summary 取代」，所以它必须写在那段条目**之后**——`recorded >= target.range_end`
    ///    由「每轮末尾都 flush」保证，锚点紧随其后，回放时正好接在那些条目之后生效。
    ///    顺序反了就会出现锚点指向尚未写入的条目，resume 直接报区间越界。
    /// 2. **锚点写失败不阻断内存压缩**。丢一次存档可以补，预算超限继续原样发请求
    ///    就是等着上下文溢出、整轮失败——后者严重得多。落盘失败本身已由
    ///    `flush_history` 负责播报，这里不再重复刷一条。
    /// 3. **压缩完必须把落盘游标收到新历史末尾**。`flush_history` 按
    ///    `history[*recorded..]` 切片，而压缩让 history 变短了：不收游标就是 start > len
    ///    直接 panic，而且是只在长会话跑到第 N 轮才炸的那种 panic。
    async fn compact_in_place(
        &self,
        history: &mut Vec<HistoryItem>,
        recorded: &mut usize,
        target: crate::compaction::Target,
        instructions: &str,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(), String> {
        let request = crate::compaction::build_request(history, &target);
        // 总结请求不带工具声明：它不需要调工具，带上只会让端点多算一份声明开销，
        // 并给模型一个"还可以继续查"的错误暗示。
        let mut stream = self
            .backend
            .stream(&request, &[], instructions)
            .await
            .map_err(|e| format!("压缩请求发起失败，本轮中止：{e}"))?;
        let mut summary = String::new();
        loop {
            let mut stream_pin = stream.as_mut();
            let next_item = stream_pin.next();
            tokio::select! {
                _ = cancel.cancelled() => return Err("压缩被用户取消".to_string()),
                item = next_item => match item {
                    None => break,
                    Some(ModelStreamEvent::OutputTextDelta { delta }) => summary.push_str(&delta),
                    // 压缩请求不该产出工具调用；真产出了也只能忽略——它不是本轮的答案
                    Some(_) => {}
                }
            }
        }
        let summary = crate::compaction::extract_summary(&summary)?;

        if let Some(writer) = &self.rollout {
            // 写失败在这里静默是刻意的：见上面第 2 点。
            let _ = writer.append(&crate::rollout::RolloutItem::Compacted {
                range_start: target.range_start,
                range_end: target.range_end,
                summary: summary.clone(),
            });
        }

        history.splice(
            target.range_start..target.range_end,
            // 摘要必须是 **user** 而不是 assistant。两个理由各自独立、都成立：
            // 1. thinking 档端点要求重放的每条 assistant 消息都带回 `reasoning_content`
            //    （SH-083），而摘要没有 reasoning——以 assistant 身份进历史就直接 400。
            //    这不是理论风险：1500 预算的真机基线里 `search-and-report` 就是这么挂的。
            // 2. 摘要不是"模型说过的话"，而是交给下一个模型的交接指令——语义上就是 user 侧输入。
            //    codex 同此：`CompactionSummary::role()` 返回 "user"。
            // 注意这并不违反 04 §6.1 不变量③：那条禁止的是把摘要塞进 **system**。
            [HistoryItem::Message { role: Role::User, text: summary, reasoning: None }],
        );
        *recorded = history.len();
        tracing::info!(
            compressed_items = target.range_end - target.range_start,
            history_len = history.len(),
            "上下文已压缩（SH-032）"
        );
        Ok(())
    }
}

impl AgentLoop {
    pub fn from_config(
        backend: Arc<dyn ModelBackend>,
        router: Arc<ToolRouter>,
        broker: Arc<ApprovalBroker>,
        plan_state: Arc<PlanState>,
        cfg: &AppConfig,
        workspace_root: PathBuf,
        rollout: Option<Arc<crate::rollout::RolloutWriter>>,
    ) -> Self {
        Self {
            backend,
            router,
            broker,
            plan_state,
            workspace_root,
            max_turns: TurnBudget::from_config(cfg).max_turns,
            token_budget: cfg.agent.token_budget,
            policy: policy::SessionPolicy {
                approval: cfg.approval.mode,
                sandbox: cfg.sandbox.mode,
            },
            rollout,
            rollout_broke: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// 执行一个完整任务（若干 turn）。失败/取消以 TurnCompleted 事件收口，
    /// 同时返回 TurnSummary（永不 panic、错误不静默）。
    pub async fn run_task(
        &self,
        task: &str,
        sink: Arc<dyn crate::events::EventSink>,
        cancel: CancellationToken,
    ) -> TurnSummary {
        let history = vec![HistoryItem::Message { role: Role::User, text: task.to_string(), reasoning: None }];
        self.run_with_history(history, 0, sink, cancel).await
    }

    /// 从既有历史续跑（SH-031 resume/fork）。
    ///
    /// `persisted` = 该历史里**已经落盘**的条数（resume 传 `resumed.history.len()`，
    /// 新会话传 0）。传错就会把整段旧历史再写一遍，档案里出现重复条目——这是
    /// 只有读的时候才会发现的错，所以做成显式参数而不是猜。
    ///
    /// 历史合法性由调用方负责：`rollout::resume` 重放出来的历史已按配对规则截断，
    /// 这里刻意不再"修"历史——静默补配对会掩盖档案损坏，而档案损坏必须由上层显式处理。
    pub async fn run_with_history(
        &self,
        mut history: Vec<HistoryItem>,
        persisted: usize,
        sink: Arc<dyn crate::events::EventSink>,
        cancel: CancellationToken,
    ) -> TurnSummary {
        // 已落盘的历史条数（SH-030）：每轮结束时补写尾部，崩溃只丢当前轮。
        // 续跑时从调用方给的起点开始，避免把旧历史再写一遍；上限锁死在历史长度，
        // 传大了也不会越过档案真实拥有的条目数。
        let mut recorded = persisted.min(history.len());
        let mut reason = TurnEndReasonLocal::Completed;
        let mut last_turn = 0u32;
        // 最近一次端点报回的上下文规模，作为压缩判据的观测侧（SH-032）。
        // 字节估算看不见推理 token 与工具声明开销，只有端点的 input_tokens 是真实值。
        let mut observed_input_tokens = 0usize;
        let emit = |sink: &Arc<dyn crate::events::EventSink>, msg: EventMsg| {
            let _ = sink.emit(&msg);
        };

        // SH-091：技能目录在每次任务开始时发现一次。目录为空时系统提示不加技能段，
        // 且不向模型声明 `skill` 工具——避免无技能工作区的 token 开销与假能力。
        let skill_summaries = crate::skills::discover(&self.workspace_root);
        let skill_catalog = crate::skills::catalog_lines(&skill_summaries, 500);
        let has_model_skills = skill_summaries.iter().any(|skill| skill.model_invocable);

        'outer: for turn in 1..=self.max_turns {
            last_turn = turn;
            emit(&sink, EventMsg::TurnStarted { turn });

            // 1) 采样（流式）：取消 → drop 流 = 断开连接
            // 系统提示每轮重算（docs/04 §6.1 不变量③）：计划态与沙箱模式变化必须反映到模型
            let instructions = prompt::system_instruction(
                &self.workspace_root,
                self.policy.sandbox,
                self.plan_state.is_active(),
                &skill_catalog,
            );
            // 计划态下只向模型声明只读工具 + exit_plan_mode（ADR-003 双层防御第一层）
            let declarations = declarations_for(
                &self.router,
                self.plan_state.is_active(),
                has_model_skills,
            );

            // 2.5) 采样前检查 token 预算（ADR-001：「每轮采样前检查 token 预算触发 compaction」）。
            // 位置必须在采样之前：压缩的产物要对本轮请求可见才有意义，放轮末等于压了个寂寞
            // ——那一轮的上下文照样发出去，溢出照旧。
            if let Some(target) = crate::compaction::plan_if_needed(
                observed_input_tokens,
                &history,
                self.token_budget,
            ) {
                if let Err(message) = self
                    .compact_in_place(&mut history, &mut recorded, target, &instructions, &cancel)
                    .await
                {
                    emit(&sink, EventMsg::Error { message });
                    reason = TurnEndReasonLocal::Failed;
                    break 'outer;
                }
            }

            // 2.7) 会话引用展开（SH-090 批 2）：只作用于本步请求副本，档案与展示历史不动。
            // 轮次内的后续采样末尾不是 user 消息，不会重复展开。
            let expanded = match self.rollout.as_ref() {
                Some(writer) => {
                    let dir = writer.path().parent().map(std::path::Path::to_path_buf);
                    let id = writer.path().file_stem().map(|stem| stem.to_string_lossy().into_owned());
                    match (dir, id) {
                        (Some(dir), Some(id)) => crate::session_reference::prepare_request(&history, &dir, &id),
                        _ => Ok(None),
                    }
                }
                None => Ok(None),
            };
            let expanded = match expanded {
                Ok(value) => value,
                Err(message) => {
                    emit(&sink, EventMsg::Error { message });
                    reason = TurnEndReasonLocal::Failed;
                    break 'outer;
                }
            };
            // 2.8) 用户 `/name` 技能触发（SH-091）：只扫描原始历史末尾的用户消息，
            // 避免把会话引用展开注入的上下文当成用户手势。技能正文最后追加，
            // 离模型回答最近（与上游“背景在前、要执行的物料在后”一致）。
            let skill_injections = crate::skills::injections(&history, &self.workspace_root);
            let mut request_history: Vec<HistoryItem> =
                expanded.unwrap_or_else(|| history.clone());
            if !skill_injections.is_empty() {
                request_history.extend(skill_injections);
            }
            let mut stream = match self.backend.stream(&request_history, &declarations, &instructions).await {
                Ok(s) => s,
                Err(e) => {
                    emit(&sink, EventMsg::Error { message: e });
                    reason = TurnEndReasonLocal::Failed;
                    break 'outer;
                }
            };

            // 2) 消费流：转发增量、收集输出项
            let mut assistant_text = String::new();
            let mut reasoning_text = String::new();
            let mut full_message: Option<String> = None;
            let mut calls: Vec<(String, String, String)> = Vec::new(); // (name, arguments, call_id)
            let mut usage: Option<Usage> = None;
            loop {
                let mut stream_pin = stream.as_mut(); // Pin<&mut dyn S>，Next 借用其存活
                let next_item = stream_pin.next();
                tokio::select! {
                    _ = cancel.cancelled() => {
                        reason = TurnEndReasonLocal::Cancelled;
                        break 'outer;
                    }
                    item = next_item => match item {
                        None => break,
                        Some(ModelStreamEvent::OutputTextDelta { delta }) => {
                            assistant_text.push_str(&delta);
                            emit(&sink, EventMsg::AgentMessageDelta { delta });
                        }
                        // 思考增量单独成事件：它不是答案内容，前端要能折叠显示，
                        // FF-5 也要把它算进"首个可见反馈"（业界同一口径，见 latency_budget）
                        Some(ModelStreamEvent::ReasoningDelta { delta }) => {
                            reasoning_text.push_str(&delta);
                            emit(&sink, EventMsg::ReasoningDelta { delta });
                        }
                        Some(ModelStreamEvent::OutputItem(ResponseItem::Message { role: _, text })) => {
                            full_message = Some(text);
                        }
                        Some(ModelStreamEvent::OutputItem(ResponseItem::FunctionCall { name, arguments, call_id })) => {
                            calls.push((name, arguments, call_id));
                        }
                        Some(ModelStreamEvent::Completed { usage: u }) => {
                            usage = Some(u);
                        }
                        Some(ModelStreamEvent::Failed { message }) => {
                            emit(&sink, EventMsg::Error { message });
                            reason = TurnEndReasonLocal::Failed;
                            break 'outer;
                        }
                    }
                }
            }
            if let Some(u) = usage {
                observed_input_tokens = usize::try_from(u.input_tokens).unwrap_or(usize::MAX);
                emit(&sink, EventMsg::TokenUsage {
                    turn,
                    input_tokens: u.input_tokens,
                    cached_input_tokens: u.cached_input_tokens,
                    output_tokens: u.output_tokens,
                });
            }

            // 3) 助手输出入历史（完整消息优先，增量累计兜底）
            let text = full_message.unwrap_or(assistant_text);
            // reasoning 只要出现过就必须挂到这条 assistant 消息上回传（SH-083）：
            // thinking 档端点缺该字段直接 400。哪怕正文为空也要留——留了才有下一轮。
            let reasoning = if reasoning_text.trim().is_empty() {
                None
            } else {
                Some(std::mem::take(&mut reasoning_text))
            };
            if !text.trim().is_empty() || reasoning.is_some() {
                history.push(HistoryItem::Message { role: Role::Assistant, text, reasoning });
            }

            // 4) 无工具调用 → 任务完成
            if calls.is_empty() {
                break;
            }

            // 5) 本轮已到预算上限且仍需工具 → 预算耗尽
            if turn == self.max_turns {
                emit(&sink, EventMsg::Error { message: format!("已达最大轮数上限（{turn}）") });
                reason = TurnEndReasonLocal::BudgetExhausted;
                break;
            }

            // 6) 工具调用入历史 → 执行 → 输出回填
            for (name, arguments, call_id) in &calls {
                history.push(HistoryItem::FunctionCall {
                    name: name.clone(),
                    arguments: arguments.clone(),
                    call_id: call_id.clone(),
                });
            }
            for (name, arguments, call_id) in &calls {
                // 未知工具 = 协议违规：不经门禁直接拒绝回填（模型自我纠正），不打扰人工
                if self.router.get(name).is_none() {
                    emit(&sink, EventMsg::ToolCallBegin {
                        call_id: call_id.clone(),
                        tool: name.clone(),
                        args: serde_json::from_str(arguments)
                            .unwrap_or(serde_json::Value::String(arguments.clone())),
                    });
                    emit(&sink, EventMsg::ToolCallEnd {
                        call_id: call_id.clone(),
                        exit_code: None,
                        error: Some(format!("未知工具: {name}")),
                        result: Some(ToolCallResult::Error),
                    });
                    history.push(HistoryItem::FunctionCallOutput {
                        call_id: call_id.clone(),
                        output: format!("未知工具: {name}"),
                    });
                    continue;
                }

                emit(&sink, EventMsg::ToolCallBegin {
                    call_id: call_id.clone(),
                    tool: name.clone(),
                    args: serde_json::from_str(arguments)
                        .unwrap_or(serde_json::Value::String(arguments.clone())),
                });

                // 计划态执行守卫（双层防御；声明已收窄，此处兜底）
                if self.plan_state.is_active() && !policy::plan_mode_allows(name) {
                    emit(&sink, EventMsg::ToolCallEnd {
                        call_id: call_id.clone(),
                        exit_code: None,
                        error: Some("计划模式仅只读工具可用".into()),
                        result: Some(ToolCallResult::Error),
                    });
                    history.push(HistoryItem::FunctionCallOutput {
                        call_id: call_id.clone(),
                        output: "计划模式仅只读工具可用；请先调用 exit_plan_mode 提交计划".into(),
                    });
                    continue;
                }

                // 审批门禁（ADR-003）：裁决 → 询问回执 → 放行/拒绝
                let args_val =
                    serde_json::from_str(arguments).unwrap_or(serde_json::Value::String(arguments.clone()));
                match self
                    .broker
                    .gate(name, &args_val, call_id, self.policy, &sink, &cancel)
                    .await
                {
                    crate::permission::GateOutcome::Allowed => {}
                    crate::permission::GateOutcome::Denied => {
                        emit(&sink, EventMsg::ToolCallEnd {
                            call_id: call_id.clone(),
                            exit_code: None,
                            error: Some("用户拒绝".into()),
                            result: Some(ToolCallResult::Denied),
                        });
                        history.push(HistoryItem::FunctionCallOutput {
                            call_id: call_id.clone(),
                            output: "用户拒绝了该工具调用".into(),
                        });
                        continue;
                    }
                    crate::permission::GateOutcome::Cancelled => {
                        emit(&sink, EventMsg::ToolCallEnd {
                            call_id: call_id.clone(),
                            exit_code: None,
                            error: Some("已取消".into()),
                            result: Some(ToolCallResult::Cancelled),
                        });
                        reason = TurnEndReasonLocal::Cancelled;
                        break 'outer;
                    }
                }

                let tool_ctx = ToolContext::new(
                    self.workspace_root.clone(),
                    cancel.clone(),
                    call_id.clone(),
                    sink.clone(),
                    self.policy.sandbox,
                );
                let result = self.router.execute(name, args_val, &tool_ctx).await;
                let output_text = match &result {
                    Ok(out) => {
                        emit(&sink, EventMsg::ToolCallEnd {
                            call_id: call_id.clone(),
                            exit_code: Some(i32::from(!out.success)),
                            error: None,
                            // 成功/失败是两回事：exit_code 只表达是否非零退出，result 才区分语义
                            result: Some(if out.success {
                                ToolCallResult::Ok
                            } else {
                                ToolCallResult::Error
                            }),
                        });
                        out.output.clone()
                    }
                    Err(crate::tool_router::ToolError::Cancelled) => {
                        emit(&sink, EventMsg::ToolCallEnd {
                            call_id: call_id.clone(),
                            exit_code: None,
                            error: Some("已取消".into()),
                            result: Some(ToolCallResult::Cancelled),
                        });
                        reason = TurnEndReasonLocal::Cancelled;
                        break 'outer;
                    }
                    Err(e) => {
                        emit(&sink, EventMsg::ToolCallEnd {
                            call_id: call_id.clone(),
                            exit_code: None,
                            error: Some(e.to_string()),
                            result: Some(ToolCallResult::Error),
                        });
                        e.to_string()
                    }
                };
                history.push(HistoryItem::FunctionCallOutput {
                    call_id: call_id.clone(),
                    output: output_text,
                });
            }
            // 轮末落盘：崩溃只丢当前轮，之前各轮的档案已经 flush 到位
            self.flush_history(&sink, &history, &mut recorded);
        }

        // 覆盖所有 break 路径（取消/失败/预算耗尽）——它们都从这里收口
        self.flush_history(&sink, &history, &mut recorded);

        emit(&sink, EventMsg::TurnCompleted {
            turn: last_turn,
            reason: match reason {
                TurnEndReasonLocal::Completed => crate::events::TurnEndReason::Completed,
                TurnEndReasonLocal::Cancelled => crate::events::TurnEndReason::Cancelled,
                TurnEndReasonLocal::Failed => crate::events::TurnEndReason::Failed,
                TurnEndReasonLocal::BudgetExhausted => crate::events::TurnEndReason::BudgetExhausted,
            },
        });
        TurnSummary { turns: last_turn, reason: reason.into() }
    }
}

/// 本轮向模型声明的工具集合（ADR-003 计划态只读的第一层防御）：
/// 计划态下仅保留只读工具与 `exit_plan_mode`，写入/执行工具对模型不可见。
///
/// SH-091：没有任何 model-invocable 技能时，`skill` 工具不声明。工具仍注册在
/// 路由里（执行路径与审批测试照常），但模型看不到它，也就不会对空目录发起无效调用。
fn declarations_for(
    router: &ToolRouter,
    plan_active: bool,
    has_model_skills: bool,
) -> Vec<crate::model_client::ToolDeclaration> {
    let mut all = router.declarations();
    if !has_model_skills {
        all.retain(|declaration| declaration.name != "skill");
    }
    if !plan_active {
        return all;
    }
    all.into_iter()
        .filter(|d| policy::plan_mode_allows(&d.name))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnEndReasonLocal {
    Completed,
    Cancelled,
    Failed,
    BudgetExhausted,
}

impl From<TurnEndReasonLocal> for crate::events::TurnEndReason {
    fn from(r: TurnEndReasonLocal) -> Self {
        match r {
            TurnEndReasonLocal::Completed => Self::Completed,
            TurnEndReasonLocal::Cancelled => Self::Cancelled,
            TurnEndReasonLocal::Failed => Self::Failed,
            TurnEndReasonLocal::BudgetExhausted => Self::BudgetExhausted,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnSummary {
    pub turns: u32,
    pub reason: crate::events::TurnEndReason,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{EventSink, TurnEndReason};
    use crate::model_client::ToolDeclaration;
    use crate::tool_router::{ToolError, ToolExecutor, ToolOutput, ToolSpec};
    use serde_json::Value;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    // ---- 测试基建：捕获 sink + 脚本后端 + echo 工具 ----

    #[derive(Clone)]
    struct CapturingSink(Arc<Mutex<Vec<EventMsg>>>);

    impl CapturingSink {
        fn new() -> (Self, Arc<Mutex<Vec<EventMsg>>>) {
            let store = Arc::new(Mutex::new(Vec::new()));
            (Self(store.clone()), store)
        }
    }

    impl EventSink for CapturingSink {
        fn emit(&self, msg: &EventMsg) -> Result<(), String> {
            self.0.lock().unwrap().push(msg.clone());
            Ok(())
        }
    }

    struct ScriptedBackend(Mutex<VecDeque<Vec<ModelStreamEvent>>>);

    impl ScriptedBackend {
        fn new(turns: Vec<Vec<ModelStreamEvent>>) -> Self {
            Self(Mutex::new(turns.into()))
        }
    }

    #[async_trait::async_trait]
    impl ModelBackend for ScriptedBackend {
        async fn stream(
            &self,
            _history: &[HistoryItem],
            _tools: &[ToolDeclaration],
            _instructions: &str,
        ) -> Result<ModelStream, String> {
            let mut q = self.0.lock().unwrap();
            match q.pop_front() {
                Some(script) if !script.is_empty() => Ok(Box::pin(futures::stream::iter(script))),
                // 空脚本 = 永不产出的事件流（取消/超时类确定性测试用）
                _ => Ok(Box::pin(futures::stream::pending())),
            }
        }
    }

    struct EchoTool {
        spec: ToolSpec,
    }

    impl Default for EchoTool {
        fn default() -> Self {
            // 命名 read_file → 风险 read → 门禁自动放行（回显工具用于循环测试）
            Self {
                spec: ToolSpec::new("read_file", "回显", serde_json::json!({"type":"object"})),
            }
        }
    }

    #[async_trait::async_trait]
    impl ToolExecutor for EchoTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }
        async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::ok(format!("echo:{args}")))
        }
    }

    fn make_loop(
        turns: Vec<Vec<ModelStreamEvent>>,
    ) -> (
        AgentLoop,
        Arc<Mutex<Vec<EventMsg>>>,
        CancellationToken,
        Arc<ApprovalBroker>,
    ) {
        let mut router = ToolRouter::new();
        router.register(Arc::new(EchoTool::default()));
        let (_sink, store) = CapturingSink::new();
        let broker = Arc::new(ApprovalBroker::new());
        let agent = AgentLoop {
            backend: Arc::new(ScriptedBackend::new(turns)),
            router: Arc::new(router),
            broker: broker.clone(),
            plan_state: Arc::new(PlanState::new()),
            workspace_root: std::env::temp_dir(),
            max_turns: 3,
            // 既有循环用例一律不启用压缩：它们断言的是轮次与工具回填行为，
            // 混进压缩会让"为什么这轮历史变短"变成隐性变量。
            token_budget: 0,
            policy: policy::SessionPolicy {
                approval: crate::config::ApprovalMode::Ask,
                sandbox: crate::config::SandboxMode::WorkspaceWrite,
            },
            rollout: None,
            rollout_broke: std::sync::atomic::AtomicBool::new(false),
        };
        (agent, store, CancellationToken::new(), broker)
    }

    fn usage_ev() -> ModelStreamEvent {
        ModelStreamEvent::Completed { usage: Usage { input_tokens: 10, cached_input_tokens: None, output_tokens: 5 } }
    }

    fn fn_call_ev() -> ModelStreamEvent {
        ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
            name: "read_file".into(),
            arguments: r#"{"x":1}"#.into(),
            call_id: "c1".into(),
        })
    }

    // ---- 用例 ----

    #[tokio::test]
    async fn single_turn_completion_emits_expected_events() {
        let (agent, store, cancel, _broker) = make_loop(vec![
            vec![
                ModelStreamEvent::OutputTextDelta { delta: "你好".into() },
                usage_ev(),
            ],
        ]);
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.reason, TurnEndReason::Completed);
        let events = store.lock().unwrap();
        let tags: Vec<&str> = events.iter().map(|e| match e {
            EventMsg::TurnStarted { .. } => "turn_started",
            EventMsg::AgentMessageDelta { .. } => "delta",
            EventMsg::TokenUsage { .. } => "usage",
            EventMsg::TurnCompleted { .. } => "turn_completed",
            other => panic!("意外事件: {other:?}"),
        }).collect();
        assert_eq!(tags, ["turn_started", "delta", "usage", "turn_completed"]);
    }

    #[tokio::test]
    async fn tool_call_cycle_then_complete() {
        let (agent, store, cancel, _broker) = make_loop(vec![
            vec![fn_call_ev(), usage_ev()],
            vec![ModelStreamEvent::OutputTextDelta { delta: "完成".into() }, usage_ev()],
        ]);
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.turns, 2);
        assert_eq!(summary.reason, TurnEndReason::Completed);
        let events = store.lock().unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            EventMsg::ToolCallBegin { call_id, tool, .. } if call_id == "c1" && tool == "read_file"
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            EventMsg::ToolCallEnd { call_id, exit_code: Some(0), error: None, result: Some(ToolCallResult::Ok) } if call_id == "c1"
        )));
    }

    /// 结果语义探针：同一个假工具按需返回失败/取消，用于钉住 emit 分支的 `result` 取值。
    #[derive(Debug, Clone, Copy)]
    enum ToolBehavior {
        Fail,
        Cancel,
    }

    fn make_loop_with_read_tool(
        behavior: ToolBehavior,
        turns: Vec<Vec<ModelStreamEvent>>,
    ) -> (
        AgentLoop,
        Arc<Mutex<Vec<EventMsg>>>,
        CancellationToken,
        Arc<ApprovalBroker>,
    ) {
        struct BehaviorTool {
            spec: ToolSpec,
            behavior: ToolBehavior,
        }
        #[async_trait::async_trait]
        impl ToolExecutor for BehaviorTool {
            fn spec(&self) -> &ToolSpec {
                &self.spec
            }
            async fn execute(&self, _a: Value, _c: &ToolContext) -> Result<ToolOutput, ToolError> {
                match self.behavior {
                    ToolBehavior::Fail => Ok(ToolOutput::fail("命令以非零状态退出")),
                    ToolBehavior::Cancel => Err(ToolError::Cancelled),
                }
            }
        }
        // 命名 read_file → 风险 read → 门禁自动放行，避免测试里再走审批
        let (mut agent, store, cancel, broker) = make_loop(turns);
        let mut r = ToolRouter::new();
        r.register(Arc::new(BehaviorTool {
            spec: ToolSpec::new("read_file", "假读工具", serde_json::json!({"type":"object"})),
            behavior,
        }));
        agent.router = Arc::new(r);
        (agent, store, cancel, broker)
    }

    #[tokio::test]
    async fn failed_tool_emits_result_error() {
        let (agent, store, cancel, _broker) = make_loop_with_read_tool(
            ToolBehavior::Fail,
            vec![
                vec![fn_call_ev(), usage_ev()],
                vec![ModelStreamEvent::OutputTextDelta { delta: "继续".into() }, usage_ev()],
            ],
        );
        agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        let events = store.lock().unwrap();
        // 非零退出：exit_code 是证据字段，result 才是语义判据
        assert!(events.iter().any(|e| matches!(
            e,
            EventMsg::ToolCallEnd { call_id, exit_code: Some(1), result: Some(ToolCallResult::Error), .. }
                if call_id == "c1"
        )));
    }

    #[tokio::test]
    async fn cancelled_tool_emits_result_cancelled() {
        let (agent, store, cancel, _broker) = make_loop_with_read_tool(
            ToolBehavior::Cancel,
            vec![vec![fn_call_ev(), usage_ev()]],
        );
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.reason, TurnEndReason::Cancelled);
        let events = store.lock().unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            EventMsg::ToolCallEnd { call_id, exit_code: None, result: Some(ToolCallResult::Cancelled), .. }
                if call_id == "c1"
        )));
    }

    #[tokio::test]
    async fn budget_exhausted_when_tools_keep_coming() {
        let (agent, store, cancel, _broker) = make_loop(vec![
            vec![fn_call_ev(), usage_ev()],
            vec![fn_call_ev(), usage_ev()],
            vec![fn_call_ev(), usage_ev()],
        ]);
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.reason, TurnEndReason::BudgetExhausted);
        assert!(store.lock().unwrap().iter().any(|e| matches!(
            e,
            EventMsg::Error { message } if message.contains("最大轮数")
        )));
    }

    #[tokio::test]
    async fn cancelled_token_yields_cancelled_completion() {
        // 空脚本 = 挂起的事件流：预取消后 select! 只有取消分支就绪（确定性）
        let (agent, store, cancel, _broker) = make_loop(vec![vec![]]);
        cancel.cancel();
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.reason, TurnEndReason::Cancelled);
        assert!(store.lock().unwrap().iter().any(|e| matches!(
            e,
            EventMsg::TurnCompleted { reason: TurnEndReason::Cancelled, .. }
        )));
    }

    #[tokio::test]
    async fn model_failure_maps_to_failed() {
        let (agent, store, cancel, _broker) = make_loop(vec![vec![ModelStreamEvent::Failed { message: "boom".into() }]]);
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.reason, TurnEndReason::Failed);
        assert!(store.lock().unwrap().iter().any(|e| matches!(
            e,
            EventMsg::Error { message } if message == "boom"
        )));
    }

    #[tokio::test]
    async fn unknown_tool_error_backfills_and_continues() {
        let (agent, store, cancel, _broker) = make_loop(vec![
            vec![ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                name: "no_such_tool".into(),
                arguments: "{}".into(),
                call_id: "c9".into(),
            }), usage_ev()],
            vec![ModelStreamEvent::OutputTextDelta { delta: "已纠正".into() }, usage_ev()],
        ]);
        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.reason, TurnEndReason::Completed);
        let events = store.lock().unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            EventMsg::ToolCallEnd { call_id, error: Some(msg), .. } if call_id == "c9" && msg.contains("未知工具")
        )));
    }

    #[tokio::test]
    async fn destructive_tool_requires_approval_and_deny_blocks_execution() {
        // wipe_disk 是注册过的假想破坏性工具（未知工具名走路由拒绝路径，见另一用例）
        let (agent, store, cancel, broker) = make_loop_with_destructive(vec![
            vec![ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                name: "wipe_disk".into(),
                arguments: "{}".into(),
                call_id: "c7".into(),
            }), usage_ev()],
            vec![ModelStreamEvent::OutputTextDelta { delta: "已纠正".into() }, usage_ev()],
        ]);
        let sink = Arc::new(CapturingSink(store.clone()));
        let run = tokio::spawn(async move { agent.run_task("任务", sink, cancel).await });
        // 等待审批请求出现后回执「拒绝」
        for _ in 0..300 {
            if broker.has_pending("appr-c7") {
                assert!(broker.resolve("appr-c7", crate::events::ApprovalDecision::Deny));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let summary = run.await.unwrap();
        assert_eq!(summary.reason, TurnEndReason::Completed);
        let events = store.lock().unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            EventMsg::ExecApprovalRequest { request_id, risk: crate::events::RiskLevel::Destructive, .. }
                if request_id == "appr-c7"
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            EventMsg::ToolCallEnd { call_id, error: Some(msg), result: Some(ToolCallResult::Denied), .. }
                if call_id == "c7" && msg.contains("用户拒绝")
        )));
    }

    fn make_loop_with_destructive(
        turns: Vec<Vec<ModelStreamEvent>>,
    ) -> (
        AgentLoop,
        Arc<Mutex<Vec<EventMsg>>>,
        CancellationToken,
        Arc<ApprovalBroker>,
    ) {
        struct WipeTool {
            spec: ToolSpec,
        }
        impl Default for WipeTool {
            fn default() -> Self {
                Self {
                    spec: ToolSpec::new("wipe_disk", "破坏性", serde_json::json!({"type":"object"})),
                }
            }
        }
        #[async_trait::async_trait]
        impl ToolExecutor for WipeTool {
            fn spec(&self) -> &ToolSpec {
                &self.spec
            }
            async fn execute(&self, _a: Value, _c: &ToolContext) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput::ok("wiped"))
            }
        }
        let (mut agent, store, cancel, broker) = make_loop(turns);
        let mut router_raw = ToolRouter::new();
        router_raw.register(Arc::new(WipeTool::default()));
        // 直接补注册（保持既有 router 引用结构）
        let router = Arc::new({
            let mut r = ToolRouter::new();
            r.register(Arc::new(EchoTool::default()));
            r.register(Arc::new(WipeTool::default()));
            r
        });
        agent.router = router;
        drop(router_raw);
        (agent, store, cancel, broker)
    }

    // ---- 系统提示下发（docs/04 §6.1）----

    /// 记录 (系统提示, 历史长度) 的后端：验证每轮下发、且不写入 history。
    type Recorded = Arc<Mutex<Vec<(String, usize)>>>;

    struct PromptRecordingBackend(Recorded);

    #[async_trait::async_trait]
    impl ModelBackend for PromptRecordingBackend {
        async fn stream(
            &self,
            history: &[HistoryItem],
            _tools: &[ToolDeclaration],
            instructions: &str,
        ) -> Result<ModelStream, String> {
            self.0.lock().unwrap().push((instructions.to_string(), history.len()));
            // 无工具调用 → 单轮即完成
            Ok(Box::pin(futures::stream::iter(vec![ModelStreamEvent::OutputTextDelta {
                delta: "ok".into(),
            }])))
        }
    }

    fn agent_with_recording(
        workspace: PathBuf,
        plan_active: bool,
    ) -> (AgentLoop, Recorded, Arc<Mutex<Vec<EventMsg>>>) {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let backend = Arc::new(PromptRecordingBackend(recorded.clone()));
        let (_sink, store) = CapturingSink::new();
        let plan_state = Arc::new(PlanState::new());
        if plan_active {
            plan_state.activate();
        }
        let agent = AgentLoop {
            backend,
            router: Arc::new(ToolRouter::new()),
            broker: Arc::new(ApprovalBroker::new()),
            plan_state,
            workspace_root: workspace,
            max_turns: 2,
            token_budget: 0,
            policy: policy::SessionPolicy {
                approval: crate::config::ApprovalMode::Ask,
                sandbox: crate::config::SandboxMode::WorkspaceWrite,
            },
            rollout: None,
            rollout_broke: std::sync::atomic::AtomicBool::new(false),
        };
        (agent, recorded, store)
    }

    #[tokio::test]
    async fn system_instruction_sent_to_model_and_kept_out_of_history() {
        let ws = std::env::temp_dir();
        let (agent, recorded, store) = agent_with_recording(ws.clone(), false);
        agent
            .run_task("任务", Arc::new(CapturingSink(store)), CancellationToken::new())
            .await;

        let calls = recorded.lock().unwrap();
        assert_eq!(calls.len(), 1, "单轮任务应只采样一次");
        let (prompt, history_len) = &calls[0];
        assert!(prompt.contains(&ws.display().to_string()), "提示须含工作区路径");
        assert!(prompt.contains("工作区写入"), "提示须反映沙箱模式");
        assert!(!prompt.contains("当前状态：计划模式"));
        // 提示不写入 history：历史里只有用户这一条
        assert_eq!(*history_len, 1, "系统提示不得混入 history");
    }

    #[tokio::test]
    async fn system_instruction_reflects_plan_mode() {
        let (agent, recorded, store) = agent_with_recording(std::env::temp_dir(), true);
        agent
            .run_task("先做计划", Arc::new(CapturingSink(store)), CancellationToken::new())
            .await;
        let calls = recorded.lock().unwrap();
        let (prompt, _) = &calls[0];
        assert!(prompt.contains("当前状态：计划模式"), "计划态必须写入提示");
        assert!(prompt.contains("exit_plan_mode"), "计划态须指引退出路径");
    }

    // ---- Plan Mode（ADR-003 只读计划态）----

    #[test]
    fn plan_mode_declarations_hide_write_and_execute_tools() {
        let mut router = ToolRouter::new();
        crate::tools::register_builtin(&mut router, Arc::new(PlanState::new()));

        let names = |plan: bool| -> Vec<String> {
            declarations_for(&router, plan, true).into_iter().map(|d| d.name).collect()
        };
        let planned = names(true);
        assert!(planned.contains(&"read_file".to_string()));
        assert!(planned.contains(&"list_dir".to_string()));
        assert!(planned.contains(&"search".to_string()));
        assert!(planned.contains(&"exit_plan_mode".to_string()));
        assert!(!planned.contains(&"apply_patch".to_string()), "计划态不得向模型声明写入工具");
        assert!(!planned.contains(&"unified_exec".to_string()), "计划态不得向模型声明命令执行工具");
        assert!(!planned.contains(&"enter_plan_mode".to_string()), "已在计划态，无需重复进入");

        let full = names(false);
        assert!(full.contains(&"apply_patch".to_string()));
        assert!(full.contains(&"unified_exec".to_string()));
    }

    #[tokio::test]
    async fn plan_mode_guard_blocks_write_tool_even_if_called() {
        struct PatchEcho {
            spec: ToolSpec,
        }
        #[async_trait::async_trait]
        impl ToolExecutor for PatchEcho {
            fn spec(&self) -> &ToolSpec {
                &self.spec
            }
            async fn execute(&self, _a: Value, _c: &ToolContext) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput::ok("patched"))
            }
        }
        let (mut agent, store, cancel, _broker) = make_loop(vec![
            vec![ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                name: "apply_patch".into(),
                arguments: r#"{"patch":"*** Begin Patch"}"#.into(),
                call_id: "c5".into(),
            }), usage_ev()],
            vec![ModelStreamEvent::OutputTextDelta { delta: "改为只读调研".into() }, usage_ev()],
        ]);
        // 只注册写入工具（read_file 回显 + apply_patch），并置为计划态
        let mut router = ToolRouter::new();
        router.register(Arc::new(EchoTool::default()));
        router.register(Arc::new(PatchEcho {
            spec: ToolSpec::new("apply_patch", "写入", serde_json::json!({"type":"object"})),
        }));
        agent.router = Arc::new(router);
        let plan_state = Arc::new(PlanState::new());
        plan_state.activate();
        agent.plan_state = plan_state;

        let summary = agent.run_task("任务", Arc::new(CapturingSink(store.clone())), cancel).await;
        assert_eq!(summary.reason, TurnEndReason::Completed);
        let events = store.lock().unwrap();
        assert!(
            events.iter().any(|e| matches!(
                e,
                EventMsg::ToolCallEnd { call_id, error: Some(msg), .. }
                    if call_id == "c5" && msg.contains("计划模式")
            )),
            "计划态下写入工具必须被执行守卫拒绝"
        );
        // 未被真正执行：没有「成功」的 ToolCallEnd
        assert!(!events.iter().any(|e| matches!(
            e,
            EventMsg::ToolCallEnd { call_id, exit_code: Some(0), error: None, .. } if call_id == "c5"
        )));
        // 也不应触发审批（守卫先于审批门禁）
        assert!(!events.iter().any(|e| matches!(e, EventMsg::ExecApprovalRequest { .. })));
    }

    /// SH-090 批 2：会话 mention 只在本步请求副本里展开——首条 user 消息剥离 mention，
    /// 紧跟一条 `<referenced-sessions>` 上下文；持久化历史（`run_with_history` 的入参）不动。
    #[tokio::test]
    async fn session_reference_mentions_expand_into_the_request_only() {
        use crate::rollout::{ConfigSnapshot, RolloutItem, RolloutWriter};

        struct RecordingBackend {
            seen: Arc<Mutex<Vec<Vec<HistoryItem>>>>,
            script: Mutex<VecDeque<Vec<ModelStreamEvent>>>,
        }

        #[async_trait::async_trait]
        impl ModelBackend for RecordingBackend {
            async fn stream(
                &self,
                history: &[HistoryItem],
                _tools: &[ToolDeclaration],
                _instructions: &str,
            ) -> Result<ModelStream, String> {
                self.seen.lock().unwrap().push(history.to_vec());
                let mut queue = self.script.lock().unwrap();
                match queue.pop_front() {
                    Some(script) => Ok(Box::pin(futures::stream::iter(script))),
                    None => Ok(Box::pin(futures::stream::pending())),
                }
            }
        }

        fn meta(id: &str) -> RolloutItem {
            RolloutItem::SessionMeta {
                session_id: id.into(),
                agent_id: id.into(),
                parent_id: None,
                created_at: 1,
                config_snapshot: ConfigSnapshot {
                    provider: "p".into(),
                    base_url: String::new(),
                    model: "m".into(),
                    sandbox: "WorkspaceWrite".into(),
                    approval: "Ask".into(),
                    workspace_root: "D:/ws".into(),
                },
            }
        }

        let dir = std::env::temp_dir().join(format!(
            "shaniu-sref-loop-{}-{}",
            std::process::id(),
            ulid::Ulid::generate()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let writer = Arc::new(RolloutWriter::create(&dir, &meta("self")).unwrap());
        let referenced = RolloutWriter::create(&dir, &meta("ref-1")).unwrap();
        referenced
            .append(&RolloutItem::ResponseItem {
                item: HistoryItem::Message { role: Role::User, text: "被引用的问题".into(), reasoning: None },
            })
            .unwrap();
        referenced
            .append(&RolloutItem::ResponseItem {
                item: HistoryItem::Message { role: Role::Assistant, text: "被引用的回答".into(), reasoning: None },
            })
            .unwrap();

        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut router = ToolRouter::new();
        router.register(Arc::new(EchoTool::default()));
        let broker = Arc::new(ApprovalBroker::new());
        let agent = AgentLoop {
            backend: Arc::new(RecordingBackend {
                seen: seen.clone(),
                script: Mutex::new(VecDeque::from([vec![
                    ModelStreamEvent::OutputTextDelta { delta: "收到".into() },
                    usage_ev(),
                ]])),
            }),
            router: Arc::new(router),
            broker: broker.clone(),
            plan_state: Arc::new(PlanState::new()),
            workspace_root: std::env::temp_dir(),
            max_turns: 1,
            token_budget: 0,
            policy: policy::SessionPolicy {
                approval: crate::config::ApprovalMode::Ask,
                sandbox: crate::config::SandboxMode::WorkspaceWrite,
            },
            rollout: Some(writer),
            rollout_broke: std::sync::atomic::AtomicBool::new(false),
        };
        let (sink, _store) = CapturingSink::new();
        let task = format!(
            "请参考 @[旧会话]({}) 回答",
            crate::session_reference::encode_uri("ref-1")
        );
        agent
            .run_with_history(
                vec![HistoryItem::Message { role: Role::User, text: task, reasoning: None }],
                0,
                Arc::new(sink),
                CancellationToken::new(),
            )
            .await;
        let captured = seen.lock().unwrap();
        let request = captured.first().expect("后端应收到请求");
        assert_eq!(request.len(), 2, "原消息 + 引用上下文");
        match &request[0] {
            HistoryItem::Message { text, .. } => assert_eq!(text, "请参考 @旧会话 回答"),
            _ => panic!("首条应是剥离 mention 的用户消息"),
        }
        match &request[1] {
            HistoryItem::Message { role: Role::User, text, .. } => {
                assert!(text.starts_with("## Referenced sessions"));
                assert!(text.contains("被引用的回答"));
            }
            _ => panic!("第二条应是引用上下文"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
