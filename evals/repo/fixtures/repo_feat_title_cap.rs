//! 评测判据：标题归一化必须按字符截断到上限并补省略号。
//!
//! 这是评测注入文件（不是业务代码）：agent 不许修改它，改动会被判据守卫拦下。

use shaniu_lib::session_title::normalize_title_text;

const MAX: usize = 40;

#[test]
fn long_title_is_capped_by_chars_not_bytes() {
    let raw = "会".repeat(80);
    let title = normalize_title_text(&raw);
    assert_eq!(title.chars().count(), MAX + 1, "应为 {MAX} 字符 + 省略号，实际: {title}");
    assert!(title.ends_with('…'), "截断后应补省略号，实际: {title}");
    assert_eq!(title.chars().take(MAX).collect::<String>(), "会".repeat(MAX));
}

#[test]
fn exactly_max_is_untouched() {
    let raw = "a".repeat(MAX);
    assert_eq!(normalize_title_text(&raw), raw);
}

#[test]
fn existing_normalization_still_applies() {
    assert_eq!(normalize_title_text("  a \n b  "), "a b");
    assert_eq!(normalize_title_text("\u{1b}[31m红色\u{1b}[0m"), "红色");
}
