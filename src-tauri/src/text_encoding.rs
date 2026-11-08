//! 子进程输出流的编码判定（UTF-8 / Windows OEM 代码页）。
//!
//! 背景：中文 Windows 上 cmd 内建命令（`type`、`echo`、各类错误消息）按 **OEM 代码页**
//! （936/GBK）写管道，而 git/python/node 这类外部程序多数写 UTF-8。旧实现用
//! `BufRead::read_line(&mut String)` 解码：遇到 GBK 字节直接得到 `InvalidData`，
//! 整行连同其后的内容一起被丢弃——模型看到的是 `�\u{73e}...` 甚至零输出，
//! 等于读不到中文系统的报错。
//!
//! 判定口径（每条流各一次，不做逐行混合解码）：
//! - 纯 ASCII 在两种编码下字节相同，先放行、不落判定；
//! - 出现第一个非 ASCII 行时，对这一行做**严格 UTF-8** 校验并定下整条流的编码：
//!   通过（或只有末尾不完整的多字节序列）→ UTF-8；不通过 → 系统 OEM 代码页；
//! - 判定一旦落下，后续行一律按同一编码解码。流中途再多出一种编码属于无解输入，
//!   这里选择"保持已判定的编码、非法序列替换为 U+FFFD"，**绝不逐行翻转**；
//! - 末尾被截断的多字节序列：不 panic、不吞后续内容（UTF-8 侧 lossy 收尾，
//!   OEM 侧由 `MultiByteToWideChar` 自己替换）。
//!
//! 最终采用的编码经 [`EncodingLedger`] 回写，由调用方写进工具输出元数据——
//! 不做无痕猜测。

use std::sync::{Arc, Mutex};

/// 一条输出流最终采用的编码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEncoding {
    /// 严格 UTF-8（纯 ASCII 流也算这一类，字节相同）。
    Utf8,
    /// 系统 OEM 代码页（中文 Windows = 936/GBK）。
    Oem(u32),
}

impl StreamEncoding {
    /// 元数据里的短标签。
    pub fn label(self) -> String {
        match self {
            StreamEncoding::Utf8 => "utf-8".to_string(),
            StreamEncoding::Oem(codepage) => format!("oem-{codepage}"),
        }
    }
}

/// 单条输出流的判定器：流内只判定一次。
pub struct StreamDecoder {
    oem_codepage: u32,
    decision: Option<StreamEncoding>,
}

impl StreamDecoder {
    /// 指定 OEM 代码页（测试用固定值，生产用 [`system_oem_codepage`]）。
    pub fn new(oem_codepage: u32) -> Self {
        Self {
            oem_codepage,
            decision: None,
        }
    }

    /// 生产入口：OEM 代码页取当前系统（中文 Windows 为 936）。
    pub fn with_system_oem() -> Self {
        Self::new(system_oem_codepage())
    }

    /// 已落定的编码；`None` = 至今只见过纯 ASCII（两种编码无差别）。
    pub fn decision(&self) -> Option<StreamEncoding> {
        self.decision
    }

    /// 解码一行（不含行尾 `\r\n`）。
    pub fn decode_line(&mut self, line: &[u8]) -> String {
        let encoding = match self.decision {
            Some(encoding) => encoding,
            None => {
                if line.is_ascii() {
                    // ASCII 在 UTF-8 与 OEM 代码页下同字节：先放行，不下判定。
                    return String::from_utf8_lossy(line).into_owned();
                }
                let encoding = classify(line, self.oem_codepage);
                self.decision = Some(encoding);
                encoding
            }
        };
        match encoding {
            StreamEncoding::Utf8 => match std::str::from_utf8(line) {
                Ok(text) => text.to_string(),
                // 判定时已排除非法序列，这里只可能是末尾截断：lossy 用 U+FFFD 收尾。
                Err(_) => String::from_utf8_lossy(line).into_owned(),
            },
            StreamEncoding::Oem(codepage) => decode_codepage(codepage, line),
        }
    }
}

/// 首个非 ASCII 行的判定：严格 UTF-8 通过就是 UTF-8；否则整条流按 OEM 代码页。
fn classify(line: &[u8], oem_codepage: u32) -> StreamEncoding {
    match std::str::from_utf8(line) {
        Ok(_) => StreamEncoding::Utf8,
        // error_len() == None：只有末尾不完整的多字节序列，前缀全部合法。
        Err(error) if error.error_len().is_none() => StreamEncoding::Utf8,
        Err(_) => StreamEncoding::Oem(oem_codepage),
    }
}

/// 读者线程/任务回写、调用方汇总的共享槽（0 = stdout，1 = stderr）。
#[derive(Clone, Default)]
pub struct EncodingLedger {
    slots: Arc<Mutex<[Option<StreamEncoding>; 2]>>,
}

impl EncodingLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录某条流落定的编码（重复调用以最后一次为准）。
    pub fn record(&self, channel: u8, encoding: StreamEncoding) {
        if let Ok(mut slots) = self.slots.lock() {
            if let Some(slot) = slots.get_mut(channel as usize) {
                *slot = Some(encoding);
            }
        }
    }

    /// 工具输出里的元数据行；只见过纯 ASCII 的流不占位，全都没有则返回 `None`。
    pub fn note(&self) -> Option<String> {
        let slots = self.slots.lock().ok()?;
        let parts: Vec<String> = ["stdout", "stderr"]
            .iter()
            .enumerate()
            .filter_map(|(index, name)| {
                slots[index].map(|encoding| match encoding {
                    StreamEncoding::Utf8 => format!("{name}=utf-8"),
                    StreamEncoding::Oem(codepage) => format!(
                        "{name}=oem-{codepage}（非 UTF-8，已按系统 OEM 代码页 {codepage} 还原）"
                    ),
                })
            })
            .collect();
        if parts.is_empty() {
            None
        } else {
            Some(format!("[输出编码] {}", parts.join("；")))
        }
    }
}

/// 当前系统 OEM 代码页（中文 Windows = 936）。
#[cfg(windows)]
pub fn system_oem_codepage() -> u32 {
    unsafe { windows::Win32::Globalization::GetOEMCP() }
}

#[cfg(not(windows))]
pub fn system_oem_codepage() -> u32 {
    65001
}

/// 按指定代码页把整段字节转成 UTF-8 字符串。
///
/// `dwFlags = 0`（不设 `MB_ERR_INVALID_CHARS`）：非法/截断序列由系统替换为 U+FFFD，
/// 不会失败、不会 panic，也不会丢后续字节。
#[cfg(windows)]
pub fn decode_codepage(codepage: u32, bytes: &[u8]) -> String {
    use windows::Win32::Globalization::{MultiByteToWideChar, MULTI_BYTE_TO_WIDE_CHAR_FLAGS};
    if bytes.is_empty() {
        return String::new();
    }
    let flags = MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0);
    unsafe {
        let needed = MultiByteToWideChar(codepage, flags, bytes, None);
        if needed > 0 {
            let mut wide = vec![0u16; needed as usize];
            let written = MultiByteToWideChar(codepage, flags, bytes, Some(&mut wide));
            if written > 0 {
                return String::from_utf16_lossy(&wide[..written as usize]);
            }
        }
    }
    // 代码页本身不可用（理论上不该发生）时退回 lossy：宁可乱码也不丢输出。
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(not(windows))]
pub fn decode_codepage(_codepage: u32, bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实 GBK 字节样本：`拒绝访问。`（中文 Windows 的 cmd 报错原文）。
    /// 不依赖当前系统代码页——码页由测试显式指定。
    const GBK_ACCESS_DENIED: [u8; 10] = [
        0xBE, 0xDC, 0xBE, 0xF8, 0xB7, 0xC3, 0xCE, 0xCA, 0xA1, 0xA3,
    ];

    #[test]
    fn gbk_bytes_decode_with_explicit_codepage() {
        // 前提：这串字节不是合法 UTF-8，否则判定会走错分支。
        // 走 Vec 而不是字面量切片：clippy 的 `invalid_from_utf8` 会把后者当成编译期错误。
        let bytes = GBK_ACCESS_DENIED.to_vec();
        assert!(String::from_utf8(bytes).is_err());

        let mut decoder = StreamDecoder::new(936);
        assert_eq!(decoder.decode_line(&GBK_ACCESS_DENIED), "拒绝访问。");
        assert_eq!(decoder.decision(), Some(StreamEncoding::Oem(936)));
    }

    #[test]
    fn ascii_prefix_and_gbk_body_decode_as_one_stream() {
        let mut raw = b"type missing.txt: ".to_vec();
        raw.extend_from_slice(&GBK_ACCESS_DENIED);
        let mut decoder = StreamDecoder::new(936);
        assert_eq!(
            decoder.decode_line(&raw),
            "type missing.txt: 拒绝访问。"
        );
    }

    #[test]
    fn utf8_stream_stays_utf8() {
        let mut decoder = StreamDecoder::new(936);
        assert_eq!(decoder.decode_line("拒绝访问。".as_bytes()), "拒绝访问。");
        assert_eq!(decoder.decision(), Some(StreamEncoding::Utf8));
    }

    #[test]
    fn ascii_only_stream_makes_no_decision() {
        let mut decoder = StreamDecoder::new(936);
        assert_eq!(decoder.decode_line(b"hello world"), "hello world");
        assert_eq!(decoder.decision(), None);
    }

    /// 判定按整条流做一次：先落定 UTF-8 后，后续 GBK 行不许翻转（也不许逐行混解）。
    #[test]
    fn decision_never_flips_per_line() {
        let mut decoder = StreamDecoder::new(936);
        assert_eq!(decoder.decode_line("中文".as_bytes()), "中文");
        assert_eq!(decoder.decision(), Some(StreamEncoding::Utf8));

        let text = decoder.decode_line(&GBK_ACCESS_DENIED);
        assert_ne!(text, "拒绝访问。");
        assert!(text.contains('\u{FFFD}'));
        assert_eq!(decoder.decision(), Some(StreamEncoding::Utf8));
    }

    /// 截断的多字节序列：不 panic、不吞后续内容。
    #[test]
    fn truncated_multibyte_tail_does_not_swallow_following_lines() {
        let mut decoder = StreamDecoder::new(936);
        // "拒" 的 UTF-8 是 E6 8B 92，这里只给前两字节（模拟进程被杀时写了一半）。
        let text = decoder.decode_line(&[0xE6, 0x8B]);
        assert_eq!(decoder.decision(), Some(StreamEncoding::Utf8));
        assert!(text.contains('\u{FFFD}'));
        assert_eq!(decoder.decode_line("后续内容".as_bytes()), "后续内容");
    }

    /// 截断的 GBK 双字节：同样不 panic，后续行照常解出。
    #[test]
    fn truncated_oem_tail_does_not_swallow_following_lines() {
        let mut decoder = StreamDecoder::new(936);
        let text = decoder.decode_line(&[0xBE]);
        assert_eq!(decoder.decision(), Some(StreamEncoding::Oem(936)));
        assert!(!text.is_empty());
        assert_eq!(decoder.decode_line(&GBK_ACCESS_DENIED), "拒绝访问。");
    }

    #[test]
    fn ledger_reports_only_streams_that_were_decided() {
        let ledger = EncodingLedger::new();
        assert!(ledger.note().is_none());

        ledger.record(0, StreamEncoding::Oem(936));
        ledger.record(1, StreamEncoding::Utf8);
        let note = ledger.note().expect("两条流都落定了判定");
        assert!(note.contains("stdout=oem-936"), "实际: {note}");
        assert!(note.contains("stderr=utf-8"), "实际: {note}");
    }
}
