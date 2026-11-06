// pub 供集成测试（src-tauri/tests/）作为契约面使用
pub mod agent_loop;
pub mod app_state;
pub mod config;
pub mod compaction;
pub mod events;
pub mod model_client;
pub mod model_discovery;
pub mod sandbox;
pub mod permission;
pub mod rollout;
pub mod session_title;
pub mod tool_router;
pub mod tools;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager,
};

use agent_loop::ModelBackend;
use app_state::AppState;
use config::AppConfig;

/// IPC 握手（M0 验收）：前端 invoke 携带 nonce，后端经事件通道原样回显。
#[tauri::command]
fn dev_ping(app: tauri::AppHandle, nonce: String) -> Result<(), String> {
    events::emit_pong(&app, &nonce)
}

/// 提交任务，并返回**本次使用的 agent_id**。
///
/// `agent_id` 为 None = 新会话（现场建档）；为 Some = 续写那份档案（SH-033 的
/// 「打开历史会话后继续输入」）。返回值是必需的而不是顺手加的：新会话的 id
/// 只有后端知道（`next_session_id` 在这里生成），前端拿不到它就无从决定
/// 下一次提交该续写谁——会话列表也就永远点不回自己刚建的那条。
#[tauri::command]
fn send_task(app: tauri::AppHandle, task: String, agent_id: Option<String>) -> Result<String, String> {
    if task.trim().is_empty() {
        return Err("任务为空".into());
    }
    let state = app.state::<AppState>();
    match agent_id.filter(|id| !id.trim().is_empty()) {
        Some(id) => {
            continue_session(&app, &state, id.clone(), &task)?;
            Ok(id)
        }
        None => {
            let (agent, cancel, new_id) = build_new_session(&state, state.workspace_root().as_path())?;
            // 首条消息 = 标题自动生成的触发点（first-prompt 节奏）：后台辅助请求，
            // 不阻塞主回合；失败/关开关/没密钥都静默保持截断 fallback。
            session_title::spawn_generation(app.clone(), new_id.clone(), task.clone());
            let sink: Arc<dyn events::EventSink> =
                Arc::new(events::TauriEventSink::root(app.clone(), new_id.clone()));
            tauri::async_runtime::spawn(async move {
                let _ = agent.run_task(&task, sink, cancel).await;
                app.state::<AppState>().end_task();
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
fn continue_session(
    app: &tauri::AppHandle,
    state: &AppState,
    agent_id: String,
    task: &str,
) -> Result<(), String> {
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
    history.push(model_client::HistoryItem::Message {
        role: model_client::Role::User,
        text: task.to_string(),
        reasoning: None,
    });

    // 会话绑定的工作区在建档时固化（SH-099）：resume 时从档案 meta 带出来，
    // 工具沙箱与文件工具的作用域跟随原目录而不是全局默认根。
    let session_workspace = PathBuf::from(&resumed.workspace_root);
    let writer = rollout::RolloutWriter::open_existing(&resumed.path)
        .map_err(|e| format!("档案续写打不开: {e}"))?;
    let (agent, cancel) = build_agent_with(
        state,
        &agent_id,
        if session_workspace.as_os_str().is_empty() { state.workspace_root() } else { session_workspace },
        Some(Arc::new(writer)),
    )?;
    let sink: Arc<dyn events::EventSink> = Arc::new(events::TauriEventSink::root(app.clone(), agent_id));
    // spawn 的 future 必须是 'static：借用的 app 只能换成 owned 句柄再 move 进去。
    let app_handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let _ = agent.run_with_history(history, persisted, sink, cancel).await;
        app_handle.state::<AppState>().end_task();
    });
    Ok(())
}

/// 重启后恢复会话并续跑新任务（SH-031）。
#[tauri::command]
fn resume_session(app: tauri::AppHandle, agent_id: String, task: String) -> Result<(), String> {
    if task.trim().is_empty() {
        return Err("任务为空".into());
    }
    let state = app.state::<AppState>();
    continue_session(&app, &state, agent_id, &task)
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
    },
    ToolCall {
        call_id: String,
        tool: String,
        arguments: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },
}

/// 档案历史 → 展示条目。
///
/// 工具输出在档案里排在**同轮全部调用之后**（"先全部 call、再全部 output"，SH-031 的
/// `nearest_safe_cut` 正是靠这个形状才敢只在配对边界截断）。展示要按调用顺序把输出带上，
/// 所以先建一遍 `call_id → output` 索引，再按原序输出，`FunctionCallOutput` 本身不再成条。
fn project_entries(items: &[rollout::RolloutItem]) -> Vec<SessionEntry> {
    let mut outputs: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for item in items {
        if let rollout::RolloutItem::ResponseItem {
            item: model_client::HistoryItem::FunctionCallOutput { call_id, output },
        } = item
        {
            outputs.insert(call_id.clone(), output.clone());
        }
    }

    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match item {
            rollout::RolloutItem::ResponseItem {
                item: model_client::HistoryItem::Message { role, text, reasoning },
            } => out.push(SessionEntry::Message {
                role: match role {
                    model_client::Role::User => "user",
                    model_client::Role::Assistant => "assistant",
                    model_client::Role::System => "system",
                }
                .to_string(),
                text: text.clone(),
                reasoning: reasoning.clone(),
            }),
            rollout::RolloutItem::ResponseItem {
                item: model_client::HistoryItem::FunctionCall { name, arguments, call_id },
            } => out.push(SessionEntry::ToolCall {
                call_id: call_id.clone(),
                tool: name.clone(),
                arguments: arguments.clone(),
                output: outputs.get(call_id).cloned(),
            }),
            // 已并进对应的 ToolCall，不单独成条。
            rollout::RolloutItem::ResponseItem {
                item: model_client::HistoryItem::FunctionCallOutput { .. },
            } => {}
            // 压缩锚点在 resume 时的语义就是"用 summary 取代那段区间"，
            // 展示也必须一致，否则用户会看到区间原文而模型看到的是摘要——两份真相。
            // 角色取 user：与 resume 回放、thinking 档协议三者必须同源（摘要没有
            // reasoning_content，以 assistant 身份喂端点会 400）。
            rollout::RolloutItem::Compacted { summary, .. } => out.push(SessionEntry::Message {
                role: "user".to_string(),
                text: summary.clone(),
                reasoning: None,
            }),
            // 首行 meta 不是历史；子代理引用要等 SH-056 的泳道 UI 才有展示形态。
            rollout::RolloutItem::SessionMeta { .. } | rollout::RolloutItem::SubagentRef { .. } => {}
        }
    }
    out
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
    Ok(project_entries(&archive.items))
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

fn workspace_view_of(state: &AppState, id: &str, path: &str, title: &str) -> serde_json::Value {
    let rows = rollout::list(&state.data_root().join(rollout::ROLLOUT_DIR));
    let mine: Vec<String> = rows
        .into_iter()
        .filter(|row| row.workspace_root == path)
        .map(|row| row.agent_id)
        .collect();
    let latest = mine.len();
    serde_json::json!({
        "workspaceId": id,
        "path": path,
        "title": title,
        "sessionIds": mine,
        "createdAt": "",
        "updatedAt": if latest == 0 { "" } else { "" },
    })
}

/// 注册新工作区（SH-099）：路径必须已存在且是目录；同一路径幂等返回已有项
/// （上游 created:false 同口径）。id 由宿主生成，title 缺省 = 目录名。
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
    let canonical_str = canonical.to_string_lossy().to_string();
    let cfg = state.config();
    if canonical_str == state.workspace_root().canonicalize().unwrap_or_default().to_string_lossy() {
        // 与默认工作区同一路径：幂等返回默认项
        return Ok(serde_json::json!({
            "workspace": workspace_view_of(&state, DEFAULT_WORKSPACE_ID, state.workspace_root().display().to_string().as_str(), DEFAULT_WORKSPACE_ID),
            "created": false,
        }));
    }
    if let Some(existing) = cfg.workspaces.iter().find(|ws| {
        std::path::Path::new(&ws.path)
            .canonicalize()
            .map(|p| p.to_string_lossy() == canonical_str)
            .unwrap_or(false)
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

/// 改工作区显示名（SH-099）：只写 title，路径不动；与其它家重名拒绝。
/// 默认工作区显示名由前端按 id 特判渲染，不支持改名（SHANIU 的默认项是
/// [workspace].root 派生，没有可写的 title 字段）。
#[tauri::command]
fn rename_workspace(
    app: tauri::AppHandle,
    workspace_id: String,
    title: String,
) -> Result<serde_json::Value, WorkspaceOpError> {
    let state = app.state::<AppState>();
    let id = workspace_id.trim().to_string();
    let title = title.trim().to_string();
    if id == DEFAULT_WORKSPACE_ID {
        return Err(WorkspaceOpError::BadRequest(
            "默认工作区的显示名固定为「默认工作区」，不支持改名".into(),
        ));
    }
    if title.is_empty() {
        return Err(WorkspaceOpError::BadRequest("工作区名不能为空".into()));
    }
    let cfg = state.config();
    let target = cfg
        .workspaces
        .iter()
        .find(|ws| ws.id == id)
        .ok_or_else(|| WorkspaceOpError::NotFound(format!("工作区不存在：{id}")))?;
    if cfg
        .workspaces
        .iter()
        .any(|ws| ws.id != id && ws.display_title() == title)
    {
        return Err(WorkspaceOpError::NameConflict(format!(
            "已有名为 {title} 的工作区，换一个名字"
        )));
    }
    let mut next = cfg.clone();
    if let Some(ws) = next.workspaces.iter_mut().find(|ws| ws.id == id) {
        ws.title = title.clone();
    }
    let _ = target;
    state.update_config(next).map_err(WorkspaceOpError::Rejected)?;
    let updated = state.config().workspace_by_id(&id).cloned();
    let ws = updated.ok_or_else(|| WorkspaceOpError::Rejected("改名后读不回工作区".into()))?;
    Ok(serde_json::json!({
        "workspace": workspace_view_of(&state, &ws.id, &ws.path, &ws.display_title()),
    }))
}

/// 删除工作区注册（SH-099）：只删注册，目录与会话档案全保留——会话在侧栏
/// 落到「未分组」（上游同口径）。默认工作区拒绝（它来自 [workspace].root，
/// 删注册等于停用全局默认根）。
#[tauri::command]
fn delete_workspace(
    app: tauri::AppHandle,
    workspace_id: String,
) -> Result<serde_json::Value, WorkspaceOpError> {
    let state = app.state::<AppState>();
    let id = workspace_id.trim().to_string();
    if id == DEFAULT_WORKSPACE_ID {
        return Err(WorkspaceOpError::BadRequest(
            "默认工作区不可删除：它是 [workspace].root 的注册形态".into(),
        ));
    }
    let mut next = state.config();
    let before = next.workspaces.len();
    next.workspaces.retain(|ws| ws.id != id);
    if next.workspaces.len() == before {
        return Err(WorkspaceOpError::NotFound(format!("工作区不存在：{id}")));
    }
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
fn create_session(app: tauri::AppHandle, workspace_path: Option<String>) -> Result<String, String> {
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
            root.canonicalize().unwrap_or(root)
        }
        None => state.workspace_root(),
    };
    let (writer, agent_id) = new_session_writer(&state, &workspace_root)?;
    //  writer 在这里只负责把首行落盘；后续写入由 send_task 重新 open_existing。
    drop(writer);
    Ok(agent_id)
}

/// 组装一次新会话所需的 AgentLoop + 取消令牌 + 它的 agent_id（现场建档）。
/// 建档失败就直接报错给前端——静默不落盘会让用户以为"能 resume"，实际不能。
fn build_new_session(
    state: &AppState,
    workspace_root: &Path,
) -> Result<(agent_loop::AgentLoop, tokio_util::sync::CancellationToken, String), String> {
    let (writer, agent_id) = new_session_writer(state, workspace_root)?;
    let (agent, cancel) = build_agent_with(state, &agent_id, workspace_root.to_path_buf(), Some(writer))?;
    Ok((agent, cancel, agent_id))
}

fn new_session_writer(
    state: &AppState,
    workspace_root: &Path,
) -> Result<(Arc<rollout::RolloutWriter>, String), String> {
    let cfg = state.config();
    let agent_id = state.next_session_id();
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

fn build_agent_with(
    state: &AppState,
    agent_id: &str,
    workspace_root: PathBuf,
    writer: Option<Arc<rollout::RolloutWriter>>,
) -> Result<(agent_loop::AgentLoop, tokio_util::sync::CancellationToken), String> {
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

    let cancel = tokio_util::sync::CancellationToken::new();
    if !state.begin_task(agent_id, cancel.clone()) {
        return Err("已有任务在运行".into());
    }

    let backend: Arc<dyn ModelBackend> = Arc::new(agent_loop::ResponsesBackend { provider, api_key });
    let mut router = tool_router::ToolRouter::new();
    tools::register_builtin(&mut router, state.plan_state.clone());
    let agent = agent_loop::AgentLoop::from_config(
        backend,
        Arc::new(router),
        state.broker.clone(),
        state.plan_state.clone(),
        &cfg,
        workspace_root,
        writer,
    );
    Ok((agent, cancel))
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
    let mut profiles = Vec::with_capacity(cfg.providers.len());
    for p in &cfg.providers {
        profiles.push(serde_json::json!({
            "id": p.id,
            "label": p.label,
            "provider": p.provider,
            "baseUrl": p.base_url,
            "model": p.model,
            "secretRef": p.secret_name(),
            "hasApiKey": secret.load_key(&p.secret_name())?.is_some(),
            // 模型清单原样回显，空清单不替 `model` 补一行：补出来的那行看起来像
            // "用户自己加的"，设置页就会给一个根本没被声明过的档位提供删除按钮。
            "models": p.models.iter().map(|m| serde_json::json!({
                "id": m.id,
                "name": m.name,
                "contextWindow": m.context_window,
                "maxTokens": m.max_tokens,
                "input": m.input,
                // 推理档声明（SH-096）：档位 → wire 值；空串 = off 档（不发字段）。
                // 前端视图把它翻成 null，写回时再归一成空串。
                "reasoningEfforts": m.reasoning_efforts,
            })).collect::<Vec<_>>(),
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

/// 报错时列给用户的合法档位名。它必须与 `config.rs` 的枚举同源——
/// `safety_mode_names_match_enums` 用穷举后的序列化结果逐字比对，加一档忘改这里会直接红。
const SANDBOX_MODE_NAMES: &str = "read_only / workspace_write / danger_full_access";
const APPROVAL_MODE_NAMES: &str = "ask / auto_read_only";

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
    cfg.workspace.root = workspace_root;
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

/// 产品目录视图：前端用它把默认工作区基线落到宿主解析出的 `<数据根>\Workspace`。
#[tauri::command]
fn get_product_dirs() -> config::ProductDirsView {
    config::resolve_dirs_from_env().view()
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

/// 从托盘恢复主窗口：取消最小化 + 显示 + 聚焦。
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

pub fn run() {
    // 数据目录解析结果写入日志第一行（docs §9.3 的「解析结果写入日志第一行」即指此条）
    let dirs = config::resolve_dirs_from_env();
    // 产品目录结构提前建好（幂等）：secrets/settings-backup、sessions/rollouts/logs/cache/crash
    // 与默认工作区 Workspace。失败不拦启动——各写路径自己 fail-closed，这里只如实报告。
    if let Err(e) = config::ensure_product_dirs(&dirs) {
        eprintln!("[SHANIU] 产品目录创建失败（数据目录不可写？）: {e}");
    }
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

    let (app_config, config_path) =
        AppConfig::load_or_create(&dirs.config_root).unwrap_or_else(|e| {
            tracing::warn!("配置加载失败（fail-closed，使用临时默认配置）: {e}");
            (AppConfig::default(), dirs.config_root.join(config::CONFIG_FILE))
        });

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::new(app_config, config_path, dirs.data_root.clone()))
        .invoke_handler(tauri::generate_handler![
            dev_ping,
            send_task,
            create_session,
            resume_session,
            list_sessions,
            load_session,
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
            cancel_task,
            approve,
            store_api_key,
            has_api_key,
            get_settings,
            save_settings,
            save_providers,
            model_discovery::discover_models,
            delete_api_key,
            open_in_explorer,
            pick_folder,
            get_product_dirs,
            read_clipboard_text,
            write_clipboard_text
        ])
        .on_window_event(|window, event| {
            // 关闭 = 隐藏到系统托盘（不退出）；真正退出只走托盘菜单「退出」
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .setup(|app| {
            // 系统托盘：菜单 = 显示 / 退出；左键单击图标 = 显示主窗口
            let show = MenuItem::with_id(app, "show", "显示", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let tray_menu = Menu::with_items(app, &[&show, &quit])?;
            TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().expect("缺少应用图标").clone())
                .tooltip("SHANIU")
                .menu(&tray_menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "show" => show_main_window(app),
                    "quit" => app.exit(0),
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
mod session_projection_tests {
    use super::{project_entries, SessionEntry, APPROVAL_MODE_NAMES, SANDBOX_MODE_NAMES};
    use crate::config;
    use crate::model_client::{HistoryItem, Role};
    use crate::rollout::{ConfigSnapshot, RolloutItem};

    fn msg(role: Role, text: &str) -> RolloutItem {
        RolloutItem::ResponseItem {
            item: HistoryItem::Message { role, text: text.into(), reasoning: None },
        }
    }

    fn call(name: &str, args: &str, id: &str) -> RolloutItem {
        RolloutItem::ResponseItem {
            item: HistoryItem::FunctionCall { name: name.into(), arguments: args.into(), call_id: id.into() },
        }
    }

    fn out(id: &str, output: &str) -> RolloutItem {
        RolloutItem::ResponseItem {
            item: HistoryItem::FunctionCallOutput { call_id: id.into(), output: output.into() },
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
        let entries = project_entries(&items);
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
        match &project_entries(&items)[0] {
            SessionEntry::ToolCall { output, .. } => assert_eq!(output, &None),
            other => panic!("期望 ToolCall，得到 {other:?}"),
        }
    }

    #[test]
    fn compaction_anchor_projects_as_user_summary() {
        // resume 时锚点的语义就是"摘要取代区间"，展示必须一致，
        // 否则用户看到区间原文而模型看到的是摘要——两份真相。
        // 角色取 user 与回放同源：摘要没有 reasoning_content，以 assistant 身份
        // 喂 thinking 端点会 400（SH-032 的真机回归就是这么撞出来的）。
        let items = vec![
            msg(Role::User, "早期任务"),
            RolloutItem::Compacted { range_start: 0, range_end: 3, summary: "已压缩的上下文".into() },
        ];
        let entries = project_entries(&items);
        assert_eq!(entries.len(), 2);
        match &entries[1] {
            SessionEntry::Message { role, text, .. } => {
                assert_eq!(role, "user");
                assert_eq!(text, "已压缩的上下文");
            }
            other => panic!("期望 Message，得到 {other:?}"),
        }
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
        let entries = project_entries(&items);
        assert_eq!(entries.len(), 1, "首行 meta 与子代理引用不是历史；SH-056 的泳道 UI 才有后者的展示形态");
        assert!(matches!(&entries[0], SessionEntry::Message { role, .. } if role == "assistant"));
    }

    #[test]
    fn reasoning_survives_projection_when_present() {
        // SH-083 的 reasoning_content 必须能在打开会话时一起回来，
        // 否则"重开旧会话"和"刚跑完"看到的上下文不一样。
        let items = vec![RolloutItem::ResponseItem {
            item: HistoryItem::Message { role: Role::Assistant, text: "答案".into(), reasoning: Some("思路".into()) },
        }];
        match &project_entries(&items)[0] {
            SessionEntry::Message { reasoning, .. } => assert_eq!(reasoning.as_deref(), Some("思路")),
            other => panic!("期望 Message，得到 {other:?}"),
        }
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

        let approval_all = [config::ApprovalMode::Ask, config::ApprovalMode::AutoReadOnly];
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
