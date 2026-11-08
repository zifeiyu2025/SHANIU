//! 子进程输出流的编码判定（UTF-8 / Windows OEM 代码页）。
//!
//! 背景：中文 Windows 上 cmd 内建命令（`type`、`echo`、各类错误消息）按 **OEM 代码页**
//! （936/GBK）写管道，而 git/python/node 这类外部程序多数写 UTF-8。旧实现用
//! `BufRead::read_line(&mut String)` 解码：遇到 GBK 字节直接得到 `InvalidData`，
//! 整行连同其后的内容一起被丢弃——模型看到的是 `�\u{73e}...` 甚至零输出，
//! 等于读不到中文系统的报错。
//!
//! 判定口径（每条流各一次，不做逐行混合解码）：
//! - 纯 ASCII 在两种编码下字节相同，直接放行、不落判定；
//! - 首个非 ASCII 行严格 UTF-8 通过时**只算暂定**：原始行先扣在手里，继续累计
//!   非 ASCII 字节；累计到 [`DECISION_SAMPLE_BYTES`] 仍合法才转正为 UTF-8。
//!   期间任一行严格 UTF-8 失败 → 整条流翻转为 OEM 代码页，扣住的原始行**整段重解**；
//! - 首个非 ASCII 行就不合法 → 立刻按系统 OEM 代码页，后续行不再改判；
//! - 判定一旦转正，后续行一律按同一编码解码。流中途再多出另一种编码属于无解输入，
//!   这里选择"保持已判定的编码、非法序列替换为 U+FFFD"，**绝不逐行翻转**；
//! - 末尾被截断的多字节序列：不 panic、不吞后续内容（UTF-8 侧 lossy 收尾，
//!   OEM 侧由 `MultiByteToWideChar` 自己替换）。
//!
//! 已知边界（有意保留，不要当成无懈可击）：GBK 双字节偶尔也能拼出合法 UTF-8——
//! 例如「一」的 GBK 是 `D2 BB`，恰是合法 UTF-8 的 U+04BB。单行巧合由上面的
//! 累计样本兜住；只有"整条流 ≥256 个非 ASCII 字节全是这种巧合"才会误判成 UTF-8。
//! 真遇到时输出会是乱码，但不会 panic、不会丢内容。
//!
//! 最终采用的编码经 [`EncodingLedger`] 回写。**只有回退到 OEM 代码页时才**把
//! 编码写进工具输出元数据——UTF-8/ASCII 是绝大多数情况，不占位、不加噪。

use std::sync::{Arc, Mutex};

/// 暂定 UTF-8 转正所需的累计非 ASCII 字节数。样本越大，GBK 巧合越难整体骗过。
const DECISION_SAMPLE_BYTES: usize = 256;

/// 暂定期最多扣住多少原始字节；超过就按 UTF-8 转正并全部放行（防无界缓冲）。
const PENDING_LIMIT_BYTES: usize = 64 * 1024;

/// 一条输出流最终采用的编码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEncoding {
    /// 严格 UTF-8。
    Utf8,
    /// 系统 OEM 代码页（中文 Windows = 936/GBK）。
    Oem(u32),
}

/// 单条输出流的判定器：流内只判定一次。
pub struct StreamDecoder {
    oem_codepage: u32,
    state: State,
}

enum State {
    /// 至今只见过纯 ASCII（两种编码无差别），尚未判定。
    Ascii,
    /// 暂定 UTF-8：原始行先扣住，等样本攒够或出现解码失败再定。
    ProvisionalUtf8 {
        pending: Vec<Vec<u8>>,
        pending_bytes: usize,
        non_ascii: usize,
    },
    /// 已转正：UTF-8。
    Utf8,
    /// 已转正：OEM 代码页。
    Oem(u32),
}

impl StreamDecoder {
    /// 指定 OEM 代码页（测试用固定值，生产用 [`system_oem_codepage`]）。
    pub fn new(oem_codepage: u32) -> Self {
        Self {
            oem_codepage,
            state: State::Ascii,
        }
    }

    /// 生产入口：OEM 代码页取当前系统（中文 Windows 为 936）。
    pub fn with_system_oem() -> Self {
        Self::new(system_oem_codepage())
    }

    /// 已落定的编码；`None` = 至今只见过纯 ASCII。
    /// 暂定 UTF-8 也报 `Utf8`（此刻的输出确实是按 UTF-8 解的）。
    pub fn decision(&self) -> Option<StreamEncoding> {
        match self.state {
            State::Ascii => None,
            State::ProvisionalUtf8 { .. } | State::Utf8 => Some(StreamEncoding::Utf8),
            State::Oem(codepage) => Some(StreamEncoding::Oem(codepage)),
        }
    }

    /// 输入一行（不含行尾 `\r\n`），返回**此刻可以放行**的解码结果。
    ///
    /// 暂定期返回空：那些行会被扣住，等判定转正或翻转后按最终编码一次性放行。
    pub fn push_line(&mut self, line: &[u8]) -> Vec<String> {
        match std::mem::replace(&mut self.state, State::Ascii) {
            State::Ascii => {
                if line.is_ascii() {
                    // ASCII 在两种编码下同字节：直接放行，不落判定。
                    self.state = State::Ascii;
                    return vec![ascii_text(line)];
                }
                if utf8_ok_or_truncated(line) {
                    let non_ascii = count_non_ascii(line);
                    if non_ascii >= DECISION_SAMPLE_BYTES {
                        self.state = State::Utf8;
                        return vec![decode_utf8_line(line)];
                    }
                    self.state = State::ProvisionalUtf8 {
                        pending: vec![line.to_vec()],
                        pending_bytes: line.len(),
                        non_ascii,
                    };
                    Vec::new()
                } else {
                    self.state = State::Oem(self.oem_codepage);
                    vec![decode_codepage(self.oem_codepage, line)]
                }
            }
            State::ProvisionalUtf8 {
                mut pending,
                mut pending_bytes,
                mut non_ascii,
            } => {
                if !utf8_ok_or_truncated(line) {
                    // 判定翻转：扣住的原始行整段按 OEM 重解——绝不逐行混合。
                    self.state = State::Oem(self.oem_codepage);
                    let mut out: Vec<String> = pending
                        .iter()
                        .map(|buffered| decode_codepage(self.oem_codepage, buffered))
                        .collect();
                    out.push(decode_codepage(self.oem_codepage, line));
                    return out;
                }
                pending.push(line.to_vec());
                pending_bytes += line.len();
                non_ascii += count_non_ascii(line);
                if non_ascii >= DECISION_SAMPLE_BYTES || pending_bytes >= PENDING_LIMIT_BYTES {
                    self.state = State::Utf8;
                    return pending.iter().map(|buffered| decode_utf8_line(buffered)).collect();
                }
                self.state = State::ProvisionalUtf8 {
                    pending,
                    pending_bytes,
                    non_ascii,
                };
                Vec::new()
            }
            State::Utf8 => {
                self.state = State::Utf8;
                vec![decode_utf8_line(line)]
            }
            State::Oem(codepage) => {
                self.state = State::Oem(codepage);
                vec![decode_codepage(codepage, line)]
            }
        }
    }

    /// 流结束：把暂定期扣住的行按暂定编码放行。
    pub fn finish(&mut self) -> Vec<String> {
        match std::mem::replace(&mut self.state, State::Ascii) {
            State::ProvisionalUtf8 { pending, .. } => {
                // EOF 仍没攒够样本：按 UTF-8 收尾（尾部截断由 lossy 兜住）。
                self.state = State::Utf8;
                pending.iter().map(|buffered| decode_utf8_line(buffered)).collect()
            }
            other => {
                self.state = other;
                Vec::new()
            }
        }
    }
}

/// 严格 UTF-8，或只有末尾不完整的多字节序列（等后续字节再判）。
fn utf8_ok_or_truncated(line: &[u8]) -> bool {
    match std::str::from_utf8(line) {
        Ok(_) => true,
        Err(error) => error.error_len().is_none(),
    }
}

fn count_non_ascii(line: &[u8]) -> usize {
    line.iter().filter(|byte| **byte >= 0x80).count()
}

fn ascii_text(line: &[u8]) -> String {
    // ASCII 字节一定是合法 UTF-8。
    String::from_utf8_lossy(line).into_owned()
}

fn decode_utf8_line(line: &[u8]) -> String {
    match std::str::from_utf8(line) {
        Ok(text) => text.to_string(),
        // 判定已转正 UTF-8，这里只可能是末尾截断：lossy 用 U+FFFD 收尾。
        Err(_) => String::from_utf8_lossy(line).into_owned(),
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

    /// 工具输出里的元数据行：**只有回退到 OEM 代码页时才写**。
    /// UTF-8/ASCII 是常态，不占位；全都没有回退则返回 `None`。
    pub fn note(&self) -> Option<String> {
        let slots = self.slots.lock().ok()?;
        let parts: Vec<String> = ["stdout", "stderr"]
            .iter()
            .enumerate()
            .filter_map(|(index, name)| match slots[index] {
                Some(StreamEncoding::Oem(codepage)) => Some(format!(
                    "{name}=oem-{codepage}（非 UTF-8，已按系统 OEM 代码页 {codepage} 还原）"
                )),
                _ => None,
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
        assert_eq!(decoder.push_line(&GBK_ACCESS_DENIED), vec!["拒绝访问。"]);
        assert_eq!(decoder.decision(), Some(StreamEncoding::Oem(936)));
    }

    #[test]
    fn ascii_prefix_and_gbk_body_decode_as_one_stream() {
        let mut raw = b"type missing.txt: ".to_vec();
        raw.extend_from_slice(&GBK_ACCESS_DENIED);
        let mut decoder = StreamDecoder::new(936);
        assert_eq!(decoder.push_line(&raw), vec!["type missing.txt: 拒绝访问。"]);
    }

    #[test]
    fn ascii_only_stream_makes_no_decision() {
        let mut decoder = StreamDecoder::new(936);
        assert_eq!(decoder.push_line(b"hello world"), vec!["hello world"]);
        assert_eq!(decoder.decision(), None);
        assert!(decoder.finish().is_empty());
    }

    /// UTF-8 中文先暂定、攒够样本后转正；转正前扣住，转正后一次放行。
    #[test]
    fn utf8_stream_finalizes_after_sample_window() {
        let mut decoder = StreamDecoder::new(936);
        // 单行 15 个非 ASCII 字节 < 256：暂定，先不放行。
        assert!(decoder.push_line("拒绝访问。".as_bytes()).is_empty());
        assert_eq!(decoder.decision(), Some(StreamEncoding::Utf8));

        // 继续补到 ≥256 个非 ASCII 字节后转正，扣住的行整段放行。
        let filler = "中文输出".repeat(9); // 4 字 × 3 字节 × 9 = 108 字节
        let mut released = decoder.push_line(filler.as_bytes());
        while released.is_empty() {
            released = decoder.push_line(filler.as_bytes());
        }
        assert_eq!(released.first().unwrap(), "拒绝访问。");
        assert!(released.len() >= 3, "扣住的行应整段放行: {}", released.len());
    }

    /// 判定按整条流做一次：先转正 UTF-8 后，后续 GBK 行不许翻转（也不许逐行混解）。
    #[test]
    fn decision_never_flips_per_line() {
        let mut decoder = StreamDecoder::new(936);
        let utf8 = "中文".repeat(64); // 384 字节非 ASCII，直接转正
        assert_eq!(decoder.push_line(utf8.as_bytes()), vec![utf8.clone()]);
        assert_eq!(decoder.decision(), Some(StreamEncoding::Utf8));

        let text = decoder.push_line(&GBK_ACCESS_DENIED).remove(0);
        assert_ne!(text, "拒绝访问。");
        assert!(text.contains('\u{FFFD}'));
        assert_eq!(decoder.decision(), Some(StreamEncoding::Utf8));
    }

    /// 关键补充：GBK 双字节偶合合法 UTF-8（「一」= D2 BB = U+04BB）。
    /// 首行巧合只算暂定，下一行解码失败即整段翻回 OEM，缓冲行一并重解。
    #[test]
    fn gbk_coincidence_is_caught_after_the_sample_window() {
        let coincidence = [0xD2, 0xBB]; // 「一」的 GBK，恰好是合法 UTF-8
        assert!(std::str::from_utf8(&coincidence).is_ok());

        let mut decoder = StreamDecoder::new(936);
        assert!(decoder.push_line(&coincidence).is_empty());
        assert_eq!(decoder.decision(), Some(StreamEncoding::Utf8)); // 暂定

        let released = decoder.push_line(&GBK_ACCESS_DENIED);
        assert_eq!(decoder.decision(), Some(StreamEncoding::Oem(936)));
        assert_eq!(released, vec!["一".to_string(), "拒绝访问。".to_string()]);
    }

    /// 已知残余边界（文档化，不修）：整条流的 ≥256 个非 ASCII 字节全是 GBK 巧合时，
    /// 与 UTF-8 无法区分，会误判成 UTF-8。至少不会 panic、不会丢内容。
    #[test]
    fn pure_coincidence_beyond_sample_window_is_a_documented_boundary() {
        let coincidence = [0xD2, 0xBB]; // 「一」
        let mut decoder = StreamDecoder::new(936);
        // 先攒过 256 字节门槛。
        let mut released = Vec::new();
        for _ in 0..200 {
            released.extend(decoder.push_line(&coincidence));
        }
        assert_eq!(decoder.decision(), Some(StreamEncoding::Utf8));
        assert!(!released.is_empty());
        // 之后再来普通 GBK 也不会翻转——这就是边界本身。
        let text = decoder.push_line(&GBK_ACCESS_DENIED).remove(0);
        assert!(text.contains('\u{FFFD}'));
        assert_eq!(decoder.decision(), Some(StreamEncoding::Utf8));
    }

    /// 截断的多字节序列：不 panic、不吞后续内容。
    #[test]
    fn truncated_multibyte_tail_does_not_swallow_following_lines() {
        let mut decoder = StreamDecoder::new(936);
        // "拒" 的 UTF-8 是 E6 8B 92，这里只给前两字节（模拟进程被杀时写了一半）。
        assert!(decoder.push_line(&[0xE6, 0x8B]).is_empty());
        let released = decoder.finish();
        assert_eq!(released.len(), 1);
        assert!(released[0].contains('\u{FFFD}'));

        let mut decoder = StreamDecoder::new(936);
        assert!(decoder.push_line(&[0xE6, 0x8B]).is_empty());
        // 暂定期：合法行也被扣住，但不会丢——finish 时按顺序放行。
        assert!(decoder.push_line("后续内容".as_bytes()).is_empty());
        let released = decoder.finish();
        assert_eq!(released.len(), 2);
        assert!(released[0].contains('\u{FFFD}'));
        assert_eq!(released[1], "后续内容");
    }

    /// 截断的 GBK 双字节：同样不 panic，后续行照常解出。
    #[test]
    fn truncated_oem_tail_does_not_swallow_following_lines() {
        let mut decoder = StreamDecoder::new(936);
        let released = decoder.push_line(&[0xBE]);
        assert_eq!(decoder.decision(), Some(StreamEncoding::Oem(936)));
        assert_eq!(released.len(), 1);
        assert!(!released[0].is_empty());
        assert_eq!(
            decoder.push_line(&GBK_ACCESS_DENIED),
            vec!["拒绝访问。"]
        );
    }

    #[test]
    fn ledger_reports_only_oem_fallbacks() {
        let ledger = EncodingLedger::new();
        assert!(ledger.note().is_none());

        // UTF-8 是常态：不写元数据。
        ledger.record(1, StreamEncoding::Utf8);
        assert!(ledger.note().is_none());

        ledger.record(0, StreamEncoding::Oem(936));
        let note = ledger.note().expect("有 OEM 回退就要写元数据");
        assert!(note.contains("stdout=oem-936"), "实际: {note}");
        assert!(!note.contains("stderr"), "UTF-8 流不该出现在元数据里: {note}");
    }
}
