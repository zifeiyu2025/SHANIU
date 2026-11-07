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
    /// 派生账本的父账本（`spawn_agent.budget_tokens`）：消耗向上汇总，
    /// 上限沿链向下生效。深度硬限为 1（ADR-101），链长实际不超过 2。
    parent: Option<Arc<Ledger>>,
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

impl Default for TreeBudget {
    /// 默认不限额：既有调用方（测试、无编排的单代理运行）不该被迫先配预算。
    fn default() -> Self {
        Self::unlimited()
    }
}

impl TreeBudget {
    pub fn new(token_limit: Option<u64>, round_limit: Option<u32>) -> Self {
        Self::with_parent(token_limit, round_limit, None)
    }

    fn with_parent(
        token_limit: Option<u64>,
        round_limit: Option<u32>,
        parent: Option<Arc<Ledger>>,
    ) -> Self {
        Self {
            inner: Arc::new(Ledger {
                token_limit,
                tokens: AtomicU64::new(0),
                round_limit,
                rounds: AtomicU32::new(0),
                parent,
            }),
        }
    }

    /// 为派生出的子树建一本子账（`spawn_agent.budget_tokens`）。
    ///
    /// 语义是「这棵子树最多花这么多」：子账本自己的上限先拦，父账本照旧汇总全部后代的
    /// 消耗并用自己的上限拦。两个上限任一触顶，子句柄都领不到新一轮。没有限额时直接
    /// 复用父账本，不制造多余层级。
    pub fn derive(&self, token_limit: Option<u64>, round_limit: Option<u32>) -> Self {
        if token_limit.is_none() && round_limit.is_none() {
            return self.clone();
        }
        Self::with_parent(token_limit, round_limit, Some(self.inner.clone()))
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
    /// 越过之后由 `exhaustion`/`claim_round` 拦下一次。子账本会逐级向上汇总，
    /// 因此父账本的 `tokens_spent` 始终等于「本代理自身 + 全部后代」。
    pub fn record_tokens(&self, tokens: u64) {
        let mut cursor = Some(self.inner.as_ref());
        while let Some(ledger) = cursor {
            ledger.tokens.fetch_add(tokens, Ordering::Relaxed);
            cursor = ledger.parent.as_deref();
        }
    }

    /// 领一轮（= 一次模型请求名额）。触顶返回 `Err`，调用方据此暂停整树。
    ///
    /// 先占自身名额，再沿链向上逐级占；任一级已满则回滚已占的名额并返回 `Err`。
    /// 回滚窗口内理论上有并发抢占的边角（轮数可能多算一次），但 token 上限在
    /// 领名额之前已判定，不会因此放行超额请求。
    pub fn claim_round(&self) -> Result<(), Exhaustion> {
        if let Some(exhausted) = self.exhaustion() {
            return Err(exhausted);
        }
        bump_rounds(&self.inner)?;
        let mut reserved: Vec<Arc<Ledger>> = Vec::new();
        let mut cursor = self.inner.parent.clone();
        while let Some(ledger) = cursor {
            if let Err(exhausted) = bump_rounds(&ledger) {
                self.inner.rounds.fetch_sub(1, Ordering::Relaxed);
                for held in &reserved {
                    held.rounds.fetch_sub(1, Ordering::Relaxed);
                }
                return Err(exhausted);
            }
            cursor = ledger.parent.clone();
            reserved.push(ledger);
        }
        Ok(())
    }

    pub fn tokens_spent(&self) -> u64 {
        self.inner.tokens.load(Ordering::Relaxed)
    }

    pub fn rounds_started(&self) -> u32 {
        self.inner.rounds.load(Ordering::Relaxed)
    }

    /// 全树（走到最顶层账本）的累计 token，用于错误信息——不能只报本子树，
    /// 否则触顶原因是父账本时用户会看到「已用 10 / 上限 100 却停了」。
    pub fn tree_tokens_spent(&self) -> u64 {
        let mut cursor = self.inner.clone();
        while let Some(parent) = cursor.parent.clone() {
            cursor = parent;
        }
        cursor.tokens.load(Ordering::Relaxed)
    }

    /// 剩余 token；`None` = 不限额。
    pub fn remaining_tokens(&self) -> Option<u64> {
        self.inner
            .token_limit
            .map(|limit| limit.saturating_sub(self.tokens_spent()))
    }

    /// 当前是否触顶；先看自身、再沿链看祖先。两个维度都触顶时先报 `Tokens`。
    pub fn exhaustion(&self) -> Option<Exhaustion> {
        let mut cursor = Some(self.inner.as_ref());
        while let Some(ledger) = cursor {
            if let Some(limit) = ledger.token_limit {
                if ledger.tokens.load(Ordering::Relaxed) >= limit {
                    return Some(Exhaustion::Tokens);
                }
            }
            cursor = ledger.parent.as_deref();
        }
        let mut cursor = Some(self.inner.as_ref());
        while let Some(ledger) = cursor {
            if let Some(limit) = ledger.round_limit {
                if ledger.rounds.load(Ordering::Relaxed) >= limit {
                    return Some(Exhaustion::Rounds);
                }
            }
            cursor = ledger.parent.as_deref();
        }
        None
    }
}

/// 在单个账本上占一轮；优先 `try_update` 保证并发下不超限。
fn bump_rounds(ledger: &Arc<Ledger>) -> Result<(), Exhaustion> {
    match ledger.round_limit {
        None => {
            ledger.rounds.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        // try_update：并发下也绝不超过上限（先比较再自增是一个原子操作）。
        Some(limit) => ledger
            .rounds
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |started| {
                (started < limit).then_some(started + 1)
            })
            .map(|_| ())
            .map_err(|_| Exhaustion::Rounds),
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
    fn derived_subtree_limit_blocks_itself_but_not_siblings() {
        let parent = TreeBudget::unlimited();
        let limited = parent.derive(Some(40), None);
        let sibling = parent.child();
        assert!(limited.claim_round().is_ok());
        limited.record_tokens(40);
        // 子树自己的上限触顶：它领不到，兄弟不受影响。
        assert_eq!(limited.claim_round(), Err(Exhaustion::Tokens));
        assert!(sibling.claim_round().is_ok());
        // 但消耗已经向上汇总到父账本。
        assert_eq!(parent.tokens_spent(), 40);
        assert_eq!(limited.tree_tokens_spent(), 40);
    }

    #[test]
    fn parent_limit_also_blocks_a_derived_subtree() {
        let parent = TreeBudget::new(Some(50), None);
        let limited = parent.derive(Some(1000), None);
        assert!(limited.claim_round().is_ok());
        limited.record_tokens(60);
        assert_eq!(limited.tokens_spent(), 60);
        // 全树上限先到，子账本自己还很宽裕也必须停。
        assert_eq!(limited.exhaustion(), Some(Exhaustion::Tokens));
        assert_eq!(limited.claim_round(), Err(Exhaustion::Tokens));
        assert_eq!(parent.claim_round(), Err(Exhaustion::Tokens));
        assert_eq!(limited.tree_tokens_spent(), 60);
    }

    #[test]
    fn derive_without_limits_reuses_the_same_ledger() {
        let parent = TreeBudget::new(Some(100), None);
        let child = parent.derive(None, None);
        child.record_tokens(30);
        assert_eq!(parent.tokens_spent(), 30);
        assert_eq!(parent.remaining_tokens(), Some(70));
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
