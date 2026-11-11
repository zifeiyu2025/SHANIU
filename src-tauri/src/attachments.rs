//! 会话附件暂存（SH-116 批 1，桌面壳文件上传的宿主面）。
//!
//! 桌面壳没有 DSH 的 HTTP 上传路由，`file-upload` 的 fetch 钩子把原始字节经 Tauri
//! 命令交到这里；只写 `<data_root>\attachments\<session>\<ULID>.bin` + 同名 `.json`
//! 元数据，不触碰 rollout。批 1 接文件与图片**暂存**（图片只存原字节，不做归一化），
//! 历史附件的投影/回读留给批 2。
//!
//! 本模块零业务依赖（std + base64/serde/ulid），数据根由组合根传入；FF-1 E38。

use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// 单次上传硬上限：与保存对话框/预览读面的 32 MiB 口径一致。
pub const MAX_UPLOAD_BYTES: usize = 32 * 1024 * 1024;
/// 读回 IPC 的默认上限（prompt 只取一个文本窗口）。
pub const DEFAULT_READ_BYTES: usize = 128 * 1024;

/// 上传成功回执（`FileUploadValue` / `ImageUploadValue` 的宿主侧形状）。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredAttachment {
    pub receipt_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<StoredFile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<StoredImage>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredFile {
    pub attachment_id: String,
    pub name: String,
    pub bytes: u64,
}

/// 图片暂存回执：桌面壳只校验/保存原字节，尺寸由浏览器解码后随上传带来。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredImage {
    pub attachment_id: String,
    pub name: String,
    pub media_type: String,
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
}

/// 读回结果：prompt 侧按 mediaType/扩展名决定内联正文还是占位。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentRead {
    pub name: String,
    pub media_type: String,
    pub bytes: u64,
    pub data: String,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Meta {
    name: String,
    media_type: String,
    bytes: u64,
    created_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    height: Option<u32>,
}

fn valid_session(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// 回执 id 必须是一个规范 ULID：既防路径穿越，也防伪造其他会话的 id。
fn parse_receipt(receipt_id: &str) -> Result<ulid::Ulid, String> {
    let id = ulid::Ulid::from_string(receipt_id).map_err(|_| format!("非法的附件回执：{receipt_id}"))?;
    if id.to_string() != receipt_id {
        return Err(format!("非法的附件回执：{receipt_id}"));
    }
    Ok(id)
}

/// 展示名只保留叶子名并去掉控制字符；真实字节永远落在 ULID 文件名下。
fn display_name(name: &str) -> String {
    let leaf = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("attachment")
        .trim();
    let cleaned: String = leaf
        .chars()
        .filter(|c| !c.is_control())
        .take(120)
        .collect();
    if cleaned.is_empty() {
        "attachment".to_string()
    } else {
        cleaned
    }
}

fn session_dir(root: &Path, session_id: &str) -> Result<PathBuf, String> {
    if !valid_session(session_id) {
        return Err(format!("非法的会话 id：{session_id}"));
    }
    Ok(root.join("attachments").join(session_id))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 附件目录占用（设置页只读一行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentStats {
    pub file_count: u64,
    pub total_bytes: u64,
}

/// 遍历 `<data_root>/attachments/<session>/` 下所有 `.bin`，累计数量与字节数。
///
/// 只读 `metadata()`，不读文件内容；只下探一层会话目录，不跟符号链接出界。
/// 目录不存在返回 `(0, 0)`；其余读取失败原样报错——界面据此显示「—」，不显示 0。
pub fn stats(data_root: &Path) -> Result<AttachmentStats, String> {
    let mut stats = AttachmentStats {
        file_count: 0,
        total_bytes: 0,
    };
    let sessions = match std::fs::read_dir(data_root.join("attachments")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(stats),
        Err(error) => return Err(format!("读取附件目录失败：{error}")),
    };
    for session in sessions {
        let session = session.map_err(|error| format!("读取附件目录失败：{error}"))?;
        let kind = session
            .file_type()
            .map_err(|error| format!("读取附件目录失败：{error}"))?;
        if !kind.is_dir() {
            continue;
        }
        let files = std::fs::read_dir(session.path())
            .map_err(|error| format!("读取附件目录失败：{error}"))?;
        for file in files {
            let file = file.map_err(|error| format!("读取附件目录失败：{error}"))?;
            let kind = file
                .file_type()
                .map_err(|error| format!("读取附件文件类型失败：{error}"))?;
            if !kind.is_file() {
                continue;
            }
            let path = file.path();
            if path.extension().and_then(|value| value.to_str()) != Some("bin") {
                continue;
            }
            let size = file
                .metadata()
                .map_err(|error| format!("读取附件文件信息失败：{error}"))?
                .len();
            stats.file_count += 1;
            stats.total_bytes = stats.total_bytes.saturating_add(size);
        }
    }
    Ok(stats)
}

/// 暂存一份文件/图片字节；返回给 file-upload 客户端消费的回执。
pub fn store(
    data_root: &Path,
    session_id: &str,
    name: &str,
    media_type: &str,
    data_base64: &str,
    width: Option<u32>,
    height: Option<u32>,
) -> Result<StoredAttachment, String> {
    let dir = session_dir(data_root, session_id)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_base64.trim())
        .map_err(|e| format!("附件 base64 解码失败: {e}"))?;
    if bytes.is_empty() {
        return Err("附件为空".into());
    }
    if bytes.len() > MAX_UPLOAD_BYTES {
        return Err(format!(
            "附件超过上限（{} 字节 > {} 字节）",
            bytes.len(),
            MAX_UPLOAD_BYTES
        ));
    }
    fs::create_dir_all(&dir).map_err(|e| format!("创建附件目录失败（{}）: {e}", dir.display()))?;
    let id = ulid::Ulid::generate().to_string();
    let data_path = dir.join(format!("{id}.bin"));
    let meta_path = dir.join(format!("{id}.json"));
    let clean_name = display_name(name);
    let clean_media = media_type.trim().chars().take(128).collect::<String>();
    let media = if clean_media.is_empty() {
        "application/octet-stream".to_string()
    } else {
        clean_media
    };
    let is_image = media.starts_with("image/");
    let (image_width, image_height) = if is_image {
        match (width, height) {
            (Some(w), Some(h)) if w > 0 && h > 0 => (Some(w), Some(h)),
            _ => return Err("图片附件缺少有效尺寸".into()),
        }
    } else {
        (None, None)
    };
    let meta = Meta {
        name: clean_name.clone(),
        media_type: media.clone(),
        bytes: bytes.len() as u64,
        created_at_ms: now_ms(),
        width: image_width,
        height: image_height,
    };
    fs::write(&data_path, &bytes).map_err(|e| format!("写附件失败（{}）: {e}", data_path.display()))?;
    let body = serde_json::to_vec(&meta).map_err(|e| format!("附件元数据序列化失败: {e}"))?;
    if let Err(e) = fs::write(&meta_path, body) {
        let _ = fs::remove_file(&data_path);
        return Err(format!("写附件元数据失败（{}）: {e}", meta_path.display()));
    }
    Ok(StoredAttachment {
        receipt_id: id.clone(),
        file: if is_image {
            None
        } else {
            Some(StoredFile { attachment_id: id.clone(), name: clean_name.clone(), bytes: bytes.len() as u64 })
        },
        image: if is_image {
            Some(StoredImage {
                attachment_id: id,
                name: clean_name,
                media_type: media,
                bytes: bytes.len() as u64,
                width: image_width.unwrap_or(1),
                height: image_height.unwrap_or(1),
            })
        } else {
            None
        },
    })
}

/// 读回附件：`max_bytes` 限制返回的字节窗口（prompt 只取一页），超出置 `truncated`。
pub fn read(
    data_root: &Path,
    session_id: &str,
    receipt_id: &str,
    max_bytes: usize,
) -> Result<AttachmentRead, String> {
    let dir = session_dir(data_root, session_id)?;
    let id = parse_receipt(receipt_id)?;
    let meta_path = dir.join(format!("{id}.json"));
    let data_path = dir.join(format!("{id}.bin"));
    let meta_body = fs::read(&meta_path).map_err(|e| format!("附件不存在或元数据缺失: {e}"))?;
    let meta: Meta = serde_json::from_slice(&meta_body).map_err(|e| format!("附件元数据损坏: {e}"))?;
    let bytes = fs::read(&data_path).map_err(|e| format!("附件字节缺失: {e}"))?;
    let limit = max_bytes.clamp(1, MAX_UPLOAD_BYTES);
    let truncated = bytes.len() > limit;
    let window = &bytes[..bytes.len().min(limit)];
    Ok(AttachmentRead {
        name: meta.name,
        media_type: meta.media_type,
        bytes: meta.bytes,
        data: base64::engine::general_purpose::STANDARD.encode(window),
        truncated,
        width: meta.width,
        height: meta.height,
    })
}

/// 读回**完整**字节（无截断）与元数据里记录的媒体类型。
///
/// 只给「把附件真正送进模型」用：文本预览走 [`read`] 的窗口，图片必须拿全量字节，
/// 否则发出去的是半张图。安全边界不变：会话 id 与回执 id 都要过 [`session_dir`] /
/// [`parse_receipt`]，`MAX_UPLOAD_BYTES` 仍是上限。
pub fn read_full(
    data_root: &Path,
    session_id: &str,
    receipt_id: &str,
) -> Result<(String, Vec<u8>), String> {
    let dir = session_dir(data_root, session_id)?;
    let id = parse_receipt(receipt_id)?;
    let meta_path = dir.join(format!("{id}.json"));
    let data_path = dir.join(format!("{id}.bin"));
    let meta_body = fs::read(&meta_path).map_err(|e| format!("附件不存在或元数据缺失: {e}"))?;
    let meta: Meta = serde_json::from_slice(&meta_body).map_err(|e| format!("附件元数据损坏: {e}"))?;
    let bytes = fs::read(&data_path).map_err(|e| format!("附件字节缺失: {e}"))?;
    if bytes.is_empty() {
        return Err("附件为空".into());
    }
    if bytes.len() > MAX_UPLOAD_BYTES {
        return Err(format!(
            "附件超过上限（{} 字节 > {} 字节）",
            bytes.len(),
            MAX_UPLOAD_BYTES
        ));
    }
    Ok((meta.media_type, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> crate::test_support::TestTempDir {
        crate::test_support::TestTempDir::new(&format!("attach-{tag}"))
    }

    #[test]
    fn stores_and_reads_file_roundtrip() {
        let root = temp_root("roundtrip");
        let stored = store(
            &root,
            "sess-1",
            "notes.txt",
            "text/plain",
            &base64::engine::general_purpose::STANDARD.encode(b"hello attachment"),
            None,
            None,
        )
        .expect("store");
        let file = stored.file.as_ref().expect("file receipt");
        assert_eq!(file.name, "notes.txt");
        assert_eq!(file.bytes, 16);
        assert!(stored.image.is_none());
        let read = read(&root, "sess-1", &stored.receipt_id, 4096).expect("read");
        assert_eq!(read.name, "notes.txt");
        assert_eq!(read.media_type, "text/plain");
        assert_eq!(read.bytes, 16);
        assert!(!read.truncated);
        assert_eq!(
            base64::engine::general_purpose::STANDARD.decode(read.data).unwrap(),
            b"hello attachment"
        );
    }

    #[test]
    fn truncates_read_window_and_keeps_metadata_size() {
        let root = temp_root("truncate");
        let stored = store(
            &root,
            "sess-1",
            "big.txt",
            "text/plain",
            &base64::engine::general_purpose::STANDARD.encode(vec![b'x'; 100]),
            None,
            None,
        )
        .expect("store");
        let read = read(&root, "sess-1", &stored.receipt_id, 10).expect("read");
        assert!(read.truncated);
        assert_eq!(read.bytes, 100);
        assert_eq!(read.data.len(), 16); // 10 bytes -> 16 base64 chars（无 padding）
    }

    #[test]
    fn stores_image_with_dimensions() {
        let root = temp_root("image");
        let stored = store(
            &root,
            "sess-1",
            "shot.png",
            "image/png",
            "aGk=",
            Some(4),
            Some(3),
        )
        .expect("store image");
        let image = stored.image.as_ref().expect("image receipt");
        assert_eq!(image.name, "shot.png");
        assert_eq!(image.media_type, "image/png");
        assert_eq!((image.width, image.height), (4, 3));
        assert!(stored.file.is_none());
        let read = read(&root, "sess-1", &stored.receipt_id, 1024).expect("read image");
        assert_eq!(read.media_type, "image/png");
        assert_eq!((read.width, read.height), (Some(4), Some(3)));
        // 图片缺尺寸必须拒绝，不能编一个 0x0 的伪图
        assert!(store(&root, "sess-1", "bad.png", "image/png", "aGk=", None, None).is_err());
    }

    #[test]
    fn rejects_traversal_and_foreign_receipts() {
        let root = temp_root("reject");
        assert!(store(&root, "../escape", "a", "text/plain", "aGk=", None, None).is_err());
        assert!(store(&root, "sess-1", "a", "text/plain", "!!!", None, None).is_err());
        assert!(store(&root, "sess-1", "a", "text/plain", "", None, None).is_err());
        assert!(read(&root, "sess-1", "../../etc/passwd", 10).is_err());
        assert!(read(&root, "sess-1", "01ARZ3NDEKTSV4RRFFQ69G5FAV", 10).is_err());
    }

    #[test]
    fn sanitizes_display_name_to_leaf() {
        let root = temp_root("name");
        let stored = store(
            &root,
            "sess-1",
            r"C:\Users\me\secret\<b>.txt",
            "text/plain",
            "aGk=",
            None,
            None,
        )
        .expect("store");
        assert_eq!(stored.file.as_ref().expect("file").name, "<b>.txt");
        // 路径穿越不能影响落点：字节文件在 ULID 名下
        assert!(root.join("attachments/sess-1").join(format!("{}.bin", stored.receipt_id)).is_file());
    }

    #[test]
    fn stats_counts_only_bin_files_under_session_dirs() {
        let root = temp_root("stats");
        // 目录还不存在：是 0，不是错误。
        assert_eq!(
            stats(&root).expect("empty stats"),
            AttachmentStats { file_count: 0, total_bytes: 0 }
        );

        let encoded = base64::engine::general_purpose::STANDARD.encode(b"png-bytes");
        store(&root, "sess-1", "a.png", "image/png", &encoded, Some(2), Some(3)).expect("store first");
        store(&root, "sess-2", "b.png", "image/png", &encoded, Some(2), Some(3)).expect("store second");
        // 元数据 `.json` 与游离文件不该计入：统计只认 `.bin`。
        std::fs::write(root.join("attachments").join("stray.txt"), b"ignore").expect("stray");

        let summary = stats(&root).expect("stats");
        assert_eq!(summary.file_count, 2);
        // 两张图同一份内容：总字节 = 两倍原始长度（`.json` 元数据不计入）。
        assert_eq!(summary.total_bytes, 2 * b"png-bytes".len() as u64);
    }
}
