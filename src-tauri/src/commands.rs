use crate::approval::UiApprover;
use crate::state::AppState;
use crewforge_core::db::{Employee, EventRow, Session, default_data_dir};
use crewforge_core::provider::claude::Decision;
use crewforge_core::orchestrator::TaskView;
use crewforge_core::runner::NoopObserver;
use crewforge_core::workspace::{MergeOutcome, RepoStatus};
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

#[tauri::command]
pub async fn repo_status(state: State<'_, AppState>, project_path: String) -> CmdResult<RepoStatus> {
    state.ws.repo_status(&PathBuf::from(project_path)).await.map_err(err)
}

#[tauri::command]
pub async fn init_repo(state: State<'_, AppState>, project_path: String) -> CmdResult<()> {
    state.ws.init_repo(&PathBuf::from(project_path)).await.map_err(err)
}

#[tauri::command]
pub async fn list_task_views(state: State<'_, AppState>, project_path: String) -> CmdResult<Vec<TaskView>> {
    state.orch.task_views(&PathBuf::from(project_path)).await.map_err(err)
}

#[tauri::command]
pub async fn task_diff(state: State<'_, AppState>, task_id: String) -> CmdResult<String> {
    state.orch.task_diff(&task_id).await.map_err(err)
}

/// 合并由人类在界面上点击确认后触发。
#[tauri::command]
pub async fn merge_task(state: State<'_, AppState>, task_id: String) -> CmdResult<MergeOutcome> {
    state.orch.merge_task(&task_id).await.map_err(err)
}

#[tauri::command]
pub async fn abandon_task(state: State<'_, AppState>, task_id: String) -> CmdResult<()> {
    state.orch.abandon_task(&task_id).await.map_err(err)
}

/// 在访达中显示某个任务的工作区目录。只允许打开已登记任务的 worktree。
#[tauri::command]
pub fn reveal_in_finder(state: State<'_, AppState>, task_id: String) -> CmdResult<()> {
    let task = state.db.get_task(&task_id).map_err(err)?.ok_or("任务不存在")?;
    let p = PathBuf::from(&task.worktree_path);
    if !p.is_dir() {
        return Err("工作区目录不存在".into());
    }
    std::process::Command::new("open").arg(&p).spawn().map_err(err)?;
    Ok(())
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
    if prompt.trim().is_empty() {
        return Err("任务内容不能为空".into());
    }
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
    let orch = state.orch.clone();
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
            orch.assign(&emp, &path, &prompt, &approver, &NoopObserver).await
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
