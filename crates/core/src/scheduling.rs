//! 排期：把「未排期」的条目排进时间线。
//!
//! - **触发**：存在未排期条目、且有人空闲时，自主推进问负责人一次，把当前全部未排期的东西（新条目、bug 的修复和复测、
//!   定时器产生的检查、评审意见里“以后再说”的事、待评估的需求）一起交给它；没有未排期的就不问。一次调用处理全部，不是每件事一次。
//! - **负责人的输出**：条目的先后顺序、额外的前置、指派、原因；对待评估的需求给出采纳 / 搁置 / 拒绝；可取消没价值的未排期条目。
//!   格式不对最多重试 3 次，仍不行或没有 AI 负责人时，按**代码规则**排（`rule_schedule`），不会卡住。
//! - **规则**：P0 的 bug 修复插到所有未开始条目的最前面，P1 排在普通条目之前，其余按原顺序追加；复测靠依赖排在修复之后。
//! - **执行**：自主推进只派「已排期」的条目；每次排入、重排都记进 `schedule_entries`，排期表页面据此显示“什么时候、因为什么被排进来”。
//! - 已有的条目（计划页里人或负责人建的）默认就是「已排期」，所以老项目的行为不变。

use crate::db::{Db, Employee, now_ms};
use crate::orchestrator::Orchestrator;
use crate::plan::PlanItem;
use crate::runner::Approver;
use anyhow::{Context, Result, bail};
use rusqlite::params;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;

pub const MAX_ITEMS_PER_CALL: usize = 40;

#[derive(Debug, Clone, PartialEq)]
pub struct SchedOp {
    pub item: String,
    pub after: Vec<String>,
    pub assignee: Option<String>,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReqDecision { pub req: String, pub decision: String, pub reason: String }

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SchedPlan {
    pub order: Vec<SchedOp>,
    pub requirements: Vec<ReqDecision>,
    pub cancel: Vec<(String, String)>,
    pub summary: String,
}

/// 解析负责人的排期结果。引用写成 U1（未排期）/ S1（已排期未开始）/ R1（待评估需求）。
pub fn parse_schedule(text: &str) -> Result<SchedPlan> {
    let (a, b) = (text.find('{').context("输出里没有 JSON")?, text.rfind('}').context("输出里没有 JSON")?);
    if b <= a { bail!("输出里没有完整的 JSON"); }
    let v: Value = serde_json::from_str(&text[a..=b]).context("JSON 无法解析")?;
    let s = |x: &Value, k: &str| x[k].as_str().unwrap_or("").trim().to_string();
    let mut out = SchedPlan { summary: s(&v, "summary"), ..Default::default() };
    for it in v["schedule"].as_array().into_iter().flatten() {
        let item = s(it, "ref");
        if item.is_empty() { bail!("schedule 里的每项都要有 ref（如 U1）"); }
        out.order.push(SchedOp {
            item,
            after: it["after"].as_array().into_iter().flatten().filter_map(|x| x.as_str()).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect(),
            assignee: Some(s(it, "assignee")).filter(|a| !a.is_empty() && a != "null"),
            note: s(it, "note"),
        });
    }
    for r in v["requirements"].as_array().into_iter().flatten() {
        let decision = s(r, "decision");
        if !["adopt", "hold", "reject"].contains(&decision.as_str()) { bail!("需求 {} 的 decision 只能是 adopt / hold / reject", s(r, "ref")); }
        let reason = s(r, "reason");
        if decision != "adopt" && reason.is_empty() { bail!("需求 {} 搁置或拒绝必须写原因", s(r, "ref")); }
        out.requirements.push(ReqDecision { req: s(r, "ref"), decision, reason });
    }
    for c in v["cancel"].as_array().into_iter().flatten() {
        let reason = s(c, "reason");
        if reason.is_empty() { bail!("取消 {} 必须写原因", s(c, "ref")); }
        out.cancel.push((s(c, "ref"), reason));
    }
    if v.get("schedule").is_none() && v.get("requirements").is_none() && v.get("cancel").is_none() { bail!("缺少 schedule / requirements / cancel"); }
    Ok(out)
}

#[derive(Debug, Clone, Default)]
pub struct SchedOutcome {
    pub scheduled: usize,
    pub reordered: usize,
    pub by_rule: usize,
    pub cancelled: Vec<String>,
    pub adopted: Vec<String>,
    pub decided: Vec<String>,
    pub rejected: Vec<String>,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TimelineRow {
    pub item: PlanItem,
    /// 最近一次排入 / 重排的时间、触发方式、原因
    pub scheduled_at: Option<i64>,
    pub trigger: Option<String>,
    pub reason: Option<String>,
    /// 完成时间（已完成的条目最后更新时间）
    pub finished_at: Option<i64>,
    pub requirement_title: Option<String>,
    pub bug_label: Option<String>,
    pub bug_priority: Option<i64>,
    /// 前置条目排在它下面（顺序有问题）的标题；空 = 顺序没问题。自主推进不受影响（前置没做完就不会开始），只是表看起来不对
    pub deps_below: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TimelineView {
    pub scheduled: Vec<TimelineRow>,
    pub unscheduled: Vec<TimelineRow>,
    /// 需求池里待评估的需求数
    pub pending_requirements: usize,
}

impl Db {
    pub fn set_schedule_state(&self, id: &str, state: &str) -> Result<()> {
        if !["scheduled", "unscheduled"].contains(&state) { bail!("排期状态只能是 scheduled 或 unscheduled"); }
        self.conn.lock().unwrap().execute("UPDATE plan_items SET schedule_state = ?2, updated_at = ?3 WHERE id = ?1", params![id, state, now_ms()])?;
        Ok(())
    }

    /// 把条目标成「未排期」并记下来源（bug / schedule / review / requirement）。
    pub fn mark_unscheduled(&self, id: &str, source: &str) -> Result<()> {
        self.conn.lock().unwrap().execute("UPDATE plan_items SET schedule_state = 'unscheduled', source = ?2, updated_at = ?3 WHERE id = ?1", params![id, source, now_ms()])?;
        Ok(())
    }

    fn set_plan_item_assignee(&self, id: &str, emp: &str) -> Result<()> {
        self.conn.lock().unwrap().execute("UPDATE plan_items SET assignee_id = ?2, updated_at = ?3 WHERE id = ?1", params![id, emp, now_ms()])?;
        Ok(())
    }

    pub fn record_schedule(&self, project_id: &str, item_id: &str, action: &str, trigger: &str, reason: &str, by: Option<&str>) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO schedule_entries (project_id, item_id, action, trigger_kind, reason, by_employee, at) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![project_id, item_id, action, trigger, reason, by, now_ms()])?;
        Ok(())
    }

    /// 还要排的条目：未排期、还没开始（待开始）。
    pub fn unscheduled_items(&self, project_id: &str) -> Result<Vec<PlanItem>> {
        Ok(self.plan_view(project_id)?.items.into_iter().filter(|i| i.schedule_state == "unscheduled" && i.status == "planned").collect())
    }

    /// 重排：给这些条目按给定顺序重新分配它们占用的那几个位置（其他条目的相对位置不变）。
    fn reorder_items(&self, ids: &[String]) -> Result<()> {
        let view = self.plan_view(&self.get_plan_item(ids.first().context("没有条目")?)?.context("条目不存在")?.project_id)?;
        let mut slots: Vec<i64> = ids.iter().filter_map(|id| view.items.iter().find(|i| &i.id == id).map(|i| i.position)).collect();
        slots.sort();
        // 位置有重复时（规则插队产生的）拉开，避免多个条目共用一个位置
        for k in 1..slots.len() { if slots[k] <= slots[k - 1] { slots[k] = slots[k - 1] + 1; } }
        let conn = self.conn.lock().unwrap();
        for (id, pos) in ids.iter().zip(slots) { conn.execute("UPDATE plan_items SET position = ?2 WHERE id = ?1", params![id, pos])?; }
        Ok(())
    }

    /// 代码规则排期（负责人没回复可用的结果、或项目没有 AI 负责人时）。只处理 `only` 里的条目（None = 全部未排期）。返回排了几个。
    pub fn rule_schedule(&self, project_id: &str, only: Option<&[String]>) -> Result<usize> {
        let view = self.plan_view(project_id)?;
        let prio_of = |it: &PlanItem| -> Option<i64> { if it.kind == "bugfix" && !it.bug_id.is_empty() { self.get_bug(&it.bug_id).ok().flatten().map(|b| b.priority) } else { None } };
        let mut todo: Vec<&PlanItem> = view.items.iter().filter(|i| i.schedule_state == "unscheduled" && matches!(i.status.as_str(), "planned" | "proposed" | "blocked") && only.map(|o| o.contains(&i.id)).unwrap_or(true)).collect();
        todo.sort_by_key(|i| i.position);
        // 最终顺序：P0 修复 → P1 修复 → 已排期未开始的（原顺序）→ 其余新来的（按原顺序追加）；整体占用这些条目原有的位置
        let (mut p0, mut p1, mut rest): (Vec<&PlanItem>, Vec<&PlanItem>, Vec<&PlanItem>) = (vec![], vec![], vec![]);
        for it in &todo { match prio_of(it) { Some(0) => p0.push(it), Some(1) => p1.push(it), _ => rest.push(it) } }
        let existing: Vec<&PlanItem> = view.items.iter().filter(|i| i.schedule_state == "scheduled" && i.status == "planned").collect();
        let order: Vec<String> = p0.iter().chain(p1.iter()).chain(existing.iter()).chain(rest.iter()).map(|i| i.id.clone()).collect();
        if order.len() > 1 { self.reorder_items(&order)?; }
        let mut n = 0;
        for it in &todo {
            self.set_schedule_state(&it.id, "scheduled")?;
            self.record_schedule(project_id, &it.id, "排入", "规则兜底", &match prio_of(it) { Some(p) => format!("bug 优先级 P{p}，按规则排入"), None => "按计划顺序追加".into() }, None)?;
            n += 1;
        }
        self.fix_order_for_deps(project_id)?;
        Ok(n)
    }

    /// 人把两个条目的先后互换（排期表里的上移 / 下移）。
    pub fn swap_plan_positions(&self, a: &str, b: &str) -> Result<()> {
        let (x, y) = (self.get_plan_item(a)?.context("条目不存在")?, self.get_plan_item(b)?.context("条目不存在")?);
        if x.project_id != y.project_id { bail!("两个条目不在同一个项目里"); }
        let (px, py) = if x.position == y.position { (x.position, y.position + 1) } else { (x.position, y.position) };
        {
            let conn = self.conn.lock().unwrap();
            conn.execute("UPDATE plan_items SET position = ?2 WHERE id = ?1", params![a, py])?;
            conn.execute("UPDATE plan_items SET position = ?2 WHERE id = ?1", params![b, px])?;
        }
        self.record_schedule(&x.project_id, a, "人调整", "人调整", &format!("和《{}》互换了先后", y.title), None)
    }

    /// 保证先后顺序和依赖一致：已排期、还没开始的条目里，前置一定排在后置前面（稳定排序：没有依赖冲突的条目保持原来的先后）。
    /// 返回位置变了的条目数。
    pub fn fix_order_for_deps(&self, project_id: &str) -> Result<usize> {
        let view = self.plan_view(project_id)?;
        let items: Vec<&PlanItem> = view.items.iter().filter(|i| i.schedule_state == "scheduled" && i.status == "planned").collect();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        let mut out: Vec<&PlanItem> = Vec::with_capacity(items.len());
        let mut placed: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut rest: Vec<&PlanItem> = items.clone();
        while !rest.is_empty() {
            // 取最靠前的、所有（在这批里的）前置都已放好的条目；有环（理论上不会）时按原顺序放，避免死循环
            let k = rest.iter().position(|i| i.deps.iter().all(|d| !ids.contains(&d.as_str()) || placed.contains(d.as_str()))).unwrap_or(0);
            let it = rest.remove(k);
            placed.insert(it.id.as_str());
            out.push(it);
        }
        let moved = out.iter().zip(items.iter()).filter(|(a, b)| a.id != b.id).count();
        if moved > 0 {
            let order: Vec<String> = out.iter().map(|i| i.id.clone()).collect();
            self.reorder_items(&order)?;
        }
        Ok(moved)
    }

    /// 人把条目拖回未排期。
    pub fn unschedule_item(&self, id: &str) -> Result<()> {
        let it = self.get_plan_item(id)?.context("条目不存在")?;
        if it.status != "planned" && it.status != "proposed" { bail!("只有还没开始的条目才能退回未排期"); }
        self.set_schedule_state(id, "unscheduled")?;
        self.record_schedule(&it.project_id, id, "退回未排期", "人调整", "", None)
    }

    /// 人把未排期的条目直接排入（追加在已排期的最后，之后可以用上移 / 下移调整）。
    pub fn schedule_item_by_human(&self, id: &str) -> Result<()> {
        let it = self.get_plan_item(id)?.context("条目不存在")?;
        self.set_schedule_state(id, "scheduled")?;
        self.record_schedule(&it.project_id, id, "排入", "人调整", "", None)
    }

    pub fn timeline_view(&self, project_id: &str) -> Result<TimelineView> {
        let view = self.plan_view(project_id)?;
        let entries: HashMap<String, (i64, String, String)> = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare("SELECT item_id, at, trigger_kind, reason FROM schedule_entries WHERE project_id = ?1 ORDER BY id")?;
            let rows = stmt.query_map([project_id], |r| Ok((r.get::<_, String>(0)?, (r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?.into_iter().collect()
        };
        let updated: HashMap<String, i64> = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare("SELECT id, updated_at FROM plan_items WHERE project_id = ?1")?;
            let rows = stmt.query_map([project_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?.into_iter().collect()
        };
        let reqs: HashMap<String, String> = self.list_specs(project_id)?.into_iter().map(|v| (v.spec.id, v.spec.title)).collect();
        let bugs: HashMap<String, (i64, i64)> = self.list_bugs(project_id)?.into_iter().map(|b| (b.id, (b.seq, b.priority))).collect();
        let pos: HashMap<String, (i64, &str)> = view.items.iter().map(|i| (i.id.clone(), (i.position, i.status.as_str()))).collect();
        let title: HashMap<String, String> = view.items.iter().map(|i| (i.id.clone(), i.title.clone())).collect();
        let (mut scheduled, mut unscheduled) = (vec![], vec![]);
        for it in view.items.clone() {
            if it.status == "cancelled" { continue; }
            let e = entries.get(&it.id);
            let row = TimelineRow {
                scheduled_at: e.map(|e| e.0), trigger: e.map(|e| e.1.clone()), reason: e.map(|e| e.2.clone()).filter(|r| !r.is_empty()),
                finished_at: if it.status == "done" { updated.get(&it.id).copied() } else { None },
                requirement_title: reqs.get(&it.requirement_id).cloned(),
                bug_label: bugs.get(&it.bug_id).map(|b| format!("BUG-{}", b.0)), bug_priority: bugs.get(&it.bug_id).map(|b| b.1),
                deps_below: it.deps.iter().filter_map(|d| pos.get(d.as_str()).filter(|(p, st)| *p > it.position && !matches!(*st, "done" | "cancelled")).map(|_| title.get(d.as_str()).cloned().unwrap_or_default())).collect(),
                item: it,
            };
            if row.item.schedule_state == "unscheduled" && row.item.status != "done" { unscheduled.push(row); } else { scheduled.push(row); }
        }
        let pending_requirements = self.list_specs(project_id)?.iter().filter(|v| v.spec.status == "pending").count();
        Ok(TimelineView { scheduled, unscheduled, pending_requirements })
    }
}

fn clip(s: &str, n: usize) -> String { s.chars().take(n).collect() }

impl Orchestrator {
    /// 让负责人给全部未排期的条目（和待评估的需求）排期。没有未排期的条目、也没有待评估的需求时返回 None（不调用模型）。
    /// 负责人回复不可用时按代码规则排（`by_rule`），保证调用返回后没有条目还留在未排期。
    pub async fn lead_schedule<A: Approver>(&self, project_id: &str, lead: &Employee, approver: &A) -> Result<Option<SchedOutcome>> {
        let mut unsched = self.db.unscheduled_items(project_id)?;
        let pending: Vec<_> = self.db.list_specs(project_id)?.into_iter().filter(|v| v.spec.status == "pending").map(|v| v.spec).collect();
        if unsched.is_empty() && pending.is_empty() { return Ok(None); }
        unsched.truncate(MAX_ITEMS_PER_CALL);
        let project = self.db.get_project(project_id)?.context("项目不存在")?;
        let members = self.db.list_members(project_id)?;
        let view = self.db.plan_view(project_id)?;
        let waiting: Vec<&PlanItem> = view.items.iter().filter(|i| i.schedule_state == "scheduled" && i.status == "planned").collect();
        let doing: Vec<&PlanItem> = view.items.iter().filter(|i| i.status == "in_progress").collect();
        let title_of = |id: &str| view.items.iter().find(|i| i.id == id).map(|i| i.title.clone()).unwrap_or_else(|| "?".into());
        let mut refs: HashMap<String, String> = HashMap::new();
        let mut p = format!("[排期] 你是项目「{}」的负责人。下面是还没排期的事，请决定先后顺序。\\n", project.name).replace("\\n", "\n");
        if !project.goal.trim().is_empty() { p.push_str(&format!("\n项目目标与硬性约束：\n{}\n", clip(project.goal.trim(), 1500))); }
        p.push_str("\n【未排期】（引用 U1…；这是要你排的）：\n");
        for (i, it) in unsched.iter().enumerate() {
            let r = format!("U{}", i + 1);
            let bug = if it.bug_id.is_empty() { String::new() } else { self.db.get_bug(&it.bug_id)?.map(|b| format!("，BUG-{} 优先级 P{}", b.seq, b.priority)).unwrap_or_default() };
            p.push_str(&format!("{r}. [{}·来源 {}{bug}] {}{}{} — {}\n", it.kind, if it.source.is_empty() { "计划" } else { &it.source }, it.title,
                it.assignee_name.as_deref().map(|n| format!("（指派 {n}）")).unwrap_or_default(), if it.level > 0 { format!("（{} 级）", it.level) } else { String::new() }, clip(it.description.trim(), 160).replace('\n', " ")));
            refs.insert(r, it.id.clone());
        }
        if !waiting.is_empty() {
            p.push_str("\n【已排期、还没开始】（引用 S1…；按现在的先后顺序；你可以调整它们的顺序）：\n");
            for (i, it) in waiting.iter().enumerate() {
                let r = format!("S{}", i + 1);
                p.push_str(&format!("{r}. [{}] {}{}{}\n", it.kind, it.title, it.assignee_name.as_deref().map(|n| format!("（{n}）")).unwrap_or_default(),
                    if it.deps.is_empty() { String::new() } else { format!("（等：{}）", it.deps.iter().map(|d| title_of(d)).collect::<Vec<_>>().join("、")) }));
                refs.insert(r, it.id.clone());
            }
        }
        if !doing.is_empty() {
            p.push_str("\n【正在做】（不能动，只作参考，避免同一个人排两件事）：\n");
            for it in &doing { p.push_str(&format!("- {}（{}）\n", it.title, it.assignee_name.as_deref().unwrap_or("未指派"))); }
        }
        if !pending.is_empty() {
            p.push_str("\n【需求池里待评估的需求】（引用 R1…；采纳后会开始写规格）：\n");
            for (i, r) in pending.iter().enumerate() {
                let key = format!("R{}", i + 1);
                p.push_str(&format!("{key}. 《{}》[{}] {}{}\n", r.title, if r.rtype == "tech" { "技术改进" } else { "功能" }, clip(r.brief.trim(), 200).replace('\n', " "),
                    if r.human_tech.trim().is_empty() { String::new() } else { format!("（人指定的技术要求：{}）", clip(r.human_tech.trim(), 120)) }));
                refs.insert(key, r.id.clone());
            }
        }
        p.push_str("\n团队成员：\n");
        for m in &members { p.push_str(&format!("- {}（{}，项目角色 {}{}）\n", m.name, m.title, m.role, self.db.cap_suffix(&m.employee_id))); }
        p.push_str("\n排期原则：\n\
- 顺序就是执行顺序，靠前的先做；有前置的在 after 里写它依赖的引用（U / S 都可以），前置没做完就不会开始。\n\
- bug 的修复默认由出问题的开发修（已指派的不要改，除非那人明显过载）；P0 要插到最前，P1 尽量靠前；复测排在修复之后（已有依赖）。\n\
- 一个人同一时间只做一件事，别把太多事都排给同一个人；互不依赖的事可以并行。\n\
- 没价值或重复的未排期条目可以取消（来源是评审意见的尤其要清理），必须写原因。\n\
- 待评估的需求：值得做的 adopt；现在不做的 hold 并写原因；不该做的 reject 并写原因。人指定的技术要求必须保留。\n\
- 没写到的未排期条目会按计划顺序追加在最后。\n");
        p.push_str("只输出一个 JSON 对象，不要任何其他文字：\n{\"schedule\":[{\"ref\":\"U1\",\"after\":[\"S2\"],\"assignee\":\"成员名或 null\",\"note\":\"一句话原因\"}],\"requirements\":[{\"ref\":\"R1\",\"decision\":\"adopt\",\"reason\":\"\"}],\"cancel\":[{\"ref\":\"U3\",\"reason\":\"...\"}],\"summary\":\"一句话\"}");
        let ids_in_call: Vec<String> = unsched.iter().map(|i| i.id.clone()).collect();
        let parsed = self.lead_turn_parsed("planning", lead, project_id, &p, "排期", approver, parse_schedule).await;
        let mut out = SchedOutcome::default();
        let plan = match parsed {
            Ok(Ok(plan)) => plan,
            Ok(Err(why)) => {
                out.rejected.push(format!("负责人的排期结果{why}，改按代码规则排"));
                out.by_rule = self.db.rule_schedule(project_id, Some(&ids_in_call))?;
                self.finish_schedule_event(project_id, lead, &out)?;
                return Ok(Some(out));
            }
            Err(e) => {
                // 调用本身失败（模型没回复）：也按规则排，不让条目一直留在未排期
                out.rejected.push(format!("排期调用失败（{e}），改按代码规则排"));
                out.by_rule = self.db.rule_schedule(project_id, Some(&ids_in_call))?;
                self.finish_schedule_event(project_id, lead, &out)?;
                return Ok(Some(out));
            }
        };
        out.summary = plan.summary.clone();
        // 需求评估
        for d in &plan.requirements {
            let Some(rid) = refs.get(&d.req).filter(|_| d.req.starts_with('R')) else { out.rejected.push(format!("需求引用 {} 不存在", d.req)); continue };
            let title = pending.iter().find(|r| &r.id == rid).map(|r| r.title.clone()).unwrap_or_default();
            let r = match d.decision.as_str() {
                "adopt" => self.db.adopt_requirement(rid, &format!("负责人（{}）评估采纳：{}", lead.name, d.reason)).map(|_| out.adopted.push(title.clone())),
                dec => self.db.decide_requirement(rid, if dec == "hold" { "held" } else { "rejected" }, &d.reason).map(|_| out.decided.push(format!("《{title}》{}", if dec == "hold" { "搁置" } else { "拒绝" }))),
            };
            if let Err(e) = r { out.rejected.push(format!("需求《{title}》：{e}")); }
        }
        // 取消（只能是本次的未排期条目）
        for (r, reason) in &plan.cancel {
            let Some(id) = refs.get(r).filter(|_| r.starts_with('U')) else { out.rejected.push(format!("取消 {r}：只能取消未排期的条目（U…）")); continue };
            match self.db.set_plan_status(id, "cancelled", &format!("排期时取消：{reason}")) {
                Ok(()) => out.cancelled.push(title_of(id)),
                Err(e) => out.rejected.push(format!("取消 {r}：{e}")),
            }
        }
        // 顺序、前置、指派
        let mut order: Vec<String> = vec![];
        for op in &plan.order {
            let Some(id) = refs.get(&op.item).filter(|_| op.item.starts_with('U') || op.item.starts_with('S')) else { out.rejected.push(format!("排期引用 {} 不存在", op.item)); continue };
            if order.contains(id) { out.rejected.push(format!("{} 重复出现", op.item)); continue; }
            let cur = self.db.get_plan_item(id)?.context("条目不存在")?;
            if cur.status != "planned" { out.rejected.push(format!("{}：条目现在是「{}」，不能排", op.item, crate::plan::status_label(&cur.status))); continue; }
            if !op.after.is_empty() {
                let mut deps = cur.deps.clone();
                for a in &op.after {
                    match refs.get(a).filter(|_| a.starts_with('U') || a.starts_with('S')) {
                        Some(d) if d != id => { if !deps.contains(d) { deps.push(d.clone()); } }
                        _ => out.rejected.push(format!("{}：前置 {a} 不存在或是自己", op.item)),
                    }
                }
                if deps != cur.deps { if let Err(e) = self.db.set_plan_deps(id, &deps) { out.rejected.push(format!("{}：{e}", op.item)); } }
            }
            if let Some(name) = &op.assignee {
                match members.iter().find(|m| &m.name == name) {
                    Some(m) if m.role != "architect" || cur.kind == crate::design::DESIGN_KIND || cur.kind == crate::maintenance::SPEC_KIND => { if cur.assignee_id.as_deref() != Some(m.employee_id.as_str()) { self.db.set_plan_item_assignee(id, &m.employee_id)?; } }
                    Some(_) => out.rejected.push(format!("{}：架构师只做设计和规格条目", op.item)),
                    None => out.rejected.push(format!("{}：{name} 不是项目成员", op.item)),
                }
            }
            order.push(id.clone());
            let was_unsched = cur.schedule_state == "unscheduled";
            if was_unsched {
                self.db.set_schedule_state(id, "scheduled")?;
                self.db.record_schedule(project_id, id, "排入", "排期调用", &op.note, Some(&lead.id))?;
                out.scheduled += 1;
            }
        }
        // 先后顺序：被提到的条目按输出顺序占用它们原来的那些位置
        if order.len() > 1 {
            let mut cur: Vec<PlanItem> = order.iter().filter_map(|id| self.db.get_plan_item(id).ok().flatten()).collect();
            cur.sort_by_key(|i| i.position);
            let before: Vec<String> = cur.iter().map(|i| i.id.clone()).collect();
            if before != order {
                self.db.reorder_items(&order)?;
                for id in &order {
                    let from = before.iter().position(|b| b == id);
                    let to = order.iter().position(|b| b == id);
                    if from != to && !ids_in_call.contains(id) { self.db.record_schedule(project_id, id, "重排", "排期调用", "", Some(&lead.id))?; out.reordered += 1; }
                }
            }
        }
        // 没被提到的未排期条目：按规则追加
        let leftover: Vec<String> = ids_in_call.iter().filter(|id| self.db.get_plan_item(id).ok().flatten().is_some_and(|i| i.schedule_state == "unscheduled" && i.status == "planned")).cloned().collect();
        if !leftover.is_empty() { out.by_rule = self.db.rule_schedule(project_id, Some(&leftover))?; }
        // 前置必须排在后置前面（负责人设了前置、但顺序没跟上时由代码纠正）
        out.reordered += self.db.fix_order_for_deps(project_id)?;
        self.finish_schedule_event(project_id, lead, &out)?;
        Ok(Some(out))
    }

    fn finish_schedule_event(&self, project_id: &str, lead: &Employee, o: &SchedOutcome) -> Result<()> {
        let mut parts = vec![];
        if o.scheduled > 0 { parts.push(format!("排入 {} 个", o.scheduled)); }
        if o.reordered > 0 { parts.push(format!("调整 {} 个的顺序", o.reordered)); }
        if o.by_rule > 0 { parts.push(format!("按规则排 {} 个", o.by_rule)); }
        if !o.cancelled.is_empty() { parts.push(format!("取消 {}", o.cancelled.iter().map(|t| format!("《{t}》")).collect::<Vec<_>>().join("、"))); }
        if !o.adopted.is_empty() { parts.push(format!("采纳需求 {}", o.adopted.iter().map(|t| format!("《{t}》")).collect::<Vec<_>>().join("、"))); }
        if !o.decided.is_empty() { parts.push(o.decided.join("、")); }
        self.db.append_event_for(Some(project_id), "plan_scheduled", Some(&lead.id), None, &json!({
            "scheduled": o.scheduled, "reordered": o.reordered, "by_rule": o.by_rule, "rejected": o.rejected,
            "text": format!("{} 排期：{}{}", lead.name, if parts.is_empty() { "没有变化".to_string() } else { parts.join("；") }, if o.summary.is_empty() { String::new() } else { format!("（{}）", clip(&o.summary, 100)) }),
        }))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bugs::NewBug;
    use crate::plan::NewPlanItem;

    fn setup() -> (Db, String) {
        let db = Db::open_memory().unwrap();
        let p = db.create_project("p", "", "/tmp/p-sched").unwrap().id;
        (db, p)
    }

    fn item(db: &Db, p: &str, t: &str) -> String { db.create_plan_item(p, &NewPlanItem { title: t.into(), kind: "migrate".into(), ..Default::default() }, None).unwrap() }

    #[test]
    fn parse_accepts_a_full_answer_and_rejects_broken_ones() {
        let ok = parse_schedule(r#"好的：{"schedule":[{"ref":"U2","after":["S1"],"assignee":"bob","note":"先修"},{"ref":"U1"}],"requirements":[{"ref":"R1","decision":"adopt"},{"ref":"R2","decision":"hold","reason":"现在不做"}],"cancel":[{"ref":"U3","reason":"重复"}],"summary":"排好了"}"#).unwrap();
        assert_eq!((ok.order.len(), ok.requirements.len(), ok.cancel.len(), ok.summary.as_str()), (2, 2, 1, "排好了"));
        assert_eq!(ok.order[0], SchedOp { item: "U2".into(), after: vec!["S1".into()], assignee: Some("bob".into()), note: "先修".into() });
        assert_eq!(ok.order[1].assignee, None);
        assert!(parse_schedule("没有 JSON").is_err());
        assert!(parse_schedule(r#"{"foo":1}"#).is_err());
        assert!(parse_schedule(r#"{"schedule":[{"note":"x"}]}"#).is_err(), "缺 ref");
        assert!(parse_schedule(r#"{"requirements":[{"ref":"R1","decision":"maybe"}]}"#).is_err());
        assert!(parse_schedule(r#"{"requirements":[{"ref":"R1","decision":"reject"}]}"#).is_err(), "拒绝要写原因");
        assert!(parse_schedule(r#"{"cancel":[{"ref":"U1"}]}"#).is_err(), "取消要写原因");
    }

    #[test]
    fn rule_schedule_puts_p0_first_p1_before_normal_items_and_leaves_the_rest_in_order() {
        let (db, p) = setup();
        let a = item(&db, &p, "A"); let b = item(&db, &p, "B");
        let p1 = db.report_bug(&p, &NewBug { title: "严重".into(), actual: "x".into(), severity: "major".into(), ..Default::default() }).unwrap();
        let p0 = db.report_bug(&p, &NewBug { title: "阻断".into(), actual: "x".into(), severity: "blocker".into(), ..Default::default() }).unwrap();
        let p2 = db.report_bug(&p, &NewBug { title: "一般".into(), actual: "x".into(), severity: "minor".into(), ..Default::default() }).unwrap();
        assert_eq!(db.unscheduled_items(&p).unwrap().len(), 6 - 3 + 3, "3 个 bug 各有修复和复测，其中修复和复测都在未排期里：6 个");
        // 6 个未排期：3 修复 + 3 复测
        assert_eq!(db.unscheduled_items(&p).unwrap().len(), 6);
        assert_eq!(db.rule_schedule(&p, None).unwrap(), 6);
        assert!(db.unscheduled_items(&p).unwrap().is_empty());
        let order: Vec<String> = db.plan_view(&p).unwrap().items.into_iter().filter(|i| i.status == "planned").map(|i| i.title).collect();
        let pos = |t: &str| order.iter().position(|x| x == t).unwrap();
        assert!(pos("修复：阻断") < pos("修复：严重") && pos("修复：严重") < pos("A"), "P0 在最前，P1 在普通条目之前：{order:?}");
        assert!(pos("A") < pos("B") && pos("B") < pos("修复：一般"), "P2 按顺序追加在普通条目之后：{order:?}");
        let _ = (a, b, p0, p1, p2);
        // 每个都有排期记录
        let tl = db.timeline_view(&p).unwrap();
        assert!(tl.unscheduled.is_empty() && tl.scheduled.iter().filter(|r| r.trigger.as_deref() == Some("规则兜底")).count() == 6);
        assert!(tl.scheduled.iter().any(|r| r.bug_label.as_deref() == Some("BUG-2") && r.bug_priority == Some(0)));
    }

    #[test]
    fn humans_can_move_items_back_and_forth_and_it_is_recorded() {
        let (db, p) = setup();
        let a = item(&db, &p, "A");
        assert_eq!(db.plan_view(&p).unwrap().items[0].schedule_state, "scheduled", "人建的条目默认已排期");
        db.unschedule_item(&a).unwrap();
        assert_eq!(db.unscheduled_items(&p).unwrap().len(), 1);
        let w = db.plan_waiting(&p, &Default::default(), true).unwrap();
        assert!(w[&a].iter().any(|x| x.contains("未排期")), "{:?}", w[&a]);
        db.schedule_item_by_human(&a).unwrap();
        assert!(db.unscheduled_items(&p).unwrap().is_empty());
        let tl = db.timeline_view(&p).unwrap();
        assert_eq!((tl.scheduled.len(), tl.scheduled[0].trigger.as_deref()), (1, Some("人调整")));
        db.set_plan_status(&a, "in_progress", "").unwrap_err();
    }

    #[test]
    fn prerequisites_are_always_ordered_before_the_items_that_wait_for_them() {
        let (db, p) = setup();
        let ids: Vec<String> = ["A", "B", "C", "D"].iter().map(|t| item(&db, &p, t)).collect();
        // A 等 C，D 等 A：现在的顺序 A B C D 里 C 在 A 下面
        db.set_plan_deps(&ids[0], &[ids[2].clone()]).unwrap();
        db.set_plan_deps(&ids[3], &[ids[0].clone()]).unwrap();
        let tl = db.timeline_view(&p).unwrap();
        assert_eq!(tl.scheduled[0].deps_below, vec!["C".to_string()], "A 的前置 C 排在它下面");
        assert!(tl.scheduled[3].deps_below.is_empty());
        assert_eq!(db.fix_order_for_deps(&p).unwrap(), 3, "三个位置变了：B、C、A");
        let order: Vec<String> = db.plan_view(&p).unwrap().items.into_iter().map(|i| i.title).collect();
        assert_eq!(order, vec!["B", "C", "A", "D"], "稳定排序：A 等 C，所以先放 B、再放 C、再放 A，D 在最后");
        assert!(db.timeline_view(&p).unwrap().scheduled.iter().all(|r| r.deps_below.is_empty()));
        assert_eq!(db.fix_order_for_deps(&p).unwrap(), 0, "已经一致就不动");
        // 已完成的前置不算问题
        db.set_plan_status(&ids[2], "done", "").unwrap();
        db.move_plan_item(&ids[2], 3).unwrap();
        assert!(db.timeline_view(&p).unwrap().scheduled.iter().all(|r| r.deps_below.is_empty()));
    }

    #[test]
    fn reorder_reuses_the_positions_the_items_already_hold() {
        let (db, p) = setup();
        let ids: Vec<String> = ["A", "B", "C", "D"].iter().map(|t| item(&db, &p, t)).collect();
        db.reorder_items(&[ids[2].clone(), ids[0].clone()]).unwrap();
        let order: Vec<String> = db.plan_view(&p).unwrap().items.into_iter().map(|i| i.title).collect();
        assert_eq!(order, vec!["C", "B", "A", "D"], "C 和 A 互换了位置，B、D 不动");
    }
}
