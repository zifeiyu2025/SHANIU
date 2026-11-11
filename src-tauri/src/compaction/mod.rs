//! token 预算判定 + 历史摘要压缩（SH-032）。
//!
//! 判据与形态取自 codex 的**本地实现**那条路（`core/src/compact.rs`），但锚点语义
//! 必须服从我们自己的文档：04 §3.1 规定 `Compacted` 存 `{range_start, range_end, summary}`
//! 这个**区间**形态，SH-031 的回放也已经建立在「用 summary 替换 history 的对应区间」上。
//! codex 那边存的是替换后的完整历史（`replacement_history`），照抄会把已交付的
//! resume/fork 语义拆掉——所以这里刻意不抄。
//!
//! 本模块刻意**不**依赖 `agent_loop`：它只做纯计算（估算、判据、区间、请求构造、
//! 摘要提取），发请求与写档案都留在循环层。否则 `agent_loop → compaction → agent_loop`
//! 会成环，而 §2.3.1 里也没有这条反向边。

use crate::model_client::{HistoryItem, Role};

/// 4 字节 ≈ 1 token 的粗估口径，与 codex `utils/string/src/truncate.rs:6` 同参数。
///
/// 没有引真实 tokenizer（codex 全仓也没有：`tiktoken`/`cl100k`/`o200k` 零命中）。
/// 预算判据要的是"该收口了没有"这个量级，不是精确计费；为它背一个几百 KB 的词表
/// 与逐模型词表选择，换来的精度用不上。
const APPROX_BYTES_PER_TOKEN: usize = 4;

/// 压缩后**保留**的额度占预算的比例。
///
/// 取 30% 而不是更高：压缩完成后历史只占预算三成，剩下七成留给新一轮工具输出。
/// 留得太满会让下一轮立刻再次触发压缩，等于把一次摘要的代价反复付。
/// 这条是设计选择，04 §6.4 只规定了摘要必含字段、没规定保留比例。
const KEEP_RATIO_PERCENT: usize = 30;

/// 模型行未声明 `context_window` 时用于推导压缩阈值的保守窗口。
///
/// 取 128K 而不是乐观值：窗口未知时宁可早压（多花一次摘要），也不能沿用 50 万预算——
/// 那等于永不压缩，128K 模型跑到一半直接 400，代价比多压几次大得多。
pub const FALLBACK_CONTEXT_WINDOW: usize = 128_000;

/// 单个上下文窗口允许占用的量：窗口 × 80%，留 20% 给下一轮回复与工具结果。
fn window_allowance(window: usize) -> usize {
    let window = if window == 0 { FALLBACK_CONTEXT_WINDOW } else { window };
    window.saturating_mul(4) / 5
}

/// 自动压缩阈值：`min(配置预算, 窗口 × 80%)`。
///
/// - `configured == 0`：显式关闭压缩，保持 0（不替用户改主意）；
/// - `window == 0`（模型未声明）：按 [`FALLBACK_CONTEXT_WINDOW`] 推导，调用方负责告警；
/// - 其余：留 20% 给下一轮回复与工具结果。
///
/// 回合内自动压缩与手动 `/compact` **必须**共用这一份口径（见 [`manual_token_budget`]）：
/// 两处各算一遍时，手动路径拿到的是未经窗口裁剪的配置值，会出现「压完仍然超窗口」。
pub fn compact_token_budget(configured: usize, window: usize) -> usize {
    if configured == 0 {
        return 0;
    }
    configured.min(window_allowance(window))
}

/// 手动压缩阈值：用户显式要求压缩时，`configured == 0` 不能解释成「关掉」。
///
/// 用 0 直接进 [`plan_manual`] 会算出「保留 0 条尾部」，把历史压到只剩最后一条——
/// 那不是任何人的预期。这里的 0 表示「不设配置上限」，于是按窗口算：与自动路径
/// 同一个窗口口径，只是不因为「关掉自动压缩」而拒绝用户显式要求的这一次。
pub fn manual_token_budget(configured: usize, window: usize) -> usize {
    if configured == 0 {
        return window_allowance(window);
    }
    configured.min(window_allowance(window))
}

/// 摘要提示词。五个必含字段逐条来自 04 §6.4：
/// 任务目标 / 已完成事项 / 关键文件与改动 / 未解决问题 / 下一步。
///
/// 少了任何一条，接手的模型就会去重做已经做过的事，或者忘掉用户已经给过的约束——
/// 那正是"压缩丢失细节"（01 §3 ADR-001 风险表）的实际发生方式。
pub const SUMMARIZATION_PROMPT: &str = "请把上面这段对话与工具执行历史压缩成一份交接摘要，交给另一个模型继续这个任务。\
摘要必须依次包含以下五项，每项一行到数行：\n\
1. 任务目标：用户最初要什么，以及过程中已经变更过的约束。\n\
2. 已完成事项：确实做完并有结果可查的动作。\n\
3. 关键文件与改动：被读过、被改过的文件路径与改动要点。\n\
4. 未解决问题：失败过、被拒绝过、或结果与预期不符的事项。\n\
5. 下一步：接手后应该立刻做的事。\n\
只输出这五项内容本身，不要前言、不要解释你做了压缩。";

/// 一次压缩的区间：`[range_start, range_end)` 的历史将被 summary 取代。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub range_start: usize,
    pub range_end: usize,
}

/// 一次压缩的唯一标识：毫秒时间戳 + 进程内序号。
///
/// 用途只是把 `compaction_started / summarized / ended` 三条事件配成一组（含日志回放）。
/// 不引 ULID：进程内序号 + 单调时间戳足以保证同一会话的事件流不撞号，而这个 id
/// 不参与任何持久化键或安全判定。
pub fn new_id() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!(
        "cmp-{now}-{}",
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

pub fn approx_tokens(text: &str) -> usize {
    text.len().div_ceil(APPROX_BYTES_PER_TOKEN)
}

/// 一条条目的模型可见量。只算内容字节——id、外层 JSON 转义与传输元数据都不进上下文，
/// 把它们算进去会让预算系统性偏紧，表现为"明明还装得下却开始压缩"。
pub fn item_tokens(item: &HistoryItem) -> usize {
    match item {
        HistoryItem::Message { text, reasoning, .. } => {
            approx_tokens(text) + reasoning.as_deref().map(approx_tokens).unwrap_or_default()
        }
        HistoryItem::FunctionCall { name, arguments, .. } => {
            approx_tokens(name) + approx_tokens(arguments)
        }
        HistoryItem::FunctionCallOutput { output, .. } => approx_tokens(output),
        // 图片字节不在历史里，按 view_image 记下的分辨率估算值计入预算。
        HistoryItem::ImageMessage { text, images, .. } => {
            approx_tokens(text) + images.iter().map(|image| image.tokens as usize).sum::<usize>()
        }
    }
}

pub fn history_tokens(items: &[HistoryItem]) -> usize {
    items.iter().map(item_tokens).sum()
}

/// 从尾部往前装，算出「保留哪些」的切点：`[0, range_end)` 是要压掉的区间。
///
/// 三条不变量是这层抽象**唯一**的存在理由，改任何一条都会让某条路径的行为变坏：
/// 1. **严格大于**：判据是 `kept + cost > keep_allowance`，保留额度**刚好**装得下一条时
///    必须留下它。写成 `>=` 会在边界上少留一条，压得比配置口径更狠。
/// 2. **至少留一条**：`split >= len` 时钳到 `len - 1`。全压掉会让下一轮看到的历史只剩
///    一段摘要，模型连"刚才那条命令的输出是什么"都无从得知。
/// 3. **钳到 1**：`range_end` 至少为 1。判据已经说超预算、而按字节估算整段都装得进
///    保留额度时（观测值含推理 token 与工具声明开销，字节估算看不见），退化成压掉
///    最旧的一条让每轮稳定回收一点，而不是返回 `[0,0)` 空转。
///
/// 调用方各自的门槛（预算是否为 0、条数是否够、阈值是否达到、`keep_allowance` 怎么算）
/// **不在这里**：自动与手动两条路径的门槛本就不同，合进来就得加参数分叉，得不偿失。
/// 前置条件：`items.len() >= 2`，由调用方保证。
fn tail_target(items: &[HistoryItem], keep_allowance: usize) -> Target {
    let mut kept = 0usize;
    let mut split = items.len();
    for index in (0..items.len()).rev() {
        let cost = item_tokens(&items[index]);
        // 不变量①：严格大于——刚好装得下的那条必须留下。
        if kept + cost > keep_allowance {
            split = index + 1;
            break;
        }
        kept += cost;
        split = index;
    }
    // 不变量②：至少留最后一条不压，哪怕它自己就超了保留额度。
    if split >= items.len() {
        split = items.len() - 1;
    }
    // 不变量③：绝不空压。
    Target { range_start: 0, range_end: split.max(1) }
}

/// 是否需要压缩，以及压哪一段。
///
/// `observed_tokens` 是上一次采样端点报回的真实用量（含推理 token 与工具声明开销，
/// 这两项字节估算都看不见）。判据取 `max(观测值, 全量估算)`：
/// 观测值可能落后于本轮刚追加的大块工具输出，估算值又系统性低估协议开销——
/// 两者各有盲区，而**低估的代价是撞上上下文溢出**（请求直接失败），
/// 高估的代价只是提前压一次。所以取较大值。
pub fn plan_if_needed(observed_tokens: usize, items: &[HistoryItem], budget: usize) -> Option<Target> {
    if budget == 0 {
        return None;
    }
    if items.len() < 2 {
        // 少于两条没什么可压：把唯一一条压成摘要，等于把任务目标本身交给一次
        // 可能失败的总结请求去保管。
        return None;
    }
    let estimate = history_tokens(items);
    if observed_tokens.max(estimate) < budget {
        return None;
    }

    // 从尾部往前装，装不下的那些进摘要。三条不变量见 [`tail_target`]。
    Some(tail_target(items, budget.saturating_mul(KEEP_RATIO_PERCENT) / 100))
}

/// 手动压缩（`/compact`，SH-115）的目标规划：与 [`plan_if_needed`] 同一条尾部保留
/// 算法，但**不看预算阈值**——用户显式要求压缩就是要现在压，哪怕当前用量还低。
/// 预算仍决定保留多少（budget=0 时只留最后一条：预算关闭说明用户不想要自动压缩，
/// 但手动命令的语义不变）。
///
/// 与 [`plan_if_needed`] 的另两点差异：
/// - 少于两条返回 `None`（没有可压的，调用方回 "No compactable history yet."）；
///   不像自动路径那样退化压最旧一条——唯一一条就是任务本身，压掉它没有意义。
/// - 全部装进保留额度时（split==0）钳到 1：手动命令既然发出了，至少压掉最旧一条，
///   与 [`plan_if_needed`] 的退化口径一致，而不是空转返回"没压任何东西"。
pub fn plan_manual(items: &[HistoryItem], budget: usize) -> Option<Target> {
    if items.len() < 2 {
        return None;
    }
    let keep_allowance = if budget == 0 {
        0
    } else {
        budget.saturating_mul(KEEP_RATIO_PERCENT) / 100
    };
    Some(tail_target(items, keep_allowance))
}

/// 构造总结请求：被压区间的全部条目 + 末尾一条总结指令。
///
/// 只送被压区间而不是整段历史：保留段马上还要原样用，把它再喂一遍只会让这次
/// 总结请求本身去撞预算。
pub fn build_request(items: &[HistoryItem], target: &Target) -> Vec<HistoryItem> {
    let mut request = items[target.range_start..target.range_end].to_vec();
    request.push(HistoryItem::Message {
        role: Role::User,
        text: SUMMARIZATION_PROMPT.to_string(),
        reasoning: None,
    });
    request
}

/// 从总结响应里取摘要。空摘要必须报出来而不是当成成功：
/// 拿空串写锚点，回放时那段历史就被替换成"什么都没有"，是静默丢上下文。
pub fn extract_summary(reply_text: &str) -> Result<String, String> {
    let trimmed = reply_text.trim();
    if trimmed.is_empty() {
        return Err("压缩失败：总结请求没有返回可用摘要".to_string());
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: Role, text: &str) -> HistoryItem {
        HistoryItem::Message { role, text: text.to_string(), reasoning: None }
    }

    fn call(id: &str, args: &str) -> HistoryItem {
        HistoryItem::FunctionCall {
            name: "unified_exec".into(),
            arguments: args.into(),
            call_id: id.into(),
        }
    }

    fn out(id: &str, output: &str) -> HistoryItem {
        HistoryItem::FunctionCallOutput { call_id: id.into(), output: output.into() }
    }

    /// 压缩阈值必须跟着窗口走：`context_window = 0` 曾被短路成"照用 50 万预算"，
    /// 128K 模型跑满也不会压缩，等端点 400 才暴露（2026-11-10 修复）。
    #[test]
    fn compact_budget_follows_window_and_never_uses_configured_limit_blindly() {
        // 已知窗口：80% 封顶，配置更小时以配置为准
        assert_eq!(compact_token_budget(500_000, 128_000), 102_400);
        assert_eq!(compact_token_budget(50_000, 128_000), 50_000);
        // 未知窗口：按保守窗口推导，而不是退回到 50 万
        assert_eq!(compact_token_budget(500_000, 0), 102_400);
        assert!(compact_token_budget(500_000, 0) < 500_000);
        // 显式关闭压缩仍然是关闭
        assert_eq!(compact_token_budget(0, 128_000), 0);
        assert_eq!(compact_token_budget(0, 0), 0);
    }

    /// I1 / P3 反向对照：手动压缩不能再拿未经窗口裁剪的配置值。
    /// 撤掉修复（`manual_token_budget` 直接返回 `configured`）时第一条断言变红——
    /// 500K 配置 + 128K 窗口会保留 15 万 token 尾巴，压完仍然超窗口。
    #[test]
    fn manual_budget_is_clamped_by_window_and_survives_disabled_auto_compaction() {
        assert_eq!(manual_token_budget(500_000, 128_000), 102_400);
        // `token_budget = 0` 只关自动压缩；用户显式 `/compact` 按窗口算，
        // 不能算出「保留 0 条尾部」把历史压到只剩最后一条。
        assert_eq!(manual_token_budget(0, 128_000), 102_400);
        assert_eq!(manual_token_budget(0, 0), 102_400);
        // 配置小于窗口时仍以配置为准
        assert_eq!(manual_token_budget(50_000, 128_000), 50_000);
    }

    #[test]
    fn token_estimate_is_ceiling_division_of_bytes() {
        assert_eq!(approx_tokens(""), 0);
        assert_eq!(approx_tokens("abcd"), 1);
        assert_eq!(approx_tokens("abcde"), 2);
        // 中文按字节算：UTF-8 下 3 字节/字，4 字 ≈ 3 token。粗估口径的已知偏差，
        // 与 codex 同一取舍，这里钉住行为而不是钉住"看起来合理"。
        assert_eq!(approx_tokens("四汉字"), 3);
    }

    #[test]
    fn item_estimate_counts_content_but_not_transport_metadata() {
        // call_id 是传输元数据，不该进预算
        let short = call("c1", "{\"a\":1}");
        let long_id = call("a-very-long-call-id-value-that-grows", "{\"a\":1}");
        assert_eq!(item_tokens(&short), item_tokens(&long_id));
        // reasoning 是模型可见内容，必须计入
        let with_reasoning = HistoryItem::Message {
            role: Role::Assistant,
            text: "答".into(),
            reasoning: Some("很长的思考过程".into()),
        };
        assert!(item_tokens(&with_reasoning) > item_tokens(&msg(Role::Assistant, "答")));
    }

    #[test]
    fn no_compaction_below_budget() {
        let items = vec![msg(Role::User, "小任务"), msg(Role::Assistant, "小回复")];
        assert_eq!(plan_if_needed(0, &items, 500_000), None);
    }

    #[test]
    fn observed_usage_alone_can_trigger_compaction_even_when_estimate_is_low() {
        // 端点报了 600k token（含推理与工具声明开销），字节估算看不到那部分
        let items = vec![msg(Role::User, "短"), msg(Role::Assistant, "短")];
        // 估算装得下保留额度、但观测值已顶到预算：不能空压成 [0,0)，必须至少压掉最旧一条。
        let target = plan_if_needed(600_000, &items, 500_000).expect("观测值超预算必须给出区间");
        assert_eq!(
            target,
            Target { range_start: 0, range_end: 1 },
            "退化时必须压掉最旧一条，否则这一轮回收为零、下一轮继续溢出"
        );
    }

    /// 保留额度**刚好**装得下一条时必须装下：判据是严格大于。
    ///
    /// 反向对照：把 `kept + cost > keep_allowance` 写成 `>=`，最后两条会被挤掉一条，
    /// 区间从 `[0,2)` 变成 `[0,3)`，本用例立刻红。自动/手动两条路径共用同一段装填，
    /// 所以两边都钉一遍。
    #[test]
    fn keep_allowance_boundary_keeps_the_item_that_fits_exactly() {
        // 每条 60_000 字节 ≈ 15_000 token（4 字节/token）；budget 100_000 → 额度 30_000
        let big = "x".repeat(60_000);
        let items: Vec<HistoryItem> = (0..4).map(|_| msg(Role::User, &big)).collect();
        assert_eq!(item_tokens(&items[0]), 15_000, "构造前提：每条恰好 15_000 token");

        let manual = plan_manual(&items, 100_000).expect("手动压缩总要给出目标");
        assert_eq!(
            manual,
            Target { range_start: 0, range_end: 2 },
            "额度刚好装下最后两条时，第二条必须留下"
        );

        let auto = plan_if_needed(100_000, &items, 100_000).expect("观测值已顶到预算");
        assert_eq!(
            auto,
            Target { range_start: 0, range_end: 2 },
            "自动路径与手动路径必须同一口径"
        );
    }

    #[test]
    fn large_tool_output_triggers_compaction_without_server_report() {
        // 本轮刚追加的大块输出：观测值还没更新，估算必须能看见
        let items = vec![
            msg(Role::User, "任务"),
            call("c1", "{}"),
            out("c1", &"x".repeat(400_000)),
            msg(Role::Assistant, "结论"),
        ];
        let target = plan_if_needed(0, &items, 50_000).expect("400k 字节的输出必须触发压缩");
        assert_eq!(target.range_start, 0);
        assert!(target.range_end < items.len(), "至少留最后一条不压");
    }

    #[test]
    fn always_keeps_the_newest_item() {
        // 保留额度极小：即使最后一条自己就超额度，也不能把它压掉
        let items = vec![msg(Role::User, "任务"), out("c1", &"y".repeat(100_000))];
        let target = plan_if_needed(0, &items, 1_000).expect("超预算必须给出区间");
        assert_eq!(target.range_end, 1, "只压第一条，最新那条必须留下");
    }

    #[test]
    fn never_compresses_a_single_item_history() {
        let items = vec![msg(Role::User, "唯一的一条超长任务")];
        assert_eq!(plan_if_needed(9_999_999, &items, 100), None);
    }

    #[test]
    fn zero_budget_disables_compaction() {
        // token_budget = 0 表示"不启用压缩"，而不是"每次都压到空"
        let items = vec![msg(Role::User, "a"), msg(Role::Assistant, "b")];
        assert_eq!(plan_if_needed(1_000_000, &items, 0), None);
    }

    #[test]
    fn request_sends_only_the_compressed_range_plus_prompt() {
        let items = vec![
            msg(Role::User, "第一条"),
            msg(Role::Assistant, "第二条"),
            msg(Role::Assistant, "第三条要保留"),
        ];
        let target = Target { range_start: 0, range_end: 2 };
        let request = build_request(&items, &target);
        assert_eq!(request.len(), 3, "两条被压内容 + 一条总结指令");
        match request.last().unwrap() {
            HistoryItem::Message { role, text, .. } => {
                assert_eq!(*role, Role::User, "总结指令是合成的 user 消息");
                assert_eq!(text, SUMMARIZATION_PROMPT);
            }
            other => panic!("期望 user 消息，得到 {other:?}"),
        }
        // 保留段不该被再喂一遍
        assert!(!request.iter().any(|i| matches!(i, HistoryItem::Message { text, .. } if text == "第三条要保留")));
    }

    #[test]
    fn summary_prompt_carries_every_field_required_by_04_6_4() {
        // 04 §6.4 的五个必含字段少一个，接手的模型就可能重做已完成的事
        for field in ["任务目标", "已完成事项", "关键文件与改动", "未解决问题", "下一步"] {
            assert!(
                SUMMARIZATION_PROMPT.contains(field),
                "摘要提示词缺少 04 §6.4 要求的字段：{field}"
            );
        }
    }

    #[test]
    fn empty_summary_is_an_error_not_a_success() {
        // 空摘要写进锚点 = 那段历史被替换成"什么都没有"，是静默丢上下文
        assert!(extract_summary("   \n ").is_err());
        assert_eq!(extract_summary(" 摘要正文 ").unwrap(), "摘要正文");
    }

    // ============================== plan_manual（SH-115）==============================

    #[test]
    fn manual_plan_compacts_even_under_budget() {
        // 自动判据按预算说不压（plan_if_needed → None）；手动命令仍要给出目标
        let items = vec![msg(Role::User, "第一条"), msg(Role::Assistant, "第二条")];
        assert_eq!(plan_if_needed(0, &items, 1_000_000), None);
        let target = plan_manual(&items, 1_000_000).expect("手动压缩不受阈值拦截");
        // 保留段装得下全部 → 钳到"至少留最后一条"
        assert_eq!(target, Target { range_start: 0, range_end: 1 });
    }

    #[test]
    fn manual_plan_without_budget_keeps_only_the_last_item() {
        let items = vec![
            msg(Role::User, "一"),
            msg(Role::User, "二"),
            msg(Role::User, "三"),
        ];
        let target = plan_manual(&items, 0).expect("budget=0 只影响保留额度，不否决手动压缩");
        assert_eq!(target, Target { range_start: 0, range_end: 2 });
    }

    #[test]
    fn manual_plan_respects_keep_allowance_when_budget_is_tight() {
        // 每条约 25_000 token（10 万字节）；budget 40_000 → 保留额度 12_000，
        // 一条都装不下 → split 落在第 0 次超限处，但钳到至少留最后一条
        let big = "x".repeat(100_000);
        let items: Vec<HistoryItem> = (0..4).map(|_| msg(Role::User, &big)).collect();
        let target = plan_manual(&items, 40_000).expect("存在可压区间");
        assert_eq!(target, Target { range_start: 0, range_end: 3 });
    }

    #[test]
    fn manual_plan_needs_at_least_two_items() {
        assert_eq!(plan_manual(&[msg(Role::User, "唯一一条")], 0), None);
        assert_eq!(plan_manual(&[], 0), None);
    }
}
