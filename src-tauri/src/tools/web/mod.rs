//! 网络工具共享层：HTTP 客户端、目标校验、不可信内容包裹。
//!
//! `web_search` 与 `web_fetch` 共用同一套校验与客户端，**不允许**某个工具
//! 为了方便（例如"搜索页是固定域名"）绕过 [`client::WebClient`]。

pub mod client;
pub mod fetch;
pub mod guard;
pub mod search;
#[cfg(test)]
pub(crate) mod test_server;

pub use client::{WebClient, WebError, WebResponse};
pub use fetch::WebFetch;
pub use search::{
    Bing, FallbackProvider, SearchError, SearchFailure, SearchHit, SearchProvider, WebSearch,
};

/// 生成一次性分隔符随机数（16 字节 hex）。
///
/// 与 MCP 输出包裹同一思路：分隔符由本进程生成，外部网页无法预知，
/// 因此无法伪造结束标记来"逃出"不可信区间。
pub fn new_nonce() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("系统熵源不可用，拒绝生成可预测的网页输出分隔符");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 把外部网页内容包进不可信区间。`label` 用于区分来源（WEB_SEARCH / WEB_FETCH）。
pub fn wrap_untrusted(label: &str, nonce: &str, body: &str) -> String {
    format!("<<<{label}:{nonce}>>>\n{body}\nEND:{nonce}>>>")
}

/// 按字符数截断并加省略标记（不能用字节切，中文会切坏）。
pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// 解码常见 HTML 实体（含数字实体）。未知实体原样保留。
///
/// 实现上按**字节**找单个 `;`，再切字符串——`&` 后面跟中文时，
/// 按字符窗口切会落在多字节字符中间并 panic（真机回归过，见单元测试）。
pub fn decode_entities(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        // 分号是单字节，按字节窗口找即可；`;` 的位置一定是合法 char 边界。
        let window = &tail.as_bytes()[..tail.len().min(12)];
        let Some(byte_index) = window.iter().position(|byte| *byte == b';') else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let semicolon = byte_index;
        let entity = &tail[1..semicolon];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            "hellip" => Some('…'),
            "mdash" => Some('—'),
            "ndash" => Some('–'),
            "rsquo" => Some('\u{2019}'),
            "lsquo" => Some('\u{2018}'),
            "ldquo" => Some('\u{201c}'),
            "rdquo" => Some('\u{201d}'),
            other if other.starts_with("#x") || other.starts_with("#X") => {
                u32::from_str_radix(&other[2..], 16).ok().and_then(char::from_u32)
            }
            other if other.starts_with('#') => {
                other[1..].parse::<u32>().ok().and_then(char::from_u32)
            }
            _ => None,
        };
        match decoded {
            Some(character) => {
                out.push(character);
                rest = &tail[semicolon + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_is_hex_and_unique() {
        let first = new_nonce();
        let second = new_nonce();
        assert_eq!(first.len(), 32);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    #[test]
    fn wrap_puts_body_between_markers() {
        let wrapped = wrap_untrusted("WEB_FETCH", "abc", "正文");
        assert_eq!(wrapped, "<<<WEB_FETCH:abc>>>\n正文\nEND:abc>>>");
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate_chars("你好世界", 2), "你好…");
        assert_eq!(truncate_chars("abc", 5), "abc");
    }
}
