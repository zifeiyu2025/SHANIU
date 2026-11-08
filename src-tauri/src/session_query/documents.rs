//! rollout 档案 → 可索引语义文档（SH-113）。
//!
//! 口径对齐上游 `session-query/src/extraction.ts` + `documents.ts`，但输入是
//! **SHANIU 的持久化事实源**（`rollout::RolloutItem` / `model_client::HistoryItem`），
//! 不是 DSH journal 事件。推导链与 `lib.rs::project_entries`（`load_session`
//! 回放路径）同源：先建 `call_id → output` 索引，再按原序投影，工具输出并进
//! 自己的调用，压缩锚点作为用户消息文本。因此**同一份档案**在「实时写入后」
//! 与「重启回放后」抽出的文档序列逐项一致（SH-108 的推导范式）。
//!
//! 与上游 host `list.ts` 的一处有意偏差：上游只把 `user/message` / `assistant/message`
//! 的 bestMatch 交给侧栏；SH-113 卡验收明确要求「命中正文与工具输出片段」，
//! 故 `tool/call` 与 `tool/result` 也产文档、也参与排名。

use std::collections::HashMap;

use crate::model_client::{HistoryItem, Role};
use crate::rollout::RolloutItem;
use crate::session_query::query::fts_text;

/// 一条可索引语义文档（对应 FTS5 `persisted_docs` / `temp.live_docs` 的一行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchDoc {
    /// 档案内稳定的文档序号：一个条目最多展开成两条（调用 + 输出），故用 `index*2`。
    pub seq: i64,
    /// 事件语义类型（DSH 词汇表同款拼写，便于与上游对照）。
    pub doc_type: &'static str,
    /// 兜底排序时间：档案 created_at + seq（rollout 没有逐条时间戳，不伪造墙钟）。
    pub time: i64,
    /// 表层标记：SHANIU 档案是只追加转录，当前可见投影一律 `current`。
    pub surface: &'static str,
    /// 已 `fts_text()` 归一化、可直接写入 FTS5 的文本。
    pub text: String,
    /// 按 Unicode 码点计的文档长度（排名用「文档短者优先」）。
    pub codepoint_length: i64,
}

/// 从一份档案的条目序列抽取全部可搜索文档；空文本不入库。
pub fn build_documents(items: &[RolloutItem], created_at: u64) -> Vec<SearchDoc> {
    // 与 lib.rs::project_entries 同一份配对规则：档案里输出排在调用之后，按 call_id 归位。
    let mut outputs: HashMap<String, String> = HashMap::new();
    for item in items {
        if let RolloutItem::ResponseItem {
            item: HistoryItem::FunctionCallOutput { call_id, output },
        } = item
        {
            outputs.insert(call_id.clone(), output.clone());
        }
    }

    let mut documents = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let base_seq = (index as i64) * 2;
        let push = |text: String, doc_type: &'static str, seq: i64, documents: &mut Vec<SearchDoc>| {
            if text.is_empty() {
                return;
            }
            let indexed = fts_text(&text);
            if indexed.is_empty() {
                return;
            }
            let codepoint_length = indexed.chars().count() as i64;
            documents.push(SearchDoc {
                seq,
                doc_type,
                time: created_at as i64 + seq,
                surface: "current",
                text: indexed,
                codepoint_length,
            });
        };

        match item {
            RolloutItem::ResponseItem {
                item: HistoryItem::Message { role, text, .. },
            } => {
                // reasoning_content 是模型协议回放字段，不是用户可见语义文本
                // （上游 extraction.ts 的 reasoning block 明确返回 []），不进索引。
                push(
                    join_text(&[text.trim()]),
                    role_doc_type(*role),
                    base_seq,
                    &mut documents,
                );
            }
            RolloutItem::ResponseItem {
                item: HistoryItem::FunctionCall { name, arguments, call_id },
            } => {
                push(
                    join_text(&[name.trim(), arguments.trim()]),
                    "tool/call",
                    base_seq,
                    &mut documents,
                );
                if let Some(output) = outputs.get(call_id) {
                    push(
                        join_text(&[output.trim()]),
                        "tool/result",
                        base_seq + 1,
                        &mut documents,
                    );
                }
            }
            // 输出已并进它自己的调用，不单独成条（与回放投影一致）。
            RolloutItem::ResponseItem {
                item: HistoryItem::FunctionCallOutput { .. },
            } => {}
            // 压缩锚点的语义就是「用摘要取代区间」，展示投影取 user 角色，索引同口径。
            RolloutItem::Compacted { summary, .. } => {
                push(
                    join_text(&[summary.trim()]),
                    "user/message",
                    base_seq,
                    &mut documents,
                );
            }
            // 带图消息只索引文字与图片路径：字节不在档案里，索引也没得可索引。
            RolloutItem::ResponseItem {
                item: HistoryItem::ImageMessage { role, text, images },
            } => {
                let paths = images
                    .iter()
                    .map(|image| image.path.as_str())
                    .collect::<Vec<_>>();
                let mut parts = vec![text.trim()];
                parts.extend(paths);
                push(join_text(&parts), role_doc_type(*role), base_seq, &mut documents);
            }
            // 首行 meta 与子代理引用不是可搜索内容。
            RolloutItem::SessionMeta { .. } | RolloutItem::SubagentRef { .. } => {}
        }
    }
    documents
}

fn role_doc_type(role: Role) -> &'static str {
    match role {
        Role::User => "user/message",
        Role::Assistant => "assistant/message",
        Role::System => "system/message",
    }
}

/// 上游 `extraction.ts` 的 `joinText`：逐段 trim、丢空段、换行拼接。
fn join_text(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}
