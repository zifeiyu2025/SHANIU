//! 模型客户端（docs §8）：OpenAI Responses SSE 流式客户端（防腐层，ADR-005 翻译面）。
//!
//! 职责：把远端 SSE 事件流翻译为 `ModelStreamEvent`；构造协议请求体。
//! 不含鉴权存储（provider.rs / config::SecretStore）、不含会话历史管理（agent_loop）。
//! 取消语义：drop 返回的 Stream 即中断底层连接（agent_loop 的 CancellationToken 据此传播）。

pub mod provider;

use eventsource_stream::{Event, Eventsource};
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};

use provider::ProviderConfig;

// ============================== 协议类型（纯数据，可测试） ==============================

/// 模型输出的协议项（agent_loop 翻译为内部事件/工具调用）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseItem {
    Message { role: String, text: String },
    FunctionCall { name: String, arguments: String, call_id: String },
}

/// Token 用量（agent_loop 映射到 EventMsg::TokenUsage 广播）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub cached_input_tokens: Option<u32>,
    #[serde(default)]
    pub output_tokens: u32,
}

/// 模型流事件：agent_loop 消费的统一形状。
#[derive(Debug, Clone, PartialEq)]
pub enum ModelStreamEvent {
    OutputTextDelta { delta: String },
    OutputItem(ResponseItem),
    Completed { usage: Usage },
    Failed { message: String },
}

/// 会话历史项（agent_loop 持有并回填；只描述模型需要看见的内容）。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HistoryItem {
    Message { role: Role, text: String },
    FunctionCall { name: String, arguments: String, call_id: String },
    FunctionCallOutput { call_id: String, output: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    System,
}

/// 工具描述（M1 增量 3 由 tool_router 生成传入；此处只负责序列化进请求）。
#[derive(Debug, Clone, Serialize)]
pub struct ToolDeclaration {
    pub name: String,
    pub description: String,
    /// JSON Schema 对象。
    pub parameters: serde_json::Value,
}

// ============================== SSE 事件翻译（纯函数） ==============================

/// 翻译单条 SSE 事件；未知事件类型返回 None（前向兼容，允许忽略）。
pub fn translate_sse_event(event: &Event) -> Option<ModelStreamEvent> {
    let data: serde_json::Value = serde_json::from_str(&event.data).ok()?;
    match event.event.as_str() {
        "response.output_text.delta" => Some(ModelStreamEvent::OutputTextDelta {
            delta: data.get("delta")?.as_str()?.to_string(),
        }),
        "response.output_item.done" => {
            let item = data.get("item")?;
            Some(ModelStreamEvent::OutputItem(parse_item(item)?))
        }
        "response.completed" => {
            let usage = data.pointer("/response/usage")?;
            Some(ModelStreamEvent::Completed {
                usage: Usage {
                    input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                    cached_input_tokens: usage
                        .pointer("/input_tokens_details/cached_tokens")
                        .and_then(|v| v.as_u64())
                        .map(|v| v as u32),
                    output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                },
            })
        }
        "response.failed" => Some(ModelStreamEvent::Failed {
            message: data
                .pointer("/response/error/message")
                .and_then(|v| v.as_str())
                .unwrap_or("response.failed")
                .to_string(),
        }),
        "error" => Some(ModelStreamEvent::Failed {
            message: data
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("stream error")
                .to_string(),
        }),
        _ => None,
    }
}

/// 协议 item → ResponseItem；无法识别的 item 类型返回 None（忽略）。
fn parse_item(item: &serde_json::Value) -> Option<ResponseItem> {
    match item.get("type")?.as_str()? {
        "message" => {
            // content: [{type:"output_text", text: "..."}...] → 聚合单文本
            let role = item.get("role")?.as_str()?.to_string();
            let mut text = String::new();
            for part in item.get("content")?.as_array()? {
                if part.get("type")?.as_str()? == "output_text" {
                    text.push_str(part.get("text")?.as_str().unwrap_or_default());
                }
            }
            Some(ResponseItem::Message { role, text })
        }
        "function_call" => Some(ResponseItem::FunctionCall {
            name: item.get("name")?.as_str()?.to_string(),
            arguments: item.get("arguments")?.as_str()?.to_string(),
            call_id: item.get("call_id")?.as_str()?.to_string(),
        }),
        _ => None,
    }
}

// ============================== 请求体构造（纯函数） ==============================

/// 构造 Responses API 请求体（stream:true 固定；工具经 tools 数组下发）。
pub fn build_responses_request(
    provider: &ProviderConfig,
    history: &[HistoryItem],
    tools: &[ToolDeclaration],
) -> serde_json::Value {
    serde_json::json!({
        "model": provider.model,
        "input": history,
        "stream": true,
        "tools": tools.iter().map(|t| serde_json::json!({
            "type": "function",
            "name": t.name,
            "description": t.description,
            "parameters": t.parameters,
        })).collect::<Vec<_>>(),
    })
}

// ============================== 网络流（薄 IO 壳） ==============================

/// 打开模型 SSE 流（Bearer 鉴权；密钥由调用方从 SecretStore 解密传入，本模块不接触存储）。
pub async fn stream_responses(
    provider: &ProviderConfig,
    api_key: &str,
    history: &[HistoryItem],
    tools: &[ToolDeclaration],
) -> Result<impl Stream<Item = ModelStreamEvent>, String> {
    let client = reqwest::Client::new();
    let body = build_responses_request(provider, history, tools);
    let response = client
        .post(provider.responses_url())
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("模型请求失败: {e}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body_text = response.text().await.unwrap_or_default();
        // 错误响应体可能含敏感信息，仅保留状态码与截断正文
        let snippet: String = body_text.chars().take(200).collect();
        return Err(format!("模型端点返回 {status}: {snippet}"));
    }
    Ok(response
        .bytes_stream()
        .eventsource()
        .map(|res| match res {
            Ok(event) => translate_sse_event(&event),
            Err(e) => Some(ModelStreamEvent::Failed { message: format!("SSE 流错误: {e}") }),
        })
        .filter_map(|ev| async move { ev })) // None = 无法翻译的事件，忽略
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(name: &str, data: &str) -> Event {
        Event {
            event: name.into(),
            data: data.into(),
            id: String::new(),
            retry: None,
        }
    }

    #[test]
    fn output_text_delta_maps_to_delta() {
        let ev = event("response.output_text.delta", r#"{"delta":"你好"}"#);
        assert_eq!(
            translate_sse_event(&ev),
            Some(ModelStreamEvent::OutputTextDelta { delta: "你好".into() })
        );
    }

    #[test]
    fn function_call_item_maps() {
        let ev = event(
            "response.output_item.done",
            r#"{"item":{"type":"function_call","name":"apply_patch","arguments":"{\"patch\":\"x\"}","call_id":"c1"}}"#,
        );
        assert_eq!(
            translate_sse_event(&ev),
            Some(ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                name: "apply_patch".into(),
                arguments: r#"{"patch":"x"}"#.into(),
                call_id: "c1".into(),
            }))
        );
    }

    #[test]
    fn message_item_aggregates_output_text_parts() {
        let ev = event(
            "response.output_item.done",
            r#"{"item":{"type":"message","role":"assistant","content":[
                {"type":"output_text","text":"第一段"},
                {"type":"output_text","text":"第二段"}]}}"#,
        );
        assert_eq!(
            translate_sse_event(&ev),
            Some(ModelStreamEvent::OutputItem(ResponseItem::Message {
                role: "assistant".into(),
                text: "第一段第二段".into(),
            }))
        );
    }

    #[test]
    fn completed_maps_usage() {
        let ev = event(
            "response.completed",
            r#"{"response":{"usage":{"input_tokens":100,"output_tokens":20,
                "input_tokens_details":{"cached_tokens":40}}}}"#,
        );
        assert_eq!(
            translate_sse_event(&ev),
            Some(ModelStreamEvent::Completed {
                usage: Usage { input_tokens: 100, cached_input_tokens: Some(40), output_tokens: 20 }
            })
        );
    }

    #[test]
    fn unknown_event_is_ignored_and_failed_maps() {
        assert_eq!(translate_sse_event(&event("response.created", "{}")), None);
        assert!(matches!(
            translate_sse_event(&event("error", r#"{"message":"boom"}"#)),
            Some(ModelStreamEvent::Failed { .. })
        ));
    }

    #[test]
    fn request_body_carries_model_stream_history_tools() {
        let p = ProviderConfig {
            provider: "openai-responses".into(),
            base_url: "https://x/v1".into(),
            model: "m1".into(),
        };
        let history = vec![
            HistoryItem::Message { role: Role::User, text: "任务".into() },
            HistoryItem::FunctionCall { name: "t".into(), arguments: "{}".into(), call_id: "c".into() },
            HistoryItem::FunctionCallOutput { call_id: "c".into(), output: "ok".into() },
        ];
        let tools = vec![ToolDeclaration {
            name: "unified_exec".into(),
            description: "执行命令".into(),
            parameters: serde_json::json!({"type":"object"}),
        }];
        let body = build_responses_request(&p, &history, &tools);
        assert_eq!(body["model"], "m1");
        assert_eq!(body["stream"], true);
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][1]["type"], "function_call");
        assert_eq!(body["input"][2]["type"], "function_call_output");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "unified_exec");
    }
}
