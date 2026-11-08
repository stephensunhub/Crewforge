use crate::approval::UiApprover;
use crate::state::AppState;
use crewforge_core::db::{Company, Employee, EmployeeCard, EventRow, Member, Project, ProjectSummary, Session, default_data_dir};
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

/// 某个项目的最近事件；不传项目则返回全部。
#[tauri::command]
pub fn recent_events(state: State<'_, AppState>, project_id: Option<String>, limit: usize) -> CmdResult<Vec<EventRow>> {
    let limit = limit.min(2000);
    match project_id {
        Some(id) => state.db.recent_events_for_project(&id, limit),
        None => state.db.recent_events(limit),
    }
    .map_err(err)
}

#[tauri::command]
pub fn get_company(state: State<'_, AppState>) -> CmdResult<Company> {
    state.db.company().map_err(err)
}

#[tauri::command]
pub fn update_company(state: State<'_, AppState>, name: String, description: String, conventions: String) -> CmdResult<()> {
    state.db.update_company(&name, &description, &conventions).map_err(err)
}

#[tauri::command]
pub fn employee_cards(state: State<'_, AppState>) -> CmdResult<Vec<EmployeeCard>> {
    state.db.employee_cards().map_err(err)
}

#[tauri::command]
pub fn update_employee(state: State<'_, AppState>, id: String, role: String, persona: String) -> CmdResult<()> {
    state.db.update_employee(&id, role.trim(), persona.trim()).map_err(err)
}

#[tauri::command]
pub fn archive_employee(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    let emp = state.db.employee_by_id(&id).map_err(err)?.ok_or("员工不存在")?;
    if state.running.lock().unwrap().contains(&emp.name) {
        return Err("该员工正在执行任务，请等待完成".into());
    }
    if !state.db.employee_projects(&id).map_err(err)?.is_empty() {
        return Err("该员工还在项目中，请先在各项目的成员页把 TA 移出".into());
    }
    state.db.set_employee_archived(&id, true).map_err(err)
}

#[tauri::command]
pub fn list_projects(state: State<'_, AppState>) -> CmdResult<Vec<ProjectSummary>> {
    state.db.project_summaries().map_err(err)
}

/// 新建或导入项目。`init_git` 为 true 时，目录不是仓库就初始化（员工的工作区需要 Git）。
#[tauri::command]
pub async fn create_project(
    state: State<'_, AppState>,
    name: String,
    description: String,
    path: String,
    init_git: bool,
) -> CmdResult<Project> {
    let p = PathBuf::from(&path);
    if !p.is_dir() {
        return Err(format!("目录不存在: {path}"));
    }
    let info = state.ws.ensure_ready(&p.canonicalize().map_err(err)?, init_git).await.map_err(err)?;
    state.db.create_project(&name, &description, &info.root.to_string_lossy()).map_err(err)
}

#[tauri::command]
pub fn update_project(state: State<'_, AppState>, id: String, name: String, description: String) -> CmdResult<()> {
    state.db.update_project(&id, &name, &description).map_err(err)
}

#[tauri::command]
pub fn archive_project(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    state.db.archive_project(&id).map_err(err)
}

#[tauri::command]
pub fn list_members(state: State<'_, AppState>, project_id: String) -> CmdResult<Vec<Member>> {
    state.db.list_members(&project_id).map_err(err)
}

#[tauri::command]
pub fn add_member(state: State<'_, AppState>, project_id: String, employee_id: String, role: String) -> CmdResult<()> {
    state.db.add_member(&project_id, &employee_id, &role).map_err(err)
}

#[tauri::command]
pub fn set_member_role(state: State<'_, AppState>, project_id: String, employee_id: String, role: String) -> CmdResult<()> {
    state.db.set_member_role(&project_id, &employee_id, &role).map_err(err)
}

#[tauri::command]
pub fn remove_member(state: State<'_, AppState>, project_id: String, employee_id: String) -> CmdResult<()> {
    state.db.remove_member(&project_id, &employee_id).map_err(err)
}

#[tauri::command]
pub async fn claude_version() -> CmdResult<String> {
    crewforge_core::provider::claude::detect("claude").await.map_err(err)
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
    project_id: String,
) -> CmdResult<()> {
    let project = state.db.get_project(&project_id).map_err(err)?.ok_or("项目不存在")?;
    let path = PathBuf::from(&project.path);
    if !path.is_dir() {
        return Err(format!("项目目录不存在: {}", project.path));
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
    if emp.archived {
        return Err(format!("{} 已归档，不能分配任务", emp.name));
    }
    if !state.db.is_member(&project.id, &emp.id).map_err(err)? {
        return Err(format!("{} 不是项目「{}」的成员，请先在成员页添加", emp.name, project.name));
    }
    if !state.running.lock().unwrap().insert(emp.name.clone()) {
        return Err(format!("{} 正在执行任务，请等待完成", emp.name));
    }
    emit_running(&app, &state);

    let db = state.db.clone();
    let orch = state.orch.clone();
    let sem = state.sem.clone();
    let running = state.running.clone();
    let approver = UiApprover { app: app.clone(), pending: state.pending.clone(), project_id: Some(project.id.clone()) };
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
