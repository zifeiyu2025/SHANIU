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
}

impl ProviderConfig {
    pub fn from_model_config(cfg: &ModelConfig) -> Self {
        Self {
            provider: cfg.provider.clone(),
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            model: cfg.model.clone(),
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
