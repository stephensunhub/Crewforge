use crate::approval::UiApprover;
use crate::state::AppState;
use crewforge_core::db::{Employee, EventRow, Session, default_data_dir};
use crewforge_core::provider::claude::Decision;
use crewforge_core::runner::{NoopObserver, TurnConfig, run_turn};
use std::path::PathBuf;
use tauri::{AppHandle, Emitter, State};

type CmdResult<T> = Result<T, String>;
fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

#[tauri::command]
pub fn list_employees(state: State<'_, AppState>) -> CmdResult<Vec<Employee>> {
    state.db.list_employees().map_err(err)
}

#[tauri::command]
pub fn add_employee(state: State<'_, AppState>, name: String, role: String, persona: String) -> CmdResult<Employee> {
    let name = name.trim();
    if name.is_empty() || name.contains(char::is_whitespace) || name.contains('@') {
        return Err("员工名称不能为空，且不能包含空格或 @".into());
    }
    state.db.add_employee(name, role.trim(), persona.trim()).map_err(err)
}

#[tauri::command]
pub fn list_sessions(state: State<'_, AppState>) -> CmdResult<Vec<Session>> {
    state.db.list_sessions().map_err(err)
}

#[tauri::command]
pub fn recent_events(state: State<'_, AppState>, limit: usize) -> CmdResult<Vec<EventRow>> {
    state.db.recent_events(limit.min(2000)).map_err(err)
}

#[tauri::command]
pub fn running_employees(state: State<'_, AppState>) -> Vec<String> {
    state.running.lock().unwrap().iter().cloned().collect()
}

#[tauri::command]
pub fn get_concurrency(state: State<'_, AppState>) -> usize {
    state.concurrency()
}

#[tauri::command]
pub fn set_concurrency(state: State<'_, AppState>, n: usize) -> usize {
    state.set_concurrency(n)
}

#[tauri::command]
pub fn data_dir() -> String {
    default_data_dir().to_string_lossy().into_owned()
}

#[tauri::command]
pub fn respond_approval(state: State<'_, AppState>, id: String, allow: bool) -> bool {
    let decision = if allow { Decision::Allow } else { Decision::Deny("人类拒绝了该操作".into()) };
    state.pending.resolve(&id, decision)
}

/// 给员工分配一个任务。立即返回，进度通过事件推送。
#[tauri::command]
pub fn run_task(
    app: AppHandle,
    state: State<'_, AppState>,
    employee: String,
    prompt: String,
    project_path: String,
) -> CmdResult<()> {
    let path = PathBuf::from(&project_path);
    if !path.is_dir() {
        return Err(format!("项目目录不存在: {project_path}"));
    }
    let path = path.canonicalize().map_err(err)?;
    let emp = state
        .db
        .employee_by_name(&employee)
        .map_err(err)?
        .ok_or_else(|| format!("员工不存在: {employee}"))?;
    if !state.running.lock().unwrap().insert(emp.name.clone()) {
        return Err(format!("{} 正在执行任务，请等待完成", emp.name));
    }
    emit_running(&app, &state);

    let db = state.db.clone();
    let sem = state.sem.clone();
    let running = state.running.clone();
    let approver = UiApprover { app: app.clone(), pending: state.pending.clone() };
    tauri::async_runtime::spawn(async move {
        let result = async {
            // 并发已满时先记录排队，界面据此区分"排队中"与"工作中"
            if sem.available_permits() == 0 {
                let _ = db.append_event("task_queued", Some(&emp.id), None, &serde_json::json!({"prompt": prompt}));
            }
            let _permit = sem.acquire_owned().await?;
            let cfg = TurnConfig { project_path: path, program: "claude".into() };
            run_turn(&db, &emp, &cfg, &prompt, &approver, &NoopObserver).await
        }
        .await;
        if let Err(e) = result {
            // run_turn 已记录 task_failed；排队阶段等未进入 run_turn 的错误在这里补记
            let _ = db.append_event("task_error", Some(&emp.id), None, &serde_json::json!({"error": e.to_string()}));
        }
        running.lock().unwrap().remove(&emp.name);
        let names: Vec<String> = running.lock().unwrap().iter().cloned().collect();
        let _ = app.emit("crewforge://running", names);
    });
    Ok(())
}

fn emit_running(app: &AppHandle, state: &AppState) {
    let names: Vec<String> = state.running.lock().unwrap().iter().cloned().collect();
    let _ = app.emit("crewforge://running", names);
}
