use std::sync::Arc;
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
use tauri::{AppHandle, Emitter, Manager, State};

type CmdResult<T> = Result<T, String>;
fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

#[tauri::command]
pub fn list_employees(state: State<'_, AppState>) -> CmdResult<Vec<Employee>> {
    state.db.list_employees().map_err(err)
}

#[tauri::command]
pub fn add_employee(state: State<'_, AppState>, name: String, role: String, persona: String, provider: Option<String>, model: Option<String>) -> CmdResult<Employee> {
    let name = name.trim();
    if name.is_empty() || name.contains(char::is_whitespace) || name.contains('@') {
        return Err("员工名称不能为空，且不能包含空格或 @".into());
    }
    let e = state.db.add_employee_with(name, role.trim(), persona.trim(), provider.as_deref().unwrap_or("claude")).map_err(err)?;
    match model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => { state.db.update_employee_full(&e.id, &e.role, &e.persona, &e.provider, m).map_err(err)?; Ok(state.db.employee_by_id(&e.id).map_err(err)?.unwrap_or(e)) }
        None => Ok(e),
    }
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
pub fn update_employee(state: State<'_, AppState>, id: String, role: String, persona: String, provider: String, model: String) -> CmdResult<()> {
    let emp = state.db.employee_by_id(&id).map_err(err)?.ok_or("员工不存在")?;
    if emp.provider != provider && state.running.lock().unwrap().contains(&emp.name) {
        return Err("该员工正在执行任务，等 TA 完成后再切换执行引擎".into());
    }
    state.db.update_employee_full(&id, role.trim(), persona.trim(), &provider, model.trim()).map_err(err)
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

/// 目录里有没有既有内容（忽略隐藏文件和 .git）：用来判断是“新建”还是“导入已有项目”。
#[tauri::command]
pub fn dir_has_content(path: String) -> bool {
    std::fs::read_dir(&path).map(|rd| rd.flatten().any(|e| !e.file_name().to_string_lossy().starts_with('.'))).unwrap_or(false)
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

/// 结束项目前：通知自主推进停下，并确认项目成员现在没有人在干活（否则会和正在运行的 CLI 抢目录）。
fn quiesce_project(state: &AppState, project_id: &str) -> CmdResult<()> {
    if let Some(ctl) = state.autopilots.lock().unwrap().get(project_id) {
        ctl.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    let members = state.db.list_members(project_id).map_err(err)?;
    let running = state.running.lock().unwrap().clone();
    let busy: Vec<String> = members.iter().filter(|m| running.contains(&m.name)).map(|m| m.name.clone()).collect();
    if !busy.is_empty() {
        return Err(format!("{} 正在执行任务。已通知自主推进停下；等他们做完当前这一步后再试", busy.join("、")));
    }
    Ok(())
}

/// 已完成 / 归档（kind = completed | archived）。
#[tauri::command]
pub async fn end_project(state: State<'_, AppState>, id: String, kind: String, reason: String) -> CmdResult<()> {
    quiesce_project(&state, &id)?;
    state.orch.end_project(&id, &kind, &reason).await.map_err(err)
}

/// 不做了：彻底删除；`delete_folder` 时把项目文件夹移到废纸篓。需要输入项目名确认。
#[tauri::command]
pub async fn discard_project(state: State<'_, AppState>, id: String, confirm_name: String, delete_folder: bool, confirm_dir: Option<String>) -> CmdResult<crewforge_core::closeout::DiscardReport> {
    quiesce_project(&state, &id)?;
    state.orch.discard_project(&id, &confirm_name, delete_folder, confirm_dir.as_deref().unwrap_or("")).await.map_err(err)
}

#[tauri::command]
pub fn list_ended_projects(state: State<'_, AppState>) -> CmdResult<Vec<crewforge_core::closeout::EndedProject>> {
    state.db.list_ended_projects().map_err(err)
}

#[tauri::command]
pub fn restore_project(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    state.db.restore_project(&id).map_err(err)
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

/// 正在工作的员工此刻各自在做什么。
#[tauri::command]
pub fn employee_activities(state: State<'_, AppState>) -> CmdResult<Vec<crewforge_core::usage::Activity>> {
    let names: Vec<String> = state.running.lock().unwrap().iter().cloned().collect();
    state.db.activities(&names).map_err(err)
}

/// 员工的工作日志：最近若干次执行，每次做了什么、进度、耗时。
#[tauri::command]
pub fn employee_log(state: State<'_, AppState>, id: String, limit: usize) -> CmdResult<Vec<crewforge_core::usage::LogRun>> {
    let emp = state.db.employee_by_id(&id).map_err(err)?.ok_or("员工不存在")?;
    let busy = state.running.lock().unwrap().contains(&emp.name);
    state.db.employee_log(&id, limit.clamp(1, 200), busy).map_err(err)
}

/// 计划里每个条目此刻在等什么（条目 id → 原因）。
#[tauri::command]
pub fn plan_waiting(state: State<'_, AppState>, project_id: String) -> CmdResult<std::collections::HashMap<String, Vec<String>>> {
    let busy: std::collections::HashSet<String> = state.running.lock().unwrap().iter().cloned().collect();
    let live = state.autopilots.lock().unwrap().contains_key(&project_id);
    state.db.plan_waiting(&project_id, &busy, live).map_err(err)
}

#[tauri::command]
pub fn get_concurrency(state: State<'_, AppState>) -> usize {
    state.sched.status().user_limit
}

#[tauri::command]
pub fn set_concurrency(state: State<'_, AppState>, n: usize) -> CmdResult<usize> {
    state.sched.set_user_limit(n).map_err(err)
}

// ================= 会话上下文上限 =================
#[tauri::command]
pub fn get_context_limit(state: State<'_, AppState>) -> u64 {
    crewforge_core::runner::context_limit(&state.db)
}

#[tauri::command]
pub fn set_context_limit(state: State<'_, AppState>, tokens: u64) -> CmdResult<()> {
    if tokens != 0 && tokens < 20_000 {
        return Err("上限太小，员工的会话会频繁换新；至少 20K，或填 0 表示不限".into());
    }
    state.db.set_setting("context_limit", &tokens.to_string()).map_err(err)
}

// ================= 批次运行期间防止 Mac 睡眠 =================
#[tauri::command]
pub fn get_keep_awake(state: State<'_, AppState>) -> bool {
    state.db.keep_awake_setting()
}

#[tauri::command]
pub fn set_keep_awake(state: State<'_, AppState>, on: bool) -> CmdResult<()> {
    state.db.set_keep_awake_setting(on).map_err(err)
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

/// 开对齐会（原站会）：系统先汇总事实，在做事的成员依次各报告一次，再做对齐检查；主持人对照目标检查、起草纪要，
/// 主持人是 AI 负责人时再处理待决定的行动项、整理“之后再说”。立即返回会议，其余在后台进行。
#[tauri::command]
pub async fn start_standup(app: AppHandle, state: State<'_, AppState>, project_id: String) -> CmdResult<Meeting> {
    run_alignment(app, &state, &project_id).await
}

pub async fn run_alignment(app: AppHandle, st: &AppState, project_id: &str) -> CmdResult<Meeting> {
    let key = format!("align:{project_id}");
    if !st.busy.lock().unwrap().insert(key.clone()) { return Err("这个项目的对齐会正在进行".into()); }
    let host_id = st.db.align_host(project_id).map_err(err)?;
    let (m, who) = match st.orch.start_standup_hosted(project_id, host_id.as_deref()).await {
        Ok(x) => x,
        Err(e) => { st.busy.lock().unwrap().remove(&key); return Err(err(e)); }
    };
    let (db, orch, sched, running, busy) = (st.db.clone(), st.orch.clone(), st.sched.clone(), st.running.clone(), st.busy.clone());
    let approver = UiApprover { app: app.clone(), pending: st.pending.clone(), project_id: None };
    let (app2, mid, pid) = (app.clone(), m.id.clone(), project_id.to_string());
    tauri::async_runtime::spawn(async move {
        let say = |t: String| { let _ = db.post_message(&mid, "system", None, &t); };
        let emit_running = || { let _ = app2.emit("crewforge://running", running.lock().unwrap().iter().cloned().collect::<Vec<_>>()); };
        for emp in who {
            // 正在执行任务的成员不打断：以系统记录的事实为准
            if !running.lock().unwrap().insert(emp.name.clone()) {
                say(format!("{} 正在执行任务，这次不打断，以上面系统记录的现状为准。", emp.name));
                continue;
            }
            emit_running();
            let res: anyhow::Result<()> = async {
                let _permit = sched.acquire_interactive().await.map_err(|e| anyhow::anyhow!(e))?;
                orch.meeting_reply(&mid, &emp, &approver).await.map(|_| ())
            }.await;
            if let Err(e) = res { say(format!("{}：{e}", emp.name)); }
            running.lock().unwrap().remove(&emp.name);
            emit_running();
        }
        let check = match orch.standup_alignment_check(&mid).await { Ok(t) => t, Err(e) => { say(format!("对齐检查失败：{e}")); String::new() } };
        // 主持人对照目标检查（只有项目负责人能提议改目标，其余人只在纪要里指出）
        let host = host_id.as_deref().and_then(|h| db.employee_by_id(h).ok().flatten());
        let host_is_lead = host.as_ref().map(|h| db.list_members(&pid).map(|ms| ms.iter().any(|x| x.employee_id == h.id && x.role == "lead")).unwrap_or(false)).unwrap_or(false);
        let mut goal_note = String::new();
        if let Some(h) = &host {
            if running.lock().unwrap().insert(h.name.clone()) {
                emit_running();
                let res: anyhow::Result<String> = async {
                    let _permit = sched.acquire_interactive().await.map_err(|e| anyhow::anyhow!(e))?;
                    orch.align_goal_check(&mid, h, &approver).await
                }.await;
                match res { Ok(l) => goal_note = l, Err(e) => say(format!("目标检查没有完成：{e}")) }
                running.lock().unwrap().remove(&h.name);
                emit_running();
            } else {
                say(format!("{} 正在执行任务，这次跳过目标检查。", h.name));
            }
        }
        let res: anyhow::Result<()> = async {
            let _permit = sched.acquire_interactive().await.map_err(|e| anyhow::anyhow!(e))?;
            orch.draft_minutes(&mid, &approver).await.map(|_| ())
        }.await;
        if let Err(e) = res { say(format!("纪要没有起草：{e}。可以稍后手动让主持人起草。")); }
        let _ = db.append_event("meeting_minutes", None, None, &serde_json::json!({"meeting_id": mid}));
        let _ = db.close_meeting(&mid);
        // AI 负责人带头：纪要里待拍板的事由它定，再整理“之后再说”
        if let (Some(lead), true) = (&host, host_is_lead) {
            if running.lock().unwrap().insert(lead.name.clone()) {
                emit_running();
                use crewforge_core::lead_actions::ActionOutcome as O;
                for id in orch.human_actions_for_lead(&pid).unwrap_or_default().into_iter().take(5) {
                    let Ok(_permit) = sched.acquire_interactive().await else { break };
                    match orch.lead_handle_action(&id, lead, &approver).await {
                        Ok(Some(O::Decided { decision, .. })) => say(format!("{} 决定：{decision}", lead.name)),
                        Ok(Some(O::Dismissed { reason })) => say(format!("{} 驳回（不必做）：{reason}", lead.name)),
                        Ok(Some(O::Escalated { reason })) => say(format!("{} 留给你：{reason}", lead.name)),
                        Ok(None) => {}
                        Err(e) => say(format!("处理待决定事项没有完成：{e}")),
                    }
                }
                if let Ok(_permit) = sched.acquire_interactive().await {
                    if let Err(e) = orch.lead_triage_later(&pid, lead, &approver).await { say(format!("整理行动项没有完成：{e}")); }
                }
                running.lock().unwrap().remove(&lead.name);
                emit_running();
            }
        }
        let _ = db.append_event_for(Some(&pid), "alignment_done", None, None, &serde_json::json!({"meeting_id": mid, "goal_note": goal_note, "check": check.chars().take(300).collect::<String>()}));
        busy.lock().unwrap().remove(&format!("align:{pid}"));
    });
    Ok(m)
}

/// 后台定时：到点（超过间隔且有变化）的项目自动开对齐会。正在自主推进的项目由推进流程自己对齐，这里跳过。
pub async fn align_tick(app: &AppHandle) {
    use tauri::Manager;
    let st = app.state::<AppState>();
    let projects = st.db.list_projects().unwrap_or_default();
    for p in projects.into_iter().filter(|p| p.status == "active") {
        if st.autopilots.lock().unwrap().contains_key(&p.id) { continue; }
        if matches!(st.db.align_due(&p.id, crewforge_core::db::now_ms()), Ok(Some(_))) {
            let _ = run_alignment(app.clone(), &st, &p.id).await;
        }
    }
}

#[tauri::command]
pub fn get_align_config(state: State<'_, AppState>, project_id: String) -> CmdResult<crewforge_core::align::AlignConfig> {
    state.db.align_config(&project_id).map_err(err)
}

#[tauri::command]
pub fn set_align_config(state: State<'_, AppState>, project_id: String, enabled: bool, interval_hours: i64, host_id: Option<String>) -> CmdResult<()> {
    state.db.set_align_config(&project_id, enabled, interval_hours, host_id.as_deref()).map_err(err)
}

/// 公司页：各项目的对齐会情况，人一屏看完，不用进每个项目。
#[derive(serde::Serialize)]
pub struct AlignOverviewRow { pub project_id: String, pub name: String, pub config: crewforge_core::align::AlignConfig, pub open_points: usize }

#[tauri::command]
pub fn align_overview(state: State<'_, AppState>) -> CmdResult<Vec<AlignOverviewRow>> {
    let mut out = Vec::new();
    for p in state.db.list_projects().map_err(err)?.into_iter().filter(|p| p.status == "active") {
        let config = state.db.align_config(&p.id).map_err(err)?;
        let open_points = state.db.open_actions_for_project(&p.id).map(|a| a.len()).unwrap_or(0);
        out.push(AlignOverviewRow { project_id: p.id, name: p.name, config, open_points });
    }
    Ok(out)
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


// ================= 负责人管理文档和记忆 =================
fn lead_of(state: &AppState, project_id: &str) -> Result<Employee, String> {
    let id = state.db.list_members(project_id).map_err(err)?.into_iter().find(|m| m.role == "lead").map(|m| m.employee_id).ok_or("项目还没有负责人，请先在“成员”页指定一位 AI 员工为负责人")?;
    state.db.employee_by_id(&id).map_err(err)?.ok_or_else(|| "负责人不存在".to_string())
}

#[tauri::command]
pub async fn list_docs(state: State<'_, AppState>, project_id: String) -> CmdResult<Vec<crewforge_core::docs::DocFile>> {
    state.orch.list_docs(&project_id).await.map_err(err)
}

#[tauri::command]
pub async fn read_doc(state: State<'_, AppState>, project_id: String, path: String) -> CmdResult<String> {
    state.orch.read_doc(&project_id, &path).await.map_err(err)
}

/// 让负责人更新项目文档（docs/crewforge/）：决策记录、已知问题、计划进度由数据生成，概览由负责人读代码撰写（一次调用）。
#[tauri::command]
pub async fn lead_update_docs(app: AppHandle, state: State<'_, AppState>, project_id: String) -> CmdResult<crewforge_core::docs::DocsPublish> {
    let lead = lead_of(&state, &project_id)?;
    if !state.busy.lock().unwrap().insert(format!("docs:{project_id}")) { return Err("正在更新文档，请稍等".into()); }
    let approver = UiApprover { app, pending: state.pending.clone(), project_id: Some(project_id.clone()) };
    let res = async {
        let _permit = state.sched.acquire_interactive().await.map_err(|e| anyhow::anyhow!(e))?;
        state.orch.update_project_docs(&project_id, &lead, true, &approver).await
    }.await;
    state.busy.lock().unwrap().remove(&format!("docs:{project_id}"));
    res.map_err(err)
}

/// 让负责人整理项目记忆：能核实的验证，过时的让它过期，重复的合并；人写的和议题决议沉淀的不碰。
#[tauri::command]
pub async fn lead_curate_memory(app: AppHandle, state: State<'_, AppState>, project_id: String) -> CmdResult<crewforge_core::curate::Curated> {
    let lead = lead_of(&state, &project_id)?;
    if !state.busy.lock().unwrap().insert(format!("curate:{project_id}")) { return Err("负责人正在整理记忆，请稍等".into()); }
    let approver = UiApprover { app, pending: state.pending.clone(), project_id: Some(project_id.clone()) };
    let res = async {
        let _permit = state.sched.acquire_interactive().await.map_err(|e| anyhow::anyhow!(e))?;
        state.orch.lead_curate_memory(&project_id, &lead, &approver).await
    }.await;
    state.busy.lock().unwrap().remove(&format!("curate:{project_id}"));
    res.map_err(err)
}

// ================= 负责人处理标给人的行动项 =================
/// 让负责人处理这个项目里所有标给人、还没处理的行动项：能定的定，不必做的驳回，必须是人的才留下并写明原因。返回每条的结果说明。
#[tauri::command]
pub async fn lead_handle_actions(app: AppHandle, state: State<'_, AppState>, project_id: String) -> CmdResult<Vec<String>> {
    let lead = lead_of(&state, &project_id)?;
    if !state.busy.lock().unwrap().insert(format!("actions:{project_id}")) { return Err("负责人正在处理，请稍等".into()); }
    let approver = UiApprover { app, pending: state.pending.clone(), project_id: Some(project_id.clone()) };
    let res = async {
        let mut out = Vec::new();
        for id in state.orch.human_actions_for_lead(&project_id)?.into_iter().take(5) {
            let _permit = state.sched.acquire_interactive().await.map_err(|e| anyhow::anyhow!(e))?;
            use crewforge_core::lead_actions::ActionOutcome as O;
            match state.orch.lead_handle_action(&id, &lead, &approver).await? {
                Some(O::Decided { decision, follow_ups }) => out.push(format!("决定：{decision}{}", if follow_ups.is_empty() { String::new() } else { format!("（追加了计划条目：{}）", follow_ups.join("、")) })),
                Some(O::Dismissed { reason }) => out.push(format!("驳回（不必做）：{reason}")),
                Some(O::Escalated { reason }) => out.push(format!("留给你：{reason}")),
                None => {}
            }
        }
        anyhow::Ok(out)
    }.await;
    state.busy.lock().unwrap().remove(&format!("actions:{project_id}"));
    res.map_err(err)
}

/// 让负责人整理“之后再说”的行动项（不够 5 条时不调用模型，返回 None）。
#[tauri::command]
pub async fn lead_triage_actions(app: AppHandle, state: State<'_, AppState>, project_id: String) -> CmdResult<Option<String>> {
    let lead = lead_of(&state, &project_id)?;
    if !state.busy.lock().unwrap().insert(format!("triage:{project_id}")) { return Err("负责人正在整理，请稍等".into()); }
    let approver = UiApprover { app, pending: state.pending.clone(), project_id: Some(project_id.clone()) };
    let res = async {
        let _permit = state.sched.acquire_interactive().await.map_err(|e| anyhow::anyhow!(e))?;
        state.orch.lead_triage_later(&project_id, &lead, &approver).await
    }.await;
    state.busy.lock().unwrap().remove(&format!("triage:{project_id}"));
    Ok(res.map_err(err)?.map(|t| format!("驳回 {} 条，转为计划条目 {} 个{}{}", t.dismissed, t.planned.len(),
        if t.planned.is_empty() { String::new() } else { format!("（{}）", t.planned.join("、")) },
        if t.rejected.is_empty() { String::new() } else { format!("；被拒绝的指令：{}", t.rejected.join("；")) })))
}

// ================= 负责人起草计划 =================
/// 让项目的 AI 负责人根据项目目标、扫描、基线、评估和现有计划起草一份计划；条目进入“提议中”，等你确认。
#[tauri::command]
pub async fn lead_draft_plan(app: AppHandle, state: State<'_, AppState>, project_id: String) -> CmdResult<usize> {
    let lead_id = state.db.list_members(&project_id).map_err(err)?.into_iter().find(|m| m.role == "lead").map(|m| m.employee_id).ok_or("项目还没有负责人，请先在“成员”页指定一位 AI 员工为负责人")?;
    let lead = state.db.employee_by_id(&lead_id).map_err(err)?.ok_or("负责人不存在")?;
    if !state.busy.lock().unwrap().insert(format!("draft:{project_id}")) {
        return Err("负责人正在起草计划，请稍等".into());
    }
    let approver = UiApprover { app, pending: state.pending.clone(), project_id: Some(project_id.clone()) };
    let res = async {
        let _permit = state.sched.acquire_interactive().await.map_err(|e| anyhow::anyhow!(e))?;
        state.orch.lead_draft_plan(&project_id, &lead, &approver).await
    }.await;
    state.busy.lock().unwrap().remove(&format!("draft:{project_id}"));
    res.map_err(err)
}

// ================= 自主推进 =================
/// 开始一个自主推进批次：负责人按依赖派活、验证、评审、合并；预算用完、需要你拍板或反复失败时自动停下。
/// 点“开始”就是你对计划的最后确认——提议中的计划条目会一并批准。
#[tauri::command]
pub fn start_autopilot(app: AppHandle, state: State<'_, AppState>, project_id: String, max_calls: i64, max_minutes: i64) -> CmdResult<crewforge_core::autopilot::AutopilotRun> {
    let run = crewforge_core::autopilot::Autopilot::<UiApprover>::prepare(&state.orch, &project_id, max_calls, max_minutes).map_err(err)?;
    launch_autopilot(&app, &state, &run);
    Ok(run)
}

/// 把已创建好的批次跑起来（后台任务）。
fn launch_autopilot(app: &AppHandle, state: &AppState, run: &crewforge_core::autopilot::AutopilotRun) {
    let project_id = run.project_id.clone();
    let ctl = Arc::new(crewforge_core::autopilot::AutopilotCtl::default());
    state.autopilots.lock().unwrap().insert(project_id.clone(), ctl.clone());
    let approver = Arc::new(UiApprover { app: app.clone(), pending: state.pending.clone(), project_id: Some(project_id.clone()) });
    let (running, autopilots, orch, sched) = (state.running.clone(), state.autopilots.clone(), state.orch.clone(), state.sched.clone());
    let emit_app = app.clone();
    let on_busy: Arc<dyn Fn(Vec<String>) + Send + Sync> = Arc::new(move |names| { let _ = emit_app.emit("crewforge://running", names); });
    match crewforge_core::autopilot::Autopilot::new_with_busy(orch, sched, approver, ctl, run, running, on_busy) {
        Ok(ap) => {
            let app2 = app.clone();
            tauri::async_runtime::spawn(async move {
                let finished = ap.run().await;
                autopilots.lock().unwrap().remove(&project_id);
                continue_after_budget(&app2, finished);
            });
        }
        Err(e) => {
            state.autopilots.lock().unwrap().remove(&project_id);
            let _ = state.db.set_autopilot_run(&run.id, "failed", 0, &e.to_string());
        }
    }
}

/// 批次结束后：预算正常用完且项目开了“自动续批次”，就接着开下一个；到了连续批次的上限就停下并说明。
fn continue_after_budget(app: &AppHandle, finished: crewforge_core::autopilot::AutopilotRun) {
    use crewforge_core::autopilot::AutoContinue;
    let state = app.state::<AppState>();
    match state.db.auto_continue_decision(&finished) {
        Ok(AutoContinue::Next { max_calls, max_minutes, chain }) => {
            match crewforge_core::autopilot::Autopilot::<UiApprover>::prepare_continue(&state.orch, &finished.project_id, max_calls, max_minutes, chain) {
                Ok(run) => {
                    let _ = state.db.append_event_for(Some(&finished.project_id), "autopilot_step", None, None, &serde_json::json!({"text": format!("上一个批次预算用完，自动续批次（连续第 {} 个）", chain + 1)}));
                    launch_autopilot(app, &state, &run);
                }
                Err(e) => {
                    let _ = state.db.append_event_for(Some(&finished.project_id), "autopilot_step", None, None, &serde_json::json!({"text": format!("自动续批次没有开始：{e}")}));
                }
            }
        }
        Ok(AutoContinue::LimitReached { batches }) => {
            let _ = state.db.append_event_for(Some(&finished.project_id), "autopilot_chain_limit", None, None, &serde_json::json!({"text": format!("自动续批次已经连续 {batches} 个批次，到了设置的上限，停下来等你。可以到“我的职责”页调整上限，或手动再开一个批次")}));
        }
        _ => {}
    }
}

/// 应用启动时：上次被中断的批次，按项目里的设置（默认开启）接着推进，预算是剩余的那部分。
/// 稍等几秒再开始，让界面和事件推送先就绪；每个项目恢复前都会重新检查条件（有负责人、没有归档等）。
pub async fn resume_interrupted_autopilots(app: AppHandle, interrupted: Vec<crewforge_core::autopilot::AutopilotRun>) {
    if interrupted.is_empty() {
        return;
    }
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    let state = app.state::<AppState>();
    let now = crewforge_core::db::now_ms();
    for old in interrupted {
        use crewforge_core::autopilot::{Autopilot, ResumeDecision};
        let decision = state.db.resume_decision(&old, now).unwrap_or(ResumeDecision::Skip);
        let (prepared, note) = match decision {
            ResumeDecision::Skip => continue,
            ResumeDecision::Resume { calls, minutes } => (
                Autopilot::<UiApprover>::prepare_resume(&state.orch, &old.project_id, calls, minutes),
                format!("应用重启，自动接着上次的推进：剩余预算 {calls} 次调用 / {}。不想自动恢复，可以在“我的职责”页关掉这个选项", if minutes > 0 { format!("{minutes} 分钟") } else { "不限时间".to_string() })),
            ResumeDecision::FreshBatch { calls, minutes, chain } => (
                Autopilot::<UiApprover>::prepare_continue(&state.orch, &old.project_id, calls, minutes, chain),
                format!("应用重启，上次批次的预算已经不够，按项目设置自动续批次（连续第 {} 个）", chain + 1)),
        };
        match prepared {
            Ok(run) => {
                let _ = state.db.append_event_for(Some(&old.project_id), "autopilot_step", None, None, &serde_json::json!({"text": note}));
                launch_autopilot(&app, &state, &run);
            }
            Err(e) => {
                let _ = state.db.append_event_for(Some(&old.project_id), "autopilot_step", None, None, &serde_json::json!({"text": format!("应用重启后没有自动恢复推进：{e}")}));
            }
        }
    }
}

#[tauri::command]
pub fn get_autopilot_resume(state: State<'_, AppState>, project_id: String) -> bool {
    state.db.autopilot_resume_enabled(&project_id)
}

#[tauri::command]
pub fn set_autopilot_resume(state: State<'_, AppState>, project_id: String, enabled: bool) -> CmdResult<()> {
    state.db.set_autopilot_resume(&project_id, enabled).map_err(err)
}

/// 暂停：手上正在做的做完就停，不再派新活。
#[tauri::command]
pub fn pause_autopilot(state: State<'_, AppState>, project_id: String) -> CmdResult<()> {
    let ctl = state.autopilots.lock().unwrap().get(&project_id).cloned().ok_or("这个项目现在没有在自主推进")?;
    ctl.pause.store(true, std::sync::atomic::Ordering::SeqCst);
    state.db.append_event_for(Some(&project_id), "autopilot_step", None, None, &serde_json::json!({"text": "你请求暂停：手上的做完就停，不再派新活"})).map_err(err)?;
    Ok(())
}

#[tauri::command]
pub fn stop_autopilot(state: State<'_, AppState>, project_id: String) -> CmdResult<()> {
    let ctl = state.autopilots.lock().unwrap().get(&project_id).cloned().ok_or("这个项目现在没有在自主推进")?;
    ctl.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    state.db.append_event_for(Some(&project_id), "autopilot_step", None, None, &serde_json::json!({"text": "你请求停止"})).map_err(err)?;
    Ok(())
}

#[derive(serde::Serialize)]
pub struct AutopilotStatus {
    run: Option<crewforge_core::autopilot::AutopilotRun>,
    live: bool,
}

#[tauri::command]
pub fn autopilot_status(state: State<'_, AppState>, project_id: String) -> CmdResult<AutopilotStatus> {
    Ok(AutopilotStatus { run: state.db.latest_autopilot_run(&project_id).map_err(err)?, live: state.autopilots.lock().unwrap().contains_key(&project_id) })
}

// ================= 引擎与模型 =================
#[derive(serde::Serialize)]
pub struct ProviderStatus {
    provider: String,
    label: String,
    enabled: bool,
    /// 是否检测到可执行文件，以及版本
    installed: bool,
    version: String,
    /// 实际使用的可执行文件路径（Copilot 可在设置里指定）
    program: String,
    employees: usize,
}

#[derive(serde::Serialize)]
pub struct ModelsOverview {
    providers: Vec<ProviderStatus>,
    models: Vec<crewforge_core::models::ModelEntry>,
    /// 用途 → (引擎 → 默认模型)；空字符串表示引擎默认
    purposes: Vec<(String, String, std::collections::HashMap<String, String>)>,
    /// Claude 各用途的推荐：(用途, 模型, 理由)
    recommended: Vec<(String, String, String)>,
}

fn copilot_program(state: &AppState) -> String {
    state.db.get_setting("copilot_program").ok().flatten().filter(|p| !p.trim().is_empty()).unwrap_or_else(|| "copilot".into())
}

async fn detect_version(program: &str) -> Option<String> {
    let out = tokio::process::Command::new(program).arg("--version").output().await.ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").trim().to_string())
}

#[tauri::command]
pub async fn models_overview(state: State<'_, AppState>) -> CmdResult<ModelsOverview> {
    let emps = state.db.list_employees().map_err(err)?;
    let mut providers = Vec::new();
    for (p, label, program) in [("claude", "Claude Code", "claude".to_string()), ("copilot", "GitHub Copilot CLI", copilot_program(&state))] {
        let version = detect_version(&program).await;
        providers.push(ProviderStatus {
            provider: p.into(), label: label.into(), enabled: state.db.provider_enabled(p).map_err(err)?,
            installed: version.is_some(), version: version.unwrap_or_default(), program,
            employees: emps.iter().filter(|e| e.provider == p).count(),
        });
    }
    let mut purposes = Vec::new();
    for (k, label) in crewforge_core::models::PURPOSES {
        let mut m = std::collections::HashMap::new();
        for p in ["claude", "copilot"] {
            m.insert(p.to_string(), state.db.purpose_model(p, k).map_err(err)?);
        }
        purposes.push((k.to_string(), label.to_string(), m));
    }
    let recommended = crewforge_core::models::RECOMMENDED_CLAUDE.iter().map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string())).collect();
    Ok(ModelsOverview { providers, models: state.db.list_models(None).map_err(err)?, purposes, recommended })
}

#[tauri::command]
pub fn set_provider_enabled(state: State<'_, AppState>, provider: String, enabled: bool) -> CmdResult<()> {
    state.db.set_provider_enabled(&provider, enabled).map_err(err)
}

#[tauri::command]
pub fn add_model(state: State<'_, AppState>, provider: String, model_id: String, label: String) -> CmdResult<()> {
    state.db.add_model(&provider, &model_id, &label).map_err(err)
}

#[tauri::command]
pub fn set_model_enabled(state: State<'_, AppState>, provider: String, model_id: String, enabled: bool) -> CmdResult<()> {
    state.db.set_model_enabled(&provider, &model_id, enabled).map_err(err)
}

#[tauri::command]
pub fn remove_model(state: State<'_, AppState>, provider: String, model_id: String) -> CmdResult<()> {
    state.db.remove_model(&provider, &model_id).map_err(err)
}

/// 把推荐模型填进 Claude 还没设置的用途；返回填了几项。
#[tauri::command]
pub fn apply_recommended_models(state: State<'_, AppState>) -> CmdResult<usize> {
    state.db.apply_recommended_models().map_err(err)
}

#[tauri::command]
pub fn set_purpose_model(state: State<'_, AppState>, provider: String, purpose: String, model: String) -> CmdResult<()> {
    state.db.set_purpose_model(&provider, &purpose, model.trim()).map_err(err)
}

/// 从 Copilot CLI 自己的说明里读出可用模型并并入清单。返回新增数量。
#[tauri::command]
pub async fn discover_copilot_models(state: State<'_, AppState>) -> CmdResult<usize> {
    let program = copilot_program(&state);
    let out = tokio::process::Command::new(&program).args(["help", "config"]).output().await.map_err(|e| format!("无法运行 {program}：{e}"))?;
    let ids = crewforge_core::models::parse_copilot_models(&String::from_utf8_lossy(&out.stdout));
    if ids.is_empty() {
        return Err("没有从 Copilot CLI 的说明里读到模型清单（版本可能变了）。可以手动添加模型。".into());
    }
    state.db.merge_discovered_models("copilot", &ids).map_err(err)
}

/// 把 Copilot CLI 安装到应用自己的数据目录（不动系统全局），并记下路径。
#[tauri::command]
pub async fn install_copilot(state: State<'_, AppState>) -> CmdResult<String> {
    let dir = crewforge_core::db::default_data_dir().join("tools");
    std::fs::create_dir_all(&dir).map_err(err)?;
    let out = tokio::process::Command::new("npm").args(["install", "--prefix"]).arg(&dir).arg("@github/copilot").output().await
        .map_err(|e| format!("无法运行 npm：{e}。请先安装 Node.js"))?;
    if !out.status.success() {
        return Err(format!("安装失败：{}", String::from_utf8_lossy(&out.stderr).chars().take(400).collect::<String>()));
    }
    let bin = dir.join("node_modules/.bin/copilot");
    state.db.set_setting("copilot_program", &bin.to_string_lossy()).map_err(err)?;
    detect_version(&bin.to_string_lossy()).await.ok_or_else(|| "已安装，但无法运行".to_string())
}

#[tauri::command]
pub fn set_copilot_program(state: State<'_, AppState>, path: String) -> CmdResult<()> {
    state.db.set_setting("copilot_program", path.trim()).map_err(err)
}

#[tauri::command]
pub fn my_duties(state: State<'_, AppState>, project_id: String) -> CmdResult<crewforge_core::duties::Duties> {
    state.db.my_duties(&project_id).map_err(err)
}

// ================= 用量页（参照 CC Switch） =================
#[tauri::command]
pub fn usage_overview(state: State<'_, AppState>, filter: crewforge_core::usage::UsageFilter, tz_offset_minutes: i64) -> CmdResult<crewforge_core::usage::UsageOverview> {
    state.db.usage_overview(&filter, tz_offset_minutes).map_err(err)
}

#[tauri::command]
pub fn usage_stats(state: State<'_, AppState>, filter: crewforge_core::usage::UsageFilter, dim: String) -> CmdResult<Vec<crewforge_core::cost::Group>> {
    state.db.usage_stats(&filter, &dim).map_err(err)
}

#[tauri::command]
pub fn usage_sessions(state: State<'_, AppState>) -> CmdResult<crewforge_core::usage::SessionsView> {
    state.db.session_contexts().map_err(err)
}

#[tauri::command]
pub fn usage_calls(state: State<'_, AppState>, filter: crewforge_core::usage::UsageFilter, page: usize, page_size: usize) -> CmdResult<crewforge_core::usage::UsagePage> {
    state.db.usage_calls(&filter, page, page_size).map_err(err)
}

// ================= 成本与额度 =================
/// 调用明细：每次模型调用一行，最新的在前，可过滤与分页。
#[tauri::command]
pub fn cost_calls(
    state: State<'_, AppState>, days: Option<i64>, employee_id: Option<String>, project_id: Option<String>,
    purpose: Option<String>, provider: Option<String>, only_errors: bool, limit: usize, offset: usize,
) -> CmdResult<CallPage> {
    let since = days.filter(|d| *d > 0).map(|d| crewforge_core::db::now_ms() - d * 86_400_000);
    let f = CallFilter { employee_id, project_id, purpose, provider, only_errors };
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

/// 项目里全部行动项（行动项页用）。
#[tauri::command]
pub fn list_project_actions(state: State<'_, AppState>, project_id: String) -> CmdResult<Vec<ActionItem>> {
    state.db.actions_for_project(&project_id).map_err(err)
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
pub fn goal_proposal_counts(state: State<'_, AppState>) -> CmdResult<Vec<(String, String, i64)>> {
    state.db.goal_proposal_counts().map_err(err)
}

#[tauri::command]
pub fn pending_goal_proposal(state: State<'_, AppState>, project_id: String) -> CmdResult<Option<crewforge_core::plan::GoalProposal>> {
    state.db.pending_goal_proposal(&project_id).map_err(err)
}

/// 人决定负责人的目标变更提议：批准（可先改写新目标）或驳回（必须写原因）。
#[tauri::command]
pub fn decide_goal_proposal(state: State<'_, AppState>, id: String, approve: bool, edited_goal: Option<String>, note: String) -> CmdResult<()> {
    state.db.decide_goal_proposal(&id, approve, edited_goal.as_deref(), &note).map_err(err)
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

// ---------- LLM 端点（系统级配置） ----------

#[tauri::command]
pub fn list_endpoints(state: State<'_, AppState>) -> CmdResult<Vec<crewforge_core::endpoint::Endpoint>> {
    state.db.list_endpoints().map_err(err)
}

#[tauri::command]
pub fn add_endpoint(state: State<'_, AppState>, input: crewforge_core::endpoint::EndpointInput) -> CmdResult<crewforge_core::endpoint::Endpoint> {
    state.db.add_endpoint(&input).map_err(err)
}

#[tauri::command]
pub fn update_endpoint(state: State<'_, AppState>, id: String, input: crewforge_core::endpoint::EndpointInput) -> CmdResult<()> {
    state.db.update_endpoint(&id, &input).map_err(err)
}

#[tauri::command]
pub fn set_endpoint_enabled(state: State<'_, AppState>, id: String, enabled: bool) -> CmdResult<()> {
    state.db.set_endpoint_enabled(&id, enabled).map_err(err)
}

#[tauri::command]
pub fn remove_endpoint(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    state.db.remove_endpoint(&id).map_err(err)
}

/// 给员工选端点（endpoint_id 为空 = 本机 Claude 订阅）和模型。
#[tauri::command]
pub fn set_employee_endpoint(state: State<'_, AppState>, id: String, endpoint_id: Option<String>, model: String) -> CmdResult<()> {
    let emp = state.db.employee_by_id(&id).map_err(err)?.ok_or("员工不存在")?;
    if emp.endpoint_id != endpoint_id && state.running.lock().unwrap().contains(&emp.name) {
        return Err("该员工正在执行任务，等 TA 完成后再切换".into());
    }
    state.db.set_employee_endpoint(&id, endpoint_id.as_deref().filter(|e| !e.is_empty()), &model).map_err(err)
}

#[derive(serde::Serialize)]
pub struct EndpointTest {
    ok: bool,
    millis: u64,
    /// 成功时是模型的回复，失败时是原因
    detail: String,
}

/// 由应用进程自己连一次 TCP。macOS 的“本地网络”权限只拦第三方进程，而系统自带的 curl 不受限，
/// 所以只用 curl 测会“测试通过、员工却连不上”；这一步让测试和员工实际遇到的限制一致。
async fn precheck_reachable(base: &str) -> Result<(), String> {
    let rest = base.split("://").nth(1).unwrap_or(base);
    let hostport = rest.split('/').next().unwrap_or(rest);
    let addr = if hostport.contains(':') { hostport.to_string() } else { format!("{hostport}:{}", if base.starts_with("https") { 443 } else { 80 }) };
    match tokio::time::timeout(std::time::Duration::from_secs(8), tokio::net::TcpStream::connect(&addr)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) if e.raw_os_error() == Some(65) => Err(format!("连不上 {addr}：系统返回“无路由”。如果这个地址在浏览器里或用 curl 能通，多半是 macOS 的“本地网络”权限拦住了：到 系统设置 → 隐私与安全性 → 本地网络，打开 Crewforge（从终端运行时要打开终端 App）。没有这一项时，可以先退出并重新打开 Crewforge 再测一次触发授权弹窗。")),
        Ok(Err(e)) => Err(format!("连不上 {addr}：{e}。检查地址和端口、对方电脑的防火墙，以及 Ollama 是否设了 OLLAMA_HOST=0.0.0.0")),
        Err(_) => Err(format!("连接 {addr} 超时（8 秒）。检查对方电脑是否开机、是否在同一网络。")),
    }
}

/// 用 curl 发一个请求（令牌走标准输入的配置，不出现在进程参数里）。返回 (HTTP 状态码, 响应体)。
async fn curl_json(url: &str, token: Option<&str>, body: Option<&str>, secs: u32) -> Result<(u16, String), String> {
    use tokio::io::AsyncWriteExt;
    let mut cfg = String::new();
    if let Some(t) = token {
        let t = t.replace('\\', "\\\\").replace('"', "\\\"");
        cfg += &format!("header = \"x-api-key: {t}\"\nheader = \"authorization: Bearer {t}\"\n");
    }
    cfg += "header = \"anthropic-version: 2023-06-01\"\nheader = \"content-type: application/json\"\n";
    let tmp = std::env::temp_dir().join(format!("crewforge-ep-{}-{}.json", std::process::id(), crewforge_core::db::now_ms()));
    let mut cmd = tokio::process::Command::new("curl");
    cmd.args(["-sS", "-K", "-", "--max-time", &secs.to_string(), "-w", "\n%{http_code}"]);
    // 配置走 -K -（标准输入），请求体走临时文件，避免两个都占标准输入
    if let Some(b) = body {
        std::fs::write(&tmp, b).map_err(err)?;
        cmd.args(["-X", "POST", "--data-binary"]).arg(format!("@{}", tmp.display()));
    }
    cmd.arg(url).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("无法运行 curl：{e}"))?;
    child.stdin.take().unwrap().write_all(cfg.as_bytes()).await.map_err(err)?;
    let out = child.wait_with_output().await.map_err(err)?;
    let _ = std::fs::remove_file(&tmp);
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        return Err(format!("连不上 {url}：{}。检查地址、端口、防火墙，以及对方 Ollama 是否设了 OLLAMA_HOST=0.0.0.0", why.trim()));
    }
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let (body, code) = text.rsplit_once('\n').unwrap_or((&text, "0"));
    Ok((code.trim().parse().unwrap_or(0), body.to_string()))
}

/// 测试连接：发一个最小的 Anthropic 消息请求，确认网络、模型名和接口兼容性都没问题。
/// 填了 id 且没填令牌时，用已保存的令牌。
#[tauri::command]
pub async fn test_endpoint(state: State<'_, AppState>, base_url: String, model: String, token: Option<String>, id: Option<String>) -> CmdResult<EndpointTest> {
    let base = crewforge_core::endpoint::normalize_base_url(&base_url).map_err(err)?;
    let model = model.trim().to_string();
    if model.is_empty() {
        return Err("先填一个模型名".into());
    }
    precheck_reachable(&base).await?;
    let token = token.filter(|t| !t.trim().is_empty()).or_else(|| id.as_deref().and_then(|i| state.db.endpoint_token(i)));
    let body = serde_json::json!({"model": model, "max_tokens": 128, "messages": [{"role": "user", "content": "只回复两个字母：ok"}]}).to_string();
    let t0 = std::time::Instant::now();
    // 第一次请求要把模型载入显存，给足时间
    let (code, text) = curl_json(&format!("{base}/v1/messages"), token.as_deref().or(Some("ollama")), Some(&body), 180).await?;
    let millis = t0.elapsed().as_millis() as u64;
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    if code == 200 {
        let reply = v["content"].as_array().map(|a| a.iter().filter_map(|c| c["text"].as_str()).collect::<String>()).unwrap_or_default();
        if reply.trim().is_empty() && v["content"].as_array().map(|a| a.is_empty()).unwrap_or(true) {
            return Ok(EndpointTest { ok: false, millis, detail: format!("接口返回了 200 但没有内容，可能不是 Anthropic 兼容接口：{}", text.chars().take(200).collect::<String>()) });
        }
        return Ok(EndpointTest { ok: true, millis, detail: format!("模型回复：{}", reply.trim()) });
    }
    let msg = v["error"]["message"].as_str().or(v["error"].as_str()).map(String::from).unwrap_or_else(|| text.chars().take(200).collect());
    let hint = match code {
        404 => "（模型名可能不对，或这个服务不支持 /v1/messages；Ollama 需要较新版本）",
        401 | 403 => "（令牌不对）",
        _ => "",
    };
    Ok(EndpointTest { ok: false, millis, detail: format!("HTTP {code}：{msg}{hint}") })
}

/// 从 Ollama 读出服务器上已有的模型（/api/tags），方便填清单。
#[tauri::command]
pub async fn discover_endpoint_models(base_url: String) -> CmdResult<Vec<String>> {
    let base = crewforge_core::endpoint::normalize_base_url(&base_url).map_err(err)?;
    precheck_reachable(&base).await?;
    let (code, text) = curl_json(&format!("{base}/api/tags"), None, None, 15).await?;
    if code != 200 {
        return Err(format!("读取模型清单失败（HTTP {code}）。不是 Ollama 的话请手动填写模型名。"));
    }
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|_| "返回的不是 Ollama 的模型清单")?;
    Ok(v["models"].as_array().map(|a| a.iter().filter_map(|m| m["name"].as_str().map(String::from)).collect()).unwrap_or_default())
}

// ---------- 沙箱网络白名单（全局设置）与卡住的员工 ----------

#[tauri::command]
pub fn get_sandbox_domains(state: State<'_, AppState>) -> crewforge_core::sandbox_net::SandboxDomains {
    state.db.sandbox_domains()
}

#[derive(serde::Serialize)]
pub struct DomainCheck {
    domain: String,
    warning: Option<String>,
    already: bool,
}

/// 加入之前先检查：规整后的域名、是否是共享托管域名（要提醒风险）、是否已经在白名单里。
#[tauri::command]
pub fn check_sandbox_domain(state: State<'_, AppState>, domain: String) -> CmdResult<DomainCheck> {
    let d = crewforge_core::sandbox_net::normalize_domain(&domain).map_err(err)?;
    Ok(DomainCheck { warning: crewforge_core::sandbox_net::shared_host_warning(&d), already: state.db.is_sandbox_domain_allowed(&d), domain: d })
}

#[tauri::command]
pub fn add_sandbox_domain(state: State<'_, AppState>, domain: String) -> CmdResult<String> {
    state.db.add_sandbox_domain(&domain).map_err(err)
}

#[tauri::command]
pub fn remove_sandbox_domain(state: State<'_, AppState>, domain: String) -> CmdResult<()> {
    state.db.remove_sandbox_domain(&domain).map_err(err)
}

/// 看起来卡住的员工（待办）。
#[tauri::command]
pub fn stuck_items(state: State<'_, AppState>) -> CmdResult<Vec<crewforge_core::stuck::StuckItem>> {
    let names: Vec<String> = state.running.lock().unwrap().iter().cloned().collect();
    let items = state.db.stuck_items(&names, crewforge_core::db::now_ms()).map_err(err)?;
    // 记进人工介入历史：待办消失后还能回头看
    let _ = state.db.record_stuck(&items);
    Ok(items)
}

#[derive(serde::Serialize)]
pub struct BlockedMergeView {
    #[serde(flatten)]
    item: crewforge_core::stuck::BlockedMerge,
    /// 主工作区现在还有哪些未提交的改动（只列已跟踪文件，它们才会挡住合并）；已经干净则为空
    dirty_files: Vec<String>,
    /// 现在是否可以直接重试合并
    can_retry: bool,
}

/// 合并被拦的任务（待办）。被“主工作区有未提交改动”拦下的，实时看主工作区现在干净了没有。
#[tauri::command]
pub async fn blocked_merges(state: State<'_, AppState>) -> CmdResult<Vec<BlockedMergeView>> {
    let mut out = Vec::new();
    for item in state.db.blocked_merges(crewforge_core::db::now_ms()).map_err(err)? {
        let mut dirty_files = Vec::new();
        if item.kind == "blocked" && item.reason.contains("未提交") {
            if let Ok(o) = tokio::process::Command::new("git").arg("-C").arg(&item.repo_path).args(["status", "--porcelain", "--untracked-files=no"]).output().await {
                dirty_files = String::from_utf8_lossy(&o.stdout).lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
            }
        }
        let can_retry = item.kind != "blocked" || dirty_files.is_empty();
        out.push(BlockedMergeView { item, dirty_files, can_retry });
    }
    Ok(out)
}

/// 人工介入中心：最近 `days` 天里每一次需要人出手的事，以及各类别的数量。
#[tauri::command]
pub fn interventions(state: State<'_, AppState>, days: i64, category: Option<String>) -> CmdResult<crewforge_core::interventions::InterventionReport> {
    let since = crewforge_core::db::now_ms() - days.clamp(1, 365) * 24 * 3_600_000;
    state.db.interventions(since, category.as_deref().filter(|c| !c.is_empty()), 300).map_err(err)
}

/// 手动记一笔：人在应用之外做的事（比如在终端里手动执行了命令）。
#[tauri::command]
pub fn add_intervention_note(state: State<'_, AppState>, project_id: Option<String>, text: String) -> CmdResult<()> {
    state.db.add_intervention_note(project_id.as_deref().filter(|p| !p.is_empty()), &text).map_err(err)
}

/// 记录界面上弹出的出错提示，进入人工介入历史。
#[tauri::command]
pub fn record_ui_error(state: State<'_, AppState>, text: String) {
    let _ = state.db.record_ui_error(&text);
}

/// 后端定时检测卡住的员工（不依赖界面开着）：发现新的就记进人工介入历史，并弹一条系统通知。
pub async fn stuck_tick(app: &AppHandle) {
    let st = app.state::<AppState>();
    let names: Vec<String> = st.running.lock().unwrap().iter().cloned().collect();
    if names.is_empty() {
        return;
    }
    let Ok(items) = st.db.stuck_items(&names, crewforge_core::db::now_ms()) else { return };
    let Ok(fresh) = st.db.record_stuck(&items) else { return };
    for x in fresh {
        let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', " ");
        let body: String = x.detail.chars().take(80).collect();
        let script = format!("display notification \"{}\" with title \"Crewforge：{}\"", esc(&body), esc(&x.title));
        let _ = tokio::process::Command::new("osascript").args(["-e", &script]).output().await;
    }
}

#[tauri::command]
pub fn get_autopilot_config(state: State<'_, AppState>, project_id: String) -> CmdResult<crewforge_core::autopilot::AutopilotConfig> {
    state.db.autopilot_config(&project_id).map_err(err)
}

#[tauri::command]
pub fn set_autopilot_config(state: State<'_, AppState>, project_id: String, max_calls: i64, max_minutes: i64, auto_continue: bool, max_batches: i64, standup_blocks: bool) -> CmdResult<()> {
    state.db.set_autopilot_config(&project_id, &crewforge_core::autopilot::AutopilotConfig { max_calls, max_minutes, auto_continue, max_batches, standup_blocks }).map_err(err)
}
