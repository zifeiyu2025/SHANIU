//! 图片字节的进程内引用存储：**base64 只活在内存里，永不进 rollout / 事件日志**。
//!
//! 会话档案里保存的是 [`ImageRef`](super::ImageRef)（hash + 尺寸 + 路径），
//! 请求构造时按 hash 从这里取 data URL。进程重启后缓存为空，引用退化为
//! 一条「图片不可用，请重新调用 view_image」的文本说明——**不重放旧字节**，
//! 也因此不会让档案膨胀。
//!
//! 淘汰策略：条目数或总字节超限时整批清空，而不是 LRU。图片是短生命周期数据，
//! 清空的代价只是模型看到「不可用」提示；换 LRU 的复杂度不值得。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// 进程内最多保留几张图片。
pub const MAX_ENTRIES: usize = 24;
/// 进程内图片原始字节总量上限。
pub const MAX_TOTAL_BYTES: usize = 48 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredImage {
    pub mime: String,
    /// 已经拼好的 `data:<mime>;base64,...`，避免每次请求重复编码。
    pub data_url: String,
    bytes: usize,
}

#[derive(Default)]
struct Store {
    entries: HashMap<String, StoredImage>,
    total_bytes: usize,
}

fn store() -> &'static Mutex<Store> {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(Store::default()))
}

fn lock() -> std::sync::MutexGuard<'static, Store> {
    store().lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 存入一张图片（`hash` 由调用方按内容计算，作为引用键）。
pub fn put(hash: &str, mime: &str, bytes: &[u8]) {
    use base64::Engine as _;
    let data_url = format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    );
    let mut guard = lock();
    if guard.entries.len() >= MAX_ENTRIES
        || guard.total_bytes.saturating_add(bytes.len()) > MAX_TOTAL_BYTES
    {
        guard.entries.clear();
        guard.total_bytes = 0;
    }
    if let Some(previous) = guard
        .entries
        .insert(hash.to_string(), StoredImage {
            mime: mime.to_string(),
            data_url,
            bytes: bytes.len(),
        })
    {
        guard.total_bytes = guard.total_bytes.saturating_sub(previous.bytes);
    }
    guard.total_bytes = guard.total_bytes.saturating_add(bytes.len());
}

/// 按 hash 取 data URL；缓存里没有（进程重启 / 已被淘汰）就返回 `None`。
pub fn data_url(hash: &str) -> Option<String> {
    lock().entries.get(hash).map(|image| image.data_url.clone())
}

/// 当前缓存里的条目数（测试与自检用）。
pub fn len() -> usize {
    lock().entries.len()
}

#[cfg(test)]
pub fn clear() {
    let mut guard = lock();
    guard.entries.clear();
    guard.total_bytes = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_then_get_roundtrip() {
        clear();
        put("hash-1", "image/png", b"\x89PNG");
        let url = data_url("hash-1").expect("存入后应能取回");
        assert!(url.starts_with("data:image/png;base64,"), "{url}");
        assert!(data_url("hash-missing").is_none());
    }

    #[test]
    fn overwrite_does_not_double_count_bytes() {
        clear();
        put("hash-2", "image/png", &[0u8; 16]);
        let first = lock().total_bytes;
        put("hash-2", "image/png", &[0u8; 16]);
        assert_eq!(lock().total_bytes, first);
        assert_eq!(len(), 1);
    }
}
