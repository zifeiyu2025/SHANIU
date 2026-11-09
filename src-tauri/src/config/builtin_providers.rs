//! 内置模型提供商（出厂数据表，不是业务模块）。
//!
//! 这里定义随应用出厂的服务商及其**固定展示次序**。次序用显式 `priority`
//! 声明，列表按它稳定排序；不要在界面里靠数组下标或 id 特判置顶——那样每加
//! 一家内置商都要改前端，改漏的表现是「设置页顺序和席位顺序不一致」。
//!
//! 数值事实来源：
//! - `resources/model-catalog.json`（2026-11-09 端点实测：上下文 / 输出上限 / 推理档）
//! - DSH `packages/llm/llm-deepseek/src/{models,defaults}.ts`（模型 id、默认模型、显示名）

use std::collections::BTreeMap;

use super::{ModelProfile, ProviderProfile};

/// 一家内置服务商。字段全部是静态数据，新增一家只改本表。
pub struct BuiltinProvider {
    /// 稳定路由名（也是密钥引用名的派生依据）。
    pub id: &'static str,
    /// 设置页与席位显示的厂商名。
    pub label: &'static str,
    /// 宿主协议族：`openai-chat` | `openai-responses`。
    pub protocol: &'static str,
    /// 默认端点。
    pub base_url: &'static str,
    /// 出厂默认模型 id。
    pub default_model: &'static str,
    /// 越小越靠前；内置服务商之间的固定次序。
    pub priority: u8,
    pub models: &'static [BuiltinModel],
}

/// 内置服务商的一行模型。
pub struct BuiltinModel {
    pub id: &'static str,
    pub name: &'static str,
    pub context_window: u32,
    pub max_tokens: u32,
    pub input: &'static [&'static str],
    /// 档位名 → 发往 `reasoning_effort` 的 wire 值（与 OpenAI 兼容端点同拼写）。
    pub reasoning_efforts: &'static [(&'static str, &'static str)],
}

const DEEPSEEK_REASONING: &[(&str, &str)] = &[("low", "low"), ("high", "high"), ("max", "max")];

/// 出厂内置服务商表。DeepSeek 排第一（`priority = 0`）。
pub const BUILTIN_PROVIDERS: &[BuiltinProvider] = &[BuiltinProvider {
    id: "deepseek",
    label: "DeepSeek",
    protocol: "openai-chat",
    base_url: "https://api.deepseek.com",
    // DSH 出厂组合的默认档是 deepseek-flash（apps/web 的 agentDefaultModel）。
    default_model: "deepseek-flash",
    priority: 0,
    models: &[
        BuiltinModel {
            id: "deepseek-flash",
            name: "DeepSeek-V4.1-Flash",
            context_window: 1_048_576,
            max_tokens: 393_216,
            input: &["text", "image"],
            reasoning_efforts: DEEPSEEK_REASONING,
        },
        BuiltinModel {
            id: "deepseek-v4-pro",
            name: "DeepSeek-V4-Pro",
            context_window: 1_048_576,
            max_tokens: 393_216,
            input: &["text"],
            reasoning_efforts: DEEPSEEK_REASONING,
        },
    ],
}];

/// 按路由名查内置服务商。
pub fn find(id: &str) -> Option<&'static BuiltinProvider> {
    BUILTIN_PROVIDERS.iter().find(|provider| provider.id == id)
}

/// 内置次序；`None` = 非内置（排在内置之后，保持文档顺序）。
pub fn priority(id: &str) -> Option<u8> {
    find(id).map(|provider| provider.priority)
}

/// 出厂默认的生效服务商 id。
pub fn default_active_id() -> &'static str {
    BUILTIN_PROVIDERS
        .iter()
        .min_by_key(|provider| provider.priority)
        .map(|provider| provider.id)
        .unwrap_or("")
}

/// 把内置表展开成 `[[providers]]` 结构（附件：设置页与运行期共用同一份文档）。
pub fn default_profiles() -> Vec<ProviderProfile> {
    BUILTIN_PROVIDERS.iter().map(profile_of).collect()
}

fn profile_of(provider: &BuiltinProvider) -> ProviderProfile {
    ProviderProfile {
        id: provider.id.to_string(),
        label: provider.label.to_string(),
        provider: provider.protocol.to_string(),
        base_url: provider.base_url.to_string(),
        model: provider.default_model.to_string(),
        reasoning_effort: String::new(),
        headers: BTreeMap::new(),
        secret_ref: format!(
            "{}_API_KEY",
            provider.id.to_uppercase().replace(|c: char| !c.is_ascii_alphanumeric(), "_")
        ),
        models: provider
            .models
            .iter()
            .map(|model| ModelProfile {
                id: model.id.to_string(),
                name: model.name.to_string(),
                context_window: model.context_window,
                max_tokens: model.max_tokens,
                input: model.input.iter().map(|item| item.to_string()).collect(),
                reasoning_efforts: model
                    .reasoning_efforts
                    .iter()
                    .map(|(level, wire)| (level.to_string(), wire.to_string()))
                    .collect(),
                capabilities: BTreeMap::new(),
            })
            .collect(),
    }
}

/// 按内置优先级稳定排序：内置服务商在前（按 `priority`），其余保持文档顺序。
///
/// 用稳定排序而不是「先找 deepseek 再拼」：后者只对当前这一家成立，第二家内置商
/// 出现时又要改一遍；`priority` 表才是可扩展的次序来源。
pub fn ordered_profiles(profiles: &[ProviderProfile]) -> Vec<&ProviderProfile> {
    let mut ordered: Vec<&ProviderProfile> = profiles.iter().collect();
    ordered.sort_by_key(|profile| priority(&profile.id).unwrap_or(u8::MAX));
    ordered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deepseek_is_first_and_complete() {
        let profiles = default_profiles();
        assert_eq!(profiles.len(), 1);
        let deepseek = &profiles[0];
        assert_eq!(deepseek.id, "deepseek");
        assert_eq!(deepseek.label, "DeepSeek");
        assert_eq!(deepseek.provider, "openai-chat");
        assert_eq!(deepseek.base_url, "https://api.deepseek.com");
        assert_eq!(deepseek.model, "deepseek-flash");
        assert_eq!(deepseek.secret_name(), "DEEPSEEK_API_KEY");
        let ids: Vec<&str> = deepseek.models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, vec!["deepseek-flash", "deepseek-v4-pro"]);
        // 实测档位来自 resources/model-catalog.json，别把未声明的档位写进来
        assert_eq!(
            deepseek.models[0].reasoning_efforts.keys().cloned().collect::<Vec<_>>(),
            vec!["high", "low", "max"]
        );
        assert_eq!(default_active_id(), "deepseek");
    }

    #[test]
    fn ordering_puts_builtin_first_regardless_of_document_order() {
        let custom = ProviderProfile {
            id: "longcat".into(),
            label: "LongCat".into(),
            provider: "openai-chat".into(),
            base_url: "https://api.longcat.chat/openai/v1".into(),
            model: "LongCat-2.5".into(),
            ..ProviderProfile::default()
        };
        let mut profiles = default_profiles();
        profiles.push(custom);
        // 模拟用户把自定义商排到了文档前面
        profiles.rotate_right(1);
        let ordered = ordered_profiles(&profiles);
        assert_eq!(ordered[0].id, "deepseek");
        assert_eq!(ordered[1].id, "longcat");
    }

    #[test]
    fn fresh_install_config_validates_and_roundtrips() {
        use crate::config::AppConfig;

        let config = AppConfig::fresh_install();
        assert_eq!(config.active_provider, "deepseek");
        AppConfig::validate_providers(&config.providers, &config.active_provider).unwrap();
        let encoded = toml::to_string_pretty(&config).unwrap();
        let decoded: AppConfig = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded, config);
        assert_eq!(decoded.effective_model().model, "deepseek-flash");
    }
}
