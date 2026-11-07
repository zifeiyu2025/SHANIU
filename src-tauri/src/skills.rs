//! 技能系统（SH-091）：发现、解析、加载与渲染 `SKILL.md`。
//!
//! 对齐上游 `packages/skill/{skill,skill-filesystem,tool-skill}` 的可观察行为：
//!
//! - **发现根**（rank 小者优先，同名去重）：
//!   - 项目根（`workspace_root` 最近含 `.git` 的祖先）下 `.dsh/skills`（100）
//!   - 项目根下 `.agents/skills`（200）
//!   - 用户 `~/.dsh/skills`（400，`DSH_HOME` 可覆盖）
//!   - 用户 `~/.agents/skills`（500，`DSH_AGENTS_HOME` 可覆盖）
//! - **两种布局**：目录包 `<dir>/SKILL.md`；扁平 `<root>/*.md`。
//! - **frontmatter**：`name`/`description` 必填且 name 为 kebab-case；`whenToUse` 可选；
//!   `disable-model-invocation`（模型不可见）与 `user-invocable`（默认 true）控制入口；
//!   旧键 `disableModelInvocation`/`modelInvocable`/`userInvocable` 一律拒绝。
//! - **渲染**：`render_skill_content` 逐字对齐上游 `<skill_content>` 包装，工具结果与
//!   用户 `/name` 注入共用同一形状。
//! - **用户显式触发**：扫描本轮新增用户文本中的 `/name`（与前端芯片同一词法），
//!   对 user-invocable 技能注入渲染后的正文；未知名字保持普通文本。
//!
//! 与上游的**有意偏差**：上游把模型目录作为 durable user message 发布，本实现把目录
//! 放进系统提示（每轮本来就重发），避免向持久历史塞入伪造的 user 轮次；目录为空时
//! 整段省略，且不声明 `skill` 工具（见 `agent_loop::declarations_for`）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::model_client::{HistoryItem, Role};

/// 项目 `.dsh/skills` 的优先级。
pub const PROJECT_DSH_RANK: i32 = 100;
/// 项目 `.agents/skills` 的优先级。
pub const PROJECT_AGENTS_RANK: i32 = 200;
/// 用户 `~/.dsh/skills` 的优先级。
pub const USER_DSH_RANK: i32 = 400;
/// 用户 `~/.agents/skills` 的优先级。
pub const USER_AGENTS_RANK: i32 = 500;

/// 技能名合法字符集：小写 kebab-case（与上游 `SKILL_NAME` 一致）。
fn is_skill_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let mut segment_len = 0usize;
    for ch in name.chars() {
        if ch == '-' {
            if segment_len == 0 {
                return false;
            }
            segment_len = 0;
        } else if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            segment_len += 1;
        } else {
            return false;
        }
    }
    segment_len > 0
}

/// 一条技能摘要（尚未加载正文）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillSummary {
    pub name: String,
    /// 指令文件的绝对路径；虚拟技能为 None。
    pub path: Option<PathBuf>,
    pub description: String,
    pub when_to_use: Option<String>,
    pub model_invocable: bool,
    pub user_invocable: bool,
    /// 资源基准目录（用于相对路径提示）。
    pub directory: PathBuf,
    /// 来源优先级；数值越小越优先。
    pub rank: i32,
}

/// 已加载正文的技能。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDefinition {
    pub summary: SkillSummary,
    pub content: String,
}

/// 宿主 `list_skills` 的 wire 形状，逐字对齐上游 `SkillEntry`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillEntry {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub description: String,
    #[serde(rename = "whenToUse", skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    #[serde(rename = "modelInvocable")]
    pub model_invocable: bool,
}

/// 把内部摘要投影成 wire 形状（只保留 human-facing 字段）。
pub fn to_entry(skill: &SkillSummary) -> SkillEntry {
    SkillEntry {
        name: skill.name.clone(),
        path: skill.path.as_ref().map(|path| path.display().to_string()),
        description: skill.description.clone(),
        when_to_use: skill.when_to_use.clone(),
        model_invocable: skill.model_invocable,
    }
}

// ============================== 发现 ==============================

/// 一个发现根。
struct SkillRoot {
    path: PathBuf,
    rank: i32,
}

/// 从 `workspace_root` 向上找最近含 `.git` 的祖先；找不到则用 `cwd` 本身。
fn find_project_root(cwd: &Path) -> PathBuf {
    let mut current = cwd.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return current;
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => return cwd.to_path_buf(),
        }
    }
}

/// 解析用户 home 下的覆盖环境变量。
fn resolve_home(configured_env: &str, dir_name: &str) -> PathBuf {
    if let Some(value) = std::env::var_os(configured_env) {
        let path = PathBuf::from(value);
        if !path.as_os_str().is_empty() {
            return path;
        }
    }
    crate::config::home_dir().join(dir_name)
}

fn roots(workspace_root: &Path) -> Vec<SkillRoot> {
    let project_root = find_project_root(workspace_root);
    vec![
        SkillRoot {
            path: project_root.join(".dsh").join("skills"),
            rank: PROJECT_DSH_RANK,
        },
        SkillRoot {
            path: project_root.join(".agents").join("skills"),
            rank: PROJECT_AGENTS_RANK,
        },
        SkillRoot {
            path: resolve_home("DSH_HOME", ".dsh").join("skills"),
            rank: USER_DSH_RANK,
        },
        SkillRoot {
            path: resolve_home("DSH_AGENTS_HOME", ".agents").join("skills"),
            rank: USER_AGENTS_RANK,
        },
    ]
}

/// 列出根下的候选指令文件：目录包取 `SKILL.md`，扁平 `.md` 取文件本身。
fn candidate_files(root: &Path) -> Vec<(PathBuf, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<(PathBuf, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            let file = path.join("SKILL.md");
            if file.is_file() {
                out.push((file, path));
            }
        } else if metadata.is_file()
            && path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
        {
            out.push((path.clone(), root.to_path_buf()));
        }
    }
    // 固定顺序，避免文件系统枚举顺序造成跨平台漂移。
    out.sort_by(|left, right| left.0.cmp(&right.0));
    out
}

/// 解析一个技能文件；任何格式错误返回 None 并记录警告。
fn parse_skill_file(path: &Path, directory: &Path, rank: i32) -> Option<SkillSummary> {
    let raw = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            tracing::warn!("skill file {} ignored: cannot read: {error}", path.display());
            return None;
        }
    };
    let (data, _body) = match parse_frontmatter(&raw) {
        Ok(Some(parsed)) => parsed,
        Ok(None) => {
            tracing::warn!(
                "skill file {} ignored: missing YAML frontmatter",
                path.display()
            );
            return None;
        }
        Err(error) => {
            tracing::warn!(
                "skill file {} ignored: invalid YAML frontmatter: {error}",
                path.display()
            );
            return None;
        }
    };
    let name = string_field(&data, "name")?;
    let description = string_field(&data, "description")?;
    if !is_skill_name(&name) {
        tracing::warn!(
            "skill file {} ignored: invalid skill name {:?}",
            path.display(),
            name
        );
        return None;
    }
    let invocation = match parse_invocation(&data) {
        Ok(policy) => policy,
        Err(error) => {
            tracing::warn!(
                "skill file {} ignored: invalid invocation frontmatter: {error}",
                path.display()
            );
            return None;
        }
    };
    Some(SkillSummary {
        name,
        path: Some(path.to_path_buf()),
        description,
        when_to_use: string_field(&data, "whenToUse"),
        model_invocable: invocation.0,
        user_invocable: invocation.1,
        directory: directory.to_path_buf(),
        rank,
    })
}

/// 按给定根集合发现技能，按 name 排序；同名保留 rank 最小者（同 rank 先发现者）。
///
/// 与 `discover` 分离：测试只传项目根，避免结果受开发机 `~/.dsh/skills`
/// 等真实用户目录影响（否则单测随环境漂移）。
fn discover_in(roots: &[SkillRoot]) -> Vec<SkillSummary> {
    let mut by_name: HashMap<String, SkillSummary> = HashMap::new();
    for root in roots {
        for (file, directory) in candidate_files(&root.path) {
            let Some(summary) = parse_skill_file(&file, &directory, root.rank) else {
                continue;
            };
            match by_name.get(&summary.name) {
                Some(existing) if existing.rank <= summary.rank => {}
                _ => {
                    by_name.insert(summary.name.clone(), summary);
                }
            }
        }
    }
    let mut skills: Vec<SkillSummary> = by_name.into_values().collect();
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    skills
}

/// 发现全部技能（项目根 + 用户根）。
pub fn discover(workspace_root: &Path) -> Vec<SkillSummary> {
    discover_in(&roots(workspace_root))
}

/// 发现并加载一个技能正文。
pub fn load(workspace_root: &Path, name: &str) -> Option<SkillDefinition> {
    let summary = discover(workspace_root)
        .into_iter()
        .find(|skill| skill.name == name)?;
    let path = summary.path.as_ref()?;
    let raw = std::fs::read_to_string(path).ok()?;
    let (_, body) = parse_frontmatter(&raw).ok()??;
    Some(SkillDefinition {
        content: body.trim().to_string(),
        summary,
    })
}

// ============================== frontmatter ==============================

type Frontmatter = (serde_yaml_ng::Value, String);

/// 解析 `---\n...\n---\nbody`。缺失返回 Ok(None)，YAML 错误返回 Err。
fn parse_frontmatter(raw: &str) -> Result<Option<Frontmatter>, String> {
    let first_line_end = match raw.find('\n') {
        Some(index) => index,
        None => return Ok(None),
    };
    if raw[..first_line_end].trim_end_matches('\r') != "---" {
        return Ok(None);
    }
    let start = first_line_end + 1;
    let mut line_start = start;
    while line_start <= raw.len() {
        let next_newline = raw[line_start..].find('\n').map(|offset| line_start + offset);
        let line_end = next_newline.unwrap_or(raw.len());
        let line = raw[line_start..line_end].trim_end_matches('\r');
        if line == "---" {
            let yaml = &raw[start..line_start];
            let value: serde_yaml_ng::Value =
                serde_yaml_ng::from_str(yaml).map_err(|error| error.to_string())?;
            if !value.is_mapping() {
                return Ok(None);
            }
            let body_start = next_newline.map(|index| index + 1).unwrap_or(raw.len());
            return Ok(Some((value, raw[body_start..].to_string())));
        }
        match next_newline {
            Some(index) => line_start = index + 1,
            None => break,
        }
    }
    Ok(None)
}

fn string_field(data: &serde_yaml_ng::Value, key: &str) -> Option<String> {
    data.get(key)
        .and_then(serde_yaml_ng::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn has_key(data: &serde_yaml_ng::Value, key: &str) -> bool {
    data.get(key).is_some()
}

fn reject_legacy(data: &serde_yaml_ng::Value, legacy: &str, canonical: &str) -> Result<(), String> {
    if has_key(data, legacy) {
        return Err(format!(
            "frontmatter field \"{legacy}\" is unsupported; use \"{canonical}\""
        ));
    }
    Ok(())
}

/// `(model_invocable, user_invocable)`
fn parse_invocation(data: &serde_yaml_ng::Value) -> Result<(bool, bool), String> {
    reject_legacy(data, "disableModelInvocation", "disable-model-invocation")?;
    reject_legacy(data, "modelInvocable", "disable-model-invocation")?;
    reject_legacy(data, "userInvocable", "user-invocable")?;
    let disable_model = frontmatter_boolean(data, "disable-model-invocation")?;
    let user_invocable = frontmatter_boolean(data, "user-invocable")?;
    Ok((disable_model != Some(true), user_invocable != Some(false)))
}

fn frontmatter_boolean(data: &serde_yaml_ng::Value, key: &str) -> Result<Option<bool>, String> {
    let Some(value) = data.get(key) else {
        return Ok(None);
    };
    if let Some(boolean) = value.as_bool() {
        return Ok(Some(boolean));
    }
    if let Some(number) = value.as_i64() {
        return match number {
            1 => Ok(Some(true)),
            0 => Ok(Some(false)),
            _ => Err(format!("frontmatter field \"{key}\" must be a boolean")),
        };
    }
    if let Some(text) = value.as_str() {
        return match text.to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Ok(Some(true)),
            "false" | "no" | "off" | "0" => Ok(Some(false)),
            _ => Err(format!("frontmatter field \"{key}\" must be a boolean")),
        };
    }
    Err(format!("frontmatter field \"{key}\" must be a boolean"))
}

// ============================== 渲染 ==============================

fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
}

fn escape_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// 逐字对齐上游 `renderSkillContent`：工具结果与 `/name` 注入共用。
pub fn render_skill_content(skill: &SkillDefinition) -> String {
    let base = skill.summary.directory.display().to_string();
    [
        format!("<skill_content name=\"{}\">", escape_attr(&skill.summary.name)),
        "<skill_resources>".to_string(),
        format!("Base directory for this skill: {}", escape_text(&base)),
        "Resolve relative paths mentioned by this skill against the base directory before using them. Load referenced resources only as needed.".to_string(),
        "</skill_resources>".to_string(),
        String::new(),
        "<skill_instructions>".to_string(),
        skill.content.clone(),
        "</skill_instructions>".to_string(),
        "</skill_content>".to_string(),
    ]
    .join("\n")
}

// ============================== 用户 `/name` 触发 ==============================

/// 与上游 `SKILL_GESTURE` 同一词法：空白边界内的 `/kebab-case`。
fn gesture_names(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        let boundary = index == 0
            || bytes[index - 1].is_ascii_whitespace();
        if boundary && bytes[index] == b'/' {
            let start = index + 1;
            let mut end = start;
            while end < bytes.len()
                && (bytes[end].is_ascii_lowercase()
                    || bytes[end].is_ascii_digit()
                    || bytes[end] == b'-')
            {
                end += 1;
            }
            let name = &text[start..end];
            let trailing = end == bytes.len() || bytes[end].is_ascii_whitespace();
            if trailing && is_skill_name(name) && !names.iter().any(|seen| seen == name) {
                names.push(name.to_string());
            }
            index = end.max(index + 1);
            continue;
        }
        index += 1;
    }
    names
}

/// 只扫描历史末尾连续的用户消息（本轮新增输入），与上游 pre-step 的 claimed batch 同义。
fn trailing_user_messages(history: &[HistoryItem]) -> &[HistoryItem] {
    let start = history
        .iter()
        .rposition(|item| !matches!(item, HistoryItem::Message { role: Role::User, .. }))
        .map(|index| index + 1)
        .unwrap_or(0);
    &history[start..]
}

fn message_text(item: &HistoryItem) -> Option<&str> {
    match item {
        HistoryItem::Message { role: Role::User, text, .. } => Some(text),
        _ => None,
    }
}

/// 计算本轮用户显式触发的技能注入消息；没有触发返回空。
///
/// 单独暴露给 `agent_loop`：会话引用展开会在末尾追加一条 user 上下文，
/// 若在那之后再扫描就会把被引用会话里的 `/name` 当成用户手势。这里只扫描
/// **原始历史末尾连续的用户消息**，与上游 pre-step 的 claimed batch 同义。
pub fn injections(history: &[HistoryItem], workspace_root: &Path) -> Vec<HistoryItem> {
    let trailing = trailing_user_messages(history);
    let mut names: Vec<String> = Vec::new();
    for item in trailing {
        let Some(text) = message_text(item) else {
            continue;
        };
        for name in gesture_names(text) {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    let mut injections: Vec<HistoryItem> = Vec::new();
    for name in names {
        let Some(skill) = load(workspace_root, &name) else {
            continue;
        };
        if !skill.summary.user_invocable {
            continue;
        }
        injections.push(HistoryItem::Message {
            role: Role::User,
            text: render_skill_content(&skill),
            reasoning: None,
        });
    }
    injections
}

/// 为本轮请求追加用户显式触发的技能正文。没有触发返回 Ok(None)。
pub fn prepare_request(
    history: &[HistoryItem],
    workspace_root: &Path,
) -> Result<Option<Vec<HistoryItem>>, String> {
    let injections = injections(history, workspace_root);
    if injections.is_empty() {
        return Ok(None);
    }
    let mut request = history.to_vec();
    request.extend(injections);
    Ok(Some(request))
}

/// 供系统提示使用的目录行：`- \`name\`: description`（描述压缩空白并截断）。
pub fn catalog_lines(skills: &[SkillSummary], max_description: usize) -> Vec<String> {
    skills
        .iter()
        .filter(|skill| skill.model_invocable)
        .map(|skill| {
            let normalized = skill.description.split_whitespace().collect::<Vec<_>>().join(" ");
            let description = if normalized.chars().count() <= max_description {
                normalized
            } else {
                let mut truncated: String = normalized
                    .chars()
                    .take(max_description.saturating_sub(3))
                    .collect();
                truncated.push_str("...");
                truncated
            };
            format!("- `{}`: {}", skill.name, escape_text(&description))
        })
        .collect()
}

/// 宿主 `list_skills(sessionId)`：返回该会话作用域下 **user-invocable** 技能摘要。
///
/// 作用域复用会话档案固化的 `workspace_root`（与 `@file`/`@session` 候选同一口径），
/// 只列摘要、不加载正文；`/name` 的正文由 `agent_loop` 在请求副本里注入。
#[tauri::command]
pub fn list_skills(
    app: tauri::AppHandle,
    session_id: String,
) -> Result<Vec<SkillEntry>, String> {
    use tauri::Manager;
    let state = app.state::<crate::app_state::AppState>();
    let root = crate::workspace_files::session_root(&state, &session_id);
    Ok(discover(&root)
        .into_iter()
        .filter(|skill| skill.user_invocable)
        .map(|skill| to_entry(&skill))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shaniu-skill-{tag}-{}-{}",
            std::process::id(),
            ulid::Ulid::generate()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_skill(root: &Path, relative: &str, frontmatter: &str, body: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("---\n{frontmatter}\n---\n{body}")).unwrap();
    }

    /// 只含项目根的发现：单测不读开发机真实用户目录。
    fn project_roots(workspace: &Path) -> Vec<SkillRoot> {
        let project_root = find_project_root(workspace);
        vec![
            SkillRoot {
                path: project_root.join(".dsh").join("skills"),
                rank: PROJECT_DSH_RANK,
            },
            SkillRoot {
                path: project_root.join(".agents").join("skills"),
                rank: PROJECT_AGENTS_RANK,
            },
        ]
    }

    fn discover_project(workspace: &Path) -> Vec<SkillSummary> {
        discover_in(&project_roots(workspace))
    }

    #[test]
    fn skill_name_grammar_matches_upstream() {
        assert!(is_skill_name("pdf"));
        assert!(is_skill_name("a-b-c"));
        assert!(is_skill_name("a1-b2"));
        assert!(!is_skill_name(""));
        assert!(!is_skill_name("-a"));
        assert!(!is_skill_name("a-"));
        assert!(!is_skill_name("A"));
        assert!(!is_skill_name("a_b"));
        assert!(!is_skill_name("a--b"));
    }

    #[test]
    fn gesture_scanner_matches_whitespace_bounded_tokens() {
        let names = gesture_names("请用 /pdf 和 /doc-x 处理，但 /usr/bin 与 5/8 不算");
        assert_eq!(names, vec!["pdf".to_string(), "doc-x".to_string()]);
        assert!(gesture_names("/PDF").is_empty());
        assert!(gesture_names("a/pdf").is_empty());
    }

    #[test]
    fn discovery_ranks_project_over_user_and_parses_layouts() {
        let workspace = temp_root("rank");
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        write_skill(
            &workspace,
            ".agents/skills/dup/SKILL.md",
            "name: dup\ndescription: agents copy",
            "agents body",
        );
        write_skill(
            &workspace,
            ".dsh/skills/dup/SKILL.md",
            "name: dup\ndescription: project copy",
            "project body",
        );
        write_skill(
            &workspace,
            ".dsh/skills/flat.md",
            "name: flat\ndescription: flat skill",
            "flat body",
        );
        let skills = discover_project(&workspace);
        assert_eq!(skills.len(), 2);
        let dup = skills.iter().find(|skill| skill.name == "dup").unwrap();
        assert_eq!(dup.description, "project copy");
        assert_eq!(dup.rank, PROJECT_DSH_RANK);
        let flat = skills.iter().find(|skill| skill.name == "flat").unwrap();
        assert_eq!(flat.rank, PROJECT_DSH_RANK);
        assert_eq!(flat.directory, workspace.join(".dsh").join("skills"));
    }

    #[test]
    fn invocation_policy_defaults_and_legacy_rejection() {
        let workspace = temp_root("invoke");
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        write_skill(
            &workspace,
            ".dsh/skills/hidden/SKILL.md",
            "name: hidden\ndescription: hidden\nuser-invocable: false",
            "body",
        );
        write_skill(
            &workspace,
            ".dsh/skills/model-off/SKILL.md",
            "name: model-off\ndescription: off\ndisable-model-invocation: true",
            "body",
        );
        write_skill(
            &workspace,
            ".dsh/skills/legacy/SKILL.md",
            "name: legacy\ndescription: legacy\nmodelInvocable: true",
            "body",
        );
        let skills = discover_project(&workspace);
        assert!(!skills.iter().any(|skill| skill.name == "legacy"));
        let hidden = skills.iter().find(|skill| skill.name == "hidden").unwrap();
        assert!(!hidden.user_invocable);
        assert!(hidden.model_invocable);
        let model_off = skills.iter().find(|skill| skill.name == "model-off").unwrap();
        assert!(model_off.user_invocable);
        assert!(!model_off.model_invocable);
    }

    #[test]
    fn render_matches_upstream_wrapper_shape() {
        let workspace = temp_root("render");
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        write_skill(
            &workspace,
            ".dsh/skills/pdf/SKILL.md",
            "name: pdf\ndescription: pdf skill",
            "Use <tools> & care.",
        );
        let skill = load(&workspace, "pdf").unwrap();
        let rendered = render_skill_content(&skill);
        assert!(rendered.starts_with("<skill_content name=\"pdf\">\n<skill_resources>\n"));
        assert!(rendered.contains("<skill_instructions>\nUse <tools> & care.\n</skill_instructions>"));
        assert!(rendered.ends_with("</skill_content>"));
    }

    #[test]
    fn prepare_request_injects_only_trailing_user_gestures() {
        let workspace = temp_root("inject");
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        write_skill(
            &workspace,
            ".dsh/skills/pdf/SKILL.md",
            "name: pdf\ndescription: pdf skill",
            "PDF BODY",
        );
        let history = vec![
            HistoryItem::Message { role: Role::User, text: "/pdf".into(), reasoning: None },
            HistoryItem::Message { role: Role::Assistant, text: "done".into(), reasoning: None },
        ];
        assert!(prepare_request(&history, &workspace).unwrap().is_none());
        let history = vec![
            HistoryItem::Message { role: Role::User, text: "old".into(), reasoning: None },
            HistoryItem::Message { role: Role::Assistant, text: "ok".into(), reasoning: None },
            HistoryItem::Message { role: Role::User, text: "use /pdf now".into(), reasoning: None },
        ];
        let request = prepare_request(&history, &workspace).unwrap().unwrap();
        assert_eq!(request.len(), history.len() + 1);
        match &request[3] {
            HistoryItem::Message { role: Role::User, text, .. } => {
                assert!(text.contains("<skill_content name=\"pdf\">"));
                assert!(text.contains("PDF BODY"));
            }
            _ => panic!("expected injected user message"),
        }
    }

    #[test]
    fn user_disabled_skill_is_not_injected() {
        let workspace = temp_root("user-off");
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        write_skill(
            &workspace,
            ".dsh/skills/hidden/SKILL.md",
            "name: hidden\ndescription: hidden\nuser-invocable: false",
            "SECRET",
        );
        let history = vec![HistoryItem::Message {
            role: Role::User,
            text: "/hidden".into(),
            reasoning: None,
        }];
        assert!(prepare_request(&history, &workspace).unwrap().is_none());
    }
}
