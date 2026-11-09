//! 模型能力声明与实测探针（SH-132，docs/04 §4.1）。
//!
//! 为什么"声明"不够：`web_search_preview` 有一次活证据——声明了、端点 200、
//! **静默忽略**。能力声明会以同样方式坏：填错就是自欺，而且坏得没有声音。
//! 所以这里把能力拆成两栏：
//!
//! - **声明**（`ModelProfile::capabilities`）：值取模型行既有字段，记录"谁说的"
//!   （端点元数据 / 手动）。
//! - **实测**（本模块的探针 + `<data_root>\cache\capabilities.json` 缓存）：只记录
//!   "测出来是什么"。两者不互相覆盖，设置页并列显示三态（已验证 / 与声明不符 /
//!   未验证）。
//!
//! 缓存键 = `base_url + 协议 + model id + 密钥指纹`：换 key / 换端点 / 换模型即
//! 自然失效并重探；同一组合只探一次，不每次启动都打请求。密钥只以 SHA-256 前
//! 16 位十六进制参与键，明文不进缓存、不进日志。

use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::Manager;

use crate::config::{CapabilityRecord, CapabilitySource, ModelProfile, ProviderProfile};

pub const CACHE_FILE: &str = "capabilities.json";
/// 实测判「不生效」的相对阈值（用户拍板）：最高档与最低档的 reasoning 指标差异
/// 小于 5% 就是噪声内，判"不生效"，而不是"生效但模型没怎么想"。判反了会让用户
/// 以为自己开了高档思考，而端点其实一个字段都没读。
pub const REASONING_NOISE_RATIO: f64 = 0.05;
/// 两次推理探针请求的最小间隔。同端点连着打两发容易被限流，实测 1s 以上稳定。
pub const PROBE_INTERVAL_MS: u64 = 1_100;

/// 能力项种类。开放集合：新增能力（`audio`/`embeddings`/`prompt_caching`…）
/// 只需加一行 [`ITEMS`]，配置与界面都按 id 泛化，不需要改结构体。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityKind {
    /// 布尔能力（值来自专属字段或能力项的 `value`）。
    Flag,
    /// 枚举能力（值来自 `reasoning_efforts` 的键）。
    Levels,
    /// 数值能力（值来自 `context_window` / `max_tokens`）。
    Number,
}

/// 一项能力的元信息（界面标签、种类、是否有探针）。
#[derive(Debug, Clone, Copy)]
pub struct CapabilityItem {
    pub id: &'static str,
    pub label: &'static str,
    pub kind: CapabilityKind,
    /// 是否有实测探针。`context_window` 明确不探（太贵），用用户/端点给的值，
    /// 超限报错时把实际限制写进日志。
    pub probe: bool,
}

/// 能力项登记表。**这里是唯一清单**：新增项只改这张表，配置、视图、设置页
/// 都按 id 读取。未登记但出现在配置里的项不会被丢弃——设置页把它们归到
/// "其他（保留）"，避免旧版本写下的能力项被一次保存抹掉。
pub const ITEMS: &[CapabilityItem] = &[
    CapabilityItem { id: "vision", label: "视觉输入", kind: CapabilityKind::Flag, probe: true },
    CapabilityItem { id: "tools", label: "工具调用", kind: CapabilityKind::Flag, probe: true },
    CapabilityItem { id: "reasoning", label: "推理档位", kind: CapabilityKind::Levels, probe: true },
    CapabilityItem { id: "context_window", label: "上下文窗口", kind: CapabilityKind::Number, probe: false },
    CapabilityItem { id: "max_output", label: "输出上限", kind: CapabilityKind::Number, probe: false },
    CapabilityItem { id: "builtin_web_search", label: "端点托管搜索", kind: CapabilityKind::Flag, probe: false },
];

pub fn item(id: &str) -> Option<&'static CapabilityItem> {
    ITEMS.iter().find(|i| i.id == id)
}

// ============================== 实测缓存 ==============================

/// 一项能力的实测结论。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProbeOutcome {
    /// `None` = 探针没跑通（网络/协议错误），不能当成"不生效"。
    pub verified: Option<bool>,
    /// 实测通过的推理档（仅 `reasoning`）。
    pub verified_levels: Vec<String>,
    pub evidence: String,
    pub checked_at_ms: u64,
}

/// 缓存文件：`{ version, entries: { cache_key: { item: ProbeOutcome } } }`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProbeCache {
    pub version: u32,
    pub entries: BTreeMap<String, BTreeMap<String, ProbeOutcome>>,
}

pub const CACHE_VERSION: u32 = 1;

impl Default for ProbeCache {
    /// 新缓存必须带上当前版本，否则落盘后读回时会被版本校验当成旧缓存丢弃。
    fn default() -> Self {
        Self { version: CACHE_VERSION, entries: BTreeMap::new() }
    }
}

impl ProbeCache {
    /// 读缓存；文件缺失/损坏一律按空缓存处理（探针是增强，不该拦住启动）。
    pub fn load(data_root: &Path) -> Self {
        let path = data_root.join(crate::config::CACHE_DIR).join(CACHE_FILE);
        let Ok(raw) = std::fs::read_to_string(&path) else { return Self::default() };
        match serde_json::from_str::<Self>(&raw) {
            Ok(cache) if cache.version == CACHE_VERSION => cache,
            Ok(_) => Self::default(),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "能力实测缓存损坏，按空缓存处理");
                Self::default()
            }
        }
    }

    /// 原子写回：先写同目录临时文件再 rename。写失败只记日志——缓存丢了只是多探一次。
    pub fn save(&self, data_root: &Path) {
        let dir = data_root.join(crate::config::CACHE_DIR);
        if let Err(error) = std::fs::create_dir_all(&dir) {
            tracing::warn!(%error, "创建能力缓存目录失败");
            return;
        }
        let path = dir.join(CACHE_FILE);
        let body = match serde_json::to_string_pretty(self) {
            Ok(body) => body,
            Err(error) => {
                tracing::warn!(%error, "序列化能力缓存失败");
                return;
            }
        };
        let tmp = path.with_extension("json.tmp");
        if let Err(error) = std::fs::write(&tmp, body).and_then(|()| std::fs::rename(&tmp, &path)) {
            let _ = std::fs::remove_file(&tmp);
            tracing::warn!(path = %path.display(), %error, "写入能力缓存失败");
        }
    }

    pub fn get(&self, key: &str) -> Option<&BTreeMap<String, ProbeOutcome>> {
        self.entries.get(key)
    }

    pub fn put(&mut self, key: String, outcomes: BTreeMap<String, ProbeOutcome>) {
        self.entries.insert(key, outcomes);
    }
}

/// 缓存键：端点 + 协议 + 模型 + 密钥指纹。任何一项变了就是另一条记录，
/// 旧结论自然失效——不用额外写"失效条件"的代码，键本身就是失效条件。
pub fn cache_key(base_url: &str, protocol: &str, model: &str, api_key: &str) -> String {
    format!("{base_url}\u{1f}{protocol}\u{1f}{model}\u{1f}{}", key_fingerprint(api_key))
}

fn key_fingerprint(api_key: &str) -> String {
    let digest = Sha256::digest(api_key.as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ============================== 端点元数据 ==============================

/// 从 `/models` 或 `/models/{id}` 解析出的能力线索。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EndpointMetadata {
    pub context_window: Option<u64>,
    pub max_output: Option<u64>,
    pub input_modalities: Option<Vec<String>>,
    pub supports_tools: Option<bool>,
    pub builtin_web_search: Option<bool>,
    /// 端点声明的推理档位（`effort.supported_levels`）。可选档 = 声明 ∩ 这一份。
    pub supported_levels: Option<Vec<String>>,
    pub default_level: Option<String>,
}

impl EndpointMetadata {
    pub fn is_empty(&self) -> bool {
        self.context_window.is_none()
            && self.max_output.is_none()
            && self.input_modalities.is_none()
            && self.supports_tools.is_none()
            && self.builtin_web_search.is_none()
            && self.supported_levels.is_none()
    }
}

/// 解析模型条目。两种形状都要吃下：
///
/// - `/models` 条目：`context_window`、`max_output_tokens`、`input_modalities`（平铺数组）
/// - `/models/{id}` 条目：`context_length`、`architecture.input_modalities`、
///   `supported_parameters`（含 `tools` / `thinking`）
///
/// 缺字段或类型不符（例如 `context_length` 是字符串）一律按缺失处理，绝不 panic、
/// 也绝不拿模型名去猜。
pub fn parse_metadata(item: &serde_json::Value) -> EndpointMetadata {
    let context_window = first_number(item, &["context_window", "context_length"])
        .or_else(|| nested_number(item, &["architecture", "context_length"]));
    let max_output = first_number(item, &["max_output_tokens", "max_completion_tokens"]);
    let input_modalities = item
        .get("input_modalities")
        .or_else(|| item.get("architecture").and_then(|a| a.get("input_modalities")))
        .and_then(serde_json::Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|list| !list.is_empty());
    let params = item.get("supported_parameters").and_then(serde_json::Value::as_array);
    let has_param = |name: &str| {
        params.is_some_and(|list| list.iter().any(|p| p.as_str() == Some(name)))
    };
    let supports_tools = params.map(|_| has_param("tools"));
    // 只有端点**明确列出**参数表时才敢判"没有"；缺整张表是"不知道"。
    let builtin_web_search = params.map(|_| {
        has_param("web_search") || has_param("web_search_preview") || has_param("builtin_tools")
    });
    let effort = item.get("effort");
    let supported_levels = effort
        .and_then(|e| e.get("supported_levels"))
        .and_then(serde_json::Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|list| !list.is_empty());
    let default_level = effort
        .and_then(|e| e.get("default_level"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    EndpointMetadata {
        context_window,
        max_output,
        input_modalities,
        supports_tools,
        builtin_web_search,
        supported_levels,
        default_level,
    }
}

fn first_number(item: &serde_json::Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| item.get(*key).and_then(serde_json::Value::as_u64))
}

fn nested_number(item: &serde_json::Value, path: &[&str]) -> Option<u64> {
    let mut cursor = item;
    for key in path {
        cursor = cursor.get(*key)?;
    }
    cursor.as_u64()
}

/// 把端点元数据落成"声明 + 来源"。返回 (模型行改动, 是否提供了任何信息)。
///
/// 自动获取失败是常态，不是错误路径：调用方拿空元数据时静默降级到探针，
/// 设置页显示"端点未提供能力信息"即可。
pub fn apply_endpoint_metadata(row: &mut ModelProfile, meta: &EndpointMetadata) {
    if let Some(window) = meta.context_window {
        if row.context_window == 0 {
            row.context_window = window.min(u32::MAX as u64) as u32;
            row.capabilities.insert(
                "context_window".into(),
                CapabilityRecord { source: CapabilitySource::Endpoint, ..Default::default() },
            );
        }
    }
    if let Some(max_output) = meta.max_output {
        if row.max_tokens == 0 {
            row.max_tokens = max_output.min(u32::MAX as u64) as u32;
            row.capabilities.insert(
                "max_output".into(),
                CapabilityRecord { source: CapabilitySource::Endpoint, ..Default::default() },
            );
        }
    }
    if let Some(modalities) = &meta.input_modalities {
        if row.input.is_empty() {
            // 只认可我们真支持的模态：`video` 这类端点声明不能映射成能力。
            row.input = modalities
                .iter()
                .filter(|m| matches!(m.as_str(), "text" | "image"))
                .cloned()
                .collect();
            row.capabilities.insert(
                "vision".into(),
                CapabilityRecord::declared(
                    row.input.iter().any(|m| m == "image"),
                    CapabilitySource::Endpoint,
                ),
            );
        }
    }
    if let Some(tools) = meta.supports_tools {
        row.capabilities
            .insert("tools".into(), CapabilityRecord::declared(tools, CapabilitySource::Endpoint));
    }
    if let Some(search) = meta.builtin_web_search {
        row.capabilities.insert(
            "builtin_web_search".into(),
            CapabilityRecord::declared(search, CapabilitySource::Endpoint),
        );
    }
    if let Some(levels) = &meta.supported_levels {
        // 端点给了档位清单就存下来：可选档 = 本行声明 ∩ 这份清单。不覆盖已有记录的
        // source（手动填过的仍是手动），只补端点这一侧的约束。
        row.capabilities
            .entry("reasoning".into())
            .or_default()
            .endpoint_levels = levels.clone();
    }
}

// ============================== 保存时的来源维护 ==============================

/// 保存服务商文档时维护能力来源：先继承旧记录，再把"用户手改过的项"标成手动。
///
/// 为什么要继承：设置页的写面不带 `capabilities`（它只编辑模型行的四个字段），
/// 直接覆盖成空 map 会把端点抓到的来源记录抹掉——下一次保存之后，界面上原本
/// 标着「端点」的能力会全部变成「未知」，而且没人能解释为什么。
///
/// 为什么手改要标手动：用户填的值必须压过内置表与端点元数据，且**探针不得再覆盖**
/// （用户拍板）。这里顺手清掉旧的实测留档，避免"手改之后还挂着上次的实测结论"。
pub fn carry_sources(previous: &[ProviderProfile], next: &mut [ProviderProfile]) {
    for provider in next.iter_mut() {
        let Some(before_provider) = previous.iter().find(|p| p.id == provider.id) else {
            continue;
        };
        for row in provider.models.iter_mut() {
            let Some(before) = before_provider.models.iter().find(|m| m.id == row.id) else {
                continue;
            };
            for (item, record) in &before.capabilities {
                row.capabilities.entry(item.clone()).or_insert_with(|| record.clone());
            }
            let mut touched = Vec::new();
            if row.context_window != before.context_window {
                touched.push("context_window");
            }
            if row.max_tokens != before.max_tokens {
                touched.push("max_output");
            }
            if row.input != before.input {
                touched.push("vision");
            }
            if row.reasoning_efforts != before.reasoning_efforts {
                touched.push("reasoning");
            }
            for item in touched {
                row.capabilities.insert(
                    item.to_string(),
                    CapabilityRecord { source: CapabilitySource::Manual, ..Default::default() },
                );
            }
        }
    }
}

// ============================== 合并视图 ==============================

/// 内置表命中：来源（内置表 / 推测）与能力字段。没有命中 = 这一层没有发言权。
fn builtin_of(row: &ModelProfile, base_url: &str) -> Option<(CapabilitySource, crate::model_catalog::BuiltinCapabilities)> {
    crate::model_catalog::lookup(base_url, &row.id).map(|hit| {
        let source = if hit.is_inferred() {
            CapabilitySource::Inferred
        } else {
            CapabilitySource::Builtin
        };
        (source, hit.capabilities())
    })
}

/// 内置表是否真的对**这一项**表过态。表没写的项目不能把来源标成「内置表」/「推测」
/// ——否则界面上会出现"来源：内置表、值：false"这种自相矛盾的组合。
fn builtin_declares(
    item: &str,
    builtin: Option<&crate::model_catalog::BuiltinCapabilities>,
) -> bool {
    let Some(builtin) = builtin else { return false };
    match item {
        "vision" => builtin.input.as_ref().is_some_and(|input| !input.is_empty()),
        "tools" => builtin.tools.is_some(),
        "builtin_web_search" => builtin.builtin_web_search.is_some(),
        "context_window" => builtin.context_window.is_some(),
        "max_output" => builtin.max_output.is_some(),
        "reasoning" => builtin.reasoning_levels.as_ref().is_some_and(|l| !l.is_empty()),
        _ => false,
    }
}

/// 声明值从哪来：显式记录 > 本行字段（等价于用户手填）> 内置表 > 未知。
fn declared_source(
    row: &ModelProfile,
    item: &str,
    local: bool,
    builtin: Option<CapabilitySource>,
) -> CapabilitySource {
    if let Some(source) = row
        .capabilities
        .get(item)
        .map(|record| record.source)
        .filter(|source| *source != CapabilitySource::Unknown)
    {
        return source;
    }
    if local {
        return CapabilitySource::Manual;
    }
    builtin.unwrap_or(CapabilitySource::Unknown)
}

/// 设置页看到的一行能力：值 + 来源 + 三态。
///
/// 来源优先级（高 → 低）：手动 > 端点 > 内置表 > 推测 > 未知。实测只改 `state`，
/// **从不改**声明值——"填错了"必须被保留在界面上，而不是被一次探测洗成"对的"。
pub fn model_view(
    row: &ModelProfile,
    base_url: &str,
    probed: Option<&BTreeMap<String, ProbeOutcome>>,
) -> serde_json::Value {
    let builtin = builtin_of(row, base_url);
    let builtin_source = builtin.as_ref().map(|(source, _)| *source);
    let builtin_fields = builtin.as_ref().map(|(_, fields)| fields);
    let catalog_updated_at = serde_json::Value::from(
        crate::model_catalog::data_updated_at().unwrap_or_default(),
    );
    let mut out = serde_json::Map::new();
    for item in ITEMS {
        let record = row.capabilities.get(item.id);
        let mut entry = serde_json::Map::new();
        let local;
        match item.kind {
            CapabilityKind::Flag => {
                let (value, local_value) = match item.id {
                    "vision" => (row.declared_vision(base_url), row.input.iter().any(|m| m == "image")),
                    _ => {
                        let from_field = record.and_then(|r| r.value);
                        let local_value = from_field.is_some();
                        let value = from_field
                            .or_else(|| match item.id {
                                "tools" => builtin_fields.and_then(|b| b.tools),
                                "builtin_web_search" => {
                                    builtin_fields.and_then(|b| b.builtin_web_search)
                                }
                                _ => None,
                            })
                            .unwrap_or(false);
                        (value, local_value)
                    }
                };
                local = local_value;
                entry.insert("value".into(), serde_json::Value::Bool(value));
            }
            CapabilityKind::Levels => {
                let levels = row.declared_reasoning_levels(base_url);
                local = !row.reasoning_efforts.is_empty();
                let default = builtin_fields
                    .and_then(|b| b.default_reasoning_level.clone())
                    .filter(|level| levels.iter().any(|l| l == level))
                    .or_else(|| {
                        let fallback = crate::config::DEFAULT_REASONING_LEVEL;
                        levels.iter().any(|l| l == fallback).then(|| fallback.to_string())
                    })
                    .or_else(|| levels.first().cloned());
                entry.insert("levels".into(), serde_json::json!(levels));
                if let Some(default) = default {
                    entry.insert("default".into(), serde_json::Value::String(default));
                }
            }
            CapabilityKind::Number => {
                let value = match item.id {
                    "context_window" => row.context_window_effective(base_url),
                    _ => row.max_tokens,
                };
                local = match item.id {
                    "context_window" => row.context_window > 0,
                    _ => row.max_tokens > 0,
                };
                entry.insert("value".into(), serde_json::Value::from(value));
            }
        }
        let source = declared_source(
            row,
            item.id,
            local,
            builtin_source.filter(|_| builtin_declares(item.id, builtin_fields)),
        );
        entry.insert(
            "source".into(),
            serde_json::to_value(source).unwrap_or(serde_json::Value::Null),
        );
        // 「内置表」/「推测」徽标旁显示数据日期：表过时要看得出来。
        if matches!(source, CapabilitySource::Builtin | CapabilitySource::Inferred) {
            entry.insert("catalogUpdatedAt".into(), catalog_updated_at.clone());
        }
        // 实测结论只认缓存（它按 key/端点/模型分区）；配置里的 verified 是手写留档。
        // 只有**显式**标了手动（`carry_sources` 在用户改动后写入的 Manual 记录）
        // 才不采纳探针结论：用户填了就以用户为准。反过来，值只是"从本行字段读出来的"
        // （没有 Manual 记录）不算用户拍板——否则声明视觉的模型永远测不出"与声明不符"，
        // 探针就白跑了。
        let manual_record = record.is_some_and(|r| r.source == CapabilitySource::Manual);
        let outcome = if manual_record {
            None
        } else {
            probed.and_then(|map| map.get(item.id))
        };
        let verified = outcome
            .and_then(|o| o.verified)
            .or_else(|| record.and_then(|r| r.verified));
        let state = match verified {
            Some(true) => "verified",
            Some(false) => "mismatch",
            None => "unverified",
        };
        entry.insert("state".into(), serde_json::Value::String(state.into()));
        if let Some(outcome) = outcome {
            if !outcome.evidence.is_empty() {
                entry.insert("evidence".into(), serde_json::Value::String(outcome.evidence.clone()));
            }
            entry.insert("checkedAtMs".into(), serde_json::Value::from(outcome.checked_at_ms));
        } else if let Some(record) = record {
            if !record.evidence.is_empty() {
                entry.insert("evidence".into(), serde_json::Value::String(record.evidence.clone()));
            }
            if let Some(at) = record.checked_at_ms {
                entry.insert("checkedAtMs".into(), serde_json::Value::from(at));
            }
        }
        // 档位选择器只列实测通过的档；没有实测记录时列声明档（未验证不阻止使用）。
        if item.kind == CapabilityKind::Levels {
            let verified_levels: Vec<String> = outcome
                .filter(|o| !o.verified_levels.is_empty())
                .map(|o| o.verified_levels.clone())
                .or_else(|| record.map(|r| r.verified_levels.clone()))
                .unwrap_or_default();
            entry.insert("verifiedLevels".into(), serde_json::json!(verified_levels));
        }
        out.insert(item.id.into(), serde_json::Value::Object(entry));
    }
    // 扩展位：未登记的能力项原样带出（"其他"分组），不因界面不认识就丢。
    let extra: serde_json::Map<String, serde_json::Value> = row
        .capabilities
        .iter()
        .filter(|(id, _)| item(id).is_none())
        .map(|(id, record)| {
            let mut entry = serde_json::to_value(record).unwrap_or(serde_json::Value::Null);
            if let Some(map) = entry.as_object_mut() {
                map.insert("state".into(), serde_json::Value::String(row.capability_state(id).into()));
            }
            (id.clone(), entry)
        })
        .collect();
    out.insert("other".into(), serde_json::Value::Object(extra));
    serde_json::Value::Object(out)
}

// ============================== 探针 ==============================

/// 探针目标（一次请求所需的全部端点信息）。
#[derive(Debug, Clone)]
pub struct ProbeTarget {
    pub base_url: String,
    pub protocol: String,
    pub model: String,
    pub headers: BTreeMap<String, String>,
    pub api_key: String,
    pub token_budget_hint: u32,
}

impl ProbeTarget {
    pub fn cache_key(&self) -> String {
        cache_key(&self.base_url, &self.protocol, &self.model, &self.api_key)
    }
}

/// 跑全部探针。单项失败不影响其它项；返回的 map 直接进缓存。
pub async fn probe_all(target: &ProbeTarget, row: &ModelProfile) -> BTreeMap<String, ProbeOutcome> {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            let outcome = ProbeOutcome {
                verified: None,
                evidence: format!("HTTP 客户端不可用: {error}"),
                checked_at_ms: now_ms(),
                ..Default::default()
            };
            return [("vision".to_string(), outcome.clone()), ("tools".to_string(), outcome)]
                .into_iter()
                .collect();
        }
    };
    let mut out = BTreeMap::new();
    out.insert("vision".to_string(), probe_vision(&client, target).await);
    out.insert("tools".to_string(), probe_tools(&client, target).await);
    if !row.declared_reasoning_levels(&target.base_url).is_empty() {
        out.insert("reasoning".to_string(), probe_reasoning(&client, target, row).await);
    }
    out
}

async fn send(client: &reqwest::Client, target: &ProbeTarget, body: serde_json::Value) -> Result<serde_json::Value, String> {
    let url = endpoint_url(target);
    let mut request = client.post(&url).header("accept", "application/json");
    for (name, value) in &target.headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = request
        .bearer_auth(&target.api_key)
        .json(&body)
        .send()
        .await
        .map_err(|error| format!("请求 {url} 失败: {error}"))?;
    let status = response.status();
    let bytes = response.bytes().await.map_err(|error| format!("读取响应失败: {error}"))?;
    if !status.is_success() {
        // 429 与"不支持"是两回事：限流只说明这次没验证成，绝不能记成"不生效"。
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(format!("{url} 返回 429 限流：本次未验证（不是『不生效』），稍后重试"));
        }
        return Err(format!("{url} 返回 {status}（能力探针只需要一次最小请求，端点拒绝说明该能力当前不可用）"));
    }
    serde_json::from_slice(&bytes).map_err(|_| format!("{url} 的响应不是 JSON"))
}

fn endpoint_url(target: &ProbeTarget) -> String {
    let base = target.base_url.trim_end_matches('/');
    if target.protocol == "openai-responses" {
        if base.ends_with("/responses") { base.to_string() } else { format!("{base}/responses") }
    } else if base.ends_with("/chat/completions") {
        base.to_string()
    } else {
        format!("{base}/chat/completions")
    }
}

/// 视觉探针：红、绿两张纯色图各问一次，**两问都对**才算生效。
///
/// 尺寸不能太小：实测 LongCat 对 64×64 PNG 会误答 "gray"，256px 才稳定答对；
/// 太小的图会制造"不支持视觉"的假阴性。正文上限给 512——推理型模型会先花 token
/// 思考，给 64 会被截断成空正文；正文为空时 [`extract_text`] 回读 `reasoning_content`。
async fn probe_vision(client: &reqwest::Client, target: &ProbeTarget) -> ProbeOutcome {
    const IMAGES: [(image::Rgb<u8>, &str, &str); 2] = [
        (image::Rgb([200, 20, 20]), "红", "red"),
        (image::Rgb([20, 140, 40]), "绿", "green"),
    ];
    const QUESTION: &str = "这张图片是什么颜色？只回答颜色名称，不要解释。";
    let mut evidence = Vec::new();
    let mut all_correct = true;
    for (index, (color, chinese, english)) in IMAGES.iter().enumerate() {
        let data_url = match solid_png_data_url(*color) {
            Ok(url) => url,
            Err(error) => {
                return ProbeOutcome { verified: None, evidence: error, checked_at_ms: now_ms(), ..Default::default() }
            }
        };
        let body = image_probe_body(target, QUESTION, &data_url);
        match send(client, target, body).await {
            Ok(json) => {
                let text = extract_text(&target.protocol, &json);
                if text.trim().is_empty() {
                    return ProbeOutcome {
                        verified: None,
                        evidence: format!("{chinese}色图既无正文也无 reasoning_content，本次无法判定"),
                        checked_at_ms: now_ms(),
                        ..Default::default()
                    };
                }
                let lower = text.to_lowercase();
                let saw = lower.contains(english) || text.contains(chinese);
                let denied = ["看不到", "无法查看", "不能查看", "没有图片", "no image", "cannot see", "can't see"]
                    .iter()
                    .any(|needle| lower.contains(&needle.to_lowercase()));
                if !saw || denied {
                    all_correct = false;
                }
                evidence.push(format!("{chinese}图→{}", truncate(&text, 40)));
            }
            Err(error) => {
                return ProbeOutcome { verified: None, evidence: error, checked_at_ms: now_ms(), ..Default::default() }
            }
        }
        if index + 1 < IMAGES.len() {
            tokio::time::sleep(std::time::Duration::from_millis(PROBE_INTERVAL_MS)).await;
        }
    }
    ProbeOutcome {
        verified: Some(all_correct),
        evidence: evidence.join("；"),
        checked_at_ms: now_ms(),
        ..Default::default()
    }
}

/// 视觉探针的两种协议请求体。图片编码形态不同：Chat 用 `image_url` + data URL，
/// Responses 用 `input_image`。
fn image_probe_body(target: &ProbeTarget, question: &str, data_url: &str) -> serde_json::Value {
    if target.protocol == "openai-responses" {
        serde_json::json!({
            "model": target.model,
            "input": [{ "role": "user", "content": [
                { "type": "input_text", "text": question },
                { "type": "input_image", "image_url": data_url },
            ]}],
            "max_output_tokens": 512,
            "stream": false,
        })
    } else {
        serde_json::json!({
            "model": target.model,
            "messages": [{ "role": "user", "content": [
                { "type": "text", "text": question },
                { "type": "image_url", "image_url": { "url": data_url } },
            ]}],
            "max_tokens": 512,
            "stream": false,
        })
    }
}

/// 工具探针：一个空 tool 声明 + "调用它"，看有没有 `tool_call` 结构回来。
async fn probe_tools(client: &reqwest::Client, target: &ProbeTarget) -> ProbeOutcome {
    let tool = serde_json::json!({
        "type": "function",
        "function": {
            "name": "probe_capability",
            "description": "能力探针：请直接调用它，参数为空对象。",
            "parameters": { "type": "object", "properties": {}, "additionalProperties": false }
        }
    });
    let instruction = "请调用 probe_capability 工具，不要用文字回答。";
    let body = if target.protocol == "openai-responses" {
        serde_json::json!({
            "model": target.model,
            "input": [{ "role": "user", "content": instruction }],
            "tools": [ { "type": "function", "name": "probe_capability", "description": "能力探针", "parameters": { "type": "object", "properties": {} } } ],
            "tool_choice": "auto",
            "max_output_tokens": 64,
            "stream": false,
        })
    } else {
        serde_json::json!({
            "model": target.model,
            "messages": [{ "role": "user", "content": instruction }],
            "tools": [tool],
            "tool_choice": "auto",
            "max_tokens": 64,
            "stream": false,
        })
    };
    match send(client, target, body).await {
        Ok(json) => {
            let called = if target.protocol == "openai-responses" {
                json.get("output")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|items| items.iter().any(|i| i.get("type").and_then(|t| t.as_str()) == Some("function_call")))
            } else {
                json.pointer("/choices/0/message/tool_calls")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|calls| !calls.is_empty())
            };
            ProbeOutcome {
                verified: Some(called),
                evidence: if called {
                    "工具声明被端点接受并返回了 tool_call".into()
                } else {
                    "端点没有返回 tool_call（声明被忽略或模型未调用）".into()
                },
                checked_at_ms: now_ms(),
                ..Default::default()
            }
        }
        Err(error) => ProbeOutcome { verified: None, evidence: error, checked_at_ms: now_ms(), ..Default::default() },
    }
}

/// 推理档判据的三态（用户拍板）。**为什么必须分三态**：
///
/// - 高档明显高于低档 = 参数生效；
/// - 两档在噪声内 = 参数被忽略，或该模型本来就不区分档位；
/// - 高档明显**低于**低档 = 端点根本没识别这个字段。
///
/// 后两种都判"不生效"，但原因不同，文案必须分开——否则排查时会把"字段没被识别"
/// 误读成"模型行为如此"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningVerdict {
    Effective,
    Ignored,
    NotRecognized,
}

/// 判据本体（纯函数，单测直接验它，不在测试里另抄一份公式）。
///
/// 阈值 [`REASONING_NOISE_RATIO`]（5%）以**低档为基准**，正反两个方向同阈值：
/// 涨幅 ≥5% 算生效，跌幅 ≥5% 算"字段未被识别"，其余都算噪声内。
/// 返回 `(判定, 相对变化)`；反向时变化为负值。
pub fn judge_reasoning(low: u64, high: u64) -> (ReasoningVerdict, f64) {
    if low == 0 && high == 0 {
        return (ReasoningVerdict::Ignored, 0.0);
    }
    if high < low {
        let drop = (low - high) as f64 / low as f64;
        let verdict = if drop >= REASONING_NOISE_RATIO {
            ReasoningVerdict::NotRecognized
        } else {
            ReasoningVerdict::Ignored
        };
        return (verdict, -drop);
    }
    let rise = if low == 0 {
        f64::INFINITY
    } else {
        (high - low) as f64 / low as f64
    };
    let verdict = if rise >= REASONING_NOISE_RATIO {
        ReasoningVerdict::Effective
    } else {
        ReasoningVerdict::Ignored
    };
    (verdict, rise)
}

/// 推理探针：同一问题分别用最低档与最高档，比较 reasoning 指标。
///
/// 判据见 [`judge_reasoning`]；两次请求间隔 [`PROBE_INTERVAL_MS`]，避免触发限流。
async fn probe_reasoning(client: &reqwest::Client, target: &ProbeTarget, row: &ModelProfile) -> ProbeOutcome {
    let levels = row.declared_reasoning_levels(&target.base_url);
    let Some(low) = levels.first() else {
        return ProbeOutcome {
            verified: None,
            evidence: "只声明了一档推理等级，无法比较".into(),
            checked_at_ms: now_ms(),
            ..Default::default()
        };
    };
    let high = levels.last().unwrap();
    if low == high {
        return ProbeOutcome {
            verified: None,
            evidence: "只声明了一档推理等级，无法比较".into(),
            checked_at_ms: now_ms(),
            ..Default::default()
        };
    }
    let wire_of = |level: &str| row.reasoning_wire(&target.base_url, level).unwrap_or_default();
    let question = "用一句话说明：为什么天空看起来是蓝色的？";
    let (low_wire, high_wire) = (wire_of(low), wire_of(high));
    let low_result = reasoning_request(client, target, question, &low_wire).await;
    tokio::time::sleep(std::time::Duration::from_millis(PROBE_INTERVAL_MS)).await;
    let high_result = reasoning_request(client, target, question, &high_wire).await;
    let (low_metric, high_metric) = match (low_result, high_result) {
        (Ok((low_metric, low_name)), Ok((high_metric, high_name))) if low_name == high_name => {
            (low_metric, high_metric)
        }
        (Err(error), _) | (_, Err(error)) => {
            return ProbeOutcome { verified: None, evidence: error, checked_at_ms: now_ms(), ..Default::default() };
        }
        _ => {
            return ProbeOutcome {
                verified: None,
                evidence: "两档回复暴露的推理指标形态不同，无法同口径比较".into(),
                checked_at_ms: now_ms(),
                ..Default::default()
            };
        }
    };
    let (verdict, ratio) = judge_reasoning(low_metric, high_metric);
    let effective = verdict == ReasoningVerdict::Effective;
    // 单调上升说明"这个字段被读了"，因此声明里的其余档一并视为可用；
    // 判不生效时一个档都不给（选择器不该让人选一个没反应的档）。
    let verified_levels = if effective { levels.clone() } else { Vec::new() };
    let change = if ratio.is_finite() {
        format!("{:+.1}%", ratio * 100.0)
    } else {
        "低档为 0".to_string()
    };
    let evidence = match verdict {
        ReasoningVerdict::Effective => format!(
            "{low}档 {low_metric} → {high}档 {high_metric}（{change}，超出噪声阈值 {:.0}%）",
            REASONING_NOISE_RATIO * 100.0
        ),
        ReasoningVerdict::Ignored => format!(
            "{low}档 {low_metric} 与 {high}档 {high_metric} 差异 {change} 在噪声内：参数被忽略或该模型不区分档位"
        ),
        ReasoningVerdict::NotRecognized => format!(
            "{low}档 {low_metric}，{high}档反而 {high_metric}（{change}）：端点未识别该参数"
        ),
    };
    ProbeOutcome { verified: Some(effective), verified_levels, evidence, checked_at_ms: now_ms() }
}

/// 发一次带档位的最小请求，返回（指标值，指标名）。指标优先级：
/// `reasoning_tokens` → 推理文本长度 → 输出 token 数。三者都拿不到算失败，
/// 不能把「拿不到」当成「两档都是 0」。
async fn reasoning_request(
    client: &reqwest::Client,
    target: &ProbeTarget,
    question: &str,
    effort: &str,
) -> Result<(u64, &'static str), String> {
    let body = if target.protocol == "openai-responses" {
        serde_json::json!({
            "model": target.model,
            "input": [{ "role": "user", "content": question }],
            "reasoning": { "effort": effort },
            "max_output_tokens": 512,
            "stream": false,
        })
    } else {
        serde_json::json!({
            "model": target.model,
            "messages": [{ "role": "user", "content": question }],
            "reasoning_effort": effort,
            "max_tokens": 512,
            "stream": false,
        })
    };
    let json = send(client, target, body).await?;
    let reasoning_tokens = json
        .pointer("/usage/completion_tokens_details/reasoning_tokens")
        .or_else(|| json.pointer("/usage/output_tokens_details/reasoning_tokens"))
        .or_else(|| json.pointer("/usage/reasoning_tokens"))
        .and_then(serde_json::Value::as_u64);
    if let Some(tokens) = reasoning_tokens {
        return Ok((tokens, "reasoning_tokens"));
    }
    let reasoning_text = if target.protocol == "openai-responses" {
        json.get("output")
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter(|i| i.get("type").and_then(|t| t.as_str()) == Some("reasoning"))
                    .filter_map(|i| i.get("summary").and_then(serde_json::Value::as_array))
                    .flatten()
                    .filter_map(|s| s.get("text").and_then(serde_json::Value::as_str))
                    .map(str::len)
                    .sum::<usize>()
            })
            .unwrap_or(0)
    } else {
        json.pointer("/choices/0/message/reasoning_content")
            .or_else(|| json.pointer("/choices/0/message/reasoning"))
            .and_then(serde_json::Value::as_str)
            .map(str::len)
            .unwrap_or(0)
    };
    if reasoning_text > 0 {
        return Ok((reasoning_text as u64, "推理文本长度"));
    }
    let output_tokens = json
        .pointer("/usage/completion_tokens")
        .or_else(|| json.pointer("/usage/output_tokens"))
        .and_then(serde_json::Value::as_u64);
    match output_tokens {
        Some(tokens) => Ok((tokens, "输出 token 数")),
        None => Err("端点既没有 reasoning_tokens，也没有推理文本或 usage：无法同口径比较".into()),
    }
}

/// 从 Chat/Responses 的响应里取正文（探针只关心文本）。
///
/// 正文为空时**回读 `reasoning_content`**：推理型模型可能把答案写在推理内容里，
/// 只看 `content` 会把"答对了"误判成"没回答"。
fn extract_text(protocol: &str, json: &serde_json::Value) -> String {
    let text = if protocol == "openai-responses" {
        if let Some(text) = json.get("output_text").and_then(serde_json::Value::as_str) {
            text.to_string()
        } else {
            json.get("output")
                .and_then(serde_json::Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|i| i.get("content").and_then(serde_json::Value::as_array))
                        .flatten()
                        .filter_map(|c| c.get("text").and_then(serde_json::Value::as_str))
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default()
        }
    } else {
        match json.pointer("/choices/0/message/content") {
            Some(serde_json::Value::String(text)) => text.clone(),
            Some(serde_json::Value::Array(parts)) => parts
                .iter()
                .filter_map(|p| p.get("text").and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>()
                .join(""),
            _ => String::new(),
        }
    };
    if !text.trim().is_empty() {
        return text;
    }
    json.pointer("/choices/0/message/reasoning_content")
        .or_else(|| json.pointer("/choices/0/message/reasoning"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn truncate(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim().replace(['\n', '\r'], " ");
    if trimmed.chars().count() <= max_chars {
        return trimmed;
    }
    trimmed.chars().take(max_chars).collect::<String>() + "…"
}

/// 探针图片边长。256px 是实测下限：64px 会被部分模型误判成灰色。
const PROBE_IMAGE_SIDE: u32 = 256;

/// 纯色 PNG 的 data URL。每次现编码（几十字节级开销）比在源码里塞一段长 base64
/// 更可读，也避免"这张图到底是什么"无从复核。
fn solid_png_data_url(color: image::Rgb<u8>) -> Result<String, String> {
    let image = image::RgbImage::from_pixel(PROBE_IMAGE_SIDE, PROBE_IMAGE_SIDE, color);
    let mut bytes = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
        .map_err(|error| format!("生成探针图片失败: {error}"))?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:image/png;base64,{encoded}"))
}

// ============================== 宿主命令 ==============================

/// 探测某个服务商当前生效模型的能力（设置页"重新探测"与首次使用自动触发）。
///
/// 返回设置页可直接渲染的能力视图。网络失败不会让命令整体失败——每项如实回
/// "未验证 + 原因"，只有"读不到密钥"才报错（那是配置问题，不是能力问题）。
#[tauri::command]
pub async fn probe_capabilities(
    app: tauri::AppHandle,
    provider: String,
    model: Option<String>,
) -> Result<serde_json::Value, String> {
    let state = app.state::<crate::app_state::AppState>();
    let cfg = state.config();
    let row = cfg
        .providers
        .iter()
        .find(|p| p.id == provider)
        .ok_or_else(|| format!("服务商 {provider} 不在配置里"))?;
    let model_id = model.filter(|m| !m.trim().is_empty()).unwrap_or_else(|| row.model.clone());
    let model_row = row
        .models
        .iter()
        .find(|m| m.id == model_id)
        .ok_or_else(|| format!("服务商 {provider} 没有模型 {model_id}"))?
        .clone();
    let api_key = crate::config::SecretStore::new(&state.config_root())
        .load_key(&row.secret_name())?
        .ok_or_else(|| format!("未配置 {} 的 API Key，无法实测", row.id))?;
    let target = ProbeTarget {
        base_url: row.base_url.clone(),
        protocol: row.provider.clone(),
        model: model_row.id.clone(),
        headers: row.headers.clone(),
        api_key,
        token_budget_hint: model_row.max_tokens,
    };
    let key = target.cache_key();
    tracing::info!(provider = %row.id, model = %model_row.id, "开始能力实测");
    let outcomes = probe_all(&target, &model_row).await;
    let data_root = state.data_root();
    let mut cache = ProbeCache::load(&data_root);
    let view = model_view(&model_row, &row.base_url, Some(&outcomes));
    // 只播报"与声明不符"：这是用户最需要知道、也最容易静默失败的一类。
    for (item, outcome) in &outcomes {
        if outcome.verified == Some(false) {
            tracing::warn!(
                provider = %row.id,
                model = %model_row.id,
                capability = %item,
                evidence = %outcome.evidence,
                "能力实测与声明不符"
            );
        }
    }
    cache.put(key, outcomes);
    cache.save(&data_root);
    Ok(view)
}

/// 拉取端点给出的能力元数据（`GET /models/{id}`）。返回视图供设置页预填；
/// 端点没给就返回空对象——**静默降级**，不弹错（这是常态，不是失败）。
#[tauri::command]
pub async fn fetch_model_metadata(
    app: tauri::AppHandle,
    provider: String,
    model: String,
) -> Result<serde_json::Value, String> {
    let state = app.state::<crate::app_state::AppState>();
    let cfg = state.config();
    let row = cfg
        .providers
        .iter()
        .find(|p| p.id == provider)
        .ok_or_else(|| format!("服务商 {provider} 不在配置里"))?;
    let api_key = crate::config::SecretStore::new(&state.config_root())
        .load_key(&row.secret_name())?
        .ok_or_else(|| format!("未配置 {} 的 API Key，无法读取端点元数据", row.id))?;
    let meta = fetch_endpoint_metadata(row, &model, &api_key).await?;
    Ok(serde_json::json!({
        "contextWindow": meta.context_window,
        "maxOutput": meta.max_output,
        "inputModalities": meta.input_modalities,
        "tools": meta.supports_tools,
        "builtinWebSearch": meta.builtin_web_search,
        "supportedLevels": meta.supported_levels,
        "defaultReasoningLevel": meta.default_level,
        // 端点提供了任何一项 = true；全空时设置页显示"端点未提供能力信息"。
        "provided": !meta.is_empty(),
    }))
}

async fn fetch_endpoint_metadata(
    row: &ProviderProfile,
    model: &str,
    api_key: &str,
) -> Result<EndpointMetadata, String> {
    let base = row.base_url.trim().trim_end_matches('/');
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return Ok(EndpointMetadata::default());
    }
    let url = format!("{base}/models/{model}");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("HTTP 客户端不可用: {e}"))?;
    let mut request = client.get(&url).header("accept", "application/json");
    for (name, value) in &row.headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = match request.bearer_auth(api_key).send().await {
        Ok(response) => response,
        Err(error) => {
            // 元数据端点不可达是常态：按"没提供"处理，交给实测。
            tracing::info!(%url, %error, "端点能力元数据不可达，降级到探针");
            return Ok(EndpointMetadata::default());
        }
    };
    if !response.status().is_success() {
        tracing::info!(%url, status = %response.status(), "端点能力元数据不可用，降级到探针");
        return Ok(EndpointMetadata::default());
    }
    let bytes = response.bytes().await.map_err(|e| format!("读取 {url} 响应失败: {e}"))?;
    if bytes.len() > 1024 * 1024 {
        return Ok(EndpointMetadata::default());
    }
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Ok(EndpointMetadata::default());
    };
    // 有的端点把单模型信息包一层 `data`，两种形状都试。
    let item = json.get("data").filter(|v| v.is_object()).unwrap_or(&json);
    Ok(parse_metadata(item))
}

/// 首次使用时确保该模型有实测记录：缓存命中直接返回，不重复打请求。
/// 只用于后台预热——失败只记日志，不影响任务。
pub async fn ensure_probed(
    data_root: PathBuf,
    target: ProbeTarget,
    row: ModelProfile,
) {
    let mut cache = ProbeCache::load(&data_root);
    let key = target.cache_key();
    if cache.get(&key).is_some_and(|entry| !entry.is_empty()) {
        return;
    }
    tracing::info!(model = %target.model, "首次使用该模型，后台跑一次能力实测");
    let outcomes = probe_all(&target, &row).await;
    for (item, outcome) in &outcomes {
        if outcome.verified == Some(false) {
            tracing::warn!(
                model = %target.model,
                capability = %item,
                evidence = %outcome.evidence,
                "能力实测与声明不符（声明层不会自动改，请在设置页确认）"
            );
        }
    }
    cache.put(key, outcomes);
    cache.save(&data_root);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    /// 不在内置表里的端点：这些用例只验行内声明与视图逻辑。
    const BASE: &str = "https://example.test/v1";

    fn row() -> ModelProfile {
        ModelProfile {
            id: "m".into(),
            name: "m".into(),
            context_window: 128_000,
            max_tokens: 8_192,
            input: vec!["text".into(), "image".into()],
            reasoning_efforts: [
                ("low".to_string(), "low".to_string()),
                ("high".to_string(), "high".to_string()),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn view_reports_source_and_three_states() {
        let mut r = row();
        r.capabilities.insert("tools".into(), CapabilityRecord::declared(true, CapabilitySource::Endpoint));
        let view = model_view(&r, BASE, None);
        assert_eq!(view["vision"]["value"], true);
        // 值来自 input 字段、没有能力项记录 → 推断为手动声明
        assert_eq!(view["vision"]["source"], "manual");
        assert_eq!(view["vision"]["state"], "unverified");
        assert_eq!(view["tools"]["source"], "endpoint");
        assert_eq!(view["tools"]["value"], true);
        assert_eq!(view["reasoning"]["levels"], serde_json::json!(["low", "high"]));
        assert_eq!(view["context_window"]["value"], 128_000);
    }

    #[test]
    fn probe_result_overrides_state_and_limits_reasoning_levels() {
        let r = row();
        let mut outcomes = BTreeMap::new();
        outcomes.insert(
            "vision".to_string(),
            ProbeOutcome {
                verified: Some(false),
                evidence: "回复：蓝色".into(),
                checked_at_ms: 7,
                ..Default::default()
            },
        );
        outcomes.insert(
            "reasoning".to_string(),
            ProbeOutcome {
                verified: Some(true),
                verified_levels: vec!["low".into()],
                evidence: "低 10 / 高 200".into(),
                checked_at_ms: 7,
            },
        );
        let view = model_view(&r, BASE, Some(&outcomes));
        assert_eq!(view["vision"]["state"], "mismatch");
        assert_eq!(view["vision"]["evidence"], "回复：蓝色");
        assert_eq!(view["reasoning"]["verifiedLevels"], serde_json::json!(["low"]));
    }

    #[test]
    fn vision_effective_requires_declaration_and_not_refuted() {
        let mut r = row();
        assert!(r.vision_effective(BASE));
        r.capabilities.insert(
            "vision".into(),
            CapabilityRecord { verified: Some(false), ..Default::default() },
        );
        assert!(!r.vision_effective(BASE), "实测不符必须拦住工具注册");
        let mut plain = row();
        plain.input = vec!["text".into()];
        assert!(!plain.declared_vision(BASE));
    }

    #[test]
    fn builtin_layer_fills_values_with_source_and_date() {
        let base = "https://api.longcat.chat/openai/v1";
        let mut r = ModelProfile { id: "LongCat-2.5".into(), ..Default::default() };
        let view = model_view(&r, base, None);
        assert_eq!(view["vision"]["value"], true);
        assert_eq!(view["vision"]["source"], "builtin");
        assert_eq!(
            view["vision"]["catalogUpdatedAt"],
            crate::model_catalog::data_updated_at().unwrap(),
            "「内置表」徽标必须带数据日期"
        );
        assert_eq!(view["context_window"]["value"], 1_048_576);
        assert_eq!(view["context_window"]["source"], "builtin");

        // 本行手填 > 内置表：用户填的值不能被表覆盖
        r.context_window = 128_000;
        let view = model_view(&r, base, None);
        assert_eq!(view["context_window"]["value"], 128_000);
        assert_eq!(view["context_window"]["source"], "manual");
    }

    #[test]
    fn family_hit_is_inferred_and_stays_conservative() {
        let base = "https://api.longcat.chat/openai/v1";
        let r = ModelProfile { id: "LongCat-2.7".into(), ..Default::default() };
        let view = model_view(&r, base, None);
        assert_eq!(view["context_window"]["source"], "inferred", "家族命中必须标「推测」");
        // 家族只声明 text，等于推测"这一代没有视觉"：值 false、来源仍是推测而不是未知
        assert_eq!(view["vision"]["value"], false);
        assert_eq!(view["vision"]["source"], "inferred");
    }

    #[test]
    fn endpoint_levels_intersect_declared_levels() {
        let base = "https://api.deepseek.com";
        let mut r = ModelProfile { id: "deepseek-flash".into(), ..Default::default() };
        r.reasoning_efforts = std::collections::BTreeMap::from([
            ("low".to_string(), "low".to_string()),
            ("high".to_string(), "high".to_string()),
        ]);
        assert_eq!(r.declared_reasoning_levels(base), vec!["low".to_string(), "high".to_string()]);
        r.capabilities.insert(
            "reasoning".into(),
            CapabilityRecord { endpoint_levels: vec!["low".into()], ..Default::default() },
        );
        assert_eq!(
            r.declared_reasoning_levels(base),
            vec!["low".to_string()],
            "端点没列出的档不能出现在可选清单里"
        );
    }

    #[test]
    fn catalog_levels_become_wire_values_only_when_declared() {
        let base = "https://api.deepseek.com";
        let mut r = ModelProfile { id: "deepseek-v4-pro".into(), ..Default::default() };
        assert_eq!(r.reasoning_wire(base, "high").as_deref(), Some("high"));
        assert!(r.reasoning_wire(base, "medium").is_none(), "内置表没声明的档不发字段");
        // 本行声明档位后以本行为准（wire 值可不同名）
        r.reasoning_efforts =
            std::collections::BTreeMap::from([("high".to_string(), "high_v2".to_string())]);
        assert_eq!(r.reasoning_wire(base, "high").as_deref(), Some("high_v2"));
        assert!(r.reasoning_wire(base, "low").is_none(), "本行声明即封闭，不回落到内置表");
    }

    #[test]
    fn saving_preserves_sources_and_marks_manual_edits() {
        let mut previous_row = ModelProfile { id: "deepseek-flash".into(), ..Default::default() };
        previous_row.capabilities.insert(
            "vision".into(),
            CapabilityRecord::declared(true, CapabilitySource::Endpoint),
        );
        let previous = vec![ProviderProfile {
            id: "p".into(),
            models: vec![previous_row],
            ..Default::default()
        }];

        // 写面只带四个字段，`capabilities` 为空——这是设置页的真实形态。
        let mut next = previous.clone();
        next[0].models[0].capabilities.clear();
        next[0].models[0].context_window = 65_536;
        carry_sources(&previous, &mut next);

        let row = &next[0].models[0];
        assert_eq!(
            row.capabilities["vision"].source,
            CapabilitySource::Endpoint,
            "写面没带的来源记录必须继承，否则每次保存都把「端点」洗成「未知」"
        );
        assert_eq!(
            row.capabilities["context_window"].source,
            CapabilitySource::Manual,
            "用户改过的项要标手动"
        );
    }

    #[test]
    fn manual_source_is_not_overwritten_by_probe() {
        let mut r = ModelProfile { id: "m-manual".into(), ..Default::default() };
        r.context_window = 131_072;
        r.capabilities.insert(
            "context_window".into(),
            CapabilityRecord { source: CapabilitySource::Manual, ..Default::default() },
        );
        let mut outcomes = std::collections::BTreeMap::new();
        outcomes.insert(
            "context_window".to_string(),
            ProbeOutcome { verified: Some(false), ..Default::default() },
        );
        let view = model_view(&r, BASE, Some(&outcomes));
        assert_eq!(view["context_window"]["source"], "manual");
        assert_eq!(view["context_window"]["state"], "unverified", "手改之后探针不再覆盖");
    }

    #[test]
    fn metadata_parses_both_endpoint_shapes_and_ignores_wrong_types() {
        let list_shape = serde_json::json!({
            "id": "m",
            "context_window": 131072,
            "max_output_tokens": 8192,
            "input_modalities": ["text", "image", "video"],
        });
        let meta = parse_metadata(&list_shape);
        assert_eq!(meta.context_window, Some(131_072));
        assert_eq!(meta.max_output, Some(8_192));
        assert_eq!(meta.input_modalities.unwrap(), vec!["text", "image", "video"]);

        let single_shape = serde_json::json!({
            "id": "m",
            "context_length": "131072",
            "architecture": { "input_modalities": ["text", "image"] },
            "supported_parameters": ["max_tokens", "tools", "thinking"],
        });
        let meta = parse_metadata(&single_shape);
        // 类型不符（字符串）按缺失处理，不 panic、不猜
        assert_eq!(meta.context_window, None);
        assert_eq!(meta.supports_tools, Some(true));
        assert_eq!(meta.builtin_web_search, Some(false));
        assert_eq!(meta.input_modalities.unwrap(), vec!["text", "image"]);
    }

    #[test]
    fn endpoint_metadata_fills_only_missing_fields_and_tags_source() {
        let mut r = row();
        r.context_window = 0;
        r.max_tokens = 0;
        r.input.clear();
        let meta = EndpointMetadata {
            context_window: Some(32_000),
            max_output: Some(4_096),
            input_modalities: Some(vec!["text".into(), "image".into(), "video".into()]),
            supports_tools: Some(true),
            builtin_web_search: Some(false),
            supported_levels: Some(vec!["low".into(), "high".into()]),
            default_level: Some("high".into()),
        };
        apply_endpoint_metadata(&mut r, &meta);
        assert_eq!(r.context_window, 32_000);
        assert_eq!(r.max_tokens, 4_096);
        // video 不映射成能力
        assert_eq!(r.input, vec!["text".to_string(), "image".to_string()]);
        assert_eq!(r.capabilities["tools"].value, Some(true));
        assert_eq!(r.capabilities["tools"].source, CapabilitySource::Endpoint);
        // 已有值不覆盖（手动 > 端点）
        let mut keep = row();
        apply_endpoint_metadata(&mut keep, &meta);
        assert_eq!(keep.context_window, 128_000);
    }

    #[test]
    fn cache_key_changes_with_any_identity_component_and_never_contains_key() {
        let a = cache_key("https://x/v1", "openai-chat", "m1", "sk-secret-aaaa");
        assert!(!a.contains("sk-secret-aaaa"), "缓存键不得包含密钥明文");
        assert_ne!(a, cache_key("https://x/v1", "openai-chat", "m1", "sk-secret-bbbb"));
        assert_ne!(a, cache_key("https://y/v1", "openai-chat", "m1", "sk-secret-aaaa"));
        assert_ne!(a, cache_key("https://x/v1", "openai-responses", "m1", "sk-secret-aaaa"));
        assert_ne!(a, cache_key("https://x/v1", "openai-chat", "m2", "sk-secret-aaaa"));
    }

    #[test]
    fn reasoning_verdict_is_three_state_and_monotonic() {
        // 阈值 5% 以低档为基准：4% 噪声内，5% 起算超出。
        assert_eq!(judge_reasoning(0, 0), (ReasoningVerdict::Ignored, 0.0));
        assert_eq!(judge_reasoning(100, 104).0, ReasoningVerdict::Ignored, "4% 差异在噪声内");
        assert_eq!(judge_reasoning(100, 105).0, ReasoningVerdict::Effective, "5% 算生效");
        assert_eq!(judge_reasoning(10, 200).0, ReasoningVerdict::Effective);
        assert_eq!(judge_reasoning(0, 50).0, ReasoningVerdict::Effective, "低档 0、高档有推理也算生效");
        // Sensenova 实测 254 → 7：高档反而更少 = 字段没被识别，不能报成「模型不区分档位」。
        let (verdict, ratio) = judge_reasoning(254, 7);
        assert_eq!(verdict, ReasoningVerdict::NotRecognized);
        assert!(ratio < 0.0, "反向时变化量为负，文案据此写成「反而」");
        assert_eq!(judge_reasoning(100, 97).0, ReasoningVerdict::Ignored, "3% 反向仍在噪声内");
    }

    #[test]
    fn probe_cache_round_trips_and_tolerates_corruption() {
        let dir = crate::test_support::TestTempDir::new("capability-cache");
        let mut cache = ProbeCache::default();
        let mut entry = BTreeMap::new();
        entry.insert(
            "tools".to_string(),
            ProbeOutcome { verified: Some(true), evidence: "ok".into(), checked_at_ms: 1, ..Default::default() },
        );
        cache.put("k".into(), entry);
        cache.save(&dir);
        let loaded = ProbeCache::load(&dir);
        assert_eq!(loaded.get("k").unwrap()["tools"].verified, Some(true));
        // 损坏文件按空缓存处理，不 panic
        std::fs::write(dir.join(crate::config::CACHE_DIR).join(CACHE_FILE), "{not json").unwrap();
        assert!(ProbeCache::load(&dir).entries.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capabilities_round_trip_through_config_save() {
        let dir = crate::test_support::TestTempDir::new("capability-config");
        let mut cfg = AppConfig::default();
        let mut r = row();
        r.capabilities.insert("tools".into(), CapabilityRecord::declared(true, CapabilitySource::Endpoint));
        let provider = ProviderProfile {
            id: "p".into(),
            base_url: "https://x/v1".into(),
            model: "m".into(),
            models: vec![r],
            ..Default::default()
        };
        cfg.providers = vec![provider];
        cfg.active_provider = "p".into();
        let path = dir.join("config.toml");
        cfg.save(&path).expect("带能力项的配置必须能落盘并读回");
        let back: AppConfig = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back.providers[0].models[0].capabilities["tools"].value, Some(true));
        assert_eq!(back.providers[0].models[0].capabilities["tools"].source, CapabilitySource::Endpoint);
        assert!(back.providers[0].models[0].vision_effective(&cfg.providers[0].base_url));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
