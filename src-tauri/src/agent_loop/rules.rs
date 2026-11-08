//! 项目规则注入（AGENTS.md）：向上查找 + 作用域优先级 + 按标题语义分层 + token 预算。
//!
//! 事实来源是磁盘上的 `AGENTS.md`，不是内置文案：仓库自己写的规则必须能被模型看到，
//! 否则"规则写在仓库里、模型只收到系统提示"就是两套口径。
//!
//! 三条约束（docs/04 §6.1）：
//!  - **向上查找**：从工作区根出发逐级向上，直到盘符根；找到几份就收几份。
//!  - **作用域优先级**：外层在前、内层在后；同名二级标题以内层为准（内层覆盖外层）。
//!  - **预算**：按二级标题切分，总量 ≤ [`RULE_TOKEN_BUDGET`]；分层见 [`Tier`]。
//!    原则类小节必须进上下文（超预算按行截断），普通小节装不下则记标题进 `dropped`。
//!
//! 只读文件内容进提示词，不做执行、不做解释；文件读不到或超限只影响注入量，不阻断运行。

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// 规则文件名（向上查找时使用的固定名字）。
pub const RULE_FILE_NAME: &str = "AGENTS.md";

/// 注入正文的 token 预算（与 docs/04 §6.1 的"项目规则 ≤1000 tokens"同源）。
pub const RULE_TOKEN_BUDGET: usize = 1000;

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

fn tier_of(heading: Option<&str>) -> Tier {
    let Some(heading) = heading else { return Tier::Other };
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
    /// 因预算不足整节未注入的标题（按优先级顺序）。**必须记标题而不是计数**：
    /// 只有计数时，"重要条款被静默裁掉"在日志和注入正文里都看不出来。
    pub dropped: Vec<String>,
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
    /// 二级标题原文（不含 `## `）；文件开头到第一个二级标题之间的内容为 `None`。
    heading: Option<String>,
    body: String,
}

impl Section {
    /// 优先级只看标题语义；文件导语没有标题，按 [`Tier::Other`] 处理。
    fn tier(&self) -> Tier {
        tier_of(self.heading.as_deref())
    }

    /// 记日志/留痕用的标题名（导语给一个可辨认的占位名）。
    fn title(&self) -> String {
        self.heading.clone().unwrap_or_else(|| "（文首导语）".to_string())
    }

    /// 渲染成注入文本：节标题用三级标题，避免与系统提示自己的二级标题混淆。
    fn render(&self) -> String {
        match &self.heading {
            Some(heading) => format!("### {heading}\n{}", self.body.trim_end()),
            None => self.body.trim_end().to_string(),
        }
    }
}

/// 把一份文件切成小节：遇到行首 `## ` 开新节。
fn split_sections(text: &str) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    let mut heading: Option<String> = None;
    let mut body = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            if heading.is_some() || !body.trim().is_empty() {
                sections.push(Section { heading: heading.take(), body: std::mem::take(&mut body) });
            }
            heading = Some(rest.trim().to_string());
            continue;
        }
        body.push_str(line);
        body.push('\n');
    }
    if heading.is_some() || !body.trim().is_empty() {
        sections.push(Section { heading, body });
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
    for path in &files {
        let Some(text) = read_capped(path) else { continue };
        snapshot.push_str(&text);
        snapshot.push('\n');
        sources.push(path.clone());
        for section in split_sections(&text) {
            match merged
                .iter_mut()
                .find(|existing| existing.heading.is_some() && existing.heading == section.heading)
            {
                Some(existing) => existing.body = section.body,
                None => merged.push(section),
            }
        }
    }

    let fingerprint = format!("{:x}", Sha256::digest(snapshot.as_bytes()))
        .chars()
        .take(16)
        .collect::<String>();

    // 分层装配：Must → Guard → Other，同层保持文档顺序。
    // **位置不参与判定**：旧实现按文档顺序贪心装预算，`11. 缝合原则` 因为排在最后
    // 被前面的普通小节挤掉——规则写在文件里、模型上下文里没有，等于没有这条规则。
    let mut ordered: Vec<&Section> = Vec::with_capacity(merged.len());
    for tier in [Tier::Must, Tier::Guard, Tier::Other] {
        ordered.extend(merged.iter().filter(|section| section.tier() == tier));
    }

    // (层级, 标题, 渲染正文)：标题留着，是为了尾部留痕时能列出"哪些没进去"
    let mut kept: Vec<(Tier, String, String)> = Vec::new();
    let mut used = 0usize;
    let mut dropped: Vec<String> = Vec::new();
    let mut clipped: Vec<String> = Vec::new();

    for section in ordered {
        let rendered = section.render();
        let tier = section.tier();
        let cost = estimate_tokens(&rendered) + 1;
        if used + cost <= RULE_TOKEN_BUDGET {
            used += cost;
            kept.push((tier, section.title(), rendered));
            continue;
        }
        if tier == Tier::Must {
            // Must 小节宁可截断也不整节消失：模型看到半条规则，也好过看不到。
            if let Some(partial) = clip_lines(&rendered, RULE_TOKEN_BUDGET - used) {
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

    // 未注入的小节必须在正文里留痕：只写日志的话，模型仍然不知道"还有规则没给到"。
    // 留痕本身也占预算，装不下就从尾部剔除**非 Must** 小节（Must 一律不动）。
    let mut note_inlined = false;
    if !dropped.is_empty() {
        let where_to_read = sources
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("、");
        loop {
            let note = dropped_note(&where_to_read, &dropped);
            let cost = estimate_tokens(&note) + 1;
            if used + cost <= RULE_TOKEN_BUDGET {
                kept.push((Tier::Other, String::new(), note));
                note_inlined = true;
                break;
            }
            match kept.iter().rposition(|(tier, _, _)| *tier != Tier::Must) {
                Some(index) => {
                    let (_, title, rendered) = kept.remove(index);
                    used = used.saturating_sub(estimate_tokens(&rendered) + 1);
                    dropped.push(title);
                }
                // 只剩 Must 小节：留痕写不进去，标题交给 `dropped` 进日志/调用方
                None => break,
            }
        }
    }

    let text = kept
        .iter()
        .map(|(_, _, rendered)| rendered.trim_end())
        .collect::<Vec<_>>()
        .join("\n\n");

    ProjectRules {
        text,
        sources,
        fingerprint,
        truncated: !dropped.is_empty() || !clipped.is_empty(),
        dropped,
        clipped,
        note_inlined,
    }
}

/// 尾部留痕：告诉模型还有哪些小节因预算没进来、去哪里读。
fn dropped_note(where_to_read: &str, dropped: &[String]) -> String {
    format!(
        "（以下小节因预算未注入，需要时读取 {where_to_read}：{}）",
        dropped.join("、")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("shaniu-rules-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("创建父目录");
        }
        std::fs::write(path, text).expect("写规则文件");
    }

    #[test]
    fn missing_files_yield_empty_rules() {
        let dir = temp_dir("missing");
        let rules = discover(&dir);
        assert!(rules.is_empty(), "没有 AGENTS.md 时不得注入空标题");
        assert_eq!(rules.fingerprint, "-");
        assert!(rules.sources.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
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
        let _ = std::fs::remove_dir_all(&outer);
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
        let _ = std::fs::remove_dir_all(&dir);
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
        let _ = std::fs::remove_dir_all(&dir);
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
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn injected_rules_must_contain_iron_law_and_degradation_redline() {
        // 硬保险：仓库自己的 AGENTS.md 在真实工作区里被注入时，「0. 铁律」与
        // 「11.3 退化红线」必须**逐字**出现在注入正文里，缺失即 fail（不是 warn）。
        //
        // 这条测试防的是"改分类器 / 改 AGENTS.md / 调预算"三者中的任何一个把红线弄丢：
        // 规则只写在文件里、模型上下文里没有，等于没有；只留在日志的 dropped 计数里，
        // 也等于没有。判据落在**注入正文**上，不落在中间变量上。
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("src-tauri 的上一级是仓库根");
        let rules = discover(repo);

        assert!(
            rules.text.contains("### 0. 铁律") && rules.text.contains("不可接受的变更"),
            "铁律必须完整出现在注入正文里（当前注入：\n{}）",
            rules.text
        );
        assert!(
            rules.text.contains("### 11.3 退化红线")
                && rules.text.contains("把「拒绝」改成「提示后放行」"),
            "11.3 退化红线必须逐字保留（当前注入：\n{}）",
            rules.text
        );
        for title in ["铁律", "缝合原则", "受保护路径", "数据边界"] {
            assert!(
                !rules.dropped.iter().any(|dropped| dropped.contains(title)),
                "「{title}」被整节丢弃即回归：{:?}",
                rules.dropped
            );
        }
        assert!(
            estimate_tokens(&rules.text) <= RULE_TOKEN_BUDGET,
            "注入正文不得超过预算：{} > {RULE_TOKEN_BUDGET}",
            estimate_tokens(&rules.text)
        );

        // 诊断输出（`--nocapture` 时可见）：直接报"这 1000 tokens 装进了哪几节"，
        // 不必再靠外部脚本复刻分类器去猜。
        let mut heading = String::from("（文首导语）");
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
        let _ = std::fs::remove_dir_all(&dir);
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
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn token_estimate_counts_cjk_as_one() {
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("中文四字"), 4);
        assert_eq!(estimate_tokens("ab中文"), 3);
    }
}
