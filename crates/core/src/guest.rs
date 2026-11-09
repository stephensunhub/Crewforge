//! 外部临时 AI（Guest）：用户或成员在会议里邀请的“外部顾问”。
//! - 不是公司员工：不进花名册、不属于任何项目、没有个人长期记忆，会议结束或到期即归档。
//! - 全新会话，没有项目历史偏见；只拿到最小上下文（议题、邀请目的、讨论），不给公司规范、记忆与其他会议。
//! - 只读，没有团队协作工具（不能提议题、不能提议记忆），更无权拍板——结论由人和负责人确认后才写入决策。

use crate::db::{Db, Employee, now_ms};
use crate::orchestrator::Orchestrator;
use anyhow::{Context, Result, bail};
use rusqlite::params;
use serde::Serialize;

pub const MIGRATION_10: &str = r#"
CREATE TABLE meeting_guests (
    meeting_id   TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    employee_id  TEXT NOT NULL REFERENCES employees(id),
    brief        TEXT NOT NULL DEFAULT '',     -- 邀请目的与要问的问题
    invited_by   TEXT NOT NULL DEFAULT 'human',
    created_at   INTEGER NOT NULL,
    expires_at   INTEGER NOT NULL,
    PRIMARY KEY (meeting_id, employee_id)
);
"#;

pub const GUEST_ROLE: &str = "guest";
/// 默认有效期（小时）与上限。
pub const DEFAULT_HOURS: i64 = 24;
pub const MAX_HOURS: i64 = 72;

#[derive(Debug, Clone, Serialize)]
pub struct Guest {
    pub employee_id: String,
    pub name: String,
    pub brief: String,
    pub invited_by: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub active: bool,
}

impl Db {
    pub fn meeting_guests(&self, meeting_id: &str) -> Result<Vec<Guest>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT g.employee_id, e.name, g.brief, g.invited_by, g.created_at, g.expires_at, e.archived
             FROM meeting_guests g JOIN employees e ON e.id = g.employee_id WHERE g.meeting_id = ?1 ORDER BY g.created_at")?;
        let now = now_ms();
        let rows = stmt.query_map([meeting_id], |r| {
            let expires: i64 = r.get(5)?;
            let archived: i64 = r.get(6)?;
            Ok(Guest { employee_id: r.get(0)?, name: r.get(1)?, brief: r.get(2)?, invited_by: r.get(3)?, created_at: r.get(4)?, expires_at: expires, active: archived == 0 && expires > now })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn guest_of_meeting(&self, meeting_id: &str, employee_id: &str) -> Result<Option<Guest>> {
        Ok(self.meeting_guests(meeting_id)?.into_iter().find(|g| g.employee_id == employee_id))
    }

    /// 结束某场会议里的所有顾问：归档其员工记录（会话与发言保留以便追溯，但不再可唤醒）。
    pub fn end_meeting_guests(&self, meeting_id: &str) -> Result<usize> {
        let ids: Vec<String> = self.meeting_guests(meeting_id)?.into_iter().filter(|g| g.active).map(|g| g.employee_id).collect();
        for id in &ids {
            self.set_employee_archived(id, true)?;
        }
        Ok(ids.len())
    }

    pub fn end_guest(&self, meeting_id: &str, employee_id: &str) -> Result<()> {
        if self.guest_of_meeting(meeting_id, employee_id)?.is_none() {
            bail!("不是这场会议的顾问");
        }
        self.set_employee_archived(employee_id, true)?;
        self.conn.lock().unwrap().execute(
            "UPDATE meeting_guests SET expires_at = ?3 WHERE meeting_id = ?1 AND employee_id = ?2", params![meeting_id, employee_id, now_ms()])?;
        Ok(())
    }
}

const GUEST_PERSONA: &str = "你是受邀参加这场讨论的外部顾问（临时），不属于这家公司，也不了解它的历史与偏好。\
你的价值是独立、直接的第三方视角：基于给你的材料和只读查看到的代码给出判断和理由，指出风险与被忽略的替代方案。\
你没有拍板权，不要替团队做决定；不确定的地方直接说明。不要客套，不要重复别人已经说过的话。";

impl Orchestrator {
    /// 邀请外部顾问加入会议。`invited_by`：human（用户直接邀请，无需批准）或某位成员（调用方负责先取得人类批准）。
    pub fn invite_guest(&self, meeting_id: &str, name: &str, brief: &str, hours: Option<i64>, invited_by: &str) -> Result<Employee> {
        let meeting = self.db.get_meeting(meeting_id)?.context("会议不存在")?;
        if meeting.status != "open" {
            bail!("会议已结束，不能再邀请顾问");
        }
        let name = name.trim().trim_start_matches('@');
        if name.is_empty() || name.chars().any(|c| c.is_whitespace() || c == '@') {
            bail!("顾问的名字不能为空，也不能含空格或 @");
        }
        if brief.trim().is_empty() {
            bail!("请写明邀请目的和想问的问题");
        }
        if self.db.employee_by_name(name)?.is_some() {
            bail!("已经有叫 {name} 的人了，换一个名字");
        }
        let hours = hours.unwrap_or(DEFAULT_HOURS).clamp(1, MAX_HOURS);
        let guest = self.db.add_employee(name, GUEST_ROLE, GUEST_PERSONA)?;
        let now = now_ms();
        self.db.conn.lock().unwrap().execute(
            "INSERT INTO meeting_guests (meeting_id, employee_id, brief, invited_by, created_at, expires_at) VALUES (?1,?2,?3,?4,?5,?6)",
            params![meeting_id, guest.id, brief.trim(), invited_by, now, now + hours * 3_600_000],
        )?;
        self.db.append_event_for(Some(&meeting.project_id), "guest_invited", Some(&guest.id), None,
            &serde_json::json!({"meeting_id": meeting_id, "name": name, "brief": brief.trim(), "invited_by": invited_by, "hours": hours}))?;
        self.db.post_message(meeting_id, "system", None, &format!(
            "已邀请外部顾问 @{name}（{hours} 小时内有效）。目的：{}\n顾问只读、没有项目记忆、只看到本会议的议题和讨论，无权拍板；会议结束后自动离场。", brief.trim()))?;
        Ok(guest)
    }

    /// 顾问收到的提示：最小上下文——邀请目的、议题、讨论；不带公司规范、项目目标之外的记忆和其他会议。
    pub(crate) fn guest_prompt(&self, meeting: &crate::board::Meeting, guest: &Employee, first: bool, transcript: &str) -> Result<String> {
        let brief = self.db.guest_of_meeting(&meeting.id, &guest.id)?.map(|g| g.brief).unwrap_or_default();
        let project = self.db.get_project(&meeting.project_id)?.context("项目不存在")?;
        let mut p = String::new();
        if first {
            p.push_str(&format!("[外部顾问] 你受邀参加一场讨论。主题：{}；项目：{}（代码在当前目录，只读）。\n", meeting.title, project.name));
            p.push_str(&format!("\n邀请你的目的与要问的问题：\n{brief}\n"));
            let issues = self.issues_block(meeting, true)?;
            if !issues.is_empty() {
                p.push_str(&format!("\n相关议题：\n{issues}"));
            }
        } else {
            p.push_str(&format!("[会议继续] {}\n", meeting.title));
        }
        p.push_str(&format!("\n{}：\n{}\n", if first { "目前的讨论" } else { "自你上次发言以来的新消息" }, if transcript.is_empty() { "（暂无）".into() } else { transcript.to_string() }));
        p.push_str("\n要求：只读，不要修改任何文件。直接回答被问到的问题，控制在 250 字以内：先给结论，再给理由和风险；列出你看过哪些文件或依据；不确定就说不确定。");
        Ok(p)
    }
}
