//! 内置工具（docs §8 tools/*）。
//!
//! 每个文件一组相关工具；全部只读工具在 `fs_read`，执行类在 `unified_exec`
//! （增量 4）、补丁在 `apply_patch`（增量 4）。工具只做执行，不做审批
//! （M2 permission 模块在路由层前置裁决）。

pub mod fs_read;
