//! 结束项目：已完成、归档、不做了（彻底删除）。
//!
//! - 已完成：要求所有任务都已合并或放弃；标记项目结束，什么都不删。
//! - 归档：标记项目结束；进行中的任务会被放弃（改动保存为 WIP、保留分支和工作区），目录一律不动。可以恢复。
//! - 不做了：彻底删除——员工在这个项目里的 worktree 和分支、项目的全部记录；可选连项目文件夹一起移到废纸篓。
//!   用量记录（`turn_result`）保留，因为那是真实花掉的额度。

use crate::db::{Db, Project, now_ms};
use crate::orchestrator::Orchestrator;
use anyhow::{Context, Result, bail};
use rusqlite::params;
use serde::Serialize;
use serde_json::json;
use std::path::{Path, PathBuf};

pub const MIGRATION_16: &str = "ALTER TABLE projects ADD COLUMN end_kind TEXT NOT NULL DEFAULT '';
ALTER TABLE projects ADD COLUMN end_reason TEXT NOT NULL DEFAULT '';
ALTER TABLE projects ADD COLUMN ended_at INTEGER;";

pub const END_KINDS: &[(&str, &str)] = &[("completed", "已完成"), ("archived", "归档")];

#[derive(Debug, Clone, Serialize)]
pub struct EndedProject {
    pub project: Project,
    /// completed | archived（早期归档的项目没有记录，按 archived 显示）
    pub end_kind: String,
    pub end_reason: String,
    pub ended_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiscardReport {
    pub tasks_removed: usize,
    pub folder_trashed: Option<String>,
}

impl Db {
    pub fn mark_project_ended(&self, id: &str, kind: &str, reason: &str) -> Result<()> {
        if !END_KINDS.iter().any(|(k, _)| *k == kind) {
            bail!("未知的结束方式：{kind}");
        }
        let now = now_ms();
        self.conn.lock().unwrap().execute(
            "UPDATE projects SET status = 'archived', end_kind = ?2, end_reason = ?3, ended_at = ?4, updated_at = ?4 WHERE id = ?1",
            params![id, kind, reason.trim(), now],
        )?;
        Ok(())
    }

    pub fn list_ended_projects(&self) -> Result<Vec<EndedProject>> {
        let ids: Vec<(String, String, String, Option<i64>)> = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare("SELECT id, end_kind, end_reason, ended_at FROM projects WHERE status = 'archived' ORDER BY COALESCE(ended_at, updated_at) DESC")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        let mut out = Vec::new();
        for (id, kind, reason, at) in ids {
            if let Some(project) = self.get_project(&id)? {
                let ended_at = at.or(Some(project.updated_at));
                out.push(EndedProject { project, end_kind: if kind.is_empty() { "archived".into() } else { kind }, end_reason: reason, ended_at });
            }
        }
        Ok(out)
    }

    /// 恢复已结束的项目（回到进行中）。被放弃的任务不会自动恢复。
    pub fn restore_project(&self, id: &str) -> Result<()> {
        let p = self.get_project(id)?.context("项目不存在")?;
        if p.status != "archived" {
            bail!("这个项目没有结束");
        }
        self.conn.lock().unwrap().execute(
            "UPDATE projects SET status = 'active', end_kind = '', end_reason = '', ended_at = NULL, updated_at = ?2 WHERE id = ?1",
            params![id, now_ms()],
        )?;
        self.append_event_for(Some(id), "project_restored", None, None, &json!({"text": format!("项目「{}」已恢复", p.name)}))?;
        Ok(())
    }

    /// 删除项目的全部记录（不碰磁盘）。用量记录 `turn_result` 保留。
    pub fn purge_project(&self, id: &str) -> Result<usize> {
        let p = self.get_project(id)?.context("项目不存在")?;
        let conn = self.conn.lock().unwrap();
        // 语句里要同时用到项目 id 和路径，而 rusqlite 要求参数个数完全一致，所以把两个值转义后直接放进语句
        let q = |v: &str| format!("'{}'", v.replace('\'', "''"));
        let (pid_q, path_q) = (q(id), q(&p.path));
        let tids = "SELECT id FROM tasks WHERE repo_path = ?2";
        let mids = "SELECT id FROM meetings WHERE project_id = ?1 OR task_id IN (SELECT id FROM tasks WHERE repo_path = ?2)";
        let iids = "SELECT id FROM issues WHERE project_id = ?1";
        let pids = "SELECT id FROM plan_items WHERE project_id = ?1";
        let removed: i64 = conn.query_row("SELECT COUNT(*) FROM tasks WHERE repo_path = ?1", [&p.path], |r| r.get(0))?;
        conn.execute_batch("BEGIN; PRAGMA defer_foreign_keys = ON;")?;
        let steps = [
            format!("DELETE FROM task_checks WHERE task_id IN ({tids}) OR baseline_id IN (SELECT id FROM baselines WHERE project_id = ?1)"),
            format!("DELETE FROM plan_deps WHERE item_id IN ({pids}) OR depends_on IN ({pids})"),
            format!("DELETE FROM plan_items WHERE project_id = ?1"),
            format!("DELETE FROM action_items WHERE project_id = ?1 OR task_id IN ({tids}) OR work_task_id IN ({tids})"),
            format!("DELETE FROM meeting_messages WHERE meeting_id IN ({mids})"),
            format!("DELETE FROM meeting_participants WHERE meeting_id IN ({mids})"),
            format!("DELETE FROM meeting_guests WHERE meeting_id IN ({mids})"),
            format!("DELETE FROM meeting_issues WHERE meeting_id IN ({mids}) OR issue_id IN ({iids})"),
            format!("DELETE FROM meetings WHERE id IN ({mids})"),
            format!("DELETE FROM issue_options WHERE issue_id IN ({iids})"),
            format!("DELETE FROM issues WHERE project_id = ?1"),
            format!("DELETE FROM memory_uses WHERE memory_id IN (SELECT id FROM memories WHERE project_id = ?1 OR task_id IN ({tids}))"),
            format!("DELETE FROM memories WHERE project_id = ?1 OR task_id IN ({tids})"),
            "DELETE FROM onboarding_reports WHERE project_id = ?1".to_string(),
            "DELETE FROM project_scans WHERE project_id = ?1".to_string(),
            "DELETE FROM baselines WHERE project_id = ?1".to_string(),
            "DELETE FROM autopilot_runs WHERE project_id = ?1".to_string(),
            "DELETE FROM project_members WHERE project_id = ?1".to_string(),
            "DELETE FROM sessions WHERE project_path = ?2 OR project_path IN (SELECT worktree_path FROM tasks WHERE repo_path = ?2)".to_string(),
            "DELETE FROM events WHERE project_id = ?1 AND kind != 'turn_result'".to_string(),
            "DELETE FROM tasks WHERE repo_path = ?2".to_string(),
            "DELETE FROM projects WHERE id = ?1".to_string(),
        ];
        for sql in &steps {
            let sql = sql.replace("?1", &pid_q).replace("?2", &path_q);
            if let Err(e) = conn.execute(&sql, []) {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(anyhow::Error::from(e).context(format!("删除失败（已回滚）：{sql}")));
            }
        }
        if let Err(e) = conn.execute_batch("COMMIT") {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(e.into());
        }
        Ok(removed as usize)
    }
}

/// 项目文件夹能不能被整个移走：只允许 Crewforge 新建的项目，且目录是干净的 Git 仓库。
pub async fn check_folder_deletable(p: &Project, data_dir: &Path) -> Result<PathBuf> {
    if p.onboarding != "none" {
        bail!("这是导入的已有项目，不能连文件夹一起删；请取消勾选“同时把项目文件夹移到废纸篓”，只删除 Crewforge 里的内容");
    }
    let dir = Path::new(&p.path).canonicalize().with_context(|| format!("项目目录不存在：{}", p.path))?;
    let home = std::env::var_os("HOME").map(PathBuf::from).and_then(|h| h.canonicalize().ok());
    if dir.components().count() < 4 || home.as_ref().is_some_and(|h| h == &dir || h.starts_with(&dir)) {
        bail!("目录太靠近根目录或家目录，拒绝删除：{}", dir.display());
    }
    if dir.starts_with(data_dir) || data_dir.starts_with(&dir) {
        bail!("项目目录和 Crewforge 的数据目录有包含关系，拒绝删除");
    }
    if !dir.join(".git").exists() {
        bail!("目录里没有 .git，拒绝删除：{}", dir.display());
    }
    let dirty = crate::workspace::git_output(&dir, &["status", "--porcelain"]).await?;
    if !dirty.trim().is_empty() {
        bail!("项目文件夹里有未提交的改动，拒绝删除（避免丢掉还没保存的东西）：\n{}", dirty.lines().take(5).collect::<Vec<_>>().join("\n"));
    }
    Ok(dir)
}

/// 移到废纸篓（不是永久删除）。
pub fn move_to_trash(dir: &Path) -> Result<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).context("找不到家目录")?;
    let trash = home.join(".Trash");
    std::fs::create_dir_all(&trash)?;
    let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "project".into());
    let dest = trash.join(format!("{name}-{}", now_ms()));
    std::fs::rename(dir, &dest).with_context(|| format!("无法移到废纸篓（可能在另一块磁盘上），请手动删除：{}", dir.display()))?;
    Ok(dest)
}

impl Orchestrator {
    /// 已完成 / 归档。
    pub async fn end_project(&self, id: &str, kind: &str, reason: &str) -> Result<()> {
        let p = self.db.get_project(id)?.context("项目不存在")?;
        if p.status != "active" {
            bail!("这个项目已经结束了");
        }
        let active: Vec<_> = self.db.list_tasks(&p.path)?.into_iter().filter(|t| t.status == "active").collect();
        match kind {
            "completed" => {
                if !active.is_empty() {
                    bail!("还有 {} 个进行中的任务。“已完成”要求任务都已合并或放弃；请先处理，或改选“归档”（会自动放弃它们）", active.len());
                }
            }
            "archived" => {
                for t in &active {
                    self.abandon_task(&t.id).await?;
                }
            }
            _ => bail!("未知的结束方式：{kind}"),
        }
        self.db.mark_project_ended(id, kind, reason)?;
        let label = END_KINDS.iter().find(|(k, _)| *k == kind).map(|(_, l)| *l).unwrap_or("结束");
        self.db.append_event(&format!("project_{kind}"), None, None, &json!({"project_id": id, "text": format!("项目「{}」{label}{}", p.name, if reason.trim().is_empty() { String::new() } else { format!("：{}", reason.trim()) })}))?;
        Ok(())
    }

    /// 不做了：彻底删除。先全部校验，再动手。
    pub async fn discard_project(&self, id: &str, confirm_name: &str, delete_folder: bool) -> Result<DiscardReport> {
        let p = self.db.get_project(id)?.context("项目不存在")?;
        if confirm_name.trim() != p.name {
            bail!("请输入完整的项目名称来确认删除");
        }
        let data_dir = self.ws.data_dir().canonicalize().unwrap_or_else(|_| self.ws.data_dir());
        let folder = if delete_folder { Some(check_folder_deletable(&p, &data_dir).await?) } else { None };

        let repo = PathBuf::from(&p.path);
        let tasks = self.db.list_tasks(&p.path)?;
        let wt_root = data_dir.join("worktrees");
        for t in &tasks {
            let wt = PathBuf::from(&t.worktree_path);
            // 只清理 Crewforge 自己建的 worktree，绝不碰数据目录以外的路径
            if wt.starts_with(&wt_root) && repo.exists() {
                self.ws.remove_checkout(&repo, &wt).await;
            }
            if repo.exists() && folder.is_none() {
                self.ws.delete_branch_best_effort(&repo, &t.branch).await;
            }
        }
        let group = self.ws.worktree_path(&repo, "x");
        if let Some(parent) = group.parent() {
            if parent.starts_with(&wt_root) && parent != wt_root {
                let _ = std::fs::remove_dir_all(parent);
            }
        }
        let tasks_removed = self.db.purge_project(id)?;
        let folder_trashed = match folder {
            Some(dir) => Some(move_to_trash(&dir)?.to_string_lossy().into_owned()),
            None => None,
        };
        self.db.append_event("project_discarded", None, None, &json!({"text": format!("项目「{}」已彻底删除{}", p.name, if folder_trashed.is_some() { "（文件夹已移到废纸篓）" } else { "" })}))?;
        Ok(DiscardReport { tasks_removed, folder_trashed })
    }
}
