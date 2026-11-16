//! 评测夹具：切点/锚点的「配对」口径。
//!
//! 覆盖四条路径，任何一条没修都会红：
//! 1. 自动压缩 `plan_if_needed`：切点不许把 function_call 和它的 output 拆到两侧；
//! 2. 手动压缩 `plan_manual`：同一条算法，两个入口都要修；
//! 3. 分叉 `nearest_safe_cut`：不能落在一次调用的中间；
//! 4. 搜索索引 `build_documents`：连续两个锚点后，序号口径仍与重放一致。

use shaniu_lib::compaction::{plan_if_needed, plan_manual, Target};
use shaniu_lib::model_client::{HistoryItem, Role};
use shaniu_lib::rollout::resume::nearest_safe_cut;
use shaniu_lib::rollout::RolloutItem;
use shaniu_lib::session_query::documents::build_documents;

fn msg(text: &str) -> HistoryItem {
    HistoryItem::Message { role: Role::User, text: text.into(), reasoning: None }
}

fn big_msg() -> HistoryItem {
    msg(&"m".repeat(400))
}

fn call(id: &str) -> HistoryItem {
    HistoryItem::FunctionCall { name: "run".into(), arguments: "a".repeat(400), call_id: id.into() }
}

fn output(id: &str) -> HistoryItem {
    HistoryItem::FunctionCallOutput { call_id: id.into(), output: "o".repeat(400) }
}

fn archive(item: HistoryItem) -> RolloutItem {
    RolloutItem::ResponseItem { item, at_ms: 1 }
}

/// 保留额度 300 tokens：朴素切法是 3，会把 index 2 的 call 压掉、留下 index 3 的 output。
#[test]
fn manual_compaction_does_not_split_a_call_from_its_output() {
    let items = vec![big_msg(), big_msg(), call("c1"), output("c1"), big_msg(), big_msg()];
    let target = plan_manual(&items, 1000).expect("应该可以压缩");
    assert_eq!(
        target,
        Target { range_start: 0, range_end: 2 },
        "压缩区间把 call 和 output 拆开了：{target:?}"
    );
}

/// 同一条算法在自动路径上也要成立——只改 plan_manual 会在这里红。
#[test]
fn automatic_compaction_does_not_split_a_call_from_its_output() {
    let items = vec![big_msg(), big_msg(), call("c1"), output("c1"), big_msg(), big_msg()];
    let target = plan_if_needed(50_000, &items, 1000).expect("超过预算，应该压缩");
    assert_eq!(
        target,
        Target { range_start: 0, range_end: 2 },
        "自动压缩把 call 和 output 拆开了：{target:?}"
    );
}

/// 配对起点就在 0：往回缩会变成空压缩，只能把整对一起压掉。
#[test]
fn a_pair_at_the_start_is_compacted_whole_instead_of_left_empty() {
    let items = vec![call("c2"), output("c2"), big_msg(), big_msg()];
    let target = plan_if_needed(50_000, &items, 1000).expect("超过预算，应该压缩");
    assert_eq!(
        target,
        Target { range_start: 0, range_end: 2 },
        "配对从 0 开始时必须整对压掉，而不是返回空区间：{target:?}"
    );
}

/// 切点本来就落在两条完整配对之间时，不许借机多压。
#[test]
fn a_clean_boundary_is_left_where_the_budget_put_it() {
    let items = vec![big_msg(), big_msg(), call("c3"), output("c3"), big_msg(), big_msg()];
    let target = plan_manual(&items, 667).expect("应该可以压缩");
    assert_eq!(
        target,
        Target { range_start: 0, range_end: 4 },
        "干净切点被改动了：{target:?}"
    );
}

/// 分叉：want=2 会留下一条有 call 无 output 的尾巴，必须回退到 1。
#[test]
fn fork_cut_never_lands_inside_a_tool_call() {
    let items = vec![archive(big_msg()), archive(call("c4")), archive(output("c4")), archive(big_msg())];
    assert_eq!(nearest_safe_cut(&items, 2), 1, "分叉切点落在调用中间");
}

/// 连续两个锚点：第二个锚点压的是第一段摘要，后面的调用/输出必须仍是 current。
#[test]
fn consecutive_compactions_keep_the_index_numbering_aligned() {
    let items = vec![
        archive(msg("alpha")),
        archive(call("c5")),
        archive(output("c5")),
        RolloutItem::Compacted { range_start: 0, range_end: 1, summary: "s1".into() },
        archive(msg("beta")),
        archive(msg("gamma")),
        RolloutItem::Compacted { range_start: 0, range_end: 1, summary: "s2".into() },
    ];
    let docs = build_documents(&items, 1_700_000_000_000);
    let surface = |seq: i64| {
        docs.iter()
            .find(|doc| doc.seq == seq)
            .unwrap_or_else(|| panic!("缺少 seq={seq} 的文档"))
            .surface
    };
    assert_eq!(surface(0), "shadowed", "alpha 已被第一次压缩取代");
    assert_eq!(surface(2), "current", "call 与 output 都没被任何锚点压掉，不该被标成 shadowed");
    assert_eq!(surface(3), "current", "output 与 call 成对保留，不该错位");
    assert_eq!(surface(8), "current", "beta 在两个锚点之后仍然可见");
}
