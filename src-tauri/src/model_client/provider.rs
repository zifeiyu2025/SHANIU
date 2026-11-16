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
    /// 服务商自定义请求头（SH-097）。部署头先放，`bearer_auth` 后到——authorization
    /// 冲突时认证头获胜（上游同口径）。
    pub headers: std::collections::BTreeMap<String, String>,
}

impl ProviderConfig {
    pub fn from_model_config(cfg: &ModelConfig) -> Self {
        Self {
            provider: cfg.provider.clone(),
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            model: cfg.model.clone(),
            reasoning_effort: None,
            headers: Default::default(),
        }
    }

    /// 从生效端点构造：推理档已在配置层解析成 wire 值。
    pub fn from_resolved(cfg: &crate::config::ResolvedModel) -> Self {
        Self {
            provider: cfg.provider.clone(),
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            model: cfg.model.clone(),
            reasoning_effort: cfg.reasoning_effort.clone().filter(|e| !e.is_empty()),
            headers: cfg.headers.clone(),
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

/// 单次请求的协议扩展选项（D1）。
///
/// 刻意不挂在 [`ProviderConfig`] 上：端点身份（协议 / 地址 / 模型 / 头）与请求策略
/// 生命周期不同——`prompt_cache_key` 按**会话**派生，而同一端点被多个会话共用。
/// 分开后端点结构体不用改形状，装配点少一圈无谓的重构。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestOptions {
    /// 会话级提示缓存键（Responses 的 `prompt_cache_key`）。
    /// `None` = 不发该键（辅助请求：标题生成）。跑评测/单测时缺省也不发。
    pub cache_key: Option<String>,
    /// 是否发送 Responses 扩展字段（`store` / `tool_choice` / `parallel_tool_calls` /
    /// `include`）。默认 true；不认这些键的老网关关掉后回到 2026-11 之前的 5 键请求体。
    pub responses_extras: bool,
    /// 响应端 `parallel_tool_calls`。只影响模型一次返回几个调用，
    /// **不改变**宿主的工具执行顺序（那由 agent_loop 决定）。
    pub parallel_tool_calls: bool,
    /// 是否捕获并回放 Responses 的推理项（D2）。默认 true：请求声明
    /// `include: ["reasoning.encrypted_content"]`，历史里的 `Reasoning` 项原样回放。
    /// 关掉后两步都不做，请求体回到 D1 形状（不付密文体积，也不带推理状态续写）。
    pub reasoning_replay: bool,
}

impl Default for RequestOptions {
    fn default() -> Self {
        Self {
            cache_key: None,
            responses_extras: true,
            parallel_tool_calls: true,
            reasoning_replay: true,
        }
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
            max_retries: 0,
            retry_mode: crate::model_client::RetryMode::Bounded,
            responses_extras: true,
            parallel_tool_calls: true,
            reasoning_replay: true,
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
            headers: Default::default(),
            vision: false,
            context_window: 0,
            max_output: 0,
            responses_extras: true,
            parallel_tool_calls: true,
            reasoning_replay: true,
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
