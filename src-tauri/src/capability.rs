//! 模型能力声明与实测探针（SH-132，docs/04 §4.1）。
//!
//! 为什么"声明"不够：`web_search_preview` 有一次活证据——声明了、端点 200、
//! **静默忽略**。能力声明会以同样方式坏：填错就是自欺，而且坏得没有声音。
//! 所以这里把能力拆成两栏：
//!
//! - **声明**（`ModelProfile::capabilities`）：值取模型行既有字段，记录"谁说的"
//!   （端点元数据 / 手动）。
//! - **实测**（本模块的探针 + `<data_root>\cache\capabilities.json` 缓存）：只记录
//!   "测出来是什么"。两者不互相覆盖，设置页并列显示四态（检测中 / 已验证 /
//!   与声明不符 / 未验证）。
//!
//! 缓存键 = `base_url + 协议 + model id + 密钥指纹 + 探针版本`：换 key / 换端点 /
//! 换模型 / 改探针即自然失效并重探；同一组合只探一次，不每次启动都打请求。密钥
//! 只以 SHA-256 前 16 位十六进制参与键，明文不进缓存、不进日志。
//!
//! 探针版本进键的原因：改了题面 / 判据 / 采样次数却不作废旧结论，等于拿旧尺子量
//! 新东西——改进本身没法被验证，还会让"探针已改好"与"缓存里还是旧答案"长期共存。

use std::collections::{BTreeMap, HashSet};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::Manager;

use crate::config::{CapabilityRecord, CapabilitySource, ModelProfile, ProviderProfile};

pub const CACHE_FILE: &str = "capabilities.json";
/// 推理探针版本号：题面 + 判据 + 每档采样次数 + 输出上限一起算出的常量。
/// **它必须进缓存键**：改探针就等于换了一把尺子，旧结论一律作废重测。
/// 否则改进探针只是对着旧答案自欺——"探针已修好"与"缓存里还是旧结论"会长期共存。
pub const PROBE_VERSION: &str = "2026-11-09/arith-selfcheck/cap512/n3/r15/k2/threshold3";
/// 推理指标（`reasoning_tokens`）的相对噪声**下限**：噪声带至少占两档较大中位数的
/// 15%。
///
/// 为什么是 15% 而不是 5%：5% 是按**单样本**噪声定的，约束不了**跨轮**漂移。
/// 2026-11-09 同一配置连续多轮实测里，同一端点的两档中位数本身就在 low 69–92 /
/// high 113–136 之间漂移（约为量级的 ±20%）。单轮 3 个样本看不见这种漂移：
/// round 1 的高档三样本恰好是 [104,124,125]，MAD 只有 1，于是 6.5 的效应/噪声比
/// 把一次幸运采成了「生效」，而其余各轮全是「数据不足」。下限抬到 15% 之后，
/// 判据要求高档/低档 ≥ 约 1.82× 才敢说"生效"——低于它就如实说不可判。
/// 这个数不是拍的，是"跨轮中位数漂移实测 ~20%，取下界 15%"。
/// **换端点或换题后必须重估**：它标定的是「同一端点、同一题面的跨轮抖动」，
/// 换了模型家族或改了题面，漂移幅度就不再是这 20%——照搬会变成凭空的魔法数。
pub const REASONING_NOISE_RATIO: f64 = 0.15;
/// 每档采样次数。偶数样本的中位数取中间两值均值，稳定性差；3 是能取中位数的
/// 最小奇数，也是"一次请求失败还剩下 2 个"之外的硬门槛——少于 3 个直接判数据不足。
pub const REASONING_SAMPLES: usize = 3;
/// 噪声带对档内离散度的放大系数：噪声带 = max(15% × 规模, 2 × 档内极差)。
///
/// 用极差、不用 MAD：**n=3 时 MAD 会被紧采样压到 0/1，不能用作离散度估计**——
/// 一次幸运的紧采样（round 1 的高档 [104,124,125]，MAD=1）就让噪声带塌陷，
/// 把抖动判成了效应。极差不会低估这组样本自身的波动。
/// 乘 2 是因为 3 个样本的极差仍只是整体波动的下界估计，再留一倍余量。
pub const NOISE_DISPERSION_K: f64 = 2.0;
/// 档内相对离散度上限：极差 / 中位数 > 50% 说明这一档自己就抖得说不出话，
/// 直接判「数据不足」，不许拿它去比方向。
pub const MAX_RELATIVE_DISPERSION: f64 = 0.5;
/// 允许下**确定结论**的最小「效应/噪声」比。
///
/// 门槛从 2 抬到 3 的不对称理由：声称"这个功能可用"需要的证据，比声称"我不知道"
/// 多。2026-11-09 概率题一轮比值 10.28 判生效、另一轮 0.79 判不足——把 10.28 那轮
/// 固化，运气好的一次就被写成事实，而且它不会自己暴露（不像"不可判"可以重测）。
/// 所以宁可多落「不可判」，也不让一次侥幸变成永久结论。
pub const EFFECT_NOISE_MIN: f64 = 3.0;
/// 推理探针每次请求的输出上限。短算式题（low 实测 78–91、max 91–116）零触顶；
/// 旧版用长链题时 low 就贴到 507/1022，截断会伪造结论，所以上限不是越小越省。
pub const REASONING_MAX_TOKENS: u32 = 512;
/// 推理段单独的时间预算（秒）。6 次请求在 ~1.2s/次的端点上约 7s；超预算记
/// 「数据不足」而不是让整卡 30s 总超时把视觉/工具的结论一起吞掉。
pub const REASONING_BUDGET_SECS: u64 = 20;
/// 两次推理探针请求的最小间隔。同端点连着打两发容易被限流，实测 1s 以上稳定。
pub const PROBE_INTERVAL_MS: u64 = 1_100;
/// 单模型一次检测的总超时。超时记 `verified=None` + 原因、**不无限等**：
/// 探针是后台增强，任何用户路径都不该被它拖住。
pub const PROBE_TOTAL_TIMEOUT_SECS: u64 = 30;

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
    /// 推理探针每次采样的原始指标值，按档位分组（如 `{"low": [56, 61, 60]}`）。
    /// **不存结论了事**：出现翻案时，要能一眼分出是数据抖了还是判据写错了。
    pub samples: BTreeMap<String, Vec<u64>>,
    /// 推理探针每档**触顶**（`completion_tokens >= max_tokens`）的样本数。
    /// 截断会伪造结论：贴顶的样本数值不可信，必须进缓存、进证据文案。
    pub censored: BTreeMap<String, u32>,
    /// 「不可判」终态：数据已采完，但判据给不出确定结论（比值不足 / 触顶）。
    /// 与瞬时失败（网络 / 429 / 超时）严格区分：只有它才是终态、不再自动重探；
    /// 瞬时失败下一轮还会再试。设置页可对终态手动重探一次。
    pub unjudgeable: bool,
    pub evidence: String,
    pub checked_at_ms: u64,
    /// 本项探针实际耗时（毫秒）。`None` = 没测到（历史缓存、请求失败、图片编码失败），
    /// 不是 0ms——真机取证要按项贴耗时，混进"0"会被读成"没打请求"。
    pub elapsed_ms: Option<u64>,
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

    /// 手动重探前清掉该键：只作废这一条，不牵连别的模型。
    pub fn remove(&mut self, key: &str) {
        self.entries.remove(key);
    }
}

/// 正在跑的探针（按缓存键）。设置页据此把「检测中」与「未验证」分开：
/// 前者是还没测完，后者是测过但没测成——混成一个词，用户会读成"能力不可用"。
#[derive(Default)]
pub struct Inflight {
    keys: Mutex<HashSet<String>>,
}

impl Inflight {
    /// 登记一次探测；已在跑时返回 `false`。重复点击 / 重复启动都走这里，
    /// 因此不会对同一模型并发打两轮请求、烧两份 token。
    pub fn begin(&self, key: &str) -> bool {
        self.keys.lock().expect("探针在跑集合锁中毒").insert(key.to_string())
    }

    pub fn end(&self, key: &str) {
        self.keys.lock().expect("探针在跑集合锁中毒").remove(key);
    }

    pub fn contains(&self, key: &str) -> bool {
        self.keys.lock().expect("探针在跑集合锁中毒").contains(key)
    }
}

/// 提前返回（缓存命中 / 超时 / panic 展开）时也要擦掉登记，
/// 否则设置页会永远停在「检测中」。
struct InflightGuard<'a> {
    inflight: &'a Inflight,
    key: &'a str,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.inflight.end(self.key);
    }
}

/// 缓存键：端点 + 协议 + 模型 + 密钥指纹 + **探针版本**。任何一项变了就是另一条
/// 记录，旧结论自然失效——不用额外写"失效条件"的代码，键本身就是失效条件。
pub fn cache_key(base_url: &str, protocol: &str, model: &str, api_key: &str) -> String {
    cache_key_for(base_url, protocol, model, api_key, PROBE_VERSION)
}

/// 带版本参数的键构造。生产走 [`cache_key`]；这里单独留一个口子，单测才能证明
/// "版本变了就落到另一条记录"，而不是只在注释里声称。
fn cache_key_for(base_url: &str, protocol: &str, model: &str, api_key: &str, version: &str) -> String {
    format!("{base_url}\u{1f}{protocol}\u{1f}{model}\u{1f}{}\u{1f}{version}", key_fingerprint(api_key))
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

/// 设置页看到的一行能力：值 + 来源 + 四态（检测中 / 已验证 / 不符 / 未验证）。
///
/// 来源优先级（高 → 低）：手动 > 端点 > 内置表 > 推测 > 未知。实测只改 `state`，
/// **从不改**声明值——"填错了"必须被保留在界面上，而不是被一次探测洗成"对的"。
pub fn model_view(
    row: &ModelProfile,
    base_url: &str,
    probed: Option<&BTreeMap<String, ProbeOutcome>>,
    probing: bool,
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
        // 布尔项的声明值先算出来，等来源解析完再决定写不写进视图：
        // 「来源：未声明、值：无」会把"没人说过"读成"明确没有"。
        let mut flag_value: Option<bool> = None;
        let mut number_value: Option<u32> = None;
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
                flag_value = Some(value);
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
                    _ => row.max_tokens_effective(base_url),
                };
                local = match item.id {
                    "context_window" => row.context_window > 0,
                    _ => row.max_tokens > 0,
                };
                number_value = Some(value);
            }
        }
        let source = declared_source(
            row,
            item.id,
            local,
            builtin_source.filter(|_| builtin_declares(item.id, builtin_fields)),
        );
        if let Some(value) = flag_value {
            if source != CapabilitySource::Unknown {
                entry.insert("value".into(), serde_json::Value::Bool(value));
            }
        }
        // 数值项同口径：没人声明、也没内置值时值为 0，但 0 不是声明值。
        if let Some(value) = number_value {
            if value > 0 || source != CapabilitySource::Unknown {
                entry.insert("value".into(), serde_json::Value::from(value));
            }
        }
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
        // 「不可判」是**终态**：数据采完了但判据给不出结论（比值不足 / 触顶），
        // 不是"没测过"。它与 `unverified` 必须分开，否则用户会以为探针坏了或者
        // 能力不可用——实际上它只是这个问题在这个端点上没法回答。
        let unjudgeable = outcome.is_some_and(|o| o.unjudgeable);
        let state = match verified {
            Some(true) => "verified",
            Some(false) => "mismatch",
            // 「检测中」不是「未验证」：前者还没测完，后者测过但没测成。
            // 手动的项不采纳探针结论，对它显示"检测中"只会误导；没有探针的项
            // （上下文窗口 / 输出上限 / 托管搜索）永远是「未验证」，不能装作在测。
            None if probing && !manual_record && item.probe => "probing",
            None if unjudgeable && !manual_record && item.probe => "unjudgeable",
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
    let started = std::time::Instant::now();
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
        elapsed_ms: Some(started.elapsed().as_millis() as u64),
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
    let started = std::time::Instant::now();
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
                elapsed_ms: Some(started.elapsed().as_millis() as u64),
                ..Default::default()
            }
        }
        Err(error) => ProbeOutcome { verified: None, evidence: error, checked_at_ms: now_ms(), ..Default::default() },
    }
}

/// 推理档判据的三态 + 「不可判」。**为什么必须留第四态**：
///
/// - 高档中位数**显著**高于低档（效应/噪声 ≥ [`EFFECT_NOISE_MIN`] 且零触顶）= 参数生效；
/// - 高档中位数**显著**低于低档 = 端点根本没识别这个字段；
/// - **不可判 = 我不知道**：效应/噪声不足、任一档触顶、样本不够、档内抖得离谱。
///
/// 只有"生效/不生效"两个出口时，任何不稳定数据都得被强行归到一边，于是必然翻案。
/// 2026-11-09 同一端点先判 56→191「生效」、再判 98→66「未识别」——两次相反的确定结论。
/// 因此「不生效」不再是一个可下结论的出口：说"它没用"和说"它有用"一样需要证据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningVerdict {
    Effective,
    NotRecognized,
    Insufficient,
}

/// 第四态的具体成因。判据与证据文案共用同一份判断，避免两处各写一套顺序、
/// 各说各话（2026-11-09 就出现过"判不足、文案却写噪声带"的错位）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsufficientReason {
    /// 每档样本少于 [`REASONING_SAMPLES`]。
    NotEnoughSamples,
    /// 档内相对离散度过大（极差 / 中位数 > [`MAX_RELATIVE_DISPERSION`]）。
    TooDispersed,
    /// 噪声带被档内离散度撑起，差异与抖动分不开。
    BandDominated,
    /// 效应/噪声比低于 [`EFFECT_NOISE_MIN`]：不足以声称"生效"，也不足以声称"没用"。
    BelowThreshold,
    /// 任一档有样本触顶（`completion_tokens >= max_tokens`）：截断会伪造结论。
    Censored,
    /// 推理段预算用尽。属**瞬时**失手，不是终态，下一轮还会重试。
    BudgetExhausted,
}

impl InsufficientReason {
    /// 是否是可以写死的「不可判」终态。
    ///
    /// 与瞬时失败的分界：数据采完、结论由判据给出 → 终态；请求没跑完（预算 / 样本
    /// 不足）→ 下一轮还能再试。把两者混成一个"数据不足"，就分不清"这个端点测不出"
    /// 与"这次运气不好"。
    fn is_terminal(self) -> bool {
        !matches!(self, Self::NotEnoughSamples | Self::BudgetExhausted)
    }
}

/// 判据的全部中间量。单测拿它验数值，界面/日志拿它写证据。
#[derive(Debug, Clone, PartialEq)]
pub struct ReasoningJudgement {
    pub verdict: ReasoningVerdict,
    /// 仅 `Insufficient` 时有值：这次为什么判不出来。
    pub reason: Option<InsufficientReason>,
    pub low_median: f64,
    pub high_median: f64,
    /// 噪声带（指标绝对值）：|中位数差| 小于它就不可判方向。
    pub band: f64,
    /// 两档里较大的档内极差。
    pub dispersion: f64,
    /// 相对变化（高档中位数 / 低档中位数 - 1）；低档为 0 时是 `INFINITY`。
    pub ratio: f64,
    /// 效应/噪声比：`|中位数差| / 噪声带`。只有它 ≥ [`EFFECT_NOISE_MIN`] 才允许下
    /// 确定结论——它同时约束了效应量与抖动，比单看差多少更稳。
    pub effect_over_noise: f64,
}

/// 取中位数（样本为空时 0）。
fn median_of(mut values: Vec<f64>) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        values[mid]
    } else {
        (values[mid - 1] + values[mid]) / 2.0
    }
}

/// 档内离散度：极差（max − min）。
///
/// **不能用 MAD**：n=3 时 MAD 会被紧采样压到 0/1，不能用作离散度估计。上一版用
/// MAD 时，round 1 的高档 [104,124,125] 恰好很紧（MAD=1），噪声带塌到 6.2，
/// 一次幸运采样就被判成「生效」。极差不会低估这组样本自身的波动。
fn range_of(samples: &[u64]) -> f64 {
    match (samples.iter().min(), samples.iter().max()) {
        (Some(min), Some(max)) => (*max as f64) - (*min as f64),
        _ => 0.0,
    }
}

/// 判据本体（纯函数，单测直接验它，不在测试里另抄一份公式）。
///
/// 噪声带 = `max(15% × 规模, 2 × 档内极差)`，规模取两档中位数较大者。
/// 这样"模型想多深"本身带来的抖动会被算进噪声，而不是被当成效果。
///
/// 顺序即优先级：**先证明数据可信（样本够、不抖、没触顶），再看效应是否大过噪声**。
/// `censored` 是两档触顶样本的总数——只要有一个，数值就被截断过，结论不可信。
pub fn judge_reasoning_samples(low: &[u64], high: &[u64], censored: u32) -> ReasoningJudgement {
    let low_median = median_of(low.iter().map(|v| *v as f64).collect());
    let high_median = median_of(high.iter().map(|v| *v as f64).collect());
    let low_dispersion = range_of(low);
    let high_dispersion = range_of(high);
    let dispersion = low_dispersion.max(high_dispersion);
    let scale = low_median.max(high_median);
    let band = (REASONING_NOISE_RATIO * scale).max(NOISE_DISPERSION_K * dispersion);
    let difference = high_median - low_median;
    let ratio = if low_median > 0.0 {
        difference / low_median
    } else if high_median > 0.0 {
        f64::INFINITY
    } else {
        0.0
    };
    let effect_over_noise = if band > 0.0 { difference.abs() / band } else { f64::INFINITY };
    let judgement = |verdict, reason| ReasoningJudgement {
        verdict,
        reason,
        low_median,
        high_median,
        band,
        dispersion,
        ratio,
        effect_over_noise,
    };
    if low.len() < REASONING_SAMPLES || high.len() < REASONING_SAMPLES {
        return judgement(ReasoningVerdict::Insufficient, Some(InsufficientReason::NotEnoughSamples));
    }
    // 触顶的样本数值不可信：截断会把"预算不够"伪装成"想得少"。
    if censored > 0 {
        return judgement(ReasoningVerdict::Insufficient, Some(InsufficientReason::Censored));
    }
    // 档内自己就抖得说不清话，拿它去比方向只会得到随机结论。
    let noisy = |median: f64, spread: f64| median > 0.0 && spread / median > MAX_RELATIVE_DISPERSION;
    if noisy(low_median, low_dispersion) || noisy(high_median, high_dispersion) {
        return judgement(ReasoningVerdict::Insufficient, Some(InsufficientReason::TooDispersed));
    }
    // 证据门槛：比值不足时，"生效"与"没用"都不许说。
    if effect_over_noise < EFFECT_NOISE_MIN {
        // 噪声带主要是被档内离散度撑起来的：连"差异是否存在"都说不清。
        if NOISE_DISPERSION_K * dispersion > REASONING_NOISE_RATIO * scale {
            return judgement(ReasoningVerdict::Insufficient, Some(InsufficientReason::BandDominated));
        }
        return judgement(ReasoningVerdict::Insufficient, Some(InsufficientReason::BelowThreshold));
    }
    if difference > 0.0 {
        judgement(ReasoningVerdict::Effective, None)
    } else {
        judgement(ReasoningVerdict::NotRecognized, None)
    }
}

/// 推理探针：同一问题在最低档 / 最高档各采 [`REASONING_SAMPLES`] 次，**交错发**，
/// 用中位数比较；判据见 [`judge_reasoning_samples`]。
///
/// 交错采样而不是"先测完低档再测高档"：模型有预热效应，同一批请求越往后答案往往
/// 越长，分组采样会把这种漂移记到高档头上，凭空造出"生效"。
/// 整段受 [`REASONING_BUDGET_SECS`] 预算约束，超预算记「数据不足」，已采样本照样留档。
async fn probe_reasoning(client: &reqwest::Client, target: &ProbeTarget, row: &ModelProfile) -> ProbeOutcome {
    let started = std::time::Instant::now();
    let levels = row.declared_reasoning_levels(&target.base_url);
    let Some(low) = levels.first().cloned() else {
        return ProbeOutcome {
            verified: None,
            evidence: "只声明了一档推理等级，无法比较".into(),
            checked_at_ms: now_ms(),
            ..Default::default()
        };
    };
    let high = levels.last().cloned().unwrap_or_default();
    if low == high {
        return ProbeOutcome {
            verified: None,
            evidence: "只声明了一档推理等级，无法比较".into(),
            checked_at_ms: now_ms(),
            ..Default::default()
        };
    }
    let wire_of = |level: &str| row.reasoning_wire(&target.base_url, level).unwrap_or_default();
    // 短算式 + 自检：题面必须**只需要短链推理**，低档才不会被预算截断；同时要求
    // 自检一遍，让"多想想"有可观测的落点。旧长链题 low 实测 507/1022，贴顶后
    // 截断会伪造结论——题面与上限是一起定的，不能单改一个。
    // 答案 162（17×23−19×19=30；30×4=120；7!÷5!=42）。
    let question = "请先计算，再自检一遍，只输出最终数字：((17×23−19×19)×4+7!÷5!)";
    let (low_wire, high_wire) = (wire_of(&low), wire_of(&high));
    let mut low_samples: Vec<u64> = Vec::with_capacity(REASONING_SAMPLES);
    let mut high_samples: Vec<u64> = Vec::with_capacity(REASONING_SAMPLES);
    let mut low_censored = 0u32;
    let mut high_censored = 0u32;
    let mut metric_name: Option<&'static str> = None;

    let sampling = async {
        for round in 0..REASONING_SAMPLES {
            let gap = std::time::Duration::from_millis(PROBE_INTERVAL_MS);
            if round > 0 {
                tokio::time::sleep(gap).await;
            }
            let low_sample = reasoning_request(client, target, question, &low_wire).await?;
            tokio::time::sleep(gap).await;
            let high_sample = reasoning_request(client, target, question, &high_wire).await?;
            if low_sample.metric != high_sample.metric {
                return Err("两档回复暴露的推理指标形态不同，无法同口径比较".to_string());
            }
            if metric_name.is_some_and(|existing| existing != low_sample.metric) {
                return Err("多轮采样暴露的推理指标形态不一致，无法同口径比较".to_string());
            }
            metric_name = Some(low_sample.metric);
            if low_sample.censored {
                low_censored += 1;
            }
            if high_sample.censored {
                high_censored += 1;
            }
            low_samples.push(low_sample.value);
            high_samples.push(high_sample.value);
        }
        Ok(())
    };

    let budget = std::time::Duration::from_secs(REASONING_BUDGET_SECS);
    let budget_hit = match tokio::time::timeout(budget, sampling).await {
        Ok(Ok(())) => false,
        Ok(Err(error)) => {
            return ProbeOutcome {
                verified: None,
                evidence: error,
                checked_at_ms: now_ms(),
                elapsed_ms: Some(started.elapsed().as_millis() as u64),
                ..Default::default()
            };
        }
        Err(_) => true,
    };

    let censored_total = low_censored + high_censored;
    let mut judgement = judge_reasoning_samples(&low_samples, &high_samples, censored_total);
    if budget_hit && judgement.reason.is_none() {
        // 预算用尽但样本恰好采满：仍按"这次没测成"处理，不把半程数据当结论。
        judgement.verdict = ReasoningVerdict::Insufficient;
        judgement.reason = Some(InsufficientReason::BudgetExhausted);
    }
    let samples = BTreeMap::from([
        (low.clone(), low_samples.clone()),
        (high.clone(), high_samples.clone()),
    ]);
    let censored = BTreeMap::from([(low.clone(), low_censored), (high.clone(), high_censored)]);
    let change = if judgement.ratio.is_finite() {
        format!("{:+.1}%", judgement.ratio * 100.0)
    } else {
        "低档为 0".to_string()
    };
    // 证据里必须同时有中位数、原始样本、噪声带、效应/噪声比和触顶数——只给结论
    // 等于要求用户相信一个无法复核的断言。
    let numbers = format!(
        "{low}档 中位 {:.0}（样本 {low_samples:?}） / {high}档 中位 {:.0}（样本 {high_samples:?}），噪声带 ±{:.1}，效应/噪声 {:.1}{}",
        judgement.low_median,
        judgement.high_median,
        judgement.band,
        judgement.effect_over_noise,
        if censored_total > 0 {
            format!("，触顶 {censored_total} 个（{low} {low_censored} / {high} {high_censored}）")
        } else {
            String::new()
        }
    );
    let evidence = match judgement.verdict {
        ReasoningVerdict::Effective => {
            format!("{numbers}：{change} 显著超出噪声带，参数生效")
        }
        ReasoningVerdict::NotRecognized => {
            format!("{numbers}：{change}，方向为负且显著超出噪声带——端点未识别该参数")
        }
        ReasoningVerdict::Insufficient => {
            // 成因由判据给出，不在这里另写一套顺序：两处各判一次必然走样。
            let reason = match judgement.reason {
                Some(InsufficientReason::NotEnoughSamples) => format!(
                    "样本不足（各档需 {REASONING_SAMPLES} 个，实得 {}/{}）",
                    low_samples.len(),
                    high_samples.len()
                ),
                Some(InsufficientReason::TooDispersed) => format!(
                    "档内离散度过大（极差 {:.1}，中位 {:.0}/{:.0}，上限 {:.0}%）",
                    judgement.dispersion,
                    judgement.low_median,
                    judgement.high_median,
                    MAX_RELATIVE_DISPERSION * 100.0
                ),
                Some(InsufficientReason::BandDominated) => format!(
                    "噪声带被档内极差撑起（极差 {:.1}，噪声带 ±{:.1}），分不清「没效果」与「数据太抖」",
                    judgement.dispersion, judgement.band
                ),
                Some(InsufficientReason::BelowThreshold) => format!(
                    "效应/噪声 {:.1} 低于门槛 {EFFECT_NOISE_MIN:.0}：证据不足以声称「生效」，也不足以声称「没用」",
                    judgement.effect_over_noise
                ),
                Some(InsufficientReason::Censored) => format!(
                    "有 {censored_total} 个样本触顶（上限 {REASONING_MAX_TOKENS}），截断的数据不可信"
                ),
                Some(InsufficientReason::BudgetExhausted) => {
                    format!("推理段 {REASONING_BUDGET_SECS}s 预算用尽")
                }
                None => "判据未给出成因".to_string(),
            };
            format!("{numbers}：{reason}——数据不足，无法判定")
        }
    };
    let effective = judgement.verdict == ReasoningVerdict::Effective;
    let unjudgeable = judgement
        .reason
        .is_some_and(InsufficientReason::is_terminal)
        && judgement.verdict == ReasoningVerdict::Insufficient;
    ProbeOutcome {
        // 「不可判」必须是 `None`：它既不是"生效"也不是"不生效"。
        verified: if judgement.verdict == ReasoningVerdict::Insufficient {
            None
        } else {
            Some(effective)
        },
        // 单调上升说明"这个字段被读了"，因此声明里的其余档一并视为可用；
        // 判不生效或不可判时一个档都不给（选择器不该让人选一个没反应/没验证过的档）。
        verified_levels: if effective { levels.clone() } else { Vec::new() },
        samples,
        censored,
        unjudgeable,
        evidence,
        checked_at_ms: now_ms(),
        elapsed_ms: Some(started.elapsed().as_millis() as u64),
    }
}

/// 一次推理采样的结果。`censored` = 本次输出贴到上限，数值已被截断。
struct ReasoningSample {
    value: u64,
    metric: &'static str,
    censored: bool,
}

/// 发一次带档位的最小请求。指标优先级：
/// `reasoning_tokens` → 推理文本长度 → 输出 token 数。三者都拿不到算失败，
/// 不能把「拿不到」当成「两档都是 0」。
///
/// **触顶判定用 `completion_tokens`，不是 `reasoning_tokens`**：推理 token 只占
/// 输出的一部分，正文也吃预算。只有总输出贴到 `max_tokens` 才说明这次被截断。
async fn reasoning_request(
    client: &reqwest::Client,
    target: &ProbeTarget,
    question: &str,
    effort: &str,
) -> Result<ReasoningSample, String> {
    let cap = REASONING_MAX_TOKENS;
    let body = if target.protocol == "openai-responses" {
        serde_json::json!({
            "model": target.model,
            "input": [{ "role": "user", "content": question }],
            "reasoning": { "effort": effort },
            "max_output_tokens": cap,
            "stream": false,
        })
    } else {
        serde_json::json!({
            "model": target.model,
            "messages": [{ "role": "user", "content": question }],
            "reasoning_effort": effort,
            "max_tokens": cap,
            "stream": false,
        })
    };
    let json = send(client, target, body).await?;
    // 截断会伪造结论：贴顶样本的方向和幅度都不可信，必须显式标出来。
    let completion = json
        .pointer("/usage/completion_tokens")
        .or_else(|| json.pointer("/usage/output_tokens"))
        .and_then(serde_json::Value::as_u64);
    let censored = completion.is_some_and(|tokens| tokens >= u64::from(cap));
    let reasoning_tokens = json
        .pointer("/usage/completion_tokens_details/reasoning_tokens")
        .or_else(|| json.pointer("/usage/output_tokens_details/reasoning_tokens"))
        .or_else(|| json.pointer("/usage/reasoning_tokens"))
        .and_then(serde_json::Value::as_u64);
    if let Some(tokens) = reasoning_tokens {
        return Ok(ReasoningSample { value: tokens, metric: "reasoning_tokens", censored });
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
        return Ok(ReasoningSample { value: reasoning_text as u64, metric: "推理文本长度", censored });
    }
    match completion {
        Some(tokens) => Ok(ReasoningSample { value: tokens, metric: "输出 token 数", censored }),
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

/// 触发一次能力探测（设置页"重新探测"按钮）。
///
/// **不等结果**：命令只负责把探测丢进后台并立刻返回当前视图（`probing = true`），
/// 设置页照常渲染、轮询到结论再更新。用户点一次不该罚站 10 秒。
/// 只有"读不到密钥"这类配置问题当场报错——那是发送前就该知道的。
///
/// `force = true` 是「不可判」终态的唯一出口：先作废这一条缓存再重探。终态之所以
/// 要能手动打破，是因为端点/模型会升级，而自动重探会把"测不出来"变成每次启动都
/// 烧一轮 token 的常态。
#[tauri::command]
pub async fn probe_capabilities(
    app: tauri::AppHandle,
    provider: String,
    model: Option<String>,
    force: Option<bool>,
) -> Result<serde_json::Value, String> {
    let state = app.state::<crate::app_state::AppState>();
    let cfg = state.config();
    let row = cfg
        .providers
        .iter()
        .find(|p| p.id == provider)
        .ok_or_else(|| format!("服务商 {provider} 不在配置里"))?
        .clone();
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
    let data_root = state.data_root();
    // 手动重探先作废这一条：否则后台任务立刻命中缓存、什么都没重测，
    // 用户会以为"点了没用"。
    if force.unwrap_or(false) {
        let mut cache = ProbeCache::load(&data_root);
        cache.remove(&key);
        cache.save(&data_root);
    }
    let probing = state.probing.clone();
    tracing::info!(provider = %row.id, model = %model_row.id, "开始能力实测");
    let row_for_task = model_row.clone();
    tauri::async_runtime::spawn(async move {
        let _ = probe_and_cache(&data_root, &target, &row_for_task, &probing).await;
    });
    // 返回当前已知结论 + 检测中标记；结论由设置页轮询取回。
    let cache = ProbeCache::load(&state.data_root());
    let cached = cache.get(&key);
    Ok(model_view(&model_row, &row.base_url, cached, true))
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

/// 总超时后的结论：每一项都记「未验证」并写明原因。
/// **绝不记成「不生效」**——超时是我们没测成，不是端点不支持；两者后果完全不同。
fn timeout_outcomes(row: &ModelProfile, base_url: &str) -> BTreeMap<String, ProbeOutcome> {
    let evidence = format!(
        "能力实测超时（{PROBE_TOTAL_TIMEOUT_SECS} 秒未完成）：本次未验证，不影响模型可用性"
    );
    let outcome = || ProbeOutcome {
        verified: None,
        evidence: evidence.clone(),
        checked_at_ms: now_ms(),
        ..Default::default()
    };
    let mut out = BTreeMap::new();
    out.insert("vision".to_string(), outcome());
    out.insert("tools".to_string(), outcome());
    if !row.declared_reasoning_levels(base_url).is_empty() {
        out.insert("reasoning".to_string(), outcome());
    }
    out
}

/// 缓存条目能否直接复用：结论（无论生效还是不生效）与「不可判」终态都算测过；
/// 只有**瞬时失败**（网络 / 429 / 超时 / 预算用尽）才留待下一轮重试。
///
/// 这条区分是「不可判终态」与「粘性缓存」的分水岭：终态是明确结论、一次写定；
/// 把瞬时失败也永久缓存，等于让一次网络抖动冒充结论。
fn reusable(entry: &BTreeMap<String, ProbeOutcome>) -> bool {
    !entry.is_empty() && entry.values().all(|outcome| outcome.verified.is_some() || outcome.unjudgeable)
}

/// 跑一次探针并落缓存，带总超时与去重。
///
/// 三条纪律：
/// 1. **只在后台任务里跑**——调用方要么 `spawn`，要么是用户手动点的检测；
///    任何回合启动路径都不 await 它。
/// 2. 同一缓存键已在跑就立刻返回（不排队、不重复烧 token）；
///    已有缓存记录就直接返回记录。
/// 3. 失败（网络断 / 429 / 超时）只影响 `verified`，**不影响模型可用性**——
///    这里从不修改声明值，也从不把模型标成不可用。
pub async fn probe_and_cache(
    data_root: &Path,
    target: &ProbeTarget,
    row: &ModelProfile,
    inflight: &Inflight,
) -> BTreeMap<String, ProbeOutcome> {
    let key = target.cache_key();
    if let Some(hit) = ProbeCache::load(data_root).get(&key).filter(|entry| reusable(entry)) {
        return hit.clone();
    }
    if !inflight.begin(&key) {
        // 另一轮正在跑：它测完会把结论写进缓存，这里不排队也不等。
        return ProbeCache::load(data_root).get(&key).cloned().unwrap_or_default();
    }
    let _guard = InflightGuard { inflight, key: &key };
    tracing::info!(model = %target.model, "首次使用该模型，后台跑一次能力实测");
    let outcomes = match tokio::time::timeout(
        std::time::Duration::from_secs(PROBE_TOTAL_TIMEOUT_SECS),
        probe_all(target, row),
    )
    .await
    {
        Ok(outcomes) => outcomes,
        Err(_) => {
            tracing::warn!(
                model = %target.model,
                seconds = PROBE_TOTAL_TIMEOUT_SECS,
                "能力实测超时：本次记未验证，不影响模型可用性"
            );
            timeout_outcomes(row, &target.base_url)
        }
    };
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
    // 重新读一次再写：别的模型可能同时在探，别把它的记录覆盖掉。
    let mut cache = ProbeCache::load(data_root);
    cache.put(key.clone(), outcomes.clone());
    cache.save(data_root);
    outcomes
}

/// 首次使用时确保该模型有实测记录。只用于后台预热——失败只记日志，不影响任务。
pub async fn ensure_probed(
    data_root: PathBuf,
    target: ProbeTarget,
    row: ModelProfile,
    inflight: &Inflight,
) {
    let _ = probe_and_cache(&data_root, &target, &row, inflight).await;
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
        let view = model_view(&r, BASE, None, false);
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
                elapsed_ms: Some(42),
                ..Default::default()
            },
        );
        let view = model_view(&r, BASE, Some(&outcomes), false);
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
        let view = model_view(&r, base, None, false);
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
        let view = model_view(&r, base, None, false);
        assert_eq!(view["context_window"]["value"], 128_000);
        assert_eq!(view["context_window"]["source"], "manual");
    }

    #[test]
    fn family_hit_is_inferred_and_stays_conservative() {
        let base = "https://api.longcat.chat/openai/v1";
        let r = ModelProfile { id: "LongCat-2.7".into(), ..Default::default() };
        let view = model_view(&r, base, None, false);
        assert_eq!(view["context_window"]["source"], "inferred", "家族命中必须标「推测」");
        // 家族只声明 text，等于推测"这一代没有视觉"：值 false、来源仍是推测而不是未知
        assert_eq!(view["vision"]["value"], false);
        assert_eq!(view["vision"]["source"], "inferred");
    }

    #[test]
    fn undeclared_value_is_absent_and_builtin_source_has_value() {
        // 没人声明（未知端点、本行空）时，布尔项不能写成 false——那会被读成"明确没有"。
        let r = ModelProfile { id: "some-unknown-model".into(), ..Default::default() };
        let view = model_view(&r, "https://example.invalid/v1", None, false);
        assert!(view["tools"].get("value").is_none(), "未声明写成了 false: {}", view["tools"]);
        assert_eq!(view["tools"]["source"], "unknown");
        assert!(view["max_output"].get("value").is_none(), "没有来源的 0 不是声明值");

        // 来源是内置表，值就必须来自同一张表（不能出现「内置表 + 未声明」）。
        let base = "https://api.deepseek.com";
        let flash = ModelProfile { id: "deepseek-flash".into(), ..Default::default() };
        let view = model_view(&flash, base, None, false);
        let builtin = crate::model_catalog::lookup(base, "deepseek-flash").expect("内置表命中");
        assert_eq!(view["max_output"]["source"], "builtin");
        assert_eq!(
            view["max_output"]["value"].as_u64(),
            builtin.capabilities().max_output,
            "来源是内置表但值不是表里的值"
        );
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
        let view = model_view(&r, BASE, Some(&outcomes), false);
        assert_eq!(view["context_window"]["source"], "manual");
        assert_eq!(view["context_window"]["state"], "unverified", "手改之后探针不再覆盖");
    }

    #[test]
    fn probing_is_its_own_state_not_unverified() {
        // 「检测中」与「未验证」必须分开：前者还没测完，用户应该等；后者测过没测成。
        let r = ModelProfile { id: "m-probing".into(), ..Default::default() };
        let view = model_view(&r, BASE, None, true);
        assert_eq!(view["vision"]["state"], "probing");
        assert_eq!(view["tools"]["state"], "probing");
        // 没有探针的项不能被写成「检测中」——它永远测不出来，装作在测就是撒谎。
        assert_eq!(view["context_window"]["state"], "unverified");
        assert_eq!(view["builtin_web_search"]["state"], "unverified");
        // 已经测出结论的项不受"正在测"影响。
        let mut outcomes = std::collections::BTreeMap::new();
        outcomes.insert(
            "vision".to_string(),
            ProbeOutcome { verified: Some(true), ..Default::default() },
        );
        let view = model_view(&r, BASE, Some(&outcomes), true);
        assert_eq!(view["vision"]["state"], "verified");
        assert_eq!(view["tools"]["state"], "probing");
        // 手动记录不采信探针，也就不该显示成"检测中"。
        let mut manual = r.clone();
        manual.capabilities.insert(
            "vision".into(),
            CapabilityRecord { source: CapabilitySource::Manual, ..Default::default() },
        );
        let view = model_view(&manual, BASE, None, true);
        assert_eq!(view["vision"]["state"], "unverified");
    }

    #[test]
    fn timeout_is_unverified_with_reason_never_unsupported() {
        let r = ModelProfile {
            id: "m-timeout".into(),
            reasoning_efforts: [("low".to_string(), "low".to_string())].into_iter().collect(),
            ..Default::default()
        };
        let outcomes = timeout_outcomes(&r, BASE);
        for item in ["vision", "tools", "reasoning"] {
            let outcome = &outcomes[item];
            assert_eq!(outcome.verified, None, "{item} 超时不能记成「不生效」");
            assert!(outcome.evidence.contains("超时"), "{}", outcome.evidence);
            assert_eq!(outcome.elapsed_ms, None, "没测完就没有耗时，不能写 0");
        }
        // 没声明推理档就不探推理，也就不该凭空多出一条结论。
        let plain = ModelProfile { id: "m-plain".into(), ..Default::default() };
        assert!(!timeout_outcomes(&plain, BASE).contains_key("reasoning"));
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

    /// 等长三样本的判定快捷方式，避免每个用例手写 `judge_reasoning_samples(..).verdict`。
    fn verdict_of(low: [u64; 3], high: [u64; 3]) -> ReasoningVerdict {
        judge_reasoning_samples(&low, &high, 0).verdict
    }

    #[test]
    fn reasoning_verdict_needs_evidence_for_both_directions() {
        // 生效：两档各自稳定，高档中位数明显更高（效应/噪声远超门槛 3）。
        assert_eq!(verdict_of([100, 101, 100], [300, 305, 300]), ReasoningVerdict::Effective);
        // 方向为负且显著 → 字段未被识别（Sensenova 实测 254 → 7 的量级）。
        let reversed = judge_reasoning_samples(&[250, 254, 252], &[7, 8, 7], 0);
        assert_eq!(reversed.verdict, ReasoningVerdict::NotRecognized);
        assert!(reversed.ratio < 0.0, "反向时变化量为负，文案据此写成「方向为负」");
        // 低档为 0、高档有推理也算生效。
        assert_eq!(verdict_of([0, 0, 0], [50, 52, 51]), ReasoningVerdict::Effective);
    }

    #[test]
    fn ratio_below_threshold_is_insufficient_not_ignored() {
        // +4% 差异：既不足以说"生效"，也不足以说"没用"——只能落「不可判」。
        // 这正是"声称可用需要更多证据"的不对称门槛：门槛 3 之下，两个方向都不许下结论。
        let weak = judge_reasoning_samples(&[100, 100, 100], &[104, 104, 104], 0);
        assert_eq!(weak.verdict, ReasoningVerdict::Insufficient);
        assert_eq!(weak.reason, Some(InsufficientReason::BelowThreshold));
        assert!(weak.effect_over_noise < EFFECT_NOISE_MIN, "比值 {:.2}", weak.effect_over_noise);
        // 门槛之上一档：同样的方向，比值够就允许下结论。
        let strong = judge_reasoning_samples(&[100, 100, 100], &[200, 200, 200], 0);
        assert_eq!(strong.verdict, ReasoningVerdict::Effective);
        assert!(strong.effect_over_noise >= EFFECT_NOISE_MIN);
    }

    #[test]
    fn censored_samples_never_produce_a_conclusion() {
        // 只要有一个样本触顶，数值就被截断过：**不许用截断数据算出生效**。
        let capped = judge_reasoning_samples(&[100, 101, 100], &[300, 305, 300], 1);
        assert_eq!(capped.verdict, ReasoningVerdict::Insufficient);
        assert_eq!(capped.reason, Some(InsufficientReason::Censored));
        // 这组数据的比值本来足以判"生效"；触顶守卫必须把它拦下来——这正是
        // "截断会伪造结论"的可执行证据，而不是只在注释里写一句规则。
        assert!(
            capped.effect_over_noise > EFFECT_NOISE_MIN,
            "用例前提：触顶前比值确实够大（{:.1}）",
            capped.effect_over_noise
        );
        assert!(InsufficientReason::Censored.is_terminal(), "触顶是终态，要写进缓存");
        // 低档触顶同样作废：低档贴顶说明题目对它也过难，效应量会失真。
        assert_eq!(
            judge_reasoning_samples(&[500, 502, 512], &[900, 900, 900], 2).reason,
            Some(InsufficientReason::Censored)
        );
    }

    #[test]
    fn reasoning_verdict_says_insufficient_instead_of_guessing() {
        // 样本不足：2 个样本取中位数不稳，直接第四态，不许硬判。
        let short = judge_reasoning_samples(&[100, 100], &[200, 200, 200], 0);
        assert_eq!(short.verdict, ReasoningVerdict::Insufficient);
        assert_eq!(short.reason, Some(InsufficientReason::NotEnoughSamples));
        assert_eq!((short.low_median, short.high_median), (100.0, 200.0));
        assert!(!InsufficientReason::NotEnoughSamples.is_terminal(), "样本不足是瞬时失手，下一轮还要试");
        // 档内相对离散度超 50%：这一档自己就说不清话。
        let dispersed = judge_reasoning_samples(&[10, 100, 300], &[300, 300, 300], 0);
        assert_eq!(dispersed.verdict, ReasoningVerdict::Insufficient);
        assert_eq!(dispersed.reason, Some(InsufficientReason::TooDispersed));
        // 差异只有 12，但两档极差都是 20 → 噪声带被撑到 40：分不清「没效果」与「太抖」。
        let band = judge_reasoning_samples(&[100, 110, 120], &[112, 122, 132], 0);
        assert_eq!(band.verdict, ReasoningVerdict::Insufficient);
        assert_eq!(band.reason, Some(InsufficientReason::BandDominated));
        assert!(band.band >= NOISE_DISPERSION_K * band.dispersion, "噪声带必须由极差撑起");
    }

    #[test]
    fn unjudgeable_is_its_own_state_with_evidence() {
        // 「不可判」不是「未验证」：前者数据采完了、只是判不出来；后者是没测成。
        let r = ModelProfile { id: "m-unjudgeable".into(), ..Default::default() };
        let mut outcomes = BTreeMap::new();
        outcomes.insert(
            "reasoning".to_string(),
            ProbeOutcome {
                verified: None,
                unjudgeable: true,
                evidence: "效应/噪声 0.8 低于门槛 3——数据不足，无法判定".into(),
                checked_at_ms: 9,
                ..Default::default()
            },
        );
        let view = model_view(&r, BASE, Some(&outcomes), false);
        assert_eq!(view["reasoning"]["state"], "unjudgeable");
        assert!(view["reasoning"]["evidence"].as_str().unwrap_or_default().contains("门槛 3"));
        // 瞬时失败（unjudgeable=false）仍显示「未验证」——两者不能混成一个词。
        let transient = BTreeMap::from([(
            "reasoning".to_string(),
            ProbeOutcome { verified: None, evidence: "429 限流".into(), ..Default::default() },
        )]);
        assert_eq!(model_view(&r, BASE, Some(&transient), false)["reasoning"]["state"], "unverified");
    }

    #[test]
    fn probe_version_change_invalidates_old_conclusion() {
        let dir = crate::test_support::TestTempDir::new("capability-version");
        let old_key = cache_key_for(BASE, "openai-chat", "m", "k", "old-probe");
        let mut cache = ProbeCache::default();
        cache.put(
            old_key,
            BTreeMap::from([(
                "reasoning".to_string(),
                ProbeOutcome { verified: Some(true), ..Default::default() },
            )]),
        );
        cache.save(&dir);
        let loaded = ProbeCache::load(&dir);
        // 当前版本的键查不到旧结论：改题面/判据/采样后必须重测，不能拿旧答案当真。
        assert!(loaded.get(&cache_key(BASE, "openai-chat", "m", "k")).is_none());
        // 版本确实参与键：只改版本就会落到另一条记录。
        assert_ne!(
            cache_key_for(BASE, "openai-chat", "m", "k", "v1"),
            cache_key_for(BASE, "openai-chat", "m", "k", "v2")
        );
        assert!(cache_key(BASE, "openai-chat", "m", "k").ends_with(PROBE_VERSION));
        let _ = std::fs::remove_dir_all(&dir);
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
