//! SH-125 失败自愈闭环（自研项——ZCode 只做对话证据判断，codex 不自动读错重试）。
//!
//! 职责边界：只观测**命令类工具**的失败结果，从输出里提取「文件:行」定位，并在下一轮
//! 以独立 `Role::System` 条目提示模型去读错、修复、重跑。它不改写工具结果、不伪造成功，
//! 也不替模型执行修复——修复动作仍然由模型经正常工具链完成。
//!
//! 停止条件（任一命中即放弃并向模型明示，之后不再补充提醒）：
//! 1. 已用尽自动修复次数（默认 2 次）；
//! 2. 相邻两次失败的错误签名完全相同（同一错误重复 = 上一轮修复没有起作用）。
//!
//! 命令成功后状态清零——失败被真正修好才算闭环完成。

/// 默认自动修复尝试次数。
pub const DEFAULT_REPAIR_ATTEMPTS: u32 = 2;
/// 回填给模型的定位条数上限（够定位，又不至于把上下文塞满）。
const MAX_DIAGNOSTICS: usize = 5;
/// 无「文件:行」时，用错误输出末尾几行做签名。
const SIGNATURE_TAIL_LINES: usize = 3;

/// 一条错误定位。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// `path:line` 或 `path:line:col`
    pub location: String,
    /// 同一行里的错误描述（尽力而为）
    pub message: String,
}

/// 本轮要给模型追加的提示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairAction {
    /// 还有重试额度：提示读错并重试。
    Nudge(String),
    /// 停止自动修复：提示如实报告，不得把失败说成成功。
    GiveUp(String),
}

/// 是否是命令类工具（只有这类失败才进入自愈闭环）。
fn is_command_tool(tool: &str) -> bool {
    matches!(tool, "unified_exec" | "exec" | "shell" | "command")
}

fn looks_like_path(text: &str) -> bool {
    !text.is_empty()
        && !text.chars().any(char::is_whitespace)
        && (text.contains('/')
            || text.contains('\\')
            || text.contains('.')
            || text.ends_with(".rs"))
}

/// 从单行里解析 `path:line[:col]`（rustc 的 `--> path:12:5`、tsc/gcc 的 `path:12:5: msg`
/// 以及 `panicked at path:12:5:` 都覆盖）。
fn parse_location(line: &str) -> Option<Diagnostic> {
    let text = line.trim();
    let text = text.strip_prefix("-->").map(str::trim_start).unwrap_or(text);
    let mut from = 0usize;
    while let Some(relative) = text[from..].find(':') {
        let colon = from + relative;
        let path = text[..colon].trim();
        let after = &text[colon + 1..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 && looks_like_path(path) {
            let line_no = &after[..digits];
            let rest = &after[digits..];
            let column = rest.strip_prefix(':').and_then(|r| {
                let count = r.bytes().take_while(u8::is_ascii_digit).count();
                (count > 0).then(|| &r[..count])
            });
            let location = match column {
                Some(column) => format!("{path}:{line_no}:{column}"),
                None => format!("{path}:{line_no}"),
            };
            let message = rest.trim_start_matches(':').trim().to_string();
            return Some(Diagnostic { location, message });
        }
        from = colon + 1;
    }
    None
}

/// 提取错误定位（去重、保序、截断到 [`MAX_DIAGNOSTICS`] 条）。
pub fn extract_diagnostics(output: &str) -> Vec<Diagnostic> {
    let mut seen = Vec::new();
    let mut result = Vec::new();
    for line in output.lines() {
        let Some(diagnostic) = parse_location(line) else { continue };
        if seen.contains(&diagnostic.location) {
            continue;
        }
        seen.push(diagnostic.location.clone());
        result.push(diagnostic);
        if result.len() >= MAX_DIAGNOSTICS {
            break;
        }
    }
    result
}

/// 失败签名：优先用定位 + 描述，其次用输出末尾几行——用于识别「同一错误重复」。
fn failure_signature(output: &str, diagnostics: &[Diagnostic]) -> String {
    if !diagnostics.is_empty() {
        return diagnostics
            .iter()
            .map(|d| format!("{}|{}", d.location, d.message))
            .collect::<Vec<_>>()
            .join("\n");
    }
    let tail: Vec<&str> = output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .rev()
        .take(SIGNATURE_TAIL_LINES)
        .collect();
    tail.into_iter().rev().collect::<Vec<_>>().join("\n")
}

/// 每任务一份的失败自愈状态机（与 `StopHook` 同级，不跨任务复用）。
#[derive(Debug)]
pub struct RepairController {
    max_attempts: u32,
    attempts: u32,
    last_failure: Option<String>,
    given_up: bool,
}

impl RepairController {
    pub fn new(max_attempts: u32) -> Self {
        Self {
            max_attempts,
            attempts: 0,
            last_failure: None,
            given_up: false,
        }
    }

    /// 观测一次工具结果；返回需要追加到历史的提醒。
    pub fn observe(&mut self, tool: &str, success: bool, output: &str) -> Option<RepairAction> {
        if !is_command_tool(tool) {
            return None;
        }
        if success {
            // 命令通过 = 闭环完成（无论之前失败过几次）。
            self.attempts = 0;
            self.last_failure = None;
            self.given_up = false;
            return None;
        }
        if self.given_up {
            // 已明确告知放弃，不再刷屏；由模型/用户决定下一步。
            return None;
        }

        let diagnostics = extract_diagnostics(output);
        let signature = failure_signature(output, &diagnostics);
        let repeated = self.last_failure.as_deref() == Some(signature.as_str());

        if repeated {
            self.given_up = true;
            return Some(RepairAction::GiveUp(give_up_message(
                "同一错误再次出现，上一轮修复没有生效",
            )));
        }
        if self.attempts >= self.max_attempts {
            self.given_up = true;
            return Some(RepairAction::GiveUp(give_up_message("自动修复次数已用尽")));
        }

        self.attempts += 1;
        self.last_failure = Some(signature);
        Some(RepairAction::Nudge(nudge_message(
            self.attempts,
            self.max_attempts,
            &diagnostics,
        )))
    }
}

fn nudge_message(attempt: u32, max_attempts: u32, diagnostics: &[Diagnostic]) -> String {
    let mut message = format!(
        "自动修复提醒（第 {attempt}/{max_attempts} 次）：上一条命令执行失败。"
    );
    if diagnostics.is_empty() {
        message.push_str("请先根据上面的错误输出定位原因，修复后再重跑同一条命令。");
    } else {
        message.push_str("错误定位：");
        for diagnostic in diagnostics {
            if diagnostic.message.is_empty() {
                message.push_str(&format!("\n- {}", diagnostic.location));
            } else {
                message.push_str(&format!(
                    "\n- {}（{}）",
                    diagnostic.location,
                    diagnostic.message.chars().take(160).collect::<String>()
                ));
            }
        }
        message.push_str(
            "\n请先用 read_file 读取上述位置的真实内容，修复后重新运行同一条命令；不要猜文件内容。",
        );
    }
    message.push_str("在命令真正通过之前，不得声称已修复。");
    message
}

fn give_up_message(reason: &str) -> String {
    format!(
        "失败自愈已停止（{reason}）。现在必须：① 停止对同一命令继续自动重试；\
         ② 向用户如实报告仍未解决的错误、已尝试的修复与当前证据；\
         ③ 不得把失败描述为成功，也不得跳过失败的验证步骤。"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUSTC_OUTPUT: &str = "   Compiling demo v0.1.0\n\
        error[E0308]: mismatched types\n\
         --> src/lib.rs:4:18\n\
          |\n\
        4 |     let n: i32 = \"x\";\n\
          |            ---   ^^^ expected `i32`, found `&str`\n";

    #[test]
    fn extracts_rustc_arrow_location() {
        let diagnostics = extract_diagnostics(RUSTC_OUTPUT);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].location, "src/lib.rs:4:18");
    }

    #[test]
    fn extracts_colon_location_without_arrow() {
        let output = "src/main.ts:12:5 - error TS2322: Type 'string' is not assignable to type 'number'.";
        let diagnostics = extract_diagnostics(output);
        assert_eq!(diagnostics[0].location, "src/main.ts:12:5");
        assert!(diagnostics[0].message.contains("TS2322"), "{diagnostics:?}");
    }

    #[test]
    fn ignores_lines_without_file_location() {
        let output = "error: could not compile `demo` due to previous error\n\
                      test result: FAILED. 1 passed; 1 failed";
        assert!(extract_diagnostics(output).is_empty());
    }

    #[test]
    fn nudges_first_failure_then_gives_up_on_repeated_error() {
        let mut controller = RepairController::new(DEFAULT_REPAIR_ATTEMPTS);
        let first = controller.observe("unified_exec", false, RUSTC_OUTPUT);
        let Some(RepairAction::Nudge(text)) = first else {
            panic!("首次失败应给出提醒: {first:?}");
        };
        assert!(text.contains("第 1/2 次"), "{text}");
        assert!(text.contains("src/lib.rs:4:18"), "{text}");

        let second = controller.observe("unified_exec", false, RUSTC_OUTPUT);
        let Some(RepairAction::GiveUp(text)) = second else {
            panic!("同一错误重复应放弃: {second:?}");
        };
        assert!(text.contains("同一错误"), "{text}");
        // 放弃后不再补充提醒
        assert!(controller
            .observe("unified_exec", false, RUSTC_OUTPUT)
            .is_none());
    }

    #[test]
    fn different_errors_allow_second_attempt_then_give_up() {
        let mut controller = RepairController::new(DEFAULT_REPAIR_ATTEMPTS);
        assert!(matches!(
            controller.observe("unified_exec", false, "src/a.rs:1:1 error A"),
            Some(RepairAction::Nudge(_))
        ));
        assert!(matches!(
            controller.observe("unified_exec", false, "src/b.rs:2:2 error B"),
            Some(RepairAction::Nudge(_))
        ));
        let third = controller.observe("unified_exec", false, "src/c.rs:3:3 error C");
        let Some(RepairAction::GiveUp(text)) = third else {
            panic!("超过重试上限应放弃: {third:?}");
        };
        assert!(text.contains("次数已用尽"), "{text}");
    }

    #[test]
    fn success_resets_the_cycle() {
        let mut controller = RepairController::new(DEFAULT_REPAIR_ATTEMPTS);
        assert!(controller
            .observe("unified_exec", false, "src/a.rs:1:1 error A")
            .is_some());
        assert!(controller.observe("unified_exec", true, "test result: ok").is_none());
        // 修好后再次失败，重新计数（第 1 次而不是第 2 次）
        let again = controller.observe("unified_exec", false, "src/a.rs:1:1 error A");
        let Some(RepairAction::Nudge(text)) = again else {
            panic!("成功后应重置计数: {again:?}");
        };
        assert!(text.contains("第 1/2 次"), "{text}");
    }

    #[test]
    fn non_command_tools_never_nudge() {
        let mut controller = RepairController::new(DEFAULT_REPAIR_ATTEMPTS);
        assert!(controller
            .observe("apply_patch", false, "write_file_stale: 拒绝覆盖")
            .is_none());
        assert!(controller.observe("read_file", false, "不存在").is_none());
    }
}
