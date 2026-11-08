//! 内置工具（docs §8 tools/*）。
//!
//! 每个文件一组相关工具：只读在 `fs_read`，命令执行在 `unified_exec`，
//! 结构化补丁在 `apply_patch`，Plan Mode 在 `plan`（M2）。工具只做执行，
//! 不做审批（M2 permission 模块在路由层前置裁决）。

pub mod apply_patch;
pub mod fs_read;
pub mod job_tools;
pub mod orchestrate;
pub mod plan;
pub mod skill;
pub mod terminal;
pub mod todo;
pub mod unified_exec;
pub mod view_image;
pub mod web;

use std::sync::Arc;

use crate::tool_router::{PlanState, ToolRouter};

/// 注册全部内置工具（Plan Mode 工具对共享 PlanState）。
pub fn register_builtin(router: &mut ToolRouter, plan_state: Arc<PlanState>) {
    router.register(Arc::new(fs_read::ReadFile::new()));
    router.register(Arc::new(fs_read::ListDir::new()));
    router.register(Arc::new(fs_read::Search::new()));
    router.register(Arc::new(unified_exec::UnifiedExec::new()));
    router.register(Arc::new(apply_patch::ApplyPatch::new()));
    router.register(Arc::new(todo::TodoWrite::default()));
    router.register(Arc::new(skill::SkillTool::new()));
    router.register(Arc::new(job_tools::JobOutput::new()));
    router.register(Arc::new(job_tools::JobList::new()));
    router.register(Arc::new(job_tools::JobKill::new()));
    // SH-070：持久交互终端（ConPTY）。会话登记表是进程级的，见 terminal.rs 模块注释。
    router.register(Arc::new(terminal::Terminal::new()));
    // SH-052：编排四工具始终注册（可见性由 agent_loop 声明层按深度/能力过滤），
    // 这样分发层的第二道门控也能对「模型伪造工具名」生效。
    orchestrate::register(router);
    plan::register(router, plan_state);

    // SH-072：web_search 内置免 key 实现，目前唯一 provider 是 Bing。
    // `SearchProvider` trait 与 FallbackProvider 保留：以后接插件或换源从这里进，不改调用方。
    // 抓取走共享 WebClient：同一套 scheme/私网/逐跳重定向校验，不因「固定域名」放宽。
    let web_client = Arc::new(web::WebClient::new());
    let chain: Arc<dyn web::SearchProvider> = Arc::new(web::Bing::new(web_client.clone()));
    router.register(Arc::new(web::WebSearch::new(chain)));
    router.register(Arc::new(web::WebFetch::new(web_client)));

    // SH-072：看工作区内的图片。只读；字节只进进程内缓存，会话档案只落引用。
    router.register(Arc::new(view_image::ViewImage::new()));
}
