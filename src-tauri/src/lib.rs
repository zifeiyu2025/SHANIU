// SHANIU 是 Windows 专属产品：沙箱（受限令牌 / Job Object / deny-ACE）、ConPTY 终端与
// WebView2 通路都只在这条平台栈上有实现。跨平台分支保留（删掉要逐 diff 摸 sandbox/ 的
// fail-closed 桩，收益不值），但编译期锁死：在别的平台上"编过去了却跑不对"比直接失败更坏。
#[cfg(not(windows))]
compile_error!("SHANIU 只支持 Windows");

// pub 供集成测试（src-tauri/tests/）作为契约面使用
pub mod agent_loop;
pub mod app_state;
pub mod attachments;
/// 模型能力声明与实测探针（SH-132，docs/04 §4.1）。
pub mod capability;
pub mod clock;
pub mod config;
pub mod compaction;
pub mod event_journal;
pub mod events;
pub mod file_actions;
pub mod file_references;
pub mod gateway;
pub mod goals;
pub mod images;
pub mod jobs;
/// A0-1：密钥三态校验（保存后用免费用端点确认 Key 是否被接受）。
pub mod key_check;
pub mod mcp_client;
/// SH-135：`/api/file` 媒体路由（工作区图片 / PDF 预览的取字节通道）。
pub mod media_route;
pub mod model_client;
pub mod model_discovery;
pub mod open_in_app;
pub mod sandbox;
pub mod permission;
pub mod rollout;
pub mod session_compact;
pub mod session_export;
pub mod session_query;
pub mod session_reference;
pub mod session_title;
pub mod skills;
pub mod subagent_registry;
pub mod text_encoding;
/// SH-071：右侧栏「给人用」的独立终端（裸进程 + ConPTY，不进沙箱 / 不进模型上下文）。
pub mod user_terminal;
/// 测试夹具公共设施（自删临时目录）。`#[doc(hidden)]`：不是产品 API，只为单测/集成测试共用。
#[doc(hidden)]
pub mod test_support;
pub mod tool_router;
pub mod tools;
/// A0-2：关窗提示的系统层出口（托盘气泡）。应用内提示在 `hide()` 之后不可见。
pub mod tray_notify;
pub mod workspace_files;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager,
};

use agent_loop::ModelBackend;
use app_state::AppState;
use clock::now_ms;
use config::AppConfig;

/// 冒烟探针用的远程调试端口：只认显式环境变量，非法值直接忽略。
/// 仅在窗口构建路径使用，不参与任何业务逻辑。
fn cdp_debug_port() -> Option<u16> {
    std::env::var("SHANIU_CDP_PORT")
        .ok()
        .and_then(|raw| raw.trim().parse::<u16>().ok())
}

/// IPC 握手（M0 验收）：前端 invoke 携带 nonce，后端经事件通道原样回显。
#[tauri::command]
fn dev_ping(app: tauri::AppHandle, nonce: String) -> Result<(), String> {
    events::emit_pong(&app, &nonce)
}

/// 主窗口是否已收到前端的就绪信号（首帧启动页已可显示）。
static FRONTEND_READY: AtomicBool = AtomicBool::new(false);

/// 本进程内是否已经提示过“图标在托盘区”。只影响第一次点关闭时多出的那一句，
/// 避免每次关闭都重复同一段位置说明。
static TRAY_HINT_SHOWN: AtomicBool = AtomicBool::new(false);

/// 首帧就绪信号：窗口以 `visible: false` 创建（tauri.conf.json），前端把启动页
/// 画出来后调用这里再显示，避免用户先看到未绘制窗口。重复调用无副作用；
/// 前端若始终不发信号（脚本崩溃、页面加载失败），setup 里的 3 秒兜底会强制显示。
///
/// 为什么"先隐藏、就绪再显示"：实测页面 FCP 与窗口 show 基本同刻（Rust 日志里
/// 就绪信号在 gateway 监听后 85–101 ms，页面 FCP 在页龄 120–128 ms），所以窗口真正
/// 可见时启动页已经画好，WebView2 默认纯白帧不会被用户看到。改回"先 show 再加载"
/// 会让那段纯白重新出现（见 docs/01 §9.5.1）。
#[tauri::command]
fn frontend_ready(app: tauri::AppHandle) {
    if !FRONTEND_READY.swap(true, Ordering::SeqCst) {
        tracing::info!("前端就绪信号到达，显示主窗口");
    }
    show_main_window(&app);
}

/// 真正退出进程（一步可达）。
///
/// 关闭窗口只隐藏到托盘，退出必须显式触发：托盘菜单「退出」走同一套清理，
/// 会话头「更多操作 → 退出 SHANIU」调这里。桌面壳里托盘图标可能被折叠，
/// 不能让它成为唯一出口。
#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    tracing::info!("收到退出请求，开始清理并退出进程");
    // SH-021：退出前先还原 DACL 并删除容器 profile，避免残留 ACE/悬空 profile。
    sandbox::shutdown();
    app.exit(0);
}

/// 托盘/菜单发起的“退出请求”事件：有任务时不直接退，先让前端给出三个选项。
pub const QUIT_REQUEST_EVENT: &str = "app://quit-requested";

/// 退出前盘点“还在跑的事”。`busy=false` 时调用方应直接退出，不多问。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuitSurvey {
    /// 正在跑的会话（agent_id）；单任务闸下最多一个。
    pub active_session: Option<String>,
    pub running_jobs: Vec<QuitJob>,
    pub running_terminals: usize,
    pub pending_approvals: usize,
    pub busy: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuitJob {
    pub id: String,
    /// 所属会话 id；无主作业为 None。
    pub session_id: Option<String>,
    pub label: String,
    pub status: String,
}

/// 盘点结果装配：`busy` 的判定只在这里，避免托盘与命令各写一套“算不算在跑”。
fn assemble_quit_survey(
    active_session: Option<String>,
    running_jobs: Vec<QuitJob>,
    running_terminals: usize,
    pending_approvals: usize,
) -> QuitSurvey {
    let busy = active_session.is_some()
        || !running_jobs.is_empty()
        || running_terminals > 0
        || pending_approvals > 0;
    QuitSurvey {
        active_session,
        running_jobs,
        running_terminals,
        pending_approvals,
        busy,
    }
}

/// 盘点当前在跑的任务/作业/终端/待审批。
fn quit_survey_of(app: &tauri::AppHandle) -> QuitSurvey {
    let state = app.state::<AppState>();
    let running_jobs: Vec<QuitJob> = state
        .jobs
        .list_running()
        .into_iter()
        .map(|job| QuitJob {
            id: job.id,
            session_id: job.owner,
            label: job.label,
            status: job.status,
        })
        .collect();
    let running_terminals = app
        .try_state::<user_terminal::UserTerminalManager>()
        .map(|manager| manager.active_count())
        .unwrap_or(0);
    let pending_approvals = state.broker.pending_count();
    assemble_quit_survey(
        state.active_agent(),
        running_jobs,
        running_terminals,
        pending_approvals,
    )
}

/// 关窗提示里「还有多少事在跑」的唯一口径（`CloseRequested` 与托盘菜单「后台运行说明」共用；
/// 退出盘点走 `QuitSurvey::busy`，不用这个计数）。
///
/// 只数会随窗口一起消失、但进程仍在替它干活的东西：活动会话、后台作业、用户终端。
/// 待审批不计入——它挂在某个在跑的会话或作业上，数进去会重复。
fn tray_running_count(survey: &QuitSurvey) -> usize {
    usize::from(survey.active_session.is_some())
        + survey.running_jobs.len()
        + survey.running_terminals
}

/// 强制中止要写进哪些会话的档案：活动会话 + 仍在跑的作业所属会话，去重保序。
fn forced_abort_targets(active_session: Option<&str>, job_sessions: &[Option<String>]) -> Vec<String> {
    let mut targets: Vec<String> = Vec::new();
    if let Some(session_id) = active_session {
        targets.push(session_id.to_string());
    }
    for owner in job_sessions.iter().flatten() {
        if !targets.contains(owner) {
            targets.push(owner.clone());
        }
    }
    targets
}

/// 限时等待“任务全部收口”。返回 `(是否超时, 实际等待毫秒)`。
///
/// 抽成纯等待是为了能测：撤掉等待（直接返回超时/不超时）或去掉轮询，
/// 反向对照必须红。
async fn wait_until_idle<F>(mut idle: F, limit: std::time::Duration) -> (bool, u64)
where
    F: FnMut() -> bool,
{
    let started = std::time::Instant::now();
    while started.elapsed() < limit {
        if idle() {
            return (false, started.elapsed().as_millis() as u64);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    (!idle(), started.elapsed().as_millis() as u64)
}

#[tauri::command]
fn quit_survey(app: tauri::AppHandle) -> QuitSurvey {
    quit_survey_of(&app)
}

/// 隐藏主窗口（退出对话框的“收回托盘”）。
#[tauri::command]
fn hide_main_window(app: tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.hide();
    }
}

/// 把“因退出而强制中止”写进该会话档案。
///
/// 失败只记日志、不拦退出：这时进程已经在收尾，档案写不进去也不能把用户卡住。
fn record_forced_abort_at(data_root: &std::path::Path, session_id: &str, reason: &str) {
    let path = data_root
        .join(rollout::ROLLOUT_DIR)
        .join(format!("{session_id}.jsonl"));
    match rollout::RolloutWriter::open_existing(&path) {
        Ok(writer) => {
            if let Err(error) = writer.append(&rollout::RolloutItem::AbortNote {
                reason: reason.to_string(),
                at_ms: now_ms(),
            }) {
                tracing::error!(
                    session_id,
                    "强制中止留痕写入档案失败（不拦退出），路径 {}: {error}",
                    path.display()
                );
            }
        }
        Err(error) => {
            tracing::error!(
                session_id,
                "强制中止留痕打开档案失败（不拦退出），路径 {}: {error}",
                path.display()
            );
        }
    }
}

/// 取消任务并退出：先走正常取消路径，限时等待；超时才强制退出并留痕。
///
/// 为什么不能直接 `app.exit(0)`：那样在跑的回合会以“非取消形态”断在半路，
/// 档案里出现一条没人能解释的中断。先发取消信号，让回合走到自己的取消分支。
#[tauri::command]
async fn quit_after_cancel(app: tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let survey = quit_survey_of(&app);
    let cancelled_approvals = state.broker.cancel_all();
    let cancelled_task = state.cancel_active();
    let job_ids: Vec<String> = state
        .jobs
        .list_running()
        .into_iter()
        .map(|job| job.id)
        .collect();
    for job_id in &job_ids {
        state.jobs.kill_unscoped(job_id);
    }
    tracing::info!(
        active_session = ?survey.active_session,
        cancelled_task,
        cancelled_approvals,
        jobs = ?job_ids,
        terminals = survey.running_terminals,
        "退出前已发出取消信号，等待任务收口（上限 3 秒）"
    );

    let started = std::time::Instant::now();
    let limit = std::time::Duration::from_millis(3000);
    let (timed_out, _waited_ms) = wait_until_idle(
        || !state.has_active_task() && state.jobs.list_running().is_empty(),
        limit,
    )
    .await;

    let still_running_jobs = state.jobs.list_running();
    if timed_out {
        let waited = started.elapsed().as_millis() as u64;
        tracing::warn!(
            waited_ms = waited,
            active_session = ?state.active_agent(),
            jobs = ?still_running_jobs.iter().map(|job| job.id.as_str()).collect::<Vec<_>>(),
            "退出时等待任务取消超时，强制中止（已写入强制中止记录）"
        );
        let reason = format!("因退出而强制中止：取消等待超时（等待 {waited} ms，上限 3000 ms）");
        let data_root = state.data_root();
        let job_sessions: Vec<Option<String>> = still_running_jobs
            .iter()
            .map(|job| job.owner.clone())
            .collect();
        for session_id in forced_abort_targets(state.active_agent().as_deref(), &job_sessions) {
            record_forced_abort_at(&data_root, &session_id, &reason);
        }
    } else {
        tracing::info!(
            waited_ms = started.elapsed().as_millis() as u64,
            "任务已在等待窗口内收口，正常退出"
        );
    }

    // SH-021：退出前先还原 DACL 并删除容器 profile，避免残留 ACE/悬空 profile。
    sandbox::shutdown();
    app.exit(0);
    Ok(())
}

/// `send_task` / `resume_session` 的失败形态。
///
/// 视觉门禁是**结构化对象**（前端按 `code` 分支），其余既有错误保持字符串——
/// `untagged` 让「任务为空」「未知沙箱档」这类文案原样到达前端，不为一个新门禁
/// 把所有调用面的错误形状都改一遍。
#[derive(Debug, serde::Serialize)]
#[serde(untagged)]
pub enum SendTaskError {
    NoVision(ModelNoVision),
    Plain(String),
}

/// 带图但当前模型看不了图：不写档案、不开回合，直接让用户换模型。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelNoVision {
    pub code: &'static str,
    pub model: String,
    pub message: String,
}

impl std::fmt::Display for SendTaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoVision(inner) => f.write_str(&inner.message),
            Self::Plain(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for SendTaskError {}

impl From<String> for SendTaskError {
    fn from(message: String) -> Self {
        Self::Plain(message)
    }
}

impl From<&str> for SendTaskError {
    fn from(message: &str) -> Self {
        Self::Plain(message.to_string())
    }
}

/// `send_task` / `resume_session` 的图片入参：前端（浏览器）侧解码后的声明。
///
/// 这些值**只当线索**：真正的 mime / 尺寸 / 字节数以磁盘文件内容为准，
/// 核对不一致直接拒绝（见 [`resolve_attachment_images`]）。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageAttachmentInput {
    pub attachment_id: String,
    pub media_type: String,
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub name: Option<String>,
}

/// 把附件回执读成可入请求的 `ImageRef`，并把字节放进 `image_store`。
///
/// 放在组合根而不是 `images`/`attachments`：FF-1 的允许边表里 `images` 不能指向
/// `attachments` 或 `tools`，这里两样都要用，属于组合根的职责。
///
/// 核对顺序：字节数 → 文件头格式 → 尺寸。任何一项与声明不符都拒绝，
/// 绝不「先用声明值发出去再说」——声明错了，端点只会回一个难懂的 400。
fn resolve_attachment_images(
    data_root: &Path,
    session_id: &str,
    inputs: &[ImageAttachmentInput],
) -> Result<Vec<images::ImageRef>, String> {
    let mut refs = Vec::with_capacity(inputs.len());
    for input in inputs {
        let (_, bytes) = attachments::read_full(data_root, session_id, &input.attachment_id)?;
        if input.bytes != bytes.len() as u64 {
            return Err(format!(
                "附件 {} 字节数与声明不一致（实际 {}，声明 {}）",
                input.attachment_id,
                bytes.len(),
                input.bytes
            ));
        }
        let actual = images::sniff_image(&bytes).ok_or_else(|| {
            format!(
                "附件 {} 不是受支持的图片（仅 PNG/JPEG/GIF/WEBP）",
                input.attachment_id
            )
        })?;
        if normalize_mime(&input.media_type) != actual.mime {
            return Err(format!(
                "附件 {} 的实际格式是 {}，与声明的 {} 不一致",
                input.attachment_id, actual.mime, input.media_type
            ));
        }
        if actual.width != input.width || actual.height != input.height {
            return Err(format!(
                "附件 {} 的实际尺寸是 {}×{}，与声明的 {}×{} 不一致",
                input.attachment_id, actual.width, actual.height, input.width, input.height
            ));
        }
        let hash = images::sha256_hex(&bytes);
        images::image_store::put(&hash, actual.mime, &bytes);
        refs.push(images::ImageRef {
            path: format!("attachments/{session_id}/{}.bin", input.attachment_id),
            sha256: hash,
            width: actual.width,
            height: actual.height,
            tokens: tools::view_image::estimate_tokens(actual.width, actual.height),
            mime: actual.mime.to_string(),
            attachment_id: Some(input.attachment_id.clone()),
            bytes: Some(bytes.len() as u64),
        });
    }
    Ok(refs)
}

/// 只统一大小写/参数与非标准别名；不做「猜格式」，猜的部分交给文件头。
fn normalize_mime(raw: &str) -> String {
    let base = raw
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match base.as_str() {
        "image/jpg" | "image/pjpeg" => "image/jpeg".to_string(),
        other => other.to_string(),
    }
}

/// 按 `ImageRef.path` 从附件目录读回字节。
///
/// 只认 `<data_root>/attachments/<owner_session>/<receipt>.bin` 这一段相对路径，
/// **不收当前会话 id**：`fork_session` 复制出的引用仍指向父会话目录（spec §5.2）。
/// 路径形状不对就返回 `None`（不猜、不拼、不跟 `..`）。
fn read_archived_image(data_root: &Path, image: &images::ImageRef) -> Option<Vec<u8>> {
    let receipt = image.attachment_id.as_deref()?;
    let mut components = Path::new(&image.path).components();
    if components.next()?.as_os_str() != "attachments" {
        return None;
    }
    let owner = match components.next()? {
        std::path::Component::Normal(name) => name.to_str()?,
        _ => return None,
    };
    let file = match components.next()? {
        std::path::Component::Normal(name) => name.to_str()?,
        _ => return None,
    };
    if components.next().is_some() || file != format!("{receipt}.bin") {
        return None;
    }
    attachments::read_full(data_root, owner, receipt)
        .ok()
        .map(|(_media_type, bytes)| bytes)
}

/// 重开会话时把历史图片的字节补回 `image_store`（生产调用点：`continue_session`）。
///
/// 单独暴露成函数是为了让集成测试走同一条生产路径，而不是在测试里复制一份读盘逻辑。
pub fn rehydrate_attachments(data_root: &Path, refs: &[images::ImageRef]) -> (usize, usize) {
    images::rehydrate(refs, |image| read_archived_image(data_root, image))
}

/// 提交前的纯判据：剥 marker → 空任务判定 → 视觉门禁。
///
/// 门禁必须发生在 `build_new_session` / `begin_task` 之前：让请求发出去再等端点
/// 回 400，既慢又把「模型看不了图」说成了「服务端错误」。
fn preflight_send(
    state: &AppState,
    task: &str,
    images: &[ImageAttachmentInput],
) -> Result<String, SendTaskError> {
    let stripped = images::strip_markers(task);
    if stripped.trim().is_empty() && images.is_empty() {
        return Err("任务为空".into());
    }
    let active = state.config().effective_model();
    vision_gate_for(&active.model, active.vision, images)?;
    Ok(stripped)
}

/// 视觉门禁本体（纯函数，便于两个方向都有断言）。
fn vision_gate_for(
    model: &str,
    vision: bool,
    images: &[ImageAttachmentInput],
) -> Result<(), SendTaskError> {
    if images.is_empty() || vision {
        return Ok(());
    }
    Err(SendTaskError::NoVision(ModelNoVision {
        code: "MODEL_NO_VISION",
        model: model.to_string(),
        message: format!("当前模型 {model} 不支持图片输入，请换成支持视觉的模型后再发送图片"),
    }))
}

/// 提交任务，并返回**本次使用的 agent_id**。
///
/// `agent_id` 为 None = 新会话（现场建档）；为 Some = 续写那份档案（SH-033 的
/// 「打开历史会话后继续输入」）。返回值是必需的而不是顺手加的：新会话的 id
/// 只有后端知道（`next_session_id` 在这里生成），前端拿不到它就无从决定
/// 下一次提交该续写谁——会话列表也就永远点不回自己刚建的那条。
#[tauri::command]
fn send_task(
    app: tauri::AppHandle,
    task: String,
    agent_id: Option<String>,
    sandbox_mode: Option<String>,
    images: Option<Vec<ImageAttachmentInput>>,
) -> Result<String, SendTaskError> {
    let images = images.unwrap_or_default();
    // 只接受已知档位；拼错就拒绝，不静默回落到更宽的档（§11.3 退化红线）。
    let sandbox = match sandbox_mode.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        Some(raw) => Some(
            config::SandboxMode::parse(raw)
                .ok_or_else(|| format!("未知沙箱档: {raw}"))?,
        ),
        None => None,
    };
    let state = app.state::<AppState>();
    let task = preflight_send(&state, &task, &images)?;
    match agent_id.filter(|id| !id.trim().is_empty()) {
        Some(id) => {
            continue_session(&app, &state, id.clone(), &task, sandbox, &images)
                .map_err(SendTaskError::Plain)?;
            Ok(id)
        }
        None => {
            // 新会话的档案 id 由后端生成，而附件是按会话 id 落盘的；没有 id 就没有
            // 附件目录可读，别把「找不到图」伪装成上传失败。
            if !images.is_empty() {
                return Err("带图片的任务需要先创建会话".into());
            }
            let (agent, cancel, new_id) =
                build_new_session(&state, state.workspace_root().as_path(), sandbox)?;
            // 首条消息 = 标题自动生成的触发点（first-prompt 节奏）：后台辅助请求，
            // 不阻塞主回合；失败/关开关/没密钥都静默保持截断 fallback。
            // 传的是剥掉 marker 的文本：标题生成也是「发给模型的文本」。
            session_title::spawn_generation(app.clone(), new_id.clone(), task.clone());
            let sink: Arc<dyn events::EventSink> =
                Arc::new(events::TauriEventSink::root(app.clone(), new_id.clone()).with_journal(Arc::new(event_journal::FileEventJournal::new(app.state::<AppState>().data_root().join(config::SESSIONS_DIR)))).with_broadcast(app.state::<gateway::GatewayHub>().broadcast_sender()).with_viewers(app.state::<gateway::GatewayHub>().viewers()));
            // 注册表在任务启动时挂到 AppState：UI 的 steering/中断命令面要摸到活体注册表。
            let orchestrator = build_orchestrator(agent.clone());
            app.state::<AppState>().set_subagents(Some(orchestrator.registry().clone()));
            tauri::async_runtime::spawn(async move {
                // Drop 守卫：正常结束、取消、panic 三条路径都释放任务槽（审计 H2）。
                let _task_slot =
                    app_state::TaskSlotGuard::new(move || app.state::<AppState>().end_task());
                let _ = agent
                    .run_task_with_orchestrator(&task, sink, cancel, Some(orchestrator))
                    .await;
            });
            Ok(new_id)
        }
    }
}

/// 续写一份既有档案——`resume_session` 与 `send_task(Some(id))` 的共用主体。
///
/// 恢复的三条硬要求都在这里成立：档案按 agent_id 定位、续跑**追加**到同一份档案
/// （`open_existing`，不重新建档）、已落盘条数作为写入起点（`persisted`，避免把旧历史
/// 重写一遍）。历史合法性由 `rollout::resume` 的重放规则保证；这里不再"修"历史——
/// 静默补配对会掩盖档案损坏，而档案损坏必须由上层显式处理。
///
/// `task` 必须是**已剥 marker** 的文本（两个入口都先过 `preflight_send`）。
fn continue_session(
    app: &tauri::AppHandle,
    state: &AppState,
    agent_id: String,
    task: &str,
    draft_sandbox: Option<config::SandboxMode>,
    images: &[ImageAttachmentInput],
) -> Result<(), String> {
    // 先解析附件：路径/尺寸/媒体类型都以磁盘内容为准，坏附件在读档案前就拒绝。
    let image_refs = resolve_attachment_images(&state.data_root(), &agent_id, images)?;
    let path = state
        .data_root()
        .join(rollout::ROLLOUT_DIR)
        .join(format!("{agent_id}.jsonl"));
    let resumed = rollout::resume::resume(&path).map_err(|e| format!("恢复会话失败: {e}"))?;
    let persisted = resumed.history.len();
    // first-prompt 资格看**追加前**的历史：档案里从没有过用户消息、又不是分叉，
    // 这条新任务才是"首条合格消息"（崩溃恢复/续跑/分叉都不重新生成标题）。
    if session_title::eligible(resumed.parent_id.as_deref(), &resumed.history) {
        session_title::spawn_generation(app.clone(), agent_id.clone(), task.to_string());
    }
    let mut history = resumed.history;
    // 重开档案时把历史图片的字节补回进程缓存：档案只存引用，字节在附件目录。
    // 按引用自带的 `path` 定位（fork 后仍指向父会话目录），不按当前 agent_id 拼。
    let archived_refs: Vec<images::ImageRef> = history
        .iter()
        .filter_map(|item| match item {
            model_client::HistoryItem::ImageMessage { images, .. } => Some(images.as_slice()),
            _ => None,
        })
        .flatten()
        .cloned()
        .collect();
    let (restored, failed) = rehydrate_attachments(&state.data_root(), &archived_refs);
    if restored > 0 || failed > 0 {
        tracing::info!(
            agent_id = %agent_id,
            restored,
            failed,
            "重开会话：历史图片字节已回灌（失败只影响该张图）"
        );
    }
    if image_refs.is_empty() {
        history.push(model_client::HistoryItem::Message {
            role: model_client::Role::User,
            text: task.to_string(),
            reasoning: None,
        });
    } else {
        history.push(model_client::HistoryItem::ImageMessage {
            role: model_client::Role::User,
            text: task.to_string(),
            images: image_refs,
        });
    }

    // resume 复检：档案里的旧根可能指向旧版数据根/它的祖先，按**当前**数据根重判。
    // 命中即 fail-closed（`resume_workspace`），不拿它跑任务，也不静默改成默认根
    // ——那会让用户以为还在原目录。
    let archived_workspace = PathBuf::from(&resumed.workspace_root);
    let session_workspace = if archived_workspace.as_os_str().is_empty() {
        archived_workspace
    } else {
        state.resume_workspace(&archived_workspace)?
    };
    // 会话档位：档案里的最后一条事件优先；草稿态（还没建档时先切了档）次之；
    // 都没有就跟随部署默认。
    // 取走草稿（无论是否采用都清掉，避免陈旧选择贴到以后的同名任务上）。
    let host_draft = state.take_draft_sandbox(&agent_id);
    let session_sandbox = resumed
        .sandbox_mode
        .as_deref()
        .and_then(config::SandboxMode::parse)
        .or(draft_sandbox)
        .or(host_draft);
    let writer = rollout::RolloutWriter::open_existing(&resumed.path)
        .map_err(|e| format!("档案续写打不开: {e}"))?;
    let (agent, cancel) = build_agent_with(
        state,
        &agent_id,
        if session_workspace.as_os_str().is_empty() { state.workspace_root() } else { session_workspace },
        Some(Arc::new(writer)),
        session_sandbox,
    )?;
    let sink: Arc<dyn events::EventSink> = Arc::new(events::TauriEventSink::root(app.clone(), agent_id).with_journal(Arc::new(event_journal::FileEventJournal::new(app.state::<AppState>().data_root().join(config::SESSIONS_DIR)))).with_broadcast(app.state::<gateway::GatewayHub>().broadcast_sender()).with_viewers(app.state::<gateway::GatewayHub>().viewers()));
    // spawn 的 future 必须是 'static：借用的 app 只能换成 owned 句柄再 move 进去。
    let app_handle = app.clone();
    let orchestrator = build_orchestrator(agent.clone());
    app.state::<AppState>().set_subagents(Some(orchestrator.registry().clone()));
    tauri::async_runtime::spawn(async move {
        // Drop 守卫：正常结束、取消、panic 三条路径都释放任务槽（审计 H2）。
        let _task_slot = app_state::TaskSlotGuard::new(move || {
            app_handle.state::<AppState>().end_task()
        });
        let _ = agent
            .run_with_history_and_orchestrator(history, persisted, sink, cancel, Some(orchestrator))
            .await;
    });
    Ok(())
}

/// 重启后恢复会话并续跑新任务（SH-031）。
///
/// 与 `send_task(Some(id))` 同一条主体：marker 剥离、空任务判定、视觉门禁
/// 都在 `preflight_send` 里，别在两条入口各写一份。
#[tauri::command]
fn resume_session(
    app: tauri::AppHandle,
    agent_id: String,
    task: String,
    images: Option<Vec<ImageAttachmentInput>>,
) -> Result<(), SendTaskError> {
    let images = images.unwrap_or_default();
    let state = app.state::<AppState>();
    let task = preflight_send(&state, &task, &images)?;
    continue_session(&app, &state, agent_id, &task, None, &images)
        .map_err(SendTaskError::Plain)
}

/// 会话列表（SH-033）：档案目录里的摘要，按更新时间倒序。
///
/// 返回空列表而不是错误：档案目录不存在是**首次运行的正常状态**，
/// 把它当错误会让前端在干净安装上必须写一条特判分支，而那条分支什么也修不了。
#[tauri::command]
fn list_sessions(app: tauri::AppHandle) -> Vec<rollout::SessionSummary> {
    let state = app.state::<AppState>();
    rollout::list(&state.data_root().join(rollout::ROLLOUT_DIR))
}

/// 会话内切档（`/permission` 的宿主面，对齐 DSH 的 `sandbox/mode` 会话事件）。
///
/// 只写**本会话档案**，绝不写 `config.toml`——这是会话内切换与设置页默认值的分界线。
/// 重进会话靠档案里最后一条事件回放当前档位，进程重启也不丢。
#[tauri::command]
fn set_session_sandbox_mode(
    app: tauri::AppHandle,
    session_id: String,
    mode: String,
) -> Result<String, String> {
    let state = app.state::<AppState>();
    let parsed = config::SandboxMode::parse(mode.trim())
        .ok_or_else(|| format!("未知沙箱档: {mode}"))?;
    let path = state
        .data_root()
        .join(rollout::ROLLOUT_DIR)
        .join(format!("{session_id}.jsonl"));
    if path.is_file() {
        let writer = rollout::RolloutWriter::open_existing(&path)
            .map_err(|e| format!("会话档案打不开: {e}"))?;
        writer
            .append(&rollout::RolloutItem::SandboxMode {
                mode: parsed.as_str().to_string(),
                at_ms: now_ms(),
            })
            .map_err(|e| format!("写入会话档位失败: {e}"))?;
    } else {
        // 还没建档（输入框里先切档）：贴到会话 id 上，首条任务落盘时写进档案。
        state.put_draft_sandbox(&session_id, parsed);
    }
    // 只有目标就是当前跑着的会话时才动运行时快照；别的会话等它自己启动时再装载，
    // 否则 A 会话的切档会借共享句柄泄进 B（§4.2 反向对照要证的就是这条）。
    if state.active_agent().as_deref() == Some(session_id.as_str()) {
        state.policy.set_sandbox(parsed);
    }
    Ok(parsed.as_str().to_string())
}

/// 读会话当前档位（未建档 / 从没切过 → None，前端回落部署默认）。
#[tauri::command]
fn session_sandbox_mode(app: tauri::AppHandle, session_id: String) -> Option<String> {
    let state = app.state::<AppState>();
    let path = state
        .data_root()
        .join(rollout::ROLLOUT_DIR)
        .join(format!("{session_id}.jsonl"));
    rollout::last_sandbox_mode(&path)
}

/// 读一页会话事件信封（SH-094 批 1）：`after_event_id` 之后、最多 `limit` 条。
///
/// 这是续播的原语：客户端重连后带最后一条 `event_id` 再取，`has_more` 为真时继续。
#[tauri::command]
fn read_session_events(
    app: tauri::AppHandle,
    session_id: String,
    after_event_id: Option<String>,
    limit: Option<usize>,
) -> Result<event_journal::EventPage, String> {
    let state = app.state::<AppState>();
    event_journal::read_after(
        &state.data_root().join(config::SESSIONS_DIR),
        &session_id,
        after_event_id.as_deref(),
        limit.unwrap_or(event_journal::DEFAULT_LIMIT),
    )
}

/// 某会话事件日志里最新的一条 `event_id`（SH-129 M3）。
///
/// 用途：事件通道的订阅引用归零、但这次订阅一条事件都没收到时，没有游标可续。
/// 让它向宿主取一个「当前最新」当游标（语义是"这之前都算看过了"），这样既能正常
/// 退订、又能保证重进时补齐期间的帧；取不到就如实返回 `None`，由调用方降级为全量补播。
#[tauri::command]
fn latest_session_event_id(
    app: tauri::AppHandle,
    session_id: String,
) -> Result<Option<String>, String> {
    let state = app.state::<AppState>();
    event_journal::latest_event_id(&state.data_root().join(config::SESSIONS_DIR), &session_id)
}

/// 反查子代理事件归属的父会话（SH-056 批 2）。
///
/// 重启后前端直接打开泳道时只有泳道 id（子代理的 `agent_id`）；子代理没有独立
/// rollout 档案，靠这条命令找回父会话再整树回放。没有命中如实返回 `null`——
/// 让前端走空态，而不是把"找不到"包装成一个假的会话 id。
#[tauri::command]
fn session_of_agent(app: tauri::AppHandle, agent_id: String) -> Result<Option<String>, String> {
    let state = app.state::<AppState>();
    event_journal::owner_session_of_agent(
        &state.data_root().join(config::SESSIONS_DIR),
        &agent_id,
    )
}

/// gateway 连接信息（SH-094 批 2，控制面）：前端据此连 `ws://127.0.0.1:{port}` 并以 token 鉴权。
/// token 是 loopback 传输鉴权串（非 API Key），仅经此特权 invoke 通道交给本应用前端，不落日志。
#[derive(serde::Serialize)]
struct GatewayInfo {
    port: u16,
    token: String,
}

#[tauri::command]
fn gateway_info(app: tauri::AppHandle) -> Result<GatewayInfo, String> {
    let hub = app.state::<gateway::GatewayHub>();
    let port = hub.port();
    if port == 0 {
        return Err("WebSocket gateway 尚未绑定端口".into());
    }
    Ok(GatewayInfo { port, token: hub.token().to_string() })
}

/// 暂存一份会话附件（SH-116 批 1，桌面壳文件上传的宿主面）。
/// 数据落在 `<data_root>\attachments\<session>\`，不写 rollout；回执只在本会话有效。
#[tauri::command]
fn upload_attachment(
    app: tauri::AppHandle,
    session_id: String,
    name: String,
    media_type: String,
    data: String,
    width: Option<u32>,
    height: Option<u32>,
) -> Result<attachments::StoredAttachment, String> {
    let state = app.state::<AppState>();
    attachments::store(&state.data_root(), &session_id, &name, &media_type, &data, width, height)
}

/// 读回附件字节窗口（prompt 拼装用）；只接受本会话暂存过的回执。
#[tauri::command]
fn read_attachment(
    app: tauri::AppHandle,
    session_id: String,
    receipt_id: String,
    max_bytes: Option<usize>,
) -> Result<attachments::AttachmentRead, String> {
    let state = app.state::<AppState>();
    attachments::read(
        &state.data_root(),
        &session_id,
        &receipt_id,
        max_bytes.unwrap_or(attachments::DEFAULT_READ_BYTES),
    )
}

/// 打开一个会话所需的历史条目。
///
/// **为什么不复用事件流重放**：`EventMsg` 里没有用户消息事件——用户消息历来是前端
/// `submitUserTask` 本地生成的。若为了重放新增一个事件变体，就得动受保护的 `events/`
/// 并牵动 FF-2（EventMsg tag ↔ types.ts 的机械绑定）。而重放的语义本来就是
/// 「一次性数据加载」而不是「流」，用 command 返回值更贴切，且对协议零侵入。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEntry {
    Message {
        role: String,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
        /// 事件落档时刻；`0` = 旧档案未记录，前端必须标"记录时刻不可信"（A0-4）。
        #[serde(default)]
        at_ms: u64,
        /// 用户附件图片的结构化引用（只有 ImageMessage 有；旧档案没有这个键）。
        /// 展示与重放都读它，而不是把图片压成正文里的 `[图片: …]` 文本。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<images::ImageRef>,
    },
    ToolCall {
        call_id: String,
        tool: String,
        arguments: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        /// 事件落档时刻；`0` = 旧档案未记录（A0-4）。
        #[serde(default)]
        at_ms: u64,
    },
    /// 回合结局（A0-3）。旧档案没有这一条，导出侧必须标 `unknown` 而不是成功。
    TurnEnd {
        turn: u32,
        reason: String,
        at_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<events::TurnErrorInfo>,
    },
}

/// 档案历史 → 展示条目。
///
/// 工具输出在档案里排在**同轮全部调用之后**（"先全部 call、再全部 output"，SH-031 的
/// `nearest_safe_cut` 正是靠这个形状才敢只在配对边界截断）。展示要按调用顺序把输出带上，
/// 所以先建一遍 `call_id → output` 索引，再按原序输出，`FunctionCallOutput` 本身不再成条。
fn project_entries(items: &[rollout::RolloutItem]) -> Result<Vec<SessionEntry>, String> {
    let mut slots = project_slots(items);
    // 锚点必须按记录顺序、对**当时**的槽位序列生效——与 `rollout::resume` 的
    // `history.splice(start..end, [summary])` 逐条对齐，否则同一份档案在
    // 「模型看到的历史」与「用户看到的视图」会分叉。
    for item in items {
        if let rollout::RolloutItem::Compacted { range_start, range_end, summary } = item {
            apply_compaction(
                &mut slots,
                *range_start,
                *range_end,
                summary_entry(summary),
            )?;
        }
    }

    let mut outputs: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for slot in &slots {
        if let Slot::Output { call_id, output } = slot {
            outputs.insert(call_id.clone(), output.clone());
        }
    }

    let mut out = Vec::with_capacity(slots.len());
    for slot in slots {
        match slot {
            Slot::Message(entry) | Slot::Summary(entry) | Slot::Audit(entry) => out.push(entry),
            Slot::Call { call_id, tool, arguments, at_ms } => out.push(SessionEntry::ToolCall {
                output: outputs.get(&call_id).cloned(),
                call_id,
                tool,
                arguments,
                at_ms,
            }),
            // 已并进对应的 ToolCall，不单独成条。
            Slot::Output { .. } => {}
        }
    }
    Ok(out)
}

/// 投影槽位：只有**会进历史序列**的条目才占压缩锚点的序号。
///
/// 锚点区间 `[range_start, range_end)` 的语义建立在 `rollout::resume` 重放出的
/// `history` 上，而那个序列里工具输出是独立一项、`TurnEnd` 之类审计记录不在其中。
/// 投影若用"档案行号"当序号，锚点就会指到别的条目上——比显示双份更糟的错位。
enum Slot {
    /// 普通消息（含图片消息）：投影即成品。
    Message(SessionEntry),
    /// 一次工具调用：输出在投影时按 `call_id` 补上。
    Call { call_id: String, tool: String, arguments: String, at_ms: u64 },
    /// 工具输出：占历史序号，但不单独成条。
    Output { call_id: String, output: String },
    /// 锚点把区间换成的摘要（取代区间后同样占一个历史序号）。
    Summary(SessionEntry),
    /// 回合结局：审计记录，不占序号，也不该被压缩区间吞掉。
    Audit(SessionEntry),
}

impl Slot {
    fn is_history(&self) -> bool {
        !matches!(self, Slot::Audit(_))
    }
}

/// 角色取 user：与 resume 回放、thinking 档协议三者同源（摘要没有
/// `reasoning_content`，以 assistant 身份喂端点会 400）。
fn summary_entry(summary: &str) -> SessionEntry {
    SessionEntry::Message {
        role: "user".to_string(),
        text: summary.to_string(),
        reasoning: None,
        // 压缩锚点没有独立时刻；取不到就标不可信，不借用导出时刻。
        at_ms: 0,
        images: Vec::new(),
    }
}

fn project_slots(items: &[rollout::RolloutItem]) -> Vec<Slot> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match item {
            rollout::RolloutItem::ResponseItem {
                item: model_client::HistoryItem::Message { role, text, reasoning },
                at_ms,
            } => out.push(Slot::Message(SessionEntry::Message {
                role: role_name(role),
                text: text.clone(),
                reasoning: reasoning.clone(),
                at_ms: *at_ms,
                images: Vec::new(),
            })),
            rollout::RolloutItem::ResponseItem {
                item: model_client::HistoryItem::FunctionCall { name, arguments, call_id },
                at_ms,
            } => out.push(Slot::Call {
                call_id: call_id.clone(),
                tool: name.clone(),
                arguments: arguments.clone(),
                at_ms: *at_ms,
            }),
            rollout::RolloutItem::ResponseItem {
                item: model_client::HistoryItem::FunctionCallOutput { call_id, output },
                ..
            } => out.push(Slot::Output { call_id: call_id.clone(), output: output.clone() }),
            // 压缩锚点本身在这里不产出条目：它的位置与内容由 `apply_compaction`
            // 在槽位序列上做替换（先收全部槽位、再按记录顺序应用锚点）。
            rollout::RolloutItem::Compacted { .. } => {}
            // 带图消息保留结构化引用：展示层自己决定怎么渲染图片卡片，
            // 重放也不再依赖正文里的历史 marker。字节永远不进档案。
            rollout::RolloutItem::ResponseItem {
                item: model_client::HistoryItem::ImageMessage { role, text, images },
                at_ms,
            } => out.push(Slot::Message(SessionEntry::Message {
                role: role_name(role),
                text: text.clone(),
                reasoning: None,
                at_ms: *at_ms,
                images: images.clone(),
            })),
            // 回合结局：如实带给导出/审计，不进对话历史。
            rollout::RolloutItem::TurnEnd { turn, reason, at_ms, error } => {
                out.push(Slot::Audit(SessionEntry::TurnEnd {
                    turn: *turn,
                    reason: reason.clone(),
                    at_ms: *at_ms,
                    error: error.clone(),
                }))
            }
            // 首行 meta 不是历史；子代理引用要等 SH-056 的泳道 UI 才有展示形态；
            // 档位事件是控制面记录，退出留痕是审计记录——都不进对话历史。
            // 系统提示词同样不进：它在轨迹视图里由 `system/message` 事件承载，
            // 这里再投影一份会造成两份真相。
            rollout::RolloutItem::SessionMeta { .. }
            | rollout::RolloutItem::SubagentRef { .. }
            | rollout::RolloutItem::SandboxMode { .. }
            | rollout::RolloutItem::AbortNote { .. }
            | rollout::RolloutItem::SystemPrompt { .. } => {}
        }
    }
    out
}

fn role_name(role: &model_client::Role) -> String {
    match role {
        model_client::Role::User => "user",
        model_client::Role::Assistant => "assistant",
        model_client::Role::System => "system",
    }
    .to_string()
}

/// 第 `n` 个历史槽位在槽位序列里的下标（`n` 等于历史长度时无对应项 = 追加到末尾）。
fn history_position(slots: &[Slot], n: usize) -> Option<usize> {
    slots
        .iter()
        .enumerate()
        .filter(|(_, slot)| slot.is_history())
        .nth(n)
        .map(|(index, _)| index)
}

/// 在槽位序列上重放一次压缩锚点：区间内的历史槽位被一条摘要取代。
///
/// 与 `rollout::resume` 的区间校验同口径：越界即报错，而不是"能显示多少显示多少"。
/// 档案里原文**不删**（有损操作要能追回去），但视图里不该再出现——否则用户会以为
/// 压缩没生效，而模型那边早已只看到摘要。
fn apply_compaction(
    slots: &mut Vec<Slot>,
    range_start: usize,
    range_end: usize,
    summary: SessionEntry,
) -> Result<(), String> {
    let history_len = slots.iter().filter(|slot| slot.is_history()).count();
    if range_start > range_end || range_end > history_len {
        return Err(format!(
            "压缩锚点区间 [{range_start},{range_end}) 超出已重放长度 {history_len}"
        ));
    }
    let Some(first) = history_position(slots, range_start) else {
        // 空档案上的空区间：等价于在末尾插一条摘要（resume 的 splice 同结果）。
        slots.push(Slot::Summary(summary));
        return Ok(());
    };
    let mut index = first;
    let mut remaining = range_end - range_start;
    while remaining > 0 {
        if slots[index].is_history() {
            slots.remove(index);
            remaining -= 1;
        } else {
            // 夹在区间里的 TurnEnd 是审计记录：不占序号，也不该被吞掉。
            index += 1;
        }
    }
    slots.insert(first, Slot::Summary(summary));
    Ok(())
}

/// 读回一个会话的历史条目，供前端渲染（SH-033 的"打开会话"）。
///
/// 走 `rollout::read` 的严格口径：档案中间坏行必须报错，绝不能给前端一份"看起来完整、
/// 实际缺尾"的历史——那正是 SH-030 设计选择 2 要防的事。
#[tauri::command]
fn load_session(app: tauri::AppHandle, agent_id: String) -> Result<Vec<SessionEntry>, String> {
    let state = app.state::<AppState>();
    let path = state
        .data_root()
        .join(rollout::ROLLOUT_DIR)
        .join(format!("{agent_id}.jsonl"));
    let archive = rollout::read(&path).map_err(|e| format!("档案读不回: {e}"))?;
    project_entries(&archive.items).map_err(|e| format!("档案读不回: {e}"))
}

/// 会话全文搜索（SH-113）：宿主侧 SQLite FTS5 派生索引。
///
/// 入参按 vendored wire 只有 `{query}`（DSH `session.search` 不送 cursor/limit，
/// 宿主不做续页）；返回 `{items: [{sessionId, snippet}], hasMore}`，其中 `hasMore`
/// 表示「命中会话数超过宿主固定上限 20，请收窄查询」，只驱动 UI 一行提示。
/// 宿主按 sessionId 去重、每会话只给最强命中的一条纯文本 snippet。索引落在
/// `<data_root>/cache/`，对 rollout 档案**只读零写入**；打开/查询/档案读失败一律
/// 给类型化 [`session_query::SearchError`]，绝不返回空表冒充"没搜到"。
#[tauri::command]
fn search_sessions(
    app: tauri::AppHandle,
    query: String,
) -> Result<session_query::SearchPage, session_query::SearchError> {
    let state = app.state::<AppState>();
    session_query::search(&state.data_root(), &query)
}

/// 手动重试会话搜索索引（SH-113）：清掉失败记忆，让下一次搜索真的重新打开索引。
///
/// 冷却只是「别让每次搜索都重撞一次墙」，不是「只能干等」——用户可能刚把磁盘清出来
/// 或解除了文件占用。界面上的「重试」入口走这里，随后重新发起一次搜索。
#[tauri::command]
fn retry_session_index() {
    session_query::retry_index_now();
}

/// 归档/置顶/排序操作的类型化错误（SH-100）：`code` 给前端按类映射 wire 错误码
/// （上游 workspace-controller types.ts 的同一批语义），措辞怎么改都不破坏形状。
#[derive(Debug, serde::Serialize)]
#[serde(tag = "code", content = "message", rename_all = "kebab-case")]
pub enum WorkspaceOpError {
    /// 目标会话正在跑任务：先停止，或携带 stopActivity 让宿主先取消再归档。
    SessionActive(String),
    /// 会话档案不存在。
    NotFound(String),
    /// 已归档的会话不能再置顶（上游按 gateway/bad-request 分派）。
    PinArchived(String),
    /// 排序移动的锚点会话不存在。
    MoveInvalid(String),
    /// 路径不合法（不存在/非目录/相对路径/重复注册）。
    InvalidPath(String),
    /// 改名与另一家工作区重名。
    NameConflict(String),
    /// 参数本身不合法（如给默认工作区做不支持的操作）。
    BadRequest(String),
    /// 其余失败（落盘 I/O）。
    Rejected(String),
}

fn session_exists(state: &AppState, id: &str) -> bool {
    state
        .data_root()
        .join(rollout::ROLLOUT_DIR)
        .join(format!("{}.jsonl", id.trim()))
        .is_file()
}

/// 归档会话（SH-100）。归档即从置顶集剔除（上游同一次持久化写里原子完成）；
/// 已归档幂等 no-op。运行中的会话：不带 stopActivity 拒绝，带了先取消再归档
/// （上游"先写归档集、再派发 stop"的顺序——单任务闸下取消即收口）。
#[tauri::command]
fn archive_session(
    app: tauri::AppHandle,
    session_id: String,
    stop_activity: Option<bool>,
) -> Result<serde_json::Value, WorkspaceOpError> {
    let state = app.state::<AppState>();
    let id = session_id.trim().to_string();
    if !session_exists(&state, &id) {
        return Err(WorkspaceOpError::NotFound(format!("会话档案不存在：{id}")));
    }
    if state.active_agent().as_deref() == Some(id.as_str()) {
        if stop_activity == Some(true) {
            state.cancel_active();
        } else {
            return Err(WorkspaceOpError::SessionActive(format!(
                "会话 {id} 正在运行：先停止任务，或携带 stopActivity 归档"
            )));
        }
    }
    let dir = state.data_root().join(rollout::ROLLOUT_DIR);
    let mut flags = rollout::read_session_order(&dir);
    if !flags.archived_session_ids.iter().any(|x| x == &id) {
        flags.archived_session_ids.push(id.clone());
    }
    flags.pinned_session_ids.retain(|x| x != &id);
    rollout::write_session_order(&dir, &flags).map_err(WorkspaceOpError::Rejected)?;
    Ok(serde_json::json!({ "archivedSessionIds": flags.archived_session_ids }))
}

/// 取消归档（SH-100）。幂等：会话回到其工作区槽位的原位置。
#[tauri::command]
fn unarchive_session(app: tauri::AppHandle, session_id: String) -> Result<serde_json::Value, WorkspaceOpError> {
    let state = app.state::<AppState>();
    let id = session_id.trim().to_string();
    let dir = state.data_root().join(rollout::ROLLOUT_DIR);
    let mut flags = rollout::read_session_order(&dir);
    flags.archived_session_ids.retain(|x| x != &id);
    rollout::write_session_order(&dir, &flags).map_err(WorkspaceOpError::Rejected)?;
    Ok(serde_json::json!({ "archivedSessionIds": flags.archived_session_ids }))
}

/// 置顶会话（SH-100）：prepend——置顶序 = 最近置顶在前（上游同口径）。
/// 已置顶幂等不重排；已归档拒绝（置顶集与归档集互斥）。
#[tauri::command]
fn pin_session(app: tauri::AppHandle, session_id: String) -> Result<serde_json::Value, WorkspaceOpError> {
    let state = app.state::<AppState>();
    let id = session_id.trim().to_string();
    if !session_exists(&state, &id) {
        return Err(WorkspaceOpError::NotFound(format!("会话档案不存在：{id}")));
    }
    let dir = state.data_root().join(rollout::ROLLOUT_DIR);
    let mut flags = rollout::read_session_order(&dir);
    if flags.archived_session_ids.iter().any(|x| x == &id) {
        return Err(WorkspaceOpError::PinArchived(format!(
            "会话 {id} 已归档：先取消归档才能置顶"
        )));
    }
    if !flags.pinned_session_ids.iter().any(|x| x == &id) {
        flags.pinned_session_ids.insert(0, id.clone());
    }
    rollout::write_session_order(&dir, &flags).map_err(WorkspaceOpError::Rejected)?;
    Ok(serde_json::json!({ "pinnedSessionIds": flags.pinned_session_ids }))
}

/// 取消置顶（SH-100）。
#[tauri::command]
fn unpin_session(app: tauri::AppHandle, session_id: String) -> Result<serde_json::Value, WorkspaceOpError> {
    let state = app.state::<AppState>();
    let id = session_id.trim().to_string();
    if !session_exists(&state, &id) {
        return Err(WorkspaceOpError::NotFound(format!("会话档案不存在：{id}")));
    }
    let dir = state.data_root().join(rollout::ROLLOUT_DIR);
    let mut flags = rollout::read_session_order(&dir);
    flags.pinned_session_ids.retain(|x| x != &id);
    rollout::write_session_order(&dir, &flags).map_err(WorkspaceOpError::Rejected)?;
    Ok(serde_json::json!({ "pinnedSessionIds": flags.pinned_session_ids }))
}

/// 手动排序（SH-100）：DOM-insertBefore 式移动——把 `session_id` 移到
/// `before_session_id` 之前；锚点省略 = 追加到末尾。顺序以侧车为准，现存但
/// 不在侧车里的会话按列表序补位（新会话出现在末尾，与上游 recency 补位同向）。
#[tauri::command]
fn insert_session_before(
    app: tauri::AppHandle,
    session_id: String,
    before_session_id: Option<String>,
) -> Result<serde_json::Value, WorkspaceOpError> {
    let state = app.state::<AppState>();
    let id = session_id.trim().to_string();
    let before = before_session_id
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(str::to_string);
    if !session_exists(&state, &id) {
        return Err(WorkspaceOpError::NotFound(format!("会话档案不存在：{id}")));
    }
    if let Some(anchor) = &before {
        if !session_exists(&state, anchor) {
            return Err(WorkspaceOpError::NotFound(format!("会话档案不存在：{anchor}")));
        }
    }
    let dir = state.data_root().join(rollout::ROLLOUT_DIR);
    // 与界面同源的单工作区账户：workspaceRoot 命中配置根的会话才参与排序
    let workspace = state.workspace_root().display().to_string();
    let mine: Vec<String> = rollout::list(&dir)
        .into_iter()
        .filter(|row| row.workspace_root == workspace)
        .map(|row| row.agent_id)
        .collect();
    let mut flags = rollout::read_session_order(&dir);
    let mut order: Vec<String> = flags
        .session_order
        .into_iter()
        .filter(|known| mine.contains(known))
        .collect();
    for id in &mine {
        if !order.contains(id) {
            order.push(id.clone());
        }
    }
    order.retain(|x| x != &id);
    match &before {
        Some(anchor) => {
            let pos = order
                .iter()
                .position(|x| x == anchor)
                .ok_or_else(|| WorkspaceOpError::MoveInvalid(format!("锚点会话 {anchor} 不在排序账户里")))?;
            order.insert(pos, id.clone());
        }
        None => order.push(id.clone()),
    }
    flags.session_order = order;
    rollout::write_session_order(&dir, &flags).map_err(WorkspaceOpError::Rejected)?;
    Ok(serde_json::json!({ "sessionOrder": flags.session_order }))
}

/// 默认工作区的 workspaceId（face 同款常量；配置根派生，不进 [[workspaces]]）。
pub const DEFAULT_WORKSPACE_ID: &str = "default-workspace";

/// 默认工作区的标题基线：`[workspace].title` 非空 = 用户标题，否则上游首启拼写
/// `default-workspace`（前端 `workspaceDisplayTitle` 据此渲染成「默认工作区」）。
/// 上游语义：registry 里它就是一个可写 title 的普通项，这里只把落点换成
/// `[workspace].title` 而不是 `[[workspaces]]` 行。
pub fn default_workspace_title(cfg: &config::AppConfig) -> String {
    let title = cfg.workspace.title.trim();
    if title.is_empty() {
        DEFAULT_WORKSPACE_ID.to_string()
    } else {
        title.to_string()
    }
}

/// 改名（SH-099）：只写 title，路径不动；与其它工作区（含默认项的标题基线）
/// 重名拒绝。纯函数——命令壳负责持久化与视图回显。
pub fn apply_workspace_rename(
    cfg: &config::AppConfig,
    workspace_id: &str,
    title: &str,
) -> Result<config::AppConfig, WorkspaceOpError> {
    let id = workspace_id.trim();
    let title = title.trim();
    if title.is_empty() {
        return Err(WorkspaceOpError::BadRequest("工作区名不能为空".into()));
    }
    let mut next = cfg.clone();
    if id == DEFAULT_WORKSPACE_ID {
        // 上游同构：默认工作区改名与别家一样生效，落 [workspace].title。
        if cfg
            .workspaces
            .iter()
            .any(|ws| ws.display_title() == title)
        {
            return Err(WorkspaceOpError::NameConflict(format!(
                "已有名为 {title} 的工作区，换一个名字"
            )));
        }
        next.workspace.title = title.to_string();
        return Ok(next);
    }
    if !cfg.workspaces.iter().any(|ws| ws.id == id) {
        return Err(WorkspaceOpError::NotFound(format!("工作区不存在：{id}")));
    }
    // 重名比对含默认项：默认项的标题基线也算一家。
    if cfg
        .workspaces
        .iter()
        .any(|ws| ws.id != id && ws.display_title() == title)
        || default_workspace_title(cfg) == title
    {
        return Err(WorkspaceOpError::NameConflict(format!(
            "已有名为 {title} 的工作区，换一个名字"
        )));
    }
    if let Some(ws) = next.workspaces.iter_mut().find(|ws| ws.id == id) {
        ws.title = title.to_string();
    }
    Ok(next)
}

/// 删除注册（SH-099）：只摘注册，目录与会话档案全保留——会话在侧栏落到
/// 「未分组」（上游同口径）。默认项是 `[workspace].root` 的注册形态，物理删除
/// 等于停用全局默认根，所以落 hidden 标记并重置标题；效果与上游 registry
/// 摘除一致，重新添加同一路径即复活。纯函数。
pub fn apply_workspace_delete(
    cfg: &config::AppConfig,
    workspace_id: &str,
) -> Result<config::AppConfig, WorkspaceOpError> {
    let id = workspace_id.trim();
    let mut next = cfg.clone();
    if id == DEFAULT_WORKSPACE_ID {
        next.workspace.hidden = true;
        next.workspace.title.clear();
        return Ok(next);
    }
    let before = next.workspaces.len();
    next.workspaces.retain(|ws| ws.id != id);
    if next.workspaces.len() == before {
        return Err(WorkspaceOpError::NotFound(format!("工作区不存在：{id}")));
    }
    Ok(next)
}

/// 注册表的派生顺序（SH-111）：可见默认项居首，其后 `[[workspaces]]` 数组序。
pub fn workspace_registry_ids(cfg: &config::AppConfig) -> Vec<String> {
    let mut ids = Vec::new();
    if !cfg.workspace.hidden {
        ids.push(DEFAULT_WORKSPACE_ID.to_string());
    }
    ids.extend(cfg.workspaces.iter().map(|ws| ws.id.clone()));
    ids
}

/// 当前生效顺序：`[workspace].order` 里仍在注册表的 id 按序优先（手写重复只认首次），
/// 未列出的按派生序补尾——与客户端 `installOrder` 的 rank 兜底同向。
pub fn workspace_order(cfg: &config::AppConfig) -> Vec<String> {
    let registry = workspace_registry_ids(cfg);
    let mut ordered: Vec<String> = Vec::new();
    for id in &cfg.workspace.order {
        if registry.contains(id) && !ordered.contains(id) {
            ordered.push(id.clone());
        }
    }
    for id in registry {
        if !ordered.contains(&id) {
            ordered.push(id);
        }
    }
    ordered
}

/// DOM-insertBefore 式移动（SH-111，上游 `registry.insertBefore` 逐条同构）：
/// 锚点省略 = 追加末尾；锚点即自身 = 原序返回；id 或锚点不在注册表 = not-found
/// （上游 `WorkspaceOrderInvalidError` 经 commands.ts 映射成 `workspace/not-found`）。
pub fn apply_workspace_order(
    cfg: &config::AppConfig,
    workspace_id: &str,
    before_workspace_id: Option<&str>,
) -> Result<Vec<String>, WorkspaceOpError> {
    let registry = workspace_registry_ids(cfg);
    let id = workspace_id.trim();
    if !registry.iter().any(|candidate| candidate == id) {
        return Err(WorkspaceOpError::NotFound(format!("工作区不存在：{id}")));
    }
    let current = workspace_order(cfg);
    if let Some(before) = before_workspace_id {
        if !registry.iter().any(|candidate| candidate == before) {
            return Err(WorkspaceOpError::NotFound(format!("工作区不存在：{before}")));
        }
        if before == id {
            return Ok(current);
        }
    }
    let without: Vec<String> = current.into_iter().filter(|candidate| candidate != id).collect();
    let at = match before_workspace_id {
        Some(before) => without.iter().position(|candidate| candidate == before).unwrap_or(without.len()),
        None => without.len(),
    };
    let mut next = without[..at].to_vec();
    next.push(id.to_string());
    next.extend_from_slice(&without[at..]);
    Ok(next)
}

/// 工作区拖拽排序持久化（SH-111）：顺序落 `[workspace].order`，回值 = **完整**
/// 注册表顺序（上游 `WorkspaceOrderValue` 契约）。默认项与别家一样可移动——
/// 上游 registry 顺序任意，恒置头部是单工作区时代的遗留。
#[tauri::command]
fn insert_workspace_before(
    app: tauri::AppHandle,
    workspace_id: String,
    before_workspace_id: Option<String>,
) -> Result<serde_json::Value, WorkspaceOpError> {
    let state = app.state::<AppState>();
    let cfg = state.config();
    let order = apply_workspace_order(&cfg, &workspace_id, before_workspace_id.as_deref())?;
    // 顺序没变就不写盘（上游 sameIds 短路同口径）
    if order != cfg.workspace.order {
        let mut next = cfg.clone();
        next.workspace.order = order.clone();
        state.update_config(next).map_err(WorkspaceOpError::Rejected)?;
    }
    Ok(serde_json::json!({ "workspaceIds": order }))
}

fn workspace_view_of(state: &AppState, id: &str, path: &str, title: &str) -> serde_json::Value {
    let rows = rollout::list(&state.data_root().join(rollout::ROLLOUT_DIR));
    let mine: Vec<String> = rows
        .into_iter()
        .filter(|row| row.workspace_root == path)
        .map(|row| row.agent_id)
        .collect();
    serde_json::json!({
        "workspaceId": id,
        "path": path,
        "title": title,
        "sessionIds": mine,
        "createdAt": "",
        "updatedAt": "",
    })
}

/// 注册表与会话 cwd 共用的规范拼写（SH-110 随批修复）：canonicalize 解析符号链接
/// 与相对段后，再还原 Windows `\\?\` verbatim 前缀（复用 SH-085 的 shell_usable）。
/// 不还原的话注册项存 verbatim、默认根存普通拼写：同一目录两种字符串，会话按
/// cwd 字符串归属会错组，侧栏 tooltip 也会露出 `\\?\`。比较与落盘同用本函数。
fn canonical_workspace_path(path: &std::path::Path) -> Option<String> {
    path.canonicalize()
        .ok()
        .map(|p| tool_router::shell_usable(&p).to_string_lossy().to_string())
}

/// 注册工作区（SH-099）：路径必须已存在且是目录。与**可见**默认项或已注册项
/// 同路径 = 幂等返回那一家（上游 created:false 同口径）；id 由宿主生成，
/// title 缺省 = 目录名。被删除过的默认项（`hidden`）不复活，而是走全新注册拿
/// 新 id（SH-110）——上游 `ClientWorkspaceModel.removedIds` 的不变式是「宿主 id
/// 永不复用」，复用固定 `default-workspace` 会让同会话内的 upsert 与 baseline
/// 全被静默丢弃。
#[tauri::command]
fn create_workspace(
    app: tauri::AppHandle,
    path: String,
) -> Result<serde_json::Value, WorkspaceOpError> {
    let state = app.state::<AppState>();
    let raw = path.trim().to_string();
    let canonical = std::path::Path::new(&raw)
        .canonicalize()
        .map_err(|e| WorkspaceOpError::InvalidPath(format!("工作区路径 {raw} 不可用: {e}")))?;
    if !canonical.is_dir() {
        return Err(WorkspaceOpError::InvalidPath(format!(
            "工作区路径 {raw} 不是目录"
        )));
    }
    // 与默认工作区、新建会话走同一份判据：注册表里不许出现指向数据根/`data\secrets`
    // 的条目，否则侧栏添加一次就把整条读取通道打开了。
    state
        .check_workspace_root(&canonical)
        .map_err(WorkspaceOpError::InvalidPath)?;
    let canonical_str = tool_router::shell_usable(&canonical).to_string_lossy().to_string();
    let cfg = state.config();
    if !cfg.workspace.hidden
        && canonical_workspace_path(&state.workspace_root()).as_deref() == Some(canonical_str.as_str())
    {
        return Ok(serde_json::json!({
            "workspace": workspace_view_of(&state, DEFAULT_WORKSPACE_ID, state.workspace_root().display().to_string().as_str(), &default_workspace_title(&cfg)),
            "created": false,
        }));
    }
    if let Some(existing) = cfg.workspaces.iter().find(|ws| {
        canonical_workspace_path(std::path::Path::new(&ws.path)).as_deref() == Some(canonical_str.as_str())
    }) {
        return Ok(serde_json::json!({
            "workspace": workspace_view_of(&state, &existing.id, &existing.path, &existing.display_title()),
            "created": false,
        }));
    }
    let id = format!(
        "ws-{}",
        ulid::Generator::new().generate_from_datetime(std::time::SystemTime::now())
            .map_err(|e| WorkspaceOpError::Rejected(format!("工作区 id 生成失败: {e}")))?
    );
    let title = std::path::Path::new(&canonical_str)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| canonical_str.clone());
    let mut next = cfg.clone();
    next.workspaces.push(config::WorkspaceProfile {
        id: id.clone(),
        path: canonical_str.clone(),
        title,
    });
    config::AppConfig::validate_workspaces(&next.workspaces).map_err(WorkspaceOpError::Rejected)?;
    state.update_config(next).map_err(WorkspaceOpError::Rejected)?;
    Ok(serde_json::json!({
        "workspace": workspace_view_of(&state, &id, &canonical_str, std::path::Path::new(&canonical_str).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default().as_str()),
        "created": true,
    }))
}

/// 改工作区显示名（SH-099 / 2026-11-07 对齐上游）：只写 title，路径不动；与其它家
/// （含默认项标题基线）重名拒绝。校验与应用在 [`apply_workspace_rename`]（纯函数）。
#[tauri::command]
fn rename_workspace(
    app: tauri::AppHandle,
    workspace_id: String,
    title: String,
) -> Result<serde_json::Value, WorkspaceOpError> {
    let state = app.state::<AppState>();
    let next = apply_workspace_rename(&state.config(), &workspace_id, &title)?;
    state.update_config(next).map_err(WorkspaceOpError::Rejected)?;
    let cfg = state.config();
    let id = workspace_id.trim();
    let (ws_id, path, ws_title) = if id == DEFAULT_WORKSPACE_ID {
        (
            DEFAULT_WORKSPACE_ID.to_string(),
            state.workspace_root().display().to_string(),
            default_workspace_title(&cfg),
        )
    } else {
        let ws = cfg
            .workspace_by_id(id)
            .ok_or_else(|| WorkspaceOpError::Rejected("改名后读不回工作区".into()))?;
        (ws.id.clone(), ws.path.clone(), ws.display_title())
    };
    Ok(serde_json::json!({
        "workspace": workspace_view_of(&state, &ws_id, &path, &ws_title),
    }))
}

/// 删除工作区注册（SH-099 / 2026-11-07 对齐上游）：只删注册，目录与会话档案全保留
/// ——会话在侧栏落到「未分组」（上游同口径）。默认项落 `[workspace].hidden` 标记
/// （`[workspace].root` 不能物理删）；摘除逻辑在 [`apply_workspace_delete`]（纯函数）。
#[tauri::command]
fn delete_workspace(
    app: tauri::AppHandle,
    workspace_id: String,
) -> Result<serde_json::Value, WorkspaceOpError> {
    let state = app.state::<AppState>();
    let id = workspace_id.trim().to_string();
    let next = apply_workspace_delete(&state.config(), &id)?;
    state.update_config(next).map_err(WorkspaceOpError::Rejected)?;
    Ok(serde_json::json!({ "removed": id }))
}

/// 归档/置顶/顺序状态的一次性读取（SH-100）：workspace 面的 baseline 与帧都从这里来。
#[tauri::command]
fn get_workspace_flags(app: tauri::AppHandle) -> serde_json::Value {
    let state = app.state::<AppState>();
    let flags = rollout::read_session_order(&state.data_root().join(rollout::ROLLOUT_DIR));
    serde_json::json!({
        "archivedSessionIds": flags.archived_session_ids,
        "pinnedSessionIds": flags.pinned_session_ids,
        "sessionOrder": flags.session_order,
    })
}

/// 从既有会话分支出一个新档案，返回新 agent_id（SH-033 的 fork 入口）。
///
/// 截断点由 `nearest_safe_cut` 兜底：前端要 N 条，实际可能给得更少（从中间切会留下
/// 有 call 无 output 的尾巴，续跑第一次请求就 400，见 SH-082）。父档案只读。
#[tauri::command]
fn fork_session(app: tauri::AppHandle, agent_id: String, keep_items: usize) -> Result<String, String> {
    let state = app.state::<AppState>();
    let dir = state.data_root().join(rollout::ROLLOUT_DIR);
    let src = dir.join(format!("{agent_id}.jsonl"));
    let new_id = state.next_session_id();
    let forked = rollout::resume::fork(&src, &dir, &new_id, keep_items)
        .map_err(|e| format!("分支失败: {e}"))?;
    Ok(forked.new_agent_id)
}

/// 改会话标题（DSH 侧栏的「重命名」入口，`remote.session.rename`）。
///
/// 返回真正生效的标题：`None` = 清空了自定义名，界面据此回落到派生标题，而不是显示一个
/// 空字符串——空标题在列表里看起来像"这个会话没有内容"。失败带机器可读的
/// [`rollout::RenameError`]：前端按 `code` 分派 DSH wire 错误，不匹配文案。
#[tauri::command]
fn rename_session(
    app: tauri::AppHandle,
    agent_id: String,
    title: String,
) -> Result<Option<String>, rollout::RenameError> {
    let state = app.state::<AppState>();
    let dir = state.data_root().join(rollout::ROLLOUT_DIR);
    rollout::set_title(&dir, &agent_id, &title)
}

/// 建一份空档案并返回 agent_id（SH-017：DSH 先建会话、再发首条消息）。
///
/// 只建盘不跑任务。档案首行的 `SessionMeta` 是 `resume` 的准入条件，先建档、
/// 之后 `send_task(agent_id)` 才能续到同一份档案上；让前端自己编 id 会让档案目录里
/// 出现后端不认识的会话，会话列表也就永远读不回它。
#[tauri::command]
fn create_session(
    app: tauri::AppHandle,
    workspace_path: Option<String>,
    session_id: Option<String>,
) -> Result<String, String> {
    let state = app.state::<AppState>();
    // 工作区绑定（SH-099）：给了路径就用它（必须存在且是目录——工具沙箱的作用域
    // 由它决定），没给回落全局默认根。face 会把 workspaceId 解析成路径后再来。
    let workspace_root = match workspace_path.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        Some(path) => {
            let root = PathBuf::from(path);
            if !root.is_absolute() {
                return Err(format!("工作区路径必须是绝对路径: {path}"));
            }
            if !root.is_dir() {
                return Err(format!("工作区路径不存在或不是目录: {path}"));
            }
            // 唯一判据：数据根本身/祖先/`data\secrets` 这类兄弟目录一律拒绝——
            // 会话工作区一旦指过去，模型就能顺着文件工具读到档案与凭据。
            state.check_workspace_root(&root)?;
            root.canonicalize().map(|p| tool_router::shell_usable(&p)).unwrap_or(root)
        }
        None => state.workspace_root(),
    };
    // 空白复用（对齐上游 blank 生命周期）：face 在工作区成员里找到的第一条空白，
    // 带 sessionId 来请求创建。宿主二次校验后复用同一档案；校验不过（档案不存在
    // 视作按该 id 新建、已有条目/正在运行视作不可复用）就新造一份，原空白原样留着
    // ——上游 createMinted 同口径，绝不静默吞掉用户的任何一份会话。
    if let Some(requested) = session_id.as_deref().map(str::trim).filter(|id| !id.is_empty()) {
        let reuse_path = state.data_root().join(rollout::ROLLOUT_DIR).join(format!("{requested}.jsonl"));
        let running_here = state.active_agent().as_deref() == Some(requested);
        if reuse_path.is_file() && !running_here {
            let blank = rollout::read(&reuse_path).map(|archive| archive.items.is_empty()).unwrap_or(false);
            if blank {
                return Ok(requested.to_string());
            }
        }
        if !reuse_path.is_file() {
            // 指名的 id 没有档案（比如 face 缓存了未落盘的空白）：以该 id 直接建档
            let (writer, agent_id) = new_session_writer(&state, &workspace_root, Some(requested.to_string()))?;
            drop(writer);
            return Ok(agent_id);
        }
        // 不可复用（已开始任务）→ 落到下面新造，原会话原样保留
    }
    let (writer, agent_id) = new_session_writer(&state, &workspace_root, None)?;
    //  writer 在这里只负责把首条落盘；后续写入由 send_task 重新 open_existing。
    drop(writer);
    Ok(agent_id)
}

/// 组装一次新会话所需的 AgentLoop + 取消令牌 + 它的 agent_id（现场建档）。
/// 建档失败就直接报错给前端——静默不落盘会让用户以为"能 resume"，实际不能。
fn build_new_session(
    state: &AppState,
    workspace_root: &Path,
    sandbox: Option<config::SandboxMode>,
) -> Result<(Arc<agent_loop::AgentLoop>, tokio_util::sync::CancellationToken, String), String> {
    let (writer, agent_id) = new_session_writer(state, workspace_root, None)?;
    if let Some(mode) = sandbox {
        // 新会话的初始档位（输入框先切档、再发首条消息）：落进档案，跟会话走。
        writer
            .append(&rollout::RolloutItem::SandboxMode {
                mode: mode.as_str().to_string(),
                at_ms: now_ms(),
            })
            .map_err(|e| format!("写入会话档位失败: {e}"))?;
    }
    let (agent, cancel) =
        build_agent_with(state, &agent_id, workspace_root.to_path_buf(), Some(writer), sandbox)?;
    Ok((agent, cancel, agent_id))
}

fn new_session_writer(
    state: &AppState,
    workspace_root: &Path,
    explicit_id: Option<String>,
) -> Result<(Arc<rollout::RolloutWriter>, String), String> {
    let cfg = state.config();
    let agent_id = explicit_id.unwrap_or_else(|| state.next_session_id());
    let meta = rollout::RolloutItem::SessionMeta {
        session_id: agent_id.clone(),
        agent_id: agent_id.clone(),
        parent_id: None,
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or_default(),
        config_snapshot: rollout::ConfigSnapshot {
            provider: cfg.model.provider.clone(),
            base_url: cfg.model.base_url.clone(),
            model: cfg.model.model.clone(),
            sandbox: format!("{:?}", cfg.sandbox.mode),
            approval: format!("{:?}", cfg.approval.mode),
            // 会话的工作区在建档时固化（SH-099）：之后 resume/工具沙箱都读这一行
            workspace_root: workspace_root.display().to_string(),
        },
    };
    let writer = rollout::RolloutWriter::create(&state.data_root().join(rollout::ROLLOUT_DIR), &meta)
        .map(Arc::new)
        .map_err(|e| format!("会话档案建档失败: {e}"))?;
    Ok((writer, agent_id))
}

/// 装配本任务的编排能力（SH-052）：注册表随任务创建、随任务释放。
///
/// 注册表只在任务生命周期内存活是刻意的：root 收口时 Drop 会取消并 abort 任何
/// 仍未结束的子代理（R102 防僵尸）。叶子子代理不注入 orchestrator，因此不会有
/// 「代理持有自身」的引用环。
fn build_orchestrator(
    engine: Arc<agent_loop::AgentLoop>,
) -> Arc<subagent_registry::orchestrator::RegistryOrchestrator> {
    let registry = Arc::new(subagent_registry::SubagentRegistry::new(
        subagent_registry::DEFAULT_MAX_CONCURRENT,
    ));
    Arc::new(subagent_registry::orchestrator::RegistryOrchestrator::new(registry, engine))
}

/// 子代理命令面的类型化错误（SH-053 批 3）：code 用上游 `remote.subagents` 的错误码，
/// 前端据此抛 RemoteError，不靠文案匹配。
#[derive(Debug, Clone, serde::Serialize, thiserror::Error)]
#[serde(tag = "code")]
pub enum SubagentCommandError {
    #[error("当前没有运行中的任务，子代理 steering 不可投递")]
    #[serde(rename = "subagent/delivery-unavailable")]
    DeliveryUnavailable,
    #[error("子代理不在本任务注册表中或已结束，不能续聊")]
    #[serde(rename = "subagent/not-resumable")]
    NotResumable,
    #[error("请求参数非法")]
    #[serde(rename = "gateway/bad-request")]
    BadRequest,
}

impl From<subagent_registry::SteerError> for SubagentCommandError {
    fn from(_: subagent_registry::SteerError) -> Self {
        Self::NotResumable
    }
}

/// 向运行中的子代理注入 steering（`remote.subagents.prompt` 的宿主面）。
/// 返回 messageId 供前端撤乐观回显；注入在子代理下一个采样迭代生效（SHANIU 的
/// inbox 语义，非流内 steer——偏差已在 docs/05 卡注声明）。
/// `rpc_id` 是客户端铸造的 requestId，随事件回传，泳道据此把 source 写成 user+rpcId。
#[tauri::command]
fn send_subagent_message(
    app: tauri::AppHandle,
    agent_id: String,
    message: String,
    rpc_id: Option<String>,
) -> Result<String, SubagentCommandError> {
    if message.trim().is_empty() {
        return Err(SubagentCommandError::NotResumable);
    }
    let registry = app
        .state::<AppState>()
        .subagents()
        .ok_or(SubagentCommandError::DeliveryUnavailable)?;
    registry.send_message(&agent_id, &message, rpc_id.as_deref())?;
    Ok(ulid::Ulid::generate().to_string())
}

/// 中断一个子代理（`remote.subagents.interruptByUser` 的宿主面）。
///
/// 语义照上游：absent/idle/已完成的目标都是**受理成功的 no-op**，只有空 id 是
/// bad-request（`subagent/src/index.ts` 的 interruptByUser 注释）。UI 的 Stop 按钮
/// 因此不会因为"子代理刚好收口"弹一个假错误。
#[tauri::command]
fn interrupt_subagent(app: tauri::AppHandle, agent_id: String) -> Result<bool, SubagentCommandError> {
    if agent_id.trim().is_empty() {
        return Err(SubagentCommandError::BadRequest);
    }
    let registry = app
        .state::<AppState>()
        .subagents()
        .ok_or(SubagentCommandError::DeliveryUnavailable)?;
    registry.cancel(&agent_id);
    Ok(true)
}

fn build_agent_with(
    state: &AppState,
    agent_id: &str,
    workspace_root: PathBuf,
    writer: Option<Arc<rollout::RolloutWriter>>,
    session_sandbox: Option<config::SandboxMode>,
) -> Result<(Arc<agent_loop::AgentLoop>, tokio_util::sync::CancellationToken), String> {
    let cfg = state.config();
    // 生效端点由配置层解析（active 服务商命中 → 那家；否则回落 [model]）。
    // 这里不再自己写死 provider 字符串：写死就等于多服务商列表里换了一家、
    // 密钥却仍从同一个文件读。
    let active = cfg.effective_model();
    // 推理档已在配置层解析成 wire 值（档位查表 / off / 未声明都在那边收口）
    let provider = model_client::provider::ProviderConfig::from_resolved(&active);
    let secret = config::SecretStore::new(&state.config_root());
    let api_key = secret
        .load_key(&active.secret_name)?
        .ok_or_else(|| format!("未配置 {} 的 API Key，请先在设置中保存", active.id))?;

    // 首次使用该模型时后台补一次能力实测（SH-132）：缓存按
    // (端点, 协议, 模型, 密钥指纹) 命中就直接返回，不阻塞回合、不重复打请求。
    // 换 key / 换端点 / 换模型自然落到另一条缓存记录上，等价于失效重探。
    if let Some(row) = cfg
        .providers
        .iter()
        .find(|p| p.id == active.id)
        .and_then(|p| p.models.iter().find(|m| m.id == active.model))
    {
        let target = capability::ProbeTarget {
            base_url: active.base_url.clone(),
            protocol: active.provider.clone(),
            model: active.model.clone(),
            headers: active.headers.clone(),
            api_key: api_key.clone(),
            token_budget_hint: row.max_tokens,
        };
        let data_root = state.data_root().to_path_buf();
        let probing = state.probing.clone();
        let row = row.clone();
        tauri::async_runtime::spawn(async move {
            capability::ensure_probed(data_root, target, row, &probing).await;
        });
    }

    let cancel = tokio_util::sync::CancellationToken::new();
    if !state.begin_task(agent_id, app_state::TaskKind::Turn, cancel.clone()) {
        return Err(state.busy_message(agent_id));
    }

    let backend: Arc<dyn ModelBackend> = Arc::new(agent_loop::ResponsesBackend { provider, api_key });
    let mut router = tool_router::ToolRouter::new();
    // 工具面跟随能力（SH-132）：视觉未生效的模型不注册 `view_image`。
    tools::register_builtin(&mut router, state.plan_state.clone(), active.vision);
    // MCP 已批准工具（SH-063）：同一批执行器副本进每个任务的 router；
    // 撤销在运行时翻转执行器开关，已下发副本随之失效。
    for executor in state.mcp.executors() {
        if let Err(error) = router.try_register(executor) {
            tracing::warn!("MCP 工具注册冲突（已跳过）: {error}");
        }
    }
    // SH-120 + 档位会话化：任务启动时把审批档重置回 config，沙箱档取本会话档案里的
    // 最后一档（没有则部署默认）。
    state.reset_policy(session_sandbox);
    let agent = agent_loop::AgentLoop::from_config(
        backend,
        Arc::new(router),
        state.broker.clone(),
        state.plan_state.clone(),
        &cfg,
        workspace_root,
        writer,
    )
    .with_jobs(agent_id.to_string(), state.jobs.clone())
    .with_policy(state.policy.clone());
    Ok((Arc::new(agent), cancel))
}

/// 取消当前任务。
#[tauri::command]
fn cancel_task(app: tauri::AppHandle) -> Result<(), String> {
    let _ = app.state::<AppState>().cancel_active();
    Ok(())
}

/// 审批回执（approval card → broker）。
#[tauri::command]
fn approve(app: tauri::AppHandle, request_id: String, decision: String) -> Result<(), String> {
    let decision = match decision.as_str() {
        "allow" => events::ApprovalDecision::Allow,
        "allow_always" => events::ApprovalDecision::AllowAlways,
        "deny" => events::ApprovalDecision::Deny,
        other => return Err(format!("未知裁决: {other}")),
    };
    app.state::<AppState>().broker.resolve(&request_id, decision);
    Ok(())
}

/// 运行中切换审批档（SH-120 / ADR-007）：只作用于**当前会话**，从下一个 turn 生效。
/// 不写 config.toml——写成全局默认就是"全局开关"，ADR-008 明确否掉。
/// `full_access` 的前提（danger 沙箱）不满足时返回错误，不静默回落。
#[tauri::command]
fn set_approval_mode(app: tauri::AppHandle, mode: String) -> Result<(), String> {
    let parsed: config::ApprovalMode = serde_json::from_value(serde_json::Value::String(mode.clone()))
        .map_err(|_| format!("未知审批档 `{mode}`，可选 {APPROVAL_MODE_NAMES}"))?;
    app.state::<AppState>().policy.set_approval(parsed)
}

/// 保存 API Key（DPAPI 加密落盘，docs §9.1 secrets\）。
#[tauri::command]
fn store_api_key(app: tauri::AppHandle, provider: String, key: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    config::SecretStore::new(&state.config_root()).store_key(&provider, &key)
}

/// 查询是否已保存某 provider 的 Key。
#[tauri::command]
fn has_api_key(app: tauri::AppHandle, provider: String) -> Result<bool, String> {
    let state = app.state::<AppState>();
    config::SecretStore::new(&state.config_root()).load_key(&provider).map(|k| k.is_some())
}

/// 读取设置（非敏感字段 + Key 是否已配置）。
#[tauri::command]
fn get_settings(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    let state = app.state::<AppState>();
    let cfg = state.config();
    let secret = config::SecretStore::new(&state.config_root());
    // 逐服务商报告"是否已配 Key"：界面只问生效那家的话，切换按钮就会在一个根本没密钥的
    // 服务商上显示可用，第一次发送才失败——失败点离原因越远越难归因。
    // 内置服务商置顶（数据层次序）：`get_settings` 是设置页与席位共用的唯一来源，
    // 排序放在这里而不是前端数组下标——前端只消费顺序，不再维护一份置顶特判。
    let ordered_providers = config::builtin_providers::ordered_profiles(&cfg.providers);
    let mut profiles = Vec::with_capacity(ordered_providers.len());
    let probe_cache = capability::ProbeCache::load(&state.data_root());
    for p in ordered_providers {
        let stored_key = secret.load_key(&p.secret_name())?;
        let models = p
            .models
            .iter()
            .map(|m| {
                // 缓存键三处共用：取结论、判断"是否正在测"。各算各的必然漂移。
                let cache_key = stored_key
                    .as_deref()
                    .map(|key| capability::cache_key(&p.base_url, &p.provider, &m.id, key));
                let probed = cache_key
                    .as_deref()
                    .and_then(|key| probe_cache.entries.get(key));
                let mut probing = cache_key
                    .as_deref()
                    .is_some_and(|key| state.probing.contains(key));
                // 打开设置页时给「当前生效模型」补一次后台探测：否则它要等用户第一次
                // 发消息才开测，设置页会长期停在「未验证」。只探生效模型——一次打开
                // 就探全部模型等于替用户烧 token。去重与缓存命中都在 probe_and_cache 里。
                if !probing && probed.is_none() && m.id == p.model {
                    if let Some(api_key) = stored_key.clone() {
                        let target = capability::ProbeTarget {
                            base_url: p.base_url.clone(),
                            protocol: p.provider.clone(),
                            model: m.id.clone(),
                            headers: p.headers.clone(),
                            api_key,
                            token_budget_hint: m.max_tokens,
                        };
                        let data_root = state.data_root().to_path_buf();
                        let inflight = state.probing.clone();
                        let row = m.clone();
                        tauri::async_runtime::spawn(async move {
                            capability::ensure_probed(data_root, target, row, &inflight).await;
                        });
                        probing = true;
                    }
                }
                serde_json::json!({
                    "id": m.id,
                    "name": m.name,
                    "contextWindow": m.context_window,
                    "maxTokens": m.max_tokens,
                    "input": m.input,
                    // 推理档声明（SH-096）：档位 → wire 值；空串 = off 档（不发字段）。
                    // 前端视图把它翻成 null，写回时再归一成空串。
                    "reasoningEfforts": m.reasoning_efforts,
                    // 生效档位（视图专用，不落盘）：与请求侧同一个判据
                    // `declared_reasoning_levels`，本行没声明时由内置表/端点补出
                    // 实际会接受的档位。设置页只读它补显示，写面逐字段挑时丢弃。
                    "effectiveReasoningLevels": m.declared_reasoning_levels(&p.base_url),
                    // 能力视图（SH-132）：值 + 来源 + 实测四态（已验证/不符/检测中/未验证）。
                    // 实测结论按 (端点, 协议, 模型, 密钥指纹) 取缓存，换任一项自然失效。
                    "capabilities": capability::model_view(m, &p.base_url, probed, probing),
                })
            })
            .collect::<Vec<_>>();
        profiles.push(serde_json::json!({
            "id": p.id,
            "label": p.label,
            "provider": p.provider,
            "baseUrl": p.base_url,
            "model": p.model,
            "secretRef": p.secret_name(),
            "hasApiKey": stored_key.is_some(),
            // 模型清单原样回显，空清单不替 `model` 补一行：补出来的那行看起来像
            // "用户自己加的"，设置页就会给一个根本没被声明过的档位提供删除按钮。
            "models": models,
            // 席位选中的推理档位（档位名；空 = 服务商默认）
            "reasoningEffort": p.reasoning_effort,
            // 自定义请求头（SH-097）：设置页可编辑的部署头，原样回显
            "headers": p.headers,
        }));
    }
    let active = cfg.effective_model();
    Ok(serde_json::json!({
        "baseUrl": active.base_url,
        "model": active.model,
        "maxTurns": cfg.model.max_turns,
        // 推理档位清单与默认档的唯一真源（SH-132 收敛）：前端不再自带一份
        // `THINKING_LEVELS`。之前两处各写一份，`path`/`title` 才会被当成档位写进
        // `reasoning_efforts`——那块字段只认这张表里的名字。
        "thinkingLevels": config::THINKING_LEVELS,
        "defaultReasoningLevel": config::DEFAULT_REASONING_LEVEL,
        "workspaceRoot": state.workspace_root().to_string_lossy(),
        "hasApiKey": secret.load_key(&active.secret_name)?.is_some(),
        // 安全面两档的回显。序列化直接走 serde（枚举本身是 snake_case），
        // 不在这里手写字符串——手写就会与 config.toml 里的写法漂移成两套名字。
        "sandboxMode": serde_json::to_value(cfg.sandbox.mode).map_err(|e| e.to_string())?,
        "approvalMode": serde_json::to_value(cfg.approval.mode).map_err(|e| e.to_string())?,
        // 生效档的协议族 id 与服务商文档（多服务商口径，04 §4）
        "activeProvider": if cfg.active_provider.is_empty() { active.id.clone() } else { cfg.active_provider.clone() },
        "providerRevision": cfg.provider_revision,
        "providers": profiles,
        // 额外工作区注册表（SH-099）：face 的多视图 baseline 从这里来
        "workspaces": cfg.workspaces.iter().map(|ws| serde_json::json!({
            "id": ws.id,
            "path": ws.path,
            "title": ws.display_title(),
        })).collect::<Vec<_>>(),
        // 默认工作区的标题基线与删除标记（2026-11-07 对齐上游可改可删语义）
        "workspaceTitle": default_workspace_title(&cfg),
        "workspaceHidden": cfg.workspace.hidden,
        // 工作区展示顺序（SH-111）：空 = 未拖拽过，face 按派生序排
        "workspaceOrder": cfg.workspace.order,
        // 配置只读（2026-11-08）：启动时坏配置修不了 → 禁止任何写回。
        // 字符串带配置文件路径，设置页可直接展示；null = 可写。
        "configReadOnly": state.config_read_only_reason(),
    }))
}

/// 写服务商文档（04 §4 `[[providers]]`）。
///
/// 整份文档一次写入而不是逐条 op：设置页的草稿本来就是"整张表"，拆成增量只会让
/// 半途失败留下一个混合状态——改坏一半的服务商列表比整份不写更难恢复。
/// `expected_revision` 传了就做乐观锁比对；不匹配必须报错而不是覆盖，否则两个人
/// 分别改两家会互相吞掉对方的那一家。
#[tauri::command]
fn save_providers(
    app: tauri::AppHandle,
    providers: Vec<config::ProviderProfile>,
    active_provider: String,
    expected_revision: Option<u64>,
) -> Result<u64, String> {
    let state = app.state::<AppState>();
    config::AppConfig::validate_providers(&providers, &active_provider)?;
    let mut cfg: AppConfig = state.config();
    if let Some(expected) = expected_revision {
        if expected != cfg.provider_revision {
            return Err(format!(
                "服务商文档已被改动（期望修订 {expected}，实际 {}），请重新读取后再写",
                cfg.provider_revision
            ));
        }
    }
    // 能力来源维护（SH-132）：继承旧记录 + 把用户手改过的项标成手动。
    let previous = cfg.providers.clone();
    let mut providers = providers;
    capability::carry_sources(&previous, &mut providers);
    cfg.providers = providers;
    cfg.active_provider = active_provider;
    cfg.provider_revision = cfg.provider_revision.saturating_add(1);
    state.update_config(cfg)?;
    Ok(state.config().provider_revision)
}

/// 删除某家的密钥（设置页"清除 Key"）。文件不存在不算错误——界面要的就是"没配"。
#[tauri::command]
fn delete_api_key(app: tauri::AppHandle, provider: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    config::SecretStore::new(&state.config_root()).delete_key(&provider)
}

/// 沙箱状态（SH-021）：UI 依据 `os_enabled=false` 常驻标注「当前未启用 OS 沙箱」。
#[tauri::command]
fn sandbox_status(app: tauri::AppHandle) -> sandbox::SandboxStatus {
    let state = app.state::<AppState>();
    let cfg = state.config();
    sandbox::status(cfg.sandbox.mode, cfg.sandbox.os)
}

/// 报错时列给用户的合法档位名。它必须与 `config.rs` 的枚举同源——
/// `safety_mode_names_match_enums` 用穷举后的序列化结果逐字比对，加一档忘改这里会直接红。
const SANDBOX_MODE_NAMES: &str = "read_only / workspace_write / danger_full_access";
const APPROVAL_MODE_NAMES: &str = "never / on_failure / on_request / full_access";

/// 保存设置：模型端点/工作区入 config.toml，API Key（可选填）DPAPI 加密落盘。
#[tauri::command]
fn save_settings(
    app: tauri::AppHandle,
    base_url: String,
    model: String,
    workspace_root: String,
    api_key: Option<String>,
    sandbox_mode: Option<String>,
    approval_mode: Option<String>,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut cfg: AppConfig = state.config();
    // 多服务商时改的是**生效那一家**，不是 [model]：[model] 只是没有列表时的回落，
    // 两处都写就会出现"用户改了模型、跑起来还是旧端点"。
    if cfg.active_provider.is_empty() {
        cfg.model.base_url = base_url.trim_end_matches('/').to_string();
        cfg.model.model = model;
    } else {
        let target = cfg.active_provider.clone();
        if let Some(profile) = cfg.providers.iter_mut().find(|p| p.id == target) {
            // 与 validate_providers 同一条不变式：这里也要守。save_settings 走的是
            // update_config，不经过 save_providers 的校验，漏掉这一步就能把生效模型
            // 改成清单里没有的一行，而席位下拉只列清单——用户看到的就是"跑着一个
            // 选不出来的模型"。
            if !profile.models.is_empty()
                && !model.trim().is_empty()
                && !profile.models.iter().any(|m| m.id == model)
            {
                return Err(format!(
                    "服务商 {target} 的模型清单里没有 {model}，请先在设置里添加这一行"
                ));
            }
            profile.base_url = base_url.trim_end_matches('/').to_string();
            profile.model = model;
        } else {
            // active 指不到任何一家是文档已经坏了的状态（validate 在写入时把关，
            // 走到这里只可能是手工编辑了 config.toml）。这里必须报错而不是"改了
            // [model]"——静默改回落段会让用户以为改的是当前服务商。
            return Err(format!("active_provider 指不到任何服务商：{target}"));
        }
    }
    // 工作区根先过唯一判据再落盘：保存成功即代表"下次启动会用它跑任务"，所以这里
    // 必须在写入前拒绝，而不是等启动时再悄悄回落。空串 = 使用默认目录。
    let trimmed_root = workspace_root.trim().to_string();
    if !trimmed_root.is_empty() {
        let candidate = std::path::PathBuf::from(&trimmed_root);
        if !candidate.is_absolute() {
            return Err(format!("工作区路径必须是绝对路径: {trimmed_root}"));
        }
        state.check_workspace_root(&candidate)?;
    }
    cfg.workspace.root = trimmed_root;
    // 安全面两档：None = 不改，所以旧调用形态（不传这两个键）行为完全不变。
    // 非法值必须报错而不是静默保留旧值——"点了保存却没生效、界面还显示旧档位"是安全
    // 设置里最坏的一类失败：用户会以为自己已经收紧（或放宽）了，实际什么都没变。
    // 解析复用 serde 而不是手写 match，这样名字永远与 config.toml 里的写法同源。
    if let Some(mode) = sandbox_mode {
        cfg.sandbox.mode = serde_json::from_value(serde_json::Value::String(mode))
            .map_err(|_| format!("沙箱模式取值非法，可用：{}", SANDBOX_MODE_NAMES))?;
    }
    if let Some(mode) = approval_mode {
        cfg.approval.mode = serde_json::from_value(serde_json::Value::String(mode))
            .map_err(|_| format!("审批模式取值非法，可用：{}", APPROVAL_MODE_NAMES))?;
    }
    // 写入文件名必须与读取路径同源（生效档的 secret_name）：这里再写死一个
    // "openai-responses"，多服务商下就是"存进去读不到"的另一副面孔。
    // 必须在 update_config 之前取——cfg 会被它移走。
    let secret_name = cfg.effective_model().secret_name;
    state.update_config(cfg)?;
    if let Some(key) = api_key {
        let key = key.trim().to_string();
        if !key.is_empty() {
            config::SecretStore::new(&state.config_root()).store_key(&secret_name, &key)?;
        }
    }
    Ok(())
}

/// 右侧栏终端的启动入口（组合根命令壳）。
///
/// 起始目录在这里解析：优先用请求给的目录，但它必须真实存在**且过工作区唯一判据**；
/// 否则回落当前会话的默认工作区。判定不复制——只调 `AppState::check_workspace_root`。
/// 终端模块本身按 E57 保持零 crate 内依赖（同 `session_export` 的处理方式）。
#[tauri::command]
fn user_terminal_open(
    app: tauri::AppHandle,
    state: tauri::State<'_, user_terminal::UserTerminalManager>,
    host: tauri::State<'_, AppState>,
    request: user_terminal::OpenRequest,
) -> Result<user_terminal::TerminalInfo, String> {
    let cwd = user_terminal::resolve_cwd(request.cwd.as_deref(), &host.workspace_root(), |path| {
        host.check_workspace_root(path)
    });
    user_terminal::open_with_cwd(&app, &state, request, cwd)
}

/// 在系统文件管理器里打开一个目录（会话头部「资源管理器」按钮的后端）。
///
/// 只接受**当前真实存在的目录**：路径由前端从会话 cwd 带来，这里不拼接、不创建、
/// 不执行调用方给定的程序名——程序名固定为系统自带的 explorer / open / xdg-open。
/// 用 `spawn` 而不是 `output`：explorer 即使成功也常返回非零退出码，等待它只会误报失败。
#[tauri::command]
fn open_in_explorer(path: String) -> Result<(), String> {
    let target = std::path::PathBuf::from(&path);
    if !target.is_dir() {
        return Err(format!("不是可打开的目录: {path}"));
    }
    let program = if cfg!(target_os = "windows") {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(program)
        .arg(&target)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("打开文件管理器失败: {e}"))
}

/// 「添加工作区」的系统目录选择：弹原生「选择文件夹」对话框。
///
/// 用户取消返回 None；选中返回绝对路径。异步命令运行在非主线程，阻塞式选择器
/// 在这里等待答复，主线程保持可渲染（插件文档要求不得在主线程阻塞调用）。
#[tauri::command]
async fn pick_folder(app: tauri::AppHandle) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    app.dialog()
        .file()
        .set_title("选择工作区目录")
        .blocking_pick_folder()
        .and_then(|path| path.into_path().ok())
        .map(|path| path.to_string_lossy().into_owned())
}

/// SH-114 批 2：导出探活（前端 `HEAD` 的落点）。
///
/// 只回答"这份档案能不能导出"，并把 SHANIU 档案首行 `SessionMeta` 翻译成上游
/// `SessionHeader` 的语义（前端用它拼 canonical `session.jsonl` 的首行）。
/// **不读全部事件**（那是 `load_session` 的事），也不写盘。
#[tauri::command]
fn export_probe(
    app: tauri::AppHandle,
    session_id: String,
) -> Result<session_export::ExportHeader, String> {
    let state = app.state::<AppState>();
    let path = state
        .data_root()
        .join(rollout::ROLLOUT_DIR)
        .join(format!("{session_id}.jsonl"));
    let archive = rollout::read(&path).map_err(|e| format!("档案读不回: {e}"))?;
    match &archive.meta {
        rollout::RolloutItem::SessionMeta {
            session_id: id,
            parent_id,
            created_at,
            config_snapshot,
            ..
        } => {
            let cwd = config_snapshot.workspace_root.trim();
            Ok(session_export::ExportHeader {
                id: id.clone(),
                created_at: *created_at,
                cwd: if cwd.is_empty() {
                    None
                } else {
                    Some(cwd.to_string())
                },
                parent_session: parent_id.clone(),
                // 上游 `delegationDepth` 缺省为 0；SHANIU 只有根/一级子代理两态。
                delegation_depth: u32::from(parent_id.is_some()),
            })
        }
        _ => Err("档案首行不是 SessionMeta".into()),
    }
}

/// SH-114 批 2：会话日志的保存位置选择（原生「另存为」对话框）。
///
/// 与 `pick_folder` 同款：阻塞式选择器只在异步命令里用，主线程保持可渲染。
/// 用户取消返回 None。
///
/// **返回的是票据而不是路径**（审计 H2 批 1）：调用方拿不到也不需要目标路径，
/// `export_session_log` 只接受这里签发的凭据，因此无法凭任意字符串把文件写到别处。
/// 取消时一并撤销上一张票据——「上次确认、这次取消」不能落回旧目标。
#[tauri::command]
async fn pick_save_file(app: tauri::AppHandle, default_name: String) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    let picked = app
        .dialog()
        .file()
        .set_title("保存会话日志")
        .set_file_name(&default_name)
        .add_filter("ZIP 压缩包", &["zip"])
        .blocking_save_file()
        .and_then(|path| path.into_path().ok());
    let state = app.state::<AppState>();
    match picked {
        Some(path) => Some(state.save_tickets.issue(path)),
        None => {
            state.save_tickets.revoke();
            None
        }
    }
}

/// 导出条目的附件来源必须落在附件目录内（审计 H2 批 1）。
///
/// 复用 `tool_router::resolve_in_workspace` 的同一套比较口径（canonicalize +
/// `starts_with`），不另写路径判定；越界或不存在都拒绝，由调用方记日志。
fn validate_export_sources(
    entries: &[session_export::ExportEntry],
    attachments_root: &std::path::Path,
) -> Result<(), String> {
    for entry in entries {
        let Some(source) = entry.source_path.as_deref() else {
            continue;
        };
        if let Err(reason) = crate::tool_router::resolve_in_workspace(attachments_root, source) {
            return Err(format!(
                "附件路径越界（附件目录之外）: {source}；条目 {}；{reason}",
                entry.path
            ));
        }
    }
    Ok(())
}

/// SH-114 批 2：把条目写成 ZIP。
///
/// 条目内容是"canonical 逻辑会话日志"（上游 `archive.ts` 的原文定义），由前端
/// `session-events.ts` 那套唯一的事件翻译产出；宿主只负责压缩与路径安全。
///
/// **`dest` 是 `pick_save_file` 的返回值（一次性票据），不是路径**（审计 H2 批 1）：
/// 票据无效/已使用/用户取消后调用，一律拒绝并留日志——没有用户手势就没有写盘目标。
#[tauri::command]
fn export_session_log(
    app: tauri::AppHandle,
    dest: String,
    entries: Vec<session_export::ExportEntry>,
) -> Result<session_export::ExportReport, String> {
    let state = app.state::<AppState>();
    let Some(target) = state.save_tickets.take(&dest) else {
        tracing::warn!(
            target: "shaniu::export",
            "导出目标票据无效或已被使用，拒绝写入"
        );
        return Err("保存目标无效或已失效：请重新选择保存位置".to_string());
    };
    let attachments_root = state.data_root().join("attachments");
    if let Err(reason) = validate_export_sources(&entries, &attachments_root) {
        tracing::warn!(
            target: "shaniu::export",
            %reason,
            "导出被拒绝：附件来源不在附件目录内"
        );
        return Err(reason);
    }
    session_export::write_zip(&target, &entries)
}

/// 已注册的工作区根：默认工作区 + 配置里的额外工作区（审计 H2 批 2）。
///
/// 额外工作区过唯一判据：被拒的项不进入可写边界（否则审计面的"仅已注册"会被
/// 一条指向 `data\secrets` 的配置绕过）。
fn registered_workspace_roots(state: &AppState) -> Vec<std::path::PathBuf> {
    let mut roots = vec![state.workspace_root()];
    roots.extend(
        state
            .config()
            .workspaces
            .iter()
            .map(|workspace| workspace.path.trim().to_string())
            .filter(|path| !path.is_empty())
            .map(std::path::PathBuf::from)
            .filter(|candidate| state.check_workspace_root(candidate).is_ok()),
    );
    roots
}

/// 目标必须落在某个已注册工作区内，返回 canonical 路径。
///
/// 比较口径直接复用 `tool_router::resolve_in_workspace`（canonicalize +
/// `starts_with`），与沙箱/工作区文件面同一套判定，不另写路径比较（审计 H2 批 2）。
fn resolve_in_registered_workspace(
    state: &AppState,
    path: &str,
) -> Result<std::path::PathBuf, String> {
    if !std::path::Path::new(path).is_absolute() {
        return Err(format!("路径必须是绝对路径: {path}"));
    }
    let mut last_err = String::new();
    for root in registered_workspace_roots(state) {
        match crate::tool_router::resolve_in_workspace(&root, path) {
            Ok(resolved) => return Ok(resolved),
            Err(reason) => last_err = reason.to_string(),
        }
    }
    tracing::warn!(
        target: "shaniu::open_in_app",
        path,
        "拒绝打开工作区之外的路径"
    );
    Err(format!("路径不在任何已注册工作区内: {path}（{last_err}）"))
}

/// 打开/定位工作区路径（审计 H2 批 2 的收紧入口）。
///
/// 改前：路径随便给，`application` 在目录场景直接当 exe 执行。改后：目标必须落在
/// 已注册工作区内；目录只认 catalog id、文件只认该文件当前注册处理器。
#[tauri::command]
fn open_workspace_path(
    state: tauri::State<'_, AppState>,
    path: String,
    action: Option<String>,
    application: Option<String>,
) -> Result<(), String> {
    let target = resolve_in_registered_workspace(&state, &path)?;
    open_in_app::open_workspace_path_at(&target, action.as_deref(), application.as_deref())
}

/// 用一个目录应用打开工作区内的目录（审计 H2 批 2 的收紧入口）。
#[tauri::command]
fn open_in_app_launch(
    state: tauri::State<'_, AppState>,
    app: String,
    path: String,
) -> Result<(), String> {
    let target = resolve_in_registered_workspace(&state, &path)?;
    open_in_app::open_in_app_launch_at(&app, &target)
}

/// SH-137：`window.open` 的宿主出口——只放行 http(s)，交给系统默认浏览器。
///
/// 前端在装配期把 `window.open` 换成宿主垫片（`src/external-link/route.ts`），
/// 避免 Tauri 把整个应用导航到外网；协议白名单在 Rust 侧再判一次。
#[tauri::command]
fn open_external_url(url: String) -> Result<(), String> {
    open_in_app::open_external_url(&url)
}

/// SH-115：`/plan` 命令的宿主面——直接置位/清位**该会话**的 PlanState。
///
/// 上游 `/plan` 注册在 host 插件（`plan-mode/src/index.ts`，inject `['commands']`），
/// 语义就是「直接改计划态」而不是走模型工具：`/plan off` 由 PlanChip 的退出路径
/// 调用（`ui-plan/client/index.ts` 的 `exitPlanMode`），`/plan` 进入后模型在**下一轮**
/// 只见只读工具。计划态按 `session_id` 存（审计 M2）：会话 A 开计划态不再把会话 B
/// 一并锁成只读。与工具路径（`enter_plan_mode`/`exit_plan_mode`）写同一份状态。
/// 返回置位后的状态，前端据此推 `plan` 投影（芯片随行）。
#[tauri::command]
fn set_plan_mode(app: tauri::AppHandle, session_id: String, active: bool) -> bool {
    let state = app.state::<AppState>();
    if active {
        state.plan_state.activate_for(Some(&session_id));
    } else {
        state.plan_state.deactivate_for(Some(&session_id));
    }
    state.plan_state.is_active_for(Some(&session_id))
}

/// SH-115：计划态查询面。补 SH-106 已知缺口（适配层重启丢 plan 态而宿主持久，
/// 宿主无查询面）——前端跟随/重开 会话时据此重建投影，芯片不再依赖"本轮恰好有
/// enter/exit 工具执行事件"才亮。按会话查询（审计 M2）。
#[tauri::command]
fn get_plan_mode(app: tauri::AppHandle, session_id: String) -> bool {
    app.state::<AppState>().plan_state.is_active_for(Some(&session_id))
}

/// SH-115：`/compact` 的宿主面——回合外按需压缩。
///
/// 复用 SH-032 的锚点格式与回放语义（`rollout::resume`），总结请求是对齐
/// `session_title` 形态的独立辅助请求（`session_compact::compact_now`）。**运行中
/// 会话拒绝**：回合内的自动压缩与这里的锚点写如果并发，两份锚点的区间会互相
/// 指错（区间语义建立在"锚点按记录顺序、对当时重放长度生效"上）。
///
/// 锁用宿主既有的任务槽（`begin_task`），而不是只比一次 `active_agent`：
/// - 只比一次是**检查-使用**竞态：检查通过后、总结请求返回前，用户再发一条消息就
///   能起新回合，两边同时改历史/写锚点；
/// - 占用任务槽后，压缩期间的新回合会被 `begin_task` 挡住（回 `busy_message`：
///   点名「会话 X 正在压缩上下文」——应用级闸必须让被挡的人知道卡在哪），
///   压的就是"打包那一刻"的会话，不会再变；
/// - 顺带让停止按钮生效：`cancel_active` 发出的令牌会被总结流 `select!` 收到，
///   取消后锚点不写（档案未改动），并如实回「压缩被用户取消」。
/// 代价：任务槽是应用级的，压缩期间**其它会话**也起不了新回合。可接受（压缩是
/// 短时辅助请求），但别把它当成"只锁这个会话"。
#[tauri::command]
async fn compact_session(app: tauri::AppHandle, agent_id: String) -> Result<Option<session_compact::CompactOutcome>, String> {
    let state = app.state::<AppState>();
    let cancel = tokio_util::sync::CancellationToken::new();
    if !state.begin_task(&agent_id, app_state::TaskKind::Compaction, cancel.clone()) {
        return Err(state.busy_message(&agent_id));
    }
    // Drop 守卫：成功、失败、取消、panic 都必须释放任务槽，否则「压缩中」会把应用卡死。
    let app_for_slot = app.clone();
    let _task_slot = app_state::TaskSlotGuard::new(move || {
        app_for_slot.state::<AppState>().end_task()
    });
    let cfg = state.config();
    let active = cfg.effective_model();
    // 辅助请求不启用推理档（与标题生成同一裁决：档位属于主回合的采样质量选择，
    // 辅助请求要快与省）；自定义请求头照带（部署头是端点级的）。
    let provider = model_client::provider::ProviderConfig {
        provider: active.provider.clone(),
        base_url: active.base_url.clone(),
        model: active.model.clone(),
        reasoning_effort: None,
        headers: active.headers.clone(),
    };
    let api_key = crate::config::SecretStore::new(&state.config_root())
        .load_key(&active.secret_name)
        .map_err(|e| format!("读取密钥失败: {e}"))?
        .ok_or_else(|| "未配置 API 密钥，无法生成摘要".to_string())?;
    let budget = cfg.agent.token_budget;
    let path = state
        .data_root()
        .join(rollout::ROLLOUT_DIR)
        .join(format!("{agent_id}.jsonl"));
    let resumed = rollout::resume::resume(&path).map_err(|e| format!("档案读不回: {e}"))?;
    // 事件面：手动压缩与自动压缩同一条事件流，重开会话/换客户端时也能对上时间线。
    let sink: Arc<dyn events::EventSink> = Arc::new(
        events::TauriEventSink::root(app.clone(), agent_id.clone())
            .with_journal(Arc::new(event_journal::FileEventJournal::new(
                app.state::<AppState>().data_root().join(config::SESSIONS_DIR),
            )))
            .with_broadcast(app.state::<gateway::GatewayHub>().broadcast_sender())
            .with_viewers(app.state::<gateway::GatewayHub>().viewers()),
    );
    // state 的借用止于上面最后一次取值（NLL），不跨下面的 await 点。
    session_compact::compact_now(&provider, &api_key, &resumed, budget, sink.as_ref(), &cancel)
        .await
}

/// 产品目录视图：前端用它把默认工作区基线落到宿主解析出的 `<数据根>\Workspace`。
#[tauri::command]
fn get_product_dirs() -> config::ProductDirsView {
    config::resolve_dirs_from_env().view()
}

/// 附件目录占用（设置页只读一行）：总文件数与总字节数。
///
/// 读失败原样报错而不是返回 0——0 会被误读成「没有附件」。
#[tauri::command]
fn attachment_stats(app: tauri::AppHandle) -> Result<attachments::AttachmentStats, String> {
    attachments::stats(&app.state::<AppState>().data_root())
}

/// 读取系统剪贴板的纯文本（输入框右键菜单「粘贴」）。
///
/// 桌面必须走宿主：WebView2 的 `navigator.clipboard.readText()` 走权限提示，
/// 被拒时粘贴会静默失败。读不到文本（无剪贴板内容/格式不符）返回空串。
#[tauri::command]
fn read_clipboard_text() -> Result<String, String> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    unsafe {
        OpenClipboard(None).map_err(|e| format!("打开剪贴板失败: {e}"))?;
        let read = (|| -> Result<String, String> {
            if IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).is_err() {
                return Ok(String::new());
            }
            let handle = GetClipboardData(CF_UNICODETEXT.0 as u32)
                .map_err(|e| format!("读取剪贴板失败: {e}"))?;
            let hglobal = HGLOBAL(handle.0);
            let ptr = GlobalLock(hglobal);
            if ptr.is_null() {
                return Err("剪贴板内存不可读".into());
            }
            let text = windows::core::PCWSTR(ptr as *const u16).to_string().unwrap_or_default();
            let _ = GlobalUnlock(hglobal);
            Ok(text)
        })();
        let _ = CloseClipboard();
        read
    }
}

/// 写入系统剪贴板的纯文本（execCommand 复制失败时的兜底）。
#[tauri::command]
fn write_clipboard_text(text: String) -> Result<(), String> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    unsafe {
        OpenClipboard(None).map_err(|e| format!("打开剪贴板失败: {e}"))?;
        let write = (|| -> Result<(), String> {
            EmptyClipboard().map_err(|e| format!("清空剪贴板失败: {e}"))?;
            let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            let bytes = wide.len() * std::mem::size_of::<u16>();
            let hmem = GlobalAlloc(GMEM_MOVEABLE, bytes)
                .map_err(|e| format!("分配剪贴板内存失败: {e}"))?;
            let ptr = GlobalLock(hmem);
            if ptr.is_null() {
                return Err("剪贴板内存不可写".into());
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr().cast::<u8>(), ptr.cast::<u8>(), bytes);
            let _ = GlobalUnlock(hmem);
            // 成功后内存所有权移交系统，不能再释放
            SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(hmem.0)))
                .map_err(|e| format!("写入剪贴板失败: {e}"))?;
            Ok(())
        })();
        let _ = CloseClipboard();
        write
    }
}

/// 事件接收方：把 MCP 应用级事件发到独立通道（无会话归属）。
struct TauriMcpSink {
    app: tauri::AppHandle,
}

impl crate::mcp_client::runtime::McpEventSink for TauriMcpSink {
    fn emit(&self, event: &crate::mcp_client::runtime::McpEvent) {
        if let Err(error) = self.app.emit(crate::mcp_client::runtime::MCP_EVENT_CHANNEL, event) {
            tracing::warn!("MCP 事件发送失败: {error}");
        }
    }
}

/// 设置页的 MCP 服务器列表项（配置 + 运行时状态合并）。
#[derive(serde::Serialize)]
struct McpServerSummary {
    id: String,
    enabled: bool,
    command: String,
    args: Vec<String>,
    unsandboxed: bool,
    allow_network: bool,
    status: String,
    fingerprint: Option<String>,
    approved_tools: Vec<crate::mcp_client::runtime::ApprovedToolView>,
    /// 用户显式启用的注入豁免数量（插件页列表层展示用）。
    exemption_count: usize,
    reason: Option<String>,
}

/// 列出配置中的 MCP server（含信任状态与已批准工具）。
#[tauri::command]
async fn mcp_list_servers(app: tauri::AppHandle) -> Result<Vec<McpServerSummary>, String> {
    let state = app.state::<AppState>();
    let cfg = state.config();
    let statuses = state.mcp.statuses().await;
    Ok(cfg
        .mcp
        .servers
        .iter()
        .map(|server| {
            let status = statuses.iter().find(|item| item.server_id == server.id);
            McpServerSummary {
                id: server.id.clone(),
                enabled: server.enabled,
                command: server.command.clone(),
                args: server.args.clone(),
                unsandboxed: server.unsandboxed,
                allow_network: server.allow_network,
                status: status
                    .map(|item| item.status.as_str().to_string())
                    .unwrap_or_else(|| {
                        if server.enabled {
                            "idle".to_string()
                        } else {
                            "disabled".to_string()
                        }
                    }),
                // blocked 但没有信任记录时没有指纹：空串对界面毫无意义，按「未知」处理。
                fingerprint: status
                    .map(|item| item.fingerprint.clone())
                    .filter(|value| !value.is_empty()),
                approved_tools: status
                    .map(|item| item.approved_tools.clone())
                    .unwrap_or_default(),
                exemption_count: status.map(|item| item.exemption_count).unwrap_or(0),
                reason: status.and_then(|item| item.reason.clone()),
            }
        })
        .collect())
}

/// 首连审查：启动 server、读取清单、计算指纹；未批准前不注册任何工具。
///
/// `accept_new_binary` 只由「需要重新信任」界面的显式确认传入：可执行文件哈希与批准
/// 记录不一致时，默认**不启动进程**（docs/02 §3.1.4(3)）。
#[tauri::command]
async fn mcp_connect_and_review(
    app: tauri::AppHandle,
    server_id: String,
    accept_new_binary: Option<bool>,
) -> Result<crate::mcp_client::runtime::ReviewPayload, String> {
    let state = app.state::<AppState>();
    let cfg = state.config();
    let server = cfg
        .mcp
        .servers
        .iter()
        .find(|server| server.id == server_id)
        .cloned()
        .ok_or_else(|| format!("配置里没有 MCP server: {server_id}"))?;
    let workspace = state.workspace_root();
    state
        .mcp
        .connect_and_review_with(&server, &workspace, accept_new_binary.unwrap_or(false))
        .await
}

/// 应用首连审查的逐工具裁决（批准位 + 风险档位 + 注入工具显式启用）。
#[tauri::command]
async fn mcp_resolve_trust(
    app: tauri::AppHandle,
    server_id: String,
    fingerprint: String,
    decisions: Vec<crate::mcp_client::runtime::TrustDecisionInput>,
) -> Result<crate::mcp_client::runtime::ReviewPayload, String> {
    let state = app.state::<AppState>();
    state
        .mcp
        .resolve_trust(&server_id, &fingerprint, &decisions)
        .await
}

/// 撤销整个 server 的信任与豁免（立即失效 + 下次连接重新审查）。
#[tauri::command]
async fn mcp_revoke_trust(app: tauri::AppHandle, server_id: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    state.mcp.revoke_trust(&server_id).await
}

/// 列出全部显式启用记录（插件页「豁免」台账；跨服务器、跨指纹只读展示）。
#[tauri::command]
async fn mcp_list_exemptions(
    app: tauri::AppHandle,
) -> Result<Vec<crate::mcp_client::guard::Exemption>, String> {
    let state = app.state::<AppState>();
    Ok(state.mcp.list_exemptions().await)
}

/// 撤销单个注入豁免；`fingerprint` 用于撤销旧指纹留下的记录（缺省 = 当前指纹）。
#[tauri::command]
async fn mcp_revoke_exemption(
    app: tauri::AppHandle,
    server_id: String,
    dispatch_name: String,
    fingerprint: Option<String>,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    state
        .mcp
        .revoke_exemption(&server_id, &dispatch_name, fingerprint.as_deref())
        .await
}

/// 从托盘恢复主窗口：取消最小化 + 显示 + 聚焦。
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        // 窗口回到前台后，托盘 tooltip 里的“仍在后台运行”就不再成立。
        if let Some(tray) = app.tray_by_id("main") {
            let _ = tray.set_tooltip(Some("SHANIU"));
        }
        // 二次启动唤回：窗口可能被隐藏（托盘态）或压在别的窗口后面，补一次原生
        // 恢复 + 置前，保证"再启动一次 = 看到同一个窗口"。
        if let Ok(hwnd) = window.hwnd() {
            config::single_instance::restore_window(hwnd.0 as *mut _);
        }
    }
}

/// 原生消息框：启动早期窗口还没创建，只能走 Win32。
fn show_native_message(text: &str) {
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
    };
    let text = HSTRING::from(text);
    let caption = HSTRING::from("SHANIU");
    unsafe {
        MessageBoxW(
            None,
            &text,
            &caption,
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST,
        );
    }
}

/// 原生二选一：返回 true = 用户点了「是」。
fn show_native_confirm(text: &str) -> bool {
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, IDYES, MB_ICONWARNING, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO,
    };
    let text = HSTRING::from(text);
    let caption = HSTRING::from("SHANIU");
    let result = unsafe {
        MessageBoxW(
            None,
            &text,
            &caption,
            MB_YESNO | MB_ICONWARNING | MB_SETFOREGROUND | MB_TOPMOST,
        )
    };
    result == IDYES
}

/// 第二个实例的告知：主窗口此时还没创建（tauri.conf.json 里 `create: false`，
/// 要等 setup 才建），所以只能用原生消息框。静默退出或"只读凑合启动"都不行——
/// 前者让用户以为程序没起来，后者会让用户以为历史还在、实际写不进去。
fn report_data_root_in_use(data_root: &Path) {
    show_native_message(&format!(
        "这个数据目录已被另一个 SHANIU 实例占用：\n{}\n\n\
         同一数据目录不能同时打开两份，否则会话历史可能被写坏。\n\
         请先退出另一个实例；或为这份副本指定独立的数据目录（便携模式：\
         把 exe 单独放进一个空文件夹，首次启动会在它旁边建 data\\）。",
        data_root.display()
    ));
}

pub fn run() {
    // 数据根：**首次启动判定一次，之后按记录固定**（2026-11-11）。判定结果写入日志
    // 第一行（docs §9.3），并作为本进程内所有路径解析的唯一依据。
    // 记录/旧目录说便携但当前不可写时，给用户一次明确出口（改用安装模式，原目录保留），
    // 不自动切换；用户选择退出则中止，不拿不可写目录凑合启动。
    let root = config::init_root_resolution_with(|resolution| {
        show_native_confirm(&format!(
            "数据目录不可用\n\n{}",
            resolution.warning.as_deref().unwrap_or_default()
        ))
    });
    if root.aborted {
        eprintln!("[SHANIU] 数据目录不可写，且用户未选择改用安装模式，已退出");
        std::process::exit(2);
    }
    let dirs = root.dirs.clone();
    // 第一层：全局（本登录会话）唯一 UI 实例。第二次启动不是"程序已在运行"然后什么
    // 都不发生——而是把已运行的窗口叫到前台；若两份指向不同数据目录，明确说明当前
    // 运行的是哪个并给出退出的指引，绝不让两个实例各自写自己的历史。
    let _global_lock = match config::single_instance::acquire_global() {
        Ok(config::single_instance::GlobalAcquire::Acquired(lock)) => lock,
        Ok(config::single_instance::GlobalAcquire::AlreadyRunning) => {
            let outcome = config::single_instance::handle_second_launch(
                &dirs.data_root,
                &config::single_instance::running_info_path(),
                config::single_instance::ACTIVATE_EVENT_NAME,
                show_native_message,
            );
            // 二次启动进程干净退出（0）：它不是错误，只是把用户送回已在运行的那个窗口。
            eprintln!("[SHANIU] 已有实例在运行（{outcome:?}），本次启动不再继续");
            std::process::exit(0);
        }
        Err(message) => {
            eprintln!("[SHANIU] {message}");
            show_native_message(&format!("无法启动：{message}"));
            std::process::exit(2);
        }
    };
    // 同一 data_root 只允许一个写者：rollout 是逐行 append 的真源，两个进程同写会把
    // 两条历史交错进同一档案。互斥体名由规范化路径 hash 派生，所以不同 `data\` 的
    // 两个副本仍能同时跑（那是合法用法，拦下来是错的）。
    // 必须早于日志初始化与任何文件写入：第二个实例不该往同一 logs\ 里再写一份。
    // 锁存活到 run() 返回 = 进程结束；异常退出时由 OS 关闭句柄自动释放。
    let _instance_lock = match config::single_instance::acquire(&dirs.data_root) {
        Ok(lock) => lock,
        Err(message) => {
            eprintln!("[SHANIU] {message}");
            report_data_root_in_use(&dirs.data_root);
            std::process::exit(2);
        }
    };
    // 告诉下一次启动"我用的是哪个数据目录"；写不下去不拦启动（最多让二次启动退化成
    // 通用提示），但必须留痕。两把锁都拿到之后才写，避免第二实例覆盖首实例的信息。
    let running_info = config::single_instance::RunningInstance::new(&dirs.data_root);
    if let Err(message) = config::single_instance::write_running_info(
        &config::single_instance::running_info_path(),
        &running_info,
    ) {
        eprintln!("[SHANIU] {message}");
    }
    // 产品目录结构提前建好（幂等）：secrets/settings-backup、sessions/rollouts/logs/cache/crash
    // 与默认工作区 Workspace。失败不拦启动——各写路径自己 fail-closed，这里只如实报告。
    if let Err(e) = config::ensure_product_dirs(&dirs) {
        eprintln!("[SHANIU] 产品目录创建失败（数据目录不可写？）: {e}");
    }
    // PRR 缺口补齐（docs/01 §9.3）：panic 先落本地 crash\ 报告（脱敏、不上报），
    // 再交给原 hook 保留 stderr 行为。安装早于日志初始化，日志起不来也能留证据。
    config::install_crash_hook(dirs.data_root.join(config::CRASH_DIR));
    // 日志初始化必须早于一切 tracing:: 事件；guard 存活期 = 进程存活期
    let _log_guard = match config::init_logging(&dirs.data_root) {
        Ok(guard) => Some(guard),
        Err(e) => {
            // 日志起不来不能拦着应用启动（fail-open 但脱敏表照常工作），兜底打印说明原因
            eprintln!("[SHANIU] 日志初始化失败（本次会话无文件日志）: {e}");
            None
        }
    };
    tracing::info!("config_root: {}", dirs.config_root.display());
    tracing::info!("data_root:   {}", dirs.data_root.display());
    tracing::info!("workspace:   {}", dirs.workspace_root().display());
    tracing::info!(
        data_root_kind = root.kind.map(config::DataRootKind::as_str).unwrap_or("env"),
        record = %root.record_path.display(),
        decided_now = root.decided_now,
        "数据根判定"
    );
    if let Some(from) = &root.switched_from {
        tracing::warn!(
            from = %from.display(),
            to = %dirs.data_root.display(),
            "经用户确认改用安装模式；原数据保留在原处，未删除、未移动"
        );
    }
    // 记录与实际不符（目录不可写 / 位置被改）必须明确告诉用户，不许静默按新位置跑。
    // 此刻窗口尚未创建，只能用原生消息框；日志同时留一条，便于事后对账。
    if let Some(warning) = &root.warning {
        tracing::warn!(%warning, "数据位置需要用户注意");
        show_native_message(&format!("数据位置需要注意\n\n{warning}"));
    }

    // 启动加载 + 一次性修复：盘上可能留着「写串段」的旧配置（`reasoning_efforts`
    // 里混进 path/title）。修复只做备份→剔除→写回；修不了就进只读、绝不覆盖。
    let loaded = AppConfig::load_or_create_diagnosed(&dirs.config_root);
    let (app_config, config_path) = (loaded.config.clone(), loaded.path.clone());
    match &loaded.outcome {
        config::ConfigLoadOutcome::Clean => {}
        config::ConfigLoadOutcome::Repaired { backup, removed } => {
            tracing::warn!(
                backup = %backup.display(),
                removed = %removed.join("；"),
                "启动时修复了写串的配置：已剔除非法键并写回（原文件已备份）"
            );
        }
        config::ConfigLoadOutcome::ReadOnly { reason } => {
            tracing::error!(
                %reason,
                "配置无法安全加载：进入只读模式，本次运行不会写回任何配置"
            );
        }
    }

    // SH-021/022：OS 沙箱机制的进程级装配。
    // 回退到 logical 必须是配置里显式写的——这里只负责把它标出来，绝不自动回退。
    sandbox::set_os_mode(app_config.sandbox.os);
    if app_config.sandbox.mode == config::SandboxMode::WorkspaceWrite
        && app_config.sandbox.os == config::OsSandbox::Logical
    {
        tracing::warn!(
            "当前未启用 OS 沙箱（sandbox.os = \"logical\"，用户显式选择）：\
             命令只有受限令牌 + Job 的逻辑隔离，没有路径级写入边界"
        );
    }

    // SH-094 批 2：gateway hub（广播发送端 + 一次性 token + 动态端口）。sink 取发送端喂事件，
    // .setup 绑定 loopback 端口跑 accept 循环，gateway_info 命令回报 port/token 给前端。
    let gateway_hub = gateway::GatewayHub::new(dirs.data_root.join(config::SESSIONS_DIR));

    let app_state = AppState::new(app_config, config_path, dirs.data_root.clone());
    if let config::ConfigLoadOutcome::ReadOnly { reason } = &loaded.outcome {
        app_state.mark_config_read_only(reason.clone());
    }
    // 默认工作区只在"这次真的以默认根工作"时创建：配置了合法自定义根时不再在数据根里
    // 留一个永远不会被使用的空 `Workspace\`（那还会被误认成正在用的目录）。
    if app_state.workspace_root() == dirs.workspace_root() {
        if let Err(error) = std::fs::create_dir_all(dirs.workspace_root()) {
            let message = format!(
                "创建默认工作区失败 {}: {error}",
                dirs.workspace_root().display()
            );
            eprintln!("[SHANIU] {message}");
            show_native_message(&message);
            std::process::exit(2);
        }
    }
    // 配置里的工作区根被唯一判据拒绝：本次已回落默认根，但必须明确告诉用户
    // （不静默改配置、不静默换成默认值——用户会以为任务跑在原目录）。
    if let Some(message) = app_state.workspace_rejection() {
        tracing::warn!("工作区设置不可用，本次已回落默认工作区：{message}");
        show_native_message(&format!("工作区设置不可用\n\n{message}"));
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(app_state)
        .manage(gateway_hub.clone())
        .manage(user_terminal::UserTerminalManager::default())
        .invoke_handler(tauri::generate_handler![
            dev_ping,
            frontend_ready,
            quit_app,
            quit_survey,
            quit_after_cancel,
            hide_main_window,
            send_task,
            create_session,
            resume_session,
            list_sessions,
            load_session,
            search_sessions,
            retry_session_index,
            fork_session,
            rename_session,
            archive_session,
            unarchive_session,
            pin_session,
            unpin_session,
            insert_session_before,
            get_workspace_flags,
            create_workspace,
            rename_workspace,
            delete_workspace,
            insert_workspace_before,
            cancel_task,
            approve,
            set_approval_mode,
            set_session_sandbox_mode,
            session_sandbox_mode,
            send_subagent_message,
            interrupt_subagent,
            store_api_key,
            has_api_key,
            get_settings,
            save_settings,
            save_providers,
            sandbox_status,
            model_discovery::discover_models,
            key_check::check_api_key,
            capability::probe_capabilities,
            capability::fetch_model_metadata,
            delete_api_key,
            open_in_explorer,
            pick_folder,
            export_probe,
            pick_save_file,
            export_session_log,
            compact_session,
            get_plan_mode,
            set_plan_mode,
            get_product_dirs,
            attachment_stats,
            read_session_events,
            latest_session_event_id,
            session_of_agent,
            gateway_info,
            upload_attachment,
            read_attachment,
            read_clipboard_text,
            write_clipboard_text,
            workspace_files::workspace_file_stat,
            workspace_files::workspace_file_read,
            workspace_files::workspace_file_read_bytes,
            workspace_files::workspace_file_list,
            file_actions::save_file_bytes,
            open_in_app::open_in_app_catalog,
            open_in_app_launch,
            open_in_app::workspace_path_applications,
            open_external_url,
            open_workspace_path,
            file_references::list_file_references,
            session_reference::list_session_references,
            skills::list_skills,
            goals::goal_get,
            goals::goal_create,
            goals::goal_edit,
            goals::goal_pause,
            goals::goal_resume,
            goals::goal_clear,
            jobs::job_list,
            jobs::job_read,
            jobs::job_kill,
            mcp_list_servers,
            mcp_list_exemptions,
            mcp_connect_and_review,
            mcp_resolve_trust,
            mcp_revoke_trust,
            mcp_revoke_exemption,
            user_terminal::user_terminal_shells,
            user_terminal_open,
            user_terminal::user_terminal_write,
            user_terminal::user_terminal_resize,
            user_terminal::user_terminal_close,
            user_terminal::user_terminal_close_all,
            user_terminal::user_terminal_active_count,
        ])
        .on_window_event(|window, event| {
            // 关闭 = 隐藏到系统托盘（不退出）；真正退出走托盘菜单「退出」
            // 或会话头「更多操作 → 退出 SHANIU」（`quit_app`）。
            //
            // A0-2：提示**必须落在应用之外**。窗口一 hide，应用内的任何 toast 都看不见，
            // 所以「还在后台」由托盘气泡发出（`tray_notify`）。不延迟 hide：用户点关闭
            // 就是要它立刻消失，多等是把实现困难转嫁给用户。
            //
            // 门控：只在「有任务在跑」或「本进程首次收托盘」时弹；其余情况只更新
            // tooltip（常驻、悬停可见），之后想再看走托盘菜单「后台运行说明」。
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    api.prevent_close();
                    let app = window.app_handle();
                    let running = tray_running_count(&quit_survey_of(app));
                    let first = !TRAY_HINT_SHOWN.swap(true, Ordering::SeqCst);
                    // tooltip 是气泡被系统屏蔽时的退路：它必须**每次都**更新，
                    // 不能只在弹气泡时更新（否则屏蔽后就是「什么都没发生」）。
                    if let Some(tray) = app.tray_by_id("main") {
                        let tooltip = if running > 0 {
                            format!("SHANIU 仍在后台运行（{running} 项任务）· 右键退出")
                        } else {
                            "SHANIU 仍在后台运行 · 右键退出".to_string()
                        };
                        let _ = tray.set_tooltip(Some(tooltip.as_str()));
                    }
                    if let Some((title, body)) = tray_notify::balloon_payload(running, first) {
                        match tray_notify::show_balloon(title, &body) {
                            Ok(()) => tracing::info!(
                                running,
                                first,
                                "关窗提示已发到系统托盘气泡（Win11 可能按系统通知设置显示或屏蔽）"
                            ),
                            // 气泡发不出去不是静默失败：tooltip 已更新，日志留痕便于对账。
                            Err(error) => tracing::warn!(
                                running,
                                first,
                                "关窗气泡发送失败，退回 tooltip（悬停可见）：{error}"
                            ),
                        }
                    } else {
                        tracing::info!(running, "非首次且无任务在跑：只更新 tooltip，不弹气泡");
                    }
                    // 应用内提示只在窗口因异常没能隐藏时才有意义（那时它才可见）。
                    match window.hide() {
                        Ok(()) => {}
                        Err(error) => {
                            tracing::warn!("隐藏主窗口失败，退回应用内提示：{error}");
                            let _ = window.emit(
                                user_terminal::TRAY_HINT_EVENT,
                                user_terminal::TrayHint { count: running, first },
                            );
                        }
                    }
                }
            }
        })
        .setup(|app| {
            // SH-135：主窗口改为**手动创建**。`on_web_resource_request`（媒体路由唯一挂点）
            // 只存在于 builder 上，而 tauri.conf.json 里的窗口由框架在 setup 之前自动建好，
            // 所以配置里标了 `create: false`，这里用同一份窗口配置重建并挂上钩子——尺寸、
            // 无边框、`visible: false`、`backgroundColor` 全部由 `from_config` 原样继承，
            // 因此"先隐藏、前端就绪后再显示"的时序不变。
            let window_config = app
                .config()
                .app
                .windows
                .iter()
                .find(|window| window.label == "main")
                .cloned()
                .ok_or("tauri.conf.json 缺少 label=main 的窗口配置")?;
            let resource_app = app.handle().clone();
            let mut builder = tauri::WebviewWindowBuilder::from_config(app.handle(), &window_config)?
                .on_web_resource_request(move |request, response| {
                    crate::media_route::handle_request(&resource_app, &request, response);
                });
            // 冒烟探针专用：显式给了 SHANIU_CDP_PORT 才开远程调试端口。
            // 不能只依赖 WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS——CI runner 上实测该环境变量
            // 会被丢掉（窗口起来了、端口没开），走 builder 参数才是确定路径。
            // 与 WebView2 自带的调试参数同权限模型：能改环境变量的人本来就能开这个口子。
            if let Some(port) = cdp_debug_port() {
                builder = builder.additional_browser_args(&format!(
                    "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --remote-debugging-port={port}"
                ));
            }
            builder.build()?;
            // 二次启动唤回：首实例在后台等命名事件，第二实例 SetEvent 后把窗口拉回前台。
            // 监听必须早于用户可能发起的第二次启动，所以紧跟窗口创建注册。
            let activate_handle = app.handle().clone();
            if let Err(error) = config::single_instance::spawn_activation_listener(
                config::single_instance::ACTIVATE_EVENT_NAME,
                move || show_main_window(&activate_handle),
            ) {
                // 监听起不来不拦启动：二次启动会退化成"无法唤出窗口"的明确提示。
                tracing::warn!("{error}");
            }
            // MCP 应用级事件通道（SH-063）：运行时在 setup 前已随 AppState 构造，
            // 这里补上真正的发送端。
            {
                let state = app.state::<AppState>();
                state.mcp.set_sink(Arc::new(TauriMcpSink {
                    app: app.handle().clone(),
                }));
            }
            // 延迟显示窗口的兜底（tauri.conf.json 的 visible:false）：前端脚本崩溃、
            // 页面加载失败或就绪信号丢失时，3 秒后强制显示，保证不存在"窗口永不出现"。
            //
            // 为什么是 3 秒：正常路径的就绪信号在进程启动后约 300–345 ms 到达（实测
            // 10 次 85–101 ms / gateway 监听后），3 秒留了约 10 倍余量，足够吸收冷启动
            // 抖动与杀软扫描；同时它必须明显短于失败出口的另外两层——主包内 8 秒超时
            // 与 index.html 的 12 秒看门狗——这样"窗口没出现"永远先于"面板报错"结束，
            // 用户不会面对一个看不见的窗口里转圈。上限侧则要压住"点了没反应"的体感，
            // 超过 3 秒还没有任何窗口，用户会怀疑程序没启动。
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                if !FRONTEND_READY.load(Ordering::SeqCst) {
                    tracing::warn!("前端未在 3 秒内发出就绪信号，强制显示主窗口");
                    show_main_window(&handle);
                }
            });
            // SH-094 批 2：loopback WS gateway。动态端口绑定后写入 hub 供 gateway_info 回报；
            // 绑定失败只告警降级（事件仍走 Tauri 广播），不拦启动。
            let gateway_hub = app.state::<gateway::GatewayHub>().inner().clone();
            tauri::async_runtime::spawn(async move {
                match tokio::net::TcpListener::bind("127.0.0.1:0").await {
                    Ok(listener) => {
                        if let Err(e) = gateway::serve(gateway_hub, listener).await {
                            tracing::warn!("WebSocket gateway 退出: {e}");
                        }
                    }
                    Err(e) => tracing::warn!("WebSocket gateway 绑定失败（事件仍走 Tauri 广播）: {e}"),
                }
            });
            // 系统托盘：菜单 = 显示 / 后台运行说明 / 退出；左键单击图标 = 显示主窗口
            let show = MenuItem::with_id(app, "show", "显示", true, None::<&str>)?;
            // 「后台运行说明」是关窗气泡不弹时的主动入口：用户之后想看，随时能再读一遍。
            let hint = MenuItem::with_id(app, "hint", "后台运行说明", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let tray_menu = Menu::with_items(app, &[&show, &hint, &quit])?;
            TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().expect("缺少应用图标").clone())
                .tooltip("SHANIU")
                .menu(&tray_menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "show" => show_main_window(app),
                    "hint" => {
                        // 不受「首次 / 有任务」门控：这是用户主动要看的。
                        let running = tray_running_count(&quit_survey_of(app));
                        let (title, body) = tray_notify::menu_hint_payload(running);
                        if let Err(error) = tray_notify::show_balloon(title, &body) {
                            tracing::warn!("托盘「后台运行说明」气泡发送失败：{error}");
                        }
                    }
                    "quit" => {
                        // 真退前先查有没有在跑的事：有就让前端给三个选项，
                        // 没有就直接退出（别让无任务的用户也点两次）。
                        let survey = quit_survey_of(app);
                        if survey.busy {
                            show_main_window(app);
                            let _ = app.emit(QUIT_REQUEST_EVENT, survey);
                        } else {
                            tracing::info!("托盘退出：没有在跑的任务，直接退出");
                            // SH-021：退出前先还原 DACL 并删除容器 profile，避免残留 ACE/悬空 profile。
                            sandbox::shutdown();
                            app.exit(0);
                        }
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main_window(tray.app_handle());
                    }
                })
                .build(app)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("SHANIU 启动失败");
}

#[cfg(test)]
mod quit_flow_tests {
    use super::{
        assemble_quit_survey, forced_abort_targets, record_forced_abort_at, tray_running_count,
        wait_until_idle, QuitJob,
    };
    use crate::rollout::{ConfigSnapshot, RolloutItem};
    use std::time::Duration;

    fn job(id: &str, session: Option<&str>) -> QuitJob {
        QuitJob {
            id: id.to_string(),
            session_id: session.map(str::to_string),
            label: format!("job-{id}"),
            status: String::from("running"),
        }
    }

    /// 反向对照：删掉任意一类“在跑的事”，`busy` 就会漏报，这条必须红。
    #[test]
    fn busy_covers_task_jobs_terminals_and_approvals() {
        assert!(!assemble_quit_survey(None, Vec::new(), 0, 0).busy);
        assert!(assemble_quit_survey(Some(String::from("sess-1")), Vec::new(), 0, 0).busy);
        assert!(assemble_quit_survey(None, vec![job("j1", None)], 0, 0).busy);
        assert!(assemble_quit_survey(None, Vec::new(), 1, 0).busy);
        assert!(assemble_quit_survey(None, Vec::new(), 0, 1).busy);
    }

    /// 关窗气泡口径：只有 agent 任务在跑（没有终端/作业）也必须算「还有事在跑」。
    /// 反向对照：把 `tray_running_count` 改回只数终端，第一条断言必红。
    #[test]
    fn tray_running_count_includes_agent_session() {
        let agent_only = assemble_quit_survey(Some(String::from("sess-1")), Vec::new(), 0, 0);
        assert_eq!(tray_running_count(&agent_only), 1);

        let mixed = assemble_quit_survey(
            Some(String::from("sess-1")),
            vec![job("j1", None), job("j2", None)],
            2,
            1,
        );
        // 待审批不重复计数（它挂在在跑的会话/作业上）。
        assert_eq!(tray_running_count(&mixed), 5);
    }

    #[test]
    fn forced_abort_targets_dedupes_active_session_and_jobs() {
        let targets = forced_abort_targets(
            Some("sess-1"),
            &[
                Some(String::from("sess-1")),
                Some(String::from("sess-2")),
                None,
            ],
        );
        assert_eq!(targets, vec![String::from("sess-1"), String::from("sess-2")]);
    }

    /// 撤掉轮询/等待 → 第二条“永不收口必须超时”会红。
    #[tokio::test]
    async fn wait_until_idle_returns_early_when_task_finishes() {
        let mut polls = 0;
        let (timed_out, waited) = wait_until_idle(|| {
            polls += 1;
            polls >= 3
        }, Duration::from_millis(300))
            .await;
        assert!(!timed_out, "第三次轮询已收口，不该判超时");
        assert!(waited < 300, "应早于上限返回，实际 {waited} ms");
    }

    #[tokio::test]
    async fn wait_until_idle_times_out_when_task_never_finishes() {
        let (timed_out, waited) = wait_until_idle(|| false, Duration::from_millis(250)).await;
        assert!(timed_out, "一直不收口必须判超时");
        assert!(waited >= 250, "必须熬到上限，实际 {waited} ms");
    }

    /// 撤掉留痕 → 档案里读不到 `abort_note`，这条红。
    #[test]
    fn forced_abort_is_written_into_archive() {
        let root = std::env::temp_dir().join(format!(
            "shaniu-quit-abort-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("时钟早于 Unix 纪元")
                .as_nanos()
        ));
        let dir = root.join(crate::rollout::ROLLOUT_DIR);
        std::fs::create_dir_all(&dir).expect("建临时 rollout 目录");
        let path = dir.join("sess-1.jsonl");
        let meta = RolloutItem::SessionMeta {
            session_id: String::from("sess-1"),
            agent_id: String::from("sess-1"),
            parent_id: None,
            created_at: 1,
            config_snapshot: ConfigSnapshot::default(),
        };
        std::fs::write(&path, format!("{}\n", serde_json::to_string(&meta).expect("序列化 meta")))
            .expect("写 meta");

        record_forced_abort_at(&root, "sess-1", "因退出而强制中止：取消等待超时（测试）");

        let text = std::fs::read_to_string(&path).expect("读档案");
        let last = text.lines().last().expect("至少两行");
        let item: RolloutItem = serde_json::from_str(last).expect("最后一行可解析");
        match item {
            RolloutItem::AbortNote { reason, at_ms } => {
                assert!(reason.contains("因退出而强制中止"));
                assert!(reason.contains("取消等待超时"));
                assert!(at_ms > 0);
            }
            other => panic!("期望 abort_note，实际 {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod session_projection_tests {
    use super::{project_entries, SessionEntry, APPROVAL_MODE_NAMES, SANDBOX_MODE_NAMES};
    use crate::config;
    use crate::model_client::{HistoryItem, Role};
    use crate::rollout::{ConfigSnapshot, RolloutItem};

    /// 一份带默认项标题与两家额外工作区的配置（工作区改名/删除纯函数的公共底座）。
    fn workspace_cfg(default_title: &str, hidden: bool) -> config::AppConfig {
        config::AppConfig {
            workspace: config::WorkspaceConfig {
                root: String::from("D:\\ws\\root"),
                title: String::from(default_title),
                hidden,
                order: Vec::new(),
            },
            workspaces: vec![
                config::WorkspaceProfile { id: String::from("ws-1"), path: String::from("D:\\ws\\one"), title: String::from("一号") },
                config::WorkspaceProfile { id: String::from("ws-2"), path: String::from("D:\\ws\\two"), title: String::new() },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn default_workspace_title_falls_back_to_upstream_spelling() {
        // [workspace].title 空 = 上游首启拼写，前端据此渲染「默认工作区」
        assert_eq!(super::default_workspace_title(&workspace_cfg("", false)), super::DEFAULT_WORKSPACE_ID);
        assert_eq!(super::default_workspace_title(&workspace_cfg("  ", false)), super::DEFAULT_WORKSPACE_ID);
        assert_eq!(super::default_workspace_title(&workspace_cfg("主力", false)), "主力");
    }

    #[test]
    fn default_workspace_rename_writes_title_and_conflicts_like_a_regular_entry() {
        // 上游同构：默认项改名与别家一样生效，落 [workspace].title
        let next = super::apply_workspace_rename(&workspace_cfg("", false), super::DEFAULT_WORKSPACE_ID, " 主力 ")
            .expect("默认工作区可改名");
        assert_eq!(next.workspace.title, "主力");
        assert!(!next.workspace.hidden);
        // 空名拒绝（上游 non-blank title 同口径）
        assert!(super::apply_workspace_rename(&workspace_cfg("", false), super::DEFAULT_WORKSPACE_ID, "  ").is_err());
        // 与额外工作区重名拒绝
        assert!(super::apply_workspace_rename(&workspace_cfg("", false), super::DEFAULT_WORKSPACE_ID, "一号").is_err());
    }

    #[test]
    fn extra_workspace_rename_conflicts_with_default_title_baseline() {
        // 默认项标题基线也算一家：改名撞上要拒绝
        assert!(super::apply_workspace_rename(&workspace_cfg("主力", false), "ws-1", "主力").is_err());
        assert!(super::apply_workspace_rename(&workspace_cfg("", false), "ws-1", "default-workspace").is_err());
        // 正常改名保留路径，只写 title
        let next = super::apply_workspace_rename(&workspace_cfg("", false), "ws-1", "二号").expect("改名生效");
        assert_eq!(next.workspaces[0].title, "二号");
        assert_eq!(next.workspaces[0].path, "D:\\ws\\one");
        // 不存在的工作区如实报 not-found
        assert!(super::apply_workspace_rename(&workspace_cfg("", false), "ws-x", "随便").is_err());
    }

    #[test]
    fn deleting_default_workspace_hides_it_and_reset_happens_on_adopt() {
        // 上游同构：删除只摘注册（目录与会话保留），额外项直接移除
        let next = super::apply_workspace_delete(&workspace_cfg("主力", false), super::DEFAULT_WORKSPACE_ID)
            .expect("默认工作区可删除");
        assert!(next.workspace.hidden);
        assert!(next.workspace.title.is_empty(), "标题一并重置：重新添加是全新注册");
        assert_eq!(next.workspaces.len(), 2, "额外工作区不受影响");
        // 额外项删除与 not-found 口径
        let next = super::apply_workspace_delete(&workspace_cfg("", false), "ws-1").expect("额外项可删除");
        assert_eq!(next.workspaces.len(), 1);
        assert!(super::apply_workspace_delete(&workspace_cfg("", false), "ws-x").is_err());
    }

    #[test]
    fn workspace_order_derives_from_registry_then_honors_the_stored_list() {
        // 未拖拽过：可见默认项居首 + [[workspaces]] 数组序
        assert_eq!(
            super::workspace_order(&workspace_cfg("", false)),
            vec!["default-workspace", "ws-1", "ws-2"],
        );
        // 默认项被删除（hidden）就不进顺序——上游 deleteKnown 不清 defaultWorkspaceId，
        // 但实体已摘，顺序里也不该有它
        assert_eq!(super::workspace_order(&workspace_cfg("", true)), vec!["ws-1", "ws-2"]);
        // 存过的顺序是权威：未知 id 丢弃、手写重复只认首次、新注册未列出的补尾
        let mut cfg = workspace_cfg("", false);
        cfg.workspace.order = vec!["ws-2".into(), "ws-gone".into(), "ws-2".into()];
        assert_eq!(super::workspace_order(&cfg), vec!["ws-2", "default-workspace", "ws-1"]);
        cfg.workspaces.push(config::WorkspaceProfile {
            id: "ws-3".into(), path: "D:\\ws\\three".into(), title: String::new(),
        });
        assert_eq!(super::workspace_order(&cfg), vec!["ws-2", "default-workspace", "ws-1", "ws-3"]);
    }

    #[test]
    fn workspace_order_move_matches_upstream_insert_before_edges() {
        let cfg = workspace_cfg("", false);
        // 带锚点：落在锚点之前
        assert_eq!(
            super::apply_workspace_order(&cfg, "ws-2", Some("default-workspace")).unwrap(),
            vec!["ws-2", "default-workspace", "ws-1"],
        );
        // 锚点省略 = 追加末尾
        assert_eq!(
            super::apply_workspace_order(&cfg, "default-workspace", None).unwrap(),
            vec!["ws-1", "ws-2", "default-workspace"],
        );
        // 锚点即自身 = 原序返回（上游同口径，不写盘）
        assert_eq!(
            super::apply_workspace_order(&cfg, "ws-1", Some("ws-1")).unwrap(),
            super::workspace_order(&cfg),
        );
        // 默认项与别家一样可移动：恒置头部是单工作区时代的遗留
        assert_eq!(
            super::apply_workspace_order(&cfg, "default-workspace", Some("ws-2")).unwrap(),
            vec!["ws-1", "default-workspace", "ws-2"],
        );
        // 被移动的 id 或锚点不在注册表 → not-found（上游 WorkspaceOrderInvalidError
        // 经 commands.ts 映射成 workspace/not-found）
        assert!(super::apply_workspace_order(&cfg, "ws-x", None).is_err());
        assert!(super::apply_workspace_order(&cfg, "ws-1", Some("ws-x")).is_err());
        // hidden 的默认项不在注册表，移动它同样 not-found
        assert!(super::apply_workspace_order(&workspace_cfg("", true), "default-workspace", None).is_err());
    }

    #[test]
    fn stored_order_survives_a_move_of_another_entry() {
        // 拖过一次之后再来一次：新移动基于**存过的**顺序，不是回落派生序
        let mut cfg = workspace_cfg("", false);
        cfg.workspace.order = super::apply_workspace_order(&cfg, "ws-2", Some("default-workspace")).unwrap();
        assert_eq!(
            super::apply_workspace_order(&cfg, "ws-1", Some("ws-2")).unwrap(),
            vec!["ws-1", "ws-2", "default-workspace"],
        );
    }

    #[test]
    fn canonical_workspace_path_strips_the_windows_verbatim_prefix() {
        // SH-110 随批：注册表/会话 cwd/界面显示共用普通拼写。canonicalize 在
        // Windows 上必产 \\?\ 前缀，不还原就会同一目录两种字符串（会话错组 +
        // tooltip 露出 verbatim）。不存在的路径如实回 None，不猜拼写。
        let root = crate::test_support::TestTempDir::new("cwp");
        let nested = root.join("子目录");
        std::fs::create_dir_all(&nested).expect("临时目录可建");
        let resolved = super::canonical_workspace_path(&nested).expect("存在的路径可规范");
        // 断言用 canonical 形态比较：CI runner 的 TEMP 可能以 8.3 短名（RUNNER~1）给出，
        // 直接比字符串会把「短名 vs 长名」误判成功能缺陷。
        assert_eq!(
            std::fs::canonicalize(&resolved).expect("解析结果可再规范"),
            nested.canonicalize().expect("临时目录可规范"),
        );
        assert!(!resolved.starts_with(r"\\?\"), "落盘拼写不得带 verbatim 前缀");
        // 相对段与符号链接语义由 canonicalize 承担：.. 归一到同一拼写
        assert_eq!(
            super::canonical_workspace_path(&nested.join("..").join("子目录")),
            Some(resolved),
        );
        assert_eq!(super::canonical_workspace_path(&root.join("不存在")), None);
    }

    fn msg(role: Role, text: &str) -> RolloutItem {
        RolloutItem::ResponseItem {
            item: HistoryItem::Message { role, text: text.into(), reasoning: None },
            at_ms: 0,
        }
    }

    fn call(name: &str, args: &str, id: &str) -> RolloutItem {
        RolloutItem::ResponseItem {
            item: HistoryItem::FunctionCall { name: name.into(), arguments: args.into(), call_id: id.into() },
            at_ms: 0,
        }
    }

    fn out(id: &str, output: &str) -> RolloutItem {
        RolloutItem::ResponseItem {
            item: HistoryItem::FunctionCallOutput { call_id: id.into(), output: output.into() },
            at_ms: 0,
        }
    }

    #[test]
    fn tool_output_merges_into_its_call_instead_of_standing_alone() {
        // 档案形状是"先全部 call、再全部 output"，投影必须按调用顺序把输出带上
        let items = vec![
            msg(Role::User, "跑一下"),
            call("unified_exec", "{\"command\":\"ls\"}", "c1"),
            call("read_file", "{\"path\":\"a.txt\"}", "c2"),
            out("c2", "文件内容"),
            out("c1", "a.txt\nb.txt"),
        ];
        let entries = project_entries(&items).expect("投影成功");
        assert_eq!(entries.len(), 3, "两条调用 + 一条用户消息，输出不单独成条");
        match &entries[1] {
            SessionEntry::ToolCall { call_id, tool, output, .. } => {
                assert_eq!(call_id, "c1");
                assert_eq!(tool, "unified_exec");
                assert_eq!(output.as_deref(), Some("a.txt\nb.txt"), "输出按 call_id 配对，不是按到达顺序");
            }
            other => panic!("期望 ToolCall，得到 {other:?}"),
        }
        match &entries[2] {
            SessionEntry::ToolCall { call_id, output, .. } => {
                assert_eq!(call_id, "c2");
                assert_eq!(output.as_deref(), Some("文件内容"));
            }
            other => panic!("期望 ToolCall，得到 {other:?}"),
        }
    }

    #[test]
    fn unpaired_call_keeps_output_none() {
        // 取消或审批拒绝的轮次：有 call 无 output，投影不能凭空补一个成功输出
        let items = vec![call("unified_exec", "{}", "c1")];
        match &project_entries(&items).expect("投影成功")[0] {
            SessionEntry::ToolCall { output, .. } => assert_eq!(output, &None),
            other => panic!("期望 ToolCall，得到 {other:?}"),
        }
    }

    #[test]
    fn projection_carries_archive_timestamp_verbatim_and_keeps_old_archive_zero() {
        // 落档时刻是事件发生时刻，投影只搬运、绝不"用打开会话那一刻"补（A0-4）。
        let items = vec![
            RolloutItem::ResponseItem {
                item: HistoryItem::Message { role: Role::User, text: "带时刻".into(), reasoning: None },
                at_ms: 1_712_345_678_901,
            },
            RolloutItem::ResponseItem {
                item: HistoryItem::FunctionCall { name: "t".into(), arguments: "{}".into(), call_id: "c1".into() },
                at_ms: 1_712_345_679_000,
            },
            // 旧档案没有 at_ms：读回来是 0，必须原样出去，交给前端标"记录时刻不可信"。
            msg(Role::Assistant, "旧消息"),
        ];
        let entries = project_entries(&items).expect("投影成功");
        match &entries[0] {
            SessionEntry::Message { at_ms, .. } => assert_eq!(*at_ms, 1_712_345_678_901),
            other => panic!("期望 Message，得到 {other:?}"),
        }
        match &entries[1] {
            SessionEntry::ToolCall { at_ms, .. } => assert_eq!(*at_ms, 1_712_345_679_000),
            other => panic!("期望 ToolCall，得到 {other:?}"),
        }
        match &entries[2] {
            SessionEntry::Message { at_ms, .. } => {
                assert_eq!(*at_ms, 0, "旧档案不得被补成当前时刻，否则就是假装记录可信")
            }
            other => panic!("期望 Message，得到 {other:?}"),
        }
    }

    /// 压缩锚点在视图里的语义必须与 resume 完全一致：**摘要取代区间**。
    ///
    /// 反向对照：撤掉 `apply_compaction` 的区间删除（只插摘要、不删原文），
    /// 本用例立刻变红——那正是修复前的行为：用户重开会话看到「摘要 + 原文」并存，
    /// 会以为压缩根本没生效，而模型那边早就只看到摘要了。
    #[test]
    fn compaction_anchor_replaces_the_range_in_the_view() {
        let items = vec![
            // 首行 meta 不占历史序号：锚点若按"档案行号"数，从这一行起就会错位
            RolloutItem::SessionMeta {
                session_id: "s".into(),
                agent_id: "a".into(),
                parent_id: None,
                created_at: 1,
                config_snapshot: ConfigSnapshot::default(),
            },
            msg(Role::User, "早期任务"),
            msg(Role::Assistant, "早期回复"),
            RolloutItem::ResponseItem {
                item: HistoryItem::FunctionCall { name: "t".into(), arguments: "{}".into(), call_id: "c1".into() },
                at_ms: 5,
            },
            // 工具输出占历史序号：区间按「历史序列」数，不是按档案行数
            RolloutItem::ResponseItem {
                item: HistoryItem::FunctionCallOutput { call_id: "c1".into(), output: "旧输出".into() },
                at_ms: 6,
            },
            RolloutItem::Compacted { range_start: 0, range_end: 2, summary: "已压缩的上下文".into() },
            msg(Role::Assistant, "压缩后的新回复"),
        ];
        let entries = project_entries(&items).expect("锚点区间合法");
        // 期望：摘要（取代前两条）+ 区间外的工具调用（输出仍在）+ 锚点之后的新回复
        assert_eq!(entries.len(), 3, "被取代的条目不得再出现在视图里：{entries:?}");
        match &entries[0] {
            // 角色取 user 与回放同源：摘要没有 reasoning_content，以 assistant 身份
            // 喂 thinking 端点会 400（SH-032 的真机回归就是这么撞出来的）。
            SessionEntry::Message { role, text, .. } => {
                assert_eq!(role, "user");
                assert_eq!(text, "已压缩的上下文");
            }
            other => panic!("期望 Message，得到 {other:?}"),
        }
        match &entries[1] {
            SessionEntry::ToolCall { call_id, output, .. } => {
                assert_eq!(call_id, "c1");
                assert_eq!(output.as_deref(), Some("旧输出"));
            }
            other => panic!("期望 ToolCall，得到 {other:?}"),
        }
        assert!(
            !entries.iter().any(|e| matches!(
                e,
                SessionEntry::Message { text, .. } if text == "早期任务"
            )),
            "被压缩区间里的原文不得混进正常视图：{entries:?}"
        );
    }

    /// 夹在压缩区间里的 `TurnEnd` 是审计记录：不占锚点序号，也不该被区间吞掉。
    #[test]
    fn compaction_anchor_keeps_interleaved_turn_end_audit_records() {
        let items = vec![
            msg(Role::User, "第一条"),
            RolloutItem::TurnEnd {
                turn: 1,
                reason: "completed".into(),
                at_ms: 3,
                error: None,
            },
            msg(Role::Assistant, "第二条"),
            RolloutItem::Compacted { range_start: 0, range_end: 2, summary: "摘要".into() },
        ];
        let entries = project_entries(&items).expect("锚点区间合法");
        assert_eq!(entries.len(), 2, "摘要取代两条历史，TurnEnd 原样留着：{entries:?}");
        assert!(matches!(&entries[0], SessionEntry::Message { text, .. } if text == "摘要"));
        assert!(matches!(&entries[1], SessionEntry::TurnEnd { turn: 1, .. }));
    }

    /// 越界锚点必须报错，而不是"能显示多少显示多少"：resume 会直接拒绝加载，
    /// 视图若给出一份看似正常的历史，两边就又对不上了。
    #[test]
    fn out_of_range_compaction_anchor_is_rejected() {
        let items = vec![
            msg(Role::User, "only"),
            RolloutItem::Compacted { range_start: 0, range_end: 5, summary: "摘要".into() },
        ];
        let error = project_entries(&items).expect_err("越界锚点必须拒绝");
        assert!(error.contains("[0,5)"), "{error}");
    }

    #[test]
    fn meta_and_subagent_ref_do_not_become_entries() {
        let items = vec![
            RolloutItem::SessionMeta {
                session_id: "s".into(),
                agent_id: "a".into(),
                parent_id: None,
                created_at: 1,
                config_snapshot: ConfigSnapshot::default(),
            },
            RolloutItem::SubagentRef { child_id: "c".into(), rollout_path: "p".into() },
            msg(Role::Assistant, "正文"),
        ];
        let entries = project_entries(&items).expect("投影成功");
        assert_eq!(entries.len(), 1, "首行 meta 与子代理引用不是历史；SH-056 的泳道 UI 才有后者的展示形态");
        assert!(matches!(&entries[0], SessionEntry::Message { role, .. } if role == "assistant"));
    }

    #[test]
    fn reasoning_survives_projection_when_present() {
        // SH-083 的 reasoning_content 必须能在打开会话时一起回来，
        // 否则"重开旧会话"和"刚跑完"看到的上下文不一样。
        let items = vec![RolloutItem::ResponseItem {
            item: HistoryItem::Message { role: Role::Assistant, text: "答案".into(), reasoning: Some("思路".into()) },
            at_ms: 0,
        }];
        match &project_entries(&items).expect("投影成功")[0] {
            SessionEntry::Message { reasoning, .. } => assert_eq!(reasoning.as_deref(), Some("思路")),
            other => panic!("期望 Message，得到 {other:?}"),
        }
    }

    #[test]
    fn image_message_projects_structured_refs_not_placeholder_text() {
        // 图片引用走结构化字段：正文是用户原话，不再被拼上 `[图片: …]`。
        // 重放也因此不必依赖旧档案正文里的 marker。
        let reference = crate::images::ImageRef {
            path: "attachments/s/01J0000000000000000000000A.bin".into(),
            sha256: "ab".repeat(32),
            width: 4,
            height: 3,
            tokens: 1,
            mime: "image/png".into(),
            attachment_id: Some("01J0000000000000000000000A".into()),
            bytes: Some(4),
        };
        let items = vec![RolloutItem::ResponseItem {
            item: HistoryItem::ImageMessage {
                role: Role::User,
                text: "看这张".into(),
                images: vec![reference.clone()],
            },
            at_ms: 7,
        }];
        match &project_entries(&items).expect("投影成功")[0] {
            SessionEntry::Message { text, images: refs, at_ms, .. } => {
                assert_eq!(text, "看这张", "正文不得被拼上占位文本");
                assert_eq!(refs, &vec![reference]);
                assert_eq!(*at_ms, 7);
            }
            other => panic!("期望 Message，得到 {other:?}"),
        }
    }

    #[test]
    fn plain_message_keeps_old_wire_shape_without_images_key() {
        // 无图消息的序列化形状必须与旧档案完全一致（additive 字段不外泄）。
        let entry = SessionEntry::Message {
            role: "user".into(),
            text: "x".into(),
            reasoning: None,
            at_ms: 0,
            images: Vec::new(),
        };
        let wire = serde_json::to_value(&entry).expect("serialize");
        assert!(wire.get("images").is_none(), "空图片列表不得写出 images 键");
    }

    /// 档位清单与枚举同源（SH-040）。
    ///
    /// 这里刻意用**序列化结果**而不是手写第二份字符串来比对：如果哪天给枚举加一档
    /// 却忘了更新报错文案里的清单，用户就会照着一份不完整的清单去填，然后撞上
    /// "取值非法"——安全设置的提示本身成了误导源。
    #[test]
    fn safety_mode_names_match_enums() {
        let sandbox_all = [
            config::SandboxMode::ReadOnly,
            config::SandboxMode::WorkspaceWrite,
            config::SandboxMode::DangerFullAccess,
        ];
        let sandbox_joined = sandbox_all
            .iter()
            .map(|m| serde_json::to_value(*m).unwrap().as_str().unwrap().to_string())
            .collect::<Vec<_>>()
            .join(" / ");
        assert_eq!(sandbox_joined, SANDBOX_MODE_NAMES, "沙箱档位清单与枚举漂移了");

        let approval_all = [
            config::ApprovalMode::Never,
            config::ApprovalMode::OnFailure,
            config::ApprovalMode::OnRequest,
            config::ApprovalMode::FullAccess,
        ];
        let approval_joined = approval_all
            .iter()
            .map(|m| serde_json::to_value(*m).unwrap().as_str().unwrap().to_string())
            .collect::<Vec<_>>()
            .join(" / ");
        assert_eq!(approval_joined, APPROVAL_MODE_NAMES, "审批档位清单与枚举漂移了");
    }

    /// 两个模式必须真的能 round-trip 回自身：只比对名字的话，serde 属性一改就会
    /// 让"界面显示的值"和"存进 config.toml 的值"变成两套东西。
    #[test]
    fn safety_modes_round_trip_through_wire_names() {
        for name in SANDBOX_MODE_NAMES.split(" / ") {
            let parsed: config::SandboxMode =
                serde_json::from_value(serde_json::Value::String(name.to_string())).unwrap();
            assert_eq!(serde_json::to_value(parsed).unwrap().as_str().unwrap(), name);
        }
        for name in APPROVAL_MODE_NAMES.split(" / ") {
            let parsed: config::ApprovalMode =
                serde_json::from_value(serde_json::Value::String(name.to_string())).unwrap();
            assert_eq!(serde_json::to_value(parsed).unwrap().as_str().unwrap(), name);
        }
        // 非法值必须解析失败——save_settings 的报错分支全靠这个前提
        assert!(serde_json::from_value::<config::SandboxMode>(
            serde_json::Value::String("full_access".into())
        )
        .is_err());
    }
}

/// 导出路径收紧的用例（审计 H2 批 1）。
#[cfg(test)]
mod export_scope_tests {
    use super::validate_export_sources;
    use crate::session_export::{write_zip, ExportEntry};

    fn source_entry(source: &std::path::Path, logical: &str) -> ExportEntry {
        ExportEntry {
            path: logical.to_string(),
            content: None,
            source_path: Some(source.to_string_lossy().into_owned()),
        }
    }

    #[test]
    fn attachment_sources_must_stay_inside_the_attachments_root() {
        let dir = crate::test_support::TestTempDir::new("export-scope");
        let attachments = dir.join("attachments");
        std::fs::create_dir_all(attachments.join("sess-1")).unwrap();
        let inside = attachments.join("sess-1").join("a.bin");
        std::fs::write(&inside, b"payload").unwrap();
        assert!(
            validate_export_sources(&[source_entry(&inside, "media/a.bin")], &attachments).is_ok(),
            "附件目录内的来源应放行"
        );

        // 目录外：文件存在，但不在附件根之下。
        let outside = dir.join("outside.bin");
        std::fs::write(&outside, b"payload").unwrap();
        let err = validate_export_sources(&[source_entry(&outside, "media/outside.bin")], &attachments)
            .unwrap_err();
        assert!(err.contains("附件路径越界"), "错误须点名越界: {err}");

        // 反向对照：拒绝来自本函数而非底层 I/O——改前命令直接调 write_zip，
        // 同一个工作区外目标会被真的写出去（这里证明底层不设位置限制）。
        let dest = dir.join("outside.zip");
        let report = write_zip(
            &dest,
            &[ExportEntry {
                path: "session.v4.jsonl".into(),
                content: Some("x".into()),
                source_path: None,
            }],
        )
        .expect("底层写盘本身不做位置校验");
        assert!(std::path::Path::new(&report.path).is_file());
    }

    #[test]
    fn missing_attachment_source_is_rejected() {
        let dir = crate::test_support::TestTempDir::new("export-missing");
        let attachments = dir.join("attachments");
        std::fs::create_dir_all(&attachments).unwrap();
        let missing = dir.join("nope.bin");
        let err = validate_export_sources(&[source_entry(&missing, "media/x.bin")], &attachments)
            .unwrap_err();
        assert!(err.contains("附件路径越界"), "不存在的来源同样拒绝: {err}");
    }
}

/// 「打开方式」路径范围收紧的用例（审计 H2 批 2）。
#[cfg(test)]
mod workspace_scope_tests {
    use super::resolve_in_registered_workspace;
    use crate::app_state::AppState;
    use crate::config::{AppConfig, WorkspaceProfile};

    fn state_with(
        dir: &crate::test_support::TestTempDir,
        default_root: &std::path::Path,
        extra: &[&std::path::Path],
    ) -> AppState {
        let mut config = AppConfig::default();
        config.workspace.root = default_root.to_string_lossy().into_owned();
        config.workspaces = extra
            .iter()
            .enumerate()
            .map(|(index, path)| WorkspaceProfile {
                id: format!("w-extra-{index}"),
                path: path.to_string_lossy().into_owned(),
                title: String::new(),
            })
            .collect();
        AppState::new(config, dir.join("config.toml"), dir.join("data"))
    }

    #[test]
    fn targets_outside_registered_workspaces_are_rejected() {
        let dir = crate::test_support::TestTempDir::new("ws-scope");
        let ws = dir.join("ws");
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        let outside = dir.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let state = state_with(&dir, &ws, &[]);

        // 工作区内：根与子目录都放行。
        assert!(resolve_in_registered_workspace(&state, &ws.to_string_lossy()).is_ok());
        assert!(resolve_in_registered_workspace(&state, &ws.join("sub").to_string_lossy()).is_ok());

        // 反向对照：改前这里直接 existing_absolute + spawn，工作区外照样打开。
        let err = resolve_in_registered_workspace(&state, &outside.to_string_lossy()).unwrap_err();
        assert!(err.contains("不在任何已注册工作区内"), "{err}");

        // 相对路径仍按旧契约拒绝，不放宽成「相对工作区根解析」。
        let err = resolve_in_registered_workspace(&state, "sub").unwrap_err();
        assert!(err.contains("绝对路径"), "{err}");
    }

    #[test]
    fn extra_workspaces_are_allowed_too() {
        let dir = crate::test_support::TestTempDir::new("ws-extra");
        let ws = dir.join("ws");
        let extra = dir.join("extra");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&extra).unwrap();
        let state = state_with(&dir, &ws, &[extra.as_path()]);
        assert!(
            resolve_in_registered_workspace(&state, &extra.to_string_lossy()).is_ok(),
            "多工作区场景不能因为只认默认根而失效"
        );
    }

    #[test]
    fn directory_application_must_be_a_catalog_id() {
        let dir = crate::test_support::TestTempDir::new("ws-app");
        let target = dir.join("some-dir");
        std::fs::create_dir_all(&target).unwrap();
        // 改前：目录场景把 application 当 exe 路径直接 spawn（任意程序 + 任意目录）。
        let err = crate::open_in_app::open_workspace_path_at(
            &target,
            None,
            Some("C:\\Windows\\System32\\calc.exe"),
        )
        .unwrap_err();
        assert!(err.contains("不在目录中"), "任意 exe 路径必须被拒: {err}");
    }
}

/// 提交前置（marker 剥离 / 空任务 / 视觉门禁）的用例。
#[cfg(test)]
mod preflight_send_tests {
    use super::{preflight_send, vision_gate_for, ImageAttachmentInput, SendTaskError};
    use crate::app_state::AppState;
    use crate::config::AppConfig;

    fn state(dir: &crate::test_support::TestTempDir) -> AppState {
        AppState::new(AppConfig::default(), dir.join("config.toml"), dir.join("data"))
    }

    fn image_input() -> ImageAttachmentInput {
        ImageAttachmentInput {
            attachment_id: "01J0000000000000000000000A".into(),
            media_type: "image/png".into(),
            bytes: 10,
            width: 4,
            height: 3,
            name: Some("a.png".into()),
        }
    }

    #[test]
    fn marker_never_reaches_the_model_text() {
        let dir = crate::test_support::TestTempDir::new("preflight-strip");
        let state = state(&dir);
        let stripped = preflight_send(
            &state,
            "看这张\n<!-- shaniu-image:%7B%7D -->\n解释一下",
            &[],
        )
        .expect("无图也应通过");
        assert_eq!(stripped, "看这张\n解释一下");
        assert!(!stripped.contains("shaniu-image"));
    }

    #[test]
    fn marker_only_text_without_images_is_an_empty_task() {
        let dir = crate::test_support::TestTempDir::new("preflight-empty");
        let state = state(&dir);
        let error = preflight_send(&state, "<!-- shaniu-image:x -->", &[]).unwrap_err();
        assert!(matches!(error, SendTaskError::Plain(message) if message == "任务为空"));
    }

    #[test]
    fn image_without_vision_is_rejected_and_writes_nothing() {
        let dir = crate::test_support::TestTempDir::new("preflight-novision");
        let state = state(&dir);
        let active = state.config().effective_model();
        assert!(!active.vision, "默认配置没有声明视觉，用例前提");
        let error = preflight_send(&state, "看这张", &[image_input()]).unwrap_err();
        match error {
            SendTaskError::NoVision(info) => {
                assert_eq!(info.code, "MODEL_NO_VISION");
                assert_eq!(info.model, active.model);
                assert!(
                    info.message.contains(&active.model),
                    "文案必须点名当前模型: {}",
                    info.message
                );
            }
            other => panic!("应为结构化 NoVision，实际 {other:?}"),
        }
        // 门禁在建档之前：数据根下不该出现任何档案。
        let rollouts = dir.join("data").join(crate::rollout::ROLLOUT_DIR);
        let archived = std::fs::read_dir(&rollouts)
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(archived, 0, "门禁拒绝后不得写档案");
    }

    #[test]
    fn vision_gate_allows_images_when_model_declares_vision() {
        let images = [image_input()];
        assert!(vision_gate_for("vision-model", true, &images).is_ok());
        // 无图时视觉开关无关。
        assert!(vision_gate_for("text-only", false, &[]).is_ok());
        // 反向：视觉关闭 + 有图必拒。
        let error = vision_gate_for("text-only", false, &images).unwrap_err();
        assert!(matches!(error, SendTaskError::NoVision(info) if info.code == "MODEL_NO_VISION"));
    }
}

/// 附件 → `ImageRef` 的核对用例：前端声明与实际文件内容不一致必须拒绝。
#[cfg(test)]
mod attachment_images_tests {
    use super::{resolve_attachment_images, ImageAttachmentInput};
    use crate::images::{image_store, sha256_hex};
    use base64::Engine as _;

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

    fn stored_png(
        root: &crate::test_support::TestTempDir,
        width: u32,
        height: u32,
    ) -> (String, Vec<u8>) {
        let bytes = png(width, height);
        let stored = crate::attachments::store(
            root,
            "sess-1",
            "shot.png",
            "image/png",
            &base64::engine::general_purpose::STANDARD.encode(&bytes),
            Some(width),
            Some(height),
        )
        .expect("store png");
        (stored.receipt_id, bytes)
    }

    fn input(id: &str, mime: &str, bytes: u64, width: u32, height: u32) -> ImageAttachmentInput {
        ImageAttachmentInput {
            attachment_id: id.to_string(),
            media_type: mime.to_string(),
            bytes,
            width,
            height,
            name: Some("shot.png".into()),
        }
    }

    #[test]
    fn builds_image_refs_from_actual_file_content() {
        let root = crate::test_support::TestTempDir::new("images-from-attach");
        let (id, bytes) = stored_png(&root, 12, 9);
        let refs = resolve_attachment_images(
            &root,
            "sess-1",
            &[input(&id, "image/png", bytes.len() as u64, 12, 9)],
        )
        .expect("resolve");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].attachment_id.as_deref(), Some(id.as_str()));
        assert_eq!(refs[0].path, format!("attachments/sess-1/{id}.bin"));
        assert_eq!((refs[0].width, refs[0].height), (12, 9));
        assert_eq!(refs[0].mime, "image/png");
        assert_eq!(refs[0].sha256, sha256_hex(&bytes));
        assert!(image_store::data_url(&refs[0].sha256).is_some());
    }

    #[test]
    fn rejects_declared_mime_that_contradicts_file_header() {
        let root = crate::test_support::TestTempDir::new("images-bad-mime");
        let (id, bytes) = stored_png(&root, 21, 13);
        // 前端声称是 JPEG，文件头是 PNG。
        let error = resolve_attachment_images(
            &root,
            "sess-1",
            &[input(&id, "image/jpeg", bytes.len() as u64, 21, 13)],
        )
        .expect_err("mime 不一致必须拒绝");
        assert!(error.contains("实际格式是 image/png"), "{error}");
        assert!(
            image_store::data_url(&sha256_hex(&bytes)).is_none(),
            "拒绝的附件不得进缓存"
        );
    }

    #[test]
    fn rejects_declared_size_that_contradicts_file_header() {
        let root = crate::test_support::TestTempDir::new("images-bad-size");
        let (id, bytes) = stored_png(&root, 33, 17);
        let error = resolve_attachment_images(
            &root,
            "sess-1",
            &[input(&id, "image/png", bytes.len() as u64, 320, 200)],
        )
        .expect_err("尺寸不一致必须拒绝");
        assert!(error.contains("实际尺寸是 33×17"), "{error}");
    }

    #[test]
    fn rejects_declared_byte_count_that_contradicts_file() {
        let root = crate::test_support::TestTempDir::new("images-bad-bytes");
        let (id, bytes) = stored_png(&root, 7, 5);
        let error = resolve_attachment_images(
            &root,
            "sess-1",
            &[input(&id, "image/png", bytes.len() as u64 + 1, 7, 5)],
        )
        .expect_err("字节数不一致必须拒绝");
        assert!(error.contains("字节数与声明不一致"), "{error}");
    }

    #[test]
    fn rejects_non_image_attachment() {
        let root = crate::test_support::TestTempDir::new("images-not-image");
        let junk: &[u8] = b"definitely not a png";
        let stored = crate::attachments::store(
            &root,
            "sess-1",
            "fake.png",
            "image/png",
            &base64::engine::general_purpose::STANDARD.encode(junk),
            Some(4),
            Some(4),
        )
        .expect("store bytes");
        let error = resolve_attachment_images(
            &root,
            "sess-1",
            &[input(&stored.receipt_id, "image/png", junk.len() as u64, 4, 4)],
        )
        .expect_err("非图片内容必须拒绝");
        assert!(error.contains("不是受支持的图片"), "{error}");
    }
}
