//! 图片引用与进程内字节缓存（SH-072）。
//!
//! 这是 `model_client` 与 `tools` 之间的共享叶子模块：两边都要用同一份
//! `ImageRef`（档案里持久化的引用）和同一份 `image_store`（进程内字节），
//! 但 `tools` 不得依赖 `model_client`（`docs/01 §2.3.2`）。把两者下沉到这里，
//! 跨层借用就变成"都依赖同一片叶子"，而不是给禁止边开豁免。

use serde::{Deserialize, Serialize};

pub mod image_store;

/// 图片引用：rollout / 事件日志里只允许出现这份元数据，永远不含字节。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRef {
    /// 工作区相对路径（展示与重新读取用）。
    pub path: String,
    /// 内容 sha256，[`image_store`] 的键。
    pub sha256: String,
    pub width: u32,
    pub height: u32,
    /// 按分辨率估算的输入 token（不是字节数）。
    pub tokens: u32,
    pub mime: String,
    /// 用户附件的回执 id（`attachments/<session>/<id>.bin`）。`view_image` 回填没有来源，
    /// 恒为 `None`；只有它是 `Some` 时，进程重启后才可能按 hash 回灌字节（rehydrate）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment_id: Option<String>,
    /// 编码后的实际字节数。前端 `ImageAttachmentRef.bytes` 需要它来与 DSH 的图片引用同形；
    /// 旧档案没有这个键，缺省 `None`（展示层按未知处理，不编造 0）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

/// 前端任务文本里的图片隐藏标记前缀（整行形态：`<!-- shaniu-image:{json} -->`）。
///
/// 新版本前端**不再写**它；保留解码知识只为两件事：识别旧客户端/旧队列送来的文本，
/// 以及在 Rust 侧把它剥掉——marker 不得进入任何发给模型或标题生成的文本。
pub const IMAGE_MARKER_PREFIX: &str = "<!-- shaniu-image:";
const IMAGE_MARKER_SUFFIX: &str = " -->";

/// 删除整行图片 marker，其余字节原样保留（不重排换行、不改用户正文）。
///
/// 只认「整行」：正文里偶然提到该前缀不该被吞掉。坏标记（缺后缀）也原样保留——
/// 宁可让模型看到一行怪文本，也不要悄悄删掉用户内容。
pub fn strip_markers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for segment in text.split_inclusive('\n') {
        let without_newline = segment.strip_suffix('\n').unwrap_or(segment);
        let line = without_newline.strip_suffix('\r').unwrap_or(without_newline);
        if line.starts_with(IMAGE_MARKER_PREFIX) {
            if line.ends_with(IMAGE_MARKER_SUFFIX) {
                continue;
            }
            // 保留但不静默：前端编码坏掉时，模型会看到一段像 HTML 注释的怪文本，
            // 得留一条日志线索。只记行长，不打印正文（不把用户内容写进日志）。
            tracing::warn!(
                line_len = line.len(),
                "文本中发现未闭合的图片 marker，已按正文保留"
            );
        }
        out.push_str(segment);
    }
    out
}

/// 字节内容的 sha256（`image_store` 的键）。图片引用的唯一哈希来源，别处不要再写一份。
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 按档案里的引用把字节补回进程缓存（重开会话用）。
///
/// 定位只有一条路：读字节的实现只拿到 [`ImageRef`]，也就是只能按 `path` 找
/// （`attachments/<owner_session>/<id>.bin`）。**签名里没有当前 `session_id`**——
/// 分叉会话的引用仍指向父会话目录，按当前会话拼路径必然落空（spec §5.2）。
///
/// 返回 `(还原数, 失败数)`。失败只记日志、不中断：单张图丢了不该让整个会话开不了。
pub fn rehydrate<F>(refs: &[ImageRef], mut read: F) -> (usize, usize)
where
    F: FnMut(&ImageRef) -> Option<Vec<u8>>,
{
    let mut restored = 0usize;
    let mut failed = 0usize;
    for image in refs {
        // 没有回执的引用（`view_image` 回填）本来就没有可回读的字节来源。
        if image.attachment_id.is_none() || image_store::contains(&image.sha256) {
            continue;
        }
        match read(image) {
            Some(bytes) if sha256_hex(&bytes) == image.sha256 => {
                image_store::put(&image.sha256, &image.mime, &bytes);
                restored += 1;
            }
            Some(_) => {
                failed += 1;
                tracing::warn!(path = %image.path, "历史图片字节与档案哈希不一致，已跳过");
            }
            None => {
                failed += 1;
                tracing::warn!(path = %image.path, "历史图片字节缺失，已跳过（附件被手工清理或数据根已迁移）");
            }
        }
    }
    (restored, failed)
}

/// 从图片文件头解析出的真实格式与尺寸（**不信任上传方声明**）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageInfo {
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
}

/// 只看文件头判断格式与像素尺寸，**不做整图解码**。
///
/// 用户附件是未信任字节：整图解码会把解压炸弹的分配面打开（`view_image` 的 12MB
/// 解码上限不适用于 32MB 附件）。只解析 PNG IHDR / GIF LSD / JPEG SOFn /
/// WEBP（VP8 / VP8L / VP8X）这几段定长头。
pub fn sniff_image(bytes: &[u8]) -> Option<ImageInfo> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        // IHDR 固定是第一个 chunk：8..12 长度，12..16 类型，16..24 宽高（大端）。
        if bytes.len() < 24 || &bytes[12..16] != b"IHDR" {
            return None;
        }
        let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
        return nonzero(ImageInfo { mime: "image/png", width, height });
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        // 逻辑屏幕描述符：6..8 宽、8..10 高（小端）。
        if bytes.len() < 10 {
            return None;
        }
        let width = u16::from_le_bytes([bytes[6], bytes[7]]) as u32;
        let height = u16::from_le_bytes([bytes[8], bytes[9]]) as u32;
        return nonzero(ImageInfo { mime: "image/gif", width, height });
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return jpeg_size(bytes).and_then(|(width, height)| {
            nonzero(ImageInfo { mime: "image/jpeg", width, height })
        });
    }
    if bytes.len() >= 30 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return webp_size(bytes).and_then(|(width, height)| {
            nonzero(ImageInfo { mime: "image/webp", width, height })
        });
    }
    None
}

fn nonzero(info: ImageInfo) -> Option<ImageInfo> {
    if info.width == 0 || info.height == 0 {
        None
    } else {
        Some(info)
    }
}

/// 扫描 JPEG 段直到 SOFn：精度 1 字节后是高度、宽度（各 2 字节，大端）。
fn jpeg_size(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut cursor = 2usize;
    while cursor + 3 < bytes.len() {
        if bytes[cursor] != 0xFF {
            cursor += 1;
            continue;
        }
        let marker = bytes[cursor + 1];
        // 填充字节与无载荷标记（SOI/EOI/RSTn）不读段长。
        if marker == 0xFF {
            cursor += 1;
            continue;
        }
        if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) {
            cursor += 2;
            continue;
        }
        let segment = u16::from_be_bytes([bytes[cursor + 2], bytes[cursor + 3]]) as usize;
        if segment < 2 {
            return None;
        }
        let is_sof = (0xC0..=0xCF).contains(&marker)
            && marker != 0xC4
            && marker != 0xC8
            && marker != 0xCC;
        if is_sof {
            if cursor + 8 >= bytes.len() {
                return None;
            }
            let height = u16::from_be_bytes([bytes[cursor + 5], bytes[cursor + 6]]) as u32;
            let width = u16::from_be_bytes([bytes[cursor + 7], bytes[cursor + 8]]) as u32;
            return Some((width, height));
        }
        cursor += 2 + segment;
    }
    None
}

/// WEBP 三种子格式的尺寸都在定长头里。
fn webp_size(bytes: &[u8]) -> Option<(u32, u32)> {
    match &bytes[12..16] {
        b"VP8 " => {
            // 3 字节帧标签 + 3 字节起始码 9D 01 2A，其后 14 位宽、14 位高。
            if bytes.len() < 30 || &bytes[23..26] != b"\x9d\x01\x2a" {
                return None;
            }
            let width = u16::from_le_bytes([bytes[26], bytes[27]]) & 0x3FFF;
            let height = u16::from_le_bytes([bytes[28], bytes[29]]) & 0x3FFF;
            Some((width as u32, height as u32))
        }
        b"VP8L" => {
            if bytes.len() < 25 || bytes[20] != 0x2F {
                return None;
            }
            let bits = u32::from_le_bytes([bytes[21], bytes[22], bytes[23], bytes[24]]);
            let width = (bits & 0x3FFF) + 1;
            let height = ((bits >> 14) & 0x3FFF) + 1;
            Some((width, height))
        }
        b"VP8X" => {
            if bytes.len() < 30 {
                return None;
            }
            let width = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], 0]) + 1;
            let height = u32::from_le_bytes([bytes[27], bytes[28], bytes[29], 0]) + 1;
            Some((width, height))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_marker_lines_but_keeps_body_bytes() {
        let text = "看这张\n<!-- shaniu-image:%7B%22a%22%3A1%7D -->\n再解释一下\n";
        assert_eq!(strip_markers(text), "看这张\n再解释一下\n");
    }

    #[test]
    fn keeps_malformed_marker_and_inline_mentions() {
        // 缺后缀：坏标记当正文保留，不因一条注释吞掉用户内容。
        let malformed = "开头\n<!-- shaniu-image:not-closed\n结尾";
        assert_eq!(strip_markers(malformed), malformed);
        // 行内提到前缀不是整行标记，也必须保留。
        let inline = "他说 <!-- shaniu-image:x --> 这句话怪怪的";
        assert_eq!(strip_markers(inline), inline);
        // CRLF 标记行：按整行识别并删除，其余行保留原换行。
        let crlf = "第一行\r\n<!-- shaniu-image:x -->\r\n第二行";
        assert_eq!(strip_markers(crlf), "第一行\r\n第二行");
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(width, height, image::Rgb([1, 2, 3]));
        let mut bytes = Vec::new();
        image
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    fn jpeg(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(width, height, image::Rgb([1, 2, 3]));
        let mut bytes = Vec::new();
        image
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        bytes
    }

    #[test]
    fn sniffs_png_and_jpeg_real_headers() {
        assert_eq!(
            sniff_image(&png(37, 19)),
            Some(ImageInfo { mime: "image/png", width: 37, height: 19 })
        );
        assert_eq!(
            sniff_image(&jpeg(41, 23)),
            Some(ImageInfo { mime: "image/jpeg", width: 41, height: 23 })
        );
    }

    #[test]
    fn sniffs_gif_and_webp_headers_without_decoding() {
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&320u16.to_le_bytes());
        gif.extend_from_slice(&200u16.to_le_bytes());
        gif.extend_from_slice(&[0u8; 8]);
        assert_eq!(
            sniff_image(&gif),
            Some(ImageInfo { mime: "image/gif", width: 320, height: 200 })
        );

        // VP8X：24 位 LE 的 width-1 / height-1。
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&100u32.to_le_bytes());
        webp.extend_from_slice(b"WEBPVP8X");
        webp.extend_from_slice(&10u32.to_le_bytes()); // chunk size
        webp.extend_from_slice(&[0u8; 4]); // flags + reserved
        webp.extend_from_slice(&[127, 1, 0]); // width-1 = 383
        webp.extend_from_slice(&[95, 0, 0]); // height-1 = 95
        assert_eq!(
            sniff_image(&webp),
            Some(ImageInfo { mime: "image/webp", width: 384, height: 96 })
        );
    }

    #[test]
    fn rejects_non_image_and_zero_sized_headers() {
        assert_eq!(sniff_image(b"not an image"), None);
        // PNG 签名正确但 IHDR 宽为 0：不是可发送的图片。
        let mut zero = b"\x89PNG\r\n\x1a\n".to_vec();
        zero.extend_from_slice(&13u32.to_be_bytes());
        zero.extend_from_slice(b"IHDR");
        zero.extend_from_slice(&0u32.to_be_bytes());
        zero.extend_from_slice(&8u32.to_be_bytes());
        assert_eq!(sniff_image(&zero), None);
    }
}

