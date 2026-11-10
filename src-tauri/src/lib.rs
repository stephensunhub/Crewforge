//! Tauri 外壳：只做桥接。所有业务逻辑在 crewforge-core。

mod approval;
mod commands;
mod state;

use crewforge_core::db::{Db, default_data_dir};
use std::sync::Arc;
use tauri::{Emitter, Manager};

pub fn run() {
    // 从 Finder 启动时 PATH 极简，先还原成用户登录 shell 的 PATH，员工进程才找得到 claude/cargo/git/npm
    crewforge_core::shell_env::fix_path();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let data_dir = default_data_dir();
            let db = Arc::new(Db::open(&data_dir)?);
            // 把事件表里的新增事件推送给界面。不用进程内的钩子：MCP 服务器和命令行是别的进程，
            // 它们写入的事件只有通过读库才看得到。
            let _ = db.close_orphaned_autopilot_runs();
            let st = state::AppState::new(db.clone(), &data_dir);
            let sched = st.sched.clone();
            let handle = app.handle().clone();
            let poll_db = db.clone();
            std::thread::spawn(move || {
                let mut last = poll_db.max_event_id().unwrap_or(0);
                let mut tick = 0u32;
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    if let Ok(events) = poll_db.events_after(last, 500) {
                        for e in events {
                            last = e.id;
                            // 新的额度读数：让排队的任务立刻重新评估
                            if e.kind == "rate_limit" {
                                sched.nudge();
                            }
                            let _ = handle.emit("crewforge://event", &e);
                        }
                    }
                    // 调度状态变化（含额度窗口到期自动恢复）时发布事件；每 3 秒检查一次
                    tick += 1;
                    if tick % 10 == 0 {
                        sched.publish_if_changed();
                    }
                }
            });
            app.manage(st);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_action_items,
            commands::set_action_status,
            commands::action_counts,
            commands::dispatch_action_item,
            commands::plan_view,
            commands::create_plan_item,
            commands::update_plan_item,
            commands::move_plan_item,
            commands::set_plan_status,
            commands::approve_plan,
            commands::plan_proposed_counts,
            commands::set_project_goal,
            commands::dispatch_plan_item,
            commands::start_planning,
            commands::cost_report,
            commands::cost_calls,
            commands::my_duties,
            commands::list_docs,
            commands::read_doc,
            commands::lead_update_docs,
            commands::lead_curate_memory,
            commands::lead_handle_actions,
            commands::lead_triage_actions,
            commands::lead_draft_plan,
            commands::start_autopilot,
            commands::pause_autopilot,
            commands::stop_autopilot,
            commands::autopilot_status,
            commands::usage_overview,
            commands::usage_stats,
            commands::usage_calls,
            commands::usage_sessions,
            commands::models_overview,
            commands::set_provider_enabled,
            commands::add_model,
            commands::set_model_enabled,
            commands::remove_model,
            commands::set_purpose_model,
            commands::apply_recommended_models,
            commands::discover_copilot_models,
            commands::install_copilot,
            commands::set_copilot_program,
            commands::onboarding_state,
            commands::run_scan,
            commands::run_baseline,
            commands::evaluate_project,
            commands::confirm_onboarding,
            commands::check_task,
            commands::latest_task_check,
            commands::list_issues,
            commands::pending_decisions,
            commands::pending_proposals,
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
            commands::start_standup,
            commands::invite_guest,
            commands::end_guest,
            commands::post_meeting_message,
            commands::draft_minutes,
            commands::list_memories,
            commands::add_memory,
            commands::update_memory,
            commands::verify_memory,
            commands::expire_memory,
            commands::delete_memory,
            commands::promote_memory,
            commands::save_meeting_lessons,
            commands::unverified_counts,
            commands::preview_retrieval,
            commands::get_company,
            commands::update_company,
            commands::employee_cards,
            commands::update_employee,
            commands::archive_employee,
            commands::list_projects,
            commands::create_project,
            commands::update_project,
            commands::archive_project,
            commands::end_project,
            commands::discard_project,
            commands::list_ended_projects,
            commands::restore_project,
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
            commands::scheduler_status,
            commands::scheduler_set_config,
            commands::get_context_limit,
            commands::set_context_limit,
            commands::scheduler_pause,
            commands::scheduler_override,
            commands::get_concurrency,
            commands::set_concurrency,
        ])
        .run(tauri::generate_context!())
        .expect("启动 Crewforge 失败");
}
