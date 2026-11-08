//! resume / fork（docs/04 §3.1，SH-031）。
//!
//! resume 的本质是"把档案重放成运行时的 `history`"，因此它的正确性判据只有一条：
//! **重放出来的历史必须能直接喂给模型且不违反协议**。SH-082 已经证明，
//! 一条 `function_call` 后面缺配对的 `function_call_output`，严格端点直接 400——
//! 所以 fork 的截断点不能随便选，见 [`nearest_safe_cut`]。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::model_client::{HistoryItem, Role};
use crate::rollout::{self, RolloutError, RolloutItem, RolloutWriter};

/// 重放出来的会话：档案元信息 + 可直接续用的历史 + 档案路径。
#[derive(Debug, Clone, PartialEq)]
pub struct Resumed {
    pub session_id: String,
    pub agent_id: String,
    pub parent_id: Option<String>,
    pub history: Vec<HistoryItem>,
    pub path: PathBuf,
    /// 会话建档时固化的工作区根（SH-099）：resume 时工具作用域跟随原目录。
    /// 旧档案没有该字段时是空串，调用方回落全局默认根。
    pub workspace_root: String,
    /// 档案里被应用的压缩锚点数（SH-032 落地后用于核对）。
    pub applied_compactions: usize,
    /// 末行半截写被丢弃的行数。
    pub torn_tail_lines: usize,
}

/// 重放档案 → 历史。
///
/// `Compacted` 锚点按记录顺序就地生效：用 `[range_start, range_end)` 换成一条摘要
/// **user** 消息。
///
/// 这里原本取的是 assistant，理由是"摘要是一个模型此前说过的话的浓缩，归 assistant
/// 语义最正"。**那个选择在 thinking 档下不成立**：端点要求重放的每条 assistant 消息
/// 都带回 `reasoning_content`（SH-083），而摘要没有这个字段，真机基线里任务因此直接
/// 400。语义上也站得住另一种读法——摘要是交给下一个模型的**交接指令**，不是复述旧回答。
/// codex 同此（`CompactionSummary::role()` 返回 "user"）。
///
/// 不变量③（04 §6.1，系统提示每轮重算）禁止的是塞进 **system**，user 不受那条约束，
/// 所以当初那条论证真正排除的只有 system，并没有支持 assistant。
pub fn resume(path: &Path) -> Result<Resumed, RolloutError> {
    let archive = rollout::read(path)?;
    let (session_id, agent_id, parent_id, workspace_root) = match &archive.meta {
        RolloutItem::SessionMeta { session_id, agent_id, parent_id, config_snapshot, .. } => {
            (session_id.clone(), agent_id.clone(), parent_id.clone(), config_snapshot.workspace_root.clone())
        }
        // read() 已保证首行是 session_meta，这里不可达；留 debug_assert 而不是运行时兜底
        other => {
            debug_assert!(false, "read 应已拒绝非 meta 首行，收到 {other:?}");
            return Err(RolloutError::NoSessionMeta(format!("{}", path.display())));
        }
    };

    let mut history: Vec<HistoryItem> = Vec::new();
    let mut applied = 0usize;
    for item in &archive.items {
        match item {
            RolloutItem::ResponseItem { item: h } => history.push(h.clone()),
            RolloutItem::Compacted { range_start, range_end, summary } => {
                let (start, end) = (*range_start, *range_end);
                if end > history.len() || start > end {
                    return Err(RolloutError::Corrupted {
                        line: 0,
                        message: format!(
                            "压缩锚点区间 [{start},{end}) 超出已重放长度 {}",
                            history.len()
                        ),
                    });
                }
                let summary_item = HistoryItem::Message {
                    // 摘要取 **user** 角色，不是 assistant：thinking 档要求重放的每条
                    // assistant 消息都带 `reasoning_content`（SH-083），摘要没有那个字段，
                    // 以 assistant 身份进历史会被端点直接 400。04 §6.1 不变量③ 禁的是塞进
                    // system，user 不受那条约束；codex 的 `CompactionSummary::role()` 同样是 "user"。
                    role: Role::User,
                    text: summary.clone(),
                    reasoning: None,
                };
                history.splice(start..end, [summary_item]);
                applied += 1;
            }
            RolloutItem::SubagentRef { .. } => {
                // 子代理引用不进父会话历史；整树 replay 是 SH-056 的范围
            }
            RolloutItem::SessionMeta { .. } => {
                return Err(RolloutError::Corrupted {
                    line: 0,
                    message: "档案主体中不应再出现 session_meta".into(),
                });
            }
        }
    }

    Ok(Resumed {
        session_id,
        agent_id,
        parent_id,
        history,
        path: path.to_path_buf(),
        workspace_root,
        applied_compactions: applied,
        torn_tail_lines: archive.torn_tail_lines,
    })
}

/// 从 `want` 往下找最近一个**安全截断点**：该位置之前的历史里，每个 `function_call`
/// 都有配对的 `function_call_output`。
///
/// 为什么不能直接按用户选的条数切：一轮的工具记录是"先全部 call、再全部 output"，
/// 从中间截断就会留下有 call 无 output 的尾巴，续写出来的会话第一次请求就 400。
/// 安全点必然落在某轮工具段开始之前，也就是索引处是 `message`（或干脆到末尾）。
pub fn nearest_safe_cut(items: &[RolloutItem], want: usize) -> usize {
    let want = want.min(items.len());
    // 从 want 往下逐个试：[0, k) 是否配对完整
    for k in (0..=want).rev() {
        if paired_prefix(&items[..k]) {
            return k;
        }
    }
    0
}

fn paired_prefix(items: &[RolloutItem]) -> bool {
    let mut calls: Vec<&str> = Vec::new();
    let mut outputs: Vec<&str> = Vec::new();
    for item in items {
        match item {
            RolloutItem::ResponseItem { item: HistoryItem::FunctionCall { call_id, .. } } => {
                calls.push(call_id.as_str())
            }
            RolloutItem::ResponseItem { item: HistoryItem::FunctionCallOutput { call_id, .. } } => {
                outputs.push(call_id.as_str())
            }
            _ => {}
        }
    }
    calls.iter().all(|c| outputs.contains(c)) && outputs.iter().all(|o| calls.contains(o))
}

/// fork 的结果：新档案路径 + 实际采用的截断点 + 因不安全而被回退掉的条目数。
#[derive(Debug, Clone, PartialEq)]
pub struct Forked {
    pub path: PathBuf,
    pub new_agent_id: String,
    pub kept_items: usize,
    pub dropped_items: usize,
}

/// 复制档案到 `<dir>/<new_agent_id>.jsonl`，截断到最近安全点，并写入带 `parent_id` 的新首行。
///
/// 原档案**只读**：fork 出错的代价不该由父会话的历史承担。
pub fn fork(src: &Path, dir: &Path, new_agent_id: &str, want_items: usize) -> Result<Forked, RolloutError> {
    let archive = rollout::read(src)?;
    let parent = match &archive.meta {
        RolloutItem::SessionMeta { agent_id, config_snapshot, .. } => (agent_id.clone(), config_snapshot.clone()),
        other => return Err(RolloutError::NoSessionMeta(format!("{:?}", other))),
    };
    let keep = nearest_safe_cut(&archive.items, want_items);
    std::fs::create_dir_all(dir).map_err(|e| RolloutError::Io(format!("{dir:?}: {e}")))?;
    let path = dir.join(format!("{new_agent_id}.jsonl"));
    let meta = RolloutItem::SessionMeta {
        session_id: new_agent_id.to_string(),
        agent_id: new_agent_id.to_string(),
        parent_id: Some(parent.0),
        created_at: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or_default(),
        config_snapshot: parent.1,
    };
    let writer = RolloutWriter::create(dir, &meta)?;
    for item in &archive.items[..keep] {
        writer.append(item)?;
    }
    Ok(Forked {
        path,
        new_agent_id: new_agent_id.to_string(),
        kept_items: keep,
        dropped_items: want_items.saturating_sub(keep),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_client::HistoryItem;
    use crate::rollout::ConfigSnapshot;

    fn temp_dir(tag: &str) -> crate::test_support::TestTempDir {
        crate::test_support::TestTempDir::new(&format!("resume-{tag}"))
    }

    fn meta(agent: &str) -> RolloutItem {
        RolloutItem::SessionMeta {
            session_id: agent.into(),
            agent_id: agent.into(),
            parent_id: None,
            created_at: 1,
            config_snapshot: ConfigSnapshot { model: "m".into(), ..Default::default() },
        }
    }

    fn msg(text: &str) -> RolloutItem {
        RolloutItem::ResponseItem {
            item: HistoryItem::Message { role: Role::User, text: text.into(), reasoning: None },
        }
    }

    fn call(id: &str) -> RolloutItem {
        RolloutItem::ResponseItem {
            item: HistoryItem::FunctionCall { name: "t".into(), arguments: "{}".into(), call_id: id.into() },
        }
    }

    fn out(id: &str) -> RolloutItem {
        RolloutItem::ResponseItem {
            item: HistoryItem::FunctionCallOutput { call_id: id.into(), output: "o".into() },
        }
    }

    #[test]
    fn safe_cut_never_splits_a_tool_call_from_its_output() {
        // 一轮工具段的真实形态：先全部 call，再全部 output
        let items = vec![msg("任务"), call("c1"), call("c2"), out("c1"), out("c2"), msg("下一轮")];
        assert_eq!(nearest_safe_cut(&items, 5), 5, "c1/c2 都已配对，5 是安全点");
        assert_eq!(nearest_safe_cut(&items, 4), 1, "切在 [..4) 会留下 c2 无输出 → 回退到 1");
        assert_eq!(nearest_safe_cut(&items, 3), 1, "切在调用段中间一律回退");
        assert_eq!(nearest_safe_cut(&items, 0), 0);
        assert_eq!(nearest_safe_cut(&items, 99), 6, "超出长度按末尾处理");
    }

    #[test]
    fn resume_replays_history_and_applies_compaction() {
        let dir = temp_dir("replay");
        let path = dir.join("a.jsonl");
        let w = RolloutWriter::create(&dir, &meta("a")).unwrap();
        w.append(&msg("第一轮")).unwrap();
        w.append(&call("c1")).unwrap();
        w.append(&out("c1")).unwrap();
        w.append(&RolloutItem::Compacted {
            range_start: 0,
            range_end: 3,
            summary: "此前：已完成第一轮".into(),
        })
        .unwrap();
        w.append(&msg("第二轮")).unwrap();
        drop(w);

        let r = resume(&path).unwrap();
        assert_eq!(r.agent_id, "a");
        assert_eq!(r.applied_compactions, 1);
        assert_eq!(r.history.len(), 2, "3 条被压成 1 条摘要 + 第二轮 1 条");
        match &r.history[0] {
            HistoryItem::Message { role, text, .. } => {
                // 摘要必须是 user：以 assistant 回放会让 thinking 端点因缺
                // reasoning_content 直接 400（SH-032 修正，见模块头注记）。
                assert_eq!(*role, Role::User);
                assert!(text.contains("此前"), "摘要应作为 user 侧交接指令回放：{text}");
            }
            other => panic!("压缩锚点应回放为 message，收到 {other:?}"),
        }
    }

    #[test]
    fn resume_rejects_compaction_anchor_out_of_range() {
        let dir = temp_dir("badanchor");
        let w = RolloutWriter::create(&dir, &meta("a")).unwrap();
        w.append(&msg("只有一条")).unwrap();
        w.append(&RolloutItem::Compacted { range_start: 0, range_end: 9, summary: "越界".into() }).unwrap();
        drop(w);
        let err = resume(&dir.join("a.jsonl")).unwrap_err();
        assert!(matches!(err, RolloutError::Corrupted { .. }), "越界锚点必须报错：{err}");
    }

    #[test]
    fn fork_writes_parent_link_and_leaves_source_untouched() {
        let dir = temp_dir("fork");
        {
            let w = RolloutWriter::create(&dir, &meta("parent")).unwrap();
            w.append(&msg("任务")).unwrap();
            w.append(&call("c1")).unwrap();
            w.append(&out("c1")).unwrap();
            w.append(&call("c2")).unwrap(); // 故意留一个没配对的调用
        }
        let src = dir.join("parent.jsonl");
        let before = std::fs::read_to_string(&src).unwrap();

        let f = fork(&src, &dir, "child", 4).unwrap();
        assert_eq!(f.kept_items, 3, "第 4 条是未配对调用 → 回退");
        assert_eq!(f.dropped_items, 1);
        assert_eq!(std::fs::read_to_string(&src).unwrap(), before, "fork 不得改动父档案");

        let child = resume(&f.path).unwrap();
        assert_eq!(child.parent_id.as_deref(), Some("parent"), "新档案首行要指回父代理");
        assert_eq!(child.history.len(), 3);
    }

    #[test]
    fn open_existing_refuses_files_without_meta() {
        let dir = temp_dir("nohead");
        let path = dir.join("bare.jsonl");
        std::fs::write(&path, "{}\n").unwrap();
        let err = RolloutWriter::open_existing(&path).map(|_| ()).unwrap_err();
        assert!(matches!(err, RolloutError::Corrupted { .. }), "非 session_meta 首行必须拒绝：{err}");
    }
}
