//! 内置模型能力表（SH-132 第一层，`resources/model-catalog.json`）。
//!
//! 归属 `config` 层而不是顶层模块：这张表只提供"用户没声明时补什么默认值"，
//! 是被 `config` 的默认值解析直接读取的数据层（`ModelProfile::builtin`）。放在
//! 顶层会让 `config` 反向依赖业务模块——`docs/01 §2.3.2` 明令禁止的边。
//!
//! 这是四层能力来源里最靠前的一层：**内置表 → 端点元数据 → 实测探针 → 手动**。
//! 有它就不必每次从零探；探针只用于校验"这家实际表现与标称是否一致"。
//!
//! 两条硬约束（用户拍板）：
//!
//! 1. **精确 id 优先，正则家族兜底**。家族命中一律标「推测」，与「内置表」区分；
//!    探针结论优先于它。家族正则用**全串匹配**（见 [`lookup_in`]），`^deepseek-chat`
//!    这种漏写 `$` 的规则也不会吃掉 `deepseek-chat-v2`——版本后缀是最常见的踩坑点。
//! 2. **匹配必须带 host**。同一个模型 id 在不同端点能力并不相同：实测
//!    `deepseek-flash` 在 `api.deepseek.com` 是 text+image / 393216 输出，在
//!    `token.sensenova.cn` 只有 text / 65536。只按模型名匹配就是把一家的能力安到
//!    另一家头上，而且错得没有声音。
//!
//! 数据来源与更新方式：全部来自端点 `GET /models`（含 `/models/{id}`）实测取回，
//! 手工维护，**不抄不确定的数字**——查不到就留空，交给端点/探针/用户。表头带
//! `schema_version` 与 `data_updated_at`，界面上的「内置表」徽标旁显示数据日期，
//! 过时看得出来。

use std::sync::OnceLock;

use regex::Regex;
use serde::Deserialize;

/// 随二进制发布的内置表。`include_str!` 而不是运行时读文件：便携版/安装版都不会
/// 因为漏拷资源目录而静默失去第一层能力来源。
pub const CATALOG_JSON: &str = include_str!("../../resources/model-catalog.json");

/// 表结构版本。**加字段不升版本**（`Option` 缺省即兼容）；改变已有字段语义才升。
/// 版本不认识时整表按不存在处理——宁可降级到探针，也不要拿错语义的旧表当真。
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    pub schema_version: u32,
    /// `YYYY-MM-DD`。界面显示「内置表数据 2026-11-09」，过时一眼能看出来。
    pub data_updated_at: String,
    #[serde(default)]
    pub note: String,
    pub models: Vec<CatalogEntry>,
    #[serde(default)]
    pub families: Vec<FamilyRule>,
}

/// 一条精确匹配的内置模型条目。
///
/// 扩展位（`audio`/`pdf`/`prompt_caching`/`native_web_search`）**当前没有任何消费方**：
/// 可以解析，但填了也不会进入能力判定或请求参数，等于静默无效配置。保留字段位是为了
/// 先固化表结构；接入实测数据时必须同时接上真实读取方，否则不要填。
/// `deny_unknown_fields` 保证手写表时拼错字段名会直接报错，而不是被 serde 静默忽略成
/// "这一项没声明"。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogEntry {
    /// 端点主机（不含 scheme/路径/端口），如 `api.deepseek.com`。
    pub host: String,
    /// 端点上的模型 id（大小写敏感，原样比较）。
    pub id: String,
    pub context_window: Option<u64>,
    pub max_output: Option<u64>,
    /// 输入模态，只用 `text` / `image`（端点声明的 `video` 不映射成能力）。
    pub input: Option<Vec<String>>,
    pub tools: Option<bool>,
    /// 端点明确列出的推理档位名（按 `THINKING_LEVELS` 拼写）。
    pub reasoning_levels: Option<Vec<String>>,
    pub default_reasoning_level: Option<String>,
    pub builtin_web_search: Option<bool>,
    // 扩展位：以下四项当前无消费方，填了也不生效（详见结构体注释）。
    pub audio: Option<bool>,
    pub pdf: Option<bool>,
    pub prompt_caching: Option<bool>,
    pub native_web_search: Option<bool>,
    #[serde(default)]
    pub note: String,
}

/// 一条家族规则：正则命中即按本条的保守取值**推测**。
///
/// 家族规则只写"所有已知成员都成立"的公共下限（例如 LongCat-2.0/2.5 都是 1M 窗口，
/// 但只有 2.5 起有 vision，家族就不声明 vision）。扩展位不上家族：家族已经是推测，
/// 再叠推测等于没有依据。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FamilyRule {
    /// 规则名，同时用于测试夹具（每条规则必须有命中用例）。
    pub id: String,
    pub host: String,
    pub pattern: String,
    pub context_window: Option<u64>,
    pub max_output: Option<u64>,
    pub input: Option<Vec<String>>,
    pub tools: Option<bool>,
    pub reasoning_levels: Option<Vec<String>>,
    pub default_reasoning_level: Option<String>,
    pub builtin_web_search: Option<bool>,
    #[serde(default)]
    pub note: String,
}

/// 命中后拿到的能力字段（两种命中形态统一成一个视图）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuiltinCapabilities {
    pub context_window: Option<u64>,
    pub max_output: Option<u64>,
    pub input: Option<Vec<String>>,
    pub tools: Option<bool>,
    pub reasoning_levels: Option<Vec<String>>,
    pub default_reasoning_level: Option<String>,
    pub builtin_web_search: Option<bool>,
}

/// 内置表命中结果。
#[derive(Debug, Clone, Copy)]
pub enum Hit<'a> {
    /// 精确 id 命中：来源「内置表」。
    Exact(&'a CatalogEntry),
    /// 家族正则命中：来源「推测」，探针结论优先于它。
    Family(&'a FamilyRule),
}

impl Hit<'_> {
    pub fn capabilities(&self) -> BuiltinCapabilities {
        match self {
            Hit::Exact(entry) => BuiltinCapabilities {
                context_window: entry.context_window,
                max_output: entry.max_output,
                input: entry.input.clone(),
                tools: entry.tools,
                reasoning_levels: entry.reasoning_levels.clone(),
                default_reasoning_level: entry.default_reasoning_level.clone(),
                builtin_web_search: entry.builtin_web_search,
            },
            Hit::Family(rule) => BuiltinCapabilities {
                context_window: rule.context_window,
                max_output: rule.max_output,
                input: rule.input.clone(),
                tools: rule.tools,
                reasoning_levels: rule.reasoning_levels.clone(),
                default_reasoning_level: rule.default_reasoning_level.clone(),
                builtin_web_search: rule.builtin_web_search,
            },
        }
    }

    /// 是否家族推测。界面据此显示「推测」而不是「内置表」。
    pub fn is_inferred(&self) -> bool {
        matches!(self, Hit::Family(_))
    }
}

struct Loaded {
    catalog: Option<Catalog>,
    regexes: Vec<Option<Regex>>,
}

impl Loaded {
    fn from_catalog(catalog: Option<Catalog>) -> Self {
        let regexes = catalog
            .as_ref()
            .map(|c| {
                c.families
                    .iter()
                    .map(|rule| match Regex::new(&rule.pattern) {
                        Ok(regex) => Some(regex),
                        Err(error) => {
                            tracing::warn!(rule = %rule.id, pattern = %rule.pattern, %error, "内置表家族正则无效，跳过该规则");
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self { catalog, regexes }
    }
}

static LOADED: OnceLock<Loaded> = OnceLock::new();

fn loaded() -> &'static Loaded {
    LOADED.get_or_init(|| Loaded::from_catalog(parse_catalog(CATALOG_JSON)))
}

/// 解析并校验表头版本。损坏或版本不认识 = 整表当不存在（降级到端点/探针），
/// 不让一条坏数据阻断启动，也不拿错语义的表当真。
pub fn parse_catalog(raw: &str) -> Option<Catalog> {
    match serde_json::from_str::<Catalog>(raw) {
        Ok(catalog) if catalog.schema_version == SCHEMA_VERSION => Some(catalog),
        Ok(catalog) => {
            tracing::warn!(
                version = catalog.schema_version,
                expected = SCHEMA_VERSION,
                "内置模型表版本不认识，按无表处理"
            );
            None
        }
        Err(error) => {
            tracing::warn!(%error, "内置模型表解析失败，按无表处理");
            None
        }
    }
}

pub fn catalog() -> Option<&'static Catalog> {
    loaded().catalog.as_ref()
}

/// 表数据日期（`YYYY-MM-DD`）。界面上的「内置表」徽标旁显示它。
pub fn data_updated_at() -> Option<&'static str> {
    catalog().map(|catalog| catalog.data_updated_at.as_str())
}

/// 从 `base_url` 取主机（小写）。取不到 scheme 就不是我们认识的端点，直接放弃匹配。
pub fn host_of(base_url: &str) -> Option<String> {
    let trimmed = base_url.trim();
    let rest = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    // 去掉 userinfo（`user:pass@host`），再去掉端口；IPv6 字面量不在模型端点范围内。
    let host = authority.rsplit('@').next()?.split(':').next()?;
    if host.is_empty() {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

/// 查内置表：先精确 id，再家族正则。两者都要求 host 一致。
pub fn lookup(base_url: &str, model: &str) -> Option<Hit<'static>> {
    lookup_in(loaded(), base_url, model)
}

fn lookup_in<'a>(loaded: &'a Loaded, base_url: &str, model: &str) -> Option<Hit<'a>> {
    let catalog = loaded.catalog.as_ref()?;
    let host = host_of(base_url)?;
    let model = model.trim();
    if model.is_empty() {
        return None;
    }
    if let Some(entry) = catalog
        .models
        .iter()
        .find(|entry| entry.id == model && entry.host.eq_ignore_ascii_case(&host))
    {
        return Some(Hit::Exact(entry));
    }
    for (index, rule) in catalog.families.iter().enumerate() {
        if !rule.host.eq_ignore_ascii_case(&host) {
            continue;
        }
        let Some(Some(regex)) = loaded.regexes.get(index) else { continue };
        // 全串匹配：规则漏写 `$` 也不会把 `deepseek-chat-v2` 吃进 `deepseek-chat` 家族。
        // 版本后缀、`-preview`、日期后缀都是这条约束的实际反例（见单测）。
        let full = regex
            .find(model)
            .is_some_and(|m| m.start() == 0 && m.end() == model.len());
        if full {
            return Some(Hit::Family(rule));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::THINKING_LEVELS;
    use std::collections::{BTreeMap, BTreeSet};

    /// 每条家族规则都必须有一个能命中它的样本——规则写了没人测，等于没写。
    const FAMILY_SAMPLES: &[(&str, &str, &str)] = &[
        ("https://api.longcat.chat/openai/v1", "LongCat-2.7", "longcat-2x"),
        ("https://token.sensenova.cn/v1", "sensenova-6.8-flash-preview", "sensenova-68-flash"),
        ("https://token.sensenova.cn/v1", "glm-5.7", "sensenova-glm-5"),
    ];

    #[test]
    fn shipped_catalog_parses_with_header() {
        let catalog = catalog().expect("随仓库发布的内置表必须能解析");
        assert_eq!(catalog.schema_version, SCHEMA_VERSION);
        let date = catalog.data_updated_at.as_str();
        assert_eq!(date.len(), 10, "data_updated_at 必须是 YYYY-MM-DD：{date}");
        assert!(
            date.as_bytes()[4] == b'-' && date.as_bytes()[7] == b'-',
            "data_updated_at 格式不对：{date}"
        );
        assert!(!catalog.models.is_empty(), "内置表不能是空表");
    }

    #[test]
    fn every_model_entry_is_reachable_by_exact_lookup() {
        let catalog = catalog().unwrap();
        let mut ids = BTreeSet::new();
        for entry in &catalog.models {
            let base = format!("https://{}/v1", entry.host);
            match lookup(&base, &entry.id) {
                Some(Hit::Exact(hit)) => assert_eq!(hit.id, entry.id),
                // 家族规则先命中说明精确表被遮蔽了——匹配顺序错了。
                Some(Hit::Family(rule)) => panic!(
                    "模型 {} 被家族规则 {} 遮蔽，精确匹配必须优先",
                    entry.id, rule.id
                ),
                None => panic!("模型 {} 查不到（host {}）", entry.id, entry.host),
            }
            assert!(
                ids.insert((entry.host.clone(), entry.id.clone())),
                "内置表重复条目：{}@{}",
                entry.id,
                entry.host
            );
        }
    }

    #[test]
    fn every_family_rule_has_a_matching_sample() {
        let catalog = catalog().unwrap();
        let rules: BTreeSet<&str> = catalog.families.iter().map(|r| r.id.as_str()).collect();
        let samples: BTreeSet<&str> = FAMILY_SAMPLES.iter().map(|s| s.2).collect();
        assert_eq!(
            rules, samples,
            "每条家族规则都要有命中样本，样本也不能指向不存在的规则"
        );
        for (base, model, rule_id) in FAMILY_SAMPLES {
            match lookup(base, model) {
                Some(Hit::Family(rule)) => assert_eq!(rule.id, *rule_id, "样本 {model} 命中了别的规则"),
                Some(Hit::Exact(_)) => panic!("样本 {model} 不该精确命中"),
                None => panic!("样本 {model} 没有命中规则 {rule_id}"),
            }
        }
    }

    /// 用户点名的三条反例：版本后缀、`-preview`、日期后缀都各有专属项，
    /// `^deepseek-chat$` 的家族规则不得把它们吃掉。
    #[test]
    fn family_rule_never_eats_versioned_preview_or_dated_ids() {
        let catalog: Catalog = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "data_updated_at": "2026-01-01",
            "models": [
                { "host": "api.deepseek.com", "id": "deepseek-chat-v2", "context_window": 200000 },
                { "host": "api.deepseek.com", "id": "deepseek-chat-preview", "context_window": 300000 },
                { "host": "api.deepseek.com", "id": "deepseek-chat-20260101", "context_window": 400000 }
            ],
            "families": [
                { "id": "deepseek-chat", "host": "api.deepseek.com", "pattern": "^deepseek-chat$", "context_window": 100000 },
                { "id": "deepseek-chat-sloppy", "host": "api.deepseek.com", "pattern": "^deepseek-chat" }
            ]
        }))
        .unwrap();
        let loaded = Loaded::from_catalog(Some(catalog));
        let base = "https://api.deepseek.com";

        for (model, window) in [
            ("deepseek-chat-v2", 200_000),
            ("deepseek-chat-preview", 300_000),
            ("deepseek-chat-20260101", 400_000),
        ] {
            match lookup_in(&loaded, base, model) {
                Some(Hit::Exact(entry)) => assert_eq!(entry.context_window, Some(window)),
                other => panic!("{model} 必须精确命中专属项，实际 {other:?}"),
            }
        }
        // 规则本身仍然能命中裸 id（不是"永远不匹配"）
        match lookup_in(&loaded, base, "deepseek-chat") {
            Some(Hit::Family(rule)) => assert_eq!(rule.id, "deepseek-chat"),
            other => panic!("裸 id 应命中家族规则，实际 {other:?}"),
        }
        // 漏写 `$` 的规则也不能前缀吃掉：全串匹配兜住了
        assert!(
            lookup_in(&loaded, base, "deepseek-chat-v9").is_none(),
            "漏写 $ 的规则不得前缀匹配"
        );
        // host 不同就不匹配（同一 id 在不同端点是不同模型）
        assert!(lookup_in(&loaded, "https://token.sensenova.cn/v1", "deepseek-chat-v2").is_none());
    }

    #[test]
    fn same_model_id_on_two_hosts_keeps_its_own_capabilities() {
        // 活证据：deepseek-flash 在两家端点上 vision 与输出上限都不同。
        let deepseek = lookup("https://api.deepseek.com", "deepseek-flash").unwrap();
        let sensenova = lookup("https://token.sensenova.cn/v1", "deepseek-flash").unwrap();
        assert_eq!(deepseek.capabilities().input, Some(vec!["text".into(), "image".into()]));
        assert_eq!(sensenova.capabilities().input, Some(vec!["text".into()]));
        assert_ne!(
            deepseek.capabilities().max_output,
            sensenova.capabilities().max_output
        );
    }

    #[test]
    fn family_hits_are_marked_inferred_and_exact_are_not() {
        let family = lookup("https://api.longcat.chat/openai/v1", "LongCat-2.7").unwrap();
        assert!(family.is_inferred());
        let exact = lookup("https://api.longcat.chat/openai/v1", "LongCat-2.5").unwrap();
        assert!(!exact.is_inferred());
    }

    #[test]
    fn catalog_semantics_hold() {
        let catalog = catalog().unwrap();
        for entry in &catalog.models {
            check_fields(&entry.id, entry.input.as_deref(), entry.reasoning_levels.as_deref(), entry.default_reasoning_level.as_deref());
        }
        for rule in &catalog.families {
            check_fields(&rule.id, rule.input.as_deref(), rule.reasoning_levels.as_deref(), rule.default_reasoning_level.as_deref());
            assert!(catalog.models.iter().all(|e| e.id != rule.id), "家族规则名 {} 与模型 id 撞名", rule.id);
        }
    }

    fn check_fields(
        what: &str,
        input: Option<&[String]>,
        levels: Option<&[String]>,
        default: Option<&str>,
    ) {
        for modality in input.unwrap_or_default() {
            assert!(
                matches!(modality.as_str(), "text" | "image"),
                "{what} 的输入模态 {modality} 不是本项目支持的能力"
            );
        }
        for level in levels.unwrap_or_default() {
            assert!(
                THINKING_LEVELS.contains(&level.as_str()),
                "{what} 的推理档位 {level} 不在 THINKING_LEVELS 里"
            );
        }
        if let Some(default) = default {
            assert!(
                levels.is_some_and(|list| list.iter().any(|l| l == default)),
                "{what} 的默认档 {default} 不在自己的档位声明里"
            );
        }
    }

    #[test]
    fn unknown_fields_and_versions_are_rejected_not_ignored() {
        let base = serde_json::json!({
            "schema_version": 1,
            "data_updated_at": "2026-01-01",
            "models": [{ "host": "h", "id": "m" }]
        });
        // 字段名写错会直接报错，而不是被静默忽略成"没声明"
        let mut typo = base.clone();
        typo["models"][0]["context_windows"] = serde_json::json!(1);
        assert!(serde_json::from_value::<Catalog>(typo).is_err(), "未声明字段必须被拒绝");
        // 版本不认识：整表当不存在
        let mut future = base.clone();
        future["schema_version"] = serde_json::json!(99);
        assert!(parse_catalog(&future.to_string()).is_none());
        assert!(parse_catalog(&base.to_string()).is_some());
        assert!(parse_catalog("{not json").is_none());
    }

    #[test]
    fn host_extraction_handles_scheme_port_and_path() {
        let cases = BTreeMap::from([
            ("https://api.deepseek.com", Some("api.deepseek.com")),
            ("https://API.DeepSeek.com/v1/", Some("api.deepseek.com")),
            ("http://127.0.0.1:8080/v1", Some("127.0.0.1")),
            ("https://user:pw@host.example.com/v1", Some("host.example.com")),
            ("api.deepseek.com", None),
            ("", None),
        ]);
        for (input, expected) in cases {
            assert_eq!(host_of(input).as_deref(), expected, "host_of({input})");
        }
    }
}
