//! 模型客户端（docs §8）：OpenAI Responses SSE 流式客户端（防腐层，ADR-005 翻译面）。
//!
//! 职责：把远端 SSE 事件流翻译为 `ModelStreamEvent`；构造协议请求体。
//! 不含鉴权存储（provider.rs / config::SecretStore）、不含会话历史管理（agent_loop）。
//! 取消语义：drop 返回的 Stream 即中断底层连接（agent_loop 的 CancellationToken 据此传播）。

pub mod provider;

use eventsource_stream::{Event, Eventsource};
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::images::{image_store, ImageRef};

pub use provider::ProviderConfig;

// ============================== 传输时限（审计 H1） ==============================

/// 建连超时：TCP/TLS 握手必须在此时限内完成。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 首包超时：从发出请求到收到响应头（TTFB）的上限。不设的话端点挂起会让任务永久卡住。
const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(120);
/// 流空闲超时：两个 SSE 分片之间的最长间隔。reqwest 的 `read_timeout` 按"每次读取"
/// 计时，服务端持续滴答但每条都很慢时靠它兜底；也能在思考阶段长时间无输出时中断。
pub const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// 整请求硬上限：防止服务端持续滴答却永不结束（`read_timeout` 每片都重置，管不住）。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(900);

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
    /// 思考模型的推理增量（ChatCompletions 的 `delta.reasoning_content`，
    /// 部分实现用 `delta.reasoning`）。与正文分开，绝不混入 `OutputTextDelta`。
    ReasoningDelta { delta: String },
    OutputItem(ResponseItem),
    Completed { usage: Usage },
    Failed { message: String },
}

/// 会话历史项（agent_loop 持有并回填；只描述模型需要看见的内容）。
/// Deserialize 是 rollout 持久化读回所需（SH-030）：档案必须能还原成同一份历史。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HistoryItem {
    Message {
        role: Role,
        text: String,
        /// 思考模型这一轮产出的 `reasoning_content`（SH-083）。
        ///
        /// 为什么必须存下来并回传：thinking 档端点（DeepSeek/LongCat）要求重放的
        /// assistant 消息带回该字段，缺了直接 400 `The reasoning_content in the thinking
        /// mode must be passed back to the API`。同时它**不能**污染 Responses 路径的
        /// `input` 序列化（那边没有这个字段，多余键会被拒），故 `skip_serializing_if`。
        /// 业界同一做法：codex 把 reasoning 作为独立 `ResponseItem::Reasoning` 重放但
        /// 排除明文 content；ZCode 把它挂在 assistant content block 上并按 provider
        /// 兼容性决定留不留。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
    },
    FunctionCall { name: String, arguments: String, call_id: String },
    FunctionCallOutput { call_id: String, output: String },
    /// 带图片的用户消息（`view_image` 回填专用）。
    ///
    /// 图片以 [`ImageRef`](crate::images::ImageRef) 引用形式**持久化**，真实字节只活在
    /// [`image_store`](crate::images::image_store) 的内存缓存里——这是「base64 不进 rollout」的落点。
    ImageMessage {
        role: Role,
        text: String,
        images: Vec<ImageRef>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
/// `instructions` 为系统提示词（Responses 协议的顶层 instructions 字段）。
pub fn build_responses_request(
    provider: &ProviderConfig,
    history: &[HistoryItem],
    tools: &[ToolDeclaration],
    instructions: &str,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": provider.model,
        "instructions": instructions,
        "input": responses_input(history),
        "stream": true,
        "tools": tools.iter().map(|t| serde_json::json!({
            "type": "function",
            "name": t.name,
            "description": t.description,
            "parameters": t.parameters,
        })).collect::<Vec<_>>(),
    });
    // 推理档（SH-096）：Responses 协议的推理参数是结构化的 `reasoning.effort`。
    // 缺省不产生该键——给不认它的端点塞陌生字段等于赌端点的宽容度。
    if let Some(effort) = provider.reasoning_effort.as_deref().filter(|e| !e.is_empty()) {
        body["reasoning"] = serde_json::json!({ "effort": effort });
    }
    body
}

/// history → Responses `input` 数组。
///
/// 除图片消息外一律沿用 serde 的既有形状（`message` / `function_call` /
/// `function_call_output`），不顺手改协议。图片消息必须发成
/// `{"role","content":[{"type":"input_text"},{"type":"input_image","image_url":"data:…"}]}`：
/// 真机探针验证过，`image_url` 写成对象会 400（`image_url is required`）。
fn responses_input(history: &[HistoryItem]) -> Vec<serde_json::Value> {
    history
        .iter()
        .map(|item| match item {
            HistoryItem::ImageMessage { role, text, images } => {
                let mut content = Vec::new();
                if !text.trim().is_empty() {
                    content.push(serde_json::json!({ "type": "input_text", "text": text }));
                }
                for image in images {
                    match image_store::data_url(&image.sha256) {
                        Some(url) => content
                            .push(serde_json::json!({ "type": "input_image", "image_url": url })),
                        None => content.push(serde_json::json!({
                            "type": "input_text",
                            "text": missing_image_note(image),
                        })),
                    }
                }
                serde_json::json!({ "role": role, "content": content })
            }
            other => serde_json::to_value(other).expect("HistoryItem 序列化不应失败"),
        })
        .collect()
}

/// 缓存里没有图片字节时的替代文本（进程重启或缓存淘汰）。
fn missing_image_note(image: &ImageRef) -> String {
    format!(
        "[图片不可用：{}（{}×{}）。会话重启或缓存淘汰后不再保留字节，需要时请重新调用 view_image]",
        image.path, image.width, image.height
    )
}

/// 图片消息 → ChatCompletions `content` 数组（文本在前，图片在后）。
fn chat_image_content(text: &str, images: &[ImageRef]) -> serde_json::Value {
    let mut parts = Vec::new();
    if !text.trim().is_empty() {
        parts.push(serde_json::json!({ "type": "text", "text": text }));
    }
    for image in images {
        match image_store::data_url(&image.sha256) {
            Some(url) => parts.push(serde_json::json!({
                "type": "image_url",
                "image_url": { "url": url },
            })),
            None => parts.push(serde_json::json!({
                "type": "text",
                "text": missing_image_note(image),
            })),
        }
    }
    serde_json::Value::Array(parts)
}

/// 构造 ChatCompletions API 请求体（stream:true；工具为 function 包装）。
/// `instructions` 以首条 system 消息下发（不写入 history，避免每轮重复累积）。
pub fn build_chat_request(
    provider: &ProviderConfig,
    history: &[HistoryItem],
    tools: &[ToolDeclaration],
    instructions: &str,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": provider.model,
        "messages": chat_messages(instructions, history),
        "stream": true,
        // 流式默认不返回 usage（OpenAI 兼容端点一致），不加这一项 token 统计直接归零——
        // 缓存命中率就无从观察。端点会在最后补一个 choices 为空、只带 usage 的分片。
        "stream_options": { "include_usage": true },
        "tools": tools.iter().map(|t| serde_json::json!({
            "type": "function",
            "function": {
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters,
            },
        })).collect::<Vec<_>>(),
    });
    // 推理档（SH-096）：ChatCompletions 的顶层 `reasoning_effort`，wire 值来自
    // 模型行的档位声明表（off/未声明在配置层已经解析成 None，不会到这里）。
    if let Some(effort) = provider.reasoning_effort.as_deref().filter(|e| !e.is_empty()) {
        body["reasoning_effort"] = serde_json::Value::String(effort.into());
    }
    body
}

/// 扁平 history → ChatCompletions messages（协议硬约束的唯一落点）。
///
/// 三条不可协商的规则，都是真机 400 打出来的（docs/05 SH-082）：
/// 1. 同轮的多个工具调用属于**同一条** assistant 消息的 `tool_calls` 数组；
///    逐个拆成多条 assistant，严格端点直接报
///    `An assistant message with 'tool_calls' must be followed by tool messages
///     responding to each 'tool_call_id'`；
/// 2. 该消息之后必须**紧跟**每个 `call_id` 对应的 `role=tool` 消息，顺序一致；
/// 3. 本轮的助手文本若与工具调用同时存在，也必须并进同一条 assistant 消息
///    （`content` + `tool_calls`），不能先出一条纯文本 assistant。
///
/// 取消会打断工具执行循环（agent_loop 在 `Err(Cancelled)` 分支 `break 'outer`），
/// 于是"已声明但未执行"的调用没有输出项。这里必须给它补一条 tool 消息，否则该会话
/// 的后续每一轮请求都会 400——历史已经不可修复，只能就地补齐。
pub fn chat_messages(instructions: &str, history: &[HistoryItem]) -> Vec<serde_json::Value> {
    let mut messages = vec![serde_json::json!({ "role": "system", "content": instructions })];
    let mut i = 0;
    while i < history.len() {
        match &history[i] {
            // 助手文本 + 紧随其后的同轮工具调用 → 合并成一条 assistant 消息
            HistoryItem::Message { role, text, reasoning }
                if *role == Role::Assistant && matches!(history.get(i + 1), Some(HistoryItem::FunctionCall { .. })) =>
            {
                let (calls, outputs, next) = collect_call_group(history, i + 1);
                messages.extend(assistant_group_messages(
                    Some(text.as_str()),
                    reasoning.as_deref(),
                    &calls,
                    &outputs,
                ));
                i = next;
            }
            HistoryItem::FunctionCall { .. } => {
                let (calls, outputs, next) = collect_call_group(history, i);
                messages.extend(assistant_group_messages(None, None, &calls, &outputs));
                i = next;
            }
            HistoryItem::Message { role, text, reasoning } => {
                let content = if text.trim().is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::Value::String(text.clone())
                };
                let mut m = serde_json::json!({ "role": role, "content": content });
                // thinking 档要求重放的 assistant 消息带回 reasoning_content（SH-083）；
                // 没有就不产生该键，避免给不认它的端点塞陌生字段
                if let Some(r) = reasoning.as_deref().filter(|r| !r.is_empty()) {
                    m["reasoning_content"] = serde_json::Value::String(r.to_string());
                }
                messages.push(m);
                i += 1;
            }
            HistoryItem::ImageMessage { role, text, images } => {
                messages.push(serde_json::json!({
                    "role": role,
                    "content": chat_image_content(text, images),
                }));
                i += 1;
            }
            // 落单的 tool 输出：只有外部裁剪/重排历史才会出现，按 call_id 原样发出
            HistoryItem::FunctionCallOutput { call_id, output } => {
                messages.push(tool_message(call_id, output));
                i += 1;
            }
        }
    }
    messages
}

/// 一次工具调用：(工具名, 参数 JSON 文本, call_id)。
type ChatCall = (String, String, String);
/// call_id → 该调用的输出文本。
type ChatOutputs = std::collections::HashMap<String, String>;

/// 从 `start` 起收集连续的 FunctionCall，以及紧随其后的连续 FunctionCallOutput。
/// 返回 (调用清单, call_id → 输出, 下一段起点)。
fn collect_call_group(history: &[HistoryItem], start: usize) -> (Vec<ChatCall>, ChatOutputs, usize) {
    let mut calls = Vec::new();
    let mut idx = start;
    while let Some(HistoryItem::FunctionCall { name, arguments, call_id }) = history.get(idx) {
        calls.push((name.clone(), arguments.clone(), call_id.clone()));
        idx += 1;
    }
    let mut outputs = std::collections::HashMap::new();
    while let Some(HistoryItem::FunctionCallOutput { call_id, output }) = history.get(idx) {
        outputs.insert(call_id.clone(), output.clone());
        idx += 1;
    }
    (calls, outputs, idx)
}

/// 取消打断工具执行循环时，"已声明未执行"的调用补位用的输出文本。
/// 不给补位 = 该会话后续每一轮都 400，历史就此作废，比文案难看严重得多。
const CANCELLED_CALL_NOTE: &str = "（本轮在该工具调用前被取消，未执行）";

fn tool_message(call_id: &str, output: &str) -> serde_json::Value {
    serde_json::json!({ "role": "tool", "tool_call_id": call_id, "content": output })
}

/// 一条 assistant 消息 + 其后每个 call_id 一条 tool 消息（缺失输出就地补位）。
fn assistant_group_messages(
    text: Option<&str>,
    reasoning: Option<&str>,
    calls: &[ChatCall],
    outputs: &ChatOutputs,
) -> Vec<serde_json::Value> {
    // 调用方保证 calls 非空；空清单在这里会产出非法请求，故用 debug_assert 卡住开发期误用
    debug_assert!(!calls.is_empty());
    let content = match text.map(str::trim) {
        Some("") | None => serde_json::Value::Null,
        Some(t) => serde_json::Value::String(t.to_string()),
    };
    let mut message = serde_json::json!({
        "role": "assistant",
        "content": content,
        "tool_calls": calls.iter().map(|(name, arguments, call_id)| serde_json::json!({
            "id": call_id,
            "type": "function",
            "function": { "name": name, "arguments": arguments },
        })).collect::<Vec<_>>(),
    });
    if let Some(r) = reasoning.filter(|r| !r.is_empty()) {
        message["reasoning_content"] = serde_json::Value::String(r.to_string());
    }
    let mut group = vec![message];
    group.extend(calls.iter().map(|(_, _, call_id)| {
        tool_message(
            call_id,
            outputs.get(call_id).map(String::as_str).unwrap_or(CANCELLED_CALL_NOTE),
        )
    }));
    group
}

// ============================== ChatCompletions 分片累积（纯状态机） ==============================

/// ChatCompletions 流式状态机：增量 chunk → 模型事件。
/// 工具调用按 index 分片到达（name 首片、arguments 逐段），finish_reason=tool_calls
/// 时统一发已装配的 FunctionCall 输出项；文本增量直接透传。
#[derive(Default)]
pub struct ChatStreamAccumulator {
    text: String,
    tool_calls: Vec<(String, String, String)>, // (id, name, arguments)
    finished: bool,
}

impl ChatStreamAccumulator {
    pub fn feed(&mut self, data: &serde_json::Value) -> Vec<ModelStreamEvent> {
        let mut events = Vec::new();
        if let Some(choices) = data.get("choices").and_then(|c| c.as_array()) {
            if let Some(choice) = choices.first() {
                if let Some(delta) = choice.get("delta") {
                    if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                        if !content.is_empty() {
                            self.text.push_str(content);
                            events.push(ModelStreamEvent::OutputTextDelta { delta: content.to_string() });
                        }
                    }
                    // 思考增量单列事件，绝不并入正文：不同实现分别用
                    // delta.reasoning_content（DeepSeek/LongCat）与 delta.reasoning
                    let reasoning = delta
                        .get("reasoning_content")
                        .or_else(|| delta.get("reasoning"))
                        .and_then(|r| r.as_str());
                    if let Some(rc) = reasoning {
                        if !rc.is_empty() {
                            events.push(ModelStreamEvent::ReasoningDelta { delta: rc.to_string() });
                        }
                    }
                    if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                        for tc in tool_calls {
                            let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                            while self.tool_calls.len() <= index {
                                self.tool_calls.push((String::new(), String::new(), String::new()));
                            }
                            if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                                self.tool_calls[index].0 = id.to_string();
                            }
                            if let Some(f) = tc.get("function") {
                                if let Some(name) = f.get("name").and_then(|v| v.as_str()) {
                                    if !name.is_empty() {
                                        self.tool_calls[index].1 = name.to_string();
                                    }
                                }
                                if let Some(a) = f.get("arguments").and_then(|v| v.as_str()) {
                                    self.tool_calls[index].2.push_str(a);
                                }
                            }
                        }
                    }
                }
                let finish = choice.get("finish_reason").and_then(|f| f.as_str());
                if finish == Some("tool_calls") && !self.finished {
                    self.finished = true;
                    for (id, name, arguments) in self.tool_calls.drain(..) {
                        events.push(ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                            name, arguments, call_id: id,
                        }));
                    }
                }
            }
        }
        if let Some(usage) = data.get("usage").filter(|u| u.is_object()) {
            let count = |key: &str| usage.get(key).and_then(|v| v.as_u64()).map(|v| v as u32);
            // 缓存命中要认两套 wire（2026-11-10 命中统计补齐）：
            // - OpenAI 系：`prompt_tokens_details.cached_tokens`（cached 是 prompt 的子集）；
            // - DeepSeek 系：`prompt_cache_hit_tokens` / `prompt_cache_miss_tokens`
            //   （hit + miss = prompt_tokens），且**不返回** prompt_tokens_details。
            // 前者优先，后者兜底；两套都不在时保持 None，不猜 0。
            let hit = usage
                .pointer("/prompt_tokens_details/cached_tokens")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32)
                .or_else(|| count("prompt_cache_hit_tokens"));
            let miss = count("prompt_cache_miss_tokens");
            // prompt_tokens 缺失时用 hit+miss 复原（DeepSeek 两条字段合起来就是输入规模）。
            let input_tokens = match (count("prompt_tokens"), hit.zip(miss)) {
                (Some(input), _) => input,
                (None, Some((h, m))) => h.saturating_add(m),
                (None, None) => 0,
            };
            let usage = Usage {
                input_tokens,
                cached_input_tokens: hit,
                output_tokens: count("completion_tokens").unwrap_or(0),
            };
            events.push(ModelStreamEvent::Completed { usage });
        }
        events
    }
}

/// ChatCompletions SSE 数据翻译（纯函数；[DONE] 返回空，error 追加 Failed）。
pub fn translate_chat_data(data: &str, acc: &mut ChatStreamAccumulator) -> Vec<ModelStreamEvent> {
    let data = data.trim();
    if data == "[DONE]" || data.is_empty() {
        return Vec::new();
    }
    let Ok(json) = serde_json::from_str::<serde_json::Value>(data) else {
        return Vec::new();
    };
    let mut events = acc.feed(&json);
    if let Some(err) = json.get("error") {
        events.push(ModelStreamEvent::Failed {
            message: err
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("chat error")
                .to_string(),
        });
    }
    events
}

// ============================== 网络流（薄 IO 壳） ==============================

/// 模型流别名（agent_loop 消费；Pin<Box> 保证 Unpin 语义简单）。
pub type ModelStream = std::pin::Pin<Box<dyn Stream<Item = ModelStreamEvent> + Send>>;

/// 统一入口：按协议族选择 Responses / ChatCompletions 流。
pub async fn stream_model(
    provider: &ProviderConfig,
    api_key: &str,
    history: &[HistoryItem],
    tools: &[ToolDeclaration],
    instructions: &str,
) -> Result<ModelStream, String> {
    if provider.is_responses() {
        Ok(Box::pin(stream_responses(provider, api_key, history, tools, instructions).await?))
    } else {
        Ok(Box::pin(stream_chat(provider, api_key, history, tools, instructions).await?))
    }
}

/// ChatCompletions 端点：base_url 已含 /chat/completions 则原样，否则拼接。
fn chat_url(provider: &ProviderConfig) -> String {
    if provider.base_url.ends_with("/chat/completions") {
        provider.base_url.clone()
    } else {
        format!("{}/chat/completions", provider.base_url)
    }
}

/// 打开 ChatCompletions SSE 流（累积器跨事件存活，流结束即释放）。
pub async fn stream_chat(
    provider: &ProviderConfig,
    api_key: &str,
    history: &[HistoryItem],
    tools: &[ToolDeclaration],
    instructions: &str,
) -> Result<impl Stream<Item = ModelStreamEvent>, String> {
    stream_body(provider, api_key, build_chat_request(provider, history, tools, instructions)).await
}

/// 打开 Responses API SSE 流。
pub async fn stream_responses(
    provider: &ProviderConfig,
    api_key: &str,
    history: &[HistoryItem],
    tools: &[ToolDeclaration],
    instructions: &str,
) -> Result<impl Stream<Item = ModelStreamEvent>, String> {
    stream_body(provider, api_key, build_responses_request(provider, history, tools, instructions)).await
}

/// 按**已备好的请求体**打开模型 SSE 流；URL 与响应解析按协议族选择。
///
/// 主循环（`stream_chat` / `stream_responses`）与辅助请求（标题生成）共用同一条传输
/// 管线：两者差别只在请求体字段（辅助请求自带 `max_tokens` 这类主循环没有的字段），
/// 不在传输本身——复制管线等于让两处各自腐化。
pub async fn stream_body(
    provider: &ProviderConfig,
    api_key: &str,
    body: serde_json::Value,
) -> Result<ModelStream, String> {
    // 审计 H1：`Client::new()` 没有任何时限——端点不响应时请求会永久挂起，
    // 连取消都进不去。三个时限各管一段：建连、每次读取、整请求。
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(STREAM_IDLE_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| format!("构造模型 HTTP 客户端失败: {e}"))?;
    let url = if provider.is_responses() { provider.responses_url() } else { chat_url(provider) };
    // 自定义请求头（SH-097）先放：bearer_auth 后到，authorization 冲突时认证头获胜
    let mut request = client.post(url);
    for (name, value) in &provider.headers {
        request = request.header(name.as_str(), value.as_str());
    }
    // 首包（响应头）单独设限：`read_timeout` 只作用于响应体，管不到 TTFB。
    let response = tokio::time::timeout(
        FIRST_BYTE_TIMEOUT,
        request.bearer_auth(api_key).json(&body).send(),
    )
    .await
    .map_err(|_| {
        format!("模型请求首包超时（{}s 内未收到响应头）", FIRST_BYTE_TIMEOUT.as_secs())
    })?
    .map_err(|e| format!("模型请求失败: {e}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body_text = response.text().await.unwrap_or_default();
        // 错误响应体可能含敏感信息，仅保留状态码与截断正文
        let snippet: String = body_text.chars().take(200).collect();
        return Err(format!("模型端点返回 {status}: {snippet}"));
    }
    if provider.is_responses() {
        Ok(Box::pin(response
            .bytes_stream()
            .eventsource()
            .map(|res| match res {
                Ok(event) => translate_sse_event(&event),
                Err(e) => Some(ModelStreamEvent::Failed { message: format!("SSE 流错误: {e}") }),
            })
            .filter_map(|ev| async move { ev }))) // None = 无法翻译的事件，忽略
    } else {
        let acc = std::cell::RefCell::new(ChatStreamAccumulator::default());
        Ok(Box::pin(response
            .bytes_stream()
            .eventsource()
            .map(move |res| match res {
                Ok(event) => translate_chat_data(&event.data, &mut acc.borrow_mut()),
                Err(e) => vec![ModelStreamEvent::Failed { message: format!("SSE 流错误: {e}") }],
            })
            .flat_map(futures::stream::iter)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(protocol: &str) -> ProviderConfig {
        ProviderConfig {
            provider: protocol.into(),
            base_url: "https://x/v1".into(),
            model: "m1".into(),
            reasoning_effort: None,
            headers: Default::default(),
        }
    }

    fn image_ref(hash: &str) -> ImageRef {
        ImageRef {
            path: "shots/screen.png".into(),
            sha256: hash.into(),
            width: 128,
            height: 128,
            tokens: 25,
            mime: "image/png".into(),
        }
    }

    #[test]
    fn responses_request_sends_image_as_input_image_string() {
        image_store::put("resp-hash", "image/png", b"\x89PNG");
        let history = vec![HistoryItem::ImageMessage {
            role: Role::User,
            text: "图里是什么".into(),
            images: vec![image_ref("resp-hash")],
        }];
        let body = build_responses_request(&provider("openai-responses"), &history, &[], "SYS");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][0]["content"][1]["type"], "input_image");
        // 真机探针：`image_url` 必须是字符串，写成对象端点报 400。
        let url = body["input"][0]["content"][1]["image_url"]
            .as_str()
            .expect("image_url 必须是字符串");
        assert!(url.starts_with("data:image/png;base64,"), "{url}");
        // 带图消息不能带 `type: message`，那是纯文本消息的形状。
        assert!(body["input"][0].get("type").is_none());
    }

    #[test]
    fn chat_request_sends_image_as_image_url_object() {
        image_store::put("chat-hash", "image/jpeg", b"\xff\xd8");
        let history = vec![HistoryItem::ImageMessage {
            role: Role::User,
            text: "图里是什么".into(),
            images: vec![ImageRef {
                mime: "image/jpeg".into(),
                ..image_ref("chat-hash")
            }],
        }];
        let body = build_chat_request(&provider("openai-chat"), &history, &[], "SYS");
        let content = &body["messages"][1]["content"];
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image_url");
        let url = content[1]["image_url"]["url"].as_str().expect("url 字段");
        assert!(url.starts_with("data:image/jpeg;base64,"), "{url}");
    }

    #[test]
    fn missing_image_bytes_degrade_to_text_note() {
        let history = vec![HistoryItem::ImageMessage {
            role: Role::User,
            text: String::new(),
            images: vec![image_ref("hash-never-stored")],
        }];
        let chat = build_chat_request(&provider("openai-chat"), &history, &[], "SYS");
        let text = chat["messages"][1]["content"][0]["text"]
            .as_str()
            .expect("应退化为文本说明");
        assert!(text.contains("图片不可用"), "{text}");
        let responses = build_responses_request(&provider("openai-responses"), &history, &[], "SYS");
        let text = responses["input"][0]["content"][0]["text"]
            .as_str()
            .expect("应退化为文本说明");
        assert!(text.contains("图片不可用"), "{text}");
    }

    #[test]
    fn image_message_serializes_without_bytes() {
        image_store::put("serialize-hash", "image/png", b"\x89PNG-body");
        let item = HistoryItem::ImageMessage {
            role: Role::User,
            text: "看图".into(),
            images: vec![image_ref("serialize-hash")],
        };
        let json = serde_json::to_string(&item).expect("序列化");
        // rollout 只能落引用：出现 base64 / data: 就说明图片字节漏进了档案。
        assert!(!json.contains("base64"), "{json}");
        assert!(!json.contains("data:image"), "{json}");
        assert!(json.contains("serialize-hash"), "{json}");
        let back: HistoryItem = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back, item);
    }

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
            reasoning_effort: None,
            headers: Default::default(),
        };
        let history = vec![
            HistoryItem::Message { role: Role::User, text: "任务".into(), reasoning: None },
            HistoryItem::FunctionCall { name: "t".into(), arguments: "{}".into(), call_id: "c".into() },
            HistoryItem::FunctionCallOutput { call_id: "c".into(), output: "ok".into() },
        ];
        let tools = vec![ToolDeclaration {
            name: "unified_exec".into(),
            description: "执行命令".into(),
            parameters: serde_json::json!({"type":"object"}),
        }];
        let body = build_responses_request(&p, &history, &tools, "SYS");
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

#[cfg(test)]
mod chat_tests {
    use super::*;

    #[test]
    fn chat_request_carries_reasoning_effort_only_when_set() {
        let base = ProviderConfig {
            provider: "openai-chat".into(),
            base_url: "https://x/v1".into(),
            model: "m1".into(),
            reasoning_effort: None,
            headers: Default::default(),
        };
        let body = build_chat_request(&base, &[], &[], "SYS");
        assert!(body.get("reasoning_effort").is_none(), "未配置档位不得产生该键：陌生字段等于赌端点宽容");

        let mut p = base.clone();
        p.reasoning_effort = Some("high".into());
        let body = build_chat_request(&p, &[], &[], "SYS");
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn responses_request_carries_structured_reasoning_effort() {
        let mut p = ProviderConfig {
            provider: "openai-responses".into(),
            base_url: "https://x/v1".into(),
            model: "m1".into(),
            reasoning_effort: None,
            headers: Default::default(),
        };
        assert!(build_responses_request(&p, &[], &[], "SYS").get("reasoning").is_none());
        p.reasoning_effort = Some("low".into());
        let body = build_responses_request(&p, &[], &[], "SYS");
        assert_eq!(body["reasoning"]["effort"], "low");
    }

    #[test]
    fn chat_request_maps_history_and_function_tools() {
        let p = ProviderConfig {
            provider: "openai-chat".into(),
            base_url: "https://api.longcat.chat/openai/v1".into(),
            model: "LongCat-2.5".into(),
            reasoning_effort: None,
            headers: Default::default(),
        };
        let history = vec![
            HistoryItem::Message { role: Role::User, text: "任务".into(), reasoning: None },
            HistoryItem::FunctionCall { name: "t".into(), arguments: "{}".into(), call_id: "c1".into() },
            HistoryItem::FunctionCallOutput { call_id: "c1".into(), output: "ok".into() },
        ];
        let tools = vec![ToolDeclaration {
            name: "unified_exec".into(),
            description: "d".into(),
            parameters: serde_json::json!({"type":"object"}),
        }];
        let body = build_chat_request(&p, &history, &tools, "SYS");
        assert_eq!(body["model"], "LongCat-2.5");
        assert_eq!(body["stream"], true);
        // 系统提示固定为第 0 条（docs/04 §6.1），history 顺移一位
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "SYS");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][2]["tool_calls"][0]["id"], "c1");
        assert_eq!(body["messages"][3]["role"], "tool");
        // 系统提示只出现一次，不随历史累积
        let system_count = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "system")
            .count();
        assert_eq!(system_count, 1);
        assert_eq!(body["tools"][0]["function"]["name"], "unified_exec");
        assert!(!p.is_responses());
    }

    /// 协议不变量（SH-082 的真机 400 对应物）：每条带 tool_calls 的 assistant 消息，
    /// 其后必须**紧跟**等量的 role=tool 消息，且 call_id 集合一致。
    /// 断言结构而非字面量——字面量快照挡不住"拆成两条 assistant"这种形态。
    fn assert_tool_calls_are_answered(messages: &[serde_json::Value]) {
        let mut i = 0;
        while i < messages.len() {
            let m = &messages[i];
            let calls = m["tool_calls"].as_array();
            let Some(calls) = calls else { i += 1; continue };
            assert_eq!(m["role"], "assistant", "tool_calls 只能挂在 assistant 上");
            let ids: Vec<&str> = calls.iter().map(|c| c["id"].as_str().unwrap()).collect();
            for (n, id) in ids.iter().enumerate() {
                let answer = messages
                    .get(i + 1 + n)
                    .unwrap_or_else(|| panic!("tool_call {id} 后面没有对应的 tool 消息"));
                assert_eq!(answer["role"], "tool", "tool_call {id} 的应答位必须是 role=tool");
                assert_eq!(answer["tool_call_id"], *id, "应答顺序必须与 tool_calls 一一对应");
            }
            i += 1 + ids.len();
        }
    }

    #[test]
    fn chat_history_merges_same_turn_multiple_calls() {
        let p = ProviderConfig {
            provider: "openai-chat".into(),
            base_url: "https://x/v1".into(),
            model: "m".into(),
            reasoning_effort: None,
            headers: Default::default(),
        };
        // agent_loop 的真实产出形态：一轮里先连续 push 所有 FunctionCall，再连续 push 输出
        let history = vec![
            HistoryItem::Message { role: Role::User, text: "任务".into(), reasoning: None },
            HistoryItem::FunctionCall { name: "read_file".into(), arguments: "{}".into(), call_id: "c1".into() },
            HistoryItem::FunctionCall { name: "list_dir".into(), arguments: "{}".into(), call_id: "c2".into() },
            HistoryItem::FunctionCallOutput { call_id: "c1".into(), output: "o1".into() },
            HistoryItem::FunctionCallOutput { call_id: "c2".into(), output: "o2".into() },
            HistoryItem::Message { role: Role::Assistant, text: "完成".into(), reasoning: None },
        ];
        let body = build_chat_request(&p, &history, &[], "SYS");
        let msgs = body["messages"].as_array().unwrap();
        // system + user + 1 条 assistant(2 calls) + 2 条 tool + 1 条 assistant = 6
        // 旧实现会产出 8 条（两个 assistant 各带一个 call）→ 严格端点 400
        assert_eq!(msgs.len(), 6);
        assert_eq!(msgs[2]["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(msgs[3]["tool_call_id"], "c1");
        assert_eq!(msgs[4]["tool_call_id"], "c2");
        assert_eq!(msgs[5]["role"], "assistant");
        assert_eq!(msgs[5]["content"], "完成");
        assert_tool_calls_are_answered(msgs);
    }

    #[test]
    fn chat_history_merges_assistant_text_with_same_turn_calls() {
        let p = ProviderConfig {
            provider: "openai-chat".into(),
            base_url: "https://x/v1".into(),
            model: "m".into(),
            reasoning_effort: None,
            headers: Default::default(),
        };
        let history = vec![
            HistoryItem::Message { role: Role::User, text: "任务".into(), reasoning: None },
            HistoryItem::Message { role: Role::Assistant, text: "我先看一眼".into(), reasoning: None },
            HistoryItem::FunctionCall { name: "read_file".into(), arguments: "{}".into(), call_id: "c1".into() },
            HistoryItem::FunctionCallOutput { call_id: "c1".into(), output: "o1".into() },
        ];
        let body = build_chat_request(&p, &history, &[], "SYS");
        let msgs = body["messages"].as_array().unwrap();
        // 文本与调用合并在同一条 assistant，不再多出一条纯文本 assistant
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[2]["content"], "我先看一眼");
        assert_eq!(msgs[2]["tool_calls"].as_array().unwrap().len(), 1);
        assert_tool_calls_are_answered(msgs);
    }

    #[test]
    fn chat_history_backfills_missing_output_after_cancel() {
        let p = ProviderConfig {
            provider: "openai-chat".into(),
            base_url: "https://x/v1".into(),
            model: "m".into(),
            reasoning_effort: None,
            headers: Default::default(),
        };
        // 取消打断工具循环：c2 已声明但从未执行，历史里没有它的输出项
        let history = vec![
            HistoryItem::Message { role: Role::User, text: "任务".into(), reasoning: None },
            HistoryItem::FunctionCall { name: "read_file".into(), arguments: "{}".into(), call_id: "c1".into() },
            HistoryItem::FunctionCall { name: "apply_patch".into(), arguments: "{}".into(), call_id: "c2".into() },
            HistoryItem::FunctionCallOutput { call_id: "c1".into(), output: "o1".into() },
        ];
        let body = build_chat_request(&p, &history, &[], "SYS");
        let msgs = body["messages"].as_array().unwrap();
        assert_tool_calls_are_answered(msgs);
        assert_eq!(msgs[2]["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(msgs[4]["tool_call_id"], "c2");
        assert_eq!(msgs[4]["content"], CANCELLED_CALL_NOTE);
    }

    #[test]
    fn chat_url_appends_or_keeps_endpoint() {
        let p1 = ProviderConfig {
            provider: "openai-chat".into(),
            base_url: "https://api.longcat.chat/openai/v1".into(),
            model: "m".into(),
            reasoning_effort: None,
            headers: Default::default(),
        };
        assert_eq!(chat_url(&p1), "https://api.longcat.chat/openai/v1/chat/completions");
        let p2 = ProviderConfig {
            provider: "openai-chat".into(),
            base_url: "https://api.longcat.chat/openai/v1/chat/completions".into(),
            model: "m".into(),
            reasoning_effort: None,
            headers: Default::default(),
        };
        assert_eq!(chat_url(&p2), "https://api.longcat.chat/openai/v1/chat/completions");
    }

    #[test]
    fn chat_accumulator_assembles_tool_call_fragments() {
        let mut acc = ChatStreamAccumulator::default();
        // 文本增量
        let evs = translate_chat_data(
            r#"{"choices":[{"delta":{"content":"你"},"finish_reason":null}]}"#,
            &mut acc,
        );
        assert_eq!(evs, vec![ModelStreamEvent::OutputTextDelta { delta: "你".into() }]);
        // 工具调用分片：首片 id+name，后续 arguments 追加
        let _ = translate_chat_data(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c9","function":{"name":"apply_patch","arguments":""}}]}}]}"#,
            &mut acc,
        );
        let _ = translate_chat_data(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"p\":"}}]}}]}"#,
            &mut acc,
        );
        let _ = translate_chat_data(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]}}]}"#,
            &mut acc,
        );
        // finish_reason=tool_calls → 装配输出
        let evs = translate_chat_data(
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            &mut acc,
        );
        assert_eq!(
            evs,
            vec![ModelStreamEvent::OutputItem(ResponseItem::FunctionCall {
                name: "apply_patch".into(),
                arguments: r#"{"p":1}"#.into(),
                call_id: "c9".into(),
            })]
        );
        // usage（完成块可同带 usage）
        let evs = translate_chat_data(
            r#"{"choices":[],"usage":{"prompt_tokens":9,"completion_tokens":3}}"#,
            &mut acc,
        );
        assert!(matches!(evs.last(), Some(ModelStreamEvent::Completed { .. })));
    }

    #[test]
    fn chat_request_asks_endpoint_to_include_usage_in_stream() {
        let p = ProviderConfig {
            provider: "openai-chat".into(),
            base_url: "https://x/v1".into(),
            model: "m1".into(),
            reasoning_effort: None,
            headers: Default::default(),
        };
        let body = build_chat_request(&p, &[], &[], "SYS");
        assert_eq!(
            body["stream_options"]["include_usage"], true,
            "流式不请求 usage 时 token 统计与缓存命中率全部归零"
        );
    }

    /// DeepSeek 的 usage 只有 `prompt_cache_hit/miss_tokens`，没有
    /// `prompt_tokens_details.cached_tokens`——不认这套 wire 命中率永远是 None。
    #[test]
    fn chat_usage_reads_deepseek_cache_fields() {
        let mut acc = ChatStreamAccumulator::default();
        let evs = translate_chat_data(
            r#"{"choices":[],"usage":{"prompt_tokens":1000,"completion_tokens":20,
                "prompt_cache_hit_tokens":900,"prompt_cache_miss_tokens":100}}"#,
            &mut acc,
        );
        assert_eq!(
            evs,
            vec![ModelStreamEvent::Completed {
                usage: Usage { input_tokens: 1000, cached_input_tokens: Some(900), output_tokens: 20 }
            }]
        );
    }

    #[test]
    fn chat_usage_reads_openai_cache_details_first() {
        let mut acc = ChatStreamAccumulator::default();
        let evs = translate_chat_data(
            r#"{"choices":[],"usage":{"prompt_tokens":1000,"completion_tokens":20,
                "prompt_tokens_details":{"cached_tokens":800},"prompt_cache_hit_tokens":900}}"#,
            &mut acc,
        );
        assert_eq!(
            evs,
            vec![ModelStreamEvent::Completed {
                usage: Usage { input_tokens: 1000, cached_input_tokens: Some(800), output_tokens: 20 }
            }],
            "两套字段同时出现时以 OpenAI 口径为准"
        );
    }

    #[test]
    fn chat_usage_restores_input_from_hit_plus_miss() {
        let mut acc = ChatStreamAccumulator::default();
        let evs = translate_chat_data(
            r#"{"choices":[],"usage":{"completion_tokens":5,"prompt_cache_hit_tokens":30,
                "prompt_cache_miss_tokens":70}}"#,
            &mut acc,
        );
        assert_eq!(
            evs,
            vec![ModelStreamEvent::Completed {
                usage: Usage { input_tokens: 100, cached_input_tokens: Some(30), output_tokens: 5 }
            }],
            "缺 prompt_tokens 时用 hit+miss 复原输入规模"
        );
    }

    #[test]
    fn chat_done_and_error_handled() {
        let mut acc = ChatStreamAccumulator::default();
        assert!(translate_chat_data("[DONE]", &mut acc).is_empty());
        let evs = translate_chat_data(r#"{"error":{"message":"quota"}}"#, &mut acc);
        assert!(matches!(evs.last(), Some(ModelStreamEvent::Failed { message }) if message == "quota"));
    }
}
