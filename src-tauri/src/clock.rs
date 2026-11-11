//! 全仓唯一的「当前时间」出口（docs/01 §2.3.1 E63）。
//!
//! R3：收敛前有九个模块各写一份 `now_ms()`，语义逐字相同（UNIX 纪元毫秒、`u64`、
//! 时钟早于 1970 回落 0），只是闭包变量名和 `unwrap_or` 写法不同。现在只留这一份。
//!
//! 本模块是纯叶子，只依赖 `std`；任何模块依赖它都不产生反向依赖。

use std::time::{SystemTime, UNIX_EPOCH};

/// 当前 UNIX 纪元毫秒。
///
/// 时钟早于 1970（系统时间被拨回）时返回 0 而不是 panic：调用方都是给时间戳字段赋值
/// （`at_ms` / `created_at_ms` / `granted_at_ms` / 作业 id …），返回 0 只影响展示，
/// panic 会连带拖垮整条会话或作业链路。这条回落语义与收敛前逐字一致。
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2024-01-01T00:00:00Z，早于它的结果只能是「秒级误用」或「回落 0」。
    const LOWER: u64 = 1_704_067_200_000;
    /// 2100-01-01T00:00:00Z，晚于它说明单位或时基被改坏。
    const UPPER: u64 = 4_102_444_800_000;

    #[test]
    fn now_ms_is_epoch_milliseconds() {
        let t = now_ms();
        assert!(
            t > LOWER,
            "now_ms 不是毫秒级纪元时间戳（疑似改成秒或返回 0）：{t}"
        );
        assert!(t < UPPER, "now_ms 超出合理区间，时基可能被改坏：{t}");
    }

    #[test]
    fn now_ms_never_goes_backwards() {
        let first = now_ms();
        let second = now_ms();
        assert!(second >= first, "now_ms 回退：{first} → {second}");
    }
}
