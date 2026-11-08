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
const MIGRATIONS: &[&str] = &[MIGRATION_1, MIGRATION_2];

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

#[derive(Debug, Clone, Serialize)]
pub struct Employee {
    pub id: String,
    pub name: String,
    pub role: String,
    pub persona: String,
}

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
    pub employee_id: Option<String>,
    pub session_id: Option<String>,
    pub payload: Value,
}

type EventHook = Arc<dyn Fn(&EventRow) + Send + Sync>;

pub struct Db {
    conn: Mutex<Connection>,
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
                "SELECT id, name, role, persona FROM employees WHERE name = ?1",
                [name],
                |r| Ok(Employee { id: r.get(0)?, name: r.get(1)?, role: r.get(2)?, persona: r.get(3)? }),
            )
            .optional()?)
    }

    pub fn employee_by_id(&self, id: &str) -> Result<Option<Employee>> {
        Ok(self
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT id, name, role, persona FROM employees WHERE id = ?1",
                [id],
                |r| Ok(Employee { id: r.get(0)?, name: r.get(1)?, role: r.get(2)?, persona: r.get(3)? }),
            )
            .optional()?)
    }

    pub fn list_employees(&self) -> Result<Vec<Employee>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id, name, role, persona FROM employees ORDER BY created_at")?;
        let rows = stmt.query_map([], |r| {
            Ok(Employee { id: r.get(0)?, name: r.get(1)?, role: r.get(2)?, persona: r.get(3)? })
        })?;
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
        let ts = now_ms();
        let payload_text = serde_json::to_string(payload)?;
        let id = {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO events (ts, kind, employee_id, session_id, payload) VALUES (?1,?2,?3,?4,?5)",
                params![ts, kind, employee_id, session_id, payload_text],
            )?;
            conn.last_insert_rowid()
        };
        if let Some(f) = self.jsonl.lock().unwrap().as_mut() {
            let line = json!({"id": id, "ts": ts, "kind": kind, "employee_id": employee_id,
                              "session_id": session_id, "payload": payload});
            writeln!(f, "{line}")?;
        }
        let hook = self.hook.lock().unwrap().clone();
        if let Some(h) = hook {
            h(&EventRow {
                id,
                ts,
                kind: kind.into(),
                employee_id: employee_id.map(Into::into),
                session_id: session_id.map(Into::into),
                payload: payload.clone(),
            });
        }
        Ok(id)
    }

    pub fn recent_events(&self, limit: usize) -> Result<Vec<EventRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, ts, kind, employee_id, session_id, payload FROM events ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| {
            let payload: String = r.get(5)?;
            Ok(EventRow {
                id: r.get(0)?,
                ts: r.get(1)?,
                kind: r.get(2)?,
                employee_id: r.get(3)?,
                session_id: r.get(4)?,
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
        db.create_task(&sample_task("e", "t1")).unwrap();
        drop(db);
        // 再次打开不会重复迁移
        assert!(Db::open(&dir).unwrap().get_task("t1").unwrap().is_some());
        std::fs::remove_dir_all(&dir).ok();
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
