use crate::approval::UiApprover;
use crate::state::AppState;
use crewforge_core::board::{Actor, IssueView, Meeting, MeetingMessage, PendingDecision};
use crewforge_core::db::{Company, Employee, EmployeeCard, EventRow, Member, Project, ProjectSummary, Session, default_data_dir};
use crewforge_core::provider::claude::Decision;
use crewforge_core::orchestrator::TaskView;
use crewforge_core::runner::NoopObserver;
use crewforge_core::workspace::{MergeOutcome, RepoStatus};
use std::collections::HashMap;
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


// ================= 议题 =================
#[tauri::command]
pub fn list_issues(state: State<'_, AppState>, project_id: String) -> CmdResult<Vec<IssueView>> {
    state.db.issue_views(&project_id).map_err(err)
}

#[tauri::command]
pub fn pending_decisions(state: State<'_, AppState>) -> CmdResult<Vec<PendingDecision>> {
    state.db.pending_decisions().map_err(err)
}

#[tauri::command]
pub fn create_issue(
    state: State<'_, AppState>, project_id: String, title: String, summary: String, kind: String,
    high_risk: bool, owner_id: Option<String>, supersedes: Option<String>,
) -> CmdResult<()> {
    // 界面里发起的议题由人类提出
    state.db.create_issue(&project_id, &title, &summary, &kind, high_risk, None, owner_id.as_deref(), supersedes.as_deref())
        .map(|_| ()).map_err(err)
}

#[tauri::command]
pub fn update_issue(state: State<'_, AppState>, id: String, title: String, summary: String, owner_id: Option<String>) -> CmdResult<()> {
    state.db.update_issue(&id, &title, &summary, owner_id.as_deref()).map_err(err)
}

#[tauri::command]
pub fn add_issue_option(state: State<'_, AppState>, issue_id: String, title: String, description: String) -> CmdResult<()> {
    state.db.add_option(&issue_id, &title, &description, None).map(|_| ()).map_err(err)
}

#[tauri::command]
pub fn set_issue_status(state: State<'_, AppState>, id: String, status: String) -> CmdResult<()> {
    state.db.set_issue_status(&id, &status).map_err(err)
}

/// 拍板：目前只有人类能在界面上拍板；权限规则（AI 不能决定需求和高风险议题）在核心层强制。
#[tauri::command]
pub fn decide_issue(
    state: State<'_, AppState>, id: String, chosen_option: Option<String>, decision: String, reason: String,
    rejected_reasons: HashMap<String, String>,
) -> CmdResult<()> {
    state.db.decide_issue(&id, &Actor::Human, chosen_option.as_deref(), &decision, &reason, &rejected_reasons).map_err(err)
}

// ================= 会议 =================
#[derive(serde::Serialize)]
pub struct MeetingDetail {
    meeting: Meeting,
    messages: Vec<MeetingMessage>,
    issue_ids: Vec<String>,
    agent_replies: i64,
}

#[tauri::command]
pub fn list_meetings(state: State<'_, AppState>, project_id: String) -> CmdResult<Vec<Meeting>> {
    state.db.list_meetings(&project_id).map_err(err)
}

#[tauri::command]
pub fn meeting_detail(state: State<'_, AppState>, id: String) -> CmdResult<MeetingDetail> {
    let meeting = state.db.get_meeting(&id).map_err(err)?.ok_or("会议不存在")?;
    Ok(MeetingDetail {
        messages: state.db.meeting_messages(&id, 0).map_err(err)?,
        issue_ids: state.db.meeting_issue_ids(&id).map_err(err)?,
        agent_replies: state.db.agent_reply_count(&id).map_err(err)?,
        meeting,
    })
}

#[tauri::command]
pub fn create_meeting(
    state: State<'_, AppState>, project_id: String, kind: String, title: String, host_id: Option<String>,
    task_id: Option<String>, issue_ids: Vec<String>, max_agent_replies: i64,
) -> CmdResult<Meeting> {
    state.db.create_meeting(&project_id, &kind, &title, host_id.as_deref(), task_id.as_deref(), &issue_ids, max_agent_replies).map_err(err)
}

#[tauri::command]
pub fn link_meeting_issue(state: State<'_, AppState>, meeting_id: String, issue_id: String) -> CmdResult<()> {
    state.db.link_meeting_issue(&meeting_id, &issue_id).map_err(err)
}

#[tauri::command]
pub fn save_minutes(state: State<'_, AppState>, id: String, minutes: String) -> CmdResult<()> {
    state.db.set_minutes(&id, &minutes).map_err(err)
}

#[tauri::command]
pub fn close_meeting(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    state.db.close_meeting(&id).map_err(err)
}

/// 人类在会议里发言。发言本身只是记录；只有被 @ 的成员才会被唤醒（各自并行、受并发上限约束）。
#[tauri::command]
pub fn post_meeting_message(app: AppHandle, state: State<'_, AppState>, meeting_id: String, content: String) -> CmdResult<Vec<String>> {
    let meeting = state.db.get_meeting(&meeting_id).map_err(err)?.ok_or("会议不存在")?;
    state.db.post_message(&meeting_id, "human", None, &content).map_err(err)?;
    let members = state.db.list_members(&meeting.project_id).map_err(err)?;
    let mut woken = Vec::new();
    for cap in content.split('@').skip(1) {
        let name: String = cap.chars().take_while(|c| !c.is_whitespace() && *c != '@').collect();
        if let Some(m) = members.iter().find(|m| m.name == name) {
            if !woken.contains(&m.name) {
                woken.push(m.name.clone());
            }
        }
    }
    for name in &woken {
        spawn_meeting_turn(&app, &state, &meeting_id, name, false);
    }
    Ok(woken)
}

/// 让主持人起草纪要（一次调用）。
#[tauri::command]
pub fn draft_minutes(app: AppHandle, state: State<'_, AppState>, meeting_id: String) -> CmdResult<()> {
    let meeting = state.db.get_meeting(&meeting_id).map_err(err)?.ok_or("会议不存在")?;
    let members = state.db.list_members(&meeting.project_id).map_err(err)?;
    let scribe = meeting.host_id.as_ref()
        .and_then(|h| members.iter().find(|m| &m.employee_id == h))
        .or_else(|| members.iter().find(|m| m.role == "lead"))
        .or_else(|| members.first())
        .ok_or("项目里没有成员，无法起草纪要，请直接手写")?;
    spawn_meeting_turn(&app, &state, &meeting_id, &scribe.name.clone(), true);
    Ok(())
}

/// 在后台执行一次会议发言/纪要起草：占用并发许可，并在员工忙碌时提示。
fn spawn_meeting_turn(app: &AppHandle, state: &AppState, meeting_id: &str, employee: &str, minutes: bool) {
    let notify = |db: &crewforge_core::db::Db, mid: &str, text: String| {
        let _ = db.post_message(mid, "system", None, &text);
    };
    let Ok(Some(emp)) = state.db.employee_by_name(employee) else { return };
    if !state.running.lock().unwrap().insert(emp.name.clone()) {
        notify(&state.db, meeting_id, format!("{} 正在执行其他任务，这次没有被唤醒。请稍后再 @ TA", emp.name));
        return;
    }
    let _ = app.emit("crewforge://running", state.running.lock().unwrap().iter().cloned().collect::<Vec<_>>());
    let (db, orch, sem, running) = (state.db.clone(), state.orch.clone(), state.sem.clone(), state.running.clone());
    let approver = UiApprover { app: app.clone(), pending: state.pending.clone(), project_id: None };
    let (app, mid) = (app.clone(), meeting_id.to_string());
    tauri::async_runtime::spawn(async move {
        let res: anyhow::Result<()> = async {
            let _permit = sem.acquire_owned().await?;
            if minutes {
                orch.draft_minutes(&mid, &approver).await?;
            } else {
                orch.meeting_reply(&mid, &emp, &approver).await?;
            }
            Ok(())
        }
        .await;
        if let Err(e) = res {
            notify(&db, &mid, format!("{}：{e}", emp.name));
        }
        running.lock().unwrap().remove(&emp.name);
        let names: Vec<String> = running.lock().unwrap().iter().cloned().collect();
        let _ = app.emit("crewforge://running", names);
        if minutes {
            // 纪要写入会议本身，通知界面刷新
            let _ = db.append_event("meeting_minutes", Some(&emp.id), None, &serde_json::json!({"meeting_id": mid}));
        }
    });
}
