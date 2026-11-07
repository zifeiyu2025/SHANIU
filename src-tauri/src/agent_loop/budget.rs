//! 轮数/token 硬上限（docs §8 agent_loop/budget.rs，风险登记册 R3：无限循环烧 token）。
//!
//! 两类预算各管一摊，别混：
//! - `TurnBudget`：**单代理**的轮数硬顶（每轮一次采样），随配置固化；
//! - `TreeBudget`：**整棵树**的 token/轮数共账（02 ADR-103 / FF-103）。父任务创建账本，
//!   每个后代克隆同一个句柄，消耗汇总；任一代理触顶后所有句柄都无法再领到新一轮。
//!   触顶的处置（暂停并询问用户）由调用方负责，账本只回答"还能不能继续"。

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use crate::config::AppConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnBudget {
    pub max_turns: u32,
}

impl TurnBudget {
    pub fn new(max_turns: u32) -> Self {
        Self { max_turns: max_turns.max(1) }
    }

    pub fn from_config(cfg: &AppConfig) -> Self {
        Self::new(cfg.model.max_turns)
    }
}

/// 触顶的维度；`Tokens` 优先报告（钱比轮数更硬）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exhaustion {
    Tokens,
    Rounds,
}

#[derive(Debug)]
struct Ledger {
    /// `None` = 不限额（与"限额为 0"区分：0 表示一个 token 都不给）。
    token_limit: Option<u64>,
    tokens: AtomicU64,
    round_limit: Option<u32>,
    rounds: AtomicU32,
}

/// 树级预算句柄：克隆即共享同一本账（父代理与全部后代同账）。
///
/// 契约（FF-103）：任一代理 `record_tokens` 越过上限后，**所有**句柄的
/// `claim_round` 都必须失败——这是"触顶后全树无新模型请求"的执行点。
/// 判定发生在发起请求之前；已经在途的请求不会被中断（那是暂停语义，由调用方处理）。
#[derive(Debug, Clone)]
pub struct TreeBudget {
    inner: Arc<Ledger>,
}

impl TreeBudget {
    pub fn new(token_limit: Option<u64>, round_limit: Option<u32>) -> Self {
        Self {
            inner: Arc::new(Ledger {
                token_limit,
                tokens: AtomicU64::new(0),
                round_limit,
                rounds: AtomicU32::new(0),
            }),
        }
    }

    /// 不限额的账本：仍需记账（供指标展示），但永不触顶。
    pub fn unlimited() -> Self {
        Self::new(None, None)
    }

    /// 后代拿到的句柄：同一个 `Arc`，不是新账本。
    pub fn child(&self) -> Self {
        self.clone()
    }

    /// 记一笔模型用量（输入+输出总量）。允许越过上限（在途请求已发生），
    /// 越过之后由 `exhaustion`/`claim_round` 拦下一次。
    pub fn record_tokens(&self, tokens: u64) {
        self.inner.tokens.fetch_add(tokens, Ordering::Relaxed);
    }

    /// 领一轮（= 一次模型请求名额）。触顶返回 `Err`，调用方据此暂停整树。
    pub fn claim_round(&self) -> Result<(), Exhaustion> {
        if let Some(exhausted) = self.exhaustion() {
            return Err(exhausted);
        }
        match self.inner.round_limit {
            None => {
                self.inner.rounds.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            // try_update：并发下也绝不超过上限（先比较再自增是一个原子操作）。
            Some(limit) => self
                .inner
                .rounds
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |started| {
                    (started < limit).then_some(started + 1)
                })
                .map(|_| ())
                .map_err(|_| Exhaustion::Rounds),
        }
    }

    pub fn tokens_spent(&self) -> u64 {
        self.inner.tokens.load(Ordering::Relaxed)
    }

    pub fn rounds_started(&self) -> u32 {
        self.inner.rounds.load(Ordering::Relaxed)
    }

    /// 剩余 token；`None` = 不限额。
    pub fn remaining_tokens(&self) -> Option<u64> {
        self.inner
            .token_limit
            .map(|limit| limit.saturating_sub(self.tokens_spent()))
    }

    /// 当前是否触顶；两个维度都触顶时先报 `Tokens`。
    pub fn exhaustion(&self) -> Option<Exhaustion> {
        if let Some(limit) = self.inner.token_limit {
            if self.tokens_spent() >= limit {
                return Some(Exhaustion::Tokens);
            }
        }
        if let Some(limit) = self.inner.round_limit {
            if self.rounds_started() >= limit {
                return Some(Exhaustion::Rounds);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn min_one_turn() {
        assert_eq!(TurnBudget::new(0).max_turns, 1);
        assert_eq!(TurnBudget::new(7).max_turns, 7);
    }

    #[test]
    fn from_config_uses_max_turns() {
        let mut cfg = AppConfig::default();
        cfg.model.max_turns = 12;
        assert_eq!(TurnBudget::from_config(&cfg).max_turns, 12);
    }

    #[test]
    fn tree_budget_child_shares_the_same_ledger() {
        let parent = TreeBudget::new(Some(100), None);
        let child = parent.child();
        child.record_tokens(40);
        parent.record_tokens(30);
        // 无论从哪个句柄记，都是同一本账。
        assert_eq!(parent.tokens_spent(), 70);
        assert_eq!(child.tokens_spent(), 70);
        assert_eq!(parent.remaining_tokens(), Some(30));
    }

    #[test]
    fn token_exhaustion_blocks_new_rounds_on_every_handle() {
        // FF-103 的核心契约：触顶后全树无新模型请求。
        let parent = TreeBudget::new(Some(100), None);
        let child = parent.child();
        assert!(child.claim_round().is_ok());
        child.record_tokens(120);
        assert_eq!(parent.exhaustion(), Some(Exhaustion::Tokens));
        assert_eq!(parent.claim_round(), Err(Exhaustion::Tokens));
        assert_eq!(child.claim_round(), Err(Exhaustion::Tokens));
        // 触顶后也不再把轮数往前推。
        assert_eq!(parent.rounds_started(), 1);
    }

    #[test]
    fn round_limit_is_shared_across_the_tree() {
        let parent = TreeBudget::new(None, Some(2));
        let child = parent.child();
        assert!(child.claim_round().is_ok());
        assert!(parent.claim_round().is_ok());
        assert_eq!(child.claim_round(), Err(Exhaustion::Rounds));
        assert_eq!(parent.exhaustion(), Some(Exhaustion::Rounds));
    }

    #[test]
    fn zero_token_limit_is_exhausted_not_unlimited() {
        // `None` 才是不限额；0 是"一个 token 都不给"，两者必须区分。
        let budget = TreeBudget::new(Some(0), None);
        assert_eq!(budget.exhaustion(), Some(Exhaustion::Tokens));
        assert_eq!(budget.claim_round(), Err(Exhaustion::Tokens));
        assert_eq!(budget.remaining_tokens(), Some(0));
    }

    #[test]
    fn unlimited_budget_still_counts_but_never_exhausts() {
        let budget = TreeBudget::unlimited();
        budget.record_tokens(10_000_000);
        assert_eq!(budget.exhaustion(), None);
        assert_eq!(budget.remaining_tokens(), None);
        assert!(budget.claim_round().is_ok());
        assert_eq!(budget.tokens_spent(), 10_000_000);
    }
}
