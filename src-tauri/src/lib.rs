// pub 供集成测试（src-tauri/tests/）作为契约面使用
pub mod agent_loop;
pub mod app_state;
pub mod config;
pub mod events;
pub mod model_client;
pub mod sandbox;
pub mod permission;
pub mod tool_router;
pub mod tools;

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

/// 提交任务：组装后端 → 启动 agent_loop 任务（事件经 TauriEventSink 广播）。
#[tauri::command]
fn send_task(app: tauri::AppHandle, task: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    if task.trim().is_empty() {
        return Err("任务为空".into());
    }
    let cfg = state.config();
    let provider = model_client::provider::ProviderConfig::from_model_config(&cfg.model);
    let secret = config::SecretStore::new(&state.config_root());
    let api_key = secret
        .load_key("openai-responses")?
        .ok_or("未配置 API Key，请先在设置中保存")?;

    let cancel = tokio_util::sync::CancellationToken::new();
    if !state.begin_task(cancel.clone()) {
        return Err("已有任务在运行".into());
    }

    let backend: Arc<dyn ModelBackend> = Arc::new(agent_loop::ResponsesBackend { provider, api_key });
    let mut router = tool_router::ToolRouter::new();
    tools::register_builtin(&mut router);
    let agent = agent_loop::AgentLoop::from_config(
        backend,
        Arc::new(router),
        state.broker.clone(),
        &cfg,
        state.workspace_root(),
    );
    let sink: Arc<dyn events::EventSink> = Arc::new(events::TauriEventSink(app.clone()));

    tauri::async_runtime::spawn(async move {
        let _ = agent.run_task(&task, sink, cancel).await;
        // 任务收口：释放活动任务槽位（无论成败）
        app.state::<AppState>().end_task();
    });
    Ok(())
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
    let has_key = config::SecretStore::new(&state.config_root())
        .load_key("openai-responses")
        .map(|k| k.is_some())?;
    Ok(serde_json::json!({
        "baseUrl": cfg.model.base_url,
        "model": cfg.model.model,
        "maxTurns": cfg.model.max_turns,
        "workspaceRoot": state.workspace_root().to_string_lossy(),
        "hasApiKey": has_key,
    }))
}

/// 保存设置：模型端点/工作区入 config.toml，API Key（可选填）DPAPI 加密落盘。
#[tauri::command]
fn save_settings(
    app: tauri::AppHandle,
    base_url: String,
    model: String,
    workspace_root: String,
    api_key: Option<String>,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut cfg: AppConfig = state.config();
    cfg.model.base_url = base_url.trim_end_matches('/').to_string();
    cfg.model.model = model;
    cfg.workspace.root = workspace_root;
    state.update_config(cfg)?;
    if let Some(key) = api_key {
        let key = key.trim().to_string();
        if !key.is_empty() {
            config::SecretStore::new(&state.config_root()).store_key("openai-responses", &key)?;
        }
    }
    Ok(())
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
    // 数据目录解析结果写入启动输出第一行（docs §9.3）
    let dirs = config::resolve_dirs_from_env();
    println!("[SHANIU] config_root: {}", dirs.config_root.display());
    println!("[SHANIU] data_root:   {}", dirs.data_root.display());

    let (app_config, config_path) =
        AppConfig::load_or_create(&dirs.config_root).unwrap_or_else(|e| {
            eprintln!("[SHANIU] 配置加载失败（fail-closed，使用临时默认配置）: {e}");
            (AppConfig::default(), dirs.config_root.join(config::CONFIG_FILE))
        });

    tauri::Builder::default()
        .manage(AppState::new(app_config, config_path))
        .invoke_handler(tauri::generate_handler![
            dev_ping,
            send_task,
            cancel_task,
            approve,
            store_api_key,
            has_api_key,
            get_settings,
            save_settings
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
