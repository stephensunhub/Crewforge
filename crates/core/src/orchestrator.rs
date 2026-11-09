//! 编排：把"给员工分配任务"落实为 worktree 隔离下的一次执行，并负责检查点与合并。
//!
//! 一个员工在一个仓库里同一时刻只有一个进行中的任务；后续消息继续该任务，
//! 直到它被合并或放弃，才能开始新任务。

use crate::db::{Db, Employee, Task, now_ms};
use crate::provider::claude::TurnResult;
use crate::runner::{Approver, Observer, TurnConfig, run_turn};
use crate::workspace::{FileChange, MergeCheck, MergeOutcome, Overlap, WorkspaceManager, find_overlaps, safe_slug};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MAX_DIFF_BYTES: usize = 400_000;

/// 这次派活是为了处理哪个评审行动项、或哪个计划条目。
#[derive(Debug, Clone, Copy)]
pub enum TaskLink<'a> {
    Action(&'a str),
    Plan(&'a str),
}

pub struct Orchestrator {
    pub db: Arc<Db>,
    pub ws: Arc<WorkspaceManager>,
    /// CLI 可执行文件，默认 `claude`；测试中替换为假的 CLI
    pub program: String,
    /// Copilot 员工使用的 CLI，默认 `copilot`；测试中替换为假的
    pub copilot_program: String,
    /// 提供团队协作工具（MCP）所用的可执行文件：就是应用或命令行自身的 `mcp-serve` 子命令；None 表示不提供
    pub mcp_exe: Option<std::path::PathBuf>,
}

/// 任务关联的评审会议：合并前要让人看到评审里提出了什么、有没有结束。
#[derive(Debug, Clone, Serialize)]
pub struct ReviewInfo {
    pub meeting_id: String,
    pub title: String,
    pub status: String,
    /// 这场评审产生的行动项（含状态）
    pub items: Vec<crate::actions::ActionItem>,
    pub agent_replies: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskView {
    pub task: Task,
    pub employee_name: String,
    pub files: Vec<FileChange>,
    /// 仅进行中的任务会预检冲突
    pub merge: Option<MergeCheck>,
    /// 与其他进行中任务同时修改的文件
    pub overlaps: Vec<Overlap>,
    /// 关联的评审会议（仅进行中的任务会查询）
    pub reviews: Vec<ReviewInfo>,
}

fn title_of(prompt: &str) -> String {
    let first = prompt.trim().lines().next().unwrap_or("").trim();
    let t: String = first.chars().take(60).collect();
    if t.is_empty() { "未命名任务".into() } else { t }
}

impl Orchestrator {
    pub fn new(db: Arc<Db>, ws: Arc<WorkspaceManager>) -> Self {
        Self { db, ws, program: "claude".into(), copilot_program: "copilot".into(), mcp_exe: std::env::current_exe().ok() }
    }

    /// 给员工分配工作：继续其进行中的任务，没有则新建任务和分支。
    pub async fn assign<A: Approver, O: Observer>(
        &self,
        employee: &Employee,
        project_path: &Path,
        prompt: &str,
        approver: &A,
        observer: &O,
    ) -> Result<TurnResult> {
        self.assign_linked(employee, project_path, prompt, None, approver, observer).await
    }

    /// 同 `assign`；`action_item` 不为空时，把这条评审行动项标为“处理中”并关联到所在的任务。
    pub(crate) async fn assign_linked<A: Approver, O: Observer>(
        &self,
        employee: &Employee,
        project_path: &Path,
        prompt: &str,
        link: Option<TaskLink<'_>>,
        approver: &A,
        observer: &O,
    ) -> Result<TurnResult> {
        let repo = self.ws.repo_info(project_path).await?;
        let repo_key = repo.root.to_string_lossy().into_owned();

        let project = self.db.project_by_path(&repo_key)?;
        if project.as_ref().is_some_and(|p| p.onboarding == "in_progress") {
            bail!("项目还在导入阶段：请先完成扫描、测试基线和评估，并确认导入，之后才能给员工派任务");
        }
        let goal = project.as_ref().map(|p| p.goal.trim().to_string()).unwrap_or_default();
        let project_id = project.map(|p| p.id);
        // 只在新任务开始时注入记忆：继续同一任务时，会话里已经有这些内容，不重复传递
        let (task, fresh) = match self.db.active_task(&employee.id, &repo_key)? {
            Some(t) => (t, false),
            None => (self.start_task(employee, &repo.root, &repo.branch, prompt).await?, true),
        };
        match link {
            Some(TaskLink::Action(a)) => self.db.mark_action_working(a, &task.id)?,
            Some(TaskLink::Plan(p)) => self.db.mark_plan_started(p, &task.id)?,
            None => {}
        }
        let block = if fresh {
            self.memory_block(project_id.as_deref(), employee, Some(&task.id), prompt, "task", &task.id)
        } else {
            String::new()
        };
        // 项目的目标与硬性约束（比如“技术栈升级、业务行为不变”）：每个新任务开始时都带上
        let goal_block = if fresh && !goal.is_empty() { format!("[项目目标与硬性约束]\n{goal}\n\n") } else { String::new() };
        let injected = !block.is_empty() || !goal_block.is_empty();
        let full_prompt = if injected { format!("{goal_block}{block}\n{prompt}") } else { prompt.to_string() };

        let cfg = TurnConfig {
            project_path: PathBuf::from(&task.worktree_path),
            task_id: Some(task.id.clone()),
            mcp: project_id.as_deref().and_then(|p| self.mcp_launch(&employee.id, p, &format!("task:{}", task.id))),
            attr_project: project_id.clone(),
            project_id,
            read_only: false,
            purpose: "task".into(),
            meeting_id: None,
            // 事件里只记录原始任务，不重复存注入的记忆（注入量见 memory_injected 事件）
            log_prompt: injected.then(|| prompt.to_string()),
            program: self.program.clone(),
            copilot_program: self.copilot_program.clone(),
        };
        let result = run_turn(&self.db, employee, &cfg, &full_prompt, approver, observer).await;
        if let Ok(r) = &result {
            if crate::protocol::enabled(&self.db) {
                let flags = crate::protocol::lint_report(&r.text);
                if !flags.is_empty() {
                    let _ = self.db.append_event_for(cfg.project_id.as_deref(), "report_flags", Some(&employee.id), None,
                        &serde_json::json!({"task_id": task.id, "flags": flags, "chars": r.text.chars().count()}));
                }
            }
        }

        // 无论成功与否都保存检查点：被中断时的进展同样不能丢
        let label = if result.is_ok() { "" } else { " (WIP，执行被中断)" };
        let msg = format!(
            "[crewforge] {}: {}{label}\n\nCrewforge-Task: {}\nCrewforge-Employee: {}",
            employee.name, task.title, task.id, employee.name
        );
        self.checkpoint_task(&task, employee, &msg).await;
        result
    }

    /// 仓库路径对应的项目（未登记为项目时为 None，例如命令行直接使用）。
    fn project_of(&self, repo_path: &str) -> Option<String> {
        self.db.project_by_path(repo_path).ok().flatten().map(|p| p.id)
    }

    async fn start_task(&self, employee: &Employee, repo: &Path, base_branch: &str, prompt: &str) -> Result<Task> {
        let id = uuid::Uuid::new_v4().to_string();
        let slug = safe_slug(&employee.name, &employee.id);
        let branch = format!("crewforge/{slug}/t-{}", &id[..8]);
        let (worktree, base_commit) = self.ws.prepare_task(repo, &slug, &branch, base_branch).await?;
        let now = now_ms();
        let task = Task {
            id,
            repo_path: repo.to_string_lossy().into_owned(),
            employee_id: employee.id.clone(),
            title: title_of(prompt),
            branch,
            base_branch: base_branch.into(),
            base_commit,
            worktree_path: worktree.to_string_lossy().into_owned(),
            status: "active".into(),
            merged_commit: None,
            created_at: now,
            updated_at: now,
        };
        self.db.create_task(&task)?;
        self.db.append_event_for(self.project_of(&task.repo_path).as_deref(), "task_created", Some(&employee.id), None, &json!({
            "task_id": task.id, "title": task.title, "branch": task.branch,
            "base_branch": task.base_branch, "worktree": task.worktree_path,
        }))?;
        Ok(task)
    }

    async fn checkpoint_task(&self, task: &Task, employee: &Employee, msg: &str) {
        let eid = Some(employee.id.as_str());
        match self.ws.checkpoint(Path::new(&task.worktree_path), &employee.name, msg).await {
            Ok(Some(cp)) => {
                let _ = self.db.append_event_for(self.project_of(&task.repo_path).as_deref(), "checkpoint_committed", eid, None, &json!({
                    "task_id": task.id, "commit": cp.commit, "files": cp.files,
                    "insertions": cp.insertions, "deletions": cp.deletions,
                }));
            }
            Ok(None) => {}
            Err(e) => {
                let _ = self.db.append_event_for(self.project_of(&task.repo_path).as_deref(), "checkpoint_failed", eid, None,
                    &json!({"task_id": task.id, "error": e.to_string()}));
            }
        }
    }

    fn task_and_employee(&self, task_id: &str) -> Result<(Task, Employee)> {
        let task = self.db.get_task(task_id)?.context("任务不存在")?;
        let emp = self.db.employee_by_id(&task.employee_id)?.context("任务所属员工不存在")?;
        Ok((task, emp))
    }

    /// 合并任务（人类确认后调用）。先保存检查点，再 squash 合入基础分支。
    pub async fn merge_task(&self, task_id: &str) -> Result<MergeOutcome> {
        self.merge_task_ack(task_id, false).await
    }

    /// 合并任务。评审里还有标为“合并前”、没处理完的行动项时，默认不合并并返回这些条目；
    /// `acknowledge_open_items` 为 true 表示人类明知如此仍然合并，会留下记录。
    pub async fn merge_task_ack(&self, task_id: &str, acknowledge_open_items: bool) -> Result<MergeOutcome> {
        let (task, emp) = self.task_and_employee(task_id)?;
        if task.status != "active" {
            bail!("任务已经是 {} 状态，不能合并", task.status);
        }
        let eid = Some(emp.id.as_str());
        let open_items = self.db.blocking_actions_for_task(task_id)?;
        if !open_items.is_empty() && !acknowledge_open_items {
            let items: Vec<String> = open_items.iter().map(|a| a.text.clone()).collect();
            self.db.append_event_for(self.project_of(&task.repo_path).as_deref(), "task_merge_blocked", eid, None,
                &json!({"task_id": task.id, "kind": "action_items", "items": items}))?;
            return Ok(MergeOutcome::OpenActionItems { items });
        }
        self.checkpoint_task(&task, &emp, &format!("[crewforge] {}: {} (合并前保存)\n\nCrewforge-Task: {}", emp.name, task.title, task.id)).await;

        let message = format!(
            "{}\n\nCrewforge-Task: {}\nCrewforge-Employee: {}\nCrewforge-Branch: {}",
            task.title, task.id, emp.name, task.branch
        );
        let repo = Path::new(&task.repo_path);
        let outcome = self.ws.merge(repo, &task.base_branch, &task.branch, &emp.name, &message).await?;
        match &outcome {
            MergeOutcome::Merged { commit } => {
                if !open_items.is_empty() {
                    self.db.append_event_for(self.project_of(&task.repo_path).as_deref(), "merged_with_open_items", eid, None,
                        &json!({"task_id": task.id, "commit": commit, "items": open_items.iter().map(|a| a.text.clone()).collect::<Vec<_>>()}))?;
                }
                self.db.set_task_status(&task.id, "merged", Some(commit))?;
                self.db.complete_plan_for_task(&task.id)?;
                // 合并即视为验证：关联该任务的待验证经验转为已验证
                self.db.verify_task_memories(&task.id, Some(commit))?;
                self.db.append_event_for(self.project_of(&task.repo_path).as_deref(), "task_merged", eid, None, &json!({"task_id": task.id, "commit": commit, "title": task.title}))?;
            }
            MergeOutcome::Empty => {
                self.db.set_task_status(&task.id, "merged", None)?;
                self.db.complete_plan_for_task(&task.id)?;
                self.db.append_event_for(self.project_of(&task.repo_path).as_deref(), "task_merged", eid, None, &json!({"task_id": task.id, "commit": null, "title": task.title, "empty": true}))?;
            }
            MergeOutcome::Conflict { files } => {
                self.db.append_event_for(self.project_of(&task.repo_path).as_deref(), "task_merge_blocked", eid, None, &json!({"task_id": task.id, "kind": "conflict", "files": files}))?;
            }
            MergeOutcome::Blocked { reason } => {
                self.db.append_event_for(self.project_of(&task.repo_path).as_deref(), "task_merge_blocked", eid, None, &json!({"task_id": task.id, "kind": "blocked", "reason": reason}))?;
            }
            MergeOutcome::OpenActionItems { .. } => {}
        }
        Ok(outcome)
    }

    /// 放弃任务：改动保存为 WIP 提交并保留分支，不删除任何内容。
    pub async fn abandon_task(&self, task_id: &str) -> Result<()> {
        let (task, emp) = self.task_and_employee(task_id)?;
        if task.status != "active" {
            bail!("任务已经是 {} 状态", task.status);
        }
        self.checkpoint_task(&task, &emp, &format!("[crewforge] {}: {} (已放弃，保留为 WIP)\n\nCrewforge-Task: {}", emp.name, task.title, task.id)).await;
        self.db.set_task_status(&task.id, "abandoned", None)?;
        // 对应的计划条目回到“待开始”，可以重新派
        self.db.release_plan_for_task(&task.id)?;
        self.db.append_event_for(self.project_of(&task.repo_path).as_deref(), "task_abandoned", Some(&emp.id), None,
            &json!({"task_id": task.id, "branch": task.branch, "title": task.title}))?;
        Ok(())
    }

    pub async fn task_diff(&self, task_id: &str) -> Result<String> {
        let task = self.db.get_task(task_id)?.context("任务不存在")?;
        if task.status == "active" {
            // 进行中：比较工作目录，包含尚未提交的改动
            let wt = Path::new(&task.worktree_path);
            if !wt.exists() {
                bail!("工作区目录已不存在");
            }
            self.ws.diff(wt, &task.base_commit, MAX_DIFF_BYTES).await
        } else {
            self.ws.diff_range(Path::new(&task.repo_path), &task.base_commit, &task.branch, MAX_DIFF_BYTES).await
        }
    }

    pub async fn task_views(&self, project_path: &Path) -> Result<Vec<TaskView>> {
        let st = self.ws.repo_status(project_path).await?;
        let Some(root) = st.root else { return Ok(vec![]) };
        let mut views = Vec::new();
        for task in self.db.list_tasks(&root)? {
            let employee_name = self.db.employee_by_id(&task.employee_id)?.map(|e| e.name).unwrap_or_default();
            let (mut files, mut merge) = (vec![], None);
            if task.status == "active" && Path::new(&task.worktree_path).exists() {
                files = self.ws.changed_files(Path::new(&task.worktree_path), &task.base_commit).await.unwrap_or_default();
                merge = self.ws.merge_check(Path::new(&task.repo_path), &task.base_branch, &task.branch).await.ok();
            }
            let mut reviews = Vec::new();
            if task.status == "active" {
                for m in self.db.meetings_for_task(&task.id)? {
                    reviews.push(ReviewInfo {
                        items: self.db.actions_for_meeting(&m.id)?.into_iter().filter(|a| a.status != "dropped").collect(),
                        agent_replies: self.db.agent_reply_count(&m.id)?, meeting_id: m.id, title: m.title, status: m.status,
                    });
                }
            }
            views.push(TaskView { task, employee_name, files, merge, overlaps: vec![], reviews });
        }
        let sets: Vec<(String, Vec<String>)> = views
            .iter()
            .filter(|v| v.task.status == "active")
            .map(|v| (v.task.id.clone(), v.files.iter().map(|f| f.path.clone()).collect()))
            .collect();
        let overlaps = find_overlaps(&sets);
        for v in views.iter_mut().filter(|v| v.task.status == "active") {
            v.overlaps = overlaps.iter().filter(|o| o.task_ids.contains(&v.task.id)).cloned().collect();
        }
        Ok(views)
    }
}
