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
}
