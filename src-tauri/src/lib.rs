//! Tauri 外壳：只做桥接。所有业务逻辑在 crewforge-core。

mod approval;
mod commands;
mod state;

use crewforge_core::db::{Db, default_data_dir};
use std::sync::Arc;
use tauri::{Emitter, Manager};

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let db = Arc::new(Db::open(&default_data_dir())?);
            // 每条落库的事件实时推送给界面
            let handle = app.handle().clone();
            db.set_event_hook(move |e| {
                let _ = handle.emit("crewforge://event", e);
            });
            app.manage(state::AppState::new(db));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_employees,
            commands::add_employee,
            commands::list_sessions,
            commands::recent_events,
            commands::running_employees,
            commands::run_task,
            commands::respond_approval,
            commands::data_dir,
            commands::get_concurrency,
            commands::set_concurrency,
        ])
        .run(tauri::generate_context!())
        .expect("启动 Crewforge 失败");
}
