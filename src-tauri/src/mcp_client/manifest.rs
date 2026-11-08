//! 工具清单 → `ToolSpec` 转换与分发名分配（SH-061，docs/02 §3.1.3 B-R1~B-R4）。
//!
//! 三条不可协商的规则：
//! - **前缀不可绕过**：分发名固定 `mcp__<server_id>__<slug>`，`server_id` 取自用户在
//!   `config.toml` 填的 id，**不采用 server 自报名称**；
//! - **重名不覆盖**：同 slug 冲突时对**双端**各追加 `_<sha1(raw)[..12]>`（照 codex
//!   `codex-rs/codex-mcp/src/tools.rs:169-204`、`registry.rs:546-611`）。ZCode 是
//!   静默覆盖（`console.warn` 后直接 `set`），且 `a b` / `a_b` 归一会撞名，**不采用**；
//! - **先全量校验再提交**：任何一项超限/冲突都只影响计划结果，注册由调用方用
//!   `ToolRouter::try_register_all` 一次性提交；失败不留半套工具（B-R3）。
//!
//! 本模块只做纯转换，不启动进程、不注册、不做注入判定（后者见 [`super::guard`]）。

use serde_json::Value;
use sha1::{Digest, Sha1};
use unicode_normalization::UnicodeNormalization;

use crate::tool_router::ToolSpec;

use super::guard;
use super::session::RemoteTool;

/// 分发名固定前缀（B-R1）。内置工具不得占用（`tool_router` 已有回归测试）。
pub const DISPATCH_PREFIX: &str = "mcp__";
/// 分发名总长上限（OpenAI 工具名上限即 64，但 codex 取 128 以容纳双端哈希；这里对齐 codex）。
pub const MAX_DISPATCH_NAME_LEN: usize = 128;
/// 描述规范化后的字节上限（A-R3）。超限**拒绝注册**，不截断。
pub const MAX_DESCRIPTION_BYTES: usize = 2 * 1024;
/// Schema 嵌套深度上限（A-R3）。
pub const MAX_SCHEMA_DEPTH: usize = 8;
/// Schema 属性总数上限（A-R3）。
pub const MAX_SCHEMA_PROPERTIES: usize = 128;
/// Schema 中单个字符串的字节上限（A-R3）。
pub const MAX_SCHEMA_STRING_BYTES: usize = 4 * 1024;
/// 消歧短哈希长度：sha1 十六进制前 12 位（codex 同款）。
pub const SHORT_HASH_LEN: usize = 12;

/// 拒绝原因（全部可解释：说清是哪个字段、超了多少、允许多少）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    EmptyName,
    DescriptionTooLarge { bytes: usize },
    SchemaTooDeep { depth: usize },
    TooManyProperties { count: usize },
    SchemaStringTooLarge { bytes: usize },
    DuplicateRawName,
    AmbiguousName { candidate: String },
}

impl RejectReason {
    /// 面向用户/审计的说明。不做截断处理，超限就是拒绝。
    pub fn explain(&self) -> String {
        match self {
            RejectReason::EmptyName => "工具名为空".to_string(),
            RejectReason::DescriptionTooLarge { bytes } => format!(
                "描述规范化后 {bytes} 字节，超过 {MAX_DESCRIPTION_BYTES} 字节上限（拒绝注册而非截断）"
            ),
            RejectReason::SchemaTooDeep { depth } => {
                format!("参数 Schema 嵌套深度 {depth}，超过 {MAX_SCHEMA_DEPTH} 层上限")
            }
            RejectReason::TooManyProperties { count } => {
                format!("参数 Schema 属性总数 {count}，超过 {MAX_SCHEMA_PROPERTIES} 个上限")
            }
            RejectReason::SchemaStringTooLarge { bytes } => format!(
                "参数 Schema 中单个字符串 {bytes} 字节，超过 {MAX_SCHEMA_STRING_BYTES} 字节上限"
            ),
            RejectReason::DuplicateRawName => "服务器的工具清单里出现了重复的原始工具名".to_string(),
            RejectReason::AmbiguousName { candidate } => {
                format!("消歧后仍得到重复分发名 {candidate}（拒绝整份清单，不静默覆盖）")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedTool {
    pub raw_name: String,
    pub reason: RejectReason,
}

/// 冲突留痕（B-R4）：注册事件与审计用，双方原始名与最终分发名都要在。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collision {
    pub slug: String,
    pub raw_names: Vec<String>,
    pub dispatch_names: Vec<String>,
}

/// 一个通过校验、已分配分发名的工具。
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedTool {
    pub raw_name: String,
    pub dispatch_name: String,
    /// `spec.description` 已是规范化文本（检测看到的与模型看到的是同一份）。
    pub spec: ToolSpec,
    /// server 提供的原始描述：注入过滤器要在原文上判定，
    /// 否则被规范化剥掉的零宽/双向控制符就失去证据（A-R2）。
    pub raw_description: String,
    /// server 自报的注解，仅用于展示与变更检测（ADR-107：**不参与风险决策**）。
    pub annotations: Option<Value>,
}

/// 一次清单转换的完整结果（尚未写入注册表）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RegistrationPlan {
    pub accepted: Vec<PlannedTool>,
    pub rejected: Vec<RejectedTool>,
    pub collisions: Vec<Collision>,
}

impl RegistrationPlan {
    pub fn is_empty(&self) -> bool {
        self.accepted.is_empty() && self.rejected.is_empty()
    }
}

/// 把 server 的原始清单转成待注册计划。不修改任何全局状态。
pub fn plan_registration(server_id: &str, tools: &[RemoteTool]) -> RegistrationPlan {
    let mut plan = RegistrationPlan::default();

    // 1. 逐项结构校验（名称/描述/Schema）；重名在下一步统一处理。
    let mut candidates: Vec<(usize, String, String, String, Value, Option<Value>)> = Vec::new();
    let mut raw_counts: Vec<(String, usize)> = Vec::new();
    for (index, tool) in tools.iter().enumerate() {
        let raw_name = tool.name.trim().to_string();
        if raw_name.is_empty() {
            plan.rejected.push(RejectedTool {
                raw_name: String::new(),
                reason: RejectReason::EmptyName,
            });
            continue;
        }
        let normalized = guard::normalize(&tool.description);
        if normalized.text.len() > MAX_DESCRIPTION_BYTES {
            plan.rejected.push(RejectedTool {
                raw_name,
                reason: RejectReason::DescriptionTooLarge { bytes: normalized.text.len() },
            });
            continue;
        }
        if let Err(reason) = validate_schema(&tool.input_schema) {
            plan.rejected.push(RejectedTool { raw_name, reason });
            continue;
        }
        match raw_counts.iter_mut().find(|(name, _)| name == &raw_name) {
            Some((_, count)) => *count += 1,
            None => raw_counts.push((raw_name.clone(), 1)),
        }
        candidates.push((
            index,
            raw_name,
            normalized.text,
            tool.description.clone(),
            tool.input_schema.clone(),
            tool.annotations.clone(),
        ));
    }

    // 2. 原始名重复：映射表必须单射，否则 UI 上的「原始名 ↔ 分发名」无法回指。
    let duplicates: Vec<String> = raw_counts
        .iter()
        .filter(|(_, count)| *count > 1)
        .map(|(name, _)| name.clone())
        .collect();
    candidates.retain(|(_, raw_name, _, _, _, _)| {
        if duplicates.contains(raw_name) {
            plan.rejected.push(RejectedTool {
                raw_name: raw_name.clone(),
                reason: RejectReason::DuplicateRawName,
            });
            false
        } else {
            true
        }
    });

    // 3. 基线分发名分组；冲突组双端各加短哈希。
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    for (position, (_, raw_name, _, _, _, _)) in candidates.iter().enumerate() {
        let candidate = assemble(server_id, &slugify(raw_name), None);
        match groups.iter_mut().find(|(slug, _)| slug == &candidate) {
            Some((_, members)) => members.push(position),
            None => groups.push((candidate, vec![position])),
        }
    }

    let mut assigned: Vec<Option<String>> = vec![None; candidates.len()];
    for (candidate, members) in &groups {
        if members.len() == 1 {
            let position = members[0];
            assigned[position] = Some(candidate.clone());
            continue;
        }
        let mut dispatch_names = Vec::new();
        for position in members {
            let raw_name = &candidates[*position].1;
            let suffixed = assemble(
                server_id,
                &slugify(raw_name),
                Some(&format!("_{}", short_hash(raw_name))),
            );
            dispatch_names.push(suffixed.clone());
            assigned[*position] = Some(suffixed);
        }
        plan.collisions.push(Collision {
            slug: candidate.clone(),
            raw_names: members.iter().map(|index| candidates[*index].1.clone()).collect(),
            dispatch_names,
        });
    }

    // 4. 消歧后仍重复（例如原始名相同但大小写/归一化不同）：拒绝该组，绝不覆盖。
    let mut final_counts: Vec<(String, usize)> = Vec::new();
    for name in assigned.iter().flatten() {
        match final_counts.iter_mut().find(|(key, _)| key == name) {
            Some((_, count)) => *count += 1,
            None => final_counts.push((name.clone(), 1)),
        }
    }

    for (position, (_, raw_name, description, raw_description, schema, annotations)) in
        candidates.into_iter().enumerate()
    {
        let dispatch_name = assigned[position].clone().unwrap_or_default();
        if final_counts
            .iter()
            .any(|(name, count)| name == &dispatch_name && *count > 1)
        {
            plan.rejected.push(RejectedTool {
                raw_name,
                reason: RejectReason::AmbiguousName { candidate: dispatch_name },
            });
            continue;
        }
        plan.accepted.push(PlannedTool {
            raw_name: raw_name.clone(),
            dispatch_name: dispatch_name.clone(),
            spec: ToolSpec::new(&dispatch_name, &description, schema),
            raw_description,
            annotations,
        });
    }

    // 稳定排序：声明顺序不应随 HashMap/分组顺序漂移。
    plan.accepted.sort_by(|a, b| a.dispatch_name.cmp(&b.dispatch_name));
    plan.rejected.sort_by(|a, b| a.raw_name.cmp(&b.raw_name));
    plan.collisions.sort_by(|a, b| a.slug.cmp(&b.slug));
    plan
}

/// 把「容器内 `dir` 可能静默失败」写进工具描述（**只在 AppContainer 模式下调用**）。
///
/// 已通过的 server 里，模型仍可能让 server 或者自己用 shell 内建列目录；
/// 容器里这类命令可能既无输出也无报错。工具描述是模型唯一一定看得到的地方，
/// 因此在计划阶段统一追加一条，不额外新增工具、不改协议。
pub fn append_container_hint(plan: &mut RegistrationPlan) {
    const HINT: &str = "\n\n[SHANIU 沙箱] 该 server 运行在 AppContainer 内：\
                        列目录请使用文件类工具，不要依赖 shell 内建的 `dir`（容器里可能无输出）。";
    for tool in &mut plan.accepted {
        tool.spec.description.push_str(HINT);
    }
}

/// 原始工具名 → slug：NFKC → 小写 → 非 `[a-z0-9_]` 映射 `_` → 折叠连续下划线 → 去首尾。
pub fn slugify(raw_name: &str) -> String {
    let folded: String = raw_name.nfkc().collect();
    let mut out = String::with_capacity(folded.len());
    let mut last_underscore = false;
    for ch in folded.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            out.push(ch);
            last_underscore = false;
        } else if !last_underscore {
            out.push('_');
            last_underscore = true;
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "tool".to_string()
    } else {
        trimmed.to_string()
    }
}

/// sha1(raw)[..12]，小写十六进制。
pub fn short_hash(raw_name: &str) -> String {
    let digest = Sha1::digest(raw_name.as_bytes());
    let mut hex = String::with_capacity(40);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    hex[..SHORT_HASH_LEN].to_string()
}

/// 组装分发名，超长时截断 slug（哈希后缀永不截断）。
fn assemble(server_id: &str, slug: &str, suffix: Option<&str>) -> String {
    let prefix = format!("{DISPATCH_PREFIX}{server_id}__");
    let suffix = suffix.unwrap_or("");
    let budget = MAX_DISPATCH_NAME_LEN
        .saturating_sub(prefix.len())
        .saturating_sub(suffix.len());
    let slug: String = slug.chars().take(budget.max(1)).collect();
    format!("{prefix}{slug}{suffix}")
}

/// Schema 结构上限（A-R3）：深度、属性总数、单字符串字节数。
pub fn validate_schema(schema: &Value) -> Result<(), RejectReason> {
    let mut properties = 0usize;
    walk_schema(schema, 1, &mut properties)?;
    if properties > MAX_SCHEMA_PROPERTIES {
        return Err(RejectReason::TooManyProperties { count: properties });
    }
    Ok(())
}

fn walk_schema(value: &Value, depth: usize, properties: &mut usize) -> Result<(), RejectReason> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(RejectReason::SchemaTooDeep { depth });
    }
    match value {
        Value::String(text) => {
            if text.len() > MAX_SCHEMA_STRING_BYTES {
                return Err(RejectReason::SchemaStringTooLarge { bytes: text.len() });
            }
        }
        Value::Array(items) => {
            for item in items {
                walk_schema(item, depth + 1, properties)?;
            }
        }
        Value::Object(map) => {
            for (key, child) in map {
                if key.len() > MAX_SCHEMA_STRING_BYTES {
                    return Err(RejectReason::SchemaStringTooLarge { bytes: key.len() });
                }
                if key == "properties" {
                    if let Value::Object(entries) = child {
                        *properties += entries.len();
                    }
                }
                walk_schema(child, depth + 1, properties)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str, description: &str) -> RemoteTool {
        RemoteTool {
            name: name.to_string(),
            description: description.to_string(),
            input_schema: json!({"type": "object", "properties": {}}),
            annotations: None,
        }
    }

    #[test]
    fn container_hint_is_appended_only_to_accepted_tools() {
        let mut plan = plan_registration("srv", &[tool("read_file", "read a file")]);
        append_container_hint(&mut plan);
        let accepted = &plan.accepted[0].spec.description;
        assert!(accepted.starts_with("read a file"));
        assert!(accepted.contains("dir"), "提示必须点名 dir: {accepted}");
        assert!(accepted.contains("AppContainer"));
        // 被注入检测拒绝的工具不会出现给模型，也不该被改写（这里直接构造一条）。
        plan.rejected.push(RejectedTool {
            raw_name: "bad".into(),
            reason: RejectReason::EmptyName,
        });
        let before: Vec<usize> = plan.rejected.iter().map(|tool| tool.raw_name.len()).collect();
        append_container_hint(&mut plan);
        let after: Vec<usize> = plan.rejected.iter().map(|tool| tool.raw_name.len()).collect();
        assert_eq!(before, after, "拒绝清单不受提示影响");
    }

    #[test]
    fn dispatch_name_uses_configured_server_id_not_self_reported() {
        let plan = plan_registration("filesystem", &[tool("read_file", "read a file")]);
        assert_eq!(plan.accepted.len(), 1);
        assert_eq!(plan.accepted[0].dispatch_name, "mcp__filesystem__read_file");
        assert!(plan.accepted[0].dispatch_name.starts_with(DISPATCH_PREFIX));
    }

    #[test]
    fn builtin_lookalike_never_escapes_namespace() {
        let plan = plan_registration("evil", &[tool("unified_exec", "x")]);
        assert_eq!(plan.accepted[0].dispatch_name, "mcp__evil__unified_exec");
        assert_ne!(plan.accepted[0].dispatch_name, "unified_exec");
    }

    #[test]
    fn slug_normalizes_case_space_and_unicode() {
        assert_eq!(slugify("Read File"), "read_file");
        assert_eq!(slugify("a  b"), "a_b");
        assert_eq!(slugify("A_B"), "a_b");
        assert_eq!(slugify("运行 命令"), "tool");
        assert_eq!(slugify(""), "tool");
    }

    #[test]
    fn same_slug_gets_dual_short_hash_and_is_stable() {
        let tools = [tool("a b", "first"), tool("a_b", "second")];
        let first = plan_registration("srv", &tools);
        let second = plan_registration("srv", &tools);

        assert!(first.rejected.is_empty());
        assert_eq!(first.collisions.len(), 1, "冲突必须留痕");
        let names: Vec<&str> = first.accepted.iter().map(|t| t.dispatch_name.as_str()).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&format!("mcp__srv__a_b_{}", short_hash("a b")).as_str()));
        assert!(names.contains(&format!("mcp__srv__a_b_{}", short_hash("a_b")).as_str()));
        // 双端消歧必须稳定：同样的输入重复转换得到同样的名字。
        assert_eq!(
            first.accepted.iter().map(|t| t.dispatch_name.clone()).collect::<Vec<_>>(),
            second.accepted.iter().map(|t| t.dispatch_name.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn duplicate_raw_names_are_rejected_not_merged() {
        let plan = plan_registration("srv", &[tool("dup", "a"), tool("dup", "b")]);
        assert!(plan.accepted.is_empty());
        assert!(plan
            .rejected
            .iter()
            .all(|tool| tool.reason == RejectReason::DuplicateRawName));
    }

    #[test]
    fn dispatch_name_is_capped_at_128_chars_and_keeps_hash_suffix() {
        let long = "x".repeat(300);
        let plan = plan_registration("srv", &[tool(&long, "a"), tool(&format!("{long}_y"), "b")]);
        assert!(plan.rejected.is_empty());
        for accepted in &plan.accepted {
            assert!(
                accepted.dispatch_name.len() <= MAX_DISPATCH_NAME_LEN,
                "分发名超长: {} ({})",
                accepted.dispatch_name,
                accepted.dispatch_name.len()
            );
            // 冲突组的哈希后缀不允许被截掉，否则消歧失效。
            assert!(accepted.dispatch_name.rsplit('_').next().unwrap().len() == SHORT_HASH_LEN);
        }
    }

    #[test]
    fn oversized_description_is_rejected_not_truncated() {
        let plan = plan_registration("srv", &[tool("big", &"字".repeat(MAX_DESCRIPTION_BYTES))]);
        assert!(plan.accepted.is_empty());
        assert!(matches!(
            plan.rejected[0].reason,
            RejectReason::DescriptionTooLarge { .. }
        ));
        assert!(plan.rejected[0].reason.explain().contains("拒绝注册而非截断"));
    }

    #[test]
    fn deep_or_wide_schema_is_rejected() {
        let mut deep = json!({"type": "object"});
        for _ in 0..MAX_SCHEMA_DEPTH {
            deep = json!({"type": "object", "properties": {"a": deep}});
        }
        let plan = plan_registration("srv", &[RemoteTool {
            name: "deep".into(),
            description: "d".into(),
            input_schema: deep,
            annotations: None,
        }]);
        assert!(matches!(plan.rejected[0].reason, RejectReason::SchemaTooDeep { .. }));

        let mut properties = serde_json::Map::new();
        for index in 0..=MAX_SCHEMA_PROPERTIES {
            properties.insert(format!("p{index}"), json!({"type": "string"}));
        }
        let wide = json!({"type": "object", "properties": properties});
        let plan = plan_registration("srv", &[RemoteTool {
            name: "wide".into(),
            description: "d".into(),
            input_schema: wide,
            annotations: None,
        }]);
        assert!(matches!(plan.rejected[0].reason, RejectReason::TooManyProperties { .. }));
    }

    #[test]
    fn description_is_normalized_before_reaching_the_model() {
        let plan = plan_registration("srv", &[tool("clean", "read\u{200B} a file")]);
        let description = &plan.accepted[0].spec.description;
        assert!(!description.contains('\u{200B}'), "零宽字符不得进入 ToolSpec");
        assert_eq!(description, "read a file");
    }
}
