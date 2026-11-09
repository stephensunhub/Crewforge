use crate::approval::UiApprover;
use crate::state::AppState;
use crewforge_core::onboarding::{OnboardingState, StoredCheck, DEFAULT_COMMAND_TIMEOUT, baseline::CheckReport, scan::{BaselineCommand, ScanReport}};
use crewforge_core::actions::ActionItem;
use crewforge_core::plan::{NewPlanItem, PlanView};
use crewforge_core::cost::{CallFilter, CallPage, CostReport};
use crewforge_core::scheduler::{SchedStatus, SchedulerConfig};
use crewforge_core::memory::{DEFAULT_MAX_CHARS, DEFAULT_MAX_ITEMS, Memory, MemoryFilter, NewMemory};
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
pub fn add_employee(state: State<'_, AppState>, name: String, role: String, persona: String, provider: Option<String>) -> CmdResult<Employee> {
    let name = name.trim();
    if name.is_empty() || name.contains(char::is_whitespace) || name.contains('@') {
        return Err("员工名称不能为空，且不能包含空格或 @".into());
    }
    state.db.add_employee_with(name, role.trim(), persona.trim(), provider.as_deref().unwrap_or("claude")).map_err(err)
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
    import_existing: bool,
) -> CmdResult<Project> {
    let p = PathBuf::from(&path);
    if !p.is_dir() {
        return Err(format!("目录不存在: {path}"));
    }
    let info = state.ws.ensure_ready(&p.canonicalize().map_err(err)?, init_git).await.map_err(err)?;
    let project = state.db.create_project(&name, &description, &info.root.to_string_lossy()).map_err(err)?;
    if import_existing {
        // 导入的既有项目先进入"导入中"：完成扫描、基线、评估并由人类确认后，才能派任务
        state.db.set_onboarding(&project.id, "in_progress").map_err(err)?;
        return state.db.get_project(&project.id).map_err(err)?.ok_or_else(|| "项目创建失败".to_string());
    }
    Ok(project)
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
    state.sched.status().user_limit
}

#[tauri::command]
pub fn set_concurrency(state: State<'_, AppState>, n: usize) -> CmdResult<usize> {
    state.sched.set_user_limit(n).map_err(err)
}

// ================= 额度调度 =================
#[tauri::command]
pub fn scheduler_status(state: State<'_, AppState>) -> SchedStatus {
    state.sched.status()
}

#[tauri::command]
pub fn scheduler_set_config(state: State<'_, AppState>, config: SchedulerConfig) -> CmdResult<()> {
    state.sched.set_config(config).map_err(err)?;
    state.sched.publish_if_changed();
    Ok(())
}

#[tauri::command]
pub fn scheduler_pause(state: State<'_, AppState>, paused: bool) {
    state.sched.set_manual_pause(paused);
    state.sched.publish_if_changed();
}

/// 临时忽略额度限制 `minutes` 分钟；0 表示取消。
#[tauri::command]
pub fn scheduler_override(state: State<'_, AppState>, minutes: u32) {
    state.sched.set_override(minutes.min(240));
    state.sched.publish_if_changed();
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
pub async fn merge_task(state: State<'_, AppState>, task_id: String, acknowledge_open_items: Option<bool>) -> CmdResult<MergeOutcome> {
    state.orch.merge_task_ack(&task_id, acknowledge_open_items.unwrap_or(false)).await.map_err(err)
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
    start_assignment(app, &state, emp, project.id, path, prompt, None)
}

/// 这次派活是为了处理哪个评审行动项、或哪个计划条目。
enum WorkLink {
    Action(String),
    Plan(String),
}

/// 把一次派活放进调度队列并在后台执行：占用员工、必要时排队并记录原因、结束后释放。
/// `action_item` 不为空时，这次派活是为了处理某条评审行动项。
fn start_assignment(
    app: AppHandle, state: &AppState, emp: Employee, project_id: String, path: PathBuf, prompt: String, link: Option<WorkLink>,
) -> CmdResult<()> {
    if !state.running.lock().unwrap().insert(emp.name.clone()) {
        return Err(format!("{} 正在执行任务，请等待完成", emp.name));
    }
    emit_running(&app, state);

    let db = state.db.clone();
    let orch = state.orch.clone();
    let sched = state.sched.clone();
    let running = state.running.clone();
    let approver = UiApprover { app: app.clone(), pending: state.pending.clone(), project_id: Some(project_id) };
    tauri::async_runtime::spawn(async move {
        let result = async {
            // 现在不能立刻开始（并发已满，或额度紧张/暂停）：先记录排队和原因，界面据此区分"排队中"与"工作中"
            if !sched.would_start_now() {
                let st = sched.status();
                let reason = if st.level == crewforge_core::scheduler::Level::Normal {
                    format!("已有 {} 个任务在运行（并发上限 {}）", st.running, st.effective_limit)
                } else {
                    st.reason.clone()
                };
                let _ = db.append_event("task_queued", Some(&emp.id), None, &serde_json::json!({"prompt": prompt, "reason": reason, "level": st.level}));
            }
            let _permit = sched.acquire().await;
            match &link {
                Some(WorkLink::Action(id)) => orch.assign_for_action(&emp, &path, &prompt, id, &approver, &NoopObserver).await,
                Some(WorkLink::Plan(id)) => orch.assign_for_plan(&emp, &path, &prompt, id, &approver, &NoopObserver).await,
                None => orch.assign(&emp, &path, &prompt, &approver, &NoopObserver).await,
            }
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
pub fn pending_proposals(state: State<'_, AppState>) -> CmdResult<Vec<PendingDecision>> {
    state.db.ai_proposals().map_err(err)
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
    guests: Vec<crewforge_core::guest::Guest>,
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
        guests: state.db.meeting_guests(&id).map_err(err)?,
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

/// 开站会：系统先汇总事实，在做事的成员依次各报告一次，再做对齐检查并起草纪要。立即返回会议，其余在后台进行。
#[tauri::command]
pub async fn start_standup(app: AppHandle, state: State<'_, AppState>, project_id: String) -> CmdResult<Meeting> {
    let (m, who) = state.orch.start_standup(&project_id).await.map_err(err)?;
    let (db, orch, sched, running) = (state.db.clone(), state.orch.clone(), state.sched.clone(), state.running.clone());
    let approver = UiApprover { app: app.clone(), pending: state.pending.clone(), project_id: None };
    let (app2, mid) = (app.clone(), m.id.clone());
    tauri::async_runtime::spawn(async move {
        let say = |t: String| { let _ = db.post_message(&mid, "system", None, &t); };
        for emp in who {
            // 正在执行任务的成员不打断：以系统记录的事实为准
            if !running.lock().unwrap().insert(emp.name.clone()) {
                say(format!("{} 正在执行任务，这次不打断，以上面系统记录的现状为准。", emp.name));
                continue;
            }
            let _ = app2.emit("crewforge://running", running.lock().unwrap().iter().cloned().collect::<Vec<_>>());
            let res: anyhow::Result<()> = async {
                let _permit = sched.acquire_interactive().await.map_err(|e| anyhow::anyhow!(e))?;
                orch.meeting_reply(&mid, &emp, &approver).await.map(|_| ())
            }.await;
            if let Err(e) = res { say(format!("{}：{e}", emp.name)); }
            running.lock().unwrap().remove(&emp.name);
            let _ = app2.emit("crewforge://running", running.lock().unwrap().iter().cloned().collect::<Vec<_>>());
        }
        if let Err(e) = orch.standup_alignment_check(&mid).await { say(format!("对齐检查失败：{e}")); }
        let res: anyhow::Result<()> = async {
            let _permit = sched.acquire_interactive().await.map_err(|e| anyhow::anyhow!(e))?;
            orch.draft_minutes(&mid, &approver).await.map(|_| ())
        }.await;
        if let Err(e) = res { say(format!("纪要没有起草：{e}。可以稍后手动让主持人起草。")); }
        let _ = db.append_event("meeting_minutes", None, None, &serde_json::json!({"meeting_id": mid}));
    });
    Ok(m)
}

/// 邀请外部临时 AI（顾问）加入会议。用户直接邀请视为已批准。
#[tauri::command]
pub fn invite_guest(state: State<'_, AppState>, meeting_id: String, name: String, brief: String, hours: Option<i64>) -> CmdResult<()> {
    state.orch.invite_guest(&meeting_id, &name, &brief, hours, "human").map(|_| ()).map_err(err)
}

/// 让顾问提前离场。
#[tauri::command]
pub fn end_guest(state: State<'_, AppState>, meeting_id: String, employee_id: String) -> CmdResult<()> {
    state.db.end_guest(&meeting_id, &employee_id).map_err(err)?;
    state.db.post_message(&meeting_id, "system", None, "顾问已离场。").map(|_| ()).map_err(err)
}

/// 人类在会议里发言。发言本身只是记录；只有被 @ 的成员才会被唤醒（各自并行、受并发上限约束）。
#[tauri::command]
pub fn post_meeting_message(app: AppHandle, state: State<'_, AppState>, meeting_id: String, content: String) -> CmdResult<Vec<String>> {
    post_and_wake(&app, &state, &meeting_id, &content)
}

fn post_and_wake(app: &AppHandle, state: &AppState, meeting_id: &str, content: &str) -> CmdResult<Vec<String>> {
    let meeting = state.db.get_meeting(meeting_id).map_err(err)?.ok_or("会议不存在")?;
    state.db.post_message(meeting_id, "human", None, content).map_err(err)?;
    let mut names: Vec<String> = state.db.list_members(&meeting.project_id).map_err(err)?.into_iter().map(|m| m.name).collect();
    names.extend(state.db.meeting_guests(meeting_id).map_err(err)?.into_iter().filter(|g| g.active).map(|g| g.name));
    let mut woken = Vec::new();
    for cap in content.split('@').skip(1) {
        let name: String = cap.chars().take_while(|c| !c.is_whitespace() && *c != '@').collect();
        if names.contains(&name) && !woken.contains(&name) {
            woken.push(name);
        }
    }
    for name in &woken {
        spawn_meeting_turn(app, state, meeting_id, name, false);
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
    let (db, orch, sched, running) = (state.db.clone(), state.orch.clone(), state.sched.clone(), state.running.clone());
    let approver = UiApprover { app: app.clone(), pending: state.pending.clone(), project_id: None };
    let (app, mid) = (app.clone(), meeting_id.to_string());
    tauri::async_runtime::spawn(async move {
        let res: anyhow::Result<()> = async {
            // 会议发言是交互式的：额度暂停时直接说明原因，不悄悄排队几个小时
            let _permit = sched.acquire_interactive().await.map_err(|e| anyhow::anyhow!(e))?;
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


// ================= 记忆 =================
#[tauri::command]
pub fn list_memories(
    state: State<'_, AppState>, layer: Option<String>, project_id: Option<String>, employee_id: Option<String>,
    status: Option<String>, query: Option<String>,
) -> CmdResult<Vec<Memory>> {
    state.db.list_memories(&MemoryFilter { layer, project_id, employee_id, status, query }).map_err(err)
}

/// 人类手写的记忆直接是已验证。
#[tauri::command]
pub fn add_memory(
    state: State<'_, AppState>, layer: String, project_id: Option<String>, employee_id: Option<String>,
    kind: String, title: String, content: String, scope: String,
) -> CmdResult<Memory> {
    state.db.add_memory(NewMemory {
        layer, project_id, employee_id, kind, title, content, scope,
        source_kind: "human".into(), created_by: "human".into(), ..Default::default()
    }).map_err(err)
}

#[tauri::command]
pub fn update_memory(state: State<'_, AppState>, id: String, title: String, content: String, scope: String) -> CmdResult<()> {
    state.db.update_memory(&id, &title, &content, &scope).map_err(err)
}

#[tauri::command]
pub fn verify_memory(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    state.db.verify_memory(&id, "human").map_err(err)
}

#[tauri::command]
pub fn expire_memory(state: State<'_, AppState>, id: String, reason: String) -> CmdResult<()> {
    state.db.expire_memory(&id, reason.trim()).map_err(err)
}

#[tauri::command]
pub fn delete_memory(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    state.db.delete_memory(&id).map_err(err)
}

#[tauri::command]
pub fn promote_memory(state: State<'_, AppState>, id: String, employee_id: String) -> CmdResult<()> {
    state.db.promote_memory(&id, &employee_id).map(|_| ()).map_err(err)
}

#[tauri::command]
pub fn save_meeting_lessons(state: State<'_, AppState>, meeting_id: String) -> CmdResult<usize> {
    state.db.save_meeting_lessons(&meeting_id).map_err(err)
}

#[tauri::command]
pub fn unverified_counts(state: State<'_, AppState>) -> CmdResult<Vec<(String, String, i64)>> {
    state.db.unverified_counts().map_err(err)
}

/// 预览：给定任务描述，这名员工在这个项目里开始任务时会被注入哪些记忆。不记录使用情况。
#[tauri::command]
pub fn preview_retrieval(state: State<'_, AppState>, project_id: Option<String>, employee_id: String, text: String) -> CmdResult<Vec<Memory>> {
    state.db.retrieve_memories(project_id.as_deref(), &employee_id, None, &text, DEFAULT_MAX_ITEMS, DEFAULT_MAX_CHARS).map_err(err)
}


// ================= 旧项目导入 =================
#[tauri::command]
pub fn onboarding_state(state: State<'_, AppState>, project_id: String) -> CmdResult<OnboardingState> {
    state.db.onboarding_state(&project_id).map_err(err)
}

#[tauri::command]
pub async fn run_scan(state: State<'_, AppState>, project_id: String) -> CmdResult<ScanReport> {
    state.orch.scan_project(&project_id).await.map_err(err)
}

/// 释放"忙碌"标记，即使命令中途出错或被取消。
struct BusyGuard { set: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>, key: String }
impl Drop for BusyGuard {
    fn drop(&mut self) { self.set.lock().unwrap().remove(&self.key); }
}
fn acquire(state: &AppState, key: String, busy_msg: &str) -> CmdResult<BusyGuard> {
    if !state.busy.lock().unwrap().insert(key.clone()) {
        return Err(busy_msg.to_string());
    }
    Ok(BusyGuard { set: state.busy.clone(), key })
}

/// 运行基线。耗时较长，进度通过事件流实时推送；命令由人类在界面上确认过。
#[tauri::command]
pub async fn run_baseline(state: State<'_, AppState>, project_id: String, commands: Vec<BaselineCommand>) -> CmdResult<()> {
    let _g = acquire(&state, format!("baseline:{project_id}"), "这个项目的基线正在运行")?;
    state.orch.run_baseline(&project_id, commands, DEFAULT_COMMAND_TIMEOUT).await.map(|_| ()).map_err(err)
}

/// 让员工做导入评估（architect = 全栈视角，qa = QA 视角）。只读，占用并发许可。
#[tauri::command]
pub async fn evaluate_project(app: AppHandle, state: State<'_, AppState>, project_id: String, kind: String, employee_id: String) -> CmdResult<()> {
    let emp = state.db.employee_by_id(&employee_id).map_err(err)?.ok_or("员工不存在")?;
    if !state.running.lock().unwrap().insert(emp.name.clone()) {
        return Err(format!("{} 正在执行其他任务，请稍后再试", emp.name));
    }
    let _ = app.emit("crewforge://running", state.running.lock().unwrap().iter().cloned().collect::<Vec<_>>());
    let result = async {
        let _permit = state.sched.acquire_interactive().await?;
        let approver = UiApprover { app: app.clone(), pending: state.pending.clone(), project_id: None };
        state.orch.evaluate_project(&project_id, &kind, &emp, &approver).await.map(|_| ()).map_err(err)
    }.await;
    state.running.lock().unwrap().remove(&emp.name);
    let _ = app.emit("crewforge://running", state.running.lock().unwrap().iter().cloned().collect::<Vec<_>>());
    result
}

#[tauri::command]
pub fn confirm_onboarding(state: State<'_, AppState>, project_id: String, skip_baseline: bool) -> CmdResult<()> {
    state.orch.confirm_onboarding(&project_id, skip_baseline).map_err(err)
}

/// 用基线的命令检查某个任务，逐项判定失败来源。
#[tauri::command]
pub async fn check_task(state: State<'_, AppState>, task_id: String) -> CmdResult<CheckReport> {
    let task = state.db.get_task(&task_id).map_err(err)?.ok_or("任务不存在")?;
    let emp = state.db.employee_by_id(&task.employee_id).map_err(err)?.ok_or("员工不存在")?;
    if state.running.lock().unwrap().contains(&emp.name) {
        return Err(format!("{} 正在执行任务，请等 TA 完成后再检查", emp.name));
    }
    let _g = acquire(&state, format!("check:{task_id}"), "这个任务正在检查")?;
    state.orch.check_task(&task_id, DEFAULT_COMMAND_TIMEOUT).await.map_err(err)
}

#[tauri::command]
pub fn latest_task_check(state: State<'_, AppState>, task_id: String) -> CmdResult<Option<StoredCheck>> {
    state.db.latest_task_check(&task_id).map_err(err)
}


// ================= 成本与额度 =================
/// 调用明细：每次模型调用一行，最新的在前，可过滤与分页。
#[tauri::command]
pub fn cost_calls(
    state: State<'_, AppState>, days: Option<i64>, employee_id: Option<String>, project_id: Option<String>,
    purpose: Option<String>, only_errors: bool, limit: usize, offset: usize,
) -> CmdResult<CallPage> {
    let since = days.filter(|d| *d > 0).map(|d| crewforge_core::db::now_ms() - d * 86_400_000);
    let f = CallFilter { employee_id, project_id, purpose, only_errors };
    state.db.cost_calls(since, &f, limit.min(200), offset).map_err(err)
}

/// `days`：只统计最近多少天；不传表示全部。`tz_offset_minutes`：本地时区相对 UTC 的分钟数，用于按天归并。
#[tauri::command]
pub fn cost_report(state: State<'_, AppState>, days: Option<i64>, tz_offset_minutes: i64) -> CmdResult<CostReport> {
    let since = days.filter(|d| *d > 0).map(|d| crewforge_core::db::now_ms() - d * 86_400_000);
    state.db.cost_report(since, tz_offset_minutes).map_err(err)
}


// ================= 评审行动项 =================
#[tauri::command]
pub fn list_action_items(state: State<'_, AppState>, meeting_id: String) -> CmdResult<Vec<ActionItem>> {
    state.db.actions_for_meeting(&meeting_id).map_err(err)
}

/// 只有人类能处理行动项：done（已处理）、dismissed（驳回，必须写原因）、open（重新打开）。
#[tauri::command]
pub fn set_action_status(state: State<'_, AppState>, id: String, status: String, note: String) -> CmdResult<()> {
    state.db.set_action_status(&id, &status, &note).map_err(err)
}

#[tauri::command]
pub fn action_counts(state: State<'_, AppState>) -> CmdResult<Vec<(String, String, i64)>> {
    state.db.open_action_counts().map_err(err)
}

/// 把行动项派给员工：对进行中任务的评审意见只能交给该任务的员工，在同一个任务里继续修。
#[tauri::command]
pub fn dispatch_action_item(app: AppHandle, state: State<'_, AppState>, id: String, employee_id: Option<String>) -> CmdResult<()> {
    let (emp, path, prompt) = state.orch.prepare_action_dispatch(&id, employee_id.as_deref()).map_err(err)?;
    let item = state.db.get_action(&id).map_err(err)?.ok_or("行动项不存在")?;
    let path = path.canonicalize().map_err(|_| format!("项目目录不存在: {}", path.display()))?;
    start_assignment(app, &state, emp, item.project_id, path, prompt, Some(WorkLink::Action(id)))
}


// ================= 计划 =================
#[derive(serde::Deserialize)]
pub struct PlanItemInput {
    title: String,
    description: String,
    acceptance: String,
    kind: String,
    module: String,
    assignee_id: Option<String>,
    deps: Vec<String>,
}
impl From<PlanItemInput> for NewPlanItem {
    fn from(i: PlanItemInput) -> Self {
        NewPlanItem { title: i.title, description: i.description, acceptance: i.acceptance, kind: i.kind, module: i.module, assignee_id: i.assignee_id, deps: i.deps }
    }
}

#[tauri::command]
pub fn plan_view(state: State<'_, AppState>, project_id: String) -> CmdResult<PlanView> {
    state.db.plan_view(&project_id).map_err(err)
}

#[tauri::command]
pub fn create_plan_item(state: State<'_, AppState>, project_id: String, item: PlanItemInput) -> CmdResult<String> {
    state.db.create_plan_item(&project_id, &item.into(), None).map_err(err)
}

#[tauri::command]
pub fn update_plan_item(state: State<'_, AppState>, id: String, item: PlanItemInput) -> CmdResult<()> {
    let deps = item.deps.clone();
    state.db.update_plan_item(&id, &item.into()).map_err(err)?;
    state.db.set_plan_deps(&id, &deps).map_err(err)
}

#[tauri::command]
pub fn move_plan_item(state: State<'_, AppState>, id: String, delta: i64) -> CmdResult<()> {
    state.db.move_plan_item(&id, delta).map_err(err)
}

#[tauri::command]
pub fn set_plan_status(state: State<'_, AppState>, id: String, status: String, note: String) -> CmdResult<()> {
    state.db.set_plan_status(&id, &status, &note).map_err(err)
}

/// 批准 AI 起草的计划：该项目所有“待确认”的条目进入“待开始”。
#[tauri::command]
pub fn approve_plan(state: State<'_, AppState>, project_id: String) -> CmdResult<usize> {
    state.db.approve_proposed_plan(&project_id).map_err(err)
}

#[tauri::command]
pub fn plan_proposed_counts(state: State<'_, AppState>) -> CmdResult<Vec<(String, String, i64)>> {
    state.db.proposed_plan_counts().map_err(err)
}

#[tauri::command]
pub fn set_project_goal(state: State<'_, AppState>, id: String, goal: String) -> CmdResult<()> {
    state.db.set_project_goal(&id, &goal).map_err(err)
}

/// 把计划条目派给员工：为它新开一个任务（带项目目标、说明、验收标准）。
#[tauri::command]
pub fn dispatch_plan_item(app: AppHandle, state: State<'_, AppState>, id: String, employee_id: Option<String>) -> CmdResult<()> {
    let (emp, path, prompt) = state.orch.prepare_plan_dispatch(&id, employee_id.as_deref()).map_err(err)?;
    let item = state.db.get_plan_item(&id).map_err(err)?.ok_or("条目不存在")?;
    let path = path.canonicalize().map_err(|_| format!("项目目录不存在: {}", path.display()))?;
    start_assignment(app, &state, emp, item.project_id, path, prompt, Some(WorkLink::Plan(id)))
}

/// 让某位员工制定计划：开一场只读的“迁移规划”会议，把事实交给 TA，TA 用 propose_plan 提交，条目先是“待确认”。
#[tauri::command]
pub fn start_planning(app: AppHandle, state: State<'_, AppState>, project_id: String, employee_id: String, brief: String) -> CmdResult<String> {
    let emp = state.db.employee_by_id(&employee_id).map_err(err)?.ok_or("员工不存在")?;
    let m = state.db.create_meeting(&project_id, "planning", "制定计划", None, None, &[], 12).map_err(err)?;
    let ask = format!(
        "@{} 请为这个项目制定一份计划，并用 propose_plan 一次性提交（先用 list_plan 看看已有的）。\n{}\n要求：每个条目一名员工一次任务能做完，写清楚说明和验收标准（最好能用命令验证），用 depends_on 标明先后顺序，用 assignee 建议谁来做。",
        emp.name, brief.trim()
    );
    post_and_wake(&app, &state, &m.id, &ask)?;
    Ok(m.id)
}
