//! `@file` 引用候选（SH-090 批 1，上游 `file-reference-local` 的 Rust 等价）。
//!
//! **只做路径候选**：上游的 `@path` 就是路径本身，模型自己用 read/list 工具取材；宿主不把
//! 文件内容塞进提示词（`FILE_REFERENCE_PROMPT` 是系统提示里的一节，另行接入）。排序照上游
//! `search.ts`：精确名 > 名字前缀 > 名字包含 > 路径包含 > 子序列；目录在前；同分按路径长度
//! 与字典序。隐藏段（`.` 开头）只在查询显式以 `.` 开头或含 `/.` 时出现。
//!
//! 作用域与会话读面同源：根 = 会话档案首行固化的 `workspace_root`（见 `workspace_files`），
//! 客户端交不出目录。遍历不跟随符号链接目录，避免环。
use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::Manager;

use crate::app_state::AppState;

/// 单次查询返回上限（上游 `DEFAULT_FILE_SEARCH_MAX_RESULTS`）。
pub const MAX_RESULTS: usize = 20;
/// 单次遍历保留上限（上游 `DEFAULT_FILE_SEARCH_MAX_ENTRIES`）。
pub const MAX_ENTRIES: usize = 50_000;
/// 遍历时整体跳过的目录名（上游默认清单逐项一致）。
pub const EXCLUDED_DIRECTORIES: &[&str] = &[
    ".git", "node_modules", "dist", "build", "out", "coverage", "target", ".next", ".nuxt",
    ".turbo", ".venv", "__pycache__", ".pytest_cache", ".mypy_cache", ".gradle",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileReferenceCandidate {
    /// 相对工作区根的 `/` 分隔路径。
    pub path: String,
    /// `file` 或 `directory`。
    pub kind: &'static str,
}

fn is_hidden_segment(path: &str) -> bool {
    path.split('/').any(|segment| segment.starts_with('.'))
}

fn visible(path: &str, query: &str) -> bool {
    if query.starts_with('.') || query.contains("/.") {
        return true;
    }
    !is_hidden_segment(path)
}

fn subsequence_score(target: &str, needle: &str) -> Option<i32> {
    let mut index = 0usize;
    let mut gap = 0i32;
    for character in needle.chars() {
        let rest = &target[index..];
        let found = rest.find(character)?;
        gap += found as i32;
        index += found + character.len_utf8();
    }
    Some(100 - gap)
}

fn score(candidate: &FileReferenceCandidate, query: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let path = candidate.path.to_lowercase();
    let name = path.rsplit('/').next().unwrap_or(&path);
    let needle = query.to_lowercase();
    let bonus = if candidate.kind == "directory" { 25 } else { 0 };
    if name == needle {
        return Some(1_000 + bonus);
    }
    if name.starts_with(&needle) {
        return Some(900 + bonus);
    }
    if name.contains(&needle) {
        return Some(700 + bonus);
    }
    if path.contains(&needle) {
        return Some(500 + bonus);
    }
    subsequence_score(&path, &needle).map(|sub| 300 + sub + bonus)
}

fn rank(mut candidates: Vec<FileReferenceCandidate>, query: &str) -> Vec<FileReferenceCandidate> {
    candidates.retain(|candidate| visible(&candidate.path, query));
    let mut scored: Vec<(i32, FileReferenceCandidate)> = candidates
        .into_iter()
        .filter_map(|candidate| score(&candidate, query).map(|score| (score, candidate)))
        .collect();
    scored.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .cmp(left_score)
            .then_with(|| kind_rank(left.kind).cmp(&kind_rank(right.kind)))
            .then_with(|| {
                if query.is_empty() {
                    std::cmp::Ordering::Equal
                } else {
                    left.path.len().cmp(&right.path.len())
                }
            })
            .then_with(|| left.path.cmp(&right.path))
    });
    scored.into_iter().take(MAX_RESULTS).map(|(_, candidate)| candidate).collect()
}

fn kind_rank(kind: &str) -> u8 {
    if kind == "directory" { 0 } else { 1 }
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|part| part.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("/")
}

fn collect(root: &Path, query: &str) -> Vec<FileReferenceCandidate> {
    let mut out: Vec<FileReferenceCandidate> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        if out.len() >= MAX_ENTRIES {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&directory) else { continue };
        let mut items: Vec<_> = entries.flatten().collect();
        items.sort_by_key(|entry| entry.file_name());
        for entry in items {
            if out.len() >= MAX_ENTRIES {
                break;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(file_type) = entry.file_type().ok() else { continue };
            let is_directory = file_type.is_dir();
            if is_directory && EXCLUDED_DIRECTORIES.iter().any(|excluded| *excluded == name) {
                continue;
            }
            // 符号链接目录不入栈：目录环会让遍历永不收敛；链接本身仍作为候选出现。
            let path = entry.path();
            out.push(FileReferenceCandidate {
                path: relative_path(root, &path),
                kind: if is_directory { "directory" } else { "file" },
            });
            if is_directory && !file_type.is_symlink() {
                stack.push(path);
            }
        }
    }
    rank(out, query)
}

/// 列出一个会话工作区里与 `query` 匹配的文件/目录候选。
#[tauri::command]
pub fn list_file_references(
    app: tauri::AppHandle,
    session_id: String,
    query: String,
) -> Result<Vec<FileReferenceCandidate>, String> {
    let state = app.state::<AppState>();
    let root = crate::workspace_files::session_root(&state, &session_id);
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    Ok(collect(&root, query.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Temp {
        root: PathBuf,
    }

    impl Temp {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "shaniu-refs-{}-{}",
                std::process::id(),
                ulid::Ulid::generate()
            ));
            std::fs::create_dir_all(&root).expect("临时根可建");
            Self { root }
        }
        fn file(&self, name: &str) {
            let path = self.root.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("父目录可建");
            }
            std::fs::write(path, "x").expect("文件可写");
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).ok();
        }
    }

    #[test]
    fn ranking_matches_the_upstream_tiers() {
        let temp = Temp::new();
        temp.file("src/main.ts");
        temp.file("src/main.test.ts");
        temp.file("docs/main-guide.md");
        let results = collect(&temp.root, "main");
        let names: Vec<&str> = results.iter().map(|entry| entry.path.as_str()).collect();
        // 名字完全等于 main 的没有；名字前缀 main.* 的两条按路径长度排，再是路径包含档。
        assert_eq!(names[0], "src/main.ts");
        assert!(names.contains(&"src/main.test.ts"));
        assert!(names.contains(&"docs/main-guide.md"));
        assert!(names.iter().position(|name| *name == "src/main.ts")
            < names.iter().position(|name| *name == "docs/main-guide.md"));
    }

    #[test]
    fn excluded_directories_and_hidden_segments_follow_the_upstream_rules() {
        let temp = Temp::new();
        temp.file("node_modules/pkg/index.js");
        temp.file("src/.cache/hidden.txt");
        temp.file("src/visible.txt");
        let results = collect(&temp.root, "");
        let names: Vec<&str> = results.iter().map(|entry| entry.path.as_str()).collect();
        assert!(!names.contains(&"node_modules/pkg/index.js"));
        assert!(!names.contains(&"src/.cache/hidden.txt"));
        assert!(names.contains(&"src/visible.txt"));
        let dotted = collect(&temp.root, ".cache");
        assert!(dotted.iter().any(|entry| entry.path == "src/.cache"));
        let excluded_dir = collect(&temp.root, "node_modules");
        assert!(excluded_dir.is_empty(), "排除目录是硬规则，查询也不能召回");
    }

    #[test]
    fn directories_rank_ahead_on_equal_scores_and_results_are_capped() {
        let temp = Temp::new();
        std::fs::create_dir_all(temp.root.join("abc")).expect("目录可建");
        temp.file("abc.txt");
        for index in 0..30 {
            temp.file(&format!("hit{index}.txt"));
        }
        let results = collect(&temp.root, "abc");
        assert_eq!(results[0].kind, "directory");
        let hits = collect(&temp.root, "hit");
        assert_eq!(hits.len(), MAX_RESULTS);
    }
}