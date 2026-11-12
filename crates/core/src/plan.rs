//! 计划（待办清单）：把一个大目标（比如“技术栈升级、业务行为不变”）拆成有顺序、有依赖的小步骤，
//! 逐条派给员工，跟踪进度。
//!
//! - 条目可以由人类添加，也可以由 AI 负责人通过 `propose_plan` 起草；AI 起草的条目先是“待确认”，人类批准后才进入计划；
//! - 有依赖的条目要等前置条目完成（或取消）才能开始；
//! - 派活 = 为条目新开一个任务（带目标、验收标准）；任务合并后条目自动完成，放弃则回到“待开始”；
//! - 每名员工在一个仓库里同一时刻只做一个任务。

use crate::db::{Db, Employee, now_ms};
use crate::orchestrator::{Orchestrator, TaskLink};
use crate::runner::{Approver, Observer};
use anyhow::{Context, Result, bail};
use rusqlite::params;
use serde::Serialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// 条目预计会改的文件（换行分隔），自主推进据此避免让改同一个文件的条目并行。
/// 项目“目标与约束”的字数上限（每个任务开始时都会带上，太长会挤占上下文）
pub const GOAL_MAX_CHARS: usize = 6000;

pub const MIGRATION_15: &str = "ALTER TABLE plan_items ADD COLUMN files TEXT NOT NULL DEFAULT '';";

/// AI 负责人提议修改项目目标：只是提议，人点头才生效。
pub const MIGRATION_17: &str = "CREATE TABLE goal_proposals (
    id          TEXT PRIMARY KEY,
    project_id  TEXT NOT NULL REFERENCES projects(id),
    proposed_by TEXT REFERENCES employees(id),
    new_goal    TEXT NOT NULL,
    reason      TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'pending',  -- pending | approved | rejected | superseded
    note        TEXT NOT NULL DEFAULT '',
    created_at  INTEGER NOT NULL,
    decided_at  INTEGER
);
CREATE INDEX idx_goal_proposals_project ON goal_proposals(project_id, status);";

#[derive(Debug, Clone, serde::Serialize)]
pub struct GoalProposal {
    pub id: String, pub project_id: String, pub proposed_by: Option<String>, pub proposer_name: Option<String>,
    pub old_goal: String, pub new_goal: String, pub reason: String, pub status: String, pub note: String, pub created_at: i64,
}

pub const MIGRATION_9: &str = r#"
ALTER TABLE projects ADD COLUMN goal TEXT NOT NULL DEFAULT '';   -- 重构目标与硬性约束，每个新任务开始时都会带上
CREATE TABLE plan_items (
    id           TEXT PRIMARY KEY,
    project_id   TEXT NOT NULL REFERENCES projects(id),
    title        TEXT NOT NULL,
    description  TEXT NOT NULL DEFAULT '',
    acceptance   TEXT NOT NULL DEFAULT '',                       -- 验收标准
    kind         TEXT NOT NULL DEFAULT 'migrate',                -- characterize | migrate | cleanup | other
    module       TEXT NOT NULL DEFAULT '',
    position     INTEGER NOT NULL,
    assignee_id  TEXT REFERENCES employees(id),
    status       TEXT NOT NULL DEFAULT 'planned',                -- proposed | planned | in_progress | done | blocked | cancelled
    task_id      TEXT REFERENCES tasks(id),
    proposed_by  TEXT,                                           -- 员工 id；NULL 表示人类
    note         TEXT NOT NULL DEFAULT '',
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
);
CREATE INDEX idx_plan_project ON plan_items(project_id, position);
CREATE INDEX idx_plan_task ON plan_items(task_id);
CREATE TABLE plan_deps (
    item_id    TEXT NOT NULL REFERENCES plan_items(id) ON DELETE CASCADE,
    depends_on TEXT NOT NULL REFERENCES plan_items(id) ON DELETE CASCADE,
    PRIMARY KEY (item_id, depends_on)
);
"#;

pub const PLAN_KINDS: &[&str] = &["characterize", "migrate", "cleanup", "other", "design"];
const MAX_ITEMS_PER_PROJECT: i64 = 300;

#[derive(Debug, Clone, Default, Serialize)]
pub struct PlanItem {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub description: String,
    pub acceptance: String,
    pub kind: String,
    pub module: String,
    pub position: i64,
    pub assignee_id: Option<String>,
    pub assignee_name: Option<String>,
    pub status: String,
    pub task_id: Option<String>,
    pub proposed_by: Option<String>,
    pub proposed_by_name: Option<String>,
    pub note: String,
    pub deps: Vec<String>,
    /// 还没完成的前置条目标题
    pub waiting_for: Vec<String>,
    /// 可以开始：待开始、前置都已完成
    pub ready: bool,
    /// 等级 1–5，0 = 未定级（见 `levels`）
    pub level: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PlanCounts {
    pub proposed: usize,
    pub planned: usize,
    pub in_progress: usize,
    pub done: usize,
    pub blocked: usize,
    pub cancelled: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanView {
    pub items: Vec<PlanItem>,
    pub counts: PlanCounts,
    /// 已完成 /（不含已取消和待确认的全部条目）
    pub percent_done: u32,
}

#[derive(Debug, Clone, Default)]
pub struct NewPlanItem {
    pub title: String,
    pub description: String,
    pub acceptance: String,
    pub kind: String,
    pub module: String,
    pub assignee_id: Option<String>,
    pub deps: Vec<String>,
}

fn validate(n: &NewPlanItem) -> Result<String> {
    if n.title.trim().is_empty() {
        bail!("条目标题不能为空");
    }
    let kind = if n.kind.is_empty() { "migrate" } else { n.kind.as_str() };
    if !PLAN_KINDS.contains(&kind) {
        bail!("未知的条目类型: {kind}，可选：{}", PLAN_KINDS.join("、"));
    }
    Ok(kind.to_string())
}

impl Db {
    pub fn create_plan_item(&self, project_id: &str, n: &NewPlanItem, proposed_by: Option<&str>) -> Result<String> {
        let kind = validate(n)?;
        self.get_project(project_id)?.context("项目不存在")?;
        let (count, max_pos): (i64, i64) = self.conn.lock().unwrap().query_row(
            "SELECT COUNT(*), COALESCE(MAX(position), 0) FROM plan_items WHERE project_id = ?1", [project_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        if count >= MAX_ITEMS_PER_PROJECT {
            bail!("计划条目已达上限 {MAX_ITEMS_PER_PROJECT}");
        }
        if let Some(a) = &n.assignee_id {
            if !self.is_member(project_id, a)? {
                bail!("负责人必须是项目成员");
            }
        }
        for d in &n.deps {
            let dep = self.get_plan_item(d)?.context("依赖的条目不存在")?;
            if dep.project_id != project_id {
                bail!("依赖的条目不属于这个项目");
            }
        }
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_ms();
        let status = if proposed_by.is_some() { "proposed" } else { "planned" };
        self.conn.lock().unwrap().execute(
            "INSERT INTO plan_items (id, project_id, title, description, acceptance, kind, module, position, assignee_id, status, proposed_by, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?12)",
            params![id, project_id, n.title.trim(), n.description, n.acceptance, kind, n.module, max_pos + 1, n.assignee_id, status, proposed_by, now],
        )?;
        for d in &n.deps {
            self.conn.lock().unwrap().execute("INSERT OR IGNORE INTO plan_deps (item_id, depends_on) VALUES (?1,?2)", params![id, d])?;
        }
        Ok(id)
    }

    /// 记录条目预计会改的文件（相对仓库根目录的路径）。
    pub fn set_plan_item_files(&self, id: &str, files: &[String]) -> Result<()> {
        self.conn.lock().unwrap().execute("UPDATE plan_items SET files = ?2 WHERE id = ?1", params![id, files.join("\n")])?;
        Ok(())
    }

    /// 项目里各条目预计会改的文件；没写的条目不在结果里。
    pub fn plan_files(&self, project_id: &str) -> Result<std::collections::HashMap<String, Vec<String>>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id, files FROM plan_items WHERE project_id = ?1 AND files != ''")?;
        let rows = stmt.query_map([project_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut m = std::collections::HashMap::new();
        for r in rows {
            let (id, f) = r?;
            m.insert(id, f.lines().map(String::from).collect());
        }
        Ok(m)
    }

    pub fn get_plan_item(&self, id: &str) -> Result<Option<PlanItem>> {
        let Some(project_id) = self.project_of_plan_item(id)? else { return Ok(None) };
        Ok(self.plan_view(&project_id)?.items.into_iter().find(|i| i.id == id))
    }

    fn project_of_plan_item(&self, id: &str) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        Ok(self.conn.lock().unwrap().query_row("SELECT project_id FROM plan_items WHERE id = ?1", [id], |r| r.get(0)).optional()?)
    }

    pub fn plan_view(&self, project_id: &str) -> Result<PlanView> {
        let rows: Vec<(String, String, String, String, String, String, String, i64, Option<String>, String, Option<String>, Option<String>, String, i64)> = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT id, project_id, title, description, acceptance, kind, module, position, assignee_id, status, task_id, proposed_by, note, level
                 FROM plan_items WHERE project_id = ?1 ORDER BY position, rowid")?;
            stmt.query_map([project_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?, r.get(12)?, r.get(13)?)))?
                .collect::<std::result::Result<_, _>>()?
        };
        let deps: Vec<(String, String)> = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare("SELECT d.item_id, d.depends_on FROM plan_deps d JOIN plan_items p ON p.id = d.item_id WHERE p.project_id = ?1")?;
            stmt.query_map([project_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?
        };
        let status_of: HashMap<&str, &str> = rows.iter().map(|r| (r.0.as_str(), r.9.as_str())).collect();
        let title_of: HashMap<&str, &str> = rows.iter().map(|r| (r.0.as_str(), r.2.as_str())).collect();
        let name = |id: &Option<String>| -> Result<Option<String>> { Ok(match id { Some(i) => self.employee_by_id(i)?.map(|e| e.name), None => None }) };
        let mut items = Vec::new();
        let mut counts = PlanCounts::default();
        for r in &rows {
            let my_deps: Vec<String> = deps.iter().filter(|d| d.0 == r.0).map(|d| d.1.clone()).collect();
            let waiting_for: Vec<String> = my_deps.iter()
                .filter(|d| !matches!(status_of.get(d.as_str()), Some(&"done") | Some(&"cancelled") | None))
                .map(|d| title_of.get(d.as_str()).copied().unwrap_or("?").to_string()).collect();
            match r.9.as_str() {
                "proposed" => counts.proposed += 1, "planned" => counts.planned += 1, "in_progress" => counts.in_progress += 1,
                "done" => counts.done += 1, "blocked" => counts.blocked += 1, _ => counts.cancelled += 1,
            }
            items.push(PlanItem {
                id: r.0.clone(), project_id: r.1.clone(), title: r.2.clone(), description: r.3.clone(), acceptance: r.4.clone(), kind: r.5.clone(),
                module: r.6.clone(), position: r.7, assignee_name: name(&r.8)?, assignee_id: r.8.clone(), status: r.9.clone(), task_id: r.10.clone(),
                proposed_by_name: name(&r.11)?, proposed_by: r.11.clone(), note: r.12.clone(), ready: r.9 == "planned" && waiting_for.is_empty(), deps: my_deps, waiting_for, level: r.13,
            });
        }
        let total = items.len() - counts.cancelled - counts.proposed;
        Ok(PlanView { percent_done: if total == 0 { 0 } else { (counts.done * 100 / total) as u32 }, items, counts })
    }

    /// 把“进行中”的条目放回“待开始”（保留关联的任务，员工接着做）。
    pub fn requeue_plan_item(&self, id: &str) -> Result<bool> {
        let n = self.conn.lock().unwrap().execute("UPDATE plan_items SET status = 'planned', note = '上次做到一半被中断，已回到待开始', updated_at = ?2 WHERE id = ?1 AND status = 'in_progress'", params![id, now_ms()])?;
        Ok(n > 0)
    }

    /// 每个还没做的条目此刻在等什么（给计划页显示）。键是条目 id，值是原因，空表示没有被拦着。
    /// `busy` 是此刻正在工作的员工名；`autopilot_live` 表示这个项目的自主推进是否在运行。
    pub fn plan_waiting(&self, project_id: &str, busy: &std::collections::HashSet<String>, autopilot_live: bool) -> Result<std::collections::HashMap<String, Vec<String>>> {
        let view = self.plan_view(project_id)?;
        let design_open = matches!(self.design_phase(project_id)?, crate::design::DesignPhase::Pending(_));
        let files = self.plan_files(project_id)?;
        let members: std::collections::HashSet<String> = self.list_members(project_id)?.into_iter().map(|m| m.employee_id).collect();
        // 员工正在做哪个条目：用于说明“等 TA 做完手上的”
        let doing: std::collections::HashMap<&str, &PlanItem> = view.items.iter().filter(|i| i.status == "in_progress").filter_map(|i| i.assignee_id.as_deref().map(|a| (a, i))).collect();
        let running_files: Vec<(&PlanItem, &Vec<String>)> = view.items.iter().filter(|i| i.status == "in_progress").filter_map(|i| files.get(&i.id).map(|f| (i, f))).collect();
        let mut out = std::collections::HashMap::new();
        for it in &view.items {
            let mut why: Vec<String> = Vec::new();
            match it.status.as_str() {
                "proposed" => why.push("待确认：你批准计划（或开始自主推进）之后才会开始".into()),
                "blocked" => why.push(if it.note.is_empty() { "已阻塞".into() } else { format!("已阻塞：{}", it.note) }),
                "planned" => {
                    if design_open && it.kind != crate::design::DESIGN_KIND {
                        why.push("等架构师完成设计：设计条目都做完，其他条目才会开始".into());
                    }
                    if !it.waiting_for.is_empty() {
                        why.push(format!("等前置完成：{}", it.waiting_for.join("、")));
                    }
                    if let (Some(a), Some(name)) = (&it.assignee_id, &it.assignee_name) {
                        if !members.contains(a) {
                            why.push(format!("指派给 {name}，但 TA 已不在本项目：自主推进会当作未指定、按角色重新分配；手动派发请先改派"));
                        } else if let Some(d) = doing.get(a.as_str()) {
                            why.push(format!("{name} 正在做《{}》，做完才轮到", d.title));
                        } else if busy.contains(name) {
                            why.push(format!("{name} 正在忙别的"));
                        }
                    }
                    if let Some(mine) = files.get(&it.id) {
                        if let Some((other, _)) = running_files.iter().find(|(o, f)| o.id != it.id && f.iter().any(|x| mine.contains(x))) {
                            why.push(format!("预计和进行中的《{}》改同一批文件，错开做以免合并冲突", other.title));
                        }
                    }
                    if why.is_empty() {
                        why.push(if autopilot_live { "可以开始：马上会被自主推进派出（受并发上限和员工空闲情况影响）".into() } else { "可以开始：自主推进没在运行，需要你手动“派发”，或开始自主推进".into() });
                    }
                }
                _ => {}
            }
            out.insert(it.id.clone(), why);
        }
        Ok(out)
    }

    pub fn update_plan_item(&self, id: &str, n: &NewPlanItem) -> Result<()> {
        let kind = validate(n)?;
        let it = self.get_plan_item(id)?.context("条目不存在")?;
        if matches!(it.status.as_str(), "done" | "cancelled") {
            bail!("已完成或已取消的条目不能修改");
        }
        if let Some(a) = &n.assignee_id {
            if !self.is_member(&it.project_id, a)? {
                bail!("负责人必须是项目成员");
            }
        }
        self.conn.lock().unwrap().execute(
            "UPDATE plan_items SET title = ?2, description = ?3, acceptance = ?4, kind = ?5, module = ?6, assignee_id = ?7, updated_at = ?8 WHERE id = ?1",
            params![id, n.title.trim(), n.description, n.acceptance, kind, n.module, n.assignee_id, now_ms()],
        )?;
        Ok(())
    }

    /// 重新设置依赖；检查不会形成环。
    pub fn set_plan_deps(&self, id: &str, deps: &[String]) -> Result<()> {
        let it = self.get_plan_item(id)?.context("条目不存在")?;
        let view = self.plan_view(&it.project_id)?;
        let mut edges: HashMap<String, Vec<String>> = view.items.iter().map(|i| (i.id.clone(), i.deps.clone())).collect();
        for d in deps {
            if !view.items.iter().any(|i| &i.id == d) {
                bail!("依赖的条目不存在或不属于这个项目");
            }
            if d == id {
                bail!("条目不能依赖它自己");
            }
        }
        edges.insert(id.to_string(), deps.to_vec());
        // 从 id 出发沿依赖走，如果走回 id 就是环
        let mut stack: Vec<&String> = deps.iter().collect();
        let mut seen: HashSet<&String> = HashSet::new();
        while let Some(cur) = stack.pop() {
            if cur == id {
                bail!("这样设置会形成循环依赖");
            }
            if seen.insert(cur) {
                if let Some(next) = edges.get(cur) {
                    stack.extend(next.iter());
                }
            }
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM plan_deps WHERE item_id = ?1", [id])?;
        for d in deps {
            tx.execute("INSERT OR IGNORE INTO plan_deps (item_id, depends_on) VALUES (?1,?2)", params![id, d])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// 上移（-1）或下移（+1）一位：与相邻条目交换顺序。
    pub fn move_plan_item(&self, id: &str, delta: i64) -> Result<()> {
        let it = self.get_plan_item(id)?.context("条目不存在")?;
        let items = self.plan_view(&it.project_id)?.items;
        let idx = items.iter().position(|i| i.id == id).context("条目不存在")?;
        let other = idx as i64 + delta.signum();
        if other < 0 || other as usize >= items.len() {
            return Ok(());
        }
        let o = &items[other as usize];
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE plan_items SET position = ?2 WHERE id = ?1", params![it.id, o.position])?;
        conn.execute("UPDATE plan_items SET position = ?2 WHERE id = ?1", params![o.id, it.position])?;
        // 位置相同（理论上不会）时，保证两者确实交换
        if it.position == o.position {
            conn.execute("UPDATE plan_items SET position = position + 1 WHERE id = ?1", [&it.id])?;
        }
        Ok(())
    }

    /// 人类改变条目状态：批准（proposed→planned）、驳回/取消（→cancelled）、阻塞/解除、手动标记完成/重新打开。
    pub fn set_plan_status(&self, id: &str, to: &str, note: &str) -> Result<()> {
        let it = self.get_plan_item(id)?.context("条目不存在")?;
        let ok = matches!(
            (it.status.as_str(), to),
            ("proposed", "planned") | ("proposed", "cancelled")
                | ("planned", "blocked") | ("planned", "cancelled") | ("planned", "done")
                | ("blocked", "planned") | ("blocked", "cancelled")
                | ("in_progress", "planned") | ("in_progress", "done") | ("in_progress", "cancelled")
                | ("done", "planned") | ("cancelled", "planned")
        );
        if !ok {
            bail!("条目不能从「{}」转为「{}」", status_label(&it.status), status_label(to));
        }
        if to == "cancelled" && it.status == "in_progress" {
            if let Some(t) = &it.task_id {
                if self.get_task(t)?.is_some_and(|t| t.status == "active") {
                    bail!("条目的任务还在进行中，请先在任务面板合并或放弃它");
                }
            }
        }
        let clear_task = matches!(to, "planned");
        self.conn.lock().unwrap().execute(
            "UPDATE plan_items SET status = ?2, note = CASE WHEN ?3 = '' THEN note ELSE ?3 END,
                    task_id = CASE WHEN ?4 = 1 THEN NULL ELSE task_id END, updated_at = ?5 WHERE id = ?1",
            params![id, to, note.trim(), clear_task as i64, now_ms()],
        )?;
        self.append_event_for(Some(&it.project_id), "plan_item_status", None, None, &json!({"item_id": id, "title": it.title, "from": it.status, "to": to}))?;
        Ok(())
    }

    /// 只改备注（负责人追加条目时写明原因）。
    pub fn set_plan_note(&self, id: &str, note: &str) -> Result<()> {
        self.conn.lock().unwrap().execute("UPDATE plan_items SET note = ?2, updated_at = ?3 WHERE id = ?1", params![id, note.trim(), now_ms()])?;
        Ok(())
    }

    pub fn mark_plan_started(&self, id: &str, task_id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE plan_items SET status = 'in_progress', task_id = ?2, updated_at = ?3 WHERE id = ?1 AND status IN ('planned','in_progress')", params![id, task_id, now_ms()])?;
        Ok(())
    }

    /// 任务合并：对应的计划条目自动完成。
    pub fn complete_plan_for_task(&self, task_id: &str) -> Result<()> {
        let rows: Vec<(String, String, String)> = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare("SELECT id, project_id, title FROM plan_items WHERE task_id = ?1 AND status = 'in_progress'")?;
            stmt.query_map([task_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<std::result::Result<_, _>>()?
        };
        for (id, pid, title) in rows {
            self.conn.lock().unwrap().execute("UPDATE plan_items SET status = 'done', updated_at = ?2 WHERE id = ?1", params![id, now_ms()])?;
            self.append_event_for(Some(&pid), "plan_item_status", None, None, &json!({"item_id": id, "title": title, "from": "in_progress", "to": "done"}))?;
        }
        Ok(())
    }

    /// 任务被放弃：对应的计划条目回到“待开始”，可以重新派。
    pub fn release_plan_for_task(&self, task_id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE plan_items SET status = 'planned', task_id = NULL, note = '上一次尝试被放弃，已回到待开始', updated_at = ?2 WHERE task_id = ?1 AND status = 'in_progress'",
            params![task_id, now_ms()])?;
        Ok(())
    }

    pub fn set_project_goal(&self, id: &str, goal: &str) -> Result<()> {
        if goal.chars().count() > GOAL_MAX_CHARS {
            bail!("目标与约束太长（上限 {GOAL_MAX_CHARS} 字）");
        }
        self.conn.lock().unwrap().execute("UPDATE projects SET goal = ?2, updated_at = ?3 WHERE id = ?1", params![id, goal.trim(), now_ms()])?;
        Ok(())
    }

    /// 负责人提议修改项目目标。同一项目只保留一条待确认的提议（新的取代旧的）。
    pub fn propose_goal_change(&self, project_id: &str, employee_id: &str, new_goal: &str, reason: &str) -> Result<String> {
        let (new_goal, reason) = (new_goal.trim(), reason.trim());
        if new_goal.is_empty() { bail!("新的目标不能为空"); }
        if new_goal.chars().count() > GOAL_MAX_CHARS { bail!("目标与约束太长（上限 {GOAL_MAX_CHARS} 字）"); }
        if reason.is_empty() { bail!("请写明为什么要改目标"); }
        let cur: String = self.conn.lock().unwrap().query_row("SELECT goal FROM projects WHERE id = ?1", params![project_id], |r| r.get(0))?;
        if cur.trim() == new_goal { bail!("新目标和现有目标相同"); }
        let rejected: Option<String> = self.conn.lock().unwrap().query_row(
            "SELECT note FROM goal_proposals WHERE project_id = ?1 AND status = 'rejected' AND new_goal = ?2 LIMIT 1", params![project_id, new_goal], |r| r.get(0)).ok();
        if let Some(n) = rejected { bail!("这份目标已经被人驳回过（原因：{n}），不要重复提议"); }
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_ms();
        {
            let conn = self.conn.lock().unwrap();
            conn.execute("UPDATE goal_proposals SET status = 'superseded', decided_at = ?2 WHERE project_id = ?1 AND status = 'pending'", params![project_id, now])?;
            conn.execute("INSERT INTO goal_proposals (id, project_id, proposed_by, new_goal, reason, created_at) VALUES (?1,?2,?3,?4,?5,?6)",
                params![id, project_id, employee_id, new_goal, reason, now])?;
        }
        self.append_event_for(Some(project_id), "plan_goal_proposed", Some(employee_id), None, &json!({"proposal_id": id, "reason": reason}))?;
        Ok(id)
    }

    pub fn pending_goal_proposal(&self, project_id: &str) -> Result<Option<GoalProposal>> {
        let conn = self.conn.lock().unwrap();
        let r = conn.query_row(
            "SELECT g.id, g.project_id, g.proposed_by, e.name, p.goal, g.new_goal, g.reason, g.status, g.note, g.created_at
             FROM goal_proposals g JOIN projects p ON p.id = g.project_id LEFT JOIN employees e ON e.id = g.proposed_by
             WHERE g.project_id = ?1 AND g.status = 'pending' ORDER BY g.created_at DESC LIMIT 1", params![project_id],
            |r| Ok(GoalProposal { id: r.get(0)?, project_id: r.get(1)?, proposed_by: r.get(2)?, proposer_name: r.get(3)?, old_goal: r.get(4)?, new_goal: r.get(5)?, reason: r.get(6)?, status: r.get(7)?, note: r.get(8)?, created_at: r.get(9)? }));
        match r { Ok(g) => Ok(Some(g)), Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None), Err(e) => Err(e.into()) }
    }

    /// 人决定：approve 才改目标（可以先改写 new_goal 再批准）；reject 必须写原因（负责人以后能看到）。
    pub fn decide_goal_proposal(&self, id: &str, approve: bool, edited_goal: Option<&str>, note: &str) -> Result<()> {
        let (pid, new_goal, status): (String, String, String) = self.conn.lock().unwrap().query_row(
            "SELECT project_id, new_goal, status FROM goal_proposals WHERE id = ?1", params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(|_| anyhow::anyhow!("提议不存在"))?;
        if status != "pending" { bail!("这条提议已经处理过了"); }
        if !approve && note.trim().is_empty() { bail!("驳回请写明原因，负责人会参考"); }
        if approve {
            self.set_project_goal(&pid, edited_goal.unwrap_or(&new_goal))?;
        }
        self.conn.lock().unwrap().execute("UPDATE goal_proposals SET status = ?2, note = ?3, decided_at = ?4 WHERE id = ?1",
            params![id, if approve { "approved" } else { "rejected" }, note.trim(), now_ms()])?;
        self.append_event_for(Some(&pid), "plan_goal_decided", None, None, &json!({"proposal_id": id, "approved": approve, "note": note.trim()}))?;
        Ok(())
    }

    /// 各项目等人确认的目标变更提议数（待办收件箱使用）。
    pub fn goal_proposal_counts(&self) -> Result<Vec<(String, String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT p.id, p.name, COUNT(*) FROM goal_proposals g JOIN projects p ON p.id = g.project_id
             WHERE g.status = 'pending' AND p.status = 'active' GROUP BY p.id, p.name ORDER BY p.name")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// 各项目等待人类确认的 AI 计划条目数（待办收件箱使用）。
    pub fn proposed_plan_counts(&self) -> Result<Vec<(String, String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT p.id, p.name, COUNT(*) FROM plan_items i JOIN projects p ON p.id = i.project_id
             WHERE i.status = 'proposed' AND p.status = 'active' GROUP BY p.id, p.name ORDER BY p.name")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// 批准一次 AI 起草的计划：把该项目所有“待确认”的条目转为“待开始”。返回条数。
    pub fn approve_proposed_plan(&self, project_id: &str) -> Result<usize> {
        let n = self.conn.lock().unwrap().execute("UPDATE plan_items SET status = 'planned', updated_at = ?2 WHERE project_id = ?1 AND status = 'proposed'", params![project_id, now_ms()])?;
        if n > 0 {
            self.append_event_for(Some(project_id), "plan_approved", None, None, &json!({"count": n}))?;
        }
        Ok(n)
    }
}

pub fn status_label(s: &str) -> &'static str {
    match s { "proposed" => "待确认", "planned" => "待开始", "in_progress" => "进行中", "done" => "已完成", "blocked" => "已阻塞", "cancelled" => "已取消", _ => "?" }
}

impl Orchestrator {
    /// 校验并准备派活：返回 (员工, 项目目录, 提示)。
    pub fn prepare_plan_dispatch(&self, item_id: &str, employee_override: Option<&str>) -> Result<(Employee, PathBuf, String)> {
        let item = self.db.get_plan_item(item_id)?.context("条目不存在")?;
        let project = self.db.get_project(&item.project_id)?.context("项目不存在")?;
        if project.onboarding == "in_progress" {
            bail!("项目还在导入阶段，确认导入之后才能派任务");
        }
        if item.status != "planned" {
            bail!("条目现在是「{}」，只有「待开始」的条目才能派活", status_label(&item.status));
        }
        if !item.waiting_for.is_empty() {
            bail!("还要等前置条目完成：{}", item.waiting_for.join("、"));
        }
        let emp_id = employee_override.map(String::from).or(item.assignee_id.clone()).context("这个条目没有指定负责人，请先选择由谁来做")?;
        let emp = self.db.employee_by_id(&emp_id)?.context("员工不存在")?;
        if emp.archived || !self.db.is_member(&project.id, &emp.id)? {
            bail!("{} 不是本项目的在职成员", emp.name);
        }
        // 员工手上进行中的任务如果正是这个条目自己的（被中断后放回待开始），就接着做；是别的任务才拒绝
        let resuming = match self.db.active_task(&emp.id, &project.path)? {
            Some(t) if item.task_id.as_deref() == Some(t.id.as_str()) => true,
            Some(t) => bail!("{} 还有进行中的任务《{}》，请先合并或放弃它，再开始新条目", emp.name, t.title),
            None => false,
        };
        // 第一行就是条目标题：它会成为任务标题和合并提交的标题
        let mut p = format!("{}\n[计划条目]\n", item.title);
        if !item.description.is_empty() {
            p.push_str(&format!("{}\n", item.description));
        }
        if !item.module.is_empty() {
            p.push_str(&format!("所属模块：{}\n", item.module));
        }
        if !item.acceptance.is_empty() {
            p.push_str(&format!("验收标准：{}\n", item.acceptance));
        }
        p.push_str("要求：先阅读相关代码和测试；只做这一个条目，不要顺手处理计划外的事；小步修改，改完运行测试和项目约定的检查确认通过，并简要说明做了什么、有什么没做。");
        if resuming {
            p.push_str("\n[系统提示] 这个条目上次做到一半被中断（例如应用重启）。你的分支和工作区还在：先用只读方式（git status、git log、读文件）核对已经完成的部分，再接着做，不要重做已完成的内容。");
        }
        Ok((emp, PathBuf::from(project.path), p))
    }

    pub async fn assign_for_plan<A: Approver, O: Observer>(
        &self, employee: &Employee, project_path: &std::path::Path, prompt: &str, item_id: &str, approver: &A, observer: &O,
    ) -> Result<crate::provider::claude::TurnResult> {
        self.assign_linked(employee, project_path, prompt, Some(TaskLink::Plan(item_id)), approver, observer).await
    }

    pub async fn dispatch_plan_item<A: Approver, O: Observer>(
        &self, item_id: &str, employee_override: Option<&str>, approver: &A, observer: &O,
    ) -> Result<crate::provider::claude::TurnResult> {
        let (emp, path, prompt) = self.prepare_plan_dispatch(item_id, employee_override)?;
        self.assign_for_plan(&emp, &path, &prompt, item_id, approver, observer).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (Db, String, String, String) {
        let db = Db::open_memory().unwrap();
        let alice = db.add_employee("alice", "全栈", "p").unwrap();
        let bob = db.add_employee("bob", "全栈", "p").unwrap();
        let p = db.create_project("P", "", "/r").unwrap();
        db.add_member(&p.id, &alice.id, "lead").unwrap();
        db.add_member(&p.id, &bob.id, "developer").unwrap();
        (db, p.id, alice.id, bob.id)
    }
    fn item(title: &str, deps: &[&str]) -> NewPlanItem {
        NewPlanItem { title: title.into(), description: "d".into(), acceptance: "a".into(), deps: deps.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }

    #[test]
    fn dependencies_decide_what_is_ready_and_progress_is_counted() {
        let (db, pid, alice, _b) = setup();
        let a = db.create_plan_item(&pid, &item("引擎", &[]), None).unwrap();
        let b = db.create_plan_item(&pid, &item("界面", &[&a]), None).unwrap();
        let c = db.create_plan_item(&pid, &NewPlanItem { assignee_id: Some(alice.clone()), ..item("测试", &[&a, &b]) }, None).unwrap();
        let v = db.plan_view(&pid).unwrap();
        let by = |id: &str| v.items.iter().find(|i| i.id == id).unwrap();
        assert!(by(&a).ready && !by(&b).ready && !by(&c).ready);
        assert_eq!(by(&b).waiting_for, vec!["引擎"]);
        assert_eq!(by(&c).waiting_for.len(), 2);
        assert_eq!((by(&c).assignee_name.as_deref(), v.percent_done), (Some("alice"), 0));
        // 完成前置之后，依赖它的条目才可以开始；取消的前置也不再拦着
        db.set_plan_status(&a, "done", "").unwrap();
        let v = db.plan_view(&pid).unwrap();
        assert!(v.items.iter().find(|i| i.id == b).unwrap().ready);
        assert_eq!(v.percent_done, 33);
        db.set_plan_status(&b, "cancelled", "").unwrap();
        let v = db.plan_view(&pid).unwrap();
        assert!(v.items.iter().find(|i| i.id == c).unwrap().ready, "被取消的前置不再拦着");
        assert_eq!((v.counts.done, v.counts.cancelled, v.counts.planned, v.percent_done), (1, 1, 1, 50), "取消的不计入分母");
    }

    #[test]
    fn validation_cycles_and_membership() {
        let (db, pid, _a, _b) = setup();
        let outsider = db.add_employee("zed", "全栈", "p").unwrap();
        assert!(db.create_plan_item(&pid, &item("  ", &[]), None).is_err());
        assert!(db.create_plan_item(&pid, &NewPlanItem { kind: "weird".into(), ..item("x", &[]) }, None).is_err());
        assert!(db.create_plan_item(&pid, &NewPlanItem { assignee_id: Some(outsider.id.clone()), ..item("x", &[]) }, None).is_err(), "负责人必须是成员");
        assert!(db.create_plan_item(&pid, &item("x", &["nope"]), None).is_err());
        let p2 = db.create_project("Q", "", "/q").unwrap();
        let other = db.create_plan_item(&p2.id, &item("别的项目", &[]), None).unwrap();
        assert!(db.create_plan_item(&pid, &item("x", &[&other]), None).is_err(), "不能依赖别的项目的条目");

        let a = db.create_plan_item(&pid, &item("A", &[]), None).unwrap();
        let b = db.create_plan_item(&pid, &item("B", &[&a]), None).unwrap();
        let c = db.create_plan_item(&pid, &item("C", &[&b]), None).unwrap();
        assert!(db.set_plan_deps(&a, &[c.clone()]).unwrap_err().to_string().contains("循环"), "A→C→B→A 是环");
        assert!(db.set_plan_deps(&a, &[a.clone()]).is_err(), "不能依赖自己");
        db.set_plan_deps(&c, &[a.clone()]).unwrap();
        assert_eq!(db.get_plan_item(&c).unwrap().unwrap().deps, vec![a]);
    }

    #[test]
    fn proposed_items_need_approval_and_status_rules_hold() {
        let (db, pid, alice, _b) = setup();
        let ai = db.create_plan_item(&pid, &item("AI 起草", &[]), Some(&alice)).unwrap();
        let human = db.create_plan_item(&pid, &item("人写的", &[]), None).unwrap();
        assert_eq!(db.get_plan_item(&ai).unwrap().unwrap().status, "proposed");
        assert_eq!(db.get_plan_item(&human).unwrap().unwrap().status, "planned");
        assert!(!db.get_plan_item(&ai).unwrap().unwrap().ready, "待确认的不能开始");
        assert_eq!(db.proposed_plan_counts().unwrap(), vec![(pid.clone(), "P".to_string(), 1)]);
        assert_eq!(db.plan_view(&pid).unwrap().percent_done, 0);
        assert!(db.set_plan_status(&ai, "done", "").is_err(), "不能越过批准直接完成");
        assert_eq!(db.approve_proposed_plan(&pid).unwrap(), 1);
        assert_eq!(db.get_plan_item(&ai).unwrap().unwrap().status, "planned");
        assert!(db.proposed_plan_counts().unwrap().is_empty());
        db.set_plan_status(&human, "blocked", "等外部确认").unwrap();
        assert_eq!(db.get_plan_item(&human).unwrap().unwrap().note, "等外部确认");
        assert!(!db.get_plan_item(&human).unwrap().unwrap().ready);
        db.set_plan_status(&human, "planned", "").unwrap();
        db.set_plan_status(&human, "done", "").unwrap();
        assert!(db.update_plan_item(&human, &item("改名", &[])).is_err(), "已完成的不能改");
        assert!(db.set_plan_status(&human, "blocked", "").is_err());
        db.set_plan_status(&human, "planned", "").unwrap(); // 重新打开
    }

    #[test]
    fn items_can_be_reordered_and_edited() {
        let (db, pid, _a, bob) = setup();
        let ids: Vec<String> = ["一", "二", "三"].iter().map(|t| db.create_plan_item(&pid, &item(t, &[]), None).unwrap()).collect();
        let order = |db: &Db| db.plan_view(&pid).unwrap().items.into_iter().map(|i| i.title).collect::<Vec<_>>();
        assert_eq!(order(&db), vec!["一", "二", "三"]);
        db.move_plan_item(&ids[2], -1).unwrap();
        assert_eq!(order(&db), vec!["一", "三", "二"]);
        db.move_plan_item(&ids[0], 1).unwrap();
        assert_eq!(order(&db), vec!["三", "一", "二"]);
        db.move_plan_item(&ids[0], 1).unwrap();
        db.move_plan_item(&ids[0], 1).unwrap(); // 已经在最后，不会越界
        assert_eq!(order(&db), vec!["三", "二", "一"]);
        db.update_plan_item(&ids[1], &NewPlanItem { assignee_id: Some(bob.clone()), kind: "cleanup".into(), ..item("二（改）", &[]) }).unwrap();
        let it = db.get_plan_item(&ids[1]).unwrap().unwrap();
        assert_eq!((it.title.as_str(), it.kind.as_str(), it.assignee_name.as_deref()), ("二（改）", "cleanup", Some("bob")));
    }

    #[test]
    fn project_goal_is_stored_and_bounded() {
        let (db, pid, _a, _b) = setup();
        db.set_project_goal(&pid, "  技术栈升级，业务行为不变  ").unwrap();
        assert_eq!(db.get_project(&pid).unwrap().unwrap().goal, "技术栈升级，业务行为不变");
        assert!(db.set_project_goal(&pid, &"字".repeat(GOAL_MAX_CHARS + 1)).is_err());
    }
}

#[cfg(test)]
mod waiting_tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn each_waiting_item_says_what_it_is_waiting_for() {
        let db = Db::open_memory().unwrap();
        let p = db.create_project("p", "", "/tmp/p-waiting").unwrap().id;
        let dev = db.add_employee("dev", "工程师", "x").unwrap();
        let outsider = db.add_employee("outsider", "工程师", "x").unwrap();
        db.add_member(&p, &dev.id, "developer").unwrap();
        let mk = |t: &str, assignee: Option<&str>, deps: Vec<String>| db.create_plan_item(&p, &NewPlanItem { title: t.into(), description: String::new(), acceptance: String::new(), kind: "migrate".into(), module: String::new(), assignee_id: assignee.map(String::from), deps }, None).unwrap();
        let a = mk("A 脚手架", Some(&dev.id), vec![]);
        let b = mk("B 存储", Some(&dev.id), vec![a.clone()]);
        db.add_member(&p, &outsider.id, "developer").unwrap();
        let c = mk("C 外人的活", Some(&outsider.id), vec![]);
        db.remove_member(&p, &outsider.id).unwrap(); // 指派之后 TA 被移出了项目：移出时已经改回未指定
        assert!(db.get_plan_item(&c).unwrap().unwrap().assignee_id.is_none(), "移出时名下没开始的条目改回未指定");
        // 旧数据（移出时没清指派）：直接写回去，检查等待原因
        db.conn.lock().unwrap().execute("UPDATE plan_items SET assignee_id = ?2 WHERE id = ?1", [&c, &outsider.id]).unwrap();
        let d = mk("D 无人认领", None, vec![]);
        db.conn.lock().unwrap().execute("UPDATE plan_items SET status = 'in_progress' WHERE id = ?1", [&a]).unwrap();

        let w = db.plan_waiting(&p, &HashSet::new(), false).unwrap();
        assert!(w[&b].iter().any(|x| x.contains("等前置完成") && x.contains("A 脚手架")), "{:?}", w[&b]);
        assert!(w[&b].iter().any(|x| x.contains("dev 正在做《A 脚手架》")), "{:?}", w[&b]);
        assert!(w[&c].iter().any(|x| x.contains("已不在本项目") && x.contains("按角色重新分配")), "{:?}", w[&c]);
        assert!(w[&d].iter().any(|x| x.contains("需要你手动")), "没人拦着、自主推进没开：提示手动派发 {:?}", w[&d]);
        let live = db.plan_waiting(&p, &HashSet::new(), true).unwrap();
        assert!(live[&d].iter().any(|x| x.contains("马上会被自主推进派出")));
        let busy: HashSet<String> = ["dev".to_string()].into();
        assert!(db.plan_waiting(&p, &busy, true).unwrap()[&a].is_empty(), "进行中的不显示等待");
    }
}
