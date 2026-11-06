//! 端点配置与鉴权（docs §8 model_client/provider.rs）。
//!
//! 鉴权密钥不落盘、不进日志：由 agent_loop 经 config::SecretStore 解密后
//! 以 `&str` 传入（model_client 不接触存储层）。

use crate::config::ModelConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConfig {
    /// 协议族：`openai-responses` | `openai-chat`（M1 实现 Responses，Chat 预留扩展点）。
    pub provider: String,
    pub base_url: String,
    pub model: String,
    /// 生效的推理档 wire 值（SH-096）。`effective_model()` 已完成"档位名 → wire 值"
    /// 的查表（off/未声明都是 None），这里只看最终字符串：Some 且非空才进请求体。
    pub reasoning_effort: Option<String>,
}

impl ProviderConfig {
    pub fn from_model_config(cfg: &ModelConfig) -> Self {
        Self {
            provider: cfg.provider.clone(),
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            model: cfg.model.clone(),
            reasoning_effort: None,
        }
    }

    /// 从生效端点构造：推理档已在配置层解析成 wire 值。
    pub fn from_resolved(cfg: &crate::config::ResolvedModel) -> Self {
        Self {
            provider: cfg.provider.clone(),
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            model: cfg.model.clone(),
            reasoning_effort: cfg.reasoning_effort.clone().filter(|e| !e.is_empty()),
        }
    }

    /// Responses API 端点：`{base_url}/responses`。
    pub fn responses_url(&self) -> String {
        format!("{}/responses", self.base_url)
    }

    /// 是否 Responses 协议族（其余视为 ChatCompletions）。
    pub fn is_responses(&self) -> bool {
        self.provider == "openai-responses"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(base: &str) -> ModelConfig {
        ModelConfig {
            provider: "openai-responses".into(),
            base_url: base.into(),
            model: "test-model".into(),
            max_turns: 8,
        }
    }

    #[test]
    fn from_resolved_carries_reasoning_effort() {
        let mut resolved = crate::config::ResolvedModel {
            id: "p".into(),
            provider: "openai-chat".into(),
            base_url: "https://a/v1".into(),
            model: "m1".into(),
            max_turns: 8,
            secret_name: "K".into(),
            reasoning_effort: Some("high".into()),
        };
        let p = ProviderConfig::from_resolved(&resolved);
        assert_eq!(p.reasoning_effort.as_deref(), Some("high"));
        resolved.reasoning_effort = None;
        assert_eq!(ProviderConfig::from_resolved(&resolved).reasoning_effort, None);
    }

    #[test]
    fn responses_url_builds_from_base() {
        let p = ProviderConfig::from_model_config(&cfg("https://api.example.com/v1"));
        assert_eq!(p.responses_url(), "https://api.example.com/v1/responses");
        assert!(p.is_responses());
    }

    #[test]
    fn trailing_slash_is_trimmed() {
        let p = ProviderConfig::from_model_config(&cfg("https://api.example.com/v1/"));
        assert_eq!(p.responses_url(), "https://api.example.com/v1/responses");
    }

    #[test]
    fn chat_family_is_not_responses() {
        let mut c = cfg("https://x/v1");
        c.provider = "openai-chat".into();
        let p = ProviderConfig::from_model_config(&c);
        assert!(!p.is_responses());
    }
}
