//! 负责人自己起草和调整计划。
//!
//! - 起草：负责人读项目目标、扫描结果、测试基线、评估报告和现有计划，输出一份有依赖关系的计划（JSON），
//!   条目进入“提议中”，由人在“开始自主推进”时确认——人只确认方向，不逐条写。
//! - 调整：自主推进时，条目反复失败后、以及计划做完后做一次完成度复盘，负责人可以追加条目、取消作废的条目、
//!   改派负责人、让失败的条目换个说明重试；每圈有数量上限，已开始和已完成的条目不动；
//!   涉及目标或需求变更时必须升级给人。追加的条目直接进入“待开始”（人在开始时已批准计划的范围）。

use crate::db::Employee;
use crate::onboarding::{summarize_baseline, summarize_scan};
use crate::orchestrator::Orchestrator;
use crate::plan::{NewPlanItem, PlanItem};
use crate::runner::{Approver, NoopObserver, TurnConfig, run_turn};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::Path;

pub const MAX_DRAFT_ITEMS: usize = 15;
/// 解析条目预计会改的文件：去掉空白和 `./` 前缀，去重，最多 8 个。
fn parse_files(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for f in v["files"].as_array().into_iter().flatten().filter_map(|x| x.as_str()) {
        let f = f.trim().trim_start_matches("./").to_string();
        if !f.is_empty() && !out.contains(&f) && out.len() < 8 {
            out.push(f);
        }
    }
    out
}

pub const MAX_ADDED_PER_ADJUST: usize = 3;

#[derive(Debug, Clone, PartialEq)]
pub struct DraftItem {
    pub title: String,
    pub description: String,
    pub acceptance: String,
    pub assignee: Option<String>,
    /// 依赖的条目（本份计划里更靠前的条目的下标，从 0 开始）
    pub depends_on: Vec<usize>,
    /// 预计会改的文件；改同一个文件的条目不会并行
    pub files: Vec<String>,
    /// 等级 1–5，0 = 没写
    pub level: i64,
}

fn json_slice(text: &str) -> Result<Value> {
    let (a, b) = (text.find('{').context("输出里没有 JSON")?, text.rfind('}').context("输出里没有 JSON")?);
    if b <= a {
        bail!("输出里没有完整的 JSON");
    }
    serde_json::from_str(&text[a..=b]).context("JSON 无法解析")
}

fn text_of(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or("").trim().to_string()
}

/// 解析并校验起草结果：1~15 个条目，负责人必须是项目成员，依赖只能指向更靠前的条目（所以不会成环）。
pub fn parse_draft(text: &str, member_names: &[String]) -> Result<Vec<DraftItem>> {
    let v = json_slice(text)?;
    let arr = v["items"].as_array().context("缺少 items 数组")?;
    if arr.is_empty() || arr.len() > MAX_DRAFT_ITEMS {
        bail!("计划条目数必须在 1 到 {MAX_DRAFT_ITEMS} 之间（现在是 {}）", arr.len());
    }
    let mut out = Vec::new();
    for (i, it) in arr.iter().enumerate() {
        let title = text_of(it, "title");
        if title.is_empty() {
            bail!("第 {} 个条目没有标题", i + 1);
        }
        let assignee = Some(text_of(it, "assignee")).filter(|a| !a.is_empty() && a != "null");
        if let Some(a) = &assignee {
            if !member_names.contains(a) {
                bail!("第 {} 个条目的负责人 {a} 不是项目成员", i + 1);
            }
        }
        let mut deps = Vec::new();
        for d in it["depends_on"].as_array().into_iter().flatten() {
            let n = d.as_u64().context("depends_on 必须是数字下标")? as usize;
            if n >= i {
                bail!("第 {} 个条目只能依赖更靠前的条目（下标 {n} 无效）", i + 1);
            }
            if !deps.contains(&n) {
                deps.push(n);
            }
        }
        out.push(DraftItem { title, description: text_of(it, "description"), acceptance: text_of(it, "acceptance"), assignee, depends_on: deps, files: parse_files(it), level: crate::levels::parse_level(it) });
    }
    Ok(out)
}

/// 调整计划的一项改动。条目用计划里的编号 `#n` 指代。
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    Add { title: String, description: String, acceptance: String, assignee: Option<String>, depends_on: Vec<String>, files: Vec<String>, level: i64, reason: String },
    Cancel { item: usize, reason: String },
    Reassign { item: usize, assignee: String },
    Retry { item: usize, note: String },
    /// 给条目定级（接手复盘时用）
    SetLevel { item: usize, level: i64 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Adjustment {
    pub changes: Vec<Change>,
    pub escalate: Option<String>,
    pub summary: String,
}

pub fn parse_adjustment(text: &str) -> Result<Adjustment> {
    let v = json_slice(text)?;
    let escalate = Some(text_of(&v, "escalate")).filter(|e| !e.is_empty() && e != "null");
    let mut changes = Vec::new();
    for c in v["changes"].as_array().into_iter().flatten() {
        let item = c["item"].as_u64().map(|n| n as usize);
        changes.push(match c["op"].as_str() {
            Some("add") => {
                let title = text_of(c, "title");
                let reason = text_of(c, "reason");
                if title.is_empty() || reason.is_empty() {
                    bail!("追加条目必须写标题和原因");
                }
                let deps = c["depends_on"].as_array().into_iter().flatten().filter_map(|d| d.as_str().map(String::from)).collect();
                Change::Add { title, description: text_of(c, "description"), acceptance: text_of(c, "acceptance"), assignee: Some(text_of(c, "assignee")).filter(|a| !a.is_empty() && a != "null"), depends_on: deps, files: parse_files(c), level: crate::levels::parse_level(c), reason }
            }
            Some("cancel") => Change::Cancel { item: item.context("cancel 需要 item")?, reason: Some(text_of(c, "reason")).filter(|r| !r.is_empty()).context("取消必须写原因")? },
            Some("reassign") => Change::Reassign { item: item.context("reassign 需要 item")?, assignee: Some(text_of(c, "assignee")).filter(|a| !a.is_empty()).context("reassign 需要 assignee")? },
            Some("retry") => Change::Retry { item: item.context("retry 需要 item")?, note: Some(text_of(c, "note")).filter(|r| !r.is_empty()).context("重试必须写新的说明")? },
            Some("set_level") => {
                let level = crate::levels::parse_level(c);
                if level == 0 { bail!("set_level 的 level 必须是 1–{}", crate::levels::MAX_LEVEL); }
                Change::SetLevel { item: item.context("set_level 需要 item")?, level }
            }
            other => bail!("未知的改动类型：{other:?}"),
        });
    }
    Ok(Adjustment { changes, escalate, summary: text_of(&v, "summary") })
}

/// 实际应用的结果。
#[derive(Debug, Clone, Default, Serialize)]
pub struct Applied {
    pub added: Vec<String>,
    pub cancelled: Vec<String>,
    pub reassigned: Vec<String>,
    pub retried: Vec<String>,
    /// 接手复盘时定了级的条目（《标题》→ L级）
    pub leveled: Vec<String>,
    /// 被拒绝的改动及原因（超出权限或上限）
    pub rejected: Vec<String>,
    pub escalate: Option<String>,
    pub summary: String,
    /// 被重试的条目 id（调用方要把它们从“已失败”里去掉）
    pub retried_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdjustTrigger {
    /// 某个条目反复失败后：可以追加、取消、改派、重试
    ItemFailed,
    /// 计划做完后的完成度复盘：只能追加
    FinalReview,
    /// 批次开始时清理重复条目：只能取消
    Dedupe,
    /// 刚接任负责人后的第一个批次：给没定级的条目定级、按能力重新分配还没开始的条目：只能定级和改派
    Takeover,
}

/// 疑似重复的条目对（#编号 与 plan_table 一致）：至少有一条还没开始（待开始/已阻塞），标题相似度 ≥ 0.7。
/// 已取消的不算；已完成的可以作为“对照”（待开始的条目和已完成的重复，也是多余的）。
pub fn duplicate_candidates(items: &[PlanItem]) -> Vec<(usize, usize)> {
    let open = |s: &str| matches!(s, "planned" | "blocked");
    let mut out = vec![];
    for (i, a) in items.iter().enumerate() {
        for (j, b) in items.iter().enumerate().skip(i + 1) {
            if a.status == "cancelled" || b.status == "cancelled" || !(open(&a.status) || open(&b.status)) { continue; }
            if crate::mcp::similarity(&a.title, &b.title) >= 0.7 { out.push((i + 1, j + 1)); }
        }
    }
    out
}

fn dedupe_key(project_id: &str) -> String { format!("dedupe_checked:{project_id}") }
fn pair_key(a: &PlanItem, b: &PlanItem) -> String {
    let (x, y) = if a.id <= b.id { (&a.id, &b.id) } else { (&b.id, &a.id) };
    format!("{x}|{y}")
}

impl crate::db::Db {
    /// 疑似重复的条目对里，负责人还没核对过的（核对过的“不是重复”不再问，每次批次开始、应用重启都问一遍要空等十几分钟）。
    pub fn unchecked_duplicate_pairs(&self, project_id: &str, items: &[PlanItem]) -> Vec<(usize, usize)> {
        let done: std::collections::HashSet<String> = self.get_setting(&dedupe_key(project_id)).ok().flatten()
            .and_then(|v| serde_json::from_str::<Vec<String>>(&v).ok()).unwrap_or_default().into_iter().collect();
        duplicate_candidates(items).into_iter().filter(|&(a, b)| !done.contains(&pair_key(&items[a - 1], &items[b - 1]))).collect()
    }

    /// 记下这些条目对负责人已经核对过（取消了的、保留下来的都算）。
    pub fn mark_duplicates_checked(&self, project_id: &str, items: &[PlanItem], pairs: &[(usize, usize)]) -> Result<()> {
        let mut all: Vec<String> = self.get_setting(&dedupe_key(project_id))?.and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default();
        for &(a, b) in pairs {
            let k = pair_key(&items[a - 1], &items[b - 1]);
            if !all.contains(&k) { all.push(k); }
        }
        self.set_setting(&dedupe_key(project_id), &serde_json::to_string(&all)?)
    }
}

pub(crate) fn plan_table(items: &[PlanItem]) -> String {
    items.iter().enumerate().map(|(n, i)| format!("#{} [{}]{} {} → {}{}{}\n", n + 1, crate::plan::status_label(&i.status), if i.level > 0 { format!(" L{}", i.level) } else { String::new() }, i.title, i.assignee_name.clone().unwrap_or_else(|| "未指定".into()),
        if i.waiting_for.is_empty() { String::new() } else { format!("（依赖：{}）", i.waiting_for.join("、")) },
        if i.note.is_empty() { String::new() } else { format!("（备注：{}）", i.note.chars().take(120).collect::<String>()) })).collect()
}

/// 负责人的回复格式不对时，最多重试几次（含第一次）。仍然不对才升级给人。
pub const MAX_FORMAT_ATTEMPTS: usize = 3;

impl Orchestrator {
    async fn lead_turn<A: Approver>(&self, lead: &Employee, project_id: &str, prompt: &str, label: &str, approver: &A) -> Result<String> {
        self.lead_turn_as("planning", lead, project_id, prompt, label, approver).await
    }

    /// 负责人的一次只读调用，`purpose` 决定计费归类和默认模型。
    pub(crate) async fn lead_turn_as<A: Approver>(&self, purpose: &str, lead: &Employee, project_id: &str, prompt: &str, label: &str, approver: &A) -> Result<String> {
        let project = self.db.get_project(project_id)?.context("项目不存在")?;
        let cfg = TurnConfig {
            project_path: Path::new(&project.path).to_path_buf(),
            task_id: None, project_id: None, read_only: true, mcp: None,
            purpose: purpose.into(), attr_project: Some(project_id.into()), meeting_id: None,
            log_prompt: Some(format!("[{label}] {}", project.name)),
            program: self.program.clone(), copilot_program: self.copilot_program.clone(), origin: "system".into(),
        };
        let r = run_turn(&self.db, lead, &cfg, prompt, approver, &NoopObserver).await?;
        if r.is_error || r.text.trim().is_empty() {
            bail!("{} 没有给出有效回复（{}）", lead.name, r.subtype);
        }
        Ok(r.text)
    }

    /// 让负责人答一次并解析；格式不对就把错误原因告诉它、让它重答，最多 `MAX_FORMAT_ATTEMPTS` 次。
    /// 返回 Ok(Ok(结果))；连续都不对返回 Ok(Err(原因))，由调用方决定怎么处理（比如升级给人）。
    /// 调用本身失败（模型没有回复、出错）仍是 Err。
    pub(crate) async fn lead_turn_parsed<T, A: Approver>(
        &self, purpose: &str, lead: &Employee, project_id: &str, prompt: &str, label: &str, approver: &A,
        parse: impl Fn(&str) -> Result<T>,
    ) -> Result<std::result::Result<T, String>> {
        let mut p = prompt.to_string();
        let mut last = String::new();
        for attempt in 1..=MAX_FORMAT_ATTEMPTS {
            let text = self.lead_turn_as(purpose, lead, project_id, &p, label, approver).await?;
            match parse(&text) {
                Ok(v) => return Ok(Ok(v)),
                Err(e) => {
                    last = e.to_string();
                    let _ = self.db.append_event_for(Some(project_id), "lead_format_retry", Some(&lead.id), None,
                        &serde_json::json!({"label": label, "attempt": attempt, "max": MAX_FORMAT_ATTEMPTS, "error": last}));
                    p = format!("{prompt}\n\n[系统提示] 你上一次的回复无法使用：{last}。请重新回答：只输出一个 JSON 对象本身，不要任何其他文字，字段和格式严格按上面的要求。（第 {} / {MAX_FORMAT_ATTEMPTS} 次）", attempt + 1);
                }
            }
        }
        Ok(Err(format!("连续 {MAX_FORMAT_ATTEMPTS} 次回复格式都不对（{last}）")))
    }

    /// 让负责人起草计划。条目进入“提议中”，等人在开始自主推进时确认。返回新增的条目数。
    pub async fn lead_draft_plan<A: Approver>(&self, project_id: &str, lead: &Employee, approver: &A) -> Result<usize> {
        let project = self.db.get_project(project_id)?.context("项目不存在")?;
        let members = self.db.list_members(project_id)?;
        let existing = self.db.plan_view(project_id)?;
        let mut p = format!("[起草计划] 你是项目「{}」的负责人，现在要为项目起草一份可以直接执行的计划。\n", project.name);
        if project.goal.trim().is_empty() {
            bail!("项目还没有写目标。请先在“计划”页写下项目目标与硬性约束，负责人才知道往哪里推进");
        }
        if let crate::design::DesignPhase::NotStarted | crate::design::DesignPhase::Pending(_) = self.db.design_phase(project_id)? {
            bail!("项目里有架构师，要等设计做完再起草实现计划。点“开始自主推进”，架构师会先做设计，做完后负责人自动按设计文档起草计划");
        }
        p.push_str(&format!("\n项目目标与硬性约束：\n{}\n", project.goal.trim()));
        p.push_str(&self.design_docs_block(project_id).await);
        if let Some(scan) = self.db.latest_scan(project_id)? {
            p.push_str(&format!("\n项目扫描结果：\n{}\n", summarize_scan(&scan)));
        }
        if let Some(b) = self.db.current_baseline(project_id)? {
            p.push_str(&format!("\n测试基线：\n{}\n", summarize_baseline(&b)));
        }
        for r in self.db.latest_reports(project_id)? {
            p.push_str(&format!("\n{}评估报告（节选）：\n{}\n", if r.kind == "architect" { "全栈" } else { "QA" }, r.content.chars().take(1800).collect::<String>()));
        }
        p.push_str("\n团队成员（负责人只能从这里选）：\n");
        for m in &members {
            p.push_str(&format!("- {}（{}，项目角色 {}{}）\n", m.name, m.title, m.role, self.db.cap_suffix(&m.employee_id)));
        }
        p.push_str(&format!("\n{}\n", crate::levels::LEVEL_RUBRIC));
        if members.iter().any(|m| m.role == "architect") {
            p.push_str("\n架构师只做设计类条目（kind=design，已经做完了），不要把实现条目指派给架构师；实现条目要遵守上面的设计文档。\n");
        }
        if !existing.items.is_empty() {
            p.push_str(&format!("\n已有计划（不要重复，新条目要和它们衔接）：\n{}", plan_table(&existing.items)));
        }
        p.push_str(&format!("\n你可以只读地查看代码。要求：1 到 {MAX_DRAFT_ITEMS} 个条目，每个条目一名员工一次任务能做完；第一个条目通常是脚手架或建立可验证的基础；用 depends_on 标明先后依赖（只能依赖更靠前的条目的下标，从 0 开始），让能并行的尽量并行；每个条目用 files 列出它预计会修改的主要文件（相对仓库根目录），改同一个文件的条目会被自动排成先后、不能并行，所以尽量让不同条目改不同的文件（例如每个命令放一个模块），实在要改同一个文件的就合并成一个条目；\
至少有一个条目交给 QA 角色的成员做测试或评审（如果有）；每个条目写清 description 和能用命令验证的 acceptance。\n"));
        p.push_str("只输出一个 JSON 对象，不要任何其他文字：\n{\"items\":[{\"title\":\"...\",\"description\":\"...\",\"acceptance\":\"...\",\"assignee\":\"成员名或 null\",\"depends_on\":[下标...],\"files\":[\"路径\"],\"level\":3}]}");
        let text = self.lead_turn(lead, project_id, &p, "起草计划", approver).await?;
        let names: Vec<String> = members.iter().map(|m| m.name.clone()).collect();
        let draft = parse_draft(&text, &names)?;
        let mut ids: Vec<String> = Vec::new();
        for d in &draft {
            let aid = d.assignee.as_ref().and_then(|n| members.iter().find(|m| &m.name == n)).map(|m| m.employee_id.clone());
            let deps = d.depends_on.iter().map(|i| ids[*i].clone()).collect();
            let id = self.db.create_plan_item(project_id, &NewPlanItem { title: d.title.clone(), description: d.description.clone(), acceptance: d.acceptance.clone(), kind: "migrate".into(), module: String::new(), assignee_id: aid, deps }, Some(&lead.id))?;
            self.db.set_plan_item_files(&id, &d.files)?;
            self.db.set_plan_item_level(&id, d.level)?;
            ids.push(id);
        }
        self.db.append_event_for(Some(project_id), "plan_drafted_by_lead", Some(&lead.id), None, &json!({"count": ids.len(), "text": format!("{} 起草了 {} 个计划条目，等你确认", lead.name, ids.len())}))?;
        Ok(ids.len())
    }

    /// 负责人调整计划。`failure` 是触发调整的失败说明（ItemFailed 时）。
    pub async fn lead_adjust_plan<A: Approver>(&self, project_id: &str, lead: &Employee, trigger: AdjustTrigger, failure: &str, approver: &A) -> Result<Applied> {
        let project = self.db.get_project(project_id)?.context("项目不存在")?;
        let members = self.db.list_members(project_id)?;
        let view = self.db.plan_view(project_id)?;
        let mut p = format!("[调整计划] 你是项目「{}」的负责人。\n\n项目目标与硬性约束：\n{}\n\n当前计划：\n{}", project.name, project.goal.trim(), plan_table(&view.items));
        p.push_str(&self.design_docs_block(project_id).await);
        p.push_str("\n团队成员：");
        p.push_str(&members.iter().map(|m| format!("{}（{}{}）", m.name, m.role, self.db.cap_suffix(&m.employee_id))).collect::<Vec<_>>().join("、"));
        p.push_str(&format!("\n\n{}", crate::levels::LEVEL_RUBRIC));
        let memory = self.memory_block(Some(project_id), lead, None, &format!("{} {}", project.name, failure), "planning", project_id);
        if !memory.is_empty() {
            p.push_str(&format!("\n\n{memory}"));
        }
        match trigger {
            AdjustTrigger::ItemFailed => p.push_str(&format!("\n\n触发原因：有条目反复失败，没能做成：\n{failure}\n你可以：追加条目（比如把失败的拆小、补一个前置）、取消作废的条目、给还没开始的条目改派负责人、让失败的条目换一个更清楚的说明重试。")),
            AdjustTrigger::Dedupe => {
                let pairs = self.db.unchecked_duplicate_pairs(project_id, &view.items).iter().map(|(a, b)| format!("#{a} 与 #{b}")).collect::<Vec<_>>().join("、");
                p.push_str(&format!("\n\n触发原因：批次开始，清理计划里的重复条目。系统按标题相似度找到这些疑似重复：{pairs}。请逐对核实它们是不是同一件事（看说明和验收标准，不要只看标题）：是同一件事，就取消还没开始的那一条（两条都没开始时保留编号小的，也就是更早建的），原因写“与 #N 重复”；不是同一件事就不要动。只能取消，不能追加、改派或重试；没有真正的重复就返回空 changes。"));
            }
            AdjustTrigger::Takeover => p.push_str("\n\n触发原因：接手复盘。你刚刚接任这个项目的负责人，原来的计划是别人定的，条目大多没有等级、指派也没有按各人的能力上限来。请复盘所有还没开始的条目（待开始、已阻塞）：① 没有等级的补上等级（按上面的评分标准）；② 按各人的能力上限重新分配：每个条目指派给“能力上限够用的最低档”的人，只有高等级的条目才给强的人（包括你自己），原来的指派合适就不要动。只能用 set_level（定级，写 item 和 level）和 reassign（改派）；不要取消、追加、重试，也不要改条目的说明。例如：{\"op\":\"set_level\",\"item\":3,\"level\":2},{\"op\":\"reassign\",\"item\":3,\"assignee\":\"成员名\"}。没有需要改的就返回空 changes。"),
            AdjustTrigger::FinalReview => p.push_str("\n\n触发原因：计划里的条目都做完了。请对照项目目标做一次完成度复盘：还有没覆盖到的必要工作吗？只能追加条目；没有就返回空 changes。不要为了凑数追加。"),
        }
        p.push_str(&format!("\n\n规则：每次最多追加 {MAX_ADDED_PER_ADJUST} 个条目，每条都要写原因；已开始和已完成的条目不能动；涉及项目目标、需求或范围变更时，不要自己改，用 escalate 交给人（写明原因）。\
条目用 #编号 指代（见上面的计划）；追加的条目 depends_on 里写 \"#编号\"（已有条目）或 \"N1\"、\"N2\"（本次追加的第几个）。\n"));
        p.push_str("只输出一个 JSON 对象，不要任何其他文字：\n{\"changes\":[{\"op\":\"add\",\"title\":\"...\",\"description\":\"...\",\"acceptance\":\"...\",\"assignee\":\"成员名\",\"depends_on\":[\"#2\"],\"files\":[\"路径\"],\"level\":3,\"reason\":\"...\"},{\"op\":\"cancel\",\"item\":3,\"reason\":\"...\"},{\"op\":\"reassign\",\"item\":4,\"assignee\":\"成员名\"},{\"op\":\"retry\",\"item\":5,\"note\":\"换成更清楚的说明\"}],\"escalate\":null,\"summary\":\"一句话说明做了什么调整\"}");
        let text = self.lead_turn(lead, project_id, &p, "调整计划", approver).await?;
        let adj = parse_adjustment(&text)?;
        let mut out = Applied { escalate: adj.escalate.clone(), summary: adj.summary.clone(), ..Default::default() };
        let by_no = |n: usize| -> Option<&PlanItem> { n.checked_sub(1).and_then(|i| view.items.get(i)) };
        let member = |name: &str| members.iter().find(|m| m.name == name).map(|m| m.employee_id.clone());
        let mut new_ids: Vec<String> = Vec::new();
        for ch in adj.changes {
            if trigger == AdjustTrigger::Dedupe && !matches!(ch, Change::Cancel { .. }) {
                out.rejected.push("清理重复条目时只能取消".into());
                continue;
            }
            if trigger == AdjustTrigger::Takeover && !matches!(ch, Change::SetLevel { .. } | Change::Reassign { .. }) {
                out.rejected.push("接手复盘时只能定级和改派".into());
                continue;
            }
            if trigger != AdjustTrigger::Takeover && matches!(ch, Change::SetLevel { .. }) {
                out.rejected.push("定级只在接手复盘时使用".into());
                continue;
            }
            match ch {
                Change::Add { title, description, acceptance, assignee, depends_on, files, level, reason } => {
                    if new_ids.len() >= MAX_ADDED_PER_ADJUST {
                        out.rejected.push(format!("追加《{title}》：超过每次 {MAX_ADDED_PER_ADJUST} 个的上限"));
                        continue;
                    }
                    let mut deps = Vec::new();
                    let mut bad = None;
                    for d in &depends_on {
                        if let Some(n) = d.strip_prefix('#').and_then(|x| x.parse::<usize>().ok()) {
                            match by_no(n) { Some(i) if i.status != "cancelled" => deps.push(i.id.clone()), _ => bad = Some(format!("依赖 {d} 不存在或已取消")) }
                        } else if let Some(n) = d.strip_prefix('N').and_then(|x| x.parse::<usize>().ok()) {
                            match n.checked_sub(1).and_then(|i| new_ids.get(i)) { Some(id) => deps.push(id.clone()), None => bad = Some(format!("依赖 {d} 还不存在")) }
                        } else { bad = Some(format!("依赖 {d} 写法不对")) }
                    }
                    if let Some(b) = bad { out.rejected.push(format!("追加《{title}》：{b}")); continue; }
                    let aid = assignee.as_deref().and_then(member);
                    if assignee.is_some() && aid.is_none() { out.rejected.push(format!("追加《{title}》：负责人不是项目成员")); continue; }
                    match self.db.create_plan_item(project_id, &NewPlanItem { title: title.clone(), description, acceptance, kind: "migrate".into(), module: String::new(), assignee_id: aid, deps }, None) {
                        Ok(id) => {
                            let _ = self.db.set_plan_item_files(&id, &files);
                            let _ = self.db.set_plan_item_level(&id, level);
                            let _ = self.db.set_plan_note(&id, &format!("负责人追加：{reason}"));
                            new_ids.push(id);
                            out.added.push(title);
                        }
                        Err(e) => out.rejected.push(format!("追加《{title}》：{e}")),
                    }
                }
                _ if trigger == AdjustTrigger::FinalReview => out.rejected.push("复盘时只能追加条目".into()),
                Change::Cancel { item, reason } => match by_no(item) {
                    Some(i) if matches!(i.status.as_str(), "planned" | "blocked") => match self.db.set_plan_status(&i.id, "cancelled", &format!("负责人取消：{reason}")) {
                        Ok(()) => out.cancelled.push(i.title.clone()),
                        Err(e) => out.rejected.push(format!("取消《{}》：{e}", i.title)),
                    },
                    Some(i) => out.rejected.push(format!("取消《{}》：条目已是「{}」，不能动", i.title, crate::plan::status_label(&i.status))),
                    None => out.rejected.push(format!("取消 #{item}：没有这个条目")),
                },
                Change::SetLevel { item, level } => match by_no(item) {
                    Some(i) if matches!(i.status.as_str(), "planned" | "blocked" | "proposed") => match self.db.set_plan_item_level(&i.id, level) {
                        Ok(()) => out.leveled.push(format!("《{}》→ L{level}", i.title)),
                        Err(e) => out.rejected.push(format!("定级《{}》：{e}", i.title)),
                    },
                    Some(i) => out.rejected.push(format!("定级《{}》：只能给还没开始的条目定级", i.title)),
                    None => out.rejected.push(format!("定级 #{item}：没有这个条目")),
                },
                Change::Reassign { item, .. } if by_no(item).is_some_and(|i| i.kind == crate::design::DESIGN_KIND) => out.rejected.push(format!("改派 #{item}：设计条目只给架构师做，不能改派")),
                Change::Reassign { item, assignee } if members.iter().any(|m| m.name == assignee && m.role == "architect") && by_no(item).is_some_and(|i| i.kind != crate::design::DESIGN_KIND) => out.rejected.push(format!("改派 #{item}：架构师只做设计条目，不能指派实现条目")),
                Change::Reassign { item, assignee } => match (by_no(item), member(&assignee)) {
                    (Some(i), Some(aid)) if i.status == "planned" => {
                        let n = NewPlanItem { title: i.title.clone(), description: i.description.clone(), acceptance: i.acceptance.clone(), kind: i.kind.clone(), module: i.module.clone(), assignee_id: Some(aid), deps: vec![] };
                        match self.db.update_plan_item(&i.id, &n) { Ok(()) => out.reassigned.push(format!("《{}》→ {assignee}", i.title)), Err(e) => out.rejected.push(format!("改派《{}》：{e}", i.title)) }
                    }
                    (Some(i), _) => out.rejected.push(format!("改派《{}》：只能改派还没开始的条目，且负责人要是项目成员", i.title)),
                    _ => out.rejected.push(format!("改派 #{item}：没有这个条目")),
                },
                Change::Retry { item, note } => match by_no(item) {
                    Some(i) if matches!(i.status.as_str(), "blocked" | "planned") => {
                        let n = NewPlanItem { title: i.title.clone(), description: format!("{}\n\n[负责人补充说明（上次失败后）]\n{note}", i.description), acceptance: i.acceptance.clone(), kind: i.kind.clone(), module: i.module.clone(), assignee_id: i.assignee_id.clone(), deps: vec![] };
                        if self.db.update_plan_item(&i.id, &n).is_ok() && (i.status == "planned" || self.db.set_plan_status(&i.id, "planned", "负责人让它换个说明重试").is_ok()) {
                            out.retried.push(i.title.clone());
                            out.retried_ids.push(i.id.clone());
                        }
                    }
                    Some(i) => out.rejected.push(format!("重试《{}》：只有失败后回到待开始或已阻塞的条目才能重试", i.title)),
                    None => out.rejected.push(format!("重试 #{item}：没有这个条目")),
                },
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_candidates_need_one_open_item_and_ignore_cancelled() {
        let mk = |t: &str, st: &str| PlanItem { title: t.into(), status: st.into(), ..Default::default() };
        let v = vec![mk("前端健壮性与防误操作：renderBoard 空值防御", "planned"), mk("SQLite 存储实现", "done"), mk("前端健壮性与防误操作：renderBoard 空值防御", "planned"),
            mk("SQLite 存储实现", "done"), mk("前端健壮性与防误操作：renderBoard 空值防御", "cancelled")];
        assert_eq!(duplicate_candidates(&v), vec![(1, 3)], "两条已完成的同名不算；已取消的不算");
    }

    #[test]
    fn checked_pairs_are_remembered_and_not_asked_again() {
        let db = crate::db::Db::open_memory().unwrap();
        let mk = |id: &str, t: &str| PlanItem { id: id.into(), title: t.into(), status: "planned".into(), ..Default::default() };
        let v = vec![mk("a", "前端健壮性与防误操作：renderBoard 空值防御"), mk("b", "前端健壮性与防误操作：renderBoard 空值防御"), mk("c", "前端健壮性与防误操作：renderBoard 空值防御")];
        let pairs = db.unchecked_duplicate_pairs("p1", &v);
        assert_eq!(pairs.len(), 3);
        db.mark_duplicates_checked("p1", &v, &pairs[..2]).unwrap();
        assert_eq!(db.unchecked_duplicate_pairs("p1", &v), vec![pairs[2]], "核对过的不再问");
        assert_eq!(db.unchecked_duplicate_pairs("p2", &v).len(), 3, "按项目分开记");
        let mut v2 = v.clone();
        v2.push(mk("d", "前端健壮性与防误操作：renderBoard 空值防御"));
        assert_eq!(db.unchecked_duplicate_pairs("p1", &v2).len(), 4, "新条目和旧条目的对仍然要核对");
    }


    fn names() -> Vec<String> { vec!["alice".into(), "bob".into()] }

    #[test]
    fn draft_is_validated_before_anything_is_created() {
        let ok = r#"好的：{"items":[{"title":"脚手架","description":"d","acceptance":"a","assignee":"bob","depends_on":[]},{"title":"逻辑","description":"d","acceptance":"a","assignee":null,"depends_on":[0]}]}"#;
        let d = parse_draft(ok, &names()).unwrap();
        assert_eq!((d.len(), d[0].assignee.as_deref(), d[1].depends_on.clone()), (2, Some("bob"), vec![0]));
        assert!(d[1].assignee.is_none());
        for bad in ["没有 json", r#"{"items":[]}"#, r#"{"items":[{"title":""}]}"#,
                    r#"{"items":[{"title":"a","assignee":"zed"}]}"#,
                    r#"{"items":[{"title":"a","depends_on":[0]}]}"#,
                    r#"{"items":[{"title":"a"},{"title":"b","depends_on":[1]}]}"#] {
            assert!(parse_draft(bad, &names()).is_err(), "{bad}");
        }
        let too_many = format!("{{\"items\":[{}]}}", (0..16).map(|i| format!("{{\"title\":\"t{i}\"}}")).collect::<Vec<_>>().join(","));
        assert!(parse_draft(&too_many, &names()).is_err());
    }

    #[test]
    fn adjustments_need_reasons_and_known_ops() {
        let ok = r##"{"changes":[{"op":"add","title":"补迁移脚本","reason":"A 失败暴露了缺这一步","depends_on":["#1","N1"]},{"op":"cancel","item":3,"reason":"不再需要"},{"op":"reassign","item":2,"assignee":"bob"},{"op":"retry","item":4,"note":"先做 X 再做 Y"}],"escalate":null,"summary":"拆小"}"##;
        let a = parse_adjustment(ok).unwrap();
        assert_eq!(a.changes.len(), 4);
        assert!(a.escalate.is_none() && a.summary == "拆小");
        let esc = parse_adjustment(r#"{"changes":[],"escalate":"需要改项目范围"}"#).unwrap();
        assert_eq!(esc.escalate.as_deref(), Some("需要改项目范围"));
        for bad in [r#"{"changes":[{"op":"add","title":"x"}]}"#, r#"{"changes":[{"op":"cancel","item":1}]}"#, r#"{"changes":[{"op":"retry","item":1}]}"#,
                    r#"{"changes":[{"op":"删除全部"}]}"#, "不是 json"] {
            assert!(parse_adjustment(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn expected_files_are_normalised_and_capped() {
        let names = vec!["bob".to_string()];
        let t = r#"{"items":[{"title":"a","description":"","acceptance":"","assignee":"bob","depends_on":[],"files":["./todo.py"," todo.py ","tests/t.py","","a","b","c","d","e","f"]},{"title":"b","depends_on":[]}]}"#;
        let d = parse_draft(t, &names).unwrap();
        assert_eq!(d[0].files[..2], ["todo.py".to_string(), "tests/t.py".to_string()], "去掉 ./ 和空白、去重");
        assert_eq!(d[0].files.len(), 8, "最多 8 个");
        assert!(d[1].files.is_empty(), "没写就是空");
    }
}
