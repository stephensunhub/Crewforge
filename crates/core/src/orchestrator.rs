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

pub struct Orchestrator {
    pub db: Arc<Db>,
    pub ws: Arc<WorkspaceManager>,
    /// CLI 可执行文件，默认 `claude`；测试中替换为假的 CLI
    pub program: String,
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
}

fn title_of(prompt: &str) -> String {
    let first = prompt.trim().lines().next().unwrap_or("").trim();
    let t: String = first.chars().take(60).collect();
    if t.is_empty() { "未命名任务".into() } else { t }
}

impl Orchestrator {
    pub fn new(db: Arc<Db>, ws: Arc<WorkspaceManager>) -> Self {
        Self { db, ws, program: "claude".into() }
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
        let repo = self.ws.repo_info(project_path).await?;
        let repo_key = repo.root.to_string_lossy().into_owned();

        let task = match self.db.active_task(&employee.id, &repo_key)? {
            Some(t) => t,
            None => self.start_task(employee, &repo.root, &repo.branch, prompt).await?,
        };

        let cfg = TurnConfig {
            project_path: PathBuf::from(&task.worktree_path),
            task_id: Some(task.id.clone()),
            program: self.program.clone(),
        };
        let result = run_turn(&self.db, employee, &cfg, prompt, approver, observer).await;

        // 无论成功与否都保存检查点：被中断时的进展同样不能丢
        let label = if result.is_ok() { "" } else { " (WIP，执行被中断)" };
        let msg = format!(
            "[crewforge] {}: {}{label}\n\nCrewforge-Task: {}\nCrewforge-Employee: {}",
            employee.name, task.title, task.id, employee.name
        );
        self.checkpoint_task(&task, employee, &msg).await;
        result
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
        self.db.append_event("task_created", Some(&employee.id), None, &json!({
            "task_id": task.id, "title": task.title, "branch": task.branch,
            "base_branch": task.base_branch, "worktree": task.worktree_path,
        }))?;
        Ok(task)
    }

    async fn checkpoint_task(&self, task: &Task, employee: &Employee, msg: &str) {
        let eid = Some(employee.id.as_str());
        match self.ws.checkpoint(Path::new(&task.worktree_path), &employee.name, msg).await {
            Ok(Some(cp)) => {
                let _ = self.db.append_event("checkpoint_committed", eid, None, &json!({
                    "task_id": task.id, "commit": cp.commit, "files": cp.files,
                    "insertions": cp.insertions, "deletions": cp.deletions,
                }));
            }
            Ok(None) => {}
            Err(e) => {
                let _ = self.db.append_event("checkpoint_failed", eid, None,
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
        let (task, emp) = self.task_and_employee(task_id)?;
        if task.status != "active" {
            bail!("任务已经是 {} 状态，不能合并", task.status);
        }
        let eid = Some(emp.id.as_str());
        self.checkpoint_task(&task, &emp, &format!("[crewforge] {}: {} (合并前保存)\n\nCrewforge-Task: {}", emp.name, task.title, task.id)).await;

        let message = format!(
            "{}\n\nCrewforge-Task: {}\nCrewforge-Employee: {}\nCrewforge-Branch: {}",
            task.title, task.id, emp.name, task.branch
        );
        let repo = Path::new(&task.repo_path);
        let outcome = self.ws.merge(repo, &task.base_branch, &task.branch, &emp.name, &message).await?;
        match &outcome {
            MergeOutcome::Merged { commit } => {
                self.db.set_task_status(&task.id, "merged", Some(commit))?;
                self.db.append_event("task_merged", eid, None, &json!({"task_id": task.id, "commit": commit, "title": task.title}))?;
            }
            MergeOutcome::Empty => {
                self.db.set_task_status(&task.id, "merged", None)?;
                self.db.append_event("task_merged", eid, None, &json!({"task_id": task.id, "commit": null, "title": task.title, "empty": true}))?;
            }
            MergeOutcome::Conflict { files } => {
                self.db.append_event("task_merge_blocked", eid, None, &json!({"task_id": task.id, "kind": "conflict", "files": files}))?;
            }
            MergeOutcome::Blocked { reason } => {
                self.db.append_event("task_merge_blocked", eid, None, &json!({"task_id": task.id, "kind": "blocked", "reason": reason}))?;
            }
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
        self.db.append_event("task_abandoned", Some(&emp.id), None,
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
            views.push(TaskView { task, employee_name, files, merge, overlaps: vec![] });
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
