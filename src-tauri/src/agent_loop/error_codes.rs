//! 用户可见错误码的受控词表（docs/04 §6.3 不变量①）。
//!
//! wire 上 `TurnErrorInfo.code` 仍是 `String`，协议不变；但取值必须来自本表。
//! 新增 emit 点先在这里登记，`codes_emitted_in_src_are_registered` 会扫描
//! `src/**/*.rs` 里的 `code: "..."` 字面量：出现词表外的字符串即红。
//!
//! 只登记**本仓库**会 emit 的码。上游 dsh 的前端专用名（如
//! `request_user_input`）不在此列，也不应照抄。

/// 模型端点错误（`ModelError::code`，见 `model_client/error.rs`）。
pub mod model {
    pub const AUTH: &str = "AUTH";
    pub const QUOTA: &str = "QUOTA";
    pub const RATE_LIMIT: &str = "RATE_LIMIT";
    pub const TIMEOUT: &str = "TIMEOUT";
    pub const NETWORK: &str = "NETWORK";
    pub const SERVER: &str = "SERVER";
    pub const ENDPOINT_UNSUPPORTED: &str = "ENDPOINT_UNSUPPORTED";
    pub const BAD_REQUEST: &str = "BAD_REQUEST";
    pub const EMPTY_RESPONSE: &str = "EMPTY_RESPONSE";
    /// 上下文超出模型窗口（确定性失败）。
    pub const CONTEXT_OVERFLOW: &str = "CONTEXT_OVERFLOW";
    /// 请求/回答被内容安全策略拦截（确定性失败）。
    pub const CONTENT_FILTERED: &str = "CONTENT_FILTERED";
    /// 端点明确表示回答未完成（`response.incomplete`）。
    pub const INCOMPLETE: &str = "INCOMPLETE";
    pub const UNKNOWN: &str = "UNKNOWN";
}

/// 本地失败（不来自模型响应，`agent_loop` 直接判定）。
pub mod local {
    /// 上下文压缩流失败；原历史保持不变。
    pub const COMPACTION_FAILED: &str = "COMPACTION_FAILED";
    /// 会话引用展开失败（读取被引用会话）。
    pub const CONTEXT_EXPAND_FAILED: &str = "CONTEXT_EXPAND_FAILED";
    /// 自愈过程改动了测试断言，修复被拒绝。
    pub const TEST_TAMPERING: &str = "TEST_TAMPERING";
    /// 收口验证未通过且已无重试轮次。
    pub const VERIFY_FAILED: &str = "VERIFY_FAILED";
}

/// 工作区文件读取失败（`workspace_files.rs`）。
pub mod workspace_file {
    pub const TOO_LARGE: &str = "workspace-file/too-large";
    pub const NOT_REGULAR_FILE: &str = "workspace-file/not-regular-file";
    pub const NOT_DIRECTORY: &str = "workspace-file/not-directory";
}

/// 任务提交失败（`SendTaskError`）。
pub mod task {
    pub const NO_VISION: &str = "MODEL_NO_VISION";
}

/// 全部受控码。
pub const ALL: &[&str] = &[
    model::AUTH,
    model::QUOTA,
    model::RATE_LIMIT,
    model::TIMEOUT,
    model::NETWORK,
    model::SERVER,
    model::ENDPOINT_UNSUPPORTED,
    model::BAD_REQUEST,
    model::EMPTY_RESPONSE,
    model::CONTEXT_OVERFLOW,
    model::CONTENT_FILTERED,
    model::INCOMPLETE,
    model::UNKNOWN,
    local::COMPACTION_FAILED,
    local::CONTEXT_EXPAND_FAILED,
    local::TEST_TAMPERING,
    local::VERIFY_FAILED,
    workspace_file::TOO_LARGE,
    workspace_file::NOT_REGULAR_FILE,
    workspace_file::NOT_DIRECTORY,
    task::NO_VISION,
];

/// `code` 是否在受控词表内。词表外的字符串不得作为错误码 emit。
pub fn is_known(code: &str) -> bool {
    ALL.contains(&code)
}

#[cfg(test)]
mod tests {
    use super::{is_known, ALL};
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    /// 第二种 emit 形态：`ModelError::code()` 的 `=> "AUTH"` 分支。
    /// 只扫该函数体，避免把同文件里别的字符串误当错误码。
    fn code_fn_literals() -> Vec<(PathBuf, usize, String)> {
        let file = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("model_client")
            .join("error.rs");
        let Ok(text) = std::fs::read_to_string(&file) else { return Vec::new() };
        let mut found = Vec::new();
        let mut inside = false;
        for (index, line) in text.lines().enumerate() {
            if line.contains("pub fn code(&self)") {
                inside = true;
                continue;
            }
            if !inside {
                continue;
            }
            if line.trim() == "}" {
                break;
            }
            let Some(pos) = line.find("=> \"") else { continue };
            let after = &line[pos + "=> \"".len()..];
            if let Some(end) = after.find('"') {
                found.push((file.clone(), index + 1, after[..end].to_string()));
            }
        }
        found
    }

    /// 扫描生产源码里的 `code: "..."` 字面量（排除 `provider_code` 一类别名），
    /// 加上 `ModelError::code()` 的 match 分支。
    fn emitted_codes() -> Vec<(PathBuf, usize, String)> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        collect_rs(&root, &mut files);
        let mut found = Vec::new();
        for file in files {
            // 词表自身只有常量与文档注释，不是 emit 点。
            if file.file_name().is_some_and(|name| name == "error_codes.rs") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&file) else { continue };
            for (index, line) in text.lines().enumerate() {
                // 注释里的 `code: "..."` 是说明文字，不是 emit。
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") || trimmed.starts_with('*') {
                    continue;
                }
                let mut rest = line;
                while let Some(pos) = rest.find("code: \"") {
                    let boundary_ok = rest[..pos]
                        .chars()
                        .last()
                        .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
                    let after = &rest[pos + "code: \"".len()..];
                    if let Some(end) = after.find('"') {
                        let code = &after[..end];
                        // `format!` 之类的动态拼接交给词表外的构造点自证，不在本扫描范围。
                        if boundary_ok && !code.is_empty() && !code.contains('{') {
                            found.push((file.clone(), index + 1, code.to_string()));
                        }
                    }
                    rest = after;
                }
            }
        }
        found.extend(code_fn_literals());
        found
    }

    /// 反向对照：把 `is_known` 改成恒真，下一个用例必红。
    #[test]
    fn unknown_code_is_rejected() {
        assert!(is_known(super::model::AUTH));
        assert!(!is_known("NOT_A_REGISTERED_CODE"));
    }

    /// 不变量①的承载物：凡被 emit 的 code 都必须在词表内。
    #[test]
    fn codes_emitted_in_src_are_registered() {
        let found = emitted_codes();
        assert!(
            found.len() >= 10,
            "扫描失效或源码骤减：只找到 {} 处 code 字面量，先确认扫描规则没被绕过",
            found.len()
        );
        let unknown: Vec<String> = found
            .iter()
            .filter(|(_, _, code)| !is_known(code))
            .map(|(file, line, code)| {
                format!("{}:{} -> {code}", file.display(), line)
            })
            .collect();
        assert!(
            unknown.is_empty(),
            "以下错误码未登记进 agent_loop/error_codes.rs：\n{}",
            unknown.join("\n")
        );
    }

    /// 词表自身不许有重复或空项，否则「登记」会退化成摆设。
    #[test]
    fn vocabulary_has_no_duplicates() {
        let unique: BTreeSet<&str> = ALL.iter().copied().collect();
        assert_eq!(unique.len(), ALL.len(), "ALL 存在重复项");
        assert!(ALL.iter().all(|code| !code.is_empty()), "ALL 存在空项");
    }
}
