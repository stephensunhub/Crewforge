//! 对齐会（原“站会”）：定期由负责人带头，对齐目标、计划、任务和行动项。
//!
//! 会议本身复用站会的流程（系统汇总事实 → 在做事的成员各报告一次 → 确定性的对齐检查 → 纪要），
//! 这里补上：
//! - 每个项目的配置：开关、间隔、主持人。默认：有 AI 负责人的项目开启；人当负责人的项目关闭（不让人去挨个主持）。
//!   人当负责人时可以打开，并指定一名 AI 成员代为主持；人只读结果，不用参会。
//! - 到点判断：距上次对齐会超过间隔，并且这期间有变化（合并、计划、行动项）才开，没变化不花额度。
//! - 目标检查：主持人对照目标和计划判断是否还对得上。目标不对时，只有项目负责人能提议改（人点头才生效）；
//!   代为主持的非负责人只在纪要里指出。

use crate::db::{Db, Employee, now_ms};
use crate::orchestrator::Orchestrator;
use crate::runner::Approver;
use anyhow::{Context, Result, bail};
use rusqlite::params;
use serde::Serialize;
use serde_json::Value;

pub const MIGRATION_18: &str = "CREATE TABLE align_config (
    project_id     TEXT PRIMARY KEY REFERENCES projects(id),
    enabled        INTEGER,                 -- NULL = 默认（有 AI 负责人时开启）
    interval_hours INTEGER NOT NULL DEFAULT 12,
    host_id        TEXT REFERENCES employees(id)
);";

pub const INTERVALS: &[i64] = &[4, 12, 24, 72];
/// 这些事件说明项目有了新的变化，值得再对齐一次。
const CHANGE_KINDS: &[&str] = &["task_merged", "task_abandoned", "plan_item_status", "plan_changed", "action_item_status", "meeting_minutes", "plan_goal_decided"];

#[derive(Debug, Clone, Serialize)]
pub struct AlignConfig {
    /// 实际是否生效（显式设置优先，否则看有没有 AI 负责人）
    pub enabled: bool,
    /// 用户有没有显式设置过
    pub explicit: bool,
    pub interval_hours: i64,
    pub host_id: Option<String>,
    /// 实际的主持人：指定的成员，否则项目负责人；都没有则为 None（此时不会开）
    pub host_name: Option<String>,
    pub has_ai_lead: bool,
    pub last_at: Option<i64>,
    pub last_meeting_id: Option<String>,
    pub last_goal_note: Option<String>,
    pub last_check: Option<String>,
}

impl Db {
    /// 实际主持人（员工 id）：指定且仍是成员的人，否则项目负责人。
    pub fn align_host(&self, project_id: &str) -> Result<Option<String>> {
        let members = self.list_members(project_id)?;
        let cfg_host: Option<String> = self.conn.lock().unwrap().query_row("SELECT host_id FROM align_config WHERE project_id = ?1", params![project_id], |r| r.get(0)).ok().flatten();
        if let Some(h) = cfg_host.filter(|h| members.iter().any(|m| &m.employee_id == h)) {
            return Ok(Some(h));
        }
        Ok(members.iter().find(|m| m.role == "lead").map(|m| m.employee_id.clone()))
    }

    pub fn align_config(&self, project_id: &str) -> Result<AlignConfig> {
        let members = self.list_members(project_id)?;
        let has_ai_lead = members.iter().any(|m| m.role == "lead");
        let row: Option<(Option<i64>, i64, Option<String>)> = self.conn.lock().unwrap().query_row(
            "SELECT enabled, interval_hours, host_id FROM align_config WHERE project_id = ?1", params![project_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).ok();
        let (enabled_raw, interval_hours, host_id) = row.unwrap_or((None, 12, None));
        let host = self.align_host(project_id)?;
        let host_name = host.and_then(|h| members.iter().find(|m| m.employee_id == h).map(|m| m.name.clone()));
        let (last_at, last_meeting_id, last_goal_note, last_check) = {
            let conn = self.conn.lock().unwrap();
            let r: Option<(i64, String)> = conn.query_row(
                "SELECT ts, payload FROM events WHERE project_id = ?1 AND kind = 'alignment_done' ORDER BY id DESC LIMIT 1", params![project_id], |r| Ok((r.get(0)?, r.get(1)?))).ok();
            match r {
                Some((ts, payload)) => {
                    let v: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
                    let g = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
                    (Some(ts), g("meeting_id"), g("goal_note"), g("check"))
                }
                None => (None, None, None, None),
            }
        };
        Ok(AlignConfig { enabled: enabled_raw.map(|e| e != 0).unwrap_or(has_ai_lead), explicit: enabled_raw.is_some(), interval_hours, host_id, host_name, has_ai_lead, last_at, last_meeting_id, last_goal_note, last_check })
    }

    pub fn set_align_config(&self, project_id: &str, enabled: bool, interval_hours: i64, host_id: Option<&str>) -> Result<()> {
        if !INTERVALS.contains(&interval_hours) {
            bail!("间隔只能是 {} 小时之一", INTERVALS.iter().map(|h| h.to_string()).collect::<Vec<_>>().join(" / "));
        }
        if let Some(h) = host_id {
            if !self.is_member(project_id, h)? { bail!("主持人必须是项目成员"); }
        }
        self.conn.lock().unwrap().execute(
            "INSERT INTO align_config (project_id, enabled, interval_hours, host_id) VALUES (?1,?2,?3,?4)
             ON CONFLICT(project_id) DO UPDATE SET enabled = ?2, interval_hours = ?3, host_id = ?4",
            params![project_id, enabled as i64, interval_hours, host_id])?;
        if enabled && self.align_host(project_id)?.is_none() {
            bail!("这个项目没有 AI 负责人，请指定一名成员代为主持（已保存设置，但在指定主持人之前不会开会）");
        }
        Ok(())
    }

    /// 应用启动时：上次被中断、还停在“进行中”的对齐会已经没有人在开了，直接结束，免得一直挂着。返回结束了几场。
    /// 结束前在会议里留一条说明，别让人看到一场空会不知道发生了什么。
    pub fn close_orphaned_standups(&self) -> Result<usize> {
        self.close_orphans("kind = 'standup' AND status = 'open'", "这场对齐会开到一半，应用重启或退出，没有人再接着开，已自动结束。")
    }

    /// 应用启动时：自主推进自动发起的评审会，如果只有那条自动发出的请求、没有任何员工回复，
    /// 说明评审人那一轮被打断了（应用重启或退出），没人会再收尾，直接结束。有员工发言的不动；人手动发起的也不动。返回结束了几场。
    pub fn close_orphaned_reviews(&self) -> Result<usize> {
        self.close_orphans(
            "kind = 'code_review' AND status = 'open'
               AND EXISTS (SELECT 1 FROM meeting_messages m WHERE m.meeting_id = meetings.id AND (m.author_kind = 'autopilot' OR (m.author_kind = 'human' AND m.content LIKE '%这条消息由负责人流程自动发出%')))
               AND NOT EXISTS (SELECT 1 FROM meeting_messages m WHERE m.meeting_id = meetings.id AND m.author_kind = 'employee')",
            "评审人还没来得及发言，这一轮就被打断了（应用重启或退出，或批次被停止；评审人在等模型名额、或在换新会话写交接摘要时也看不到任何动静），没有人再接着评审，已自动结束。这个任务的评审结果以之后重新发起的评审会为准。")
    }

    fn close_orphans(&self, cond: &str, note: &str) -> Result<usize> {
        let ids: Vec<String> = {
            let conn = self.conn.lock().unwrap();
            let mut st = conn.prepare(&format!("SELECT id FROM meetings WHERE {cond}"))?;
            st.query_map([], |r| r.get::<_, String>(0))?.flatten().collect()
        };
        for id in &ids {
            let _ = self.post_message(id, "system", None, note);
            self.conn.lock().unwrap().execute("UPDATE meetings SET status = 'closed', closed_at = ?2 WHERE id = ?1", params![id, now_ms()])?;
        }
        Ok(ids.len())
    }

    /// 该不该开一次对齐会：生效、有主持人、超过间隔，并且上次之后有变化（从没开过则只看间隔）。返回理由。
    pub fn align_due(&self, project_id: &str, now: i64) -> Result<Option<String>> {
        let cfg = self.align_config(project_id)?;
        if !cfg.enabled || cfg.host_name.is_none() {
            return Ok(None);
        }
        let last = cfg.last_at.unwrap_or(0);
        if now - last < cfg.interval_hours * 3_600_000 {
            return Ok(None);
        }
        // 冷却：最近 30 分钟内已经开过（哪怕没开完，比如应用中途被关掉）就不再开，
        // 否则每次重启都会因为“上次没有完成记录”而立刻再开一场
        let recent: i64 = self.conn.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM meetings WHERE project_id = ?1 AND kind = 'standup' AND created_at > ?2", params![project_id, now - 30 * 60_000], |r| r.get(0))?;
        if recent > 0 {
            return Ok(None);
        }
        if last == 0 {
            return Ok(Some("还没开过对齐会".into()));
        }
        let kinds = CHANGE_KINDS.iter().map(|k| format!("'{k}'")).collect::<Vec<_>>().join(",");
        let n: i64 = self.conn.lock().unwrap().query_row(
            &format!("SELECT COUNT(*) FROM events WHERE project_id = ?1 AND ts > ?2 AND kind IN ({kinds})"), params![project_id, last], |r| r.get(0))?;
        Ok((n > 0).then(|| format!("上次之后有 {n} 项变化")))
    }
}

/// 主持人对照目标和计划的判断。
#[derive(Debug, PartialEq)]
pub struct GoalCheck { pub goal_ok: bool, pub note: String, pub new_goal: Option<String>, pub reason: Option<String> }

pub fn parse_goal_check(text: &str) -> Result<GoalCheck> {
    let (a, b) = (text.find('{'), text.rfind('}'));
    let (Some(a), Some(b)) = (a, b) else { bail!("没有找到 JSON") };
    let v: Value = serde_json::from_str(&text[a..=b]).context("JSON 解析失败")?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(|x| x.trim().to_string()).filter(|x| !x.is_empty());
    let goal_ok = v.get("goal_ok").and_then(Value::as_bool).context("缺少 goal_ok")?;
    let note = s("note").context("缺少 note")?;
    if !goal_ok && (s("new_goal").is_none() || s("reason").is_none()) {
        bail!("目标不对时必须给出修改后的完整目标和理由");
    }
    Ok(GoalCheck { goal_ok, note, new_goal: s("new_goal"), reason: s("reason") })
}

impl Orchestrator {
    /// 主持人的目标检查：读目标、计划进度和本次对齐会的记录，判断目标是否还对得上现实。
    /// 返回写进会议的一句话。目标不对且主持人是项目负责人时，直接提交“目标变更提议”（人点头才生效）。
    pub async fn align_goal_check<A: Approver>(&self, meeting_id: &str, host: &Employee, approver: &A) -> Result<String> {
        let meeting = self.db.get_meeting(meeting_id)?.context("会议不存在")?;
        let project = self.db.get_project(&meeting.project_id)?.context("项目不存在")?;
        let is_lead = self.db.list_members(&project.id)?.iter().any(|m| m.employee_id == host.id && m.role == "lead");
        let msgs = self.db.meeting_messages(meeting_id, 0)?;
        let transcript = crate::meeting::format_messages(&msgs, &|id| id.and_then(|i| self.db.employee_by_id(i).ok().flatten()).map(|e| e.name).unwrap_or_else(|| "系统".into()), 6000);
        let plan = self.db.plan_view(&project.id)?;
        let live = self.db.open_actions_for_project(&project.id)?.len();
        let prompt = format!(
            "[对齐会·目标检查] 你是项目「{}」的{}。这是一次对齐会，请对照项目目标判断：目标和现在的计划、实际做的事还对得上吗？\n\n项目目标：\n{}\n\n计划（完成 {}%）：\n{}\n还有 {live} 条未处理的行动项。\n\n本次对齐会的记录：\n{transcript}\n\n\
             判断原则：只有目标本身过时或做不到才算不对——会议里出现了需求变化、目标里的约束在现实中行不通。成员做的事偏离了目标，说明工作要纠正而不是目标要改：此时 goal_ok 填 true，在 note 里点明谁偏离了什么（纪要会把它变成行动项）。进度慢、文件冲突、有行动项都不算目标不对。目标没写（为空）就算对，note 里提醒应该写。\n\
             只输出一个 JSON：{{\"goal_ok\":true,\"note\":\"一句话结论\"}}；目标不对时：{{\"goal_ok\":false,\"note\":\"一句话说明哪里不符\",\"new_goal\":\"修改后的完整目标与约束（替换全文，2000 字内）\",\"reason\":\"为什么改（400 字内）\"}}",
            project.name, if is_lead { "负责人" } else { "受托主持人（你不是项目负责人，不能提议改目标，只需指出）" },
            if project.goal.trim().is_empty() { "（空）" } else { project.goal.trim() }, plan.percent_done, crate::planning::plan_table(&plan.items));
        let text = self.lead_turn_as("meeting", host, &project.id, &prompt, "对齐会目标检查", approver).await?;
        let c = parse_goal_check(&text)?;
        let line = if c.goal_ok {
            format!("目标检查（{}）：目标仍然适用。{}", host.name, c.note)
        } else if is_lead {
            self.db.propose_goal_change(&project.id, &host.id, c.new_goal.as_deref().unwrap_or(""), c.reason.as_deref().unwrap_or(""))?;
            format!("目标检查（{}）：{} 已提议修改目标，等人确认，确认前目标不变。理由：{}", host.name, c.note, c.reason.unwrap_or_default())
        } else {
            format!("目标检查（{}，受托主持）：{} 建议修改目标：{}（需要项目负责人或你来提）", host.name, c.note, c.new_goal.unwrap_or_default().chars().take(300).collect::<String>())
        };
        self.db.post_message(meeting_id, "system", None, &line)?;
        Ok(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::now_ms;

    fn setup(with_lead: bool) -> (Db, String, String, String) {
        let db = Db::open_memory().unwrap();
        let alice = db.add_employee("alice", "全栈", "p").unwrap();
        let bob = db.add_employee("bob", "全栈", "p").unwrap();
        let p = db.create_project("P", "", "/r").unwrap();
        if with_lead { db.add_member(&p.id, &alice.id, "lead").unwrap(); } else { db.add_member(&p.id, &alice.id, "developer").unwrap(); }
        db.add_member(&p.id, &bob.id, "developer").unwrap();
        (db, p.id, alice.id, bob.id)
    }

    #[test]
    fn default_is_on_with_an_ai_lead_and_off_when_the_human_leads() {
        let (db, p, alice, _) = setup(true);
        let c = db.align_config(&p).unwrap();
        assert_eq!((c.enabled, c.explicit, c.has_ai_lead, c.host_name.as_deref(), c.interval_hours), (true, false, true, Some("alice"), 12));
        let (db, p, alice, bob) = setup(false);
        let c = db.align_config(&p).unwrap();
        assert_eq!((c.enabled, c.has_ai_lead, c.host_name), (false, false, None), "人当负责人默认关闭");
        // 打开但没有主持人：保存了，但提示要指定，也不会到点
        assert!(db.set_align_config(&p, true, 12, None).is_err());
        assert!(db.align_config(&p).unwrap().enabled);
        assert_eq!(db.align_due(&p, now_ms() + 10_000_000_000).unwrap(), None, "没有主持人不会开");
        // 指定主持人后生效；主持人必须是成员
        db.set_align_config(&p, true, 12, Some(&bob)).unwrap();
        assert_eq!(db.align_config(&p).unwrap().host_name.as_deref(), Some("bob"));
        assert!(db.align_due(&p, now_ms()).unwrap().is_some());
        let stranger = db.add_employee("zed", "x", "p").unwrap();
        assert!(db.set_align_config(&p, true, 12, Some(&stranger.id)).is_err());
        assert!(db.set_align_config(&p, true, 5, Some(&alice)).is_err(), "间隔只能取预设值");
        // 显式关闭优先于默认
        let (db, p, ..) = setup(true);
        db.set_align_config(&p, false, 24, None).unwrap();
        let c = db.align_config(&p).unwrap();
        assert_eq!((c.enabled, c.explicit, c.interval_hours), (false, true, 24));
        assert_eq!(db.align_due(&p, now_ms() + 10_000_000_000).unwrap(), None);
    }

    #[test]
    fn due_needs_the_interval_to_pass_and_something_to_have_changed() {
        let (db, p, ..) = setup(true);
        let now = now_ms();
        assert!(db.align_due(&p, now).unwrap().is_some(), "从没开过：到点就开");
        db.append_event_for(Some(&p), "alignment_done", None, None, &serde_json::json!({"meeting_id": "m1", "goal_note": "目标仍然适用", "check": "ok"})).unwrap();
        let c = db.align_config(&p).unwrap();
        assert_eq!((c.last_meeting_id.as_deref(), c.last_goal_note.as_deref()), (Some("m1"), Some("目标仍然适用")));
        let h = 3_600_000;
        assert_eq!(db.align_due(&p, now_ms() + 5 * h).unwrap(), None, "间隔内不开");
        assert_eq!(db.align_due(&p, now_ms() + 13 * h).unwrap(), None, "过了间隔但没有变化：不开，不花额度");
        db.append_event_for(Some(&p), "task_merged", None, None, &serde_json::json!({})).unwrap();
        assert!(db.align_due(&p, now_ms() + 13 * h).unwrap().unwrap().contains("1 项变化"));
    }

    #[test]
    fn goal_check_parsing() {
        let c = parse_goal_check("好的\n{\"goal_ok\":true,\"note\":\"仍然适用\"}").unwrap();
        assert!(c.goal_ok && c.note == "仍然适用");
        let c = parse_goal_check("{\"goal_ok\":false,\"note\":\"需求变了\",\"new_goal\":\"新目标\",\"reason\":\"r\"}").unwrap();
        assert_eq!((c.goal_ok, c.new_goal.as_deref()), (false, Some("新目标")));
        for bad in ["没有 json", "{\"goal_ok\":false,\"note\":\"x\"}", "{\"note\":\"x\"}", "{\"goal_ok\":true}"] { assert!(parse_goal_check(bad).is_err(), "{bad}"); }
    }

    #[test]
    fn an_interrupted_meeting_does_not_make_the_next_start_open_another_one() {
        let (db, p, alice, _) = setup(true);
        let now = now_ms();
        assert!(db.align_due(&p, now).unwrap().is_some(), "从没开过：到点就开");
        // 开了一场但应用中途被关掉：没有完成记录，也不能一重启就再开一场
        let m = db.create_meeting(&p, "standup", "对齐会", Some(&alice), None, &[], 4).unwrap();
        assert_eq!(db.align_due(&p, now + 5 * 60_000).unwrap(), None, "30 分钟冷却");
        // 重启时，停在进行中的对齐会被结束
        assert_eq!(db.close_orphaned_standups().unwrap(), 1);
        assert_eq!(db.get_meeting(&m.id).unwrap().unwrap().status, "closed");
        assert_eq!(db.close_orphaned_standups().unwrap(), 0);
        assert!(db.align_due(&p, now + 40 * 60_000).unwrap().is_some(), "冷却过后又可以开");
    }

    #[test]
    fn interrupted_auto_reviews_with_no_reply_are_closed_on_startup_but_real_ones_are_kept() {
        let (db, p, alice, bob) = setup(true);
        let auto = "@Ollama-3-QA 请评审改动。（这条消息由负责人流程自动发出）";
        // 1) 自动发起、评审人没来得及回复：被打断的
        let orphan = db.create_meeting(&p, "code_review", "评审：A", Some(&alice), None, &[], 4).unwrap();
        db.post_message(&orphan.id, "human", None, auto).unwrap();
        // 1b) 新数据里自动发出的请求作者是 autopilot
        let orphan2 = db.create_meeting(&p, "code_review", "评审：A2", Some(&alice), None, &[], 4).unwrap();
        db.post_message(&orphan2.id, "autopilot", None, "@Ollama-3-QA 请评审改动。").unwrap();
        // 2) 自动发起、评审人已经回复了：有内容，不动
        let replied = db.create_meeting(&p, "code_review", "评审：B", Some(&alice), None, &[], 4).unwrap();
        db.post_message(&replied.id, "human", None, auto).unwrap();
        db.post_message(&replied.id, "employee", Some(&bob), "有条件通过").unwrap();
        // 3) 人手动发起的评审，还没人回复：不是自动发的，不动
        let manual = db.create_meeting(&p, "code_review", "评审：C", Some(&alice), None, &[], 4).unwrap();
        db.post_message(&manual.id, "human", None, "@bob 帮我看看这个改动").unwrap();
        // 4) 别的类型的会议，哪怕有同样的消息，也不动
        let design = db.create_meeting(&p, "design", "方案讨论", Some(&alice), None, &[], 4).unwrap();
        db.post_message(&design.id, "human", None, auto).unwrap();

        assert_eq!(db.close_orphaned_reviews().unwrap(), 2);
        let said = db.meeting_messages(&orphan.id, 0).unwrap();
        assert!(said.iter().any(|m| m.author_kind == "system" && m.content.contains("被打断")), "空会结束前要留下说明：{said:?}");
        let st = |id: &str| db.get_meeting(id).unwrap().unwrap().status;
        assert_eq!((st(&orphan.id).as_str(), st(&replied.id).as_str(), st(&manual.id).as_str(), st(&design.id).as_str()), ("closed", "open", "open", "open"));
        assert_eq!(db.close_orphaned_reviews().unwrap(), 0, "再跑一次没有新的");
    }
}
