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

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
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
        if version == 0 {
            conn.execute_batch(SCHEMA)?;
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        } else if version != SCHEMA_VERSION {
            anyhow::bail!("数据库版本 {version} 与程序版本 {SCHEMA_VERSION} 不一致，尚未实现迁移");
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
