//! Bug 栏目：独立的 bug 单，有自己的生命周期。
//!
//! - 谁记：QA（团队工具 `report_bug`，在测试那一轮里顺手记，不额外调用模型）或人（Bug 栏目的表单）；
//! - 谁修：默认是做了「出自哪个条目」的那位员工（`origin_item_id` → 条目的任务 → 员工）；没写出处或那人已不在项目里，留空，由负责人排期时指派；
//! - 记下之后，代码（不调用模型）立刻：按严重程度给默认优先级（P0..P3），建一个**修复条目**（指派给责任开发）和一个**复测条目**
//!   （依赖修复条目；**有 QA 时给 QA，没有 QA 时给负责人**），两个条目都进「未排期」，由排期决定顺序；
//! - 状态：`open`（已确认，等修）→ `fixing`（修复条目开始）→ `verify`（修复条目完成，待复测）→ `closed`（复测条目完成）；
//!   复测没通过用 `reopen_bug`：回到 `fixing`，并建新的修复条目和复测条目；出口还有 `cannot_repro`、`duplicate`。

use crate::db::{Db, now_ms};
use crate::plan::NewPlanItem;
use anyhow::{Context, Result, bail};
use rusqlite::params;
use serde::Serialize;
use serde_json::json;

pub const SEVERITIES: &[(&str, &str)] = &[("blocker", "阻断（线上不可用 / 数据错误，要马上修）"), ("major", "严重（主要功能受影响）"), ("minor", "一般（有影响但有绕过办法）")];
pub const BUG_STATUSES: &[&str] = &["open", "fixing", "verify", "closed", "cannot_repro", "duplicate"];

pub const FIX_ACCEPTANCE: &str = "按报告里的步骤，修复前能复现、修复后不再出现；新增或更新了会在修复前失败、修复后通过的自动化测试（确实无法自动化测试的，在说明里写清原因和手工验证步骤）；报告里「不应改变的行为」仍然正常；原有测试仍然全部通过";
pub const RETEST_ACCEPTANCE: &str = "按 bug 报告的复现步骤在最新代码上验证已修复，并检查「不应改变的行为」没有被破坏；通过则条目完成；没通过用 reopen_bug 说明原因";

#[derive(Debug, Clone, Default)]
pub struct NewBug {
    pub title: String,
    /// blocker | major | minor
    pub severity: String,
    pub steps: String,
    pub expected: String,
    pub actual: String,
    /// 不应改变的行为（回归范围）
    pub unchanged: String,
    pub extra: String,
    /// human | qa | employee
    pub source: String,
    pub reporter_id: Option<String>,
    /// 出自哪个计划条目
    pub origin_item_id: Option<String>,
    /// 手动指定的责任开发（不指定则取出处条目的开发）
    pub owner_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Bug {
    pub id: String,
    pub project_id: String,
    pub seq: i64,
    pub title: String,
    pub severity: String,
    /// 0..3（P0..P3）
    pub priority: i64,
    pub steps: String,
    pub expected: String,
    pub actual: String,
    pub unchanged: String,
    pub extra: String,
    pub source: String,
    pub reporter_id: Option<String>,
    pub reporter_name: Option<String>,
    pub origin_item_id: Option<String>,
    pub origin_title: Option<String>,
    pub owner_id: Option<String>,
    pub owner_name: Option<String>,
    pub status: String,
    pub resolution: String,
    pub fix_item_id: Option<String>,
    pub retest_item_id: Option<String>,
    /// 修复条目、复测条目现在的状态（待开始 / 进行中 / 已完成……）
    pub fix_status: Option<String>,
    pub retest_status: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

pub fn default_priority(severity: &str) -> i64 {
    match severity { "blocker" => 0, "major" => 1, _ => 2 }
}

fn fix_level(severity: &str) -> i64 {
    match severity { "blocker" => 4, "major" => 3, _ => 2 }
}

/// 把 bug 单整理成条目说明。
pub fn bug_description(seq: i64, b: &NewBug) -> String {
    let sev = SEVERITIES.iter().find(|(k, _)| *k == b.severity).map(|(_, v)| *v).unwrap_or("未标注");
    let mut s = format!("【BUG-{seq}】严重程度：{sev}\n");
    for (label, v) in [("复现步骤", &b.steps), ("期望行为", &b.expected), ("实际行为", &b.actual), ("不应改变的行为（回归范围）", &b.unchanged), ("补充信息（版本 / 环境 / 日志）", &b.extra)] {
        if !v.trim().is_empty() { s.push_str(&format!("{label}：{}\n", v.trim())); }
    }
    s.trim_end().to_string()
}

const BUG_COLS: &str = "b.id, b.project_id, b.seq, b.title, b.severity, b.priority, b.steps, b.expected, b.actual, b.unchanged, b.extra, b.source, b.reporter_id,
    (SELECT name FROM employees WHERE id = b.reporter_id), b.origin_item_id, (SELECT title FROM plan_items WHERE id = b.origin_item_id), b.owner_id,
    (SELECT name FROM employees WHERE id = b.owner_id), b.status, b.resolution, b.fix_item_id, b.retest_item_id,
    (SELECT status FROM plan_items WHERE id = b.fix_item_id), (SELECT status FROM plan_items WHERE id = b.retest_item_id), b.created_at, b.updated_at";

fn row_to_bug(r: &rusqlite::Row<'_>) -> rusqlite::Result<Bug> {
    Ok(Bug {
        id: r.get(0)?, project_id: r.get(1)?, seq: r.get(2)?, title: r.get(3)?, severity: r.get(4)?, priority: r.get(5)?, steps: r.get(6)?, expected: r.get(7)?,
        actual: r.get(8)?, unchanged: r.get(9)?, extra: r.get(10)?, source: r.get(11)?, reporter_id: r.get(12)?, reporter_name: r.get(13)?, origin_item_id: r.get(14)?,
        origin_title: r.get(15)?, owner_id: r.get(16)?, owner_name: r.get(17)?, status: r.get(18)?, resolution: r.get(19)?, fix_item_id: r.get(20)?, retest_item_id: r.get(21)?,
        fix_status: r.get(22)?, retest_status: r.get(23)?, created_at: r.get(24)?, updated_at: r.get(25)?,
    })
}

impl Db {
    pub fn get_bug(&self, id: &str) -> Result<Option<Bug>> {
        Ok(self.conn.lock().unwrap().query_row(&format!("SELECT {BUG_COLS} FROM bugs b WHERE b.id = ?1"), [id], row_to_bug).ok())
    }

    pub fn list_bugs(&self, project_id: &str) -> Result<Vec<Bug>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!("SELECT {BUG_COLS} FROM bugs b WHERE b.project_id = ?1 ORDER BY b.seq DESC"))?;
        let rows = stmt.query_map([project_id], row_to_bug)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// 出处条目的开发：条目的任务的员工，其次条目的指派；不在项目里（移出、归档）就是 None。
    fn developer_of_item(&self, project_id: &str, item_id: &str) -> Result<Option<String>> {
        let Some(it) = self.get_plan_item(item_id)? else { return Ok(None) };
        let by_task = match &it.task_id { Some(t) => self.get_task(t)?.map(|t| t.employee_id), None => None };
        let Some(emp) = by_task.or(it.assignee_id) else { return Ok(None) };
        Ok(match self.employee_by_id(&emp)? { Some(e) if !e.archived && self.is_member(project_id, &e.id)? => Some(e.id), _ => None })
    }

    /// 复测人：有 QA 给 QA，没有 QA 给负责人；都没有则不指定。
    fn retester(&self, project_id: &str) -> Result<Option<String>> {
        let ms = self.list_members(project_id)?;
        Ok(ms.iter().find(|m| m.role == "qa").or_else(|| ms.iter().find(|m| m.role == "lead")).map(|m| m.employee_id.clone()))
    }

    fn make_bug_items(&self, bug: &Bug, label: &str) -> Result<(String, String)> {
        let desc = bug_description(bug.seq, &NewBug { title: bug.title.clone(), severity: bug.severity.clone(), steps: bug.steps.clone(), expected: bug.expected.clone(), actual: bug.actual.clone(), unchanged: bug.unchanged.clone(), extra: bug.extra.clone(), ..Default::default() });
        let fix = self.create_plan_item(&bug.project_id, &NewPlanItem {
            title: format!("修复：{}{label}", bug.title), description: desc.clone(), acceptance: FIX_ACCEPTANCE.into(), kind: "bugfix".into(), module: String::new(),
            assignee_id: bug.owner_id.clone(), deps: vec![],
        }, None)?;
        self.set_plan_item_level(&fix, fix_level(&bug.severity))?;
        let retest = self.create_plan_item(&bug.project_id, &NewPlanItem {
            title: format!("复测：{}{label}", bug.title), description: desc, acceptance: RETEST_ACCEPTANCE.into(), kind: "retest".into(), module: String::new(),
            assignee_id: self.retester(&bug.project_id)?, deps: vec![fix.clone()],
        }, None)?;
        for id in [&fix, &retest] {
            self.conn.lock().unwrap().execute("UPDATE plan_items SET bug_id = ?2, source = 'bug', schedule_state = 'unscheduled' WHERE id = ?1", params![id, bug.id])?;
        }
        self.set_plan_note(&fix, &format!("由 BUG-{} 产生：未排期，排期时按优先级 P{} 安排", bug.seq, bug.priority))?;
        self.set_plan_note(&retest, &format!("BUG-{} 的复测：修复完成后做", bug.seq))?;
        Ok((fix, retest))
    }

    /// 记一个 bug：建 bug 单、修复条目、复测条目（都不调用模型）。返回 bug id。
    pub fn report_bug(&self, project_id: &str, b: &NewBug) -> Result<String> {
        if b.title.trim().is_empty() { bail!("请写一句话说明是什么问题"); }
        if b.steps.trim().is_empty() && b.actual.trim().is_empty() { bail!("请至少写明复现步骤或实际看到的现象"); }
        let severity = if b.severity.is_empty() { "major" } else { b.severity.as_str() };
        if !SEVERITIES.iter().any(|(k, _)| *k == severity) { bail!("未知的严重程度：{severity}"); }
        let source = if b.source.is_empty() { "human" } else { b.source.as_str() };
        if !["human", "qa", "employee"].contains(&source) { bail!("未知的来源：{source}"); }
        self.get_project(project_id)?.context("项目不存在")?;
        if let Some(o) = &b.origin_item_id {
            let it = self.get_plan_item(o)?.context("出处条目不存在")?;
            if it.project_id != project_id { bail!("出处条目不属于这个项目"); }
        }
        let owner = match &b.owner_id {
            Some(o) => { if !self.is_member(project_id, o)? { bail!("责任开发必须是项目成员"); } Some(o.clone()) }
            None => match &b.origin_item_id { Some(o) => self.developer_of_item(project_id, o)?, None => None },
        };
        let seq: i64 = self.conn.lock().unwrap().query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM bugs WHERE project_id = ?1", [project_id], |r| r.get(0))?;
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_ms();
        self.conn.lock().unwrap().execute(
            "INSERT INTO bugs (id, project_id, seq, title, severity, priority, steps, expected, actual, unchanged, extra, source, reporter_id, origin_item_id, owner_id, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?16)",
            params![id, project_id, seq, b.title.trim(), severity, default_priority(severity), b.steps.trim(), b.expected.trim(), b.actual.trim(), b.unchanged.trim(), b.extra.trim(), source, b.reporter_id, b.origin_item_id, owner, now])?;
        let bug = self.get_bug(&id)?.context("bug 单没有写进去")?;
        let (fix, retest) = self.make_bug_items(&bug, "")?;
        self.conn.lock().unwrap().execute("UPDATE bugs SET fix_item_id = ?2, retest_item_id = ?3 WHERE id = ?1", params![id, fix, retest])?;
        self.append_event_for(Some(project_id), "bug_reported", b.reporter_id.as_deref(), None, &json!({"bug_id": id, "seq": seq, "severity": severity, "title": b.title.trim(), "text": format!("BUG-{seq}（P{}）：{}", default_priority(severity), b.title.trim())}))?;
        Ok(id)
    }

    /// 人或负责人调优先级。
    pub fn set_bug_priority(&self, id: &str, priority: i64) -> Result<()> {
        if !(0..=3).contains(&priority) { bail!("优先级只能是 0–3（P0–P3）"); }
        self.conn.lock().unwrap().execute("UPDATE bugs SET priority = ?2, updated_at = ?3 WHERE id = ?1", params![id, priority, now_ms()])?;
        Ok(())
    }

    /// 改责任开发（同时改还没开始的修复条目的指派）。
    pub fn set_bug_owner(&self, id: &str, owner_id: &str) -> Result<()> {
        let b = self.get_bug(id)?.context("bug 不存在")?;
        if !self.is_member(&b.project_id, owner_id)? { bail!("责任开发必须是项目成员"); }
        self.conn.lock().unwrap().execute("UPDATE bugs SET owner_id = ?2, updated_at = ?3 WHERE id = ?1", params![id, owner_id, now_ms()])?;
        if let Some(f) = &b.fix_item_id {
            self.conn.lock().unwrap().execute("UPDATE plan_items SET assignee_id = ?2 WHERE id = ?1 AND status IN ('planned','proposed')", params![f, owner_id])?;
        }
        Ok(())
    }

    /// 关闭为「无法复现」或「重复」（必须写原因；重复的写明和哪个重复），并取消还没做的修复 / 复测条目。
    pub fn resolve_bug_without_fix(&self, id: &str, status: &str, reason: &str) -> Result<()> {
        if !["cannot_repro", "duplicate"].contains(&status) { bail!("只能标为无法复现（cannot_repro）或重复（duplicate）"); }
        if reason.trim().is_empty() { bail!("必须写原因"); }
        let b = self.get_bug(id)?.context("bug 不存在")?;
        if matches!(b.status.as_str(), "closed" | "cannot_repro" | "duplicate") { bail!("这个 bug 已经结束了"); }
        for it in [&b.fix_item_id, &b.retest_item_id].into_iter().flatten() {
            if self.get_plan_item(it)?.is_some_and(|i| matches!(i.status.as_str(), "planned" | "proposed" | "blocked")) { self.set_plan_status(it, "cancelled", "bug 已结束，不用做了")?; }
        }
        self.conn.lock().unwrap().execute("UPDATE bugs SET status = ?2, resolution = ?3, updated_at = ?4 WHERE id = ?1", params![id, status, reason.trim(), now_ms()])?;
        Ok(())
    }

    /// 复测没通过：回到修复中，建新的修复条目和复测条目。
    pub fn reopen_bug(&self, id: &str, reason: &str) -> Result<()> {
        if reason.trim().is_empty() { bail!("请写明复测发现了什么（现象、步骤）"); }
        let b = self.get_bug(id)?.context("bug 不存在")?;
        if !matches!(b.status.as_str(), "verify" | "closed" | "fixing") { bail!("这个 bug 现在是「{}」，只有修复后（待验证 / 已关闭）才能重开", b.status); }
        let n: i64 = self.conn.lock().unwrap().query_row("SELECT COUNT(*) FROM plan_items WHERE bug_id = ?1 AND kind = 'bugfix'", [id], |r| r.get(0))?;
        let mut again = b.clone();
        again.actual = format!("{}\n复测未通过：{}", b.actual.trim(), reason.trim()).trim().to_string();
        let (fix, retest) = self.make_bug_items(&again, &format!("（第 {} 次）", n + 1))?;
        self.conn.lock().unwrap().execute("UPDATE bugs SET status = 'fixing', fix_item_id = ?2, retest_item_id = ?3, actual = ?4, resolution = '', updated_at = ?5 WHERE id = ?1", params![id, fix, retest, again.actual, now_ms()])?;
        self.append_event_for(Some(&b.project_id), "bug_reopened", None, None, &json!({"bug_id": id, "text": format!("BUG-{} 复测未通过，重新修复：{}", b.seq, reason.trim())}))?;
        Ok(())
    }

    /// 条目开始：bug 的修复条目开始 → 修复中。
    pub(crate) fn on_item_started(&self, item_id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE bugs SET status = 'fixing', updated_at = ?2 WHERE fix_item_id = ?1 AND status = 'open'", params![item_id, now_ms()])?;
        Ok(())
    }

    /// 条目完成：修复条目完成 → 待验证；复测条目完成 → 已关闭。
    pub(crate) fn on_item_done(&self, item_id: &str) -> Result<()> {
        let now = now_ms();
        self.conn.lock().unwrap().execute("UPDATE bugs SET status = 'verify', updated_at = ?2 WHERE fix_item_id = ?1 AND status IN ('open','fixing')", params![item_id, now])?;
        let closed: Vec<(String, String, i64, String)> = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare("SELECT id, project_id, seq, title FROM bugs WHERE retest_item_id = ?1 AND status IN ('verify','fixing','open')")?;
            stmt.query_map([item_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<std::result::Result<_, _>>()?
        };
        for (id, pid, seq, title) in closed {
            self.conn.lock().unwrap().execute("UPDATE bugs SET status = 'closed', updated_at = ?2 WHERE id = ?1", params![id, now])?;
            self.append_event_for(Some(&pid), "bug_closed", None, None, &json!({"bug_id": id, "text": format!("BUG-{seq} 复测通过，已关闭：{title}")}))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (Db, String, String, String, String) {
        let db = Db::open_memory().unwrap();
        let p = db.create_project("p", "", "/tmp/p-bugs").unwrap().id;
        let lead = db.add_employee("lead", "全栈", "x").unwrap();
        let dev = db.add_employee("dev", "全栈", "x").unwrap();
        let qa = db.add_employee("qa", "QA", "x").unwrap();
        db.add_member(&p, &lead.id, "lead").unwrap();
        db.add_member(&p, &dev.id, "developer").unwrap();
        db.add_member(&p, &qa.id, "qa").unwrap();
        (db, p, lead.id, dev.id, qa.id)
    }

    fn bug(title: &str) -> NewBug { NewBug { title: title.into(), steps: "点导出".into(), expected: "生成文件".into(), actual: "闪退".into(), severity: "major".into(), ..Default::default() } }

    #[test]
    fn validation_and_default_priority_follow_severity() {
        let (db, p, ..) = setup();
        assert!(db.report_bug(&p, &NewBug { title: "  ".into(), ..Default::default() }).is_err());
        assert!(db.report_bug(&p, &NewBug { title: "崩".into(), ..Default::default() }).is_err(), "没有步骤也没有现象");
        assert!(db.report_bug(&p, &NewBug { severity: "huge".into(), ..bug("崩") }).is_err());
        for (sev, pr) in [("blocker", 0), ("major", 1), ("minor", 2)] {
            let id = db.report_bug(&p, &NewBug { severity: sev.into(), ..bug(sev) }).unwrap();
            assert_eq!(db.get_bug(&id).unwrap().unwrap().priority, pr);
        }
        assert_eq!(db.list_bugs(&p).unwrap().iter().map(|b| b.seq).collect::<Vec<_>>(), vec![3, 2, 1], "编号按项目递增，列表新的在前");
    }

    #[test]
    fn a_bug_creates_an_unscheduled_fix_for_the_original_developer_and_a_retest_for_qa() {
        let (db, p, lead, dev, qa) = setup();
        // 出处条目：dev 做的
        let origin = db.create_plan_item(&p, &NewPlanItem { title: "实现导出".into(), kind: "migrate".into(), assignee_id: Some(dev.clone()), ..Default::default() }, None).unwrap();
        let id = db.report_bug(&p, &NewBug { origin_item_id: Some(origin.clone()), source: "qa".into(), reporter_id: Some(qa.clone()), unchanged: "已有的导入仍正常".into(), ..bug("导出崩溃") }).unwrap();
        let b = db.get_bug(&id).unwrap().unwrap();
        assert_eq!((b.status.as_str(), b.owner_id.as_deref(), b.source.as_str(), b.origin_title.as_deref()), ("open", Some(dev.as_str()), "qa", Some("实现导出")));
        let items = db.plan_view(&p).unwrap().items;
        let fix = items.iter().find(|i| Some(&i.id) == b.fix_item_id.as_ref()).unwrap();
        let retest = items.iter().find(|i| Some(&i.id) == b.retest_item_id.as_ref()).unwrap();
        assert_eq!((fix.kind.as_str(), fix.assignee_id.as_deref(), fix.schedule_state.as_str(), fix.source.as_str(), fix.bug_id.as_str()), ("bugfix", Some(dev.as_str()), "unscheduled", "bug", id.as_str()));
        assert_eq!((retest.kind.as_str(), retest.assignee_id.as_deref(), retest.schedule_state.as_str()), ("retest", Some(qa.as_str()), "unscheduled"), "有 QA 由 QA 复测");
        assert_eq!(retest.deps, vec![fix.id.clone()], "复测依赖修复");
        assert!(fix.description.contains("不应改变的行为") && fix.description.contains("BUG-1") && fix.level == 3);
        // 没有 QA：负责人复测
        let (db2, p2, lead2, ..) = { let d = Db::open_memory().unwrap(); let p = d.create_project("q", "", "/tmp/q-bugs").unwrap().id; let l = d.add_employee("lead", "全栈", "x").unwrap(); d.add_member(&p, &l.id, "lead").unwrap(); (d, p, l.id, ()) };
        let id2 = db2.report_bug(&p2, &bug("崩")).unwrap();
        let b2 = db2.get_bug(&id2).unwrap().unwrap();
        assert!(b2.owner_id.is_none(), "没写出处就没有默认责任人，由负责人排期时指派");
        let r2 = db2.get_plan_item(b2.retest_item_id.as_ref().unwrap()).unwrap().unwrap();
        assert_eq!(r2.assignee_id.as_deref(), Some(lead2.as_str()), "没有 QA 由负责人复测");
        let _ = lead;
    }

    #[test]
    fn the_original_developer_must_still_be_a_member() {
        let (db, p, _, dev, _) = setup();
        let origin = db.create_plan_item(&p, &NewPlanItem { title: "x".into(), assignee_id: Some(dev.clone()), ..Default::default() }, None).unwrap();
        db.remove_member(&p, &dev).unwrap();
        let id = db.report_bug(&p, &NewBug { origin_item_id: Some(origin), ..bug("崩") }).unwrap();
        assert!(db.get_bug(&id).unwrap().unwrap().owner_id.is_none());
    }

    #[test]
    fn lifecycle_open_fixing_verify_closed_and_reopen() {
        let (db, p, ..) = setup();
        let id = db.report_bug(&p, &bug("导出崩溃")).unwrap();
        let b = db.get_bug(&id).unwrap().unwrap();
        let (fix, retest) = (b.fix_item_id.clone().unwrap(), b.retest_item_id.clone().unwrap());
        db.on_item_started(&fix).unwrap();
        assert_eq!(db.get_bug(&id).unwrap().unwrap().status, "fixing");
        db.set_plan_status(&fix, "done", "").unwrap();
        assert_eq!(db.get_bug(&id).unwrap().unwrap().status, "verify", "修复完成 → 待验证");
        // 复测没通过：重开
        assert!(db.reopen_bug(&id, "  ").is_err());
        db.reopen_bug(&id, "空列表仍然崩溃").unwrap();
        let b2 = db.get_bug(&id).unwrap().unwrap();
        assert_eq!(b2.status, "fixing");
        assert_ne!(b2.retest_item_id.as_deref(), Some(retest.as_str()), "新的复测条目");
        assert!(b2.actual.contains("复测未通过：空列表仍然崩溃"));
        assert!(db.get_plan_item(b2.fix_item_id.as_ref().unwrap()).unwrap().unwrap().title.contains("第 2 次"));
        // 旧的复测条目完成不能关闭 bug（它已经被新的取代）
        db.set_plan_status(&retest, "done", "").unwrap();
        assert_eq!(db.get_bug(&id).unwrap().unwrap().status, "fixing");
        // 第二轮修好并复测通过 → 关闭
        db.set_plan_status(b2.fix_item_id.as_ref().unwrap(), "done", "").unwrap();
        db.set_plan_status(b2.retest_item_id.as_ref().unwrap(), "done", "").unwrap();
        assert_eq!(db.get_bug(&id).unwrap().unwrap().status, "closed");
    }

    #[test]
    fn cannot_repro_and_duplicate_need_a_reason_and_cancel_the_open_items() {
        let (db, p, ..) = setup();
        let id = db.report_bug(&p, &bug("崩")).unwrap();
        assert!(db.resolve_bug_without_fix(&id, "cannot_repro", " ").is_err());
        assert!(db.resolve_bug_without_fix(&id, "closed", "x").is_err());
        db.resolve_bug_without_fix(&id, "duplicate", "和 BUG-0 重复").unwrap();
        let b = db.get_bug(&id).unwrap().unwrap();
        assert_eq!((b.status.as_str(), b.fix_status.as_deref(), b.retest_status.as_deref()), ("duplicate", Some("cancelled"), Some("cancelled")));
        assert!(db.resolve_bug_without_fix(&id, "cannot_repro", "x").is_err(), "已结束");
    }

    #[test]
    fn owner_and_priority_can_be_changed() {
        let (db, p, lead, dev, _) = setup();
        let id = db.report_bug(&p, &bug("崩")).unwrap();
        assert!(db.set_bug_priority(&id, 4).is_err());
        db.set_bug_priority(&id, 0).unwrap();
        assert_eq!(db.get_bug(&id).unwrap().unwrap().priority, 0);
        db.set_bug_owner(&id, &dev).unwrap();
        let b = db.get_bug(&id).unwrap().unwrap();
        assert_eq!(db.get_plan_item(b.fix_item_id.as_ref().unwrap()).unwrap().unwrap().assignee_id.as_deref(), Some(dev.as_str()));
        assert!(db.set_bug_owner(&id, "nobody").is_err());
        let _ = lead;
    }
}
