//! R3 反向对照：`now_ms` 只允许有一个实现，且落在 `src/clock.rs`（docs/01 §2.3.1 E63）。
//!
//! 收敛前有九个模块各写一份逐字同义的 `now_ms()`，谁都能再抄一份。这条用例把
//! 「只有一份」变成可执行的判据：往 `src/` 里贴回第二份、或把唯一的出口删掉，都会红。
//!
//! 探针字符串在运行时拼接，避免用例自己命中自己（本文件就在 `tests/` 下）。

use std::fs;
use std::path::{Path, PathBuf};

/// `tests/` 下允许存在的同名实现，必须在此登记并写明理由。
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

/// 文件里是否有一行**代码**（非行注释）定义了 `now_ms`。
fn defines_now_ms(path: &Path) -> bool {
    // 拼成 "fn now_ms("：分开写是为了让本文件自己不含这串字面量（否则会命中自己）。
    let needle = ["fn", "now_ms("].join(" ");
    let Ok(text) = fs::read_to_string(path) else { return false };
    text.lines().any(|line| {
        let trimmed = line.trim_start();
        !trimmed.starts_with("//") && trimmed.contains(&needle)
    })
}

/// 扫描目录，返回定义 `now_ms` 的文件（相对仓库内的 `src-tauri` 根，正斜杠）。
fn files_defining(dir: &Path, manifest_dir: &Path) -> Vec<String> {
    let mut all = Vec::new();
    collect_rs(dir, &mut all);
    let mut found: Vec<String> = all
        .into_iter()
        .filter(|path| defines_now_ms(path))
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

/// 扫描本身不能是空的：目录读不到或扩展名过滤写错时，上面两条断言会假绿。
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
fn src_has_exactly_one_now_ms() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let found = files_defining(&manifest_dir.join("src"), manifest_dir);
    // 改坏方式示例：在任一模块里贴回 `fn now_ms() -> u64 { ... }`，这里立刻多一项。
    assert_eq!(
        found,
        vec!["src/clock.rs".to_string()],
        "src/ 下 now_ms 必须只有 src/clock.rs 一处实现，实际：{found:?}"
    );
}

#[test]
fn tests_only_have_registered_now_ms() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let found = files_defining(&manifest_dir.join("tests"), manifest_dir);
    let mut expected: Vec<String> = TESTS_EXEMPT.iter().map(|(path, _)| (*path).to_string()).collect();
    expected.sort();
    // 双向断言：既不许新增私货，也不许为了让用例变绿把豁免那份删掉/合并掉。
    assert_eq!(
        found, expected,
        "tests/ 下的 now_ms 必须与豁免登记逐一对应，实际：{found:?}"
    );
    for (path, reason) in TESTS_EXEMPT {
        assert!(
            !reason.trim().is_empty(),
            "tests/ 豁免 {path} 必须写明不合并的理由"
        );
    }
}
