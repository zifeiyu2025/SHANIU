//! 全仓唯一的「当前时间 → UNIX 毫秒」出口守门（docs/01 §2.3.1 E63）。
//!
//! 判据按**行为**而不是函数名：同一个文件里同时出现「duration_since + UNIX 纪元」
//! 与「毫秒换算」，就视为又造了一份时间戳出口。旧判据只匹配 `fn now_ms(`，
//! 改个名字就能绕过——历史上正是这样漏掉了十处等价实现。
//!
//! 只扫代码行，注释里的说明不算实现；探针字符串在运行时拼接，避免用例自己命中自己
//! （本文件也在被扫描的目录里）。

use std::fs;
use std::path::{Path, PathBuf};

/// `src/` 下允许自己算时间的文件：必须登记，并写明为什么不能换成 `clock::now_ms`。
const SRC_EXEMPT: &[(&str, &str)] = &[
    (
        "src/tools/web/fetch.rs",
        "缓存时间戳取秒（as_secs），单位与 clock::now_ms 不同，替换会改变缓存有效期",
    ),
    (
        "src/workspace_files.rs",
        "读文件 mtime 作为版本号，不是当前时间；换成 now_ms 会让每次读都判成变更",
    ),
    (
        "src/mcp_client/runtime.rs",
        "读可执行文件 mtime 做变更检测，不是当前时间",
    ),
    (
        "src/rollout/mod.rs",
        "读档案文件 mtime 作为会话 updated_at，不是当前时间",
    ),
];

/// `tests/` 下允许存在的等价实现，必须在此登记并写明理由。
///
/// 这不是「漏网名单」：`evals_baseline.rs` 那份返回 `u128` 且时钟早于 1970 时 panic，
/// 与 `clock::now_ms`（`u64`、回落 0）类型与失败语义都不同，R3 明确不合并。
const TESTS_EXEMPT: &[(&str, &str)] = &[(
    "tests/evals_baseline.rs",
    "返回 u128 且时钟早于 1970 时 panic，与 clock::now_ms 的 u64/回落 0 语义不同，R3 决定不合并",
)];

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// 文件里是否**同时**出现「duration_since + UNIX 纪元」与「毫秒换算」。
///
/// 按整文件判定而不是逐行：实现天然跨四五行（`SystemTime::now()` / `.duration_since` /
/// `.map(|d| d.as_millis())`），逐行匹配会放过最常见的写法。
fn computes_epoch_millis(path: &Path) -> bool {
    // 拼成完整探针：分开写是为了让本文件自己不出现这些字面量。
    let needles = [
        ["duration", "_since"].concat(),
        ["UNIX", "_EPOCH"].concat(),
        ["as_", "millis"].concat(),
    ];
    let Ok(text) = fs::read_to_string(path) else { return false };
    let code = text
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    needles.iter().all(|needle| code.contains(needle.as_str()))
}

/// 扫描目录，返回自造纪元毫秒的文件（相对 `src-tauri` 根，正斜杠）。
fn files_computing_epoch_millis(dir: &Path, manifest_dir: &Path) -> Vec<String> {
    let mut all = Vec::new();
    collect_rs(dir, &mut all);
    let mut found: Vec<String> = all
        .into_iter()
        .filter(|path| computes_epoch_millis(path))
        .map(|path| {
            path.strip_prefix(manifest_dir)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    found.sort();
    found
}

/// 扫描本身不能是空的：目录读不到或扩展名过滤写错时，下面的断言会假绿。
#[test]
fn scan_is_not_vacuous() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut src = Vec::new();
    collect_rs(&manifest_dir.join("src"), &mut src);
    let mut tests = Vec::new();
    collect_rs(&manifest_dir.join("tests"), &mut tests);
    assert!(src.len() > 50, "没扫到 src/ 的源码（{} 个），用例会假绿", src.len());
    assert!(!tests.is_empty(), "没扫到 tests/ 的源码，用例会假绿");
}

#[test]
fn src_only_clock_computes_epoch_millis() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let found = files_computing_epoch_millis(&manifest_dir.join("src"), manifest_dir);
    // 改坏方式示例：在任一模块里再写一份「duration_since + 毫秒换算」，这里立刻多一项。
    let mut expected: Vec<String> = SRC_EXEMPT.iter().map(|(path, _)| (*path).to_string()).collect();
    expected.push("src/clock.rs".to_string());
    expected.sort();
    // 双向断言：既不许新增私货，也不许把登记项悄悄删掉换成未登记的文件。
    assert_eq!(
        found, expected,
        "src/ 下自造纪元毫秒的文件必须与登记逐一对应，实际：{found:?}"
    );
    for (path, reason) in SRC_EXEMPT {
        assert!(!reason.trim().is_empty(), "src/ 豁免 {path} 必须写明理由");
    }
}

#[test]
fn tests_only_have_registered_epoch_millis() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let found = files_computing_epoch_millis(&manifest_dir.join("tests"), manifest_dir);
    let mut expected: Vec<String> = TESTS_EXEMPT.iter().map(|(path, _)| (*path).to_string()).collect();
    expected.sort();
    // 双向断言：既不许新增私货，也不许为了让用例变绿把豁免那份删掉/合并掉。
    assert_eq!(
        found, expected,
        "tests/ 下的等价实现必须与豁免登记逐一对应，实际：{found:?}"
    );
    for (path, reason) in TESTS_EXEMPT {
        assert!(
            !reason.trim().is_empty(),
            "tests/ 豁免 {path} 必须写明不合并的理由"
        );
    }
}
