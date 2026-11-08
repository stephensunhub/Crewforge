//! 议题板与会议：议题（需要讨论并决定的事）、候选方案、会议、会议消息。
//!
//! 议题回答"需要讨论并决定什么"，与任务、Bug 是不同对象。解决后产生决策记录。
//! 被推翻的决议不修改原记录，而是新建议题并标注"取代"。

use crate::db::{Db, now_ms};
use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;

pub const MIGRATION_4: &str = r#"
CREATE TABLE issues (
    id              TEXT PRIMARY KEY,
    project_id      TEXT NOT NULL REFERENCES projects(id),
    title           TEXT NOT NULL,
    summary         TEXT NOT NULL DEFAULT '',
    kind            TEXT NOT NULL,                -- technical | requirement | bug | risk | process | implementation
    high_risk       INTEGER NOT NULL DEFAULT 0,   -- 涉及资金、生产、不可逆操作、安全：只能人类拍板
    status          TEXT NOT NULL DEFAULT 'open',
    raised_by       TEXT,                         -- 员工 id；NULL 表示人类
    owner_id        TEXT REFERENCES employees(id),-- 推动解决的负责人
    decision        TEXT,
    decision_reason TEXT,
    decided_by      TEXT,                         -- 员工 id 或 'human'
    decided_at      INTEGER,
    supersedes      TEXT REFERENCES issues(id),
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);
CREATE INDEX idx_issues_project ON issues(project_id, status);
CREATE TABLE issue_options (
    id            TEXT PRIMARY KEY,
    issue_id      TEXT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    title         TEXT NOT NULL,
    description   TEXT NOT NULL DEFAULT '',
    proposed_by   TEXT,                           -- 员工 id；NULL 表示人类
    status        TEXT NOT NULL DEFAULT 'candidate', -- candidate | chosen | rejected
    reject_reason TEXT,
    created_at    INTEGER NOT NULL
);
CREATE TABLE meetings (
    id                TEXT PRIMARY KEY,
    project_id        TEXT NOT NULL REFERENCES projects(id),
    kind              TEXT NOT NULL,              -- requirements | design | code_review | qa_case | retro
    title             TEXT NOT NULL,
    host_id           TEXT REFERENCES employees(id), -- NULL 表示人类主持
    task_id           TEXT REFERENCES tasks(id),     -- Code Review 关联的任务
    status            TEXT NOT NULL DEFAULT 'open',  -- open | closed
    minutes           TEXT NOT NULL DEFAULT '',
    max_agent_replies INTEGER NOT NULL DEFAULT 12,
    created_at        INTEGER NOT NULL,
    closed_at         INTEGER
);
CREATE INDEX idx_meetings_project ON meetings(project_id, created_at);
CREATE TABLE meeting_issues (
    meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    issue_id   TEXT NOT NULL REFERENCES issues(id),
    PRIMARY KEY (meeting_id, issue_id)
);
CREATE TABLE meeting_messages (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    meeting_id   TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    author_kind  TEXT NOT NULL,                   -- human | employee | system
    employee_id  TEXT,
    content      TEXT NOT NULL,
    created_at   INTEGER NOT NULL
);
CREATE INDEX idx_mmsg ON meeting_messages(meeting_id, id);
-- 每个员工在每场会议里已经"听到"的最后一条消息，用于只发送新增内容，节省 token
CREATE TABLE meeting_participants (
    meeting_id   TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    employee_id  TEXT NOT NULL REFERENCES employees(id),
    last_seen_id INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (meeting_id, employee_id)
);
"#;

pub const ISSUE_KINDS: &[&str] = &["technical", "requirement", "bug", "risk", "process", "implementation"];
pub const MEETING_KINDS: &[&str] = &["requirements", "design", "code_review", "qa_case", "retro"];

#[derive(Debug, Clone, Serialize)]
pub struct Issue {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub summary: String,
    pub kind: String,
    pub high_risk: bool,
    pub status: String,
    pub raised_by: Option<String>,
    pub owner_id: Option<String>,
    pub decision: Option<String>,
    pub decision_reason: Option<String>,
    pub decided_by: Option<String>,
    pub decided_at: Option<i64>,
    pub supersedes: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct IssueOption {
    pub id: String,
    pub issue_id: String,
    pub title: String,
    pub description: String,
    pub proposed_by: Option<String>,
    pub status: String,
    pub reject_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IssueView {
    pub issue: Issue,
    pub options: Vec<IssueOption>,
    /// 谁有权拍板，用于界面展示
    pub decider: String,
    pub raised_by_name: Option<String>,
    pub owner_name: Option<String>,
    /// 取代的旧议题标题
    pub supersedes_title: Option<String>,
    /// 被哪个新议题取代
    pub superseded_by: Option<(String, String)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingDecision {
    pub issue: Issue,
    pub project_name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Meeting {
    pub id: String,
    pub project_id: String,
    pub kind: String,
    pub title: String,
    pub host_id: Option<String>,
    pub task_id: Option<String>,
    pub status: String,
    pub minutes: String,
    pub max_agent_replies: i64,
    pub created_at: i64,
    pub closed_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MeetingMessage {
    pub id: i64,
    pub meeting_id: String,
    pub author_kind: String,
    pub employee_id: Option<String>,
    pub content: String,
    pub created_at: i64,
}

/// 做决定的人。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Actor {
    Human,
    Employee(String),
}

/// 议题状态机允许的转移。
pub fn can_transition(from: &str, to: &str) -> bool {
    matches!(
        (from, to),
        ("open", "discussing") | ("open", "cancelled")
            | ("discussing", "pending_decision") | ("discussing", "shelved") | ("discussing", "cancelled")
            | ("pending_decision", "decided") | ("pending_decision", "discussing")
            | ("decided", "implementing") | ("decided", "closed")
            | ("implementing", "closed")
            | ("shelved", "discussing")
    )
}

/// 谁有权拍板：
/// - 人类始终可以；
/// - 需求类、高风险类：只能人类；
/// - 一般技术方案 / 流程 / 风险 / Bug 归因：项目负责人（没有 AI 负责人时只有人类）；
/// - 代码细节（implementation）：议题负责人。
/// 临时外部 AI（Guest）不是员工，永远无权。
pub fn check_authority(issue: &Issue, actor: &Actor, lead_id: Option<&str>) -> Result<(), String> {
    let Actor::Employee(eid) = actor else { return Ok(()) };
    if issue.high_risk || issue.kind == "requirement" {
        return Err("需求、业务方向和高风险议题只有人类可以拍板".into());
    }
    let allowed = if issue.kind == "implementation" { issue.owner_id.as_deref() } else { lead_id };
    if allowed == Some(eid.as_str()) { Ok(()) } else { Err("你没有权限拍板这个议题".into()) }
}

pub fn decider_label(issue: &Issue, lead_name: Option<&str>, owner_name: Option<&str>) -> String {
    if issue.high_risk || issue.kind == "requirement" {
        return "仅人类".into();
    }
    if issue.kind == "implementation" {
        return format!("议题负责人（{}）或人类", owner_name.unwrap_or("未指定"));
    }
    match lead_name {
        Some(n) => format!("项目负责人（{n}）或人类"),
        None => "人类".into(),
    }
}

const ISSUE_SELECT: &str = "SELECT id, project_id, title, summary, kind, high_risk, status, raised_by, owner_id,
    decision, decision_reason, decided_by, decided_at, supersedes, created_at, updated_at FROM issues";

fn row_to_issue(r: &rusqlite::Row<'_>) -> rusqlite::Result<Issue> {
    Ok(Issue {
        id: r.get(0)?, project_id: r.get(1)?, title: r.get(2)?, summary: r.get(3)?, kind: r.get(4)?,
        high_risk: r.get::<_, i64>(5)? != 0, status: r.get(6)?, raised_by: r.get(7)?, owner_id: r.get(8)?,
        decision: r.get(9)?, decision_reason: r.get(10)?, decided_by: r.get(11)?, decided_at: r.get(12)?,
        supersedes: r.get(13)?, created_at: r.get(14)?, updated_at: r.get(15)?,
    })
}

fn row_to_meeting(r: &rusqlite::Row<'_>) -> rusqlite::Result<Meeting> {
    Ok(Meeting {
        id: r.get(0)?, project_id: r.get(1)?, kind: r.get(2)?, title: r.get(3)?, host_id: r.get(4)?,
        task_id: r.get(5)?, status: r.get(6)?, minutes: r.get(7)?, max_agent_replies: r.get(8)?,
        created_at: r.get(9)?, closed_at: r.get(10)?,
    })
}
const MEETING_SELECT: &str = "SELECT id, project_id, kind, title, host_id, task_id, status, minutes,
    max_agent_replies, created_at, closed_at FROM meetings";

impl Db {
    // ================= 议题 =================
    #[allow(clippy::too_many_arguments)]
    pub fn create_issue(
        &self, project_id: &str, title: &str, summary: &str, kind: &str, high_risk: bool,
        raised_by: Option<&str>, owner_id: Option<&str>, supersedes: Option<&str>,
    ) -> Result<Issue> {
        if title.trim().is_empty() {
            bail!("议题标题不能为空");
        }
        if !ISSUE_KINDS.contains(&kind) {
            bail!("未知的议题类型: {kind}");
        }
        self.get_project(project_id)?.context("项目不存在")?;
        if let Some(old) = supersedes {
            let o = self.get_issue(old)?.context("被取代的议题不存在")?;
            if o.project_id != project_id {
                bail!("只能取代同一项目的议题");
            }
            if !matches!(o.status.as_str(), "decided" | "implementing" | "closed") {
                bail!("只能取代已有决议的议题");
            }
        }
        let now = now_ms();
        let id = uuid::Uuid::new_v4().to_string();
        self.conn.lock().unwrap().execute(
            "INSERT INTO issues (id, project_id, title, summary, kind, high_risk, raised_by, owner_id, supersedes, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?10)",
            params![id, project_id, title.trim(), summary, kind, high_risk as i64, raised_by, owner_id, supersedes, now],
        )?;
        self.append_event_for(Some(project_id), "issue_created", raised_by, None,
            &json!({"issue_id": id, "title": title.trim(), "kind": kind, "supersedes": supersedes}))?;
        Ok(self.get_issue(&id)?.context("创建议题失败")?)
    }

    pub fn get_issue(&self, id: &str) -> Result<Option<Issue>> {
        Ok(self.conn.lock().unwrap().query_row(&format!("{ISSUE_SELECT} WHERE id = ?1"), [id], row_to_issue).optional()?)
    }

    pub fn list_issues(&self, project_id: &str) -> Result<Vec<Issue>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!("{ISSUE_SELECT} WHERE project_id = ?1 ORDER BY created_at DESC"))?;
        let rows = stmt.query_map([project_id], row_to_issue)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn issue_options(&self, issue_id: &str) -> Result<Vec<IssueOption>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, issue_id, title, description, proposed_by, status, reject_reason FROM issue_options
             WHERE issue_id = ?1 ORDER BY created_at, rowid",
        )?;
        let rows = stmt.query_map([issue_id], |r| {
            Ok(IssueOption {
                id: r.get(0)?, issue_id: r.get(1)?, title: r.get(2)?, description: r.get(3)?,
                proposed_by: r.get(4)?, status: r.get(5)?, reject_reason: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn issue_views(&self, project_id: &str) -> Result<Vec<IssueView>> {
        let lead = self.list_members(project_id)?.into_iter().find(|m| m.role == "lead");
        let issues = self.list_issues(project_id)?;
        let titles: HashMap<String, String> = issues.iter().map(|i| (i.id.clone(), i.title.clone())).collect();
        let name_of = |id: &Option<String>| -> Result<Option<String>> {
            Ok(match id { Some(i) => self.employee_by_id(i)?.map(|e| e.name), None => None })
        };
        let mut out = Vec::new();
        for issue in &issues {
            let owner_name = name_of(&issue.owner_id)?;
            out.push(IssueView {
                options: self.issue_options(&issue.id)?,
                decider: decider_label(issue, lead.as_ref().map(|l| l.name.as_str()), owner_name.as_deref()),
                raised_by_name: name_of(&issue.raised_by)?,
                owner_name,
                supersedes_title: issue.supersedes.as_ref().and_then(|s| titles.get(s).cloned()),
                superseded_by: issues.iter().find(|n| n.supersedes.as_deref() == Some(&issue.id)).map(|n| (n.id.clone(), n.title.clone())),
                issue: issue.clone(),
            });
        }
        Ok(out)
    }

    pub fn update_issue(&self, id: &str, title: &str, summary: &str, owner_id: Option<&str>) -> Result<()> {
        let i = self.get_issue(id)?.context("议题不存在")?;
        if matches!(i.status.as_str(), "decided" | "implementing" | "closed" | "cancelled") {
            bail!("议题已有结论或已结束，不能再修改；如需推翻请新建议题取代它");
        }
        if title.trim().is_empty() {
            bail!("议题标题不能为空");
        }
        self.conn.lock().unwrap().execute(
            "UPDATE issues SET title = ?2, summary = ?3, owner_id = ?4, updated_at = ?5 WHERE id = ?1",
            params![id, title.trim(), summary, owner_id, now_ms()],
        )?;
        Ok(())
    }

    pub fn add_option(&self, issue_id: &str, title: &str, description: &str, proposed_by: Option<&str>) -> Result<IssueOption> {
        let i = self.get_issue(issue_id)?.context("议题不存在")?;
        if !matches!(i.status.as_str(), "open" | "discussing" | "pending_decision") {
            bail!("议题已有结论或已结束，不能再添加方案");
        }
        if title.trim().is_empty() {
            bail!("方案标题不能为空");
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.conn.lock().unwrap().execute(
            "INSERT INTO issue_options (id, issue_id, title, description, proposed_by, created_at) VALUES (?1,?2,?3,?4,?5,?6)",
            params![id, issue_id, title.trim(), description, proposed_by, now_ms()],
        )?;
        Ok(self.issue_options(issue_id)?.into_iter().find(|o| o.id == id).context("添加方案失败")?)
    }

    pub fn set_issue_status(&self, id: &str, to: &str) -> Result<()> {
        let i = self.get_issue(id)?.context("议题不存在")?;
        if to == "decided" {
            bail!("请通过\"拍板\"来形成决议");
        }
        if !can_transition(&i.status, to) {
            bail!("议题不能从「{}」转为「{}」", i.status, to);
        }
        self.conn.lock().unwrap().execute(
            "UPDATE issues SET status = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, to, now_ms()],
        )?;
        self.append_event_for(Some(&i.project_id), "issue_status", None, None,
            &json!({"issue_id": id, "title": i.title, "from": i.status, "to": to}))?;
        Ok(())
    }

    /// 拍板：校验权限与状态，记录最终方案、原因，并保留被否方案及其原因。
    pub fn decide_issue(
        &self, id: &str, actor: &Actor, chosen_option: Option<&str>, decision: &str, reason: &str,
        rejected_reasons: &HashMap<String, String>,
    ) -> Result<()> {
        let i = self.get_issue(id)?.context("议题不存在")?;
        if i.status != "pending_decision" {
            bail!("只有「待决策」的议题才能拍板，当前是「{}」", i.status);
        }
        let lead = self.list_members(&i.project_id)?.into_iter().find(|m| m.role == "lead").map(|m| m.employee_id);
        check_authority(&i, actor, lead.as_deref()).map_err(anyhow::Error::msg)?;
        if decision.trim().is_empty() {
            bail!("请写明最终决定");
        }
        let options = self.issue_options(id)?;
        if let Some(c) = chosen_option {
            if !options.iter().any(|o| o.id == c) {
                bail!("所选方案不属于这个议题");
            }
        }
        let by = match actor { Actor::Human => "human".to_string(), Actor::Employee(e) => e.clone() };
        let now = now_ms();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        for o in &options {
            if Some(o.id.as_str()) == chosen_option {
                tx.execute("UPDATE issue_options SET status = 'chosen' WHERE id = ?1", [&o.id])?;
            } else {
                let why = rejected_reasons.get(&o.id).map(|s| s.trim()).filter(|s| !s.is_empty()).unwrap_or("未被采纳");
                tx.execute("UPDATE issue_options SET status = 'rejected', reject_reason = ?2 WHERE id = ?1", params![o.id, why])?;
            }
        }
        tx.execute(
            "UPDATE issues SET status = 'decided', decision = ?2, decision_reason = ?3, decided_by = ?4, decided_at = ?5, updated_at = ?5 WHERE id = ?1",
            params![id, decision.trim(), reason.trim(), by, now],
        )?;
        tx.commit()?;
        drop(conn);
        // 决议沉淀为项目决策记忆；它取代的旧决议对应的记忆自动过期
        if let Some(done) = self.get_issue(id)? {
            let rejected: Vec<(String, String)> = self.issue_options(id)?.into_iter()
                .filter(|o| o.status == "rejected")
                .map(|o| (o.title, o.reject_reason.unwrap_or_default()))
                .collect();
            self.record_decision_memory(&done, &by, &rejected)?;
        }
        self.append_event_for(Some(&i.project_id), "issue_decided", Some(&by).filter(|b| *b != "human").map(String::as_str), None,
            &json!({"issue_id": id, "title": i.title, "decision": decision.trim(), "decided_by": by}))?;
        Ok(())
    }

    /// 所有项目中等待决策的议题（待办收件箱使用）。
    pub fn pending_decisions(&self) -> Result<Vec<PendingDecision>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT i.id, i.project_id, i.title, i.summary, i.kind, i.high_risk, i.status, i.raised_by, i.owner_id,
                    i.decision, i.decision_reason, i.decided_by, i.decided_at, i.supersedes, i.created_at, i.updated_at, p.name
             FROM issues i JOIN projects p ON p.id = i.project_id
             WHERE i.status = 'pending_decision' AND p.status = 'active' ORDER BY i.updated_at"
        ))?;
        let rows = stmt.query_map([], |r| Ok(PendingDecision { issue: row_to_issue(r)?, project_name: r.get(16)? }))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// AI 员工提议、等待人类确认是否值得讨论的议题（待办收件箱使用）。
    pub fn ai_proposals(&self) -> Result<Vec<PendingDecision>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT i.id, i.project_id, i.title, i.summary, i.kind, i.high_risk, i.status, i.raised_by, i.owner_id,
                    i.decision, i.decision_reason, i.decided_by, i.decided_at, i.supersedes, i.created_at, i.updated_at, p.name
             FROM issues i JOIN projects p ON p.id = i.project_id
             WHERE i.status = 'open' AND i.raised_by IS NOT NULL AND p.status = 'active' ORDER BY i.created_at"
        ))?;
        let rows = stmt.query_map([], |r| Ok(PendingDecision { issue: row_to_issue(r)?, project_name: r.get(16)? }))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    // ================= 会议 =================
    pub fn create_meeting(
        &self, project_id: &str, kind: &str, title: &str, host_id: Option<&str>, task_id: Option<&str>,
        issue_ids: &[String], max_agent_replies: i64,
    ) -> Result<Meeting> {
        if !MEETING_KINDS.contains(&kind) {
            bail!("未知的会议类型: {kind}");
        }
        if title.trim().is_empty() {
            bail!("会议主题不能为空");
        }
        if !(1..=200).contains(&max_agent_replies) {
            bail!("发言上限需在 1 到 200 之间");
        }
        self.get_project(project_id)?.context("项目不存在")?;
        if let Some(h) = host_id {
            if !self.is_member(project_id, h)? {
                bail!("主持人必须是项目成员");
            }
        }
        if let Some(t) = task_id {
            let task = self.get_task(t)?.context("关联的任务不存在")?;
            let p = self.get_project(project_id)?.context("项目不存在")?;
            if task.repo_path != p.path {
                bail!("关联的任务不属于这个项目");
            }
        }
        for i in issue_ids {
            let issue = self.get_issue(i)?.context("关联的议题不存在")?;
            if issue.project_id != project_id {
                bail!("只能关联本项目的议题");
            }
        }
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_ms();
        {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO meetings (id, project_id, kind, title, host_id, task_id, max_agent_replies, created_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![id, project_id, kind, title.trim(), host_id, task_id, max_agent_replies, now],
            )?;
            for i in issue_ids {
                conn.execute("INSERT OR IGNORE INTO meeting_issues (meeting_id, issue_id) VALUES (?1,?2)", params![id, i])?;
            }
        }
        self.append_event_for(Some(project_id), "meeting_started", host_id, None, &json!({"meeting_id": id, "title": title.trim(), "kind": kind}))?;
        Ok(self.get_meeting(&id)?.context("创建会议失败")?)
    }

    pub fn get_meeting(&self, id: &str) -> Result<Option<Meeting>> {
        Ok(self.conn.lock().unwrap().query_row(&format!("{MEETING_SELECT} WHERE id = ?1"), [id], row_to_meeting).optional()?)
    }

    pub fn list_meetings(&self, project_id: &str) -> Result<Vec<Meeting>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!("{MEETING_SELECT} WHERE project_id = ?1 ORDER BY created_at DESC"))?;
        let rows = stmt.query_map([project_id], row_to_meeting)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn meeting_issue_ids(&self, meeting_id: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT issue_id FROM meeting_issues WHERE meeting_id = ?1")?;
        let rows = stmt.query_map([meeting_id], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn link_meeting_issue(&self, meeting_id: &str, issue_id: &str) -> Result<()> {
        let m = self.get_meeting(meeting_id)?.context("会议不存在")?;
        let i = self.get_issue(issue_id)?.context("议题不存在")?;
        if m.project_id != i.project_id {
            bail!("只能关联本项目的议题");
        }
        self.conn.lock().unwrap().execute("INSERT OR IGNORE INTO meeting_issues (meeting_id, issue_id) VALUES (?1,?2)", params![meeting_id, issue_id])?;
        Ok(())
    }

    pub fn post_message(&self, meeting_id: &str, author_kind: &str, employee_id: Option<&str>, content: &str) -> Result<MeetingMessage> {
        let m = self.get_meeting(meeting_id)?.context("会议不存在")?;
        if m.status != "open" {
            bail!("会议已结束，不能再发言");
        }
        if content.trim().is_empty() {
            bail!("消息不能为空");
        }
        let now = now_ms();
        let id = {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO meeting_messages (meeting_id, author_kind, employee_id, content, created_at) VALUES (?1,?2,?3,?4,?5)",
                params![meeting_id, author_kind, employee_id, content, now],
            )?;
            conn.last_insert_rowid()
        };
        self.append_event_for(Some(&m.project_id), "meeting_message", employee_id, None,
            &json!({"meeting_id": meeting_id, "message_id": id, "author_kind": author_kind}))?;
        Ok(MeetingMessage { id, meeting_id: meeting_id.into(), author_kind: author_kind.into(), employee_id: employee_id.map(Into::into), content: content.into(), created_at: now })
    }

    pub fn meeting_messages(&self, meeting_id: &str, after_id: i64) -> Result<Vec<MeetingMessage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, meeting_id, author_kind, employee_id, content, created_at FROM meeting_messages
             WHERE meeting_id = ?1 AND id > ?2 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![meeting_id, after_id], |r| {
            Ok(MeetingMessage { id: r.get(0)?, meeting_id: r.get(1)?, author_kind: r.get(2)?, employee_id: r.get(3)?, content: r.get(4)?, created_at: r.get(5)? })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// 员工已发表的回复数，用于限制每场会议的 AI 发言总量。
    pub fn agent_reply_count(&self, meeting_id: &str) -> Result<i64> {
        Ok(self.conn.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM meeting_messages WHERE meeting_id = ?1 AND author_kind = 'employee'",
            [meeting_id], |r| r.get(0))?)
    }

    pub fn participant_last_seen(&self, meeting_id: &str, employee_id: &str) -> Result<Option<i64>> {
        Ok(self.conn.lock().unwrap().query_row(
            "SELECT last_seen_id FROM meeting_participants WHERE meeting_id = ?1 AND employee_id = ?2",
            params![meeting_id, employee_id], |r| r.get(0)).optional()?)
    }

    pub fn set_participant_last_seen(&self, meeting_id: &str, employee_id: &str, last_seen: i64) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO meeting_participants (meeting_id, employee_id, last_seen_id) VALUES (?1,?2,?3)
             ON CONFLICT(meeting_id, employee_id) DO UPDATE SET last_seen_id = MAX(last_seen_id, excluded.last_seen_id)",
            params![meeting_id, employee_id, last_seen],
        )?;
        Ok(())
    }

    pub fn set_minutes(&self, meeting_id: &str, minutes: &str) -> Result<()> {
        self.conn.lock().unwrap().execute("UPDATE meetings SET minutes = ?2 WHERE id = ?1", params![meeting_id, minutes])?;
        Ok(())
    }

    pub fn close_meeting(&self, meeting_id: &str) -> Result<()> {
        let m = self.get_meeting(meeting_id)?.context("会议不存在")?;
        if m.status == "closed" {
            bail!("会议已经结束");
        }
        self.conn.lock().unwrap().execute(
            "UPDATE meetings SET status = 'closed', closed_at = ?2 WHERE id = ?1", params![meeting_id, now_ms()])?;
        self.append_event_for(Some(&m.project_id), "meeting_closed", None, None, &json!({"meeting_id": meeting_id, "title": m.title}))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (Db, String, String, String) {
        let db = Db::open_memory().unwrap();
        let lead = db.add_employee("lead", "架构师", "p").unwrap();
        let dev = db.add_employee("dev", "全栈", "p").unwrap();
        let p = db.create_project("P", "", "/r").unwrap();
        db.add_member(&p.id, &lead.id, "lead").unwrap();
        db.add_member(&p.id, &dev.id, "developer").unwrap();
        (db, p.id, lead.id, dev.id)
    }

    fn to_pending(db: &Db, id: &str) {
        db.set_issue_status(id, "discussing").unwrap();
        db.set_issue_status(id, "pending_decision").unwrap();
    }

    #[test]
    fn state_machine_rules() {
        assert!(can_transition("open", "discussing"));
        assert!(!can_transition("open", "decided"));
        assert!(!can_transition("closed", "discussing"));
        assert!(!can_transition("decided", "discussing"), "已决议不能退回讨论，应新建议题取代");
        let (db, pid, ..) = setup();
        let i = db.create_issue(&pid, "WS 还是 SSE", "选推送方案", "technical", false, None, None, None).unwrap();
        assert!(db.set_issue_status(&i.id, "closed").is_err());
        assert!(db.set_issue_status(&i.id, "decided").is_err(), "不能绕过拍板直接决议");
        to_pending(&db, &i.id);
        assert_eq!(db.pending_decisions().unwrap().len(), 1);
    }

    #[test]
    fn authority_by_kind() {
        let (db, pid, lead, dev) = setup();
        let mk = |kind: &str, risk: bool, owner: Option<&str>| db.create_issue(&pid, "t", "", kind, risk, None, owner, None).unwrap();
        let (l, d) = (Actor::Employee(lead.clone()), Actor::Employee(dev.clone()));
        assert!(check_authority(&mk("technical", false, None), &l, Some(&lead)).is_ok());
        assert!(check_authority(&mk("technical", false, None), &d, Some(&lead)).is_err());
        assert!(check_authority(&mk("requirement", false, None), &l, Some(&lead)).is_err(), "需求只能人类");
        assert!(check_authority(&mk("technical", true, None), &l, Some(&lead)).is_err(), "高风险只能人类");
        assert!(check_authority(&mk("implementation", false, Some(&dev)), &d, Some(&lead)).is_ok(), "代码细节由议题负责人");
        assert!(check_authority(&mk("implementation", false, Some(&dev)), &l, Some(&lead)).is_err());
        assert!(check_authority(&mk("requirement", true, None), &Actor::Human, None).is_ok(), "人类始终可以");
        // 没有 AI 负责人时，AI 不能拍板一般技术议题
        assert!(check_authority(&mk("technical", false, None), &d, None).is_err());
    }

    #[test]
    fn decide_records_choice_and_keeps_rejected_reasons() {
        let (db, pid, lead, _) = setup();
        let i = db.create_issue(&pid, "WS 还是 SSE", "", "technical", false, None, None, None).unwrap();
        let a = db.add_option(&i.id, "WebSocket", "双向", None).unwrap();
        let b = db.add_option(&i.id, "SSE", "单向，简单", Some(&lead)).unwrap();
        assert!(db.decide_issue(&i.id, &Actor::Human, Some(&a.id), "用 WS", "需要双向", &HashMap::new()).is_err(), "未到待决策");
        to_pending(&db, &i.id);
        assert!(db.decide_issue(&i.id, &Actor::Human, Some("nope"), "x", "y", &HashMap::new()).is_err());
        assert!(db.decide_issue(&i.id, &Actor::Human, Some(&a.id), "  ", "y", &HashMap::new()).is_err(), "必须写明决定");
        let mut rej = HashMap::new();
        rej.insert(b.id.clone(), "无法满足客户端上报".to_string());
        db.decide_issue(&i.id, &Actor::Human, Some(&a.id), "采用 WebSocket", "需要双向通信", &rej).unwrap();

        let v = &db.issue_views(&pid).unwrap()[0];
        assert_eq!(v.issue.status, "decided");
        assert_eq!(v.issue.decided_by.as_deref(), Some("human"));
        let chosen = v.options.iter().find(|o| o.id == a.id).unwrap();
        let rejected = v.options.iter().find(|o| o.id == b.id).unwrap();
        assert_eq!((chosen.status.as_str(), rejected.status.as_str()), ("chosen", "rejected"));
        assert_eq!(rejected.reject_reason.as_deref(), Some("无法满足客户端上报"));
        assert!(db.update_issue(&i.id, "改标题", "", None).is_err(), "已决议不可改");
        assert!(db.add_option(&i.id, "新方案", "", None).is_err());
        assert!(db.decide_issue(&i.id, &Actor::Human, None, "再来一次", "", &HashMap::new()).is_err(), "不能重复拍板");
        assert!(db.pending_decisions().unwrap().is_empty());
    }

    #[test]
    fn ai_cannot_decide_requirement_but_lead_can_decide_technical() {
        let (db, pid, lead, dev) = setup();
        let req = db.create_issue(&pid, "做不做会员", "", "requirement", false, None, None, None).unwrap();
        to_pending(&db, &req.id);
        assert!(db.decide_issue(&req.id, &Actor::Employee(lead.clone()), None, "做", "", &HashMap::new()).is_err());
        let tech = db.create_issue(&pid, "选库", "", "technical", false, None, None, None).unwrap();
        to_pending(&db, &tech.id);
        assert!(db.decide_issue(&tech.id, &Actor::Employee(dev), None, "x", "", &HashMap::new()).is_err());
        db.decide_issue(&tech.id, &Actor::Employee(lead.clone()), None, "用 serde", "生态好", &HashMap::new()).unwrap();
        assert_eq!(db.get_issue(&tech.id).unwrap().unwrap().decided_by.as_deref(), Some(lead.as_str()));
    }

    #[test]
    fn supersede_keeps_history() {
        let (db, pid, ..) = setup();
        let old = db.create_issue(&pid, "旧方案", "", "technical", false, None, None, None).unwrap();
        assert!(db.create_issue(&pid, "新", "", "technical", false, None, None, Some(&old.id)).is_err(), "未决议不能被取代");
        to_pending(&db, &old.id);
        db.decide_issue(&old.id, &Actor::Human, None, "用旧的", "", &HashMap::new()).unwrap();
        let new = db.create_issue(&pid, "推翻旧方案", "", "technical", false, None, None, Some(&old.id)).unwrap();
        let views = db.issue_views(&pid).unwrap();
        let o = views.iter().find(|v| v.issue.id == old.id).unwrap();
        let n = views.iter().find(|v| v.issue.id == new.id).unwrap();
        assert_eq!(o.issue.decision.as_deref(), Some("用旧的"), "原记录不被修改");
        assert_eq!(o.superseded_by.as_ref().unwrap().0, new.id);
        assert_eq!(n.supersedes_title.as_deref(), Some("旧方案"));
        assert!(db.create_issue(&pid, "x", "", "weird", false, None, None, None).is_err());
        assert!(db.create_issue(&pid, " ", "", "technical", false, None, None, None).is_err());
    }

    #[test]
    fn meetings_messages_and_participants() {
        let (db, pid, lead, dev) = setup();
        let i = db.create_issue(&pid, "议题", "", "technical", false, None, None, None).unwrap();
        assert!(db.create_meeting(&pid, "party", "x", None, None, &[], 10).is_err());
        assert!(db.create_meeting(&pid, "design", "x", Some("nobody"), None, &[], 10).is_err(), "主持人需是成员");
        assert!(db.create_meeting(&pid, "design", "x", None, None, &[], 0).is_err());
        assert!(db.create_meeting(&pid, "design", "x", None, None, &["nope".into()], 10).is_err());
        let m = db.create_meeting(&pid, "design", "方案评审", Some(&lead), None, &[i.id.clone()], 3).unwrap();
        assert_eq!(db.meeting_issue_ids(&m.id).unwrap(), vec![i.id.clone()]);

        let a = db.post_message(&m.id, "human", None, "大家看看").unwrap();
        let b = db.post_message(&m.id, "employee", Some(&dev), "我觉得可以").unwrap();
        assert!(db.post_message(&m.id, "human", None, "  ").is_err());
        assert_eq!(db.agent_reply_count(&m.id).unwrap(), 1);
        assert_eq!(db.meeting_messages(&m.id, 0).unwrap().len(), 2);
        assert_eq!(db.meeting_messages(&m.id, a.id).unwrap()[0].id, b.id);

        assert_eq!(db.participant_last_seen(&m.id, &dev).unwrap(), None);
        db.set_participant_last_seen(&m.id, &dev, b.id).unwrap();
        db.set_participant_last_seen(&m.id, &dev, a.id).unwrap(); // 不会回退
        assert_eq!(db.participant_last_seen(&m.id, &dev).unwrap(), Some(b.id));

        db.close_meeting(&m.id).unwrap();
        assert!(db.post_message(&m.id, "human", None, "还有话").is_err(), "结束后不能发言");
        assert!(db.close_meeting(&m.id).is_err());
        assert_eq!(db.get_meeting(&m.id).unwrap().unwrap().status, "closed");
    }
}
