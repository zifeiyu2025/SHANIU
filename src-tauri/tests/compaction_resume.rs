//! compaction 锚点 ↔ resume 回放的闭环（SH-032 验收「长会话不丢上下文」）。
//!
//! 这里刻意**不**跑模型：压缩的判定、区间与锚点形状全是纯计算，而"上下文到底丢没丢"
//! 取决于锚点写进去以后 `resume` 能不能把它接回一份合法历史。那才是这条验收的真判据。
//! 真端点上的表现另由 `SHANIU_EVAL_TOKEN_BUDGET` 驱动的评估基线覆盖（见 evals_baseline.rs）。

use shaniu_lib::compaction;
use shaniu_lib::model_client::{HistoryItem, Role};
use shaniu_lib::rollout::{self, ConfigSnapshot, RolloutItem, RolloutWriter};
use std::path::PathBuf;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("shaniu-compaction-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
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
        writer.append(&RolloutItem::ResponseItem { item: item.clone() }).unwrap();
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
    std::fs::remove_dir_all(&dir).ok();
}

/// 摘要的角色必须是 assistant：塞进 system 会与 04 §6.1 不变量③（系统提示每轮重算）打架。
#[test]
fn summary_lands_as_an_assistant_message_not_a_system_one() {
    let dir = temp_dir("role");
    let items = vec![user_msg("任务"), big_output("c0", 40_000), big_output("c1", 40_000)];
    let writer = RolloutWriter::create(&dir, &meta("a2")).unwrap();
    for item in &items {
        writer.append(&RolloutItem::ResponseItem { item: item.clone() }).unwrap();
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
        HistoryItem::Message { role, .. } => assert_eq!(*role, Role::Assistant),
        other => panic!("摘要必须是 message，得到 {other:?}"),
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// 区间越界必须报错而不是静默吞掉：静默会让一份坏档案伪装成"压缩成功"。
#[test]
fn out_of_range_target_is_rejected_by_resume() {
    let dir = temp_dir("oor");
    let writer = RolloutWriter::create(&dir, &meta("a3")).unwrap();
    writer
        .append(&RolloutItem::ResponseItem { item: user_msg("只有一条") })
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
    std::fs::remove_dir_all(&dir).ok();
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
