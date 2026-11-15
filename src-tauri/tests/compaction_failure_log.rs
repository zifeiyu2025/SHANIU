//! 压缩失败的日志守门（R5-5）。
//!
//! 判据按**行为**：三处失败路径（手动 `/compact`、溢出补救、轮前预算）都必须留下
//! 一条可 grep 的 warn。此前失败只在帧面与 rollback 里有痕迹、日志 0 行，事后无法
//! 回答"这次为什么没压成"。
//!
//! 为什么用源码扫描而不是 `tracing` 捕获：三条路径各自需要真实流/档案的失败注入，
//! 捕获器又会随并发测试互相串日志；而这条守门要防的是"有人把日志删了"，扫描最直接。
//! 与 `now_ms_single_source.rs` 同一族做法。

use std::fs;
use std::path::Path;

/// 与成功日志同 tag、不同级别：`grep SH-032` 能同时命中成/败，`grep 上下文压缩失败` 只看失败。
const TAG: &str = "上下文压缩失败（SH-032）";

/// 档案扫描的源码文件（相对 `src-tauri`）。
const SOURCES: &[&str] = &["src/session_compact.rs", "src/agent_loop/mod.rs"];

/// 每条失败路径的关头文案：必须落在某条 `TAG` 日志的附近（`warn!` 可能跨多行）。
const SITES: &[&str] = &["trigger = \"manual\"", "溢出补救失败", "轮前预算压缩失败"];

fn read(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("读不到 {relative}（{e}）——扫描失败必须显式报错，不能假绿"))
}

/// `TAG` 那一行起 6 行内是否出现 `marker`（`tracing::warn!` 的参数常换行书写）。
fn logged_near(lines: &[&str], marker: &str) -> bool {
    lines.iter().enumerate().any(|(index, line)| {
        line.contains(TAG) && lines[index..lines.len().min(index + 7)].iter().any(|near| near.contains(marker))
    })
}

#[test]
fn every_compaction_failure_path_logs_greppable_warn() {
    let mut total = 0;
    for relative in SOURCES {
        let text = read(relative);
        let hits = text.lines().filter(|line| line.contains(TAG)).count();
        assert!(
            hits > 0,
            "{relative} 里没有「{TAG}」：压缩失败必须留一条可 grep 的 warn，别只在前端留痕"
        );
        total += hits;
    }
    assert!(total >= 3, "失败路径至少三条（手动/溢出/轮前），只找到 {total} 条日志");
}

/// 三处失败点必须各自命中，而不是同一个文件里复制三遍同一条。
#[test]
fn failure_logs_cover_all_three_paths() {
    let text = format!("{}\n{}", read(SOURCES[0]), read(SOURCES[1]));
    let lines: Vec<&str> = text.lines().collect();
    for marker in SITES {
        assert!(
            logged_near(&lines, marker),
            "缺 {marker} 的失败日志：每条失败路径都要自己的 warn，不能合并成一条通用日志"
        );
    }
}
