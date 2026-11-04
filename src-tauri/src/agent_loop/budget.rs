//! 轮数/token 硬上限（docs §8 agent_loop/budget.rs，风险登记册 R3：无限循环烧 token）。

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
}
