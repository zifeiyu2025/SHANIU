//! 模型请求重试的**纯逻辑**：能不能重试、要等多久。
//!
//! 刻意不碰 sink、不碰取消令牌、不睡眠——那三样留在 `agent_loop`（它才知道回合、
//! 事件与取消语义）。因此本文件不依赖 tokio，单测是确定性的。
//!
//! 参数与公式照搬 dsh（`llm/llm/src/retry-policy.ts`、`llm/llm-retry/src/index.ts`）：
//! 指数退避 + 对称抖动 + 封顶，`Retry-After` 优先。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::RetryMode;

use super::ModelErrorKind;

/// 重试策略。字段公开：配置装配与测试都直接按值构造。
#[derive(Debug, Clone, PartialEq)]
pub struct RetryPolicy {
    /// 单个采样步最多重试几次（不含首次请求）。`mode = Always` 时无意义。
    pub max_retries: u32,
    /// 次数语义（D5）。默认 `Bounded`。
    pub mode: RetryMode,
    /// 第一次重试的基准等待。
    pub initial_delay: Duration,
    /// 单次等待的硬上限，也是 `Retry-After` 的可接受上限。
    pub max_delay: Duration,
    /// 对称抖动比例：`1 ± jitter_ratio`。
    pub jitter_ratio: f64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        // 数值逐项对齐 dsh retry-policy.ts:14-17。
        Self {
            max_retries: 5,
            mode: RetryMode::Bounded,
            initial_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(10),
            jitter_ratio: 0.1,
        }
    }
}

impl RetryPolicy {
    /// 只改次数，其余沿用默认值（`from_config` 的唯一入口）。
    pub fn new(max_retries: u32) -> Self {
        Self { max_retries, ..Self::default() }
    }

    /// 测试与回放用：0 次重试，不产生任何 `model_retry` 事件。
    pub fn disabled() -> Self {
        Self::new(0)
    }

    /// 是否允许重试（`Bounded(0)` 关闭；`Always` 永远允许）。
    pub fn enabled(&self) -> bool {
        match self.mode {
            RetryMode::Bounded => self.max_retries > 0,
            RetryMode::Always => true,
        }
    }

    /// 这一类失败是否值得再试：`Bounded` 走白名单，`Always` 一律接受（上游语义）。
    pub fn accepts(&self, kind: ModelErrorKind) -> bool {
        match self.mode {
            RetryMode::Bounded => Self::is_retryable(kind),
            RetryMode::Always => true,
        }
    }

    /// 白名单判定。白名单之外一次失败即终局——不猜、不重试。
    pub fn is_retryable(kind: ModelErrorKind) -> bool {
        matches!(
            kind,
            ModelErrorKind::RateLimited
                | ModelErrorKind::Timeout
                | ModelErrorKind::Network
                | ModelErrorKind::Server
                | ModelErrorKind::EmptyResponse
        )
    }

    /// 第 `retry` 次重试（从 1 起）该等多久；`None` = 放弃重试。
    ///
    /// - `Bounded`：超过次数即放弃；端点给了 `Retry-After` 就优先采用，超过 `max_delay`
    ///   直接放弃，不自行缩短；
    /// - `Always`：永不因次数放弃；`Retry-After` 超过 `max_delay` 时改用本地指数退避
    ///   （上游 `llm-retry/index.ts:228-233` 同一处理），而不是等一小时或直接放弃；
    /// - 本地退避：`initial * 2^(retry-1)`，封顶 `max_delay`，再乘对称抖动并二次封顶。
    ///
    /// `random01` 由调用方注入（`[0, 1]`），生产用 [`random01`]，测试用固定值。
    pub fn delay(&self, retry: u32, provider_retry_after_ms: Option<u64>, random01: f64) -> Option<Duration> {
        if retry == 0 {
            return None;
        }
        if let RetryMode::Bounded = self.mode {
            if retry > self.max_retries {
                return None;
            }
        }
        // `2^(retry-1)` 用移位算，先夹住指数防止手写超大次数时溢出。
        let exponent = retry.saturating_sub(1).min(20);
        let exponential = self.initial_delay.saturating_mul(1u32 << exponent).min(self.max_delay);
        let jitter = 1.0 - self.jitter_ratio + 2.0 * self.jitter_ratio * random01.clamp(0.0, 1.0);
        let local = exponential.mul_f64(jitter).min(self.max_delay);
        if let Some(required) = provider_retry_after_ms {
            let required = Duration::from_millis(required);
            if required <= self.max_delay {
                return Some(required);
            }
            return match self.mode {
                RetryMode::Bounded => None,
                RetryMode::Always => Some(local),
            };
        }
        Some(local)
    }
}

/// 生产用随机源：`[0, 1)`。系统熵源不可用时退回时钟低位，不 panic——
/// 抖动只影响等待的分布，不影响正确性。
pub fn random01() -> f64 {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_ok() {
        // 取高 53 位映射到 [0, 1)，与 f64 尾数精度一致。
        return (u64::from_le_bytes(bytes) >> 11) as f64 / (1u64 << 53) as f64;
    }
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    f64::from(nanos) / 1e9
}

#[cfg(test)]
mod tests {
    use super::RetryPolicy;
    use crate::model_client::ModelErrorKind;
    use std::time::Duration;

    fn no_jitter(max_retries: u32) -> RetryPolicy {
        RetryPolicy { jitter_ratio: 0.0, ..RetryPolicy::new(max_retries) }
    }

    /// 反向对照：把任意一个可重试类从白名单删掉，对应断言必红。
    #[test]
    fn whitelist_is_exactly_the_transient_failures() {
        for kind in [
            ModelErrorKind::RateLimited,
            ModelErrorKind::Timeout,
            ModelErrorKind::Network,
            ModelErrorKind::Server,
            ModelErrorKind::EmptyResponse,
        ] {
            assert!(RetryPolicy::is_retryable(kind), "{kind:?} 属瞬态失败，必须可重试");
        }
        for kind in [
            ModelErrorKind::Auth,
            ModelErrorKind::Quota,
            ModelErrorKind::BadRequest,
            ModelErrorKind::EndpointUnsupported,
            ModelErrorKind::HookDenied,
            ModelErrorKind::AccessProgramUnsupported,
            ModelErrorKind::Unknown,
        ] {
            assert!(!RetryPolicy::is_retryable(kind), "{kind:?} 重试多少次都不会变，必须一次终局");
        }
    }

    #[test]
    fn delay_doubles_and_caps() {
        let policy = no_jitter(8);
        let expected = [500u64, 1000, 2000, 4000, 8000, 10_000, 10_000];
        for (index, millis) in expected.iter().enumerate() {
            let retry = index as u32 + 1;
            assert_eq!(
                policy.delay(retry, None, 0.5),
                Some(Duration::from_millis(*millis)),
                "第 {retry} 次重试的等待不符合指数退避"
            );
        }
    }

    #[test]
    fn jitter_is_symmetric_and_never_exceeds_cap() {
        let policy = RetryPolicy { jitter_ratio: 0.1, ..RetryPolicy::new(8) };
        // 第一次基准 500ms：下界 0.9× = 450ms，上界 1.1× = 550ms。
        assert_eq!(policy.delay(1, None, 0.0), Some(Duration::from_millis(450)));
        assert_eq!(policy.delay(1, None, 1.0), Some(Duration::from_millis(550)));
        // 封顶后即使乘上 1.1 也不得越过 max_delay。
        let capped = policy.delay(8, None, 1.0).expect("应给出等待");
        assert!(capped <= policy.max_delay, "抖动把等待推过了上限：{capped:?}");
    }

    #[test]
    fn provider_retry_after_wins_and_oversize_gives_up() {
        let policy = no_jitter(5);
        assert_eq!(policy.delay(1, Some(3_000), 0.5), Some(Duration::from_millis(3_000)));
        assert_eq!(policy.delay(1, Some(30_000), 0.5), None, "超过上限必须放弃，不得自行缩短");
        assert_eq!(policy.delay(1, Some(10_000), 0.5), Some(Duration::from_secs(10)), "等于上限仍可采用");
    }

    #[test]
    fn disabled_policy_never_retries() {
        let policy = RetryPolicy::disabled();
        assert_eq!(policy.max_retries, 0);
        assert_eq!(policy.delay(1, None, 0.5), None);
        assert_eq!(policy.delay(1, Some(1_000), 0.5), None);
    }

    #[test]
    fn retry_beyond_configured_budget_has_no_delay() {
        let policy = no_jitter(2);
        assert!(policy.delay(2, None, 0.5).is_some());
        assert_eq!(policy.delay(3, None, 0.5), None);
        assert_eq!(policy.delay(0, None, 0.5), None, "retry 从 1 起");
    }

    #[test]
    fn random01_stays_in_unit_interval() {
        for _ in 0..64 {
            let value = super::random01();
            assert!((0.0..1.0).contains(&value), "随机源越界: {value}");
        }
    }

    /// D5：always 不设次数上限、不筛失败类别；默认仍必须是 bounded。
    ///
    /// 反向对照：把 `always_mode` 改成 `bounded`（或让 `enabled/accepts` 不看 mode），
    /// 本用例立刻红。
    #[test]
    fn always_mode_never_exhausts_and_ignores_failure_class() {
        let always = RetryPolicy { mode: super::RetryMode::Always, ..RetryPolicy::new(0) };
        assert!(always.enabled(), "always 不得被 max_retries = 0 关掉");
        for retry in [1u32, 2, 5, 100, 10_000] {
            assert!(always.delay(retry, None, 0.5).is_some(), "always 第 {retry} 次仍应重试");
        }
        for kind in [
            ModelErrorKind::Auth,
            ModelErrorKind::Quota,
            ModelErrorKind::BadRequest,
            ModelErrorKind::ContextOverflow,
            ModelErrorKind::HookDenied,
        ] {
            assert!(always.accepts(kind), "always 不筛失败类别：{kind:?}");
        }

        let default = RetryPolicy::new(3);
        assert_eq!(default.mode, super::RetryMode::Bounded, "默认模式必须还是 bounded");
        assert!(default.enabled());
        assert!(!default.accepts(ModelErrorKind::Auth), "bounded 的确定性失败不得重试");
        assert_eq!(default.delay(4, None, 0.5), None, "bounded 超预算必须放弃");
        assert!(!RetryPolicy::disabled().enabled(), "bounded(0) 仍然是关闭");
    }

    /// D5：always 遇到超上限的 `Retry-After` 用本地退避接着重试，
    /// bounded 保持「超上限即放弃」——两条语义不许互相污染。
    #[test]
    fn always_mode_degrades_oversize_retry_after_to_local_backoff() {
        let always = RetryPolicy { mode: super::RetryMode::Always, ..no_jitter(0) };
        assert_eq!(
            always.delay(1, Some(120_000), 0.5),
            Some(Duration::from_millis(500)),
            "always 不能等 120 秒，也不能直接放弃"
        );
        assert_eq!(
            no_jitter(5).delay(1, Some(120_000), 0.5),
            None,
            "bounded 的既有判据不得放宽"
        );
    }
}
