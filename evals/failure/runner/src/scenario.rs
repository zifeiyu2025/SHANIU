//! 单题执行器：把题单里的一条 injection 剧本跑成可判定的观测。
//!
//! 观测只记录宿主侧事实：请求序列与时间、事件流、回合结局、落盘档案、resume 结果。
//! 判据由 `evals/failure/judge/run.ps1` 对这份观测做裁决。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{json, Value};

use shaniu_lib::agent_loop::{AgentLoop, ResponsesBackend, TreeBudget};
use shaniu_lib::config::{ApprovalMode, SandboxMode};
use shaniu_lib::events::{EventMsg, EventSink};
use shaniu_lib::model_client::{HistoryItem, ProviderConfig, RetryPolicy, Role};
use shaniu_lib::permission::{policy, ApprovalBroker};
use shaniu_lib::rollout::{resume, ConfigSnapshot, RolloutItem, RolloutWriter};
use shaniu_lib::tool_router::{PlanState, ToolRouter};
use tokio_util::sync::CancellationToken;

use crate::mock::{head, Action, MockServer};

#[derive(Debug, Deserialize)]
pub struct TaskFile {
    pub tasks: Vec<TaskSpec>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TaskSpec {
    pub id: String,
    #[serde(default)]
    pub title: String,
    /// 给人看的注入说明，不进入模型上下文。
    #[serde(default)]
    pub description: String,
    /// 本回合用户输入。
    #[serde(default)]
    pub task: String,
    pub setup: SetupSpec,
    pub inject: InjectSpec,
    #[serde(default)]
    pub probes: Vec<String>,
    pub judge: JudgeSpec,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SetupSpec {
    /// none | overflow_seed | long_session
    pub history: String,
    pub token_budget: usize,
    pub manual_token_budget: usize,
    pub rollout: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InjectSpec {
    pub max_retries: u32,
    pub initial_delay_ms: u64,
    pub max_delay_ms: u64,
    #[serde(default)]
    pub jitter_ratio: f64,
    pub steps: Vec<Action>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JudgeSpec {
    pub key: String,
    /// pass = 期望绿；gap = 已知缺口，期望红
    pub expect: String,
    #[serde(default)]
    pub sources: Vec<String>,
}

#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<EventMsg>>,
}

impl EventSink for Recorder {
    fn emit(&self, message: &EventMsg) -> Result<(), String> {
        self.events
            .lock()
            .map_err(|_| "事件表锁中毒".to_string())?
            .push(message.clone());
        Ok(())
    }
}

pub async fn run(
    spec: &TaskSpec,
    run_id: &str,
    work_root: &Path,
    repo_commit: &str,
) -> Result<Value, String> {
    let server = MockServer::spawn(spec.inject.steps.clone(), spec.probes.clone())
        .await
        .map_err(|error| format!("mock 端点启动失败: {error}"))?;
    let provider = ProviderConfig {
        provider: "openai-responses".to_string(),
        base_url: format!("http://127.0.0.1:{}/v1", server.port),
        model: "failure-eval".to_string(),
        reasoning_effort: None,
        headers: Default::default(),
    };

    let workspace = work_root.join(&spec.id);
    std::fs::create_dir_all(&workspace).map_err(|error| format!("工作区创建失败: {error}"))?;

    let rollout = if spec.setup.rollout {
        let dir = work_root.join(format!("{}-rollout", spec.id));
        let meta = RolloutItem::SessionMeta {
            session_id: format!("{run_id}-{}", spec.id),
            agent_id: format!("{run_id}-{}", spec.id),
            parent_id: None,
            created_at: now_ms(),
            config_snapshot: ConfigSnapshot {
                provider: provider.provider.clone(),
                base_url: provider.base_url.clone(),
                model: provider.model.clone(),
                sandbox: "workspace_write".to_string(),
                approval: "on_request".to_string(),
                workspace_root: workspace.display().to_string(),
            },
        };
        Some(Arc::new(
            RolloutWriter::create(&dir, &meta).map_err(|error| format!("档案创建失败: {error}"))?,
        ))
    } else {
        None
    };

    // 预置历史必须先落盘，再把「已持久化条数」交给引擎；否则压缩锚点会排到历史之前，
    // resume 无法重放。这正是长会话题要钉住的调用方契约。
    let history = seed_history(&spec.setup.history, &spec.task);
    let seeded_len = history.len();
    if let Some(writer) = &rollout {
        for item in &history {
            writer
                .append(&RolloutItem::ResponseItem { item: item.clone(), at_ms: now_ms() })
                .map_err(|error| format!("预置历史落盘失败: {error}"))?;
        }
    }

    let recorder = Arc::new(Recorder::default());
    let retry = RetryPolicy {
        max_retries: spec.inject.max_retries,
        initial_delay: Duration::from_millis(spec.inject.initial_delay_ms),
        max_delay: Duration::from_millis(spec.inject.max_delay_ms),
        jitter_ratio: spec.inject.jitter_ratio,
    };
    let agent = AgentLoop {
        backend: Arc::new(ResponsesBackend {
            provider: provider.clone(),
            api_key: "failure-eval-key".to_string(),
        }),
        router: Arc::new(ToolRouter::new()),
        broker: Arc::new(ApprovalBroker::new()),
        plan_state: Arc::new(PlanState::new()),
        workspace_root: workspace.clone(),
        max_turns: 3,
        token_budget: spec.setup.token_budget,
        manual_token_budget: spec.setup.manual_token_budget,
        tree_budget: TreeBudget::unlimited(),
        policy: Arc::new(policy::SessionPolicyHandle::new(policy::SessionPolicy {
            approval: ApprovalMode::OnRequest,
            sandbox: SandboxMode::WorkspaceWrite,
        })),
        rollout: rollout.clone(),
        rollout_broke: std::sync::atomic::AtomicBool::new(false),
        session_id: None,
        jobs: None,
        stop_hook: None,
        verify: Default::default(),
        retry,
    };

    let summary = agent
        .run_with_history(history, seeded_len, recorder.clone(), CancellationToken::new())
        .await;

    let events: Vec<Value> = recorder
        .events
        .lock()
        .map_err(|_| "事件表锁中毒".to_string())?
        .iter()
        .map(|event| serde_json::to_value(event).unwrap_or(Value::Null))
        .collect();
    let requests = server
        .requests
        .lock()
        .map_err(|_| "请求表锁中毒".to_string())?
        .clone();
    let gaps: Vec<u64> = requests
        .windows(2)
        .map(|pair| pair[1].at_ms.saturating_sub(pair[0].at_ms))
        .collect();
    let turn_error = events
        .iter()
        .find(|event| event["type"] == "turn_completed")
        .and_then(|event| event.get("error").cloned())
        .filter(|error| !error.is_null());
    let reason = serde_json::to_value(summary.reason)
        .unwrap_or(Value::Null)
        .as_str()
        .unwrap_or("unknown")
        .to_string();

    let rollout_observation = match &rollout {
        Some(writer) => Some(observe_rollout(writer.path(), seeded_len)),
        None => None,
    };

    Ok(json!({
        "id": spec.id,
        "title": spec.title,
        "description": spec.description,
        "expect": spec.judge.expect,
        "judge_key": spec.judge.key,
        "sources": spec.judge.sources,
        "repo_commit": repo_commit,
        "setup": {
            "history": spec.setup.history,
            "token_budget": spec.setup.token_budget,
            "manual_token_budget": spec.setup.manual_token_budget,
            "rollout": spec.setup.rollout,
        },
        "inject": {
            "max_retries": spec.inject.max_retries,
            "initial_delay_ms": spec.inject.initial_delay_ms,
            "max_delay_ms": spec.inject.max_delay_ms,
            "jitter_ratio": spec.inject.jitter_ratio,
            "steps": spec.inject.steps.iter().map(Action::name).collect::<Vec<_>>(),
        },
        "requests": requests,
        "request_gaps_ms": gaps,
        "events": events,
        "turn": { "reason": reason, "turns": summary.turns },
        "turn_error": turn_error,
        "rollout": rollout_observation,
    }))
}

/// 题单里的预置历史。最后一条必须是 user，等价于用户本回合的输入。
fn seed_history(kind: &str, task_text: &str) -> Vec<HistoryItem> {
    let task = if task_text.trim().is_empty() {
        "执行失败面测试任务：请直接作答。".to_string()
    } else {
        task_text.to_string()
    };
    match kind {
        "overflow_seed" => vec![
            HistoryItem::Message {
                role: Role::User,
                text: "背景资料：这是一段会被摘要替换的历史。".to_string(),
                reasoning: None,
            },
            HistoryItem::Message {
                role: Role::Assistant,
                text: format!("已读取背景。SEED_OVERFLOW_SHADOW {}", "细节".repeat(400)),
                reasoning: None,
            },
            HistoryItem::Message { role: Role::User, text: task, reasoning: None },
        ],
        "long_session" => {
            let mut history = Vec::new();
            for turn in 0..6 {
                history.push(HistoryItem::Message {
                    role: Role::User,
                    text: format!("第 {turn} 轮：{}", "历史细节".repeat(140)),
                    reasoning: None,
                });
                let marker = if turn == 0 { "SEED_LONG_SHADOW " } else { "" };
                history.push(HistoryItem::Message {
                    role: Role::Assistant,
                    text: format!("第 {turn} 轮已处理：{marker}{}", "结论".repeat(60)),
                    reasoning: None,
                });
            }
            history.push(HistoryItem::Message { role: Role::User, text: task, reasoning: None });
            history
        }
        _ => vec![HistoryItem::Message { role: Role::User, text: task, reasoning: None }],
    }
}

fn observe_rollout(path: &Path, seeded_len: usize) -> Value {
    let mut items = Vec::new();
    let mut read_error = Value::Null;
    match std::fs::read_to_string(path) {
        Ok(text) => {
            for line in text.lines() {
                let Ok(item) = serde_json::from_str::<Value>(line) else {
                    items.push(json!({"type": "unparsable"}));
                    continue;
                };
                let kind = item["type"].as_str().unwrap_or("unknown");
                let summary = match kind {
                    "response_item" => {
                        let inner = &item["item"];
                        json!({
                            "type": kind,
                            "item_type": inner["type"].clone(),
                            "role": inner["role"].clone(),
                            "text_head": head(inner["text"].as_str().unwrap_or(""), 160),
                        })
                    }
                    "compacted" => json!({
                        "type": kind,
                        "range_start": item["range_start"].clone(),
                        "range_end": item["range_end"].clone(),
                        "summary_head": head(item["summary"].as_str().unwrap_or(""), 160),
                        "summary_chars": item["summary"].as_str().unwrap_or("").chars().count(),
                    }),
                    "turn_end" => json!({"type": kind, "reason": item["reason"].clone()}),
                    _ => json!({ "type": kind }),
                };
                items.push(summary);
            }
        }
        Err(error) => read_error = Value::String(format!("{error}")),
    }
    let resumed = match resume::resume(path) {
        Ok(resumed) => {
            let first_user = first_user_full(&resumed.history);
            json!({
                "ok": true,
                "applied_compactions": resumed.applied_compactions,
                "history_len": resumed.history.len(),
                "seeded_len": seeded_len,
                "first_user_text_head": head(&first_user, 160),
                "first_user_chars": first_user.chars().count(),
                "first_user_contains_summary": first_user.contains("【交接摘要】"),
                "damaged_lines": resumed.damaged_lines,
                "torn_tail_lines": resumed.torn_tail_lines,
            })
        }
        Err(error) => json!({"ok": false, "error": format!("{error}")}),
    };
    json!({
        "enabled": true,
        "path": path.display().to_string(),
        "items": items,
        "read_error": read_error,
        "resume": resumed,
    })
}

fn first_user_full(history: &[HistoryItem]) -> String {
    history
        .iter()
        .find_map(|item| match item {
            HistoryItem::Message { role: Role::User, text, .. } => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// 供 main 组装结果文件路径。
pub fn results_dir_for(tasks_path: &Path) -> PathBuf {
    tasks_path
        .parent()
        .map(|parent| parent.join("results"))
        .unwrap_or_else(|| PathBuf::from("results"))
}
