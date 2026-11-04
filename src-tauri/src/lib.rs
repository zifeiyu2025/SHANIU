// pub 供集成测试（src-tauri/tests/）作为契约面使用
pub mod app_state;
pub mod config;
pub mod events;

use app_state::AppState;
use config::AppConfig;

/// IPC 握手（M0 验收）：前端 invoke 携带 nonce，后端经事件通道原样回显。
#[tauri::command]
fn dev_ping(app: tauri::AppHandle, nonce: String) -> Result<(), String> {
    events::emit_pong(&app, &nonce)
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
        .invoke_handler(tauri::generate_handler![dev_ping])
        .run(tauri::generate_context!())
        .expect("SHANIU 启动失败");
}
