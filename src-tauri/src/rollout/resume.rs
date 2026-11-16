//! resume / fork（docs/04 §3.1，SH-031）。
//!
//! resume 的本质是"把档案重放成运行时的 `history`"，因此它的正确性判据只有一条：
//! **重放出来的历史必须能直接喂给模型且不违反协议**。SH-082 已经证明，
//! 一条 `function_call` 后面缺配对的 `function_call_output`，严格端点直接 400——
//! 所以重放会给"有 call 无 output"的落单调用**补一条「结果未知」占位**
//! （[`ORPHAN_TOOL_OUTPUT`]），调用本身留在原位（不能剔除：剔了模型会以为那轮什么都没执行，
//! 然后盲目重跑）。压缩锚点、控制面记录与正常消息一条不丢。fork 仍按前缀截断，
//! 见 [`nearest_safe_cut`]。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::clock::now_ms;
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
    /// 会话内最后一次档位切换（无则 None = 跟随部署默认）。旧档案字段。
    pub sandbox_mode: Option<String>,
    /// 会话内最后一次权限预设切换（新档案；无则 None = 回落到 `sandbox_mode`/部署默认）。
    pub permission_preset: Option<String>,
    /// 档案里记录过的系统提示词版本（按写入顺序）。旧档案为空 = 未记录。
    ///
    /// **仅审计留档**：当前生产无读取方（`tests/wire_field_consumers.rs` 登记为 Gap），
    /// resume 不使用它、也不影响运行行为；版本漂移检测尚未接生产入口。
    pub prompt_versions: Vec<SystemPromptRecord>,
    /// 末行半截写被丢弃的行数。
    pub torn_tail_lines: usize,
    /// 中间坏行（已跳过）的 1-based 行号；空 = 档案完好。重放出来的历史因此可能缺条目。
    pub damaged_lines: Vec<usize>,
    /// 重放时给**落单 `function_call`** 补「结果未知」占位的条数。
    ///
    /// 典型成因：崩溃时档案停在 `function_call` 之后、`function_call_output` 之前。
    /// 这些调用留在模型历史里并紧跟一条 [`ORPHAN_TOOL_OUTPUT`] 占位——带着无回包的调用
    /// 续写，第一条请求就会被严格端点 400（SH-082）；而直接剔除会让模型以为那一轮
    /// 什么都没执行，进而盲目重跑非幂等命令。只数这个方向（有 call 无 output）：
    /// 落单输出与 `Compacted` 锚点、控制面/审计记录、正常消息全都原样保留。
    pub patched_orphan_calls: usize,
}

/// 档案里的一条系统提示词版本（只读快照，供审计与轨迹回放）。
///
/// 注意：整个结构当前只有测试读取，生产链路不消费——填了也不生效，仅审计留档。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemPromptRecord {
    /// 落盘后的文本（写入时已经过统一脱敏）。
    pub text: String,
    pub sha256: String,
    /// 写入时刻：仅审计留档，当前无读取方。
    pub at_ms: u64,
    pub turn: u32,
}

/// 崩溃恢复时补在孤立调用后面的结果文本。措辞要让模型做两件事：
/// 1) 不假设它执行成功；2) 需要时重新调用，但先检查副作用。
///
/// 首句固定成"工具调用结果未记录"：它同时是用户排查时的检索锚点，
/// 也是模型区分"宿主没记下来"与"工具真的返回了这句话"的唯一依据。
pub const ORPHAN_TOOL_OUTPUT: &str = "工具调用结果未记录：宿主在结果写入前中止（会话已恢复）。\
不要假设该调用已执行或已成功；如需继续，请先确认副作用，再决定是否重新调用。";

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
    // 档位事件不进模型历史，只取最后一条作为本会话的当前档位。
    let mut sandbox_mode: Option<String> = None;
    // 预设事件同理。两者都存在时以**预设**为准：新档案只写预设，旧档案只有沙箱档，
    // 同一档案里两种事件混排只可能出现在升级过渡期，那时预设是更晚、更完整的那次选择。
    let mut permission_preset: Option<String> = None;
    // 系统提示词版本不进模型历史（每轮由 SessionPrompt 重新组装），只做审计与回放。
    let mut prompt_versions: Vec<SystemPromptRecord> = Vec::new();
    for item in &archive.items {
        match item {
            RolloutItem::ResponseItem { item: h, .. } => history.push(h.clone()),
            RolloutItem::SandboxMode { mode, .. } => sandbox_mode = Some(mode.clone()),
            RolloutItem::PermissionPreset { preset, .. } => permission_preset = Some(preset.clone()),
            RolloutItem::SystemPrompt { text, sha256, at_ms, turn } => {
                // 自洽校验：档案被手工改动/写坏时，宁可拒绝 resume 也不能把它当真历史展示。
                // 只校验"文本与指纹是否匹配"，不拿当前配置重算——那等于用推测覆盖事实。
                let actual = rollout::sha256_hex(text);
                if &actual != sha256 {
                    return Err(RolloutError::Corrupted {
                        line: 0,
                        message: format!(
                            "系统提示词行校验失败（sha256 不符；档案内记录 {sha256}，实算 {actual}）"
                        ),
                    });
                }
                prompt_versions.push(SystemPromptRecord {
                    text: text.clone(),
                    sha256: sha256.clone(),
                    at_ms: *at_ms,
                    turn: *turn,
                });
            }
            // 回放只关心区间与摘要正文；时刻/触发是审计字段，重放历史里没有位置。
            RolloutItem::Compacted { range_start, range_end, summary, .. } => {
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
            RolloutItem::AbortNote { reason, .. } => {
                // 退出等待超时的强制中止留痕：不进模型历史，但必须在日志里看得见，
                // 否则“档案为什么断在这里”又变成无人能解释。
                tracing::warn!("会话档案含强制中止记录：{reason}");
            }
            RolloutItem::ApprovalRequest { tool, risk, .. } => {
                // 审批请求是审计/重开呈现用的事实，不是对话内容：喂给模型会让它
                // 把自己的权限询问当成上一轮说过的话。重开要呈现的那条「已失效」
                // 由投影层（`project_slots_with`）读档案给出。
                tracing::debug!("会话档案含审批请求：{tool}（风险 {risk}）");
            }
            RolloutItem::ApprovalDecision { decision, .. } => {
                tracing::debug!("会话档案含审批裁决：{decision}");
            }
            RolloutItem::TurnEnd { reason, .. } => {
                // 结局只服务于导出/统计/审计，不参与模型历史重建。
                tracing::debug!("会话档案回合结局：{reason}");
            }
            RolloutItem::Verification { .. } => {
                // 写后验证裁决是审计记录：它描述的是「宿主跑过什么」，不是对话内容。
                // 重放它会让模型把上一次的命令与诊断当成自己的话，故只留在盘上。
            }
            RolloutItem::ToolOutputPruned { call_id, .. } => {
                // 裁剪留痕只说明「模型当时看到的是裁剪版」，档案里的输出仍是原文。
                // 它不改变重放出来的历史（原文照常进历史，下次撞预算再裁一遍）。
                tracing::debug!("会话档案含确定性裁剪留痕：{call_id}");
            }
            RolloutItem::SessionMeta { .. } => {
                return Err(RolloutError::Corrupted {
                    line: 0,
                    message: "档案主体中不应再出现 session_meta".into(),
                });
            }
        }
    }

    // 重放不截断：锚点、控制面/审计记录、正常消息一律按档案顺序还原。唯一不安全的是
    // "有 call 无 output"的落单调用（SH-082：带着它续写，第一条请求就 400），所以在重放
    // 完成后**原位补一条「结果未知」占位**，而不是像 d610652 那样"从它开始把后面全丢掉"，
    // 也不是把调用剔除——剔除会让模型以为那一轮什么都没执行，进而重跑非幂等命令。
    // 崩溃之后用户接着说的话、以及写在其后的压缩锚点都不该陪葬。
    //
    // 配对必须按**最终历史**判：锚点可能把调用连同输出一起压掉，也可能只压掉调用、留下
    // 输出（落单输出在发请求前会被降级成文本，见 `model_client::degrade_orphan_tool_outputs`）。
    //
    // 只在内存里补，不回写档案：档案是 append-only 的，同一份档案重复 resume 必须得到
    // 同一份历史（幂等），盘上内容一个字都不改。
    let answered: HashSet<String> = history
        .iter()
        .filter_map(|item| match item {
            HistoryItem::FunctionCallOutput { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    let mut patched_orphan_calls = 0usize;
    let mut fixed: Vec<HistoryItem> = Vec::with_capacity(history.len());
    for item in history {
        match &item {
            HistoryItem::FunctionCall { call_id, .. } if !answered.contains(call_id) => {
                patched_orphan_calls += 1;
                let call_id = call_id.clone();
                fixed.push(item);
                // 紧跟在调用之后：运行时形态是"call 全在前、output 全在后"，但恢复场景下
                // 必须逐对相邻，否则紧邻段里的 call/output 配对会被打断
                // （`model_client::collect_call_group` 只收连续段）。
                fixed.push(HistoryItem::FunctionCallOutput {
                    call_id,
                    output: ORPHAN_TOOL_OUTPUT.to_string(),
                });
            }
            _ => fixed.push(item),
        }
    }
    let history = fixed;
    if patched_orphan_calls > 0 {
        tracing::warn!(
            path = %path.display(),
            patched = patched_orphan_calls,
            "档案里存在落单的工具调用：重放已补「结果未知」占位（盘上内容未改动）"
        );
    }

    Ok(Resumed {
        session_id,
        agent_id,
        parent_id,
        history,
        path: path.to_path_buf(),
        workspace_root,
        applied_compactions: applied,
        sandbox_mode,
        permission_preset,
        prompt_versions,
        torn_tail_lines: archive.torn_tail_lines,
        damaged_lines: archive.damaged_lines,
        patched_orphan_calls,
    })
}

/// 从 `want` 往下找最近一个**安全截断点**：该位置之前的条目里，每个 `function_call`
/// 都有配对的 `function_call_output`（按 `call_id` 单向判定，不校验出现顺序，
/// 也不看待配对条目本身是 message 还是工具记录）。
///
/// 为什么不能直接按用户选的条数切：一轮的工具记录是"先全部 call、再全部 output"，
/// 从中间截断就会留下有 call 无 output 的尾巴，续写出来的会话第一次请求就 400。
///
/// 返回值只承诺一件事：**`[0, k)` 可喂给模型**。它不保证 `items[k]` 是 message——
/// `0`（没有任何安全前缀）与末尾（`want` 本身安全）都可能返回，工具段之外也可能
/// 落在多条消息之间；调用方只能依赖"前缀可喂给模型"这一条契约。
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

/// `[0, items.len())` 里是否**每个调用都有输出**。
///
/// 只判这一个方向，不要求"每个输出都有调用"：截断只会制造有 call 无 output 的尾巴，
/// 反过来（有 output 无 call）只会来自档案被外部裁剪/重排，发请求前会被降级成文本
/// （`model_client::degrade_orphan_tool_outputs`），不会让请求失败。要求双向包含会把
/// 这类档案的切点一路退到 0，把正常消息与压缩锚点一起丢掉。
fn paired_prefix(items: &[RolloutItem]) -> bool {
    let mut calls: Vec<&str> = Vec::new();
    let mut outputs: Vec<&str> = Vec::new();
    for item in items {
        match item {
            RolloutItem::ResponseItem { item: HistoryItem::FunctionCall { call_id, .. }, .. } => {
                calls.push(call_id.as_str())
            }
            RolloutItem::ResponseItem {
                item: HistoryItem::FunctionCallOutput { call_id, .. },
                ..
            } => {
                outputs.push(call_id.as_str())
            }
            _ => {}
        }
    }
    calls.iter().all(|call| outputs.contains(call))
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
        created_at: now_ms(),
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
            at_ms: 1,
        }
    }

    fn call(id: &str) -> RolloutItem {
        RolloutItem::ResponseItem {
            item: HistoryItem::FunctionCall { name: "t".into(), arguments: "{}".into(), call_id: id.into() },
            at_ms: 2,
        }
    }

    fn out(id: &str) -> RolloutItem {
        RolloutItem::ResponseItem {
            item: HistoryItem::FunctionCallOutput { call_id: id.into(), output: "o".into() },
            at_ms: 3,
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
    fn replay_patches_the_unpaired_call_with_unknown_output() {
        let dir = temp_dir("truncate");
        let path = dir.join("a.jsonl");
        let w = RolloutWriter::create(&dir, &meta("a")).unwrap();
        w.append(&msg("任务")).unwrap();
        w.append(&call("c1")).unwrap();
        w.append(&out("c1")).unwrap();
        // 崩溃点：档案停在 call 之后、output 之前。重放给 c2 补一条「结果未知」。
        w.append(&call("c2")).unwrap();
        drop(w);

        let r = resume(&path).unwrap();
        assert_eq!(r.history.len(), 5, "c2 必须留在历史里并带占位：{:#?}", r.history);
        assert_eq!(r.patched_orphan_calls, 1, "补占位的条数必须带出来");
        match &r.history[4] {
            HistoryItem::FunctionCallOutput { call_id, output } => {
                assert_eq!(call_id, "c2");
                assert_eq!(output, ORPHAN_TOOL_OUTPUT);
            }
            other => panic!("落单调用后面应紧跟占位，收到 {other:?}"),
        }
        // 反向对照：盘上内容原样保留，补写只影响这次重放。
        assert_eq!(rollout::read(&path).unwrap().items.len(), 4);
    }

    #[test]
    fn replay_keeps_messages_that_follow_an_unpaired_call() {
        let dir = temp_dir("unpaired-mid");
        let path = dir.join("a.jsonl");
        let w = RolloutWriter::create(&dir, &meta("a")).unwrap();
        w.append(&msg("任务")).unwrap();
        w.append(&call("c1")).unwrap(); // 崩溃/坏行：c1 永远等不到 output
        w.append(&msg("补一句")).unwrap(); // 崩溃之后用户接着说的话，不许陪葬
        // 锚点只压第一轮任务，落单的 c1 仍然要在最终历史上补占位——两种机制互不干扰。
        w.append(&RolloutItem::Compacted {
            range_start: 0,
            range_end: 1,
            summary: "此前：任务".into(),
            at_ms: 0,
            trigger: None,
        })
            .unwrap();
        drop(w);

        let r = resume(&path).unwrap();
        assert_eq!(r.patched_orphan_calls, 1, "只该给 c1 补占位");
        assert_eq!(r.applied_compactions, 1, "写在落单调用之后的锚点同样要生效");
        assert_eq!(r.history.len(), 4, "摘要、调用、占位与「补一句」都必须保留：{:#?}", r.history);
        match &r.history[0] {
            HistoryItem::Message { role, text, .. } => {
                assert_eq!(*role, Role::User);
                assert!(text.contains("此前"), "摘要应回放成 user：{text}");
            }
            other => panic!("压缩锚点应回放为 message，收到 {other:?}"),
        }
        match &r.history[1] {
            HistoryItem::FunctionCall { call_id, .. } => assert_eq!(call_id, "c1"),
            other => panic!("落单调用必须保留，收到 {other:?}"),
        }
        match &r.history[2] {
            HistoryItem::FunctionCallOutput { call_id, output } => {
                assert_eq!(call_id, "c1");
                assert_eq!(output, ORPHAN_TOOL_OUTPUT);
            }
            other => panic!("落单调用后面应紧跟占位，收到 {other:?}"),
        }
        match &r.history[3] {
            HistoryItem::Message { text, .. } => assert_eq!(text, "补一句"),
            other => panic!("落单调用之后的消息必须保留，收到 {other:?}"),
        }
    }

    #[test]
    fn sandbox_mode_is_per_session_and_never_leaks_across_sessions() {
        let dir = temp_dir("sandbox-scope");
        let a = dir.join("a.jsonl");
        let b = dir.join("b.jsonl");
        let wa = RolloutWriter::create(&dir, &meta("a")).unwrap();
        wa.append(&msg("任务")).unwrap();
        wa.append(&RolloutItem::SandboxMode { mode: "danger_full_access".into(), at_ms: 1 }).unwrap();
        drop(wa);
        let wb = RolloutWriter::create(&dir, &meta("b")).unwrap();
        wb.append(&msg("任务")).unwrap();
        drop(wb);

        let ra = resume(&a).unwrap();
        assert_eq!(ra.sandbox_mode.as_deref(), Some("danger_full_access"));
        // 反向对照：B 从没切过档，绝不能继承 A 的档位 → None 由调用方回落部署默认。
        assert_eq!(resume(&b).unwrap().sandbox_mode, None);
        // 控制面事件不进模型历史：否则模型会读到一行"沙箱切换到……"。
        assert_eq!(ra.history.len(), 1);
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
            at_ms: 0,
            trigger: None,
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
        w.append(&RolloutItem::Compacted {
            range_start: 0,
            range_end: 9,
            summary: "越界".into(),
            at_ms: 0,
            trigger: None,
        })
        .unwrap();
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

    #[test]
    fn system_prompt_round_trips_and_is_verified() {
        let dir = temp_dir("prompt");
        let path = dir.join("p.jsonl");
        let raw = "你是 SHANIU。\n## 工作区\nD:\\ws\n";
        // 与写入侧同口径：指纹对落盘文本（脱敏后）计算。
        let stored = crate::config::redact(raw);
        let sha = crate::rollout::sha256_hex(&stored);
        let w = RolloutWriter::create(&dir, &meta("p")).unwrap();
        w.append(&RolloutItem::SystemPrompt {
            text: raw.into(),
            sha256: sha.clone(),
            at_ms: 9,
            turn: 1,
        })
        .unwrap();
        w.append(&msg("任务")).unwrap();
        drop(w);

        let r = resume(&path).unwrap();
        assert_eq!(r.prompt_versions.len(), 1);
        assert_eq!(r.prompt_versions[0].sha256, sha);
        assert_eq!(r.prompt_versions[0].turn, 1);
        // 提示词不进模型历史：它每轮重新组装，混进历史会逐轮累积。
        assert_eq!(r.history.len(), 1);
    }

    #[test]
    fn tampered_system_prompt_is_rejected_not_ignored() {
        let dir = temp_dir("prompt-bad");
        let path = dir.join("p.jsonl");
        let stored = "原始提示词";
        let sha = crate::rollout::sha256_hex(stored);
        let w = RolloutWriter::create(&dir, &meta("p")).unwrap();
        w.append(&RolloutItem::SystemPrompt {
            text: stored.into(),
            sha256: sha,
            at_ms: 1,
            turn: 1,
        })
        .unwrap();
        drop(w);

        // 反向对照：不改 → 能 resume（上面的 round_trips 用例已覆盖，这里再锁一次）
        assert!(resume(&path).is_ok());

        // 手工改一个字符：档案已不自洽，必须硬错误而不是当成"未记录"。
        let tampered = std::fs::read_to_string(&path).unwrap().replace("原始提示词", "原始提示语");
        std::fs::write(&path, tampered).unwrap();
        let err = resume(&path).unwrap_err();
        assert!(matches!(err, RolloutError::Corrupted { .. }), "篡改必须拒绝 resume：{err}");
    }

    #[test]
    fn legacy_archive_without_system_prompt_has_no_versions() {
        let dir = temp_dir("prompt-legacy");
        let path = dir.join("old.jsonl");
        let w = RolloutWriter::create(&dir, &meta("old")).unwrap();
        w.append(&msg("旧会话")).unwrap();
        drop(w);
        let r = resume(&path).unwrap();
        assert!(r.prompt_versions.is_empty(), "旧档案只能显示未记录，禁止回填");
    }
}
