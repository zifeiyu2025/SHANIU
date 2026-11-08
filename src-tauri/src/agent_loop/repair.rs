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
//!
//! **SH-126 硬约束：不许靠改测试变绿。**
//! 自愈闭环 + 能改文件 = 存在「把断言删掉让测试通过」的路径。宿主看不到模型意图，
//! 所以在命令重新通过时，用写日志反查「这次通过有没有动测试」：
//! - 删减断言 / 强断言降级 / 新增 `#[ignore]` → [`RepairAction::RejectedRepair`]（按失败处理）；
//! - 只改了测试文件或断言内容、没有明显放宽 → [`RepairAction::SuspiciousRepair`]（必须提示）。

use std::path::{Path, PathBuf};

use crate::tool_router::FileWrite;

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
    /// 只含路径（不含行号），用于判定「失败是否落在测试文件」。
    pub file: String,
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
    /// 修复改了测试、且改动了断言（含删减/放宽）：不认这次成功，按失败处理。
    RejectedRepair(String),
    /// 修复改了测试文件但没检测到断言删减/放宽：记为「可疑修复」，必须提示。
    SuspiciousRepair(String),
}

/// 是否是命令类工具（只有这类失败才进入自愈闭环）。
pub fn is_command_tool(tool: &str) -> bool {
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
    // `cargo test` 的失败是 panic 而不是编译错误：`thread 'x' panicked at src/lib.rs:12:5:`。
    // 不剥掉这段前缀的话，路径候选里永远带空格，整行都解析不出来——
    // 而「测试跑挂了」正是自愈闭环最该定位的场景。
    let text = match text.split_once("panicked at ") {
        Some((_, rest)) => rest.trim_start(),
        None => text,
    };
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
            return Some(Diagnostic {
                location,
                file: path.to_string(),
                message,
            });
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

/// 是否属于「专用测试文件」（按路径判定，不需要读内容）。
fn is_dedicated_test_file(path: &Path) -> bool {
    let lower = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_lowercase();
    lower.contains("/tests/")
        || lower.starts_with("tests/")
        || lower.ends_with("_test.rs")
        || lower.ends_with("_tests.rs")
        || lower.ends_with("test_.py")
        || lower.ends_with("_test.py")
        || lower.ends_with(".test.ts")
        || lower.ends_with(".test.tsx")
        || lower.ends_with(".test.js")
        || lower.ends_with(".spec.ts")
        || lower.ends_with(".spec.tsx")
        || lower.ends_with(".spec.js")
}

/// 是否是测试相关文件：路径像测试，或源码里内联了测试块（Rust `#[cfg(test)]` 很常见，
/// 只按路径判会把 `src/lib.rs` 里的测试全漏掉）。
fn is_test_file(path: &Path, content: Option<&str>) -> bool {
    if is_dedicated_test_file(path) {
        return true;
    }
    let Some(content) = content else { return false };
    content.contains("#[cfg(test)]")
        || content.contains("#[test]")
        || content.contains("#[tokio::test]")
        || content.contains("describe(")
        || (content.contains("it(") && content.contains("expect("))
}

/// 每任务一份的失败自愈状态机（与 `StopHook` 同级，不跨任务复用）。
#[derive(Debug)]
pub struct RepairController {
    max_attempts: u32,
    attempts: u32,
    last_failure: Option<String>,
    given_up: bool,
    /// 工作区根（诊断里的相对路径据此定位真实文件）。
    workspace_root: PathBuf,
    /// 失败诊断点名过的、判定为测试相关的文件（归一化绝对路径）。
    watched: Vec<PathBuf>,
    /// 是否处在「已失败、尚未真正修好」的恢复窗口内。
    recovering: bool,
}

impl RepairController {
    pub fn new(max_attempts: u32, workspace_root: PathBuf) -> Self {
        Self {
            max_attempts,
            attempts: 0,
            last_failure: None,
            given_up: false,
            workspace_root: crate::tool_router::canonical_key(&workspace_root),
            watched: Vec::new(),
            recovering: false,
        }
    }

    /// 观测一次工具结果；返回需要追加到历史的提醒。
    pub fn observe(&mut self, tool: &str, success: bool, output: &str) -> Option<RepairAction> {
        if !is_command_tool(tool) {
            return None;
        }
        if success {
            // 命令通过 = 闭环完成（无论之前失败过几次）。
            // 注意这里**不**清 `watched`/`recovering`：是否靠改测试变绿，
            // 由 [`Self::check_test_tampering`] 在同一个成功事件上结算并清理。
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
        self.remember_watched(&diagnostics);
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

    /// 命令通过时结算：这次通过有没有动过测试。
    ///
    /// 只有「命令类工具 + 恢复窗口内」（前面确实失败过）才判定；窗口外的普通编辑不算修复。
    /// 无论结论如何，调用一次即结束窗口（避免同一次成功被反复判定）。
    pub fn check_test_tampering(
        &mut self,
        tool: &str,
        writes: &[FileWrite],
    ) -> Option<RepairAction> {
        if !is_command_tool(tool) || !self.recovering {
            return None;
        }
        self.recovering = false;
        let watched = std::mem::take(&mut self.watched);

        let mut rejected = Vec::new();
        let mut suspicious = Vec::new();
        for write in writes.iter().filter(|w| w.changed) {
            let is_watched = watched.contains(&write.path);
            let is_dedicated = is_dedicated_test_file(&write.path);
            if !is_watched && !is_dedicated {
                continue;
            }
            let name = self.display(&write.path);
            if write.weakened {
                let detail = if write.assertion_delta < 0 {
                    format!("{name}（断言净减少 {} 条）", -write.assertion_delta)
                } else {
                    format!("{name}（断言被放宽或测试被跳过）")
                };
                rejected.push(detail);
            } else if is_dedicated || write.assertions_touched {
                suspicious.push(name);
            }
        }

        if !rejected.is_empty() {
            return Some(RepairAction::RejectedRepair(rejected_message(&rejected)));
        }
        if !suspicious.is_empty() {
            return Some(RepairAction::SuspiciousRepair(suspicious_message(&suspicious)));
        }
        None
    }

    /// 是否处在「已失败、尚未真正修好」的恢复窗口内。
    pub fn is_recovering(&self) -> bool {
        self.recovering
    }

    /// 失败诊断点名的文件里，凡是测试相关的都纳入观察。
    fn remember_watched(&mut self, diagnostics: &[Diagnostic]) {
        self.recovering = true;
        for diagnostic in diagnostics {
            let raw = Path::new(&diagnostic.file);
            let full = if raw.is_absolute() {
                raw.to_path_buf()
            } else {
                self.workspace_root.join(raw)
            };
            let content = std::fs::read_to_string(&full).ok();
            if !is_test_file(&full, content.as_deref()) {
                continue;
            }
            let key = crate::tool_router::canonical_key(&full);
            if !self.watched.contains(&key) {
                self.watched.push(key);
            }
        }
    }

    fn display(&self, path: &Path) -> String {
        path.strip_prefix(&self.workspace_root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
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

fn rejected_message(files: &[String]) -> String {
    format!(
        "修复被拒绝：命令虽然通过了，但检测到修复过程中收窄了测试约束：\n{}\n\
         这种「通过」不算修复成功，本次任务按失败处理。\
         正确做法是把测试恢复到原样（断言不得删减、放宽或挂 #[ignore]），然后修被测代码本身。",
        files
            .iter()
            .map(|f| format!("- {f}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

fn suspicious_message(files: &[String]) -> String {
    format!(
        "可疑修复：命令虽然通过了，但通过在修复过程中修改了测试相关文件：\n{}\n\
         这次「通过」不能当作修复成功的证据。请向用户说明改了测试的哪一部分、为什么必须改；\
         如果只是为了让测试变绿，必须还原测试并修被测代码。",
        files
            .iter()
            .map(|f| format!("- {f}"))
            .collect::<Vec<_>>()
            .join("\n")
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

    fn controller(max_attempts: u32) -> RepairController {
        RepairController::new(max_attempts, std::env::temp_dir())
    }

    fn temp_workspace() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("shaniu-repair-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

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
    fn extracts_panic_location_from_cargo_test_output() {
        // cargo test 的失败形态是 panic，不是编译错误——自愈闭环最常遇到的正是这个。
        let output = "test adds ... FAILED\n\n\
            thread 'adds' panicked at src/lib.rs:12:5:\n\
            assertion `left == right` failed\n";
        let diagnostics = extract_diagnostics(output);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].location, "src/lib.rs:12:5");
        assert_eq!(diagnostics[0].file, "src/lib.rs", "必须能取出纯路径用于判定测试文件");
    }

    #[test]
    fn nudges_first_failure_then_gives_up_on_repeated_error() {
        let mut controller = controller(DEFAULT_REPAIR_ATTEMPTS);
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
        let mut controller = controller(DEFAULT_REPAIR_ATTEMPTS);
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
        let mut controller = controller(DEFAULT_REPAIR_ATTEMPTS);
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
        let mut controller = controller(DEFAULT_REPAIR_ATTEMPTS);
        assert!(controller
            .observe("apply_patch", false, "write_file_stale: 拒绝覆盖")
            .is_none());
        assert!(controller.observe("read_file", false, "不存在").is_none());
    }

    // ---------------- SH-126：不许靠改测试变绿 ----------------

    const BROKEN_TEST_FILE: &str = "#[test]\nfn adds() {\n    assert_eq!(add(2, 2), 4);\n}\n";

    /// 建一个「测试文件 + 失败输出指向它」的工作区，返回 (工作区, 失败输出)。
    fn failing_test_workspace(relative: &str, content: &str) -> (PathBuf, String) {
        let ws = temp_workspace();
        let full = ws.join(relative);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&full, content).unwrap();
        let output = format!(
            "test adds ... FAILED\nthread 'adds' panicked at {relative}:3:5:\nassertion failed"
        );
        (ws, output)
    }

    fn write_of(ws: &Path, relative: &str, before: &str, after: &str) -> FileWrite {
        let path = crate::tool_router::canonical_key(&ws.join(relative));
        FileWrite::between(path, Some(before), Some(after))
    }

    #[test]
    fn deleting_assertion_after_failure_is_rejected() {
        let (ws, failure) = failing_test_workspace("tests/check.rs", BROKEN_TEST_FILE);
        let mut controller = RepairController::new(DEFAULT_REPAIR_ATTEMPTS, ws.clone());
        assert!(matches!(
            controller.observe("unified_exec", false, &failure),
            Some(RepairAction::Nudge(_))
        ));
        // 模型把断言删掉，然后命令「通过」
        let after = "#[test]\nfn adds() {\n}\n";
        let writes = vec![write_of(&ws, "tests/check.rs", BROKEN_TEST_FILE, after)];
        assert!(controller.observe("unified_exec", true, "test result: ok").is_none());
        let verdict = controller.check_test_tampering("unified_exec", &writes);
        let Some(RepairAction::RejectedRepair(text)) = verdict else {
            panic!("删减断言必须判为拒绝，而不是修复成功: {verdict:?}");
        };
        assert!(text.contains("tests/check.rs"), "{text}");
        assert!(text.contains("按失败处理"), "{text}");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn widening_assertion_value_is_at_least_suspicious() {
        // 「条数不变，只把期望值改成能通过的值」——语义上无法自动判定是改宽还是重构，
        // 但改了测试文件这件事必须被看见（SH-126 的底线要求）。
        let (ws, failure) = failing_test_workspace("tests/check.rs", BROKEN_TEST_FILE);
        let mut controller = RepairController::new(DEFAULT_REPAIR_ATTEMPTS, ws.clone());
        assert!(controller.observe("unified_exec", false, &failure).is_some());
        let after = BROKEN_TEST_FILE.replace("add(2, 2), 4", "add(2, 2), 0");
        let writes = vec![write_of(&ws, "tests/check.rs", BROKEN_TEST_FILE, &after)];
        controller.observe("unified_exec", true, "test result: ok");
        let verdict = controller.check_test_tampering("unified_exec", &writes);
        let Some(RepairAction::SuspiciousRepair(text)) = verdict else {
            panic!("改测试期望值必须标为可疑修复: {verdict:?}");
        };
        assert!(text.contains("tests/check.rs"), "{text}");
        assert!(text.contains("不能当作修复成功的证据"), "{text}");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn fixing_inline_source_assertions_is_rejected() {
        // Rust 常见形态：实现与测试同在 src/lib.rs。改实现是正常修复，改断言才是问题。
        let source = "fn add(a: i32, b: i32) -> i32 { a - b }\n\n#[cfg(test)]\nmod t {\n    #[test]\n    fn adds() { assert_eq!(super::add(2, 2), 4); }\n}\n";
        let (ws, failure) = failing_test_workspace("src/lib.rs", source);
        let mut controller = RepairController::new(DEFAULT_REPAIR_ATTEMPTS, ws.clone());
        assert!(controller.observe("unified_exec", false, &failure).is_some());
        let widened = source.replace("assert_eq!(super::add(2, 2), 4);", "assert!(true);");
        let writes = vec![write_of(&ws, "src/lib.rs", source, &widened)];
        controller.observe("unified_exec", true, "test result: ok");
        assert!(
            matches!(
                controller.check_test_tampering("unified_exec", &writes),
                Some(RepairAction::RejectedRepair(_))
            ),
            "同一文件里把断言降级也必须被抓到"
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn fixing_implementation_only_is_not_flagged() {
        let source = "fn add(a: i32, b: i32) -> i32 { a - b }\n\n#[cfg(test)]\nmod t {\n    #[test]\n    fn adds() { assert_eq!(super::add(2, 2), 4); }\n}\n";
        let (ws, failure) = failing_test_workspace("src/lib.rs", source);
        let mut controller = RepairController::new(DEFAULT_REPAIR_ATTEMPTS, ws.clone());
        assert!(controller.observe("unified_exec", false, &failure).is_some());
        let fixed = source.replace("a - b", "a + b");
        let writes = vec![write_of(&ws, "src/lib.rs", source, &fixed)];
        controller.observe("unified_exec", true, "test result: ok");
        assert!(
            controller.check_test_tampering("unified_exec", &writes).is_none(),
            "只改实现、断言原样保留，不得被判可疑"
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn tampering_check_only_applies_inside_a_recovery_window() {
        // 没失败过就写测试文件 = 正常开发，不该被自愈闭环判定。
        let ws = temp_workspace();
        std::fs::create_dir_all(ws.join("tests")).unwrap();
        std::fs::write(ws.join("tests/check.rs"), BROKEN_TEST_FILE).unwrap();
        let mut controller = RepairController::new(DEFAULT_REPAIR_ATTEMPTS, ws.clone());
        let writes = vec![write_of(&ws, "tests/check.rs", BROKEN_TEST_FILE, "fn adds() {}\n")];
        assert!(controller.check_test_tampering("unified_exec", &writes).is_none());
        std::fs::remove_dir_all(&ws).ok();
    }
}
