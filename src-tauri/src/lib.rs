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
            let data_dir = default_data_dir();
            let db = Arc::new(Db::open(&data_dir)?);
            // 每条落库的事件实时推送给界面
            let handle = app.handle().clone();
            db.set_event_hook(move |e| {
                let _ = handle.emit("crewforge://event", e);
            });
            app.manage(state::AppState::new(db, &data_dir));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_issues,
            commands::pending_decisions,
            commands::create_issue,
            commands::update_issue,
            commands::add_issue_option,
            commands::set_issue_status,
            commands::decide_issue,
            commands::list_meetings,
            commands::meeting_detail,
            commands::create_meeting,
            commands::link_meeting_issue,
            commands::save_minutes,
            commands::close_meeting,
            commands::post_meeting_message,
            commands::draft_minutes,
            commands::get_company,
            commands::update_company,
            commands::employee_cards,
            commands::update_employee,
            commands::archive_employee,
            commands::list_projects,
            commands::create_project,
            commands::update_project,
            commands::archive_project,
            commands::list_members,
            commands::add_member,
            commands::set_member_role,
            commands::remove_member,
            commands::claude_version,
            commands::list_employees,
            commands::add_employee,
            commands::list_sessions,
            commands::recent_events,
            commands::running_employees,
            commands::run_task,
            commands::respond_approval,
            commands::data_dir,
            commands::repo_status,
            commands::init_repo,
            commands::list_task_views,
            commands::task_diff,
            commands::merge_task,
            commands::abandon_task,
            commands::reveal_in_finder,
            commands::get_concurrency,
            commands::set_concurrency,
        ])
        .run(tauri::generate_context!())
        .expect("启动 Crewforge 失败");
}
