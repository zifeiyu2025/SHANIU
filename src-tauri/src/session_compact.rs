//! 会话外按需压缩（`/compact` 命令，SH-115）。
//!
//! [`crate::agent_loop`] 的自动压缩（SH-032）跑在回合内：历史超预算时先总结再继续。
//! 这里的 `/compact` 把同一套机制暴露成**回合外**的手动命令：重放档案 → 规划目标 →
//! 发一次不带工具的总结请求 → 写 `Compacted` 锚点。锚点格式、回放语义、"摘要必须
//! 以 user 角色重放"（thinking 档 reasoning_content 约束）全部复用既有约定，零新格式。
//!
//! 与回合内路径（`agent_loop::compact_in_place`）刻意共享 [`crate::compaction`] 的
//! 纯计算层（`plan_manual` / `build_request` / `extract_summary`），但请求执行各自实现：
//! 回合内走 backend trait（有取消令牌），这里是一次独立的辅助请求（对齐
//! `session_title` 的形态：`stream_body` + 超时掐断 + 专用系统提示）。系统提示不复用
//! `prompt::system_instruction`——那份提示词讲的是"在这个工作区用工具干活"，对一次
//! 无工具的总结请求是纯噪声；摘要风格由 SUMMARIZATION_PROMPT（末条 user 消息）决定，
//! 与回合内同源。

use futures::StreamExt;

use crate::compaction::{self, Target};
use crate::model_client::{self, HistoryItem, ModelStreamEvent};
use crate::model_client::provider::ProviderConfig;
use crate::rollout::resume::Resumed;
use crate::rollout::RolloutItem;

/// 一次成功手动压缩的产出。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactOutcome {
    pub summary: String,
    /// 被摘要取代的历史条目数（`range_end - range_start`）。
    pub shadowed_items: usize,
    /// 被取代区间的字节估算 token 数（与预算判据同口径，供结果文案用）。
    pub shadowed_tokens: usize,
}

/// 总结请求的系统提示：只约束"输出交接摘要"这一件事。
const COMPACT_SYSTEM: &str =
    "你是对话压缩器。根据用户消息里的对话与工具历史，严格按其要求输出交接摘要，不执行任何任务。";

/// 辅助请求的端到端超时：标题生成用配置值（秒级），压缩的输入是整段被压区间、输出
/// 是五段式摘要，量级大得多；180s 与回合内压缩"用户可取消"的体感上限同档。超时按
/// 失败处理——锚点不落盘，历史原样保留，比写半个摘要安全。
const COMPACT_TIMEOUT_MS: u64 = 180_000;

/// 防失控的输出上限（与 session_title 的 max_output_tokens 同一角色）。
const MAX_OUTPUT_TOKENS: u64 = 8192;

/// 执行一次手动压缩。成功 = 锚点已落盘；`Ok(None)` = 没有可压的历史（调用方回
/// "No compactable history yet."）。任何 `Err` 都保证档案未被改动（锚点是最后一步，
/// 且写在内存里成功之后）。
pub async fn compact_now(
    provider: &ProviderConfig,
    api_key: &str,
    resumed: &Resumed,
    budget: usize,
) -> Result<Option<CompactOutcome>, String> {
    let Some(target) = compaction::plan_manual(&resumed.history, budget) else {
        return Ok(None);
    };
    let summary = summarize(provider, api_key, &resumed.history, target).await?;
    let outcome = CompactOutcome {
        summary: summary.clone(),
        shadowed_items: target.range_end - target.range_start,
        shadowed_tokens: compaction::history_tokens(
            &resumed.history[target.range_start..target.range_end],
        ),
    };
    write_anchor(resumed, target, summary)?;
    Ok(Some(outcome))
}

/// 发总结请求并取回摘要。不带工具声明（与回合内同理由：端点多算声明开销，且给模型
/// "还能继续查"的错误暗示）；流式消费到收口，超时掐断，迟到的成功不采纳。
async fn summarize(
    provider: &ProviderConfig,
    api_key: &str,
    history: &[HistoryItem],
    target: Target,
) -> Result<String, String> {
    let request = compaction::build_request(history, &target);
    let mut body = if provider.is_responses() {
        model_client::build_responses_request(provider, &request, &[], COMPACT_SYSTEM)
    } else {
        model_client::build_chat_request(provider, &request, &[], COMPACT_SYSTEM)
    };
    // 两个协议族的输出上限字段名不同，注入进各自的请求体。
    if provider.is_responses() {
        body["max_output_tokens"] = serde_json::json!(MAX_OUTPUT_TOKENS);
    } else {
        body["max_tokens"] = serde_json::json!(MAX_OUTPUT_TOKENS);
    }
    let mut stream = model_client::stream_body(provider, api_key, body).await?;
    let mut text = String::new();
    let consumed = tokio::time::timeout(
        std::time::Duration::from_millis(COMPACT_TIMEOUT_MS),
        async {
            while let Some(event) = stream.next().await {
                match event {
                    ModelStreamEvent::OutputTextDelta { delta } => text.push_str(&delta),
                    ModelStreamEvent::Failed { message } => {
                        return Err(format!("压缩总结流失败: {message}"))
                    }
                    ModelStreamEvent::Completed { .. } => break,
                    _ => {} // 思考增量与结构化项不进摘要
                }
            }
            Ok::<(), String>(())
        },
    )
    .await;
    match consumed {
        Err(_) => Err(format!("压缩总结超过 {}ms 超时（档案未改动）", COMPACT_TIMEOUT_MS)),
        Ok(Err(e)) => Err(e),
        Ok(Ok(())) => compaction::extract_summary(&text),
    }
}

/// 把锚点追加进档案。独立打开 writer 而不是复用回合内的句柄：本命令只在会话**没有**
/// 活动任务时可达（lib.rs 的命令入口有守卫），此时不存在并发写者；O_APPEND 单行
/// 写入与 `read()` 的半截写判定兼容。写失败原样上报——调用方据此把失败回给用户，
/// 不能静默：锚点丢了用户却看到"压缩成功"，重放时历史会多出一段已被摘要取代的原文。
fn write_anchor(resumed: &Resumed, target: Target, summary: String) -> Result<(), String> {
    let writer = crate::rollout::RolloutWriter::open_existing(&resumed.path)
        .map_err(|e| format!("档案续写打不开: {e}"))?;
    writer
        .append(&RolloutItem::Compacted {
            range_start: target.range_start,
            range_end: target.range_end,
            summary,
        })
        .map_err(|e| format!("压缩锚点写入失败: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_client::Role;
    use crate::rollout::resume::resume;
    use crate::rollout::{ConfigSnapshot, RolloutItem, RolloutWriter};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir() -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("shaniu-scompact-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn meta(agent: &str) -> RolloutItem {
        RolloutItem::SessionMeta {
            session_id: format!("sess-{agent}"),
            agent_id: agent.into(),
            parent_id: None,
            created_at: 1_700_000_000_000,
            config_snapshot: ConfigSnapshot {
                model: "m".into(),
                base_url: "https://x/v1".into(),
                ..Default::default()
            },
        }
    }

    fn write_session(dir: &std::path::Path, name: &str, turns: usize) -> PathBuf {
        let writer = RolloutWriter::create(dir, &meta(name)).unwrap();
        for i in 0..turns {
            writer
                .append(&RolloutItem::ResponseItem {
                    item: HistoryItem::Message {
                        role: Role::User,
                        text: format!("用户消息 {i}"),
                        reasoning: None,
                    },
                })
                .unwrap();
            writer
                .append(&RolloutItem::ResponseItem {
                    item: HistoryItem::Message {
                        role: Role::Assistant,
                        text: format!("助手回复 {i}"),
                        reasoning: None,
                    },
                })
                .unwrap();
        }
        writer.path().to_path_buf()
    }

    #[test]
    fn anchor_round_trips_through_resume() {
        // 端到端格式验证（不带模型）：compact_now 的最后一步 write_anchor 写出的
        // 锚点必须能被 resume() 以同一语义应用——这是 /compact 与 SH-032 回放共享
        // 格式的硬承诺。
        let dir = temp_dir();
        let path = write_session(&dir, "compact-anchor", 3);
        let resumed = resume(&path).unwrap();
        assert_eq!(resumed.history.len(), 6);

        write_anchor(
            &resumed,
            Target { range_start: 0, range_end: 4 },
            "交接摘要".to_string(),
        )
        .unwrap();

        let replayed = resume(&path).unwrap();
        // [0,4) 被 1 条摘要取代，剩原区间外的 2 条
        assert_eq!(replayed.history.len(), 3);
        match &replayed.history[0] {
            HistoryItem::Message { role, text, .. } => {
                assert_eq!(*role, Role::User);
                assert_eq!(text, "交接摘要");
            }
            other => panic!("首条应是摘要 user 消息，得到 {other:?}"),
        }
        assert_eq!(replayed.applied_compactions, 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
