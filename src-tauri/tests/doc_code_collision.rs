//! 文档与代码对撞（缝合机制第一件，`AGENTS.md` §4.4「写了要接上」）。
//!
//! 目的只有一个：文档说「已实现 / 已接线」的地方，代码里必须真的**有调用点**。
//! 只断言「符号存在」是没有分辨力的——`fn on_unexpected_exit` 存在但没人调用，
//! 与「已经接上」在 grep 结果上一模一样；而这一轮已经证明「写了没接上」是最常见的形态
//! （`ProjectRules::selection()` 无人消费、§4 整整一节没进注入）。
//!
//! 口径：
//! - 只扫 `src-tauri/src/` 下的**非测试**代码：`#[cfg(test)] mod tests` 之后整段剥掉，
//!   所以「只在测试里被调用」不算接上。
//! - 每条断言给一个**只在调用点出现的符号**（函数名、构造 `EventMsg::X {`、常量读取），
//!   并排除定义行本身（`fn x(` / `struct x` / `const x` ...），只留下真正的使用处。
//! - 每条都带文档位置与原文断言；文档改了而代码没跟上时，这条用例先红。
//!
//! 本清单**只收已核实成立**的断言。核出来「仅有定义、生产无调用」的不往这里塞——
//! 那属于缺口，进报告，不进绿灯。

use std::path::{Path, PathBuf};

struct Claim {
    /// 文档位置（文件 + 小节/行）。
    doc: &'static str,
    /// 原文断言（压缩到一句，保留主谓）。
    claim: &'static str,
    /// 只在**调用点**出现的符号。
    call_marker: &'static str,
}

/// 已核实的断言清单。加条目之前先手工确认「除测试外至少有一个调用点」。
const CLAIMS: &[Claim] = &[
    Claim {
        doc: "docs/04 §1.1 L28",
        claim: "event_id 由共享单调生成器产生，编号与发送处于同一临界区",
        call_marker: "EVENT_IDS.lock",
    },
    Claim {
        doc: "docs/04 §1.2 L68",
        claim: "子代理事件 subagent_spawned 已实现（生产端发送）",
        call_marker: "EventMsg::SubagentSpawned",
    },
    Claim {
        doc: "docs/04 §1.2 L68",
        claim: "子代理事件 subagent_exited 已实现（生产端发送）",
        call_marker: "EventMsg::SubagentExited",
    },
    Claim {
        doc: "docs/04 §1.2 L68",
        claim: "子代理事件 subagent_message 已实现（生产端发送）",
        call_marker: "EventMsg::SubagentMessage",
    },
    Claim {
        doc: "docs/04 §2.3 L206",
        claim: "被其他条目严格包含的路径丢弃，父目录继承 ACE 已覆盖",
        call_marker: "drop_nested",
    },
    Claim {
        doc: "docs/05 SH-094 批 2 L225",
        claim: "gateway 首帧 hello 鉴权，5s 超时，错则 unauthorized",
        call_marker: "HELLO_TIMEOUT",
    },
    Claim {
        doc: "docs/04 §1.4 L141",
        claim: "loopback WebSocket gateway 服务端已接进组合根",
        call_marker: "gateway::serve",
    },
    Claim {
        doc: "docs/05 SH-094 批 1 L223",
        claim: "广播出口逐条追加 AgentEvent 到 events.jsonl",
        call_marker: "journal.append",
    },
    Claim {
        doc: "docs/05 SH-094 批 1 L223",
        claim: "read_session_events 按 after_event_id 游标分页读回",
        call_marker: "event_journal::read_after",
    },
    Claim {
        doc: "docs/05 SH-121 L790",
        claim: "仅当本轮真的派发了工具调用才继续（calls.is_empty() → 结束）",
        call_marker: "calls.is_empty()",
    },
    Claim {
        doc: "docs/05 SH-054 L431",
        claim: "allowed_tools 参数已放行并参与权限合成",
        call_marker: "allowed_tools.as_ref()",
    },
    Claim {
        doc: "docs/10 L394 / docs/12 §9 L121",
        claim: "模型请求重试已实现：RetryPolicy 在循环里构造",
        call_marker: "RetryPolicy::new",
    },
    Claim {
        doc: "docs/10 L394",
        claim: "重试过程发 model_retry 事件",
        call_marker: "EventMsg::ModelRetry",
    },
    Claim {
        doc: "docs/12 §9 L128",
        claim: "PROMPT_VERSION + 全文指纹进日志",
        call_marker: "prompt_version = PROMPT_VERSION",
    },
    Claim {
        doc: "docs/02 M6 L257",
        claim: "崩溃重启退避 1s 起、指数增长、上限 8s",
        call_marker: "RESTART_BACKOFF_BASE",
    },
    Claim {
        doc: "docs/02 M6 L257",
        claim: "重启额度用尽 → degraded，工具移除",
        call_marker: "RestartDecision::Degraded",
    },
    Claim {
        doc: "docs/05 SH-039 L84",
        claim: "会话标题落 titles.json 侧车，原子替换",
        call_marker: "TITLES_FILE",
    },
];

/// 剥掉 `#[cfg(test)] mod tests { ... }` 之后的内容。
///
/// 只认「模块级」的 `#[cfg(test)]`：`gateway.rs` 里有单条 `#[cfg(test)] fn`，
/// 按第一个标记截断会把后面的生产代码（包括 `serve`）一起丢掉，得到假红。
fn strip_test_modules(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    loop {
        let Some(pos) = rest.find("#[cfg(test)]") else {
            out.push_str(rest);
            break;
        };
        let after = &rest[pos + "#[cfg(test)]".len()..];
        let tail = after.trim_start();
        if tail.starts_with("mod tests") || tail.starts_with("pub mod tests") {
            out.push_str(&rest[..pos]);
            break;
        }
        out.push_str(&rest[..pos + "#[cfg(test)]".len()]);
        rest = after;
    }
    out
}

/// 这一行只是**定义**（`fn x(` / `struct x` / `const x` ...），不是调用点。
fn is_definition_line(line: &str, marker: &str) -> bool {
    let trimmed = line.trim();
    let Some(index) = trimmed.find(marker) else {
        return false;
    };
    let before = trimmed[..index].trim_end();
    ["fn", "struct", "enum", "trait", "const", "static", "type", "mod"]
        .iter()
        .any(|kind| before.ends_with(kind))
        || before.ends_with("fn")
}

fn is_comment(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("//") || trimmed.starts_with('*')
}

/// 返回真正使用该符号的行（排除注释行与定义行）。
fn call_sites(src: &str, marker: &str) -> Vec<String> {
    src.lines()
        .filter(|line| line.contains(marker))
        .filter(|line| !is_comment(line))
        .filter(|line| !is_definition_line(line, marker))
        .map(|line| line.trim().to_string())
        .collect()
}

fn collect_rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rust_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

fn production_corpus() -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rust_sources(&root, &mut files);
    assert!(files.len() > 20, "src/ 下只找到 {} 个 .rs，路径或工作目录不对", files.len());
    files
        .iter()
        .map(|path| {
            let text = std::fs::read_to_string(path).expect("读源码");
            strip_test_modules(&text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn documented_features_have_a_production_call_site() {
    let corpus = production_corpus();
    let mut missing = Vec::new();
    for claim in CLAIMS {
        if call_sites(&corpus, claim.call_marker).is_empty() {
            missing.push(format!(
                "- {}：断言「{}」找不到调用点（符号 `{}`），只有定义或只被测试用到",
                claim.doc, claim.claim, claim.call_marker
            ));
        }
    }
    assert!(
        missing.is_empty(),
        "文档声称已接线、代码里却没有生产调用点（要么补调用点，要么把文档改成「未接线」）：\n{}",
        missing.join("\n")
    );
    println!("[对撞] {} 条断言全部有生产调用点", CLAIMS.len());
}

/// 反向对照：证明上面那套判定**不是永真**。
///
/// 没有这条，`call_sites` 写错（比如把定义行也算进去、或者忘了剥测试模块）时，
/// 正向用例照样全绿——那正是本轮反复抓到的「假绿」形态。
#[test]
fn detector_rejects_definition_only_and_test_only_callers() {
    // 只有定义 → 0 个调用点。
    let definition_only = "pub fn probe_symbol(x: u32) -> u32 { x }\n";
    assert!(
        call_sites(definition_only, "probe_symbol").is_empty(),
        "定义行被当成了调用点，判定失去分辨力"
    );

    // 定义 + 真实调用 → 恰好 1 个调用点。
    let with_caller = "pub fn probe_symbol(x: u32) -> u32 { x }\nfn main() { let _ = probe_symbol(1); }\n";
    assert_eq!(call_sites(with_caller, "probe_symbol").len(), 1);

    // 只在测试模块里调用 → 剥掉后为 0。
    let test_only = "fn main() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() { probe_symbol(1); }\n}\n";
    assert!(
        call_sites(&strip_test_modules(test_only), "probe_symbol").is_empty(),
        "测试里的调用被算成了生产调用点"
    );

    // 单条 `#[cfg(test)] fn` 不能把后面的生产代码一起剥掉。
    let mixed = "#[cfg(test)] fn helper() {}\npub fn serve() {}\nfn main() { serve(); }\n";
    assert_eq!(call_sites(&strip_test_modules(mixed), "serve()").len(), 1);
}
