//! compaction 锚点 ↔ resume 回放的闭环（SH-032 验收「长会话不丢上下文」）。
//!
//! 这里刻意**不**跑模型：压缩的判定、区间与锚点形状全是纯计算，而"上下文到底丢没丢"
//! 取决于锚点写进去以后 `resume` 能不能把它接回一份合法历史。那才是这条验收的真判据。
//! 真端点上的表现另由 `SHANIU_EVAL_TOKEN_BUDGET` 驱动的评估基线覆盖（见 evals_baseline.rs）。

use shaniu_lib::compaction;
use shaniu_lib::model_client::{HistoryItem, Role};
use shaniu_lib::rollout::{self, ConfigSnapshot, RolloutItem, RolloutWriter};

fn temp_dir(tag: &str) -> shaniu_lib::test_support::TestTempDir {
    shaniu_lib::test_support::TestTempDir::new(&format!("compaction-{tag}"))
}

fn meta(agent: &str) -> RolloutItem {
    RolloutItem::SessionMeta {
        session_id: agent.into(),
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

fn user_msg(text: &str) -> HistoryItem {
    HistoryItem::Message { role: Role::User, text: text.into(), reasoning: None }
}

fn big_output(id: &str, size: usize) -> HistoryItem {
    HistoryItem::FunctionCallOutput { call_id: id.into(), output: "z".repeat(size) }
}

fn tool_call(id: &str) -> HistoryItem {
    HistoryItem::FunctionCall { name: "t".into(), arguments: "{}".into(), call_id: id.into() }
}

fn text_of(item: &HistoryItem) -> Option<&str> {
    match item {
        HistoryItem::Message { text, .. } => Some(text),
        _ => None,
    }
}

/// 压缩 + 锚点落盘 + 重放的完整一遍。
#[test]
fn anchor_written_by_compaction_replays_into_a_shorter_but_complete_history() {
    let dir = temp_dir("replay");
    let mut items = vec![user_msg("任务：把端口改成 9090")];
    for index in 0..6 {
        items.push(big_output(&format!("c{index}"), 40_000));
    }
    let original_len = items.len();

    let writer = RolloutWriter::create(&dir, &meta("a1")).unwrap();
    for item in &items {
        writer.append(&RolloutItem::ResponseItem { item: item.clone(), at_ms: 0 }).unwrap();
    }
    let path = writer.path().to_path_buf();
    drop(writer);

    let target = compaction::plan_if_needed(0, &items, 50_000).expect("240k 字节必须触发压缩");
    assert!(target.range_end < original_len, "必须至少留最新一条不压");

    // 锚点写在被替换的那些条目之后——这是 resume 能把它接上的前提。
    let writer = RolloutWriter::open_existing(&path).unwrap();
    writer
        .append(&RolloutItem::Compacted {
            range_start: target.range_start,
            range_end: target.range_end,
            summary: "交接摘要：目标是把端口改成 9090".into(),
        })
        .unwrap();
    drop(writer);

    let resumed = rollout::resume::resume(&path).expect("锚点区间合法，resume 不该报错");
    assert_eq!(resumed.applied_compactions, 1);
    assert!(
        resumed.history.len() < original_len,
        "压缩必须真的让历史变短：{} → {original_len}",
        resumed.history.len()
    );
    // 摘要在场
    assert!(
        resumed
            .history
            .iter()
            .any(|item| text_of(item) == Some("交接摘要：目标是把端口改成 9090")),
        "摘要必须进重放出的历史，否则就是丢了上下文"
    );
    // 最新那条大块输出必须保留（它才是模型下一步要用的东西）
    assert_eq!(
        text_of(resumed.history.last().unwrap()),
        None,
        "最后一条应仍是保留段里的工具输出，而不是摘要"
    );
    assert!(matches!(
        resumed.history.last().unwrap(),
        HistoryItem::FunctionCallOutput { call_id, .. } if call_id == "c5"
    ));
}

/// 摘要的角色必须是 **user**：thinking 档端点要求重放的每条 assistant 消息都带
/// `reasoning_content`（SH-083），而摘要没有那个字段。
///
/// 这条不是理论洁癖：把摘要写成 assistant，1500 预算的真机基线里 `search-and-report`
/// 就以 `The reasoning_content in the thinking mode must be passed back to the API` 挂掉。
/// 04 §6.1 不变量③ 禁的是塞进 system，user 不受那条约束。
#[test]
fn summary_lands_as_a_user_message_not_an_assistant_one() {
    let dir = temp_dir("role");
    let items = vec![user_msg("任务"), big_output("c0", 40_000), big_output("c1", 40_000)];
    let writer = RolloutWriter::create(&dir, &meta("a2")).unwrap();
    for item in &items {
        writer.append(&RolloutItem::ResponseItem { item: item.clone(), at_ms: 0 }).unwrap();
    }
    let path = writer.path().to_path_buf();
    let target = compaction::plan_if_needed(0, &items, 20_000).unwrap();
    writer
        .append(&RolloutItem::Compacted {
            range_start: target.range_start,
            range_end: target.range_end,
            summary: "摘要".into(),
        })
        .unwrap();
    drop(writer);

    let resumed = rollout::resume::resume(&path).unwrap();
    let summary_item = resumed
        .history
        .iter()
        .find(|item| text_of(item) == Some("摘要"))
        .expect("摘要应在历史里");
    match summary_item {
        HistoryItem::Message { role, .. } => assert_eq!(
            *role,
            Role::User,
            "摘要以 assistant 身份进历史会让 thinking 端点直接 400（它没有 reasoning_content）"
        ),
        other => panic!("摘要必须是 message，得到 {other:?}"),
    }
}

/// 区间越界必须报错而不是静默吞掉：静默会让一份坏档案伪装成"压缩成功"。
#[test]
fn out_of_range_target_is_rejected_by_resume() {
    let dir = temp_dir("oor");
    let writer = RolloutWriter::create(&dir, &meta("a3")).unwrap();
    writer
        .append(&RolloutItem::ResponseItem { item: user_msg("只有一条"), at_ms: 0 })
        .unwrap();
    let path = writer.path().to_path_buf();
    writer
        .append(&RolloutItem::Compacted {
            range_start: 0,
            range_end: 9,
            summary: "越界摘要".into(),
        })
        .unwrap();
    drop(writer);

    let err = rollout::resume::resume(&path).expect_err("越界区间必须报错");
    assert!(
        matches!(err, rollout::RolloutError::Corrupted { .. }),
        "越界要判成档案损坏，而不是悄悄跳过：{err}"
    );
}

/// 把因果链一次钉死：压缩后的历史喂进 ChatCompletions 请求构造，
/// 每条 assistant 消息都必须带 `reasoning_content`——这正是端点 400 的那条要求。
/// 摘要一旦写成 assistant，这条就会红，而不是等到真机基线才发现。
#[test]
fn compacted_history_still_satisfies_thinking_mode_reasoning_requirement() {
    use shaniu_lib::model_client::chat_messages;

    let dir = temp_dir("thinking");
    let mut items = vec![user_msg("任务：找出配置文件里的端口")];
    for index in 0..4 {
        items.push(HistoryItem::Message {
            role: Role::Assistant,
            text: format!("第 {index} 轮结论"),
            reasoning: Some(format!("第 {index} 轮思路")),
        });
        items.push(big_output(&format!("c{index}"), 30_000));
    }
    let writer = RolloutWriter::create(&dir, &meta("a4")).unwrap();
    for item in &items {
        writer.append(&RolloutItem::ResponseItem { item: item.clone(), at_ms: 0 }).unwrap();
    }
    let path = writer.path().to_path_buf();
    let target = compaction::plan_if_needed(0, &items, 30_000).unwrap();
    writer
        .append(&RolloutItem::Compacted {
            range_start: target.range_start,
            range_end: target.range_end,
            summary: "交接摘要".into(),
        })
        .unwrap();
    drop(writer);

    let resumed = rollout::resume::resume(&path).unwrap();
    let messages = chat_messages("系统提示", &resumed.history);
    let offenders: Vec<_> = messages
        .iter()
        .filter(|m| m["role"] == "assistant" && m.get("reasoning_content").is_none())
        .collect();
    assert!(
        offenders.is_empty(),
        "压缩后仍有 {n} 条 assistant 消息缺 reasoning_content，thinking 档会直接 400：{offenders:?}",
        n = offenders.len()
    );
    // 摘要必须在场，且是以 user 身份——否则等于"为了过协议把上下文丢了"
    assert!(
        messages
            .iter()
            .any(|m| m["role"] == "user" && m["content"].as_str() == Some("交接摘要")),
        "摘要必须仍在请求里"
    );
}

/// compaction 自己不该产越界区间：`range_end` 必须严格小于历史长度。
#[test]
fn planner_never_emits_a_range_that_eats_the_whole_history() {
    for size in 2..12usize {
        let mut items = vec![user_msg("任务")];
        for index in 0..size {
            items.push(big_output(&format!("c{index}"), 9_000));
        }
        if let Some(target) = compaction::plan_if_needed(0, &items, 30_000) {
            assert!(
                target.range_end < items.len(),
                "size={size} 时区间 [{},{}) 会吃掉全部历史",
                target.range_start,
                target.range_end
            );
        }
    }
}

/// 回归（CI 43292778234）：工具输出可以没有配对的调用——`chat_messages` 明确把这种
/// 「落单输出」当正常输入（"只有外部裁剪/重排历史才会出现，按 call_id 原样发出"），
/// 所以它**不是**不安全尾部。把配对判成双向包含会让这类档案的安全切点一路退到 0，
/// 连写在尾部的压缩锚点一起丢掉：`applied_compactions` 归零，摘要与最新输出全部消失。
#[test]
fn orphan_tool_outputs_do_not_cut_away_the_tail_anchor() {
    let dir = temp_dir("orphan-out");
    let items = vec![user_msg("任务"), big_output("c0", 40_000), big_output("c1", 40_000)];
    let writer = RolloutWriter::create(&dir, &meta("a5")).unwrap();
    for item in &items {
        writer.append(&RolloutItem::ResponseItem { item: item.clone(), at_ms: 0 }).unwrap();
    }
    let path = writer.path().to_path_buf();
    let target = compaction::plan_if_needed(0, &items, 20_000).expect("120k 字节必须触发压缩");
    writer
        .append(&RolloutItem::Compacted {
            range_start: target.range_start,
            range_end: target.range_end,
            summary: "摘要".into(),
        })
        .unwrap();
    drop(writer);

    let resumed = rollout::resume::resume(&path).unwrap();
    assert_eq!(resumed.applied_compactions, 1, "写在尾部的锚点不得被截断丢掉");
    assert!(
        resumed.history.iter().any(|item| text_of(item) == Some("摘要")),
        "摘要必须进重放出的历史，否则就是丢了上下文"
    );
    // 最新那条输出是模型下一步要用的，不能被一起截掉。
    assert!(
        matches!(
            resumed.history.last().unwrap(),
            HistoryItem::FunctionCallOutput { call_id, .. } if call_id == "c1"
        ),
        "保留段必须以最新工具输出收尾：{:#?}",
        resumed.history
    );
    assert_eq!(resumed.dropped_unsafe_tail, 0, "没有任何落单调用，不该报截断");
}

/// 锚点写在一轮「有 call 无 output」的落单调用之后（崩溃后先 resume、再压缩就是这个形状）：
/// 截断只能丢掉那条落单调用，锚点照常生效。
#[test]
fn anchor_written_after_an_unfinished_round_is_still_applied() {
    let dir = temp_dir("anchor-after-torn");
    let items = vec![user_msg("任务"), tool_call("c1"), big_output("c1", 1_000), tool_call("c2")];
    let writer = RolloutWriter::create(&dir, &meta("a6")).unwrap();
    for item in &items {
        writer.append(&RolloutItem::ResponseItem { item: item.clone(), at_ms: 0 }).unwrap();
    }
    let path = writer.path().to_path_buf();
    writer
        .append(&RolloutItem::Compacted { range_start: 0, range_end: 3, summary: "交接摘要".into() })
        .unwrap();
    drop(writer);

    let resumed = rollout::resume::resume(&path).unwrap();
    assert_eq!(resumed.applied_compactions, 1, "锚点落在落单调用之后，也必须重放");
    assert_eq!(resumed.dropped_unsafe_tail, 1, "只该丢 c2 这一条落单调用");
    assert_eq!(
        resumed.history.len(),
        1,
        "区间 [0,3) 应被换成一条摘要：{:#?}",
        resumed.history
    );
    assert_eq!(text_of(&resumed.history[0]), Some("交接摘要"));
}

/// 锚点之后紧跟一轮未完成的工具调用（压缩之后又崩在 call 之后）：
/// 锚点必须留下，只丢那条落单调用。
#[test]
fn anchor_survives_an_unfinished_round_that_follows_it() {
    let dir = temp_dir("anchor-before-torn");
    let writer = RolloutWriter::create(&dir, &meta("a7")).unwrap();
    for item in [user_msg("任务"), tool_call("c1"), big_output("c1", 1_000)] {
        writer.append(&RolloutItem::ResponseItem { item, at_ms: 0 }).unwrap();
    }
    writer
        .append(&RolloutItem::Compacted { range_start: 0, range_end: 3, summary: "交接摘要".into() })
        .unwrap();
    // 崩溃点：c2 只有调用、没有输出。
    writer.append(&RolloutItem::ResponseItem { item: tool_call("c2"), at_ms: 0 }).unwrap();
    let path = writer.path().to_path_buf();
    drop(writer);

    let resumed = rollout::resume::resume(&path).unwrap();
    assert_eq!(resumed.applied_compactions, 1, "锚点之后的落单调用不得连带丢掉锚点");
    assert_eq!(resumed.dropped_unsafe_tail, 1, "只该丢 c2 这一条落单调用");
    assert_eq!(
        resumed.history.len(),
        1,
        "区间 [0,3) 应被换成一条摘要：{:#?}",
        resumed.history
    );
    assert_eq!(text_of(&resumed.history[0]), Some("交接摘要"));
}
