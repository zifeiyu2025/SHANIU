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

use std::collections::{HashMap, HashSet};

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
    /// 该段内容在**当前上下文**里的位置：
    /// - `current`：仍在模型看得到的历史里；
    /// - `shadowed`：已被压缩锚点取代——档案里还在、能搜到，但模型当前不再看到。
    ///
    /// 「能查到」与「还在生效」是两件事，搜索命中时必须能区分（否则用户会以为
    /// 模型还记得那段，并基于这个假设做判断）。
    pub surface: &'static str,
    /// 已 `fts_text()` 归一化、可直接写入 FTS5 的文本。
    pub text: String,
    /// 按 Unicode 码点计的文档长度（排名用「文档短者优先」）。
    pub codepoint_length: i64,
}

/// 从一份档案的条目序列抽取全部可搜索文档；空文本不入库。
pub fn build_documents(items: &[RolloutItem], created_at: u64) -> Vec<SearchDoc> {
    // 与 lib.rs::project_entries 同一份配对规则：档案里输出排在调用之后，按 call_id 归位。
    // 同时记住输出的档案下标，供 `surface` 判定（调用与输出可能落在压缩区间两侧）。
    let mut outputs: HashMap<String, (String, usize)> = HashMap::new();
    for (index, item) in items.iter().enumerate() {
        if let RolloutItem::ResponseItem {
            item: HistoryItem::FunctionCallOutput { call_id, output },
            ..
        } = item
        {
            outputs.insert(call_id.clone(), (output.clone(), index));
        }
    }
    let shadowed = shadowed_indices(items);
    let surface_of = |index: usize| {
        if shadowed.contains(&index) {
            "shadowed"
        } else {
            "current"
        }
    };

    let mut documents = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let base_seq = (index as i64) * 2;
        let push = |text: String,
                    doc_type: &'static str,
                    seq: i64,
                    surface: &'static str,
                    documents: &mut Vec<SearchDoc>| {
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
                surface,
                text: indexed,
                codepoint_length,
            });
        };

        match item {
            RolloutItem::ResponseItem {
                item: HistoryItem::Message { role, text, .. },
                ..
            } => {
                // reasoning_content 是模型协议回放字段，不是用户可见语义文本
                // （上游 extraction.ts 的 reasoning block 明确返回 []），不进索引。
                push(
                    join_text(&[text.trim()]),
                    role_doc_type(*role),
                    base_seq,
                    surface_of(index),
                    &mut documents,
                );
            }
            RolloutItem::ResponseItem {
                item: HistoryItem::FunctionCall { name, arguments, call_id },
                ..
            } => {
                push(
                    join_text(&[name.trim(), arguments.trim()]),
                    "tool/call",
                    base_seq,
                    surface_of(index),
                    &mut documents,
                );
                if let Some((output, output_index)) = outputs.get(call_id) {
                    push(
                        join_text(&[output.trim()]),
                        "tool/result",
                        base_seq + 1,
                        surface_of(*output_index),
                        &mut documents,
                    );
                }
            }
            // 输出已并进它自己的调用，不单独成条（与回放投影一致）。
            RolloutItem::ResponseItem {
                item: HistoryItem::FunctionCallOutput { .. },
                ..
            } => {}
            // 压缩锚点的语义就是「用摘要取代区间」：摘要本身是新的当前内容，
            // 被它取代的原文才转成 shadowed（见 `shadowed_indices`）。
            RolloutItem::Compacted { summary, .. } => {
                push(
                    join_text(&[summary.trim()]),
                    "user/message",
                    base_seq,
                    "current",
                    &mut documents,
                );
            }
            // 带图消息只索引文字与图片路径：字节不在档案里，索引也没得可索引。
            RolloutItem::ResponseItem {
                item: HistoryItem::ImageMessage { role, text, images },
                ..
            } => {
                let paths = images
                    .iter()
                    .map(|image| image.path.as_str())
                    .collect::<Vec<_>>();
                let mut parts = vec![text.trim()];
                parts.extend(paths);
                push(
                    join_text(&parts),
                    role_doc_type(*role),
                    base_seq,
                    surface_of(index),
                    &mut documents,
                );
            }
            // 首行 meta、子代理引用、档位事件、退出留痕与回合结局都不是可搜索内容。
            // 系统提示词也不进索引：它是宿主拼装的固定说明，不是用户/模型对话；
            // 混进来会让每次搜索都命中同一段模板，把真正的会话内容压下去。
            // 写后验证裁决同理：它记录的是宿主执行的命令与退出码，不是对话正文。
            RolloutItem::SessionMeta { .. }
            | RolloutItem::SubagentRef { .. }
            | RolloutItem::SandboxMode { .. }
            | RolloutItem::PermissionPreset { .. }
            | RolloutItem::AbortNote { .. }
            | RolloutItem::TurnEnd { .. }
            | RolloutItem::Verification { .. }
            | RolloutItem::SystemPrompt { .. } => {}
        }
    }
    documents
}

/// 会被压缩锚点取代的档案下标集合（`surface = shadowed`）。
///
/// 序号口径必须与 `rollout::resume` / `lib.rs::project_entries` 完全一致：
/// 只数进入历史序列的条目（消息、调用、**工具输出**、图片），锚点用一条摘要
/// 顶掉 `[range_start, range_end)`——所以这里用一个哨兵值占住摘要那个槽位，
/// 否则后一个锚点的区间会整体错位一格。
fn shadowed_indices(items: &[RolloutItem]) -> HashSet<usize> {
    let mut history: Vec<usize> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        if is_history_item(item) {
            history.push(index);
        } else if let RolloutItem::Compacted { range_start, range_end, .. } = item {
            // 越界锚点按"未压缩"处理：同一份档案在 resume/视图里会报错拒绝加载，
            // 索引这边没必要再制造第三个口径；标注缺失好过标注错位。
            if range_start <= range_end && *range_end <= history.len() {
                history.splice(*range_start..*range_end, std::iter::once(usize::MAX));
            }
        }
    }
    let visible: HashSet<usize> = history.into_iter().filter(|index| *index != usize::MAX).collect();
    items
        .iter()
        .enumerate()
        .filter(|(index, item)| is_history_item(item) && !visible.contains(index))
        .map(|(index, _)| index)
        .collect()
}

/// 是否进入历史序列（`rollout::resume` 的重放口径）。`TurnEnd` 等审计记录不在其中。
fn is_history_item(item: &RolloutItem) -> bool {
    matches!(
        item,
        RolloutItem::ResponseItem {
            item: HistoryItem::Message { .. }
                | HistoryItem::FunctionCall { .. }
                | HistoryItem::FunctionCallOutput { .. }
                | HistoryItem::ImageMessage { .. },
            ..
        }
    )
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
