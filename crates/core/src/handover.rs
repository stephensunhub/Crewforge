//! 角色交接：把项目里的某个角色（比如负责人）从一个员工交给另一个。
//!
//! 典型场景：Ollama 效率低，把负责人换成云模型。流程分两步：
//! 1. **准备**（`handover_prepare`）：系统整理项目当前的状态快照（计划、进行中的任务、待处理的议题和行动项、最近的决定、批次状态），
//!    再让原员工续着自己在这个项目里的会话写一份交接摘要（可选，本地模型上要几分钟，最长等 5 分钟，超时就只用快照）。
//!    人可以看、可以改摘要。
//! 2. **执行**（`handover_execute`）：新人接任角色；原员工保留为开发或移出项目；可选把原员工名下还没开始的条目和行动项一并交给新人；
//!    原员工负责的议题、进行中的会议的主持人转给新人；摘要存成一次性说明，新人下一次被调用时放在提示词最前面（用完即清），
//!    同时记一条项目记忆和一条事件留档。
//!
//! 有进行中的任务时不能交接（先合并或放弃）；项目的自主推进在运行时由命令层拦住（先暂停或停止）。

use crate::db::{Db, Employee, PROJECT_ROLES, now_ms};
use crate::memory::{MemoryFilter, NewMemory};
use crate::orchestrator::Orchestrator;
use anyhow::{Context, Result, bail};
use rusqlite::params;
use serde::Serialize;
use serde_json::json;

/// 让原员工写摘要最长等多久。
pub const SUMMARY_TIMEOUT_SECS: u64 = 300;

pub fn role_label(role: &str) -> &'static str {
    match role { "lead" => "负责人", "developer" => "开发", "qa" => "QA", "reviewer" => "审查", "architect" => "架构师", _ => "成员" }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct HandoverPrep {
    /// 系统整理的状态快照
    pub snapshot: String,
    /// 原员工自己写的摘要；没要、写不出来或超时是 None
    pub model_summary: Option<String>,
    /// 需要你知道的提醒（新人的能力上限比原员工低、新人连的是本地模型、原员工还有没开始的活等）
    pub warnings: Vec<String>,
    /// 摘要没写出来的原因（用户选了要但没成）
    pub summary_note: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct HandoverReport {
    pub from: String,
    pub to: String,
    pub role: String,
    pub moved_plan_items: usize,
    pub moved_actions: usize,
    pub moved_issues: usize,
    pub moved_meetings: usize,
    pub warnings: Vec<String>,
}

fn note_key(project_id: &str, employee_id: &str) -> String { format!("handover_note:{project_id}:{employee_id}") }

impl Db {
    /// 取走（并清掉）留给这位员工的一次性交接说明。
    pub fn take_handover_note(&self, project_id: &str, employee_id: &str) -> Option<String> {
        let key = note_key(project_id, employee_id);
        let v = self.get_setting(&key).ok().flatten().filter(|v| !v.trim().is_empty())?;
        let _ = self.set_setting(&key, "");
        Some(v)
    }

    /// 交接前的检查：返回（原员工，新员工）。
    pub fn check_handover(&self, project_id: &str, from_id: &str, to_id: &str, role: &str) -> Result<(Employee, Employee)> {
        if !PROJECT_ROLES.contains(&role) { bail!("未知的项目角色：{role}"); }
        let project = self.get_project(project_id)?.context("项目不存在")?;
        if project.status != "active" { bail!("项目已归档"); }
        if from_id == to_id { bail!("不能交接给自己"); }
        let from = self.employee_by_id(from_id)?.context("原员工不存在")?;
        let to = self.employee_by_id(to_id)?.context("新员工不存在")?;
        if to.archived || to.role == "guest" { bail!("{} 已归档或是顾问，不能接任角色", to.name); }
        let held = self.list_members(project_id)?.into_iter().find(|m| m.employee_id == from_id).map(|m| m.role);
        if held.as_deref() != Some(role) { bail!("{} 在这个项目里不是「{}」", from.name, role_label(role)); }
        if self.active_task(from_id, &project.path)?.is_some() { bail!("{} 在这个项目里有进行中的任务，请先合并或放弃，再交接", from.name); }
        Ok((from, to))
    }

    /// 系统整理的状态快照（只读数据库，不调用模型）。
    pub fn handover_snapshot(&self, project_id: &str, from_id: &str, role: &str) -> Result<String> {
        let project = self.get_project(project_id)?.context("项目不存在")?;
        let from = self.employee_by_id(from_id)?.context("员工不存在")?;
        let mut s = format!("项目「{}」，{} 交出的角色：{}。\n", project.name, from.name, role_label(role));
        if !project.goal.trim().is_empty() {
            s.push_str(&format!("\n【项目目标】\n{}\n", project.goal.trim().chars().take(800).collect::<String>()));
        }
        let view = self.plan_view(project_id)?;
        let c = &view.counts;
        s.push_str(&format!("\n【计划】完成 {}%：已完成 {}、进行中 {}、待开始 {}、已阻塞 {}、待确认 {}\n", view.percent_done, c.done, c.in_progress, c.planned, c.blocked, c.proposed));
        let line = |i: &crate::plan::PlanItem| format!("- {}{}《{}》→ {}{}\n", crate::plan::status_label(&i.status), if i.level > 0 { format!(" L{}", i.level) } else { String::new() }, i.title.chars().take(60).collect::<String>(),
            i.assignee_name.clone().unwrap_or_else(|| "未指定".into()), if i.note.is_empty() { String::new() } else { format!("（{}）", i.note.chars().take(60).collect::<String>()) });
        for i in view.items.iter().filter(|i| matches!(i.status.as_str(), "in_progress" | "blocked")).take(8) { s.push_str(&line(i)); }
        let planned: Vec<_> = view.items.iter().filter(|i| i.status == "planned").collect();
        for i in planned.iter().take(8) { s.push_str(&line(i)); }
        if planned.len() > 8 { s.push_str(&format!("- ……还有 {} 个待开始的条目\n", planned.len() - 8)); }
        let mine = view.items.iter().filter(|i| i.assignee_id.as_deref() == Some(from_id) && matches!(i.status.as_str(), "proposed" | "planned" | "blocked")).count();
        s.push_str(&format!("{} 名下还没开始的条目：{} 个\n", from.name, mine));

        let issues = self.list_issues(project_id)?;
        let open: Vec<_> = issues.iter().filter(|i| matches!(i.status.as_str(), "open" | "discussing" | "pending_decision")).collect();
        s.push_str(&format!("\n【议题】还没结论的 {} 个\n", open.len()));
        for i in open.iter().take(5) { s.push_str(&format!("- 《{}》（{}）\n", i.title.chars().take(60).collect::<String>(), i.status)); }
        let (open_actions, working): (i64, i64) = {
            let conn = self.conn.lock().unwrap();
            (conn.query_row("SELECT COUNT(*) FROM action_items WHERE project_id = ?1 AND status = 'open'", [project_id], |r| r.get(0))?,
             conn.query_row("SELECT COUNT(*) FROM action_items WHERE project_id = ?1 AND status = 'working'", [project_id], |r| r.get(0))?)
        };
        s.push_str(&format!("【行动项】待处理 {open_actions}，处理中 {working}\n"));

        let decisions: Vec<_> = self.list_memories(&MemoryFilter { layer: Some("project".into()), project_id: Some(project_id.into()), ..Default::default() })?
            .into_iter().filter(|m| m.kind == "decision" && m.status != "expired").take(5).collect();
        if !decisions.is_empty() {
            s.push_str("\n【最近的决定】\n");
            for m in decisions { s.push_str(&format!("- {}：{}\n", m.title.chars().take(50).collect::<String>(), m.content.chars().take(100).collect::<String>().replace('\n', " "))); }
        }
        if let Some(r) = self.latest_autopilot_run(project_id)? {
            s.push_str(&format!("\n【自主推进】最近一个批次：{}{}\n", r.status, if r.reason.is_empty() { String::new() } else { format!("（{}）", r.reason.chars().take(80).collect::<String>()) }));
        }
        Ok(s)
    }

    /// 把一个员工在项目里还没开始的活（计划条目、待处理行动项）交给另一个人。返回（条目数，行动项数）。
    fn move_unstarted_work(&self, project_id: &str, from_id: &str, to_id: &str) -> Result<(usize, usize)> {
        let conn = self.conn.lock().unwrap();
        let plans = conn.execute("UPDATE plan_items SET assignee_id = ?3, updated_at = ?4 WHERE project_id = ?1 AND assignee_id = ?2 AND status IN ('proposed','planned','blocked')", params![project_id, from_id, to_id, now_ms()])?;
        let actions = conn.execute("UPDATE action_items SET assignee_id = ?3 WHERE project_id = ?1 AND assignee_id = ?2 AND status = 'open'", params![project_id, from_id, to_id])?;
        Ok((plans, actions))
    }
}

impl Orchestrator {
    /// 准备交接：状态快照，加上（可选）让原员工自己写一份摘要。可能要几分钟，调用方负责在界面上说明。
    pub async fn handover_prepare(&self, project_id: &str, from_id: &str, to_id: &str, role: &str, ask_summary: bool) -> Result<HandoverPrep> {
        let (from, to) = self.db.check_handover(project_id, from_id, to_id, role)?;
        let project = self.db.get_project(project_id)?.context("项目不存在")?;
        let mut prep = HandoverPrep { snapshot: self.db.handover_snapshot(project_id, from_id, role)?, ..Default::default() };
        // 提醒
        let (cf, ct) = (self.db.level_cap(&from)?, self.db.level_cap(&to)?);
        if ct < cf { prep.warnings.push(format!("{} 的能力上限是 {ct} 级，比 {} 的 {cf} 级低：交接后计划里高等级的活可能没人够格", to.name, from.name)); }
        if role == "lead" && to.endpoint_id.is_some() {
            prep.warnings.push(format!("{} 连的是自建端点（本地模型）：负责人要整理行动项、裁决议题、起草计划，本地模型上又慢又容易格式出错，不建议", to.name));
        }
        if role == "lead" && to.provider == "copilot" {
            prep.warnings.push(format!("{} 用的是 Copilot：负责人的工作靠 MCP 团队工具，Copilot 员工是否可用没有验证过", to.name));
        }
        // 摘要
        if ask_summary {
            let session = self.db.get_or_create_session(&from.id, &project.path)?;
            if from.provider != "claude" {
                prep.summary_note = format!("{} 用的是 {}，暂不支持让 TA 自己写摘要，只用系统整理的快照", from.name, from.provider);
            } else if !session.started {
                prep.summary_note = format!("{} 在这个项目里还没有会话（没干过活），没有可以总结的上下文", from.name);
            } else {
                let resolved = self.db.resolve_endpoint(&from)?;
                let (model, env) = match &resolved {
                    Some(r) => (Some(r.model.clone()), r.env.clone()),
                    None => (self.db.effective_model(&from.provider, &from.model, "task")?, vec![]),
                };
                let _slot = match &resolved { Some(r) => Some(crate::endpoint::acquire_slot_with(&r.endpoint, &from.id, 0).await), None => None };
                let prompt = format!("[系统提示] 你在项目「{}」里担任{}，现在要把这个角色交接给 {}。请不要调用任何工具，直接写一份交接摘要，600 字以内，包含：\n\
                    ① 你现在在推进什么、做到哪了；② 已经做出的关键决定和原因；③ 还没决定或待处理的事，以及你的倾向；④ 你对计划、人员、风险的判断；⑤ 接手后第一件事建议做什么。\n只写事实，不要客套。",
                    project.name, role_label(role), to.name);
                match crate::runner::ask_session_once(&from, &self.program, std::path::Path::new(&project.path), &session.id, model.as_deref(), env, &prompt, SUMMARY_TIMEOUT_SECS, 2000).await {
                    Some(t) => prep.model_summary = Some(t),
                    None => prep.summary_note = format!("{} 没能在 {} 分钟内写出摘要（超时或出错），只用系统整理的快照", from.name, SUMMARY_TIMEOUT_SECS / 60),
                }
            }
        }
        Ok(prep)
    }

    /// 执行交接。`summary` 是要留给新人的全部说明（快照 + 摘要，人可能改过）；`after` 是原员工之后的去向：`keep`（保留为开发）或 `remove`（移出项目）；
    /// `move_work` 为 true 时，原员工名下还没开始的条目和行动项一并交给新人。
    pub fn handover_execute(&self, project_id: &str, from_id: &str, to_id: &str, role: &str, summary: &str, after: &str, move_work: bool) -> Result<HandoverReport> {
        if !matches!(after, "keep" | "remove") { bail!("原员工之后的去向只能是 keep 或 remove"); }
        let (from, to) = self.db.check_handover(project_id, from_id, to_id, role)?;
        let mut rep = HandoverReport { from: from.name.clone(), to: to.name.clone(), role: role.into(), ..Default::default() };
        // 1. 新人接任（不在项目里就加入）
        if self.db.is_member(project_id, to_id)? { self.db.set_member_role(project_id, to_id, role)?; } else { self.db.add_member(project_id, to_id, role)?; }
        // 2. 原员工的去向
        if after == "remove" {
            let (p, a) = if move_work { self.db.move_unstarted_work(project_id, from_id, to_id)? } else { (0, 0) };
            rep.moved_plan_items = p; rep.moved_actions = a;
            // 没交接过去的由 remove_member 改回未指定
            self.db.remove_member(project_id, from_id)?;
        } else {
            // 负责人只能有一位，set_member_role 已经把原负责人降为开发；其他角色多人可以并存，原员工也降为开发
            if role != "lead" { self.db.set_member_role(project_id, from_id, "developer")?; }
            if move_work { let (p, a) = self.db.move_unstarted_work(project_id, from_id, to_id)?; rep.moved_plan_items = p; rep.moved_actions = a; }
        }
        // 3. 负责人的附带职责：议题的负责人、进行中的会议的主持人
        if role == "lead" {
            let conn = self.db.conn.lock().unwrap();
            rep.moved_issues = conn.execute("UPDATE issues SET owner_id = ?3, updated_at = ?4 WHERE project_id = ?1 AND owner_id = ?2 AND status IN ('open','discussing','pending_decision')", params![project_id, from_id, to_id, now_ms()])?;
            rep.moved_meetings = conn.execute("UPDATE meetings SET host_id = ?3 WHERE project_id = ?1 AND host_id = ?2 AND status != 'closed'", params![project_id, from_id, to_id])?;
        }
        // 4. 留给新人的一次性说明、项目记忆、事件
        let note = format!("[角色交接] {} 把这个项目的「{}」交接给了你。下面是交接说明，请先读完再开始工作：\n\n{}", from.name, role_label(role), summary.trim().chars().take(4500).collect::<String>());
        self.db.set_setting(&note_key(project_id, to_id), &note)?;
        let _ = self.db.add_memory(NewMemory {
            layer: "project".into(), project_id: Some(project_id.into()), kind: "decision".into(), title: format!("交接：{} → {}（{}）", from.name, to.name, role_label(role)),
            content: summary.trim().chars().take(3500).collect(), scope: "角色交接".into(), source_kind: "handover".into(), created_by: from.name.clone(), status: "verified".into(), ..Default::default()
        });
        let text = format!("{} 把「{}」交接给了 {}{}", from.name, role_label(role), to.name, if rep.moved_plan_items + rep.moved_actions > 0 { format!("；一并交过去 {} 个计划条目、{} 条行动项", rep.moved_plan_items, rep.moved_actions) } else { String::new() });
        self.db.append_event_for(Some(project_id), "role_handover", Some(to_id), None, &json!({"from": from.name, "to": to.name, "role": role, "after": after, "moved_plan_items": rep.moved_plan_items, "moved_actions": rep.moved_actions, "moved_issues": rep.moved_issues, "moved_meetings": rep.moved_meetings, "text": text}))?;
        Ok(rep)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::NewPlanItem;

    fn setup() -> (Db, String, Employee, Employee, Employee) {
        let db = Db::open_memory().unwrap();
        let p = db.create_project("P", "", "/tmp/p-handover").unwrap().id;
        let (old, new, dev) = (db.add_employee("old", "全栈", "x").unwrap(), db.add_employee("new", "全栈", "x").unwrap(), db.add_employee("dev", "全栈", "x").unwrap());
        db.add_member(&p, &old.id, "lead").unwrap();
        db.add_member(&p, &dev.id, "developer").unwrap();
        (db, p, old, new, dev)
    }

    #[test]
    fn checks_reject_bad_handovers() {
        let (db, p, old, new, dev) = setup();
        assert!(db.check_handover(&p, &old.id, &old.id, "lead").unwrap_err().to_string().contains("自己"));
        assert!(db.check_handover(&p, &dev.id, &new.id, "lead").unwrap_err().to_string().contains("不是「负责人」"), "只有当前担任这个角色的人才能交出去");
        assert!(db.check_handover(&p, &old.id, &new.id, "boss").is_err());
        db.set_employee_archived(&new.id, true).unwrap();
        assert!(db.check_handover(&p, &old.id, &new.id, "lead").unwrap_err().to_string().contains("归档"));
    }

    #[test]
    fn snapshot_mentions_plan_and_the_outgoing_employees_unstarted_work() {
        let (db, p, old, _new, _dev) = setup();
        let id = db.create_plan_item(&p, &NewPlanItem { title: "写核心逻辑".into(), assignee_id: Some(old.id.clone()), ..Default::default() }, None).unwrap();
        db.set_plan_item_level(&id, 3).unwrap();
        let s = db.handover_snapshot(&p, &old.id, "lead").unwrap();
        assert!(s.contains("交出的角色：负责人") && s.contains("写核心逻辑") && s.contains("L3") && s.contains("old 名下还没开始的条目：1 个"), "{s}");
    }

    #[test]
    fn one_shot_note_is_taken_once() {
        let db = Db::open_memory().unwrap();
        assert!(db.take_handover_note("p", "e").is_none());
        db.set_setting(&note_key("p", "e"), "说明").unwrap();
        assert_eq!(db.take_handover_note("p", "e").as_deref(), Some("说明"));
        assert!(db.take_handover_note("p", "e").is_none(), "用完即清");
    }
}
