// pub 供集成测试（src-tauri/tests/）作为契约面使用
pub mod agent_loop;
pub mod app_state;
pub mod attachments;
/// 模型能力声明与实测探针（SH-132，docs/04 §4.1）。
pub mod capability;
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
pub mod mcp_client;
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
use config::AppConfig;

/// IPC 握手（M0 验收）：前端 invoke 携带 nonce，后端经事件通道原样回显。
#[tauri::command]
fn dev_ping(app: tauri::AppHandle, nonce: String) -> Result<(), String> {
    events::emit_pong(&app, &nonce)
}

/// 主窗口是否已收到前端的就绪信号（首帧启动页已可显示）。
static FRONTEND_READY: AtomicBool = AtomicBool::new(false);

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
) -> Result<String, String> {
    if task.trim().is_empty() {
        return Err("任务为空".into());
    }
    // 只接受已知档位；拼错就拒绝，不静默回落到更宽的档（§11.3 退化红线）。
    let sandbox = match sandbox_mode.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        Some(raw) => Some(
            config::SandboxMode::parse(raw)
                .ok_or_else(|| format!("未知沙箱档: {raw}"))?,
        ),
        None => None,
    };
    let state = app.state::<AppState>();
    match agent_id.filter(|id| !id.trim().is_empty()) {
        Some(id) => {
            continue_session(&app, &state, id.clone(), &task, sandbox)?;
            Ok(id)
        }
        None => {
            let (agent, cancel, new_id) =
                build_new_session(&state, state.workspace_root().as_path(), sandbox)?;
            // 首条消息 = 标题自动生成的触发点（first-prompt 节奏）：后台辅助请求，
            // 不阻塞主回合；失败/关开关/没密钥都静默保持截断 fallback。
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
fn continue_session(
    app: &tauri::AppHandle,
    state: &AppState,
    agent_id: String,
    task: &str,
    draft_sandbox: Option<config::SandboxMode>,
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
#[tauri::command]
fn resume_session(app: tauri::AppHandle, agent_id: String, task: String) -> Result<(), String> {
    if task.trim().is_empty() {
        return Err("任务为空".into());
    }
    let state = app.state::<AppState>();
    continue_session(&app, &state, agent_id, &task, None)
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
            // 带图消息按普通消息回放：只展示文字与图片路径引用，绝不还原字节
            // （UI 取字节有独立预览通道，不走会话档案）。
            rollout::RolloutItem::ResponseItem {
                item: model_client::HistoryItem::ImageMessage { role, text, images },
            } => {
                let mut shown = text.clone();
                for image in images {
                    shown.push_str(&format!(
                        "\n[图片: {}（{}×{}，约 {} tokens）]",
                        image.path, image.width, image.height, image.tokens
                    ));
                }
                out.push(SessionEntry::Message {
                    role: match role {
                        model_client::Role::User => "user",
                        model_client::Role::Assistant => "assistant",
                        model_client::Role::System => "system",
                    }
                    .to_string(),
                    text: shown,
                    reasoning: None,
                });
            }
            // 首行 meta 不是历史；子代理引用要等 SH-056 的泳道 UI 才有展示形态；
            // 档位事件是控制面记录，不进对话历史。
            rollout::RolloutItem::SessionMeta { .. }
            | rollout::RolloutItem::SubagentRef { .. }
            | rollout::RolloutItem::SandboxMode { .. } => {}
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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
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
    if !state.begin_task(agent_id, cancel.clone()) {
        return Err("已有任务在运行".into());
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
/// 指错（区间语义建立在"锚点按记录顺序、对当时重放长度生效"上）——单任务闸下
/// 守住"目标会话不是活动任务"即可。
#[tauri::command]
async fn compact_session(app: tauri::AppHandle, agent_id: String) -> Result<Option<session_compact::CompactOutcome>, String> {
    let state = app.state::<AppState>();
    if state.active_agent().as_deref() == Some(agent_id.as_str()) {
        return Err("会话正在运行：请等当前任务结束（或停止它）再压缩".into());
    }
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
    // state 的借用止于上面最后一次取值（NLL），不跨下面的 await 点。
    session_compact::compact_now(&provider, &api_key, &resumed, budget).await
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

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(app_state)
        .manage(gateway_hub.clone())
        .manage(user_terminal::UserTerminalManager::default())
        .invoke_handler(tauri::generate_handler![
            dev_ping,
            frontend_ready,
            send_task,
            create_session,
            resume_session,
            list_sessions,
            load_session,
            search_sessions,
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
            open_in_app::open_in_app_launch,
            open_in_app::workspace_path_applications,
            open_in_app::open_workspace_path,
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
            user_terminal::user_terminal_open,
            user_terminal::user_terminal_write,
            user_terminal::user_terminal_resize,
            user_terminal::user_terminal_close,
            user_terminal::user_terminal_close_all,
            user_terminal::user_terminal_active_count,
        ])
        .on_window_event(|window, event| {
            // 关闭 = 隐藏到系统托盘（不退出）；真正退出只走托盘菜单「退出」
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    api.prevent_close();
                    // 隐藏 ≠ 退出：仍有终端在跑时提示一次，避免「关了窗口就没事」的误判。
                    // 只在隐藏这一刻发一次，不重复轰炸；没有存活终端则不发。
                    let count = window
                        .app_handle()
                        .try_state::<user_terminal::UserTerminalManager>()
                        .map(|state| state.active_count())
                        .unwrap_or(0);
                    if count > 0 {
                        let _ = window.emit(
                            user_terminal::TRAY_HINT_EVENT,
                            user_terminal::TrayHint { count },
                        );
                    }
                    let _ = window.hide();
                }
            }
        })
        .setup(|app| {
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
                    "quit" => {
                        // SH-021：退出前先还原 DACL 并删除容器 profile，避免残留 ACE/悬空 profile。
                        sandbox::shutdown();
                        app.exit(0);
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
