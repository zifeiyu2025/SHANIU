//! 内置工具（docs §8 tools/*）。
//!
//! 每个文件一组相关工具：只读在 `fs_read`，命令执行在 `unified_exec`，
//! 结构化补丁在 `apply_patch`，Plan Mode 在 `plan`（M2）。工具只做执行，
//! 不做审批（M2 permission 模块在路由层前置裁决）。

pub mod apply_patch;
pub mod fs_read;
pub mod unified_exec;

use std::sync::Arc;

use crate::tool_router::ToolRouter;

/// 注册 M1 全部内置工具。
pub fn register_builtin(router: &mut ToolRouter) {
    router.register(Arc::new(fs_read::ReadFile::new()));
    router.register(Arc::new(fs_read::ListDir::new()));
    router.register(Arc::new(fs_read::Search::new()));
    router.register(Arc::new(unified_exec::UnifiedExec::new()));
    router.register(Arc::new(apply_patch::ApplyPatch::new()));
}
