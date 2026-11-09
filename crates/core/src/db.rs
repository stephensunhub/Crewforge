//! 存储层：SQLite（结构化数据）+ JSONL（追加式事件日志，便于审计与排查）。
//!
//! 所有群聊、任务、Agent 输出、审批都以事件形式写入 `events`，
//! 崩溃恢复和“按需唤醒”都基于事件表。

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// 迁移列表：下标 + 1 即版本号。只能追加，不能修改已发布的迁移。
const MIGRATIONS: &[&str] = &[MIGRATION_1, MIGRATION_2, MIGRATION_3, crate::board::MIGRATION_4, crate::memory::MIGRATION_5, crate::onboarding::MIGRATION_6, crate::scheduler::MIGRATION_7, crate::actions::MIGRATION_8, crate::plan::MIGRATION_9, crate::guest::MIGRATION_10];

const MIGRATION_1: &str = r#"
CREATE TABLE employees (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    role        TEXT NOT NULL,
    persona     TEXT NOT NULL,
    created_at  INTEGER NOT NULL
);
CREATE TABLE sessions (
    id            TEXT PRIMARY KEY,          -- 传给 CLI 的 session id
    employee_id   TEXT NOT NULL REFERENCES employees(id),
    project_path  TEXT NOT NULL,
    status        TEXT NOT NULL DEFAULT 'idle', -- idle | running
    started       INTEGER NOT NULL DEFAULT 0,   -- CLI 是否已建立该会话
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    UNIQUE (employee_id, project_path)
);
CREATE TABLE events (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    ts           INTEGER NOT NULL,
    kind         TEXT NOT NULL,
    employee_id  TEXT,
    session_id   TEXT,
    payload      TEXT NOT NULL
);
CREATE INDEX idx_events_session ON events(session_id, id);
CREATE INDEX idx_events_kind ON events(kind, id);
"#;

/// 任务：员工在某个仓库里的一项工作，对应一个分支。
/// 员工在每个仓库有一个持久的 worktree（便于复用 CLI 会话），任务以分支区分。
const MIGRATION_2: &str = r#"
CREATE TABLE tasks (
    id             TEXT PRIMARY KEY,
    repo_path      TEXT NOT NULL,
    employee_id    TEXT NOT NULL REFERENCES employees(id),
    title          TEXT NOT NULL,
    branch         TEXT NOT NULL,
    base_branch    TEXT NOT NULL,
    base_commit    TEXT NOT NULL,
    worktree_path  TEXT NOT NULL,
    status         TEXT NOT NULL DEFAULT 'active', -- active | merged | abandoned
    merged_commit  TEXT,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);
CREATE INDEX idx_tasks_employee ON tasks(employee_id, repo_path, status);
CREATE INDEX idx_tasks_repo ON tasks(repo_path, created_at);
"#;

/// 公司与项目：公司（本机一个）→ 员工（公司级长期身份）、项目；
/// 员工与项目多对多，通过 project_members 关联并在项目中担任角色。
/// 项目负责人是成员角色 'lead'；没有 lead 表示由人类担任。
const MIGRATION_3: &str = r#"
CREATE TABLE company (
    id           INTEGER PRIMARY KEY CHECK (id = 1),   -- 目前只有一家公司
    name         TEXT NOT NULL,
    description  TEXT NOT NULL DEFAULT '',
    conventions  TEXT NOT NULL DEFAULT '',             -- 公司级工程规范
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
);
INSERT INTO company (id, name, created_at, updated_at)
VALUES (1, '我的公司', CAST(strftime('%s','now') AS INTEGER) * 1000, CAST(strftime('%s','now') AS INTEGER) * 1000);

ALTER TABLE employees ADD COLUMN archived INTEGER NOT NULL DEFAULT 0;

CREATE TABLE projects (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    description  TEXT NOT NULL DEFAULT '',
    path         TEXT NOT NULL UNIQUE,                 -- Git 仓库根目录
    status       TEXT NOT NULL DEFAULT 'active',       -- active | archived
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
);
CREATE TABLE project_members (
    project_id   TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    employee_id  TEXT NOT NULL REFERENCES employees(id),
    role         TEXT NOT NULL DEFAULT 'developer',    -- lead | developer | qa | reviewer
    joined_at    INTEGER NOT NULL,
    PRIMARY KEY (project_id, employee_id)
);
CREATE INDEX idx_members_employee ON project_members(employee_id);

ALTER TABLE events ADD COLUMN project_id TEXT;         -- 事件归属的项目（群聊按它过滤）
CREATE INDEX idx_events_project ON events(project_id, id);
"#;

#[derive(Debug, Clone, Serialize)]
pub struct Employee {
    pub id: String,
    pub name: String,
    pub role: String,
    pub persona: String,
    pub archived: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Company {
    pub name: String,
    pub description: String,
    pub conventions: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub description: String,
    pub path: String,
    pub status: String,
    /// none（新建项目）| in_progress（导入中，不能派任务）| confirmed（已确认导入）
    pub onboarding: String,
    /// 重构目标与硬性约束：每个新任务开始时都会带上
    pub goal: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectSummary {
    pub project: Project,
    pub member_count: usize,
    pub active_tasks: usize,
    /// 项目负责人（AI）；None 表示由人类担任
    pub lead_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Member {
    pub employee_id: String,
    pub name: String,
    /// 员工的岗位（公司级）
    pub title: String,
    /// 员工在本项目中的角色：lead | developer | qa | reviewer
    pub role: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EmployeeCard {
    pub employee: Employee,
    pub projects: Vec<(String, String)>,
    pub active_tasks: usize,
}

pub const PROJECT_ROLES: &[&str] = &["lead", "developer", "qa", "reviewer"];

#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub id: String,
    pub employee_id: String,
    pub project_path: String,
    pub status: String,
    pub started: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Task {
    pub id: String,
    pub repo_path: String,
    pub employee_id: String,
    pub title: String,
    pub branch: String,
    pub base_branch: String,
    pub base_commit: String,
    pub worktree_path: String,
    pub status: String,
    pub merged_commit: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventRow {
    pub id: i64,
    pub ts: i64,
    pub kind: String,
    pub project_id: Option<String>,
    pub employee_id: Option<String>,
    pub session_id: Option<String>,
    pub payload: Value,
}

type EventHook = Arc<dyn Fn(&EventRow) + Send + Sync>;

pub struct Db {
    pub(crate) conn: Mutex<Connection>,
    jsonl: Mutex<Option<File>>,
    hook: Mutex<Option<EventHook>>,
}

/// 默认数据目录：环境变量 CREWFORGE_HOME，否则系统数据目录下的 Crewforge。
/// 命令行与桌面应用共用同一目录。
pub fn default_data_dir() -> PathBuf {
    std::env::var_os("CREWFORGE_HOME").map(PathBuf::from).unwrap_or_else(|| {
        let base = std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library").join("Application Support"))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("Crewforge")
    })
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl Db {
    /// 在目录下创建/打开 `crewforge.db` 与 `events.jsonl`。
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("创建数据目录 {}", dir.display()))?;
        let conn = Connection::open(dir.join("crewforge.db"))?;
        let jsonl = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("events.jsonl"))?;
        Self::init(conn, Some(jsonl))
    }

    pub fn open_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?, None)
    }

    fn init(conn: Connection, jsonl: Option<File>) -> Result<Self> {
        // MCP 服务器是另一个进程，会和应用同时写同一个库：忙时等待而不是立刻报错
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let latest = MIGRATIONS.len() as i64;
        if version > latest {
            anyhow::bail!("数据库版本 {version} 高于程序支持的版本 {latest}，请升级程序");
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            // 每个迁移在一个事务内完成，失败则整体回滚
            conn.execute_batch(&format!("BEGIN;\n{sql}\nPRAGMA user_version = {};\nCOMMIT;", i + 1))
                .with_context(|| format!("执行数据库迁移 {} 失败", i + 1))?;
        }
        Ok(Self { conn: Mutex::new(conn), jsonl: Mutex::new(jsonl), hook: Mutex::new(None) })
    }

    /// 注册事件钩子：每条事件落库后同步调用，用于向界面实时推送。钩子内不要做耗时操作。
    pub fn set_event_hook(&self, f: impl Fn(&EventRow) + Send + Sync + 'static) {
        *self.hook.lock().unwrap() = Some(Arc::new(f));
    }

    pub fn add_employee(&self, name: &str, role: &str, persona: &str) -> Result<Employee> {
        let e = Employee {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            role: role.into(),
            persona: persona.into(),
            archived: false,
        };
        self.conn.lock().unwrap().execute(
            "INSERT INTO employees (id, name, role, persona, created_at) VALUES (?1,?2,?3,?4,?5)",
            params![e.id, e.name, e.role, e.persona, now_ms()],
        )?;
        self.append_event("employee_created", Some(&e.id), None, &json!({"name": e.name, "role": e.role}))?;
        Ok(e)
    }

    pub fn employee_by_name(&self, name: &str) -> Result<Option<Employee>> {
        Ok(self
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT id, name, role, persona, archived FROM employees WHERE name = ?1",
                [name],
                row_to_employee,
            )
            .optional()?)
    }

    pub fn employee_by_id(&self, id: &str) -> Result<Option<Employee>> {
        Ok(self
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT id, name, role, persona, archived FROM employees WHERE id = ?1",
                [id],
                row_to_employee,
            )
            .optional()?)
    }

    pub fn list_employees(&self) -> Result<Vec<Employee>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id, name, role, persona, archived FROM employees WHERE archived = 0 AND role != 'guest' ORDER BY created_at")?;
        let rows = stmt.query_map([], row_to_employee)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// 员工在某项目中的会话：同一员工 + 同一项目路径只有一个，保证上下文隔离。
    pub fn get_or_create_session(&self, employee_id: &str, project_path: &str) -> Result<Session> {
        let conn = self.conn.lock().unwrap();
        let existing = conn
            .query_row(
                "SELECT id, employee_id, project_path, status, started FROM sessions
                 WHERE employee_id = ?1 AND project_path = ?2",
                params![employee_id, project_path],
                row_to_session,
            )
            .optional()?;
        if let Some(s) = existing {
            return Ok(s);
        }
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_ms();
        conn.execute(
            "INSERT INTO sessions (id, employee_id, project_path, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?4)",
            params![id, employee_id, project_path, now],
        )?;
        Ok(Session {
            id,
            employee_id: employee_id.into(),
            project_path: project_path.into(),
            status: "idle".into(),
            started: false,
        })
    }

    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, employee_id, project_path, status, started FROM sessions ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], row_to_session)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn set_session_status(&self, session_id: &str, status: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE sessions SET status = ?2, updated_at = ?3 WHERE id = ?1",
            params![session_id, status, now_ms()],
        )?;
        Ok(())
    }

    pub fn mark_session_started(&self, session_id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE sessions SET started = 1, updated_at = ?2 WHERE id = ?1",
            params![session_id, now_ms()],
        )?;
        Ok(())
    }

    pub fn create_task(&self, t: &Task) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO tasks (id, repo_path, employee_id, title, branch, base_branch, base_commit,
                                worktree_path, status, merged_commit, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![t.id, t.repo_path, t.employee_id, t.title, t.branch, t.base_branch, t.base_commit,
                    t.worktree_path, t.status, t.merged_commit, t.created_at, t.updated_at],
        )?;
        Ok(())
    }

    /// 员工在某仓库里正在进行的任务（同一时刻至多一个）。
    pub fn active_task(&self, employee_id: &str, repo_path: &str) -> Result<Option<Task>> {
        Ok(self
            .conn
            .lock()
            .unwrap()
            .query_row(
                &format!("{TASK_SELECT} WHERE employee_id = ?1 AND repo_path = ?2 AND status = 'active'"),
                params![employee_id, repo_path],
                row_to_task,
            )
            .optional()?)
    }

    pub fn get_task(&self, id: &str) -> Result<Option<Task>> {
        Ok(self
            .conn
            .lock()
            .unwrap()
            .query_row(&format!("{TASK_SELECT} WHERE id = ?1"), [id], row_to_task)
            .optional()?)
    }

    /// 某仓库的任务，最新的在前。
    pub fn list_tasks(&self, repo_path: &str) -> Result<Vec<Task>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!("{TASK_SELECT} WHERE repo_path = ?1 ORDER BY created_at DESC"))?;
        let rows = stmt.query_map([repo_path], row_to_task)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn set_task_status(&self, id: &str, status: &str, merged_commit: Option<&str>) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE tasks SET status = ?2, merged_commit = COALESCE(?3, merged_commit), updated_at = ?4 WHERE id = ?1",
            params![id, status, merged_commit, now_ms()],
        )?;
        Ok(())
    }

    /// 追加事件：先写 SQLite，再写 JSONL。返回事件 id。
    pub fn append_event(
        &self,
        kind: &str,
        employee_id: Option<&str>,
        session_id: Option<&str>,
        payload: &Value,
    ) -> Result<i64> {
        self.append_event_for(None, kind, employee_id, session_id, payload)
    }

    /// 追加归属于某个项目的事件（项目群聊按 project_id 过滤）。
    pub fn append_event_for(
        &self,
        project_id: Option<&str>,
        kind: &str,
        employee_id: Option<&str>,
        session_id: Option<&str>,
        payload: &Value,
    ) -> Result<i64> {
        let ts = now_ms();
        let payload_text = serde_json::to_string(payload)?;
        let id = {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO events (ts, kind, project_id, employee_id, session_id, payload) VALUES (?1,?2,?3,?4,?5,?6)",
                params![ts, kind, project_id, employee_id, session_id, payload_text],
            )?;
            conn.last_insert_rowid()
        };
        if let Some(f) = self.jsonl.lock().unwrap().as_mut() {
            let line = json!({"id": id, "ts": ts, "kind": kind, "project_id": project_id,
                              "employee_id": employee_id, "session_id": session_id, "payload": payload});
            writeln!(f, "{line}")?;
        }
        let hook = self.hook.lock().unwrap().clone();
        if let Some(h) = hook {
            h(&EventRow {
                id,
                ts,
                kind: kind.into(),
                project_id: project_id.map(Into::into),
                employee_id: employee_id.map(Into::into),
                session_id: session_id.map(Into::into),
                payload: payload.clone(),
            });
        }
        Ok(id)
    }

    pub fn recent_events(&self, limit: usize) -> Result<Vec<EventRow>> {
        self.query_events("", &[], limit)
    }

    pub fn max_event_id(&self) -> Result<i64> {
        Ok(self.conn.lock().unwrap().query_row("SELECT COALESCE(MAX(id), 0) FROM events", [], |r| r.get(0))?)
    }

    /// id 大于 `after_id` 的事件，按 id 升序。用于把**其他进程**（MCP 服务器、命令行）写入的事件推送给界面。
    pub fn events_after(&self, after_id: i64, limit: usize) -> Result<Vec<EventRow>> {
        let mut v = self.query_events("WHERE id > ?1", &[&after_id], usize::MAX / 2)?;
        v.sort_by_key(|e| e.id);
        v.truncate(limit);
        Ok(v)
    }

    /// 某个项目的最近事件（项目群聊使用）。
    pub fn recent_events_for_project(&self, project_id: &str, limit: usize) -> Result<Vec<EventRow>> {
        self.query_events("WHERE project_id = ?1", &[&project_id], limit)
    }

    fn query_events(&self, filter: &str, args: &[&dyn rusqlite::ToSql], limit: usize) -> Result<Vec<EventRow>> {
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT id, ts, kind, project_id, employee_id, session_id, payload FROM events {filter} ORDER BY id DESC LIMIT {}",
            limit as i64
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(args, |r| {
            let payload: String = r.get(6)?;
            Ok(EventRow {
                id: r.get(0)?,
                ts: r.get(1)?,
                kind: r.get(2)?,
                project_id: r.get(3)?,
                employee_id: r.get(4)?,
                session_id: r.get(5)?,
                payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
            })
        })?;
        let mut v: Vec<EventRow> = rows.collect::<std::result::Result<_, _>>()?;
        v.reverse();
        Ok(v)
    }
}

const TASK_SELECT: &str = "SELECT id, repo_path, employee_id, title, branch, base_branch, base_commit,
    worktree_path, status, merged_commit, created_at, updated_at FROM tasks";

fn row_to_task(r: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        id: r.get(0)?,
        repo_path: r.get(1)?,
        employee_id: r.get(2)?,
        title: r.get(3)?,
        branch: r.get(4)?,
        base_branch: r.get(5)?,
        base_commit: r.get(6)?,
        worktree_path: r.get(7)?,
        status: r.get(8)?,
        merged_commit: r.get(9)?,
        created_at: r.get(10)?,
        updated_at: r.get(11)?,
    })
}

impl Db {
    // ---------- 公司 ----------
    pub fn company(&self) -> Result<Company> {
        Ok(self.conn.lock().unwrap().query_row(
            "SELECT name, description, conventions FROM company WHERE id = 1",
            [],
            |r| Ok(Company { name: r.get(0)?, description: r.get(1)?, conventions: r.get(2)? }),
        )?)
    }

    pub fn update_company(&self, name: &str, description: &str, conventions: &str) -> Result<()> {
        if name.trim().is_empty() {
            anyhow::bail!("公司名称不能为空");
        }
        self.conn.lock().unwrap().execute(
            "UPDATE company SET name = ?1, description = ?2, conventions = ?3, updated_at = ?4 WHERE id = 1",
            params![name.trim(), description, conventions, now_ms()],
        )?;
        Ok(())
    }

    // ---------- 员工（公司级） ----------
    /// 修改岗位与身份说明。名称不可改：它决定 worktree 目录与分支名。
    pub fn update_employee(&self, id: &str, role: &str, persona: &str) -> Result<()> {
        let n = self.conn.lock().unwrap().execute(
            "UPDATE employees SET role = ?2, persona = ?3 WHERE id = ?1",
            params![id, role, persona],
        )?;
        if n == 0 {
            anyhow::bail!("员工不存在");
        }
        Ok(())
    }

    /// 归档员工：保留历史与记忆，不再出现在列表里，也不能被分配任务。
    pub fn set_employee_archived(&self, id: &str, archived: bool) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE employees SET archived = ?2 WHERE id = ?1",
            params![id, archived as i64],
        )?;
        Ok(())
    }

    /// 员工参与的项目 (id, 名称)。
    pub fn employee_projects(&self, employee_id: &str) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT p.id, p.name FROM project_members m JOIN projects p ON p.id = m.project_id
             WHERE m.employee_id = ?1 AND p.status = 'active' ORDER BY p.name",
        )?;
        let rows = stmt.query_map([employee_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn employee_cards(&self) -> Result<Vec<EmployeeCard>> {
        let mut out = Vec::new();
        for e in self.list_employees()? {
            let projects = self.employee_projects(&e.id)?;
            let active_tasks: i64 = self.conn.lock().unwrap().query_row(
                "SELECT COUNT(*) FROM tasks WHERE employee_id = ?1 AND status = 'active'",
                [&e.id],
                |r| r.get(0),
            )?;
            out.push(EmployeeCard { employee: e, projects, active_tasks: active_tasks as usize });
        }
        Ok(out)
    }

    // ---------- 项目 ----------
    pub fn create_project(&self, name: &str, description: &str, path: &str) -> Result<Project> {
        if name.trim().is_empty() {
            anyhow::bail!("项目名称不能为空");
        }
        let now = now_ms();
        let p = Project {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.trim().into(),
            description: description.into(),
            path: path.into(),
            status: "active".into(),
            onboarding: "none".into(),
            goal: String::new(),
            created_at: now,
            updated_at: now,
        };
        let res = self.conn.lock().unwrap().execute(
            "INSERT INTO projects (id, name, description, path, status, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?6)",
            params![p.id, p.name, p.description, p.path, p.status, now],
        );
        if let Err(rusqlite::Error::SqliteFailure(e, _)) = &res {
            if e.code == rusqlite::ErrorCode::ConstraintViolation {
                anyhow::bail!("该目录已经是一个项目");
            }
        }
        res?;
        self.append_event_for(Some(&p.id), "project_created", None, None, &json!({"name": p.name, "path": p.path}))?;
        Ok(p)
    }

    pub fn get_project(&self, id: &str) -> Result<Option<Project>> {
        Ok(self.conn.lock().unwrap().query_row(&format!("{PROJECT_SELECT} WHERE id = ?1"), [id], row_to_project).optional()?)
    }

    pub fn project_by_path(&self, path: &str) -> Result<Option<Project>> {
        Ok(self.conn.lock().unwrap().query_row(&format!("{PROJECT_SELECT} WHERE path = ?1"), [path], row_to_project).optional()?)
    }

    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!("{PROJECT_SELECT} WHERE status = 'active' ORDER BY created_at"))?;
        let rows = stmt.query_map([], row_to_project)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn project_summaries(&self) -> Result<Vec<ProjectSummary>> {
        let mut out = Vec::new();
        for project in self.list_projects()? {
            let members = self.list_members(&project.id)?;
            let active_tasks: i64 = self.conn.lock().unwrap().query_row(
                "SELECT COUNT(*) FROM tasks WHERE repo_path = ?1 AND status = 'active'",
                [&project.path],
                |r| r.get(0),
            )?;
            out.push(ProjectSummary {
                lead_name: members.iter().find(|m| m.role == "lead").map(|m| m.name.clone()),
                member_count: members.len(),
                active_tasks: active_tasks as usize,
                project,
            });
        }
        Ok(out)
    }

    pub fn set_onboarding(&self, id: &str, state: &str) -> Result<()> {
        if !matches!(state, "none" | "in_progress" | "confirmed") {
            anyhow::bail!("未知的导入状态: {state}");
        }
        self.conn.lock().unwrap().execute("UPDATE projects SET onboarding = ?2, updated_at = ?3 WHERE id = ?1", params![id, state, now_ms()])?;
        Ok(())
    }

    /// 修改名称与描述。路径不可改：任务与工作区都按它定位。
    pub fn update_project(&self, id: &str, name: &str, description: &str) -> Result<()> {
        if name.trim().is_empty() {
            anyhow::bail!("项目名称不能为空");
        }
        self.conn.lock().unwrap().execute(
            "UPDATE projects SET name = ?2, description = ?3, updated_at = ?4 WHERE id = ?1",
            params![id, name.trim(), description, now_ms()],
        )?;
        Ok(())
    }

    /// 归档项目：数据保留，不再出现在列表里。存在进行中的任务时拒绝。
    pub fn archive_project(&self, id: &str) -> Result<()> {
        let p = self.get_project(id)?.context("项目不存在")?;
        let active: i64 = self.conn.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM tasks WHERE repo_path = ?1 AND status = 'active'",
            [&p.path],
            |r| r.get(0),
        )?;
        if active > 0 {
            anyhow::bail!("还有 {active} 个进行中的任务，请先合并或放弃");
        }
        self.conn.lock().unwrap().execute(
            "UPDATE projects SET status = 'archived', updated_at = ?2 WHERE id = ?1",
            params![id, now_ms()],
        )?;
        Ok(())
    }

    // ---------- 项目成员 ----------
    pub fn list_members(&self, project_id: &str) -> Result<Vec<Member>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT e.id, e.name, e.role, m.role FROM project_members m JOIN employees e ON e.id = m.employee_id
             WHERE m.project_id = ?1 AND e.archived = 0
             ORDER BY CASE m.role WHEN 'lead' THEN 0 ELSE 1 END, m.joined_at",
        )?;
        let rows = stmt.query_map([project_id], |r| {
            Ok(Member { employee_id: r.get(0)?, name: r.get(1)?, title: r.get(2)?, role: r.get(3)? })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn is_member(&self, project_id: &str, employee_id: &str) -> Result<bool> {
        let n: i64 = self.conn.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM project_members WHERE project_id = ?1 AND employee_id = ?2",
            params![project_id, employee_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    pub fn add_member(&self, project_id: &str, employee_id: &str, role: &str) -> Result<()> {
        if !PROJECT_ROLES.contains(&role) {
            anyhow::bail!("未知的项目角色: {role}");
        }
        if self.is_member(project_id, employee_id)? {
            anyhow::bail!("该员工已经是项目成员");
        }
        self.conn.lock().unwrap().execute(
            "INSERT INTO project_members (project_id, employee_id, role, joined_at) VALUES (?1,?2,?3,?4)",
            params![project_id, employee_id, role, now_ms()],
        )?;
        if role == "lead" {
            self.set_member_role(project_id, employee_id, "lead")?;
        }
        Ok(())
    }

    /// 修改成员在项目中的角色。负责人只能有一位：指定新负责人时，原负责人降为开发者。
    pub fn set_member_role(&self, project_id: &str, employee_id: &str, role: &str) -> Result<()> {
        if !PROJECT_ROLES.contains(&role) {
            anyhow::bail!("未知的项目角色: {role}");
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        if role == "lead" {
            tx.execute(
                "UPDATE project_members SET role = 'developer' WHERE project_id = ?1 AND role = 'lead' AND employee_id != ?2",
                params![project_id, employee_id],
            )?;
        }
        let n = tx.execute(
            "UPDATE project_members SET role = ?3 WHERE project_id = ?1 AND employee_id = ?2",
            params![project_id, employee_id, role],
        )?;
        if n == 0 {
            anyhow::bail!("该员工不是项目成员");
        }
        tx.commit()?;
        Ok(())
    }

    /// 移出项目成员。该员工在项目里有进行中的任务时拒绝，避免工作区被遗弃。
    pub fn remove_member(&self, project_id: &str, employee_id: &str) -> Result<()> {
        let p = self.get_project(project_id)?.context("项目不存在")?;
        if self.active_task(employee_id, &p.path)?.is_some() {
            anyhow::bail!("该员工在本项目有进行中的任务，请先合并或放弃");
        }
        self.conn.lock().unwrap().execute(
            "DELETE FROM project_members WHERE project_id = ?1 AND employee_id = ?2",
            params![project_id, employee_id],
        )?;
        Ok(())
    }
}

fn row_to_employee(r: &rusqlite::Row<'_>) -> rusqlite::Result<Employee> {
    Ok(Employee { id: r.get(0)?, name: r.get(1)?, role: r.get(2)?, persona: r.get(3)?, archived: r.get::<_, i64>(4)? != 0 })
}

fn row_to_project(r: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: r.get(0)?, name: r.get(1)?, description: r.get(2)?, path: r.get(3)?,
        status: r.get(4)?, created_at: r.get(5)?, updated_at: r.get(6)?, onboarding: r.get(7)?, goal: r.get(8)?,
    })
}

const PROJECT_SELECT: &str = "SELECT id, name, description, path, status, created_at, updated_at, onboarding, goal FROM projects";

fn row_to_session(r: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: r.get(0)?,
        employee_id: r.get(1)?,
        project_path: r.get(2)?,
        status: r.get(3)?,
        started: r.get::<_, i64>(4)? != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn employee_and_session_are_isolated() {
        let db = Db::open_memory().unwrap();
        let a = db.add_employee("alice", "fullstack", "p").unwrap();
        let b = db.add_employee("bob", "fullstack", "p").unwrap();
        let sa = db.get_or_create_session(&a.id, "/proj").unwrap();
        let sb = db.get_or_create_session(&b.id, "/proj").unwrap();
        assert_ne!(sa.id, sb.id);
        // 同一员工同一项目复用会话
        assert_eq!(sa.id, db.get_or_create_session(&a.id, "/proj").unwrap().id);
        // 同一员工不同项目是不同会话
        assert_ne!(sa.id, db.get_or_create_session(&a.id, "/other").unwrap().id);
    }

    #[test]
    fn duplicate_employee_name_rejected() {
        let db = Db::open_memory().unwrap();
        db.add_employee("alice", "fullstack", "p").unwrap();
        assert!(db.add_employee("alice", "qa", "p").is_err());
    }

    #[test]
    fn events_are_ordered_and_limited() {
        let db = Db::open_memory().unwrap();
        for i in 0..5 {
            db.append_event("t", None, None, &json!({"i": i})).unwrap();
        }
        let v = db.recent_events(3).unwrap();
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].payload["i"], 2);
        assert_eq!(v[2].payload["i"], 4);
    }

    #[test]
    fn events_after_returns_only_newer_events_in_order() {
        let db = Db::open_memory().unwrap();
        assert_eq!(db.max_event_id().unwrap(), 0);
        for i in 0..5 { db.append_event("t", None, None, &json!({"i": i})).unwrap(); }
        let first = db.events_after(0, 100).unwrap();
        assert_eq!(first.iter().map(|e| e.payload["i"].as_i64().unwrap()).collect::<Vec<_>>(), vec![0, 1, 2, 3, 4]);
        let mid = first[2].id;
        assert_eq!(db.events_after(mid, 100).unwrap().len(), 2);
        assert_eq!(db.events_after(mid, 1).unwrap()[0].payload["i"], 3, "limit 取最早的几条，保证不漏");
        assert!(db.events_after(db.max_event_id().unwrap(), 10).unwrap().is_empty());
    }

    #[test]
    fn event_hook_receives_events() {
        let db = Db::open_memory().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        db.set_event_hook(move |e| s2.lock().unwrap().push(e.kind.clone()));
        db.append_event("a", None, None, &json!({})).unwrap();
        db.append_event("b", None, None, &json!({})).unwrap();
        assert_eq!(*seen.lock().unwrap(), vec!["a", "b"]);
    }

    fn sample_task(emp: &str, id: &str) -> Task {
        Task {
            id: id.into(), repo_path: "/r".into(), employee_id: emp.into(), title: "t".into(),
            branch: format!("b-{id}"), base_branch: "main".into(), base_commit: "abc".into(),
            worktree_path: "/w".into(), status: "active".into(), merged_commit: None,
            created_at: 1, updated_at: 1,
        }
    }

    #[test]
    fn task_lifecycle() {
        let db = Db::open_memory().unwrap();
        let a = db.add_employee("alice", "fullstack", "p").unwrap();
        db.create_task(&sample_task(&a.id, "t1")).unwrap();
        assert_eq!(db.active_task(&a.id, "/r").unwrap().unwrap().id, "t1");
        assert!(db.active_task(&a.id, "/other").unwrap().is_none());
        db.set_task_status("t1", "merged", Some("deadbeef")).unwrap();
        assert!(db.active_task(&a.id, "/r").unwrap().is_none());
        let t = db.get_task("t1").unwrap().unwrap();
        assert_eq!((t.status.as_str(), t.merged_commit.as_deref()), ("merged", Some("deadbeef")));
        db.create_task(&sample_task(&a.id, "t2")).unwrap();
        assert_eq!(db.list_tasks("/r").unwrap().len(), 2);
    }

    #[test]
    fn migrates_v1_database_to_latest() {
        let dir = std::env::temp_dir().join(format!("crewforge-mig-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        {
            // 模拟只有 v1 结构的旧数据库，并带有数据
            let c = Connection::open(dir.join("crewforge.db")).unwrap();
            c.execute_batch(MIGRATION_1).unwrap();
            c.pragma_update(None, "user_version", 1).unwrap();
            c.execute("INSERT INTO employees (id,name,role,persona,created_at) VALUES ('e','old','r','p',1)", []).unwrap();
        }
        let db = Db::open(&dir).unwrap();
        assert_eq!(db.employee_by_name("old").unwrap().unwrap().id, "e");
        assert!(!db.employee_by_name("old").unwrap().unwrap().archived);
        assert_eq!(db.company().unwrap().name, "我的公司", "升级后自动有公司记录");
        db.create_task(&sample_task("e", "t1")).unwrap();
        drop(db);
        // 再次打开不会重复迁移
        assert!(Db::open(&dir).unwrap().get_task("t1").unwrap().is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn company_exists_and_updates() {
        let db = Db::open_memory().unwrap();
        assert_eq!(db.company().unwrap().name, "我的公司");
        db.update_company("Acme", "做事的公司", "所有代码必须有测试").unwrap();
        let c = db.company().unwrap();
        assert_eq!((c.name.as_str(), c.conventions.as_str()), ("Acme", "所有代码必须有测试"));
        assert!(db.update_company("  ", "", "").is_err());
    }

    #[test]
    fn employee_update_and_archive() {
        let db = Db::open_memory().unwrap();
        let a = db.add_employee("alice", "全栈", "p").unwrap();
        db.update_employee(&a.id, "架构师", "新的身份").unwrap();
        let got = db.employee_by_name("alice").unwrap().unwrap();
        assert_eq!((got.role.as_str(), got.persona.as_str()), ("架构师", "新的身份"));
        db.set_employee_archived(&a.id, true).unwrap();
        assert!(db.list_employees().unwrap().is_empty());
        assert!(db.employee_by_id(&a.id).unwrap().unwrap().archived, "归档后仍可查到历史");
        assert!(db.update_employee("nope", "r", "p").is_err());
    }

    #[test]
    fn projects_members_and_single_lead() {
        let db = Db::open_memory().unwrap();
        let a = db.add_employee("alice", "全栈", "p").unwrap();
        let b = db.add_employee("bob", "全栈", "p").unwrap();
        let p = db.create_project("Shop", "商城", "/repo/shop").unwrap();
        assert!(db.create_project("Dup", "", "/repo/shop").is_err(), "同一目录不能重复建项目");
        assert!(db.create_project("  ", "", "/x").is_err());

        db.add_member(&p.id, &a.id, "lead").unwrap();
        db.add_member(&p.id, &b.id, "developer").unwrap();
        assert!(db.add_member(&p.id, &b.id, "qa").is_err(), "不能重复加入");
        assert!(db.add_member(&p.id, &b.id, "boss").is_err(), "未知角色");
        assert_eq!(db.list_members(&p.id).unwrap()[0].name, "alice", "负责人排在最前");

        db.set_member_role(&p.id, &b.id, "lead").unwrap();
        let m = db.list_members(&p.id).unwrap();
        assert_eq!(m.iter().filter(|x| x.role == "lead").count(), 1);
        assert_eq!(m.iter().find(|x| x.name == "alice").unwrap().role, "developer", "原负责人降为开发者");

        // 员工与项目多对多
        let p2 = db.create_project("Blog", "", "/repo/blog").unwrap();
        db.add_member(&p2.id, &a.id, "developer").unwrap();
        assert_eq!(db.employee_projects(&a.id).unwrap().len(), 2);
        let sums = db.project_summaries().unwrap();
        assert_eq!(sums.len(), 2);
        assert_eq!(sums[0].lead_name.as_deref(), Some("bob"));
        assert_eq!(sums[1].lead_name, None, "没有 lead 表示人类负责");
    }

    #[test]
    fn member_with_active_task_cannot_leave_and_project_cannot_archive() {
        let db = Db::open_memory().unwrap();
        let a = db.add_employee("alice", "全栈", "p").unwrap();
        let p = db.create_project("Shop", "", "/r").unwrap();
        db.add_member(&p.id, &a.id, "developer").unwrap();
        db.create_task(&sample_task(&a.id, "t1")).unwrap(); // repo_path = /r
        assert!(db.remove_member(&p.id, &a.id).is_err());
        assert!(db.archive_project(&p.id).is_err());
        db.set_task_status("t1", "merged", None).unwrap();
        db.remove_member(&p.id, &a.id).unwrap();
        db.archive_project(&p.id).unwrap();
        assert!(db.list_projects().unwrap().is_empty());
        assert!(db.get_project(&p.id).unwrap().is_some(), "归档不删除数据");
    }

    #[test]
    fn events_are_filtered_by_project() {
        let db = Db::open_memory().unwrap();
        db.append_event_for(Some("p1"), "a", None, None, &json!({})).unwrap();
        db.append_event_for(Some("p2"), "b", None, None, &json!({})).unwrap();
        db.append_event("c", None, None, &json!({})).unwrap();
        let v = db.recent_events_for_project("p1", 10).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, "a");
        assert_eq!(db.recent_events(10).unwrap().len(), 3);
    }

    #[test]
    fn session_status_roundtrip() {
        let db = Db::open_memory().unwrap();
        let a = db.add_employee("alice", "fullstack", "p").unwrap();
        let s = db.get_or_create_session(&a.id, "/p").unwrap();
        db.set_session_status(&s.id, "running").unwrap();
        db.mark_session_started(&s.id).unwrap();
        let s2 = db.get_or_create_session(&a.id, "/p").unwrap();
        assert_eq!(s2.status, "running");
        assert!(s2.started);
    }
}
