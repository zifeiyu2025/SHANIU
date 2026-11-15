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
    let now = crate::clock::now_ms();
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
        // 密文确实占输入 token（下一轮原样回放），不计数会让预算系统性低估、
        // 压缩比实际需要更晚触发。摘要文本也随请求发出，一并计入。
        HistoryItem::Reasoning { summary, encrypted_content, .. } => {
            summary.iter().map(|part| approx_tokens(&part.text)).sum::<usize>()
                + encrypted_content.as_deref().map(approx_tokens).unwrap_or_default()
        }
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
/// 四条不变量是这层抽象**唯一**的存在理由，改任何一条都会让某条路径的行为变坏：
/// 1. **严格大于**：判据是 `kept + cost > keep_allowance`，保留额度**刚好**装得下一条时
///    必须留下它。写成 `>=` 会在边界上少留一条，压得比配置口径更狠。
/// 2. **至少留一条**：`split >= len` 时钳到 `len - 1`。全压掉会让下一轮看到的历史只剩
///    一段摘要，模型连"刚才那条命令的输出是什么"都无从得知。
/// 3. **不拆工具调用组**：切点回退到被保留输出所对应的调用下标（见 [`cut_keeps_pairs`]）。
///    切在组中间会让保留段以一条"调用已被摘要取代"的 `tool_call_id` 开头，端点直接
///    500 `Cannot resolve tool name for tool_call_id=...`，而这个错误重试多少次都
///    不会好——整个会话从此报废（2026-11-14 真机冒烟 N1）。代价是保留段可能比
///    `keep_allowance` 多出半个组的 token：多留一点只多花上下文，拆散配对则让会话
///    永久不可用，两者不对称。判据按 `call_id` 实配，整段无调用的孤立输出不触发回退。
/// 4. **钳到 1**：`range_end` 至少为 1。判据已经说超预算、而按字节估算整段都装得进
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
    // 不变量③：切点不能落在工具调用组中间（调用被压掉、输出还留在保留段）。
    // 不变量④：推理项必须和它后面的消息一起留下——"有消息没推理"无害，
    // "推理被压掉、消息还在"会让严格端点在续写时对不上号（D2）。
    let split = cut_keeps_reasoning(items, cut_keeps_pairs(items, split));
    // 不变量⑤：绝不空压。
    Target { range_start: 0, range_end: split.max(1) }
}

/// 把切点向前回退，直到丢弃段不再以「推理项」结尾（推理与它附着的消息被拆开）。
///
/// 规则只在这里实现：切点的另一处来源是手动压缩，它同样走 [`tail_target`]；
/// 在下游补一遍会让两处口径漂移。多枚连续推理项时一并回退到整段之前。
fn cut_keeps_reasoning(items: &[HistoryItem], mut split: usize) -> usize {
    if !matches!(items.get(split), Some(HistoryItem::Message { .. })) {
        return split;
    }
    while split > 0 && matches!(items.get(split - 1), Some(HistoryItem::Reasoning { .. })) {
        split -= 1;
    }
    split
}

/// 把切点向前回退，直到丢弃段里不再有"输出仍留在保留段"的调用。
///
/// 判据必须按 `call_id` 的实际配对算，而不是按条目类型：历史里可能存在**整段都没有
/// 调用**的孤立输出（旧版本档案、外部裁剪）。那种输出本来就不指向任何被丢弃的调用，
/// 切在它旁边不会拆散配对——降级成文本由 `model_client` 兜住；按类型一刀切会把这类
/// 历史的切点一路退到 0，连压缩都不做了（那是另一个方向的坏）。
///
/// 组内顺序固定为「先连续 `FunctionCall`、再连续 `FunctionCallOutput`」，所以回退
/// 只会发生两种形态：切在输出上（它的调用在丢弃段里）、切在一组多个调用的中间
/// （前半个调用的输出还在保留段里）。回退到调用的下标让整组留在保留段：保留段可能
/// 比 `keep_allowance` 长一点，但配对完整。旧实现只按 token 切，真机上就是这里把
/// `call_0e6f3061…` 的调用压掉、输出留下，端点 500 后会话永久不可用。
fn cut_keeps_pairs(items: &[HistoryItem], mut split: usize) -> usize {
    loop {
        // 丢弃段里的调用：id → 下标（同 id 重复出现时取最靠前的那条）。
        let mut dropped: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for (index, item) in items[..split].iter().enumerate() {
            if let HistoryItem::FunctionCall { call_id, .. } = item {
                dropped.entry(call_id.as_str()).or_insert(index);
            }
        }
        // 保留段里只要有输出指向被丢弃的调用，切点就必须退到那条调用之前。
        let mut moved = None;
        for item in &items[split..] {
            if let HistoryItem::FunctionCallOutput { call_id, .. } = item {
                if let Some(&index) = dropped.get(call_id.as_str()) {
                    moved = Some(moved.map_or(index, |current: usize| current.min(index)));
                }
            }
        }
        // 回退后新并入保留段的条目可能再引出更早的调用，循环到稳定为止。
        match moved {
            Some(index) if index < split => split = index,
            _ => return split,
        }
    }
}

/// 这一条是否是**本模块此前生成的摘要**（以 user 身份写回历史）。
///
/// 识别依据只有 [`compose_summary`] 写出的固定两段式（标题 + 分隔线），
/// 结构上无法与真实的用户发言混淆；旧版本无标题的裸摘要不在识别范围内。
///
/// **口径：只保证新数据不再产生，不追溯剥离历史档案里的裸摘要。** 裸摘要没有结构
/// 标记，按内容（如「压缩对象为空集」）猜测识别既把语言细节焊进判据，又可能误删
/// 真实用户原文；重放/再压缩时它按普通用户消息处理，代价是旧档案多背一段文字。
fn is_generated_summary(item: &HistoryItem) -> bool {
    matches!(
        item,
        HistoryItem::Message { role: Role::User, text, .. } if carried_user_text(text).is_some()
    )
}

/// 区间里是否有真实对话内容可供总结。
///
/// **系统注入**（环境快照等，`Role::System`）与**旧摘要**都不算内容：
/// [`tail_target`] 的不变量④ 会把区间钳到 `[0,1)`，若最旧一条正好是系统注入，
/// 送进总结请求的就只有一段环境描述，模型只能回「压缩对象为空集」。这条空摘要
/// 随后以 user 身份写回历史，下一轮就被当成用户原文逐字继承（2026-11-15 真机 N9：
/// 第 2、3 次摘要依次把前一次摘要正文当原文保留）。
///
/// 跳过它没有副作用：尾部继续累积，等真的出现可压缩内容时再压。
fn has_real_content(items: &[HistoryItem], target: &Target) -> bool {
    items[target.range_start..target.range_end]
        .iter()
        .any(|item| match item {
            // 系统注入不是对话内容：压掉它只会得到一段「本轮未收到具体用户任务」。
            HistoryItem::Message { role: Role::System, .. }
            | HistoryItem::ImageMessage { role: Role::System, .. } => false,
            // 旧摘要也不是：再压一层只会把摘要换一层包装（N8）。
            item if is_generated_summary(item) => false,
            HistoryItem::Message { text, .. } => !text.trim().is_empty(),
            HistoryItem::ImageMessage { text, images, .. } => {
                !text.trim().is_empty() || !images.is_empty()
            }
            HistoryItem::FunctionCall { .. } | HistoryItem::FunctionCallOutput { .. } => true,
            // 推理项本身不是可总结的对话内容；它附着在紧接着的助手消息上，
            // 那条消息会决定这段区间是否值得压。
            HistoryItem::Reasoning { .. } => false,
        })
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

    // 从尾部往前装，装不下的那些进摘要。不变量见 [`tail_target`]。
    let target = tail_target(items, budget.saturating_mul(KEEP_RATIO_PERCENT) / 100);
    if !has_real_content(items, &target) {
        // 区间里只有系统注入或上一条摘要：压了要么得到「压缩对象为空集」，
        // 要么只是把摘要换一层包装；等尾部攒够真实内容再压。
        return None;
    }
    Some(target)
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

/// 压缩时原样保留的用户原文段落占预算的比例。
///
/// 只压不保等于把用户给过的硬约束交给一次摘要请求去保管：摘要漏抄一条，
/// 接手模型就再也看不到原始要求。取 30% 与保留额度同档，避免反过来挤掉摘要。
const USER_TEXT_KEEP_PERCENT: usize = 30;

/// 用户原文段落的标题行。单独成段放在摘要**之前**，便于人与模型一眼区分
/// 「用户原话」与「模型写的交接摘要」。
const USER_TEXT_HEADER: &str = "## 用户原文（压缩时逐字保留，不得改写）";

/// 原文段与模型摘要之间的分隔线，由 [`compose_summary`] 写入。它是识别
/// 「这条 user 消息是本模块生成的摘要」的唯一依据，改动格式必须同步这里。
const SUMMARY_SEPARATOR: &str = "\n\n---\n\n";

/// 若 `text` 是本模块生成的摘要，返回其中逐字保留的原始用户段；否则 `None`。
///
/// 摘要以 **user** 身份写回历史（thinking 档要求 assistant 必带 reasoning，见
/// `agent_loop::replace_with_summary`），于是下一次压缩时它和真正的用户消息一样
/// 落在区间的 `Role::User` 条目里。不作区分，就会把上一条摘要整段当成「用户原文」
/// 再包一层标题：每压一次标题多一层、摘要越滚越大（2026-11-14 真机冒烟 N8：
/// 单回合 12 次压缩，摘要里标题嵌套 11 层，167 → 6412 字节）。
///
/// 形态是 [`compose_summary`] 写出的固定两段式：标题 + 原文段 + 分隔线 + 模型摘要。
/// 没有用户原文时原文段为空，但仍保留标题与分隔线，返回值是 `Some("")`——调用方
/// 据此把它认成「生成摘要、无可继承原文」，而不是退化成整段继承（N9）。
/// 旧版本写出的嵌套摘要（标题里再套标题）逐层剥掉，只留最内层原文；没有分隔线的
/// 正文（用户自己写的，或旧版本无标题的裸摘要）不改写，宁可当普通用户消息处理，也不猜。
fn carried_user_text(text: &str) -> Option<&str> {
    let mut rest = text.strip_prefix(USER_TEXT_HEADER)?;
    if !rest.contains(SUMMARY_SEPARATOR) {
        return None;
    }
    loop {
        // 本层原文只可能在第一条分隔线之前；之后的模型摘要不再逐字传递，
        // 由这次总结请求重新生成（`build_request` 仍会把旧摘要整段送给模型）。
        let segment = match rest.split_once(SUMMARY_SEPARATOR) {
            Some((before, _)) => before.trim_start_matches('\n'),
            None => rest.trim_start_matches('\n'),
        };
        match segment.strip_prefix(USER_TEXT_HEADER) {
            Some(inner) => rest = inner,
            None => return Some(segment.trim_end()),
        }
    }
}

/// 压缩前收集区间内的用户原文，拼在摘要之前，作为 `Compacted` 锚点 summary 的前半段。
///
/// 仍然只写一条字符串、只改 summary 内容，**不动档案结构**——旧档案里没有这段的
/// 摘要照常回放，resume 的重放公式也不需要知道这里多了一段。
///
/// 取舍规则（按预算从远到近裁）：
/// 1. **首条用户消息无条件保留**：它承载任务目标与硬约束，丢了等于丢任务本身；
/// 2. 其余从最新往回装，装不下的直接截断（更旧的同样不装），保证留下的是连续近段；
/// 3. 输出按时间顺序、每段原样引用，不做任何改写，便于逐字核对。
///
/// 区间里没有可继承的真实用户发言（空集摘要、纯系统注入）时**仍写出标题与分隔线**，
/// 只留一个空原文段：结构标记是下一轮唯一的身份依据，省略它就会退化成整段继承（N9）。
pub fn compose_summary(items: &[HistoryItem], target: &Target, budget: usize, summary: &str) -> String {
    let users: Vec<(usize, &str)> = items[target.range_start..target.range_end]
        .iter()
        .enumerate()
        .filter_map(|(offset, item)| match item {
            HistoryItem::Message { role: Role::User, text, .. } if !text.trim().is_empty() => {
                // 已经压缩过的区段只传递它逐字保留的原文段，不再把整份旧摘要
                // 当成新的「用户原文」嵌进来（否则每压一次就多包一层，N8）。
                // 原文段可能为空（空集摘要），空段没有信息，直接丢弃（N9）。
                let carried = carried_user_text(text).unwrap_or(text);
                (!carried.trim().is_empty()).then_some((target.range_start + offset, carried))
            }
            _ => None,
        })
        .collect();

    let allowance = budget.saturating_mul(USER_TEXT_KEEP_PERCENT) / 100;
    let mut picked: Vec<(usize, &str)> = Vec::new();
    // 规则 1：首条用户消息无条件保留；整个区间没有用户发言时 picked 保持空。
    if let Some(&(first_index, first_text)) = users.first() {
        picked.push((first_index, first_text));
    }
    let mut used = picked
        .first()
        .map(|(_, text)| approx_tokens(USER_TEXT_HEADER) + approx_tokens(text))
        .unwrap_or_default();
    for &(index, text) in users.iter().skip(1).rev() {
        let cost = approx_tokens(text);
        if used + cost > allowance {
            break;
        }
        used += cost;
        picked.push((index, text));
    }
    picked.sort_by_key(|(index, _)| *index);
    picked.dedup_by(|(_, later), (_, earlier)| later == earlier);

    let mut composed = String::with_capacity(used * APPROX_BYTES_PER_TOKEN + summary.len());
    composed.push_str(USER_TEXT_HEADER);
    for (_, text) in picked {
        composed.push_str("\n\n");
        composed.push_str(text);
    }
    composed.push_str(SUMMARY_SEPARATOR);
    composed.push_str(summary);
    composed
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

    /// 与 `agent_loop::replace_with_summary` 同语义：区间 → 一条 user 摘要。
    fn replace_with_summary(items: &mut Vec<HistoryItem>, target: Target, summary: String) {
        let mut next: Vec<HistoryItem> = items[..target.range_start].to_vec();
        next.push(msg(Role::User, &summary));
        next.extend(items[target.range_end..].iter().cloned());
        *items = next;
    }

    fn first_text(items: &[HistoryItem]) -> &str {
        match &items[0] {
            HistoryItem::Message { text, .. } => text,
            other => panic!("首条不是消息：{other:?}"),
        }
    }

    /// 保留段里每个工具输出都必须能找到同段的调用。
    fn assert_pairs_complete(items: &[HistoryItem]) {
        let mut calls = std::collections::HashSet::new();
        for item in items {
            match item {
                HistoryItem::FunctionCall { call_id, .. } => {
                    calls.insert(call_id.as_str());
                }
                HistoryItem::FunctionCallOutput { call_id, .. } => {
                    assert!(calls.contains(call_id.as_str()), "保留段出现孤立工具输出：{call_id}");
                }
                _ => {}
            }
        }
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

    /// 2026-11-14 真机冒烟 N1：切点落在「调用 → 输出」组中间时，保留段以孤立的
    /// `tool_call_id` 开头，端点 500 `Cannot resolve tool name for tool_call_id=...`，
    /// 重试多少次都不好，会话永久不可用。
    ///
    /// 反向对照：删掉 `tail_target` 里的 `cut_keeps_pairs`，这里会得到 `range_end = 3`
    /// （切在输出上），`assert_pairs_complete` 立刻红。
    #[test]
    fn cut_never_splits_a_tool_call_group() {
        let items = vec![
            msg(Role::User, "任务目标"),   // 3 token
            msg(Role::Assistant, "先读文件"), // 3
            call("call_0e6f3061a37b43cf96c3ef09", "{\"path\":\"package.json\"}"), // 9
            out("call_0e6f3061a37b43cf96c3ef09", &"x".repeat(100)), // 25
            msg(Role::User, "继续"),      // 2
        ];
        // 手动：预算 100 → 保留额度 30，旧算法只能装下 [输出, 继续]
        let manual = plan_manual(&items, 100).expect("有可压区间");
        assert_eq!(manual.range_end, 2, "切点必须回退到调用组起点，而不是落在输出上");
        assert_pairs_complete(&items[manual.range_end..]);

        // 自动路径共用同一段装填，口径必须一致
        let auto = plan_if_needed(100, &items, 100).expect("观测值已顶到预算");
        assert_eq!(auto, manual);
        assert_pairs_complete(&items[auto.range_end..]);
    }

    /// 第二种拆组形态：切在一组多个调用的中间。前半个调用被压掉，它的输出却还
    /// 留在保留段里——同样是孤立 `tool_call_id`。
    #[test]
    fn cut_inside_a_multi_call_group_moves_to_the_group_start() {
        let items = vec![
            msg(Role::User, "任务"),   // 2 token
            call("c1", "{}"),          // 4
            call("c2", "{}"),          // 4
            out("c1", &"a".repeat(100)), // 25
            out("c2", &"b".repeat(100)), // 25
            msg(Role::User, "继续"),   // 2
        ];
        // 预算 190 → 保留额度 57：旧算法从尾部装到 call c2 为止（56 ≤ 57 < 60）
        let target = plan_manual(&items, 190).expect("有可压区间");
        assert_eq!(target.range_end, 1, "切到组中间会丢掉 c1 却留下它的输出");
        assert_pairs_complete(&items[target.range_end..]);
    }

    /// D2：密文随下一轮请求原样回放，确实占输入 token。不计入就会让预算系统性
    /// 低估、压缩比实际需要更晚触发。
    ///
    /// 反向对照：删掉 `item_tokens` 的 `Reasoning` 分支，差值变成 0，这里变红。
    #[test]
    fn reasoning_tokens_are_counted() {
        let plain = vec![msg(Role::User, "任务")];
        let mut with_reasoning = plain.clone();
        with_reasoning.push(HistoryItem::Reasoning {
            id: None,
            summary: vec![],
            encrypted_content: Some("x".repeat(400)),
        });
        assert_eq!(
            history_tokens(&with_reasoning) - history_tokens(&plain),
            100,
            "400 字节密文应按 100 token 计入预算"
        );
    }

    /// D2：切点不能落在「推理项 → 它的助手消息」之间。只看保留侧，有消息没推理无害；
    /// 推理被压掉、消息还在，严格端点在续写时会对不上号。
    ///
    /// 反向对照：去掉 `tail_target` 里的 `cut_keeps_reasoning`，这里会得到 `range_end = 2`。
    #[test]
    fn cut_never_separates_reasoning_from_its_message() {
        let items = vec![
            msg(Role::User, &"u".repeat(400)), // 100 token
            HistoryItem::Reasoning {
                id: None,
                summary: vec![],
                encrypted_content: Some("g".repeat(40)), // 10
            },
            msg(Role::Assistant, &"a".repeat(20)), // 5
            msg(Role::User, "继续"),                // 2
        ];
        // 预算 50 → 保留额度 15：装得下最后两条（7），装不下推理项（17）。
        let target = plan_manual(&items, 50).expect("有可压区间");
        assert_eq!(target.range_end, 1, "推理项必须和它的助手消息一起留下");
    }

    /// 配对完整的尾巴不会被无谓地多留：切点本来就是组边界时，回退逻辑不得再动它。
    #[test]
    fn cut_on_a_group_boundary_is_left_untouched() {
        let items = vec![
            msg(Role::User, "任务"),          // 2 token
            call("c1", "{}"),                 // 4
            out("c1", &"a".repeat(100)),      // 25
            msg(Role::User, &"y".repeat(4_000)), // 1000
            msg(Role::User, "继续"),          // 2
        ];
        // 预算 3400 → 保留额度 1020：装得下最后两条（1002），装不下输出（1027）。
        let target = plan_manual(&items, 3_400).expect("有可压区间");
        assert_eq!(target.range_end, 3, "切点已在消息边界上，回退逻辑不得再动它");
        assert_pairs_complete(&items[target.range_end..]);
    }

    /// 反向回归：历史是一串**没有调用**的孤立输出（旧档案形态）时，切点不得一路退到 0。
    /// 按条目类型一刀切的实现会在这里把压缩做废（7 条进、7 条出）。
    #[test]
    fn orphan_outputs_do_not_block_compaction() {
        let mut items = vec![msg(Role::User, "任务")];
        for index in 0..6 {
            items.push(out(&format!("c{index}"), &"z".repeat(40_000)));
        }
        let target = plan_if_needed(0, &items, 50_000).expect("超预算必须给出区间");
        assert!(target.range_end < items.len(), "至少留最新一条");
        let after = 1 + items.len() - target.range_end;
        assert!(after < items.len(), "压缩必须真的让历史变短：{after} → {}", items.len());
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

    // ============================== 用户原文保真 ==============================

    #[test]
    fn user_text_is_preserved_verbatim_before_the_summary() {
        let items = vec![
            msg(Role::User, "硬约束：不得修改 src/sandbox/mod.rs"),
            msg(Role::Assistant, "收到"),
            msg(Role::User, "再补一条：只改测试"),
            msg(Role::Assistant, "好"),
            msg(Role::User, "第三条：不要 push"),
        ];
        let target = Target { range_start: 0, range_end: 5 };
        let composed = compose_summary(&items, &target, 500_000, "摘要正文");

        // 首条必须逐字存在——压缩把任务目标交给一次模型总结去保管是不可接受的
        assert!(composed.contains("硬约束：不得修改 src/sandbox/mod.rs"));
        assert!(composed.contains("再补一条：只改测试"));
        assert!(composed.contains("第三条：不要 push"));
        // 原文段落必须在摘要之前，摘要仍在最后
        let header = composed.find(USER_TEXT_HEADER).expect("必须有原文段落");
        let first = composed.find("硬约束").expect("首条必须在");
        let summary = composed.find("摘要正文").expect("摘要必须在");
        assert!(header < first && first < summary, "顺序必须是 原文段落 → 摘要");
        // 原文按时间顺序，不因"从近到远"装填而被倒序
        assert!(first < composed.find("再补一条").unwrap());
        assert!(composed.find("再补一条").unwrap() < composed.find("第三条").unwrap());
    }

    #[test]
    fn user_text_budget_keeps_first_and_the_nearest_window() {
        // budget 1_000 → 原文额度 300 token ≈ 1200 字节；首条约 200 字节无条件保留。
        let first = "首条硬约束：".to_string() + &"a".repeat(180);
        let old = "旧消息：".to_string() + &"b".repeat(2000);
        let near = "近消息：".to_string() + &"c".repeat(100);
        let items = vec![
            msg(Role::User, &first),
            msg(Role::User, &old),
            msg(Role::Assistant, "中间回复"),
            msg(Role::User, &near),
        ];
        let target = Target { range_start: 0, range_end: 4 };
        let composed = compose_summary(&items, &target, 1_000, "摘要");

        assert!(composed.contains(&first), "首条无条件保留");
        assert!(composed.contains(&near), "预算内要从近到远装入");
        assert!(!composed.contains(&old), "超预算时先裁最旧的，近的留下");

        // 预算为 0（显式关压缩）也必须保住首条
        let zero = compose_summary(&items, &target, 0, "摘要");
        assert!(zero.contains(&first));
        assert!(!zero.contains(&near));
    }

    /// N9：区间没有用户发言时也必须留下结构标记，且原文段为空而不是整段继承。
    ///
    /// 反向对照：退回「无用户消息就 `return summary.to_string()`」，这里会拿到裸摘要，
    /// `carried_user_text` 返回 None——下一轮就把它整段当成用户原文（真机 N9）。
    #[test]
    fn compose_without_user_messages_marks_an_empty_user_section() {
        let items = vec![msg(Role::Assistant, "只有模型发言")];
        let target = Target { range_start: 0, range_end: 1 };
        let composed = compose_summary(&items, &target, 100_000, "摘要正文");

        assert_eq!(composed.matches(USER_TEXT_HEADER).count(), 1, "标题必须保留：{composed}");
        assert!(composed.contains(SUMMARY_SEPARATOR), "没有分隔线就认不出这是生成摘要");
        assert!(composed.ends_with("摘要正文"), "模型摘要正文必须在：{composed}");
        assert_eq!(
            carried_user_text(&composed),
            Some(""),
            "空原文段必须被识别成空，而不是退化成整段继承"
        );

        // 空白用户消息不算原文：标题下不能出现一段纯空白。
        let items = vec![msg(Role::User, "   \n"), msg(Role::Assistant, "只有模型发言")];
        let target = Target { range_start: 0, range_end: 2 };
        assert_eq!(carried_user_text(&compose_summary(&items, &target, 5_000, "摘要正文")), Some(""));
    }

    // ============================== 连续压缩的幂等性（N8） ==============================

    /// 2026-11-14 真机冒烟 N8 的形态：每次压缩只覆盖上一条摘要（`range=0-1`）。
    ///
    /// 摘要以 user 身份进历史，旧实现把它当「用户原文」整段再包一层新标题：
    /// 12 次之后标题嵌套 11 层、体积 167 → 6412 字节。
    ///
    /// 反向对照：去掉 `compose_summary` 里的 `carried_user_text`，第二轮起
    /// 标题数变 2、3…，且「旧摘要-00-END」会被逐字嵌入下一版，本用例立刻红。
    #[test]
    fn recompacting_a_generated_summary_does_not_nest_it_again() {
        let mut history = vec![
            msg(Role::User, "硬约束：只改测试，不要 push"),
            msg(Role::Assistant, "收到"),
        ];
        let mut sizes = Vec::new();
        for round in 0..12 {
            let target = Target { range_start: 0, range_end: 1 };
            let summary = format!("旧摘要-{round:02}-END");
            let composed = compose_summary(&history, &target, 5_000, &summary);

            assert_eq!(
                composed.matches(USER_TEXT_HEADER).count(),
                1,
                "第 {round} 轮：标题被嵌套了"
            );
            assert_eq!(
                composed.matches("硬约束：只改测试，不要 push").count(),
                1,
                "第 {round} 轮：原文应恰好保留一份"
            );
            assert!(composed.contains(&summary), "第 {round} 轮：本次模型摘要必须在");
            assert_eq!(
                carried_user_text(&composed),
                Some("硬约束：只改测试，不要 push"),
                "第 {round} 轮：用户原文段被模型摘要正文污染"
            );
            if round > 0 {
                assert!(
                    !composed.contains(&format!("旧摘要-{:02}-END", round - 1)),
                    "第 {round} 轮：上一版模型摘要被当成用户原文逐字嵌入"
                );
            }
            sizes.push(composed.len());
            history = vec![msg(Role::User, &composed), msg(Role::Assistant, "继续")];
        }
        assert!(
            sizes.windows(2).all(|pair| pair[1] <= pair[0] + 32),
            "摘要体积随压缩次数膨胀：{sizes:?}"
        );
        assert_eq!(history.len(), 2, "1 条换 1 条，条目数不变");
    }

    /// N8 的空转形态：最旧一条就是上一条摘要时，自动压缩必须跳过。
    ///
    /// 反向对照：退回 `is_lone_generated_summary`（只认 `Role::User` 的摘要）而不看
    /// 循环里的系统注入，`automatic_planning_skips_a_system_only_range` 会红；这里
    /// 换成退回「所有区间都放行」，本用例立刻返回 `Some([0,1))`。
    #[test]
    fn automatic_planning_skips_a_lone_generated_summary() {
        let generated = compose_summary(
            &[msg(Role::User, "任务目标")],
            &Target { range_start: 0, range_end: 1 },
            5_000,
            "模型摘要",
        );
        assert!(carried_user_text(&generated).is_some(), "构造前提：本模块生成的摘要");

        let items = vec![msg(Role::User, &generated), msg(Role::Assistant, "最新一条")];
        assert_eq!(
            plan_if_needed(5_000, &items, 5_000),
            None,
            "只有上一条摘要的区间没有任何可回收内容"
        );

        // 反例：最旧一条是真用户消息时，退化压缩照旧保留，否则观测值虚高时无法回收。
        let plain = vec![msg(Role::User, "任务目标"), msg(Role::Assistant, "最新一条")];
        assert_eq!(
            plan_if_needed(5_000, &plain, 5_000),
            Some(Target { range_start: 0, range_end: 1 })
        );
    }

    /// N9 的触发源：区间只有系统注入（环境快照）时，自动压缩必须跳过。
    ///
    /// 真机 `2026-11-14T16:10:31.117818Z` 就是这里放行的：`tail_target` 的不变量④
    /// 把区间钳到 `[0,1)`，唯一一条是 `Role::System` 的环境快照；旧守卫
    /// `is_lone_generated_summary` 只认 `Role::User`，于是 `compressed_items=1`
    /// 的空转摘要被写进历史。
    ///
    /// 反向对照：退回旧守卫，本用例第一段返回 `Some([0,1))` 变红。
    #[test]
    fn automatic_planning_skips_a_system_only_range() {
        let items = vec![
            msg(Role::System, "## 环境快照\n- workdir: repo"),
            msg(Role::Assistant, "最新的真实内容"),
        ];
        assert_eq!(
            plan_if_needed(5_000, &items, 5_000),
            None,
            "系统注入不是可总结内容，压它只会得到空集摘要"
        );

        // 反例：区间里确实有真实助手内容时不得跳过，否则观测值虚高就永远回收不了。
        let items = vec![
            msg(Role::System, "## 环境快照"),
            msg(Role::Assistant, "真实内容"),
            msg(Role::Assistant, &"x".repeat(8_000)),
        ];
        assert_eq!(
            plan_if_needed(5_000, &items, 5_000),
            Some(Target { range_start: 0, range_end: 2 })
        );
    }

    /// 手动 `/compact` 与溢出补救**不挂**「无真实内容不压」的闸门。
    ///
    /// 溢出补救是强制补救：端点已经报了超长，「区间里没内容」这个前提不成立，
    /// 跳过等于放任失败（真机契约 `retry_contract::c16` 要求原请求 + 一次补救压缩）。
    /// 只有系统注入时压出来的摘要带标题与分隔线、原文段为空，下一轮不会被当成
    /// 用户原文继承（见 [`compose_without_user_messages_marks_an_empty_user_section`]），
    /// 所以放开它不会把 N9 重新打开。
    #[test]
    fn manual_planning_compacts_even_a_system_only_range_without_inheriting_it() {
        let items = vec![msg(Role::System, "## 环境快照"), msg(Role::Assistant, "最新一条")];
        let target = plan_manual(&items, 5_000).expect("强制补救必须尝试压缩");
        assert_eq!(target, Target { range_start: 0, range_end: 1 });

        let composed = compose_summary(&items, &target, 5_000, "压缩对象为空集");
        assert_eq!(
            carried_user_text(&composed),
            Some(""),
            "空集摘要不得留下可继承的用户原文段"
        );

        // 旧摘要同样允许重压：摘要过大时那是唯一收缩手段。
        let generated = compose_summary(
            &[msg(Role::User, "任务目标")],
            &Target { range_start: 0, range_end: 1 },
            5_000,
            "模型摘要",
        );
        let items = vec![msg(Role::User, &generated), msg(Role::Assistant, "最新一条")];
        assert_eq!(
            plan_manual(&items, 5_000),
            Some(Target { range_start: 0, range_end: 1 })
        );
    }

    /// N9 的真机链路：空集摘要落进历史后，不得成为下一份摘要的用户原文段。
    ///
    /// 真机形态：区间 `[0,1)` 只有环境快照，模型回「压缩对象为空集…本轮未收到
    /// 具体用户任务」，这条裸摘要以 user 身份写回，下一轮 `carried_user_text`
    /// 认不出（无标题+分隔线），整段被当成用户原文逐字继承，此后每次压缩都多背一段。
    ///
    /// 两侧都要堵住：空集区间不产生计划；即便摘要真被写出（手动路径），
    /// `compose_summary` 也会给它结构标记，下一轮只继承空原文段。
    ///
    /// 反向对照：单独退回 `compose_summary` 的裸摘要分支，第 2 段 `Some("")` 变 None；
    /// 单独退回 `plan_if_needed` 的旧守卫，第 1 段变红。
    #[test]
    fn empty_range_never_produces_inheritable_user_text() {
        let mut history = vec![
            msg(Role::System, "## 环境快照\n- workdir: repo"),
            msg(Role::User, "任务：跑测试"),
            msg(Role::Assistant, "好"),
        ];
        // 1) 只有系统注入的区间不产生压缩计划（真机空转的直接来源）。
        assert_eq!(plan_if_needed(5_000, &history, 5_000), None);

        // 2) 假使走手动路径写出摘要，它也必须带结构标记、原文段为空。
        let target = Target { range_start: 0, range_end: 1 };
        let checkpoint = compose_summary(&history, &target, 5_000, "压缩对象为空集，本轮未收到具体用户任务。");
        assert_eq!(
            carried_user_text(&checkpoint),
            Some(""),
            "空集摘要必须被认出是生成摘要，且没有可继承原文"
        );
        replace_with_summary(&mut history, target, checkpoint);

        // 3) 下一轮压缩：用户原文段只含真实用户发言，不含模型摘要正文。
        let target = Target { range_start: 0, range_end: 2 };
        let next = compose_summary(&history, &target, 5_000, "第二份摘要");
        let carried = carried_user_text(&next).expect("两段式输出必须可识别");
        assert!(carried.contains("任务：跑测试"), "真实用户发言要留下：{carried}");
        assert!(!carried.contains("压缩对象为空集"), "模型摘要正文不得进入用户原文段：{carried}");
        assert_eq!(next.matches(USER_TEXT_HEADER).count(), 1, "标题不得嵌套：{next}");
    }

    /// N8 的循环形态：小预算 + 每步追加少量条目，自动压缩不得每步空转、摘要不得膨胀、
    /// 历史不得无界增长。
    ///
    /// 反向对照（两个修复各撤一个都会红）：撤掉跳过空转检查，第 1 步起标题数就 +1；
    /// 撤掉原文提取，摘要体积随压缩次数线性增长，token 上界断言同样会红。
    #[test]
    fn repeated_pressure_compaction_keeps_summary_flat_and_history_bounded() {
        let budget = 5_000usize;
        let observed = budget; // 真机同款：端点报回的用量已顶到预算
        let mut history = vec![
            msg(Role::User, "任务：改 src/sum.js 并跑 pnpm test"),
            msg(Role::Assistant, "先读文件"),
            call("c1", "{\"path\":\"src/sum.js\"}"),
            out("c1", "export function sum(a, b) { return a + b }"),
            msg(Role::Assistant, "看到了"),
        ];
        let mut compactions = 0;
        let mut largest = 0usize;
        for step in 0..40 {
            history.push(msg(Role::Assistant, &format!("第 {step} 步")));
            history.push(call(&format!("c{step}"), "{}"));
            history.push(out(&format!("c{step}"), &"y".repeat(200)));

            if let Some(target) = plan_if_needed(observed, &history, budget) {
                assert!(
                    has_real_content(&history, &target),
                    "第 {step} 步压缩了没有真实内容的区间（range={target:?}）"
                );
                let composed = compose_summary(
                    &history,
                    &target,
                    budget,
                    &format!("模型摘要-{step:02}-END"),
                );
                replace_with_summary(&mut history, target, composed);
                compactions += 1;
            }
            assert_eq!(
                first_text(&history).matches(USER_TEXT_HEADER).count(),
                1,
                "第 {step} 步摘要出现嵌套"
            );
            assert!(
                history_tokens(&history) <= 2_000,
                "第 {step} 步历史 token 无界：{}",
                history_tokens(&history)
            );
            largest = largest.max(history.len());
        }
        assert!(compactions < 40, "每一步都在压缩：{compactions}");
        // 保留额度是 1500 token，条目这一侧的上界由它换算而来；这里留足余量。
        assert!(largest <= 90, "history_len 无界：{largest}");
    }

    /// 问题一（真机 14 步 4 次压缩）的判据：常规预算下压缩次数不随步数增长。
    ///
    /// 真机那几次压缩都发生在 `token_budget=5000`（r4）/ `100`（r5-auto）的冒烟配置里：
    /// 同会话首个请求报回 5,154 tokens，其中 system prompt 与工具声明是**固定开销**，
    /// 压缩动不了它。预算低于固定开销时，每一步都必然越过阈值——这是预算定义下的
    /// 必然结果，不是策略在空转；产品默认 500,000 时同样的 14 步应当一次都不压。
    ///
    /// 这里用真机量级（每步约 1.5k tokens）锁住常规预算的判据；小预算下"只压真实
    /// 内容、不压纯系统注入/旧摘要"的回收口径由 `automatic_planning_*` 三条钉住。
    ///
    /// 反向对照：把判据改成不比较预算（恒返回 `Some`），本用例立刻红。
    #[test]
    fn normal_budget_does_not_compact_a_fourteen_step_turn() {
        let mut history = vec![msg(Role::User, "跑一遍冒烟并汇报")];
        let mut compactions = 0;
        for step in 0..14 {
            history.push(msg(
                Role::Assistant,
                &format!("第 {step} 步说明：{}", "说明".repeat(300)),
            ));
            history.push(msg(
                Role::Assistant,
                &format!("工具输出：{}", "结果".repeat(600)),
            ));
            let estimate = history_tokens(&history);
            if plan_if_needed(estimate, &history, 500_000).is_some() {
                compactions += 1;
            }
        }
        assert_eq!(compactions, 0, "常规预算下 14 步触发了压缩");
        // 按 128K 窗口推导的阈值（102,400）同样够用。
        assert!(
            history_tokens(&history) < 102_400,
            "14 步估算 {} 已逼近窗口阈值",
            history_tokens(&history)
        );
    }
}
