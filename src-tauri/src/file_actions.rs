//! 桌面文件动作的落盘面（SH-092 批 2 / 上游 `ui-file-actions` 的 Desktop 适配）。
//!
//! **路径只来自用户在原生保存对话框里的选择**：命令自己弹窗、自己写盘，不接受前端传入的
//! 目标路径。若把路径当参数暴露，Web 内容就得到一个任意写能力——保存动作的所有权必须在
//! 用户手势这一侧。
//!
//! `download` 与 `saveAs` 同走本命令：桌面壳没有 Chromium 下载管理器，静默写 Downloads
//! 会绕过用户选择（与上游桌面版的偏差，已在 docs/05 声明）。
use base64::Engine as _;

/// 单次保存的字节上限：与 `workspace_files::MAX_FILE_BYTES` 同量级，超出如实拒绝。
pub const MAX_SAVE_BYTES: usize = 64 * 1024 * 1024;

fn decode_limited(body: &str, limit: usize) -> Result<Vec<u8>, String> {
    let data = base64::engine::general_purpose::STANDARD
        .decode(body.trim())
        .map_err(|_| "payload 不是合法 base64".to_string())?;
    if data.len() > limit {
        return Err(format!("超出单次保存上限 {limit} 字节"));
    }
    Ok(data)
}

/// 建议文件名：去掉路径分隔符与 Windows 非法字符，去掉首尾点与空白；空名回落 `download`。
fn sanitize_save_name(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| if "<>:\"/\\|?*".contains(c) || c.is_control() { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim_matches(|c: char| c == '.' || c.is_whitespace());
    if cleaned.is_empty() {
        "download".to_string()
    } else {
        cleaned.chars().take(180).collect()
    }
}

#[tauri::command]
pub fn save_file_bytes(
    app: tauri::AppHandle,
    default_name: String,
    bytes: String,
) -> Result<Option<String>, String> {
    let data = decode_limited(&bytes, MAX_SAVE_BYTES)?;
    use tauri_plugin_dialog::DialogExt;
    let picked = app
        .dialog()
        .file()
        .set_title("保存文件")
        .set_file_name(sanitize_save_name(&default_name))
        .blocking_save_file();
    let Some(path) = picked.and_then(|path| path.into_path().ok()) else {
        return Ok(None);
    };
    std::fs::write(&path, data).map_err(|e| format!("写入失败: {e}"))?;
    Ok(Some(path.to_string_lossy().into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_payload_is_decoded_and_capped() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(b"hello");
        assert_eq!(decode_limited(&encoded, 16).expect("合法载荷"), b"hello");
        assert!(decode_limited("不是 base64", 16).is_err());
        let too_big = base64::engine::general_purpose::STANDARD.encode(b"hello");
        assert_eq!(decode_limited(&too_big, 4).expect_err("超限要拒"), "超出单次保存上限 4 字节");
    }

    #[test]
    fn suggested_names_cannot_escape_or_carry_illegal_characters() {
        assert_eq!(sanitize_save_name("a/b\\c:d*.txt"), "a_b_c_d_.txt");
        assert_eq!(sanitize_save_name("  .. "), "download");
        assert_eq!(sanitize_save_name(""), "download");
        assert_eq!(sanitize_save_name(&"长".repeat(300)).chars().count(), 180);
    }
}