//! 项目规则注入（AGENTS.md）：向上查找 + 作用域优先级 + 按标题语义分层 + token 预算。
//!
//! 事实来源是磁盘上的 `AGENTS.md`，不是内置文案：仓库自己写的规则必须能被模型看到，
//! 否则"规则写在仓库里、模型只收到系统提示"就是两套口径。
//!
//! 三条约束（docs/04 §6.1）：
//!  - **向上查找**：从工作区根出发逐级向上，直到盘符根；找到几份就收几份。
//!  - **作用域优先级**：外层在前、内层在后；同名二级标题以内层为准（内层覆盖外层）。
//!  - **切节粒度**：`## ` 与 `### ` 都算节（`4.2` 与 `4.2.1` 是两节，否则声明 `4.2`
//!    会把细则一起拉进常驻）；文首导语没有标题，单列一节。
//!  - **选节**：文件里有常驻声明（`<!-- shaniu-rules: resident ... -->`）时按声明**精确**
//!    选节，命中的小节整节注入、不裁剪不丢弃；没有声明时按标题语义分层（[`Tier`]），
//!    原则类小节超预算按行截断，普通小节装不下则记标题进 `dropped`。
//!  - **预算**：注入正文（常驻正文 + 按需索引）≤ [`RULE_TOKEN_BUDGET`]。
//!
//! 只读文件内容进提示词，不做执行、不做解释；文件读不到或超限只影响注入量，不阻断运行。

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// 规则文件名（向上查找时使用的固定名字）。
pub const RULE_FILE_NAME: &str = "AGENTS.md";

/// 注入正文的 token 预算（docs/04 §6.1 的"项目规则 ≤1100 tokens"）。
///
/// 三个数各有地位，别混：
///  - **2500**（`prompt::PROMPT_TOKEN_LIMIT`）是**实测**：生产日志
///    `prompt_tokens=2722 limit=2500`，那是整套提示词的上限。
///  - **1100** 是**约定**：规则段占全套 44%（1100/2500）。常驻集跨会话逐字不变，
///    属稳定前缀、走 prefix cache；1100 能稳稳越过 1024 的最小缓存块，比贴线的 1000 稳。
///  - **1000** 是**已废的约定**：没有推导，只是当初拍的分配额，装不下全部禁令。
pub const RULE_TOKEN_BUDGET: usize = 1100;

/// 声明坏掉时插在正文最前面的横幅（`ProjectRules::text` 的第一行）。
pub const DECLARATION_BANNER: &str = "⚠ 规则声明无效：";

const DECLARATION_TAG: &str = "shaniu-rules:";

/// 常驻声明：`<!-- shaniu-rules: resident 0.,1.0,2.,2.2,3.,4.2,4.3,11.3 -->`。
///
/// 有声明 = 常驻集由文件自己说了算；没有 = 按标题语义分层（旧行为）。
/// **声明坏了不回退成空注入**：agent 零规则运行比超预算更坏，所以回退旧分层 + 首行横幅。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Declaration {
    /// 文件里没有声明行：按标题语义分层。
    #[default]
    Absent,
    /// 声明有效：只注入锚点命中的小节，按文档顺序整节注入。
    Resident { anchors: Vec<String>, line: usize },
    /// 声明存在但坏了：回退旧分层，并在正文首行插横幅。
    Invalid { kind: &'static str, line: usize },
}

/// 解析声明行。这里只做**语法**判断；「锚点一条都匹配不到」需要标题，由 [`discover`] 补判。
pub fn read_declaration(text: &str) -> Declaration {
    let Some((index, line)) =
        text.lines().enumerate().find(|(_, line)| line.contains(DECLARATION_TAG))
    else {
        return Declaration::Absent;
    };
    let no = index + 1;
    let trimmed = line.trim();
    let Some(inner) = trimmed.strip_prefix("<!--") else {
        return Declaration::Invalid { kind: "声明不在注释标签里", line: no };
    };
    let Some(inner) = inner.strip_suffix("-->") else {
        return Declaration::Invalid { kind: "注释标签未闭合", line: no };
    };
    let Some(rest) = inner.split_once(DECLARATION_TAG).map(|(_, rest)| rest.trim()) else {
        return Declaration::Invalid { kind: "声明字段缺失", line: no };
    };
    let Some(rest) = rest.strip_prefix("resident") else {
        return Declaration::Invalid { kind: "缺少 resident 字段", line: no };
    };
    let anchors: Vec<String> = rest
        .split(',')
        .map(str::trim)
        .filter(|anchor| !anchor.is_empty())
        .map(str::to_string)
        .collect();
    if anchors.is_empty() {
        return Declaration::Invalid { kind: "resident 为空", line: no };
    }
    Declaration::Resident { anchors, line: no }
}

/// **声明匹配（常驻）**：分隔符只认空格或标题结尾。
///
/// `4.2` 命中 `4.2 门禁响度`，**不**命中 `4.2.1 事故与细则`。常驻是预算分配，
/// 多拉一节就多花一份 token；这里若按前缀匹配，声明里拆出 `4.2.1` 就等于白拆。
///
/// 与 [`matches_readback`] 是两套语义，**禁止合并**：合并的后果是下一个「顺手统一」
/// 的人把常驻集悄悄变大，而预算账面上看不出来。两套各写各的、各自单测。
fn matches_declaration(heading: &str, anchor: &str) -> bool {
    match heading.strip_prefix(anchor) {
        Some("") => true,
        Some(rest) => rest.starts_with(' '),
        None => false,
    }
}

/// **回读匹配（按需）**：命中本节与全部子节，多认 `.` 续号。
///
/// `4.2` 命中 `4.2` 和 `4.2.*`：索引说的是"去读哪一段"，读到 `4.2` 当然连细则一起读。
/// 与 [`matches_declaration`] 只差一个 `.`，但两件事目的相反（一个分配预算、
/// 一个覆盖阅读范围），合并会让其中一边出错且看不出来。
fn matches_readback(heading: &str, anchor: &str) -> bool {
    match heading.strip_prefix(anchor) {
        Some("") => true,
        Some(rest) => rest.starts_with(' ') || rest.starts_with('.'),
        None => false,
    }
}

/// 单个规则文件的字节上限：超过即视为异常文件，截断后标记，不整份塞进提示词。
const MAX_FILE_BYTES: u64 = 256 * 1024;

/// **必须保留**的语义：铁律 / 原则 / 红线 / 安全 / 边界 / 保护。
///
/// 「安全 / 边界 / 保护」与铁律同级而不是降级为「优先」：它们写的是不可越过的约束，
/// 不是"有空再读"的说明。命中即进最高优先级，预算不足时按行截断，不允许整节消失。
const MUST_KEYWORDS: [&str; 6] = ["铁律", "原则", "红线", "安全", "边界", "保护"];

/// **约束性**语义：禁止 / 约定 / 约束 / 规范。
/// 它们排在必须保留之后、普通说明之前；预算不够时整节丢弃并**记下标题**。
const GUARD_KEYWORDS: [&str; 4] = ["禁止", "约定", "约束", "规范"];

/// 小节优先级。判定只看**标题语义**，不看位置：位置会随文档重排而变，
/// 语义不会——这是「缝合原则被裁掉」那次事故的直接教训。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    /// 铁律/原则/红线/安全/边界/保护：必须出现在上下文里，超出预算就按行截断。
    Must,
    /// 禁止/约定/约束/规范：优先于普通说明。
    Guard,
    /// 目录、流程、背景等：填满剩余预算即可。
    Other,
}

fn tier_of(heading: &str) -> Tier {
    if MUST_KEYWORDS.iter().any(|word| heading.contains(word)) {
        return Tier::Must;
    }
    if GUARD_KEYWORDS.iter().any(|word| heading.contains(word)) {
        return Tier::Guard;
    }
    Tier::Other
}

/// 一次注入的产物。`text` 是真正进提示词的部分，`fingerprint` 覆盖**未裁剪的全文**，
/// 这样"注入被预算裁掉了一半"这件事能从指纹与 `truncated` 上直接看出来。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectRules {
    /// 已按预算裁剪的正文（空串表示没有可注入内容）。
    pub text: String,
    /// 参与注入的文件，外层在前、内层在后。
    pub sources: Vec<PathBuf>,
    /// 所有来源文件全文拼接后的 sha256 前 16 位（空集合为 `"-"`）。
    pub fingerprint: String,
    /// 未注入的标题：声明模式下 = 声明排除的小节（按需读），旧分层下 = 预算装不下的小节。
    /// **必须记标题而不是计数**：只有计数时，"重要条款被静默裁掉"在日志里看不出来。
    pub dropped: Vec<String>,
    /// 声明行的解析结果（`Absent` = 无声明按旧分层；`Invalid` = 已回退并加了首行横幅）。
    pub declaration: Declaration,
    /// 注入了但被按行截断的标题（只可能是 [`Tier::Must`]）。
    pub clipped: Vec<String>,
    /// 未注入标题是否已经写进 `text` 的尾部留痕。
    /// `false` + `dropped` 非空 = 预算连留痕都装不下，调用方要另外说明。
    pub note_inlined: bool,
    /// 是否有任何取舍（等价于 `dropped` 或 `clipped` 非空，供日志直读）。
    pub truncated: bool,
}

impl ProjectRules {
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    /// 一句话说明这批规则是按什么选出来的（进"内容锁"日志）。
    ///
    /// 出问题时第一句要能回答"为什么这几节在、那几节不在"；只记 `dropped` 清单
    /// 回答不了这个——清单说的是结果，不是依据。
    pub fn selection(&self) -> String {
        match &self.declaration {
            Declaration::Resident { anchors, line } => {
                format!("声明选节：第 {line} 行，{} 个锚点", anchors.len())
            }
            Declaration::Invalid { kind, line } => {
                format!("声明无效（第 {line} 行：{kind}），已回退旧分层")
            }
            Declaration::Absent => "无声明，按标题语义分层".to_string(),
        }
    }
}

/// 粗粒度 token 估算：CJK 字符按 1 token，ASCII 按 4 字节 1 token。
///
/// 估算而不是精确分词的原因：预算只需要一个稳定的上界口径，精确分词要引入
/// 与模型绑定的 tokenizer（端点一换就失真），反而不可比。
pub fn estimate_tokens(text: &str) -> usize {
    let mut wide = 0usize;
    let mut narrow = 0usize;
    for ch in text.chars() {
        if ch.is_ascii() {
            narrow += 1;
        } else {
            wide += 1;
        }
    }
    wide + narrow.div_ceil(4)
}

/// 按行截断到预算内（不切半行），并记录**截到哪一行**。
///
/// 只给节标题是不够的：日志里必须能看出被砍掉的是哪几条（例如「11.3 退化红线」
/// 是否整段消失），否则"截断"仍然是一个看不出后果的数字。
struct ClippedLines {
    text: String,
    kept_lines: usize,
    total_lines: usize,
    /// 第一条没进正文的行（压缩空白后截到 40 字符）。
    first_omitted: Option<String>,
}

/// 返回值的 `estimate_tokens` 严格 ≤ `budget`：先给截断标注留位置，再按整行往里填。
fn clip_lines(rendered: &str, budget: usize) -> Option<ClippedLines> {
    const CLIP_MARKER: &str = "\n（本节超预算，已截断）";
    let room = budget.saturating_sub(estimate_tokens(CLIP_MARKER));
    let lines: Vec<&str> = rendered.lines().collect();
    let mut kept = String::new();
    let mut kept_lines = 0usize;
    for line in &lines {
        let candidate = if kept.is_empty() {
            (*line).to_string()
        } else {
            format!("{kept}\n{line}")
        };
        if estimate_tokens(&candidate) > room {
            break;
        }
        kept = candidate;
        kept_lines += 1;
    }
    if kept.trim().is_empty() {
        return None;
    }
    let first_omitted = lines.get(kept_lines).map(|line| {
        let collapsed = line.trim();
        if collapsed.chars().count() > 40 {
            format!("{}…", collapsed.chars().take(40).collect::<String>())
        } else {
            collapsed.to_string()
        }
    });
    Some(ClippedLines {
        text: format!("{kept}{CLIP_MARKER}"),
        kept_lines,
        total_lines: lines.len(),
        first_omitted,
    })
}

/// 一个小节的标题与正文。
struct Section {
    /// 标题原文（不含 `## ` / `### `）。
    heading: String,
    body: String,
}

impl Section {
    /// 优先级只看标题语义。
    fn tier(&self) -> Tier {
        tier_of(&self.heading)
    }

    /// 记日志/留痕用的标题名。
    fn title(&self) -> String {
        self.heading.clone()
    }

    /// 渲染成注入文本：节标题用三级标题，避免与系统提示自己的二级标题混淆。
    fn render(&self) -> String {
        format!("### {}\n{}", self.heading, self.body.trim_end())
    }
}

/// 把一份文件切成小节：行首 `## ` 或 `### ` 开新节。
///
/// 两级都切是为了让「声明 `4.2`」与「`4.2.1` 细则」成为两节——只切 `## ` 的话，
/// 声明 `4.2` 会把细则整段一起拉进常驻，预算当场多出一大块。
/// 渲染时两级都降为 `### `（注入正文是平铺的小节列表，不还原层级）。
///
/// **文首导语不算节**：第一个标题之前的内容（声明行、给维护者看的取舍政策）不进注入、
/// 不计入目录——它是写给改这个文件的人看的，不是给模型的规则，因此也是零预算成本的。
fn split_sections(text: &str) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    let mut heading: Option<String> = None;
    let mut body = String::new();
    for line in text.lines() {
        let rest = line.strip_prefix("### ").or_else(|| line.strip_prefix("## "));
        if let Some(rest) = rest {
            if let Some(done) = heading.take() {
                sections.push(Section { heading: done, body: std::mem::take(&mut body) });
            }
            heading = Some(rest.trim().to_string());
            continue;
        }
        body.push_str(line);
        body.push('\n');
    }
    if let Some(done) = heading {
        sections.push(Section { heading: done, body });
    }
    sections
}

/// 从工作区根向上收集 `AGENTS.md`，返回外层到内层的顺序。
fn discover_files(workspace_root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut cursor = Some(workspace_root);
    while let Some(dir) = cursor {
        let candidate = dir.join(RULE_FILE_NAME);
        if candidate.is_file() {
            found.push(candidate);
        }
        cursor = dir.parent();
    }
    found.reverse();
    found
}

/// 读单个文件；超过上限时按字符边界截断（避免切碎 UTF-8）。
fn read_capped(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_FILE_BYTES {
        let bytes = std::fs::read(path).ok()?;
        let cut = MAX_FILE_BYTES as usize;
        let end = (0..=cut).rev().find(|i| bytes.get(*i).is_none_or(|b| (b & 0xC0) != 0x80))?;
        return String::from_utf8_lossy(&bytes[..end]).into_owned().into();
    }
    std::fs::read_to_string(path).ok()
}

/// 向上查找并组装注入正文。
///
/// 读不到任何规则文件时返回空结构（`fingerprint` 为 `"-"`）：调用方据此整段省略，
/// 不向模型注入空的"项目规则"标题。
pub fn discover(workspace_root: &Path) -> ProjectRules {
    let files = discover_files(workspace_root);
    if files.is_empty() {
        return ProjectRules { fingerprint: "-".to_string(), ..ProjectRules::default() };
    }

    let mut snapshot = String::new();
    // 外层先行写入，内层同名小节原地覆盖 —— 位置保持外层顺序，内容以最内层为准
    let mut merged: Vec<Section> = Vec::new();
    let mut sources = Vec::new();
    let mut loaded: Vec<(PathBuf, String)> = Vec::new();
    for path in &files {
        let Some(text) = read_capped(path) else { continue };
        snapshot.push_str(&text);
        snapshot.push('\n');
        sources.push(path.clone());
        for section in split_sections(&text) {
            match merged
                .iter_mut()
                .find(|existing| existing.heading == section.heading)
            {
                Some(existing) => existing.body = section.body,
                None => merged.push(section),
            }
        }
        loaded.push((path.clone(), text));
    }

    let fingerprint = format!("{:x}", Sha256::digest(snapshot.as_bytes()))
        .chars()
        .take(16)
        .collect::<String>();

    // 声明以**最内层**为准：外层仓库的声明不该管住内层工作区的规则组织。
    let declaration = loaded
        .iter()
        .rev()
        .map(|(_, text)| read_declaration(text))
        .find(|declaration| !matches!(declaration, Declaration::Absent))
        .map(|declaration| match declaration {
            Declaration::Resident { anchors, line }
                if !anchors
                    .iter()
                    .any(|anchor| merged.iter().any(|s| matches_declaration(&s.heading, anchor))) =>
            {
                // 声明指向的小节一个都不存在：不是"内容全被覆盖"，是文件改坏了。
                tracing::error!(
                    line,
                    anchors = %anchors.join(","),
                    "AGENTS.md 的常驻声明锚点一条都匹配不到"
                );
                Declaration::Invalid { kind: "锚点一条都匹配不到", line }
            }
            other => other,
        })
        .unwrap_or_default();

    let (mut text, dropped, clipped, note_inlined) = match &declaration {
        Declaration::Resident { anchors, .. } => {
            // 声明即契约：命中的小节**整节**注入，不裁剪、不丢弃。
            // 装不下是文档的错，由用例红来暴露，不在运行时把它悄悄改成半条规则。
            let (kept, rest): (Vec<&Section>, Vec<&Section>) =
                merged.iter().partition(|section| {
                    anchors.iter().any(|anchor| matches_declaration(&section.heading, anchor))
                });
            warn_unindexed(&rest);
            let mut text =
                kept.iter().map(|section| section.render()).collect::<Vec<_>>().join("\n\n");
            let note_inlined = if rest.is_empty() {
                false
            } else {
                text.push_str("\n\n");
                text.push_str(&ondemand_note(rest.len()));
                true
            };
            (text, rest.iter().map(|section| section.title()).collect(), Vec::new(), note_inlined)
        }
        _ => pack_legacy(&merged),
    };

    if let Declaration::Invalid { kind, line } = &declaration {
        text = format!(
            "{DECLARATION_BANNER}{kind}（第 {line} 行），本次按旧分层注入，可能超预算——请修 AGENTS.md\n\n{text}"
        );
    }

    ProjectRules {
        text,
        sources,
        fingerprint,
        declaration,
        truncated: !dropped.is_empty() || !clipped.is_empty(),
        dropped,
        clipped,
        note_inlined,
    }
}

/// 索引覆盖自检：未注入的小节必须能在按需索引里被找到。
///
/// 这里用**回读（前缀）**匹配而不是声明匹配：索引条目 `§4` 要覆盖 `4.1`、`4.4`。
/// 漏覆盖只影响"模型去哪读"，不该阻断运行，所以记 warn 而不是 panic。
fn warn_unindexed(sections: &[&Section]) {
    for section in sections {
        let covered = ONDEMAND_INDEX.iter().any(|(anchors, _)| {
            anchors.iter().any(|anchor| matches_readback(&section.heading, anchor))
        });
        if !covered {
            tracing::warn!(
                heading = %section.title(),
                "按需索引没有覆盖这个未注入小节，模型读不到它"
            );
        }
    }
}

/// 旧分层装法（无声明 / 坏声明）：Must → Guard → Other，同层保持文档顺序。
///
/// **位置不参与判定**：旧实现按文档顺序贪心装预算，`11. 缝合原则` 因为排在最后
/// 被前面的普通小节挤掉——规则写在文件里、模型上下文里没有，等于没有这条规则。
fn pack_legacy(merged: &[Section]) -> (String, Vec<String>, Vec<String>, bool) {
    let mut ordered: Vec<&Section> = Vec::with_capacity(merged.len());
    for tier in [Tier::Must, Tier::Guard, Tier::Other] {
        ordered.extend(merged.iter().filter(|section| section.tier() == tier));
    }

    // 留痕本身占预算，所以必须在装节**之前**预留：先装完再试着塞，塞不进就只剩
    // "只报数量"这一档——模型看到"可能不全"，却不知道少了哪几条，而这正是要治的病。
    // 丢的节越多留痕越长，越该在前面让位，而不是把留痕自己挤掉。
    // 重算到稳定（丢的节只会随预算缩小而变多，两三轮即收敛）。
    let mut packed = pack(&ordered, RULE_TOKEN_BUDGET);
    let mut reserve = 0usize;
    for _ in 0..8 {
        if packed.dropped.is_empty() {
            break;
        }
        let need = estimate_tokens(&dropped_note(&packed.dropped)) + 1;
        if need <= reserve {
            break;
        }
        reserve = need.min(NOTE_TOKEN_CAP + 1);
        packed = pack(&ordered, RULE_TOKEN_BUDGET - reserve);
    }
    let Packed { mut kept, dropped, clipped, used } = packed;

    let mut note_inlined = false;
    if !dropped.is_empty() {
        let note = dropped_note(&dropped);
        let cost = estimate_tokens(&note) + 1;
        if used + cost <= RULE_TOKEN_BUDGET {
            kept.push((Tier::Other, String::new(), note));
            note_inlined = true;
        }
    }

    let text = kept
        .iter()
        .map(|(_, _, rendered)| rendered.trim_end())
        .collect::<Vec<_>>()
        .join("\n\n");
    (text, dropped, clipped, note_inlined)
}

struct Packed {
    kept: Vec<(Tier, String, String)>,
    dropped: Vec<String>,
    clipped: Vec<String>,
    used: usize,
}

/// 按给定预算装一遍：Must 装不下就按行截断，其它装不下整节记进 `dropped`。
fn pack(ordered: &[&Section], budget: usize) -> Packed {
    let mut kept: Vec<(Tier, String, String)> = Vec::new();
    let mut dropped: Vec<String> = Vec::new();
    let mut clipped: Vec<String> = Vec::new();
    let mut used = 0usize;
    for section in ordered {
        let rendered = section.render();
        let tier = section.tier();
        let cost = estimate_tokens(&rendered) + 1;
        if used + cost <= budget {
            used += cost;
            kept.push((tier, section.title(), rendered));
            continue;
        }
        if tier == Tier::Must {
            // Must 小节宁可截断也不整节消失：模型看到半条规则，也好过看不到。
            if let Some(partial) = clip_lines(&rendered, budget.saturating_sub(used)) {
                used += estimate_tokens(&partial.text) + 1;
                clipped.push(format!(
                    "{}：保留 {}/{} 行，未注入起于「{}」",
                    section.title(),
                    partial.kept_lines,
                    partial.total_lines,
                    partial.first_omitted.as_deref().unwrap_or("-")
                ));
                kept.push((tier, section.title(), partial.text));
                continue;
            }
        }
        dropped.push(section.title());
    }
    Packed { kept, dropped, clipped, used }
}

/// 按需索引：常驻集之外的节不在上下文里，模型必须知道"去哪读"。
///
/// 每项 = (锚点, 展示文案)。锚点按**回读（前缀）**语义覆盖子节——索引说的是
/// "去读哪一段"，读到 `4.2` 当然连 `4.2.1` 一起读；这也让 [`warn_unindexed`]
/// 能证明每个未注入小节都有入口。
const ONDEMAND_INDEX: &[(&[&str], &str)] = &[
    (&["1."], "§1 新增工具/改协议/删文件"),
    (&["2.1"], "§2.1 逐路径要求"),
    (&["4.", "4.1"], "§4 必跑检查与密钥"),
    (&["4.2", "4.3"], "§4.2/4.3 断言与证据"),
    (&["4.4"], "§4.4 判据/接线"),
    (&["5.", "6.", "7.", "8.", "9."], "§5–§9 交付/依赖/例外"),
    // `11` 不带点：`11.` 加上 `1` 是 `11.1`，前缀匹配只认空格与点号边界，写 `11.` 覆盖不到子节。
    (&["10.", "11"], "§10–§11 方向/缝合"),
    (&["文档地图（终态）"], "文档地图"),
];

/// 按需索引正文。**节数必须由调用方数出来**：手写「17 节」会随 `AGENTS.md` 改动失真，
/// 而这个数字是印进模型正文的，错一个就是一句假话。
fn ondemand_note(total: usize) -> String {
    let entries =
        ONDEMAND_INDEX.iter().map(|(_, label)| *label).collect::<Vec<_>>().join("；");
    format!("（{total} 节未注入，规则不完整。命中即读 `AGENTS.md`：{entries}）")
}

/// 留痕的 token 上限 = 按需索引实测 94 + 25 的留白。
///
/// 留白留给"索引加一条触发句"；用例 `note_token_cap_has_headroom` 断言
/// `NOTE_TOKEN_CAP >= 实测 + 25`，改索引不涨 CAP 会先红。
const NOTE_TOKEN_CAP: usize = 119;

/// 尾部留痕：告诉模型还有几条规则没进来。
///
/// 不写"去读 <路径>"：提示词开头已经声明过「来源：…」，再说一遍只是重复；
/// 更重要的是模型必须知道**自己看到的不完整**，而不是"去哪找一个它够不到的地方"。
fn dropped_note(dropped: &[String]) -> String {
    let mut listed: Vec<&str> = Vec::new();
    for title in dropped {
        listed.push(title);
        if listed.len() > 1 && estimate_tokens(&render_note(dropped.len(), &listed)) > NOTE_TOKEN_CAP
        {
            listed.pop();
            break;
        }
    }
    render_note(dropped.len(), &listed)
}

fn render_note(total: usize, listed: &[&str]) -> String {
    if listed.len() >= total {
        format!("（另有 {total} 节规则因预算未注入，你看到的规则不完整：{}）", listed.join("、"))
    } else {
        format!(
            "（另有 {total} 节规则因预算未注入，你看到的规则不完整，例如：{}）",
            listed.join("、")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> crate::test_support::TestTempDir {
        crate::test_support::TestTempDir::new(&format!("rules-{name}"))
    }

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("创建父目录");
        }
        std::fs::write(path, text).expect("写规则文件");
    }

    fn repo_root() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("src-tauri 的上一级是仓库根")
    }

    /// 分节账单（`--nocapture` 可见）：断言失败时最需要这份账，
    /// 否则只能看到"某节不见了"而无从知道是谁挤掉的。
    fn report(rules: &ProjectRules) {
        let mut heading = String::from("（按需索引）");
        let mut cost = 0usize;
        for line in rules.text.lines() {
            // 小节渲染时标题降一级（`## x` → `### x`），嵌在「## 项目规则」之下。
            if let Some(title) = line.strip_prefix("### ") {
                eprintln!("[rules] {heading} → {cost} tokens");
                heading = title.trim().to_string();
                cost = 0;
            }
            cost += estimate_tokens(line) + 1;
        }
        eprintln!("[rules] {heading} → {cost} tokens");
        eprintln!(
            "[rules] 合计 {} tokens；未注入 {} 节：{}",
            estimate_tokens(&rules.text),
            rules.dropped.len(),
            rules.dropped.join("、")
        );
    }

    /// 抠出按需索引自己印的那个节数（正文里没有则 `None`）。
    fn printed_note_count(text: &str) -> Option<usize> {
        text.split('（')
            .find(|part| {
                part.starts_with(|c: char| c.is_ascii_digit()) && part.contains(" 节未注入")
            })
            .and_then(|part| part.split(' ').next())
            .and_then(|digits| digits.parse().ok())
    }

    #[test]
    fn missing_files_yield_empty_rules() {
        let dir = temp_dir("missing");
        let rules = discover(&dir);
        assert!(rules.is_empty(), "没有 AGENTS.md 时不得注入空标题");
        assert_eq!(rules.fingerprint, "-");
        assert!(rules.sources.is_empty());
    }

    #[test]
    fn inner_file_overrides_same_heading_from_outer() {
        // 外层（临时目录的父级）与内层（工作区）同名小节：内层内容必须覆盖外层，
        // 且外层独有小节仍然保留——这是"作用域优先级"，不是"只读最近一份"。
        let outer = temp_dir("override");
        let inner = outer.join("ws");
        std::fs::create_dir_all(&inner).expect("创建工作区");
        write(&outer.join(RULE_FILE_NAME), "## 铁律\n外层铁律\n\n## 提交\n外层提交\n");
        write(&inner.join(RULE_FILE_NAME), "## 铁律\n内层铁律\n");

        let rules = discover(&inner);
        assert!(rules.text.contains("内层铁律"), "同名小节必须取内层：{}", rules.text);
        assert!(!rules.text.contains("外层铁律"), "被覆盖的外层内容不得残留");
        assert!(rules.text.contains("外层提交"), "外层独有小节必须保留");
        assert_eq!(rules.sources.len(), 2, "两份文件都要记进来源");
    }

    #[test]
    fn must_sections_win_the_budget() {
        // 原则类小节排在前面并优先占预算；预算耗尽后其余小节**记标题**进 dropped。
        let dir = temp_dir("budget");
        let filler = "填充".repeat(400);
        let big = format!(
            "## 背景\n{filler}\n\n## 0. 铁律\n必须遵守\n\n## 附录\n{filler}\n"
        );
        write(&dir.join(RULE_FILE_NAME), &big);

        let rules = discover(&dir);
        assert!(rules.text.contains("必须遵守"), "原则类小节必须保留：{}", rules.text);
        assert!(rules.truncated, "超预算必须标记 truncated");
        assert!(
            rules.dropped.iter().any(|title| title.contains("附录")),
            "被裁掉的小节要记标题而不是计数：{:?}",
            rules.dropped
        );
        assert!(
            !rules.dropped.iter().any(|title| title.contains("铁律")),
            "必须保留的小节不得出现在 dropped：{:?}",
            rules.dropped
        );
        assert!(
            estimate_tokens(&rules.text) <= RULE_TOKEN_BUDGET,
            "注入正文不得超过预算：{} > {RULE_TOKEN_BUDGET}",
            estimate_tokens(&rules.text)
        );
        assert_eq!(rules.fingerprint.len(), 16, "指纹覆盖全文，长度固定");
    }

    #[test]
    fn oversized_principle_section_is_clipped_not_dropped() {
        // 唯一的原则类小节也超预算时，按行截断保留——"零规则"是最坏结果。
        let dir = temp_dir("clip");
        write(&dir.join(RULE_FILE_NAME), &format!("## 0. 铁律\n{}", "铁律一条\n".repeat(900)));
        let rules = discover(&dir);
        assert!(!rules.is_empty(), "唯一的原则类小节不得被整节丢弃：{}", rules.text);
        assert!(rules.text.contains("铁律一条"));
        assert!(rules.text.contains("已截断"));
        assert_eq!(rules.clipped.len(), 1, "截断要记标题：{:?}", rules.clipped);
        assert!(rules.dropped.is_empty(), "截断不等于丢弃：{:?}", rules.dropped);
        assert!(rules.truncated, "截断必须标记");
        assert!(
            estimate_tokens(&rules.text) <= RULE_TOKEN_BUDGET,
            "截断后仍须在预算内：{}",
            estimate_tokens(&rules.text)
        );
    }

    #[test]
    fn trailing_principle_section_is_not_starved_by_ordinary_sections() {
        // 回归用例（实测事故）：`11. 缝合原则` 排在文档最后，旧实现按文档顺序贪心装预算，
        // 前面几个普通小节把预算吃光，缝合原则整节消失。判定改成语义分层后，
        // 无论它排在哪里都必须进上下文（装不下就截断）。
        let dir = temp_dir("trailing-must");
        let filler = "背景说明".repeat(150); // 约 600 tokens
        let body = format!(
            "## 文档地图\n{filler}\n\n## 0. 铁律\n必须遵守\n\n## 11. 缝合原则\n{}\n",
            "缝合原则一条\n".repeat(400) // 约 2400 tokens，必然超预算
        );
        write(&dir.join(RULE_FILE_NAME), &body);

        let rules = discover(&dir);
        assert!(
            rules.text.contains("缝合原则"),
            "排在末尾的原则小节不得被普通小节挤掉：{}",
            rules.text
        );
        assert!(
            rules.clipped.iter().any(|title| title.contains("缝合原则")),
            "超预算的原则小节应以截断形式保留并记入 clipped：{:?}",
            rules.clipped
        );
        assert!(
            !rules.dropped.iter().any(|title| title.contains("缝合原则")),
            "dropped 里出现原则小节即为回归：{:?}",
            rules.dropped
        );
        assert!(
            rules.dropped.iter().any(|title| title.contains("文档地图")),
            "被挤掉的普通小节要记标题：{:?}",
            rules.dropped
        );
        assert!(
            estimate_tokens(&rules.text) <= RULE_TOKEN_BUDGET,
            "注入正文不得超过预算：{}",
            estimate_tokens(&rules.text)
        );
    }

    #[test]
    fn injected_rules_must_contain_iron_law_and_degradation_redline() {
        // 硬保险：仓库自己的 AGENTS.md 在真实工作区里被注入时，声明命中的每一节都必须
        // **逐字整节**出现在注入正文里，缺失即 fail（不是 warn）。
        //
        // 防的是"改分类器 / 改 AGENTS.md / 调预算"三者中的任何一个把红线弄丢：
        // 规则只写在文件里、模型上下文里没有，等于没有。判据落在**注入正文**上。
        let repo = repo_root();
        let rules = discover(repo);
        report(&rules);

        // 声明与标题都读**同一份文件**：测试里再抄一份标题清单，就多一处要同步的地方。
        let Declaration::Resident { anchors, .. } = rules.declaration.clone() else {
            panic!("仓库 AGENTS.md 必须带有效的常驻声明：{:?}", rules.declaration);
        };
        let file = std::fs::read_to_string(repo.join(RULE_FILE_NAME)).expect("读仓库 AGENTS.md");
        let sections = split_sections(&file);
        for anchor in &anchors {
            let section = sections
                .iter()
                .find(|section| matches_declaration(&section.heading, anchor))
                .unwrap_or_else(|| panic!("声明锚点 `{anchor}` 在 AGENTS.md 里找不到小节"));
            assert!(
                rules.text.contains(&section.render()),
                "常驻小节必须逐字完整注入（{anchor}）：\n{}",
                rules.text
            );
            assert!(
                !rules.clipped.iter().any(|title| title == &section.title()),
                "常驻小节不得被截断（{anchor}）：{:?}",
                rules.clipped
            );
        }

        // 两条最容易被挤掉的红线单独点名，诊断信息更具体。
        assert!(
            rules.text.contains("不可接受的变更"),
            "铁律原文必须完整出现在注入正文里（当前注入：\n{}）",
            rules.text
        );
        assert!(
            rules.text.contains("把「拒绝」改成「提示后放行」"),
            "11.3 退化红线必须逐字保留（当前注入：\n{}）",
            rules.text
        );
        // 有节被省略时，正文必须自己说"不完整"：静默截断会让模型把残缺当完整。
        assert!(rules.text.contains("规则不完整"), "未注入的节必须留痕：\n{}", rules.text);
        assert!(
            estimate_tokens(&rules.text) <= RULE_TOKEN_BUDGET,
            "注入正文不得超过预算：{} > {RULE_TOKEN_BUDGET}",
            estimate_tokens(&rules.text)
        );
    }

    #[test]
    fn dropped_titles_are_named_in_the_injected_text() {
        // 留痕：模型自己也要知道"还有规则没给到"，否则它会把残缺当完整。
        let dir = temp_dir("note");
        let filler = "填充".repeat(300);
        write(
            &dir.join(RULE_FILE_NAME),
            &format!("## 0. 铁律\n必须遵守\n\n## 背景\n{filler}\n\n## 附录\n{filler}\n"),
        );

        let rules = discover(&dir);
        assert!(!rules.dropped.is_empty(), "应至少丢掉一个小节：{}", rules.text);
        for title in &rules.dropped {
            assert!(
                rules.text.contains(title.as_str()),
                "未注入的小节标题必须在正文留痕：{title} 不在 {}",
                rules.text
            );
        }
    }

    #[test]
    fn omission_note_survives_a_fully_spent_budget() {
        // 留痕必须在"预算已经被 Must 吃满"时仍然进正文。旧实现是装完再试着塞，
        // 塞不进又没有一个非 Must 小节可以让位，于是退化成"只报数量"——模型看到
        // 的是一句"可能不全"，却不知道少了哪几条。
        // 断言落在**注入正文的内容**上，不是 `note_inlined` 这个内部状态。
        let dir = temp_dir("note-reserve");
        // 20 行 × 84 字 ≈ 1680 tokens：必须比"预算 − 留痕预留"更大，否则 Must 小节装得下、
        // 这个场景就不存在了（1100 预算下原来的 12 行刚好装得下，用例会退化成假绿）。
        let long: String = (0..20).map(|i| format!("第{i}行：{}\n", "甲".repeat(80))).collect();
        let mut doc = format!("## 0. 铁律\n必须遵守\n\n## 1. 安全边界\n{long}\n");
        // 附录的正文与标题都够长：留痕会被 NOTE_TOKEN_CAP 截到上限，
        // 于是"装完再塞"必然失败（残余空间装不下 CAP 那么长的留痕）。
        for i in 0..6 {
            doc.push_str(&format!("## 附录章节{i}号\n{}\n\n", "乙".repeat(200)));
        }
        write(&dir.join(RULE_FILE_NAME), &doc);

        let rules = discover(&dir);
        assert!(!rules.clipped.is_empty(), "构造的场景必须让 Must 小节被按行截断");
        assert_eq!(rules.dropped.len(), 6, "六个附录必须整节丢弃：{:?}", rules.dropped);
        assert!(
            rules.text.contains("规则不完整"),
            "预算吃满时留痕仍必须进正文：{}",
            rules.text
        );
        assert!(
            rules.text.contains(&rules.dropped[0]),
            "被省略的标题必须在正文里读得到：{}",
            rules.text
        );
        assert!(
            estimate_tokens(&rules.text) <= RULE_TOKEN_BUDGET,
            "留痕不能把总量顶破预算：{}",
            estimate_tokens(&rules.text)
        );
    }

    #[test]
    fn fingerprint_tracks_full_text_not_clipped_text() {
        // 预算裁剪后 text 可能相同，但指纹必须随原文变化——否则"内容锁"锁不住漂移。
        let dir = temp_dir("fingerprint");
        write(&dir.join(RULE_FILE_NAME), "## 铁律\n第一版\n");
        let first = discover(&dir);
        write(&dir.join(RULE_FILE_NAME), "## 铁律\n第二版\n");
        let second = discover(&dir);
        assert_ne!(first.fingerprint, second.fingerprint, "内容变化必须改变指纹");
    }

    #[test]
    fn declaration_matches_exactly_but_not_sub_sections() {
        // 声明是**预算分配**：`4.2` 多拉一节 `4.2.1`，账面就多一块钱，而且看不出来。
        assert!(matches_declaration("4.2 门禁响度（只许增，不许降）", "4.2"));
        assert!(!matches_declaration("4.2.1 事故与细则", "4.2"), "点号不是声明分隔符");
        assert!(matches_declaration("11.3 退化红线", "11.3"));
        assert!(matches_declaration("2. 受保护路径", "2."));
        assert!(!matches_declaration("2.1 逐路径要求与原因", "2."));
        assert!(!matches_declaration("4.20 不存在的节", "4.2"));
    }

    #[test]
    fn readback_matches_the_whole_subtree() {
        // 回读是**阅读范围**：读到 4.2 当然连 4.2.1 一起读。
        assert!(matches_readback("4.2", "4.2"));
        assert!(matches_readback("4.2 门禁响度", "4.2"));
        assert!(matches_readback("4.2.1 事故与细则", "4.2"));
        assert!(!matches_readback("4.3 证据自洽", "4.2"));
        assert!(matches_readback("11. 缝合原则（全文见 `docs/01` §2.4）", "11."));
        assert!(!matches_readback("文档地图（终态）", "文档"), "只认空格与点号边界");
    }

    #[test]
    fn absent_declaration_falls_back_to_tier_layers_verbatim() {
        // 反向对照之一：**删掉声明**必须回到旧分层，且逐字与旧版一致（无横幅、无索引）。
        let dir = temp_dir("absent");
        write(&dir.join(RULE_FILE_NAME), "## 0. 铁律\n必须遵守\n\n## 背景\n背景正文\n");
        let rules = discover(&dir);
        assert_eq!(rules.declaration, Declaration::Absent);
        assert_eq!(
            rules.text, "### 0. 铁律\n必须遵守\n\n### 背景\n背景正文",
            "无声明必须走旧分层，正文逐字不变"
        );
        assert!(rules.clipped.is_empty(), "没有取舍就不该有截断：{:?}", rules.clipped);
        assert!(!rules.note_inlined, "没有丢节就不该有留痕");
    }

    #[test]
    fn broken_declaration_falls_back_to_legacy_with_banner_on_first_line() {
        // 反向对照之二：**声明改坏**必须回退旧分层（不是空注入），且横幅独占正文首行。
        let cases = [
            ("<!-- shaniu-rules: resident 0.", "注释标签未闭合"),
            ("<!-- shaniu-rules: resident -->", "resident 为空"),
            ("<!-- shaniu-rules: resident 9.9.9 -->", "锚点一条都匹配不到"),
        ];
        for (declaration, kind) in cases {
            let dir = temp_dir("broken");
            write(&dir.join(RULE_FILE_NAME), &format!("{declaration}\n\n## 0. 铁律\n必须遵守\n"));
            let rules = discover(&dir);
            assert!(
                matches!(&rules.declaration, Declaration::Invalid { kind: got, .. } if *got == kind),
                "{declaration} 应判 Invalid({kind})：{:?}",
                rules.declaration
            );
            let first = rules.text.lines().next().unwrap_or_default();
            assert!(first.starts_with(DECLARATION_BANNER), "横幅必须独占正文首行：{first}");
            assert!(
                first.contains(kind) && first.contains("请修 AGENTS.md"),
                "横幅要指名坏在哪、怎么修：{first}"
            );
            assert!(rules.text.contains("必须遵守"), "坏声明不许空注入：{:?}", rules.text);
        }
    }

    #[test]
    fn declared_parent_does_not_drag_in_its_sub_sections() {
        // `## ` 与 `### ` 必须切成两节：否则声明 `4.2` 会把 `4.2.1` 细则一起拉进常驻。
        let dir = temp_dir("sub-sections");
        write(
            &dir.join(RULE_FILE_NAME),
            "<!-- shaniu-rules: resident 4.2 -->\n\n## 4. 证据\n总则\n\n\
             ### 4.2 门禁\n只许增\n\n### 4.2.1 细则\n事故\n",
        );
        let rules = discover(&dir);
        assert!(rules.text.contains("### 4.2 门禁"), "命中的小节必须在：{}", rules.text);
        assert!(
            !rules.text.contains("### 4.2.1 细则"),
            "声明 4.2 不得把 4.2.1 拉进常驻：{}",
            rules.text
        );
        assert_eq!(rules.dropped.len(), 2, "未注入的是 4. 与 4.2.1：{:?}", rules.dropped);
    }

    #[test]
    fn ondemand_note_count_matches_the_real_file() {
        // 「17 节」印在模型正文里，今天已经错过三次（13/14/17）：数字必须数出来，不是写出来的。
        let repo = repo_root();
        let rules = discover(repo);
        let file = std::fs::read_to_string(repo.join(RULE_FILE_NAME)).expect("读仓库 AGENTS.md");
        let sections = split_sections(&file);
        let Declaration::Resident { anchors, .. } = &rules.declaration else {
            panic!("仓库 AGENTS.md 必须带有效声明：{:?}", rules.declaration);
        };
        let resident = sections
            .iter()
            .filter(|section| {
                anchors.iter().any(|anchor| matches_declaration(&section.heading, anchor))
            })
            .count();
        let non_resident = sections.len() - resident;
        assert_eq!(
            non_resident,
            rules.dropped.len(),
            "未注入清单必须与文件对得上：{:?}",
            rules.dropped
        );
        assert_eq!(
            printed_note_count(&rules.text),
            Some(non_resident),
            "正文里印的节数必须等于实测：\n{}",
            rules.text
        );
    }

    #[test]
    fn note_token_cap_has_headroom() {
        let rules = discover(repo_root());
        let note = ondemand_note(rules.dropped.len());
        assert!(
            estimate_tokens(&note) + 25 <= NOTE_TOKEN_CAP,
            "索引实测 {} + 25 > CAP {NOTE_TOKEN_CAP}；加触发句要同时涨 CAP",
            estimate_tokens(&note)
        );
    }

    #[test]
    fn every_non_resident_section_is_reachable_through_the_index() {
        // 索引是"去哪读"的唯一入口：漏一节，模型就永远读不到那一节。
        // 判覆盖用**回读（前缀）**匹配：条目 `§4` 要覆盖 4.1、4.4。
        let repo = repo_root();
        let rules = discover(repo);
        let file = std::fs::read_to_string(repo.join(RULE_FILE_NAME)).expect("读仓库 AGENTS.md");
        let mut checked = 0;
        for section in split_sections(&file) {
            if rules.text.contains(&section.render()) {
                continue; // 常驻
            }
            assert!(
                ONDEMAND_INDEX.iter().any(|(anchors, _)| {
                    anchors.iter().any(|anchor| matches_readback(&section.heading, anchor))
                }),
                "「{}」不在按需索引里，模型读不到它",
                section.heading
            );
            checked += 1;
        }
        assert_eq!(checked, rules.dropped.len(), "覆盖检查必须走遍每一个未注入小节");
    }

    #[test]
    fn token_estimate_counts_cjk_as_one() {
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("中文四字"), 4);
        assert_eq!(estimate_tokens("ab中文"), 3);
    }
}
