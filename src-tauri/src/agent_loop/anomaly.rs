//! SH-122 空转检测：连续相同工具调用 → 注入提醒（照 ZCode
//! `runtime/helpers/model-anomaly.ts:23-62` 的语义，阈值按 SH-122 验收取 4）。
//!
//! 判据是「同一工具 + 规范化参数」（对象键排序后再比较，避免同义 JSON 漏判）。
//! 提醒以独立 `Role::System` 条目进入上下文，不伪装成工具结果、不计入工具失败率；
//! 每轮最多 3 条，普通重复不重复刷屏（只在连续第 4 次触发一次）。

/// 连续命中多少次算空转（第 3 次仍容忍，第 4 次提醒）。
pub const REPEATED_CALL_THRESHOLD: u32 = 4;
/// 每轮最多注入条数（超出后静默抑制）。
pub const MAX_REMINDERS_PER_TURN: u32 = 3;

#[derive(Debug, Default)]
pub struct AnomalyGuard {
    last_signature: Option<String>,
    streak: u32,
    reminders_this_turn: u32,
}

impl AnomalyGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// 每轮开始时重置提醒预算；连续计数跨轮保留（跨轮重复同样算空转）。
    pub fn begin_turn(&mut self) {
        self.reminders_this_turn = 0;
    }

    /// 观测一次模型发起的工具调用；触发且未超预算时返回提醒文本。
    pub fn observe(&mut self, tool: &str, arguments: &str) -> Option<String> {
        let signature = signature(tool, arguments);
        if self.last_signature.as_deref() == Some(signature.as_str()) {
            self.streak += 1;
        } else {
            self.last_signature = Some(signature);
            self.streak = 1;
        }
        if self.streak != REPEATED_CALL_THRESHOLD {
            return None;
        }
        if self.reminders_this_turn >= MAX_REMINDERS_PER_TURN {
            return None;
        }
        self.reminders_this_turn += 1;
        Some(format!(
            "空转提醒：你已经连续 {} 次用完全相同的参数调用 `{}`。不要再用同样参数重复调用；\
             请基于已有结果换一个步骤，或说明卡点并向用户求助。",
            self.streak, tool
        ))
    }
}

fn signature(tool: &str, arguments: &str) -> String {
    let normalized = serde_json::from_str::<serde_json::Value>(arguments)
        .map(|value| stable_json(&value))
        .unwrap_or_else(|_| arguments.trim().to_string());
    format!("{tool}\u{0}{normalized}")
}

fn stable_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let body = keys
                .iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_default(),
                        stable_json(&map[*key])
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{body}}}")
        }
        serde_json::Value::Array(items) => format!(
            "[{}]",
            items.iter().map(stable_json).collect::<Vec<_>>().join(",")
        ),
        other => serde_json::to_string(other).unwrap_or_else(|_| "null".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fires_on_the_fourth_identical_call() {
        let mut guard = AnomalyGuard::new();
        guard.begin_turn();
        for call in 1..REPEATED_CALL_THRESHOLD {
            assert!(
                guard.observe("read_file", r#"{"path":"a.txt"}"#).is_none(),
                "第 {call} 次不应提醒"
            );
        }
        let reminder = guard.observe("read_file", r#"{"path":"a.txt"}"#).expect("第 4 次必须提醒");
        assert!(reminder.contains("连续 4 次"), "{reminder}");
        assert!(reminder.contains("read_file"), "{reminder}");
    }

    #[test]
    fn different_arguments_reset_the_streak() {
        let mut guard = AnomalyGuard::new();
        guard.begin_turn();
        assert!(guard.observe("read_file", r#"{"path":"a.txt"}"#).is_none());
        assert!(guard.observe("read_file", r#"{"path":"a.txt"}"#).is_none());
        assert!(guard.observe("read_file", r#"{"path":"a.txt"}"#).is_none());
        // 参数不同 → 重置计数
        assert!(guard.observe("read_file", r#"{"path":"b.txt"}"#).is_none());
        // 换回 a.txt 也只是新序列的第 2 次
        assert!(guard.observe("read_file", r#"{"path":"a.txt"}"#).is_none());
        assert!(guard.observe("read_file", r#"{"path":"b.txt"}"#).is_none());
    }

    #[test]
    fn json_key_order_does_not_break_the_signature() {
        let mut guard = AnomalyGuard::new();
        guard.begin_turn();
        for _ in 0..3 {
            assert!(guard
                .observe("apply_patch", r#"{"patch":"x","note":"y"}"#)
                .is_none());
        }
        // 同样的语义、不同的键序，仍应被识别为同一次调用
        assert!(guard
            .observe("apply_patch", r#"{"note":"y","patch":"x"}"#)
            .is_some());
    }

    #[test]
    fn fires_once_per_streak() {
        let mut guard = AnomalyGuard::new();
        guard.begin_turn();
        for _ in 0..4 {
            guard.observe("read_file", r#"{"path":"a.txt"}"#);
        }
        // 第 5 次不再刷屏
        assert!(guard.observe("read_file", r#"{"path":"a.txt"}"#).is_none());
        assert!(guard.observe("read_file", r#"{"path":"a.txt"}"#).is_none());
    }

    #[test]
    fn caps_reminders_at_three_per_turn() {
        let mut guard = AnomalyGuard::new();
        guard.begin_turn();
        let mut reminders = 0;
        for tool in ["a", "b", "c", "d"] {
            for _ in 0..REPEATED_CALL_THRESHOLD {
                if guard.observe(tool, "{}").is_some() {
                    reminders += 1;
                }
            }
        }
        assert_eq!(reminders, MAX_REMINDERS_PER_TURN);
        // 下一轮预算重置
        guard.begin_turn();
        assert!(guard.observe("e", "{}").is_none());
        assert!(guard.observe("e", "{}").is_none());
        assert!(guard.observe("e", "{}").is_none());
        assert!(guard.observe("e", "{}").is_some());
    }
}
