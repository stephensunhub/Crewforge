//! 团队协作工具（MCP 服务器）：让 AI 员工通过结构化工具提议议题、补充方案、请求拍板、提议记忆。
//!
//! 这是一个 stdio 上的 JSON-RPC 服务器，由 Claude Code 按 `--mcp-config` 启动；启动参数里写死了
//! 员工、项目和来源，员工无法冒用别人的身份。它与应用共用同一个 SQLite 库（跨进程写入，WAL + 忙等待）。
//!
//! 边界：AI **只能提议**。它不能拍板议题，也不能让记忆直接生效——
//! 议题进入“待讨论”等人类确认，记忆进入“待验证”等人类验证；公司级记忆不对 AI 开放。

use crate::board::ISSUE_KINDS;
use crate::db::Db;
use crate::memory::{KINDS, NewMemory};
use crate::plan::{NewPlanItem, PLAN_KINDS};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

/// 追加到员工身份说明后的工具使用指引。
pub const TEAM_TOOLS_HINT: &str = "[团队协作工具] 你可以使用名字以 mcp__crewforge__ 开头的工具与团队协作：\
遇到需要团队讨论并决定、而你无权单独决定的事（例如有多个可行方案的技术取舍、需求不明确），先用 list_issues 看看是否已有相关议题，\
没有再用 propose_issue 提议；已有议题就用 add_option 补充你的方案；讨论充分后用 request_decision 请求拍板。\
你亲自验证过、值得长期保留的经验，用 propose_memory 提议（需要人类验证后才会生效）。\
当被要求制定计划（比如把一次大的重构或新项目拆成步骤）时，先用 list_plan 看现有计划，再用 propose_plan 一次提交整份计划：每个条目写清说明和验收标准，用 depends_on 标明先后依赖；人类批准后才会生效。\
不要为琐碎的事提议题，每次任务或会议最多提 3 个。你只能提议，不能拍板。";

const MAX_TITLE: usize = 80;
const MAX_SUMMARY: usize = 600;
const MAX_OPTION_TEXT: usize = 300;
const MAX_MEMORY: usize = 600;
const MAX_OPTIONS_PER_PROPOSAL: usize = 4;
const MAX_OPTIONS_PER_ISSUE: usize = 8;
/// 每名员工在一个项目里同时未处理的 AI 提议上限
const MAX_PENDING_ISSUES: i64 = 5;
const MAX_PENDING_MEMORIES: i64 = 5;
/// 每次任务/会议里最多提议的条数
const MAX_PER_ORIGIN: i64 = 3;
const DUP_THRESHOLD: f64 = 0.7;
const PLAN_DUP_THRESHOLD: f64 = 0.8;
const MAX_PLAN_ITEMS: usize = 30;
const MAX_PLAN_CALLS_PER_ORIGIN: i64 = 2;
const MAX_PENDING_PLAN_ITEMS: usize = 60;

#[derive(Debug, Clone)]
pub struct McpContext {
    pub employee_id: String,
    pub project_id: String,
    /// 来源，如 "task:<id>" 或 "meeting:<id>"
    pub origin: String,
}

pub struct McpServer {
    db: Arc<Db>,
    ctx: McpContext,
}

fn chars(s: &str) -> usize {
    s.chars().count()
}

/// 归一化：小写、去掉空白和标点，只留文字与数字。
fn normalize(s: &str) -> String {
    s.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect()
}

/// 字符二元组的 Jaccard 相似度，用于发现“换了个说法的重复议题”。
pub fn similarity(a: &str, b: &str) -> f64 {
    let (a, b) = (normalize(a), normalize(b));
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a == b {
        return 1.0;
    }
    let grams = |s: &str| -> BTreeSet<(char, char)> { let v: Vec<char> = s.chars().collect(); v.windows(2).map(|w| (w[0], w[1])).collect() };
    let (ga, gb) = (grams(&a), grams(&b));
    if ga.is_empty() || gb.is_empty() {
        return 0.0; // 单字的标题只有完全相同才算重复
    }
    ga.intersection(&gb).count() as f64 / ga.union(&gb).count() as f64
}

fn tools() -> Value {
    let str_prop = |d: &str| json!({"type": "string", "description": d});
    json!([
        {"name": "list_issues", "description": "列出本项目里还没结束的议题和最近的决议（含议题 ID）。提议新议题之前先用它查重。",
         "inputSchema": {"type": "object", "properties": {}}},
        {"name": "propose_issue",
         "description": "提议一个需要团队讨论并决定的议题。你只是提议：议题会进入“待讨论”，等人类确认后才开始讨论。提议前请先 list_issues 查重。",
         "inputSchema": {"type": "object", "required": ["title", "summary"], "properties": {
            "title": str_prop("议题标题，一句话，不超过 80 字"),
            "summary": str_prop("问题摘要：要决定什么、为什么现在要决定，不超过 600 字"),
            "kind": {"type": "string", "enum": ISSUE_KINDS, "description": "类型，默认 technical；涉及需求/业务方向用 requirement"},
            "high_risk": {"type": "boolean", "description": "涉及资金、生产环境、不可逆操作或安全时为 true（只能人类拍板）"},
            "options": {"type": "array", "maxItems": MAX_OPTIONS_PER_PROPOSAL, "description": "你认为可行的候选方案",
                "items": {"type": "object", "required": ["title"], "properties": {"title": str_prop("方案名"), "description": str_prop("方案说明与利弊")}}}}}},
        {"name": "add_option", "description": "给已有议题补充一个候选方案（议题还没有结论时才可以）。",
         "inputSchema": {"type": "object", "required": ["issue_id", "title"], "properties": {
            "issue_id": str_prop("议题 ID（来自 list_issues）"), "title": str_prop("方案名"), "description": str_prop("方案说明与利弊")}}},
        {"name": "request_decision", "description": "讨论充分后，请求有权限的人拍板：把“讨论中”的议题推进到“待决策”。你不能自己拍板。",
         "inputSchema": {"type": "object", "required": ["issue_id"], "properties": {
            "issue_id": str_prop("议题 ID"), "note": str_prop("给拍板人的一句话：你的倾向和理由")}}},
        {"name": "list_plan", "description": "列出本项目的计划条目（含 ID、状态、依赖）。制定或补充计划之前先用它，避免重复。",
         "inputSchema": {"type": "object", "properties": {}}},
        {"name": "propose_plan",
         "description": "一次提交整份计划：把大目标拆成有顺序、有依赖的小步骤。条目先是“待确认”，人类批准后才进入计划。每个条目只做一件事，粒度以一名员工一次任务能完成为宜。",
         "inputSchema": {"type": "object", "required": ["items"], "properties": {"items": {"type": "array", "minItems": 1, "maxItems": 30, "items": {
            "type": "object", "required": ["title"], "properties": {
              "title": str_prop("条目标题，不超过 80 字"),
              "description": str_prop("要做什么、范围和注意事项，不超过 600 字"),
              "acceptance": str_prop("验收标准：做到什么算完成，最好能用命令验证，不超过 400 字"),
              "kind": {"type": "string", "enum": PLAN_KINDS, "description": "characterize=补特征测试；migrate=迁移/实现；cleanup=清理；other=其他，默认 migrate"},
              "module": str_prop("所属模块或目录，可选"),
              "assignee": str_prop("建议的负责人（项目成员的名字），可选"),
              "depends_on": {"type": "array", "description": "依赖：本次提交里排在前面的条目的序号（从 0 开始），或已有条目的 ID", "items": {}}}}}}}},
        {"name": "propose_memory",
         "description": "提议一条值得长期保留的经验。它会先成为“待验证”的候选，人类验证后才会被注入给员工；不要提议未经你验证的猜测。",
         "inputSchema": {"type": "object", "required": ["content"], "properties": {
            "title": str_prop("标题，留空则取内容第一句"), "content": str_prop("可以直接照做的一两句话，不超过 600 字"),
            "kind": {"type": "string", "enum": KINDS, "description": "类型，默认 lesson"},
            "layer": {"type": "string", "enum": ["project", "personal"], "description": "project：项目共享；personal：只给你自己，默认 project"},
            "scope": str_prop("适用范围，可选")}}}
    ])
}

fn ok(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}
fn rpc_err(id: &Value, code: i64, msg: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}})
}
fn text_result(text: String, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

fn arg_str<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

impl McpServer {
    pub fn new(db: Arc<Db>, ctx: McpContext) -> Self {
        Self { db, ctx }
    }

    /// 处理一条 JSON-RPC 消息。通知（没有 id）不需要回复，返回 None。
    pub fn handle(&self, req: &Value) -> Option<Value> {
        let id = req.get("id")?.clone();
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        Some(match method {
            "initialize" => ok(&id, json!({
                "protocolVersion": req["params"]["protocolVersion"].as_str().unwrap_or("2024-11-05"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "crewforge", "version": env!("CARGO_PKG_VERSION")},
            })),
            "ping" => ok(&id, json!({})),
            "tools/list" => ok(&id, json!({"tools": tools()})),
            "tools/call" => {
                let name = req["params"]["name"].as_str().unwrap_or("");
                let args = req["params"].get("arguments").cloned().unwrap_or_else(|| json!({}));
                match self.call(name, &args) {
                    Ok(t) => ok(&id, text_result(t, false)),
                    // 业务上的拒绝作为工具结果返回，让模型看得到原因并调整，而不是协议错误
                    Err(e) => ok(&id, text_result(format!("{e}"), true)),
                }
            }
            _ => rpc_err(&id, -32601, &format!("不支持的方法: {method}")),
        })
    }

    fn call(&self, name: &str, a: &Value) -> Result<String> {
        // 每次调用都重新校验身份：必须是项目成员，且没有被归档
        let emp = self.db.employee_by_id(&self.ctx.employee_id)?.context("员工不存在")?;
        if emp.archived || !self.db.is_member(&self.ctx.project_id, &emp.id)? {
            bail!("你不是这个项目的成员，不能使用团队协作工具");
        }
        match name {
            "list_issues" => self.list_issues(),
            "propose_issue" => self.propose_issue(a),
            "add_option" => self.add_option(a),
            "request_decision" => self.request_decision(a),
            "propose_memory" => self.propose_memory(a),
            "list_plan" => self.list_plan(),
            "propose_plan" => self.propose_plan(a),
            other => bail!("未知工具: {other}"),
        }
    }

    fn issue_of_project(&self, id: &str) -> Result<crate::board::Issue> {
        let i = self.db.get_issue(id)?.context("议题不存在，请用 list_issues 确认议题 ID")?;
        if i.project_id != self.ctx.project_id {
            bail!("这个议题不属于当前项目");
        }
        Ok(i)
    }

    fn record(&self, kind: &str, payload: Value) {
        let mut p = payload;
        p["type"] = json!(kind);
        p["origin"] = json!(self.ctx.origin);
        let _ = self.db.append_event_for(Some(&self.ctx.project_id), "agent_proposed", Some(&self.ctx.employee_id), None, &p);
    }

    fn proposals_from_origin(&self, ptype: &str) -> Result<i64> {
        Ok(self.db.conn.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM events WHERE kind = 'agent_proposed' AND json_extract(payload,'$.origin') = ?1 AND json_extract(payload,'$.type') = ?2",
            rusqlite::params![self.ctx.origin, ptype], |r| r.get(0))?)
    }

    fn list_issues(&self) -> Result<String> {
        let views = self.db.issue_views(&self.ctx.project_id)?;
        let live: Vec<_> = views.iter().filter(|v| !matches!(v.issue.status.as_str(), "cancelled" | "closed")).take(25).collect();
        if live.is_empty() {
            return Ok("目前没有未结束的议题。".into());
        }
        let mut out = String::new();
        for v in live {
            let i = &v.issue;
            out.push_str(&format!("- id={} [{}] 《{}》", i.id, crate::mcp::status_label(&i.status), i.title));
            if !i.summary.is_empty() {
                out.push_str(&format!("：{}", i.summary.chars().take(120).collect::<String>()));
            }
            if !v.options.is_empty() {
                out.push_str(&format!("｜方案：{}", v.options.iter().map(|o| o.title.as_str()).collect::<Vec<_>>().join(" / ")));
            }
            if let Some(d) = &i.decision {
                out.push_str(&format!("｜决议：{d}"));
            }
            out.push('\n');
        }
        Ok(out)
    }

    fn propose_issue(&self, a: &Value) -> Result<String> {
        let title = arg_str(a, "title").context("缺少 title")?;
        let summary = arg_str(a, "summary").context("缺少 summary：请写清楚要决定什么、为什么")?;
        if chars(title) > MAX_TITLE {
            bail!("标题太长（上限 {MAX_TITLE} 字），请精简");
        }
        if chars(summary) > MAX_SUMMARY {
            bail!("摘要太长（上限 {MAX_SUMMARY} 字），请精简");
        }
        let kind = arg_str(a, "kind").unwrap_or("technical");
        if !ISSUE_KINDS.contains(&kind) {
            bail!("未知的议题类型 {kind}，可选：{}", ISSUE_KINDS.join("、"));
        }
        let high_risk = a.get("high_risk").and_then(Value::as_bool).unwrap_or(false);
        let options: Vec<(String, String)> = a.get("options").and_then(Value::as_array).map(|v| {
            v.iter().filter_map(|o| Some((o.get("title")?.as_str()?.trim().to_string(), o.get("description").and_then(Value::as_str).unwrap_or("").trim().to_string()))).filter(|(t, _)| !t.is_empty()).collect()
        }).unwrap_or_default();
        if options.len() > MAX_OPTIONS_PER_PROPOSAL {
            bail!("一次最多带 {MAX_OPTIONS_PER_PROPOSAL} 个候选方案");
        }
        if options.iter().any(|(t, d)| chars(t) > MAX_TITLE || chars(d) > MAX_OPTION_TEXT) {
            bail!("方案的标题或说明太长，请精简");
        }

        // 限流：每人未处理的提议数、每次任务/会议的提议数
        let pending: i64 = self.db.conn.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM issues WHERE project_id = ?1 AND raised_by = ?2 AND status IN ('open','discussing','pending_decision')",
            rusqlite::params![self.ctx.project_id, self.ctx.employee_id], |r| r.get(0))?;
        if pending >= MAX_PENDING_ISSUES {
            bail!("你已经有 {pending} 个议题还没处理完（上限 {MAX_PENDING_ISSUES}）。请等它们有结论后再提新的，或用 add_option 补充已有议题");
        }
        if self.proposals_from_origin("issue")? >= MAX_PER_ORIGIN {
            bail!("这次任务/会议里你已经提了 {MAX_PER_ORIGIN} 个议题，不要再提了；把重点放在手头的工作上");
        }
        // 查重：和本项目里未取消的议题比较
        for v in self.db.issue_views(&self.ctx.project_id)? {
            if v.issue.status == "cancelled" {
                continue;
            }
            if similarity(&v.issue.title, title) >= DUP_THRESHOLD {
                bail!("已经有类似的议题：id={}《{}》（{}）{}。请用 add_option 补充方案，不要重复提议",
                    v.issue.id, v.issue.title, status_label(&v.issue.status), v.issue.decision.as_deref().map(|d| format!("，已决议：{d}")).unwrap_or_default());
            }
        }

        let issue = self.db.create_issue(&self.ctx.project_id, title, summary, kind, high_risk, Some(&self.ctx.employee_id), None, None)?;
        for (t, d) in &options {
            self.db.add_option(&issue.id, t, d, Some(&self.ctx.employee_id))?;
        }
        self.record("issue", json!({"issue_id": issue.id, "title": issue.title, "options": options.len()}));
        Ok(format!("已提议议题（id={}）。它现在是“待讨论”，等人类确认后才会开始讨论；你不需要也不能自己推进它。", issue.id))
    }

    fn add_option(&self, a: &Value) -> Result<String> {
        let issue = self.issue_of_project(arg_str(a, "issue_id").context("缺少 issue_id")?)?;
        let title = arg_str(a, "title").context("缺少 title")?;
        let desc = arg_str(a, "description").unwrap_or("");
        if chars(title) > MAX_TITLE || chars(desc) > MAX_OPTION_TEXT {
            bail!("方案的标题或说明太长，请精简");
        }
        let existing = self.db.issue_options(&issue.id)?;
        if existing.len() >= MAX_OPTIONS_PER_ISSUE {
            bail!("这个议题已经有 {} 个候选方案，够多了", existing.len());
        }
        if let Some(o) = existing.iter().find(|o| similarity(&o.title, title) >= DUP_THRESHOLD) {
            bail!("已经有类似的方案《{}》，不要重复添加", o.title);
        }
        self.db.add_option(&issue.id, title, desc, Some(&self.ctx.employee_id))?;
        self.record("option", json!({"issue_id": issue.id, "issue_title": issue.title, "title": title}));
        Ok(format!("已给《{}》添加候选方案《{title}》。", issue.title))
    }

    fn request_decision(&self, a: &Value) -> Result<String> {
        let issue = self.issue_of_project(arg_str(a, "issue_id").context("缺少 issue_id")?)?;
        match issue.status.as_str() {
            "discussing" => {}
            "open" => bail!("这个议题还没有被人类确认讨论，现在不能请求拍板"),
            "pending_decision" => bail!("这个议题已经在等待拍板了"),
            s => bail!("议题当前是「{}」，不能请求拍板", status_label(s)),
        }
        self.db.set_issue_status(&issue.id, "pending_decision")?;
        self.record("request_decision", json!({"issue_id": issue.id, "title": issue.title, "note": arg_str(a, "note").unwrap_or("")}));
        Ok(format!("已请求拍板《{}》。有权限的人会收到待办；你不能替他们决定。", issue.title))
    }

    fn list_plan(&self) -> Result<String> {
        let view = self.db.plan_view(&self.ctx.project_id)?;
        if view.items.is_empty() {
            return Ok("目前还没有计划条目。".into());
        }
        let mut out = format!("进度 {}%（完成 {} / 进行中 {} / 待开始 {} / 待确认 {}）\n", view.percent_done, view.counts.done, view.counts.in_progress, view.counts.planned, view.counts.proposed);
        for i in view.items.iter().filter(|i| i.status != "cancelled") {
            out.push_str(&format!("- id={} [{}] 《{}》", i.id, crate::plan::status_label(&i.status), i.title));
            if !i.module.is_empty() { out.push_str(&format!("｜模块 {}", i.module)); }
            if !i.waiting_for.is_empty() { out.push_str(&format!("｜等待：{}", i.waiting_for.join("、"))); }
            out.push('\n');
        }
        Ok(out)
    }

    fn propose_plan(&self, a: &Value) -> Result<String> {
        let raw = a.get("items").and_then(Value::as_array).context("缺少 items")?;
        if raw.is_empty() || raw.len() > MAX_PLAN_ITEMS {
            bail!("一次提交 1 到 {MAX_PLAN_ITEMS} 个条目");
        }
        if self.proposals_from_origin("plan")? >= MAX_PLAN_CALLS_PER_ORIGIN {
            bail!("这次任务/会议里你已经提交过计划了。要调整请告诉人类，或等人类处理后再补充");
        }
        let existing = self.db.plan_view(&self.ctx.project_id)?;
        if existing.counts.proposed + raw.len() > MAX_PENDING_PLAN_ITEMS {
            bail!("已经有 {} 个条目在等人类确认，请等它们被处理后再提交", existing.counts.proposed);
        }
        let members = self.db.list_members(&self.ctx.project_id)?;
        // 先全部校验并解析，确保要么全部创建、要么一个都不创建
        let mut parsed: Vec<(NewPlanItem, Vec<serde_json::Value>)> = Vec::new();
        for (i, it) in raw.iter().enumerate() {
            let title = arg_str(it, "title").with_context(|| format!("第 {i} 个条目缺少 title"))?;
            let (desc, acc) = (arg_str(it, "description").unwrap_or(""), arg_str(it, "acceptance").unwrap_or(""));
            let module = arg_str(it, "module").unwrap_or("");
            if chars(title) > MAX_TITLE || chars(desc) > MAX_SUMMARY || chars(acc) > MAX_OPTION_TEXT + 100 || chars(module) > 60 {
                bail!("第 {i} 个条目的标题、说明、验收标准或模块太长，请精简");
            }
            let kind = arg_str(it, "kind").unwrap_or("migrate");
            if !PLAN_KINDS.contains(&kind) {
                bail!("第 {i} 个条目的类型 {kind} 未知，可选：{}", PLAN_KINDS.join("、"));
            }
            let assignee_id = arg_str(it, "assignee").and_then(|n| members.iter().find(|m| m.name == n)).map(|m| m.employee_id.clone());
            let deps = it.get("depends_on").and_then(Value::as_array).cloned().unwrap_or_default();
            for d in &deps {
                match d {
                    Value::Number(n) if n.as_u64().is_some_and(|x| (x as usize) < i) => {}
                    Value::Number(_) => bail!("第 {i} 个条目的依赖序号必须指向排在它前面的条目"),
                    Value::String(id) if existing.items.iter().any(|x| &x.id == id) => {}
                    _ => bail!("第 {i} 个条目的依赖 {d} 无效：用本次提交里排在前面的序号，或已有条目的 ID"),
                }
            }
            parsed.push((NewPlanItem { title: title.into(), description: desc.into(), acceptance: acc.into(), kind: kind.into(), module: module.into(), assignee_id, deps: vec![] }, deps));
        }
        // 与已有的（未取消的）条目查重：重复的跳过，依赖它的条目改为依赖那个已有条目
        let mut ids: Vec<Option<String>> = Vec::new();
        let (mut created, mut skipped) = (0, Vec::new());
        for (n, deps) in parsed {
            let mut item = n;
            if let Some(dup) = existing.items.iter().find(|x| x.status != "cancelled" && similarity(&x.title, &item.title) >= PLAN_DUP_THRESHOLD) {
                skipped.push(format!("《{}》与已有的《{}》重复", item.title, dup.title));
                ids.push(Some(dup.id.clone()));
                continue;
            }
            for d in deps {
                match d {
                    Value::Number(k) => { if let Some(Some(id)) = ids.get(k.as_u64().unwrap_or(0) as usize) { item.deps.push(id.clone()); } }
                    Value::String(id) => item.deps.push(id),
                    _ => {}
                }
            }
            item.deps.sort();
            item.deps.dedup();
            let id = self.db.create_plan_item(&self.ctx.project_id, &item, Some(&self.ctx.employee_id))?;
            ids.push(Some(id));
            created += 1;
        }
        self.record("plan", json!({"count": created, "skipped": skipped.len()}));
        Ok(format!("已提交 {created} 个计划条目，它们现在是“待确认”，人类批准后才会进入计划。{}", if skipped.is_empty() { String::new() } else { format!("跳过了 {} 个重复条目：{}", skipped.len(), skipped.join("；")) }))
    }

    fn propose_memory(&self, a: &Value) -> Result<String> {
        let content = arg_str(a, "content").context("缺少 content")?;
        if chars(content) > MAX_MEMORY {
            bail!("内容太长（上限 {MAX_MEMORY} 字），请精简成一两句可直接照做的话");
        }
        let kind = arg_str(a, "kind").unwrap_or("lesson");
        if !KINDS.contains(&kind) {
            bail!("未知的记忆类型 {kind}，可选：{}", KINDS.join("、"));
        }
        let layer = match arg_str(a, "layer").unwrap_or("project") {
            "project" => "project",
            "personal" => "employee",
            other => bail!("layer 只能是 project 或 personal，不能是 {other}（公司级记忆不对 AI 开放）"),
        };
        let pending: i64 = self.db.conn.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM memories WHERE created_by = ?1 AND source_kind = 'agent' AND status = 'unverified' AND (project_id = ?2 OR project_id IS NULL)",
            rusqlite::params![self.ctx.employee_id, self.ctx.project_id], |r| r.get(0))?;
        if pending >= MAX_PENDING_MEMORIES {
            bail!("你已经有 {pending} 条经验在等待人类验证（上限 {MAX_PENDING_MEMORIES}），请等它们被处理后再提议");
        }
        if self.proposals_from_origin("memory")? >= MAX_PER_ORIGIN {
            bail!("这次任务/会议里你已经提议了 {MAX_PER_ORIGIN} 条经验，够了");
        }
        let dup: i64 = self.db.conn.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM memories WHERE status != 'expired' AND content = ?1 AND (project_id = ?2 OR layer = 'company')",
            rusqlite::params![content, self.ctx.project_id], |r| r.get(0))?;
        if dup > 0 {
            bail!("已经有完全相同的记忆了");
        }
        let m = self.db.add_memory(NewMemory {
            layer: layer.into(), project_id: Some(self.ctx.project_id.clone()),
            employee_id: (layer == "employee").then(|| self.ctx.employee_id.clone()),
            kind: kind.into(), title: arg_str(a, "title").unwrap_or("").into(), content: content.into(),
            scope: arg_str(a, "scope").unwrap_or("").into(), source_kind: "agent".into(), source_ref: Some(self.ctx.origin.clone()),
            created_by: self.ctx.employee_id.clone(), status: "unverified".into(), ..Default::default()
        })?;
        self.record("memory", json!({"memory_id": m.id, "title": m.title, "layer": layer}));
        Ok("已提议这条经验。它现在是“待验证”，人类验证之前不会被注入给任何员工。".into())
    }

    /// 阻塞地从 stdin 读取并回复，直到对端关闭。
    pub fn serve_stdio(&self) -> Result<()> {
        let stdin = std::io::stdin();
        let mut out = std::io::stdout().lock();
        for line in stdin.lock().lines() {
            let line = line?;
            let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
            if let Some(resp) = self.handle(&req) {
                writeln!(out, "{resp}")?;
                out.flush()?;
            }
        }
        Ok(())
    }
}

pub fn status_label(s: &str) -> &'static str {
    match s {
        "open" => "待讨论", "discussing" => "讨论中", "pending_decision" => "待决策", "decided" => "已决议",
        "implementing" => "实施中", "closed" => "已关闭", "shelved" => "搁置", "cancelled" => "已取消", _ => "?",
    }
}

/// `mcp-serve` 子命令的入口：解析固定的身份参数，打开数据库，开始服务。
/// 注意：stdout 只能输出协议消息，任何日志都必须写 stderr。
pub fn run_from_args(args: &[String]) -> Result<()> {
    let get = |flag: &str| -> Result<String> {
        args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned().with_context(|| format!("缺少参数 {flag}"))
    };
    let db = Arc::new(Db::open(&PathBuf::from(get("--data-dir")?))?);
    let ctx = McpContext { employee_id: get("--employee")?, project_id: get("--project")?, origin: get("--origin")? };
    McpServer::new(db, ctx).serve_stdio()
}

impl crate::orchestrator::Orchestrator {
    /// 为某次任务/会议构造 MCP 启动参数。没有可执行文件路径（或显式关闭）时返回 None。
    pub fn mcp_launch(&self, employee_id: &str, project_id: &str, origin: &str) -> Option<crate::provider::claude::McpLaunch> {
        let exe = self.mcp_exe.clone()?;
        Some(crate::provider::claude::McpLaunch {
            exe,
            args: ["mcp-serve", "--data-dir", &self.ws.data_dir().to_string_lossy(), "--employee", employee_id, "--project", project_id, "--origin", origin]
                .map(String::from).into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (Arc<Db>, String, String, String) {
        let db = Arc::new(Db::open_memory().unwrap());
        let alice = db.add_employee("alice", "全栈", "p").unwrap();
        let bob = db.add_employee("bob", "全栈", "p").unwrap();
        let p = db.create_project("P", "", "/r").unwrap();
        db.add_member(&p.id, &alice.id, "lead").unwrap();
        db.add_member(&p.id, &bob.id, "developer").unwrap();
        (db, alice.id, bob.id, p.id)
    }
    fn server(db: &Arc<Db>, emp: &str, pid: &str, origin: &str) -> McpServer {
        McpServer::new(db.clone(), McpContext { employee_id: emp.into(), project_id: pid.into(), origin: origin.into() })
    }
    fn call(s: &McpServer, name: &str, args: Value) -> (bool, String) {
        let r = s.handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}})).unwrap();
        (r["result"]["isError"].as_bool().unwrap(), r["result"]["content"][0]["text"].as_str().unwrap().to_string())
    }

    #[test]
    fn protocol_basics() {
        let (db, a, _b, p) = setup();
        let s = server(&db, &a, &p, "t");
        let init = s.handle(&json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}})).unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert!(init["result"]["capabilities"]["tools"].is_object());
        assert_eq!(init["result"]["serverInfo"]["name"], "crewforge");
        assert!(s.handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).is_none(), "通知不回复");
        assert!(s.handle(&json!({"jsonrpc": "2.0", "id": 2, "method": "ping"})).unwrap()["result"].is_object());
        let list = s.handle(&json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"})).unwrap();
        let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["list_issues", "propose_issue", "add_option", "request_decision", "list_plan", "propose_plan", "propose_memory"]);
        assert!(!names.contains(&"decide_issue"), "AI 没有拍板工具");
        for t in list["result"]["tools"].as_array().unwrap() {
            assert_eq!(t["inputSchema"]["type"], "object", "每个工具都有合法的 schema");
        }
        assert_eq!(s.handle(&json!({"jsonrpc": "2.0", "id": 4, "method": "nope"})).unwrap()["error"]["code"], -32601);
    }

    #[test]
    fn propose_issue_creates_a_pending_proposal_attributed_to_the_employee() {
        let (db, a, _b, p) = setup();
        let s = server(&db, &a, &p, "task:t1");
        let (err, text) = call(&s, "propose_issue", json!({"title": "WebSocket 还是 SSE", "summary": "要选推送方案", "kind": "technical",
            "options": [{"title": "WebSocket", "description": "双向"}, {"title": "SSE"}]}));
        assert!(!err, "{text}");
        let v = &db.issue_views(&p).unwrap()[0];
        assert_eq!((v.issue.status.as_str(), v.issue.raised_by.as_deref(), v.issue.kind.as_str()), ("open", Some(a.as_str()), "technical"), "待讨论，等人类确认");
        assert_eq!(v.options.len(), 2);
        assert!(v.options.iter().all(|o| o.proposed_by.as_deref() == Some(a.as_str())));
        assert!(db.pending_decisions().unwrap().is_empty(), "还没到需要拍板的阶段");
        // 项目事件流里留痕，便于群聊展示
        let ev = db.recent_events_for_project(&p, 20).unwrap();
        let e = ev.iter().find(|e| e.kind == "agent_proposed").unwrap();
        assert_eq!((e.payload["type"].as_str(), e.payload["origin"].as_str(), e.employee_id.as_deref()), (Some("issue"), Some("task:t1"), Some(a.as_str())));
    }

    #[test]
    fn invalid_proposals_are_rejected_with_reasons_the_model_can_act_on() {
        let (db, a, _b, p) = setup();
        let s = server(&db, &a, &p, "t");
        for (args, needle) in [
            (json!({"summary": "x"}), "title"),
            (json!({"title": "x"}), "summary"),
            (json!({"title": "x".repeat(81), "summary": "s"}), "太长"),
            (json!({"title": "x", "summary": "s".repeat(601)}), "太长"),
            (json!({"title": "x", "summary": "s", "kind": "weird"}), "未知的议题类型"),
            (json!({"title": "x", "summary": "s", "options": [{"title":"a"},{"title":"b"},{"title":"c"},{"title":"d"},{"title":"e"}]}), "最多带"),
        ] {
            let (err, text) = call(&s, "propose_issue", args);
            assert!(err && text.contains(needle), "{text}");
        }
        assert!(db.list_issues(&p).unwrap().is_empty(), "被拒绝的提议不留任何记录");
    }

    #[test]
    fn duplicates_are_detected_even_when_reworded() {
        assert!(similarity("实时推送用 WebSocket 还是 SSE", "实时推送用 WebSocket 还是 SSE？") >= 0.99);
        assert!(similarity("实时推送用 WebSocket 还是 SSE", "实时推送选 WebSocket 还是 SSE") >= DUP_THRESHOLD);
        assert!(similarity("实时推送用 WebSocket 还是 SSE", "数据库迁移脚本要不要幂等") < 0.3);
        assert_eq!(similarity("", "a"), 0.0);
        let (db, a, b, p) = setup();
        let (e1, _) = call(&server(&db, &a, &p, "t1"), "propose_issue", json!({"title": "实时推送用 WebSocket 还是 SSE", "summary": "s"}));
        assert!(!e1);
        // 另一个员工换个说法再提：被拒绝，并指向已有议题
        let (err, text) = call(&server(&db, &b, &p, "t2"), "propose_issue", json!({"title": "实时推送选 WebSocket 还是 SSE？", "summary": "s"}));
        assert!(err && text.contains("已经有类似的议题") && text.contains("add_option"), "{text}");
        assert_eq!(db.list_issues(&p).unwrap().len(), 1);
        // 已被取消的议题不算重复
        let id = db.list_issues(&p).unwrap()[0].id.clone();
        db.set_issue_status(&id, "cancelled").unwrap();
        assert!(!call(&server(&db, &b, &p, "t3"), "propose_issue", json!({"title": "实时推送选 WebSocket 还是 SSE", "summary": "s"})).0);
    }

    #[test]
    fn rate_limits_per_origin_and_per_employee() {
        let (db, a, _b, p) = setup();
        let topics = ["数据库迁移策略", "日志格式统一", "缓存失效方案", "接口版本管理", "错误码规范", "前端状态管理"];
        let s = server(&db, &a, &p, "task:t1");
        for t in &topics[..3] {
            assert!(!call(&s, "propose_issue", json!({"title": t, "summary": "s"})).0);
        }
        let (err, text) = call(&s, "propose_issue", json!({"title": topics[3], "summary": "s"}));
        assert!(err && text.contains("已经提了 3 个"), "同一次任务最多 3 个：{text}");
        // 换一次任务，但同一员工未处理的提议已有 3 个，继续到 5 个上限
        let s2 = server(&db, &a, &p, "task:t2");
        assert!(!call(&s2, "propose_issue", json!({"title": topics[3], "summary": "s"})).0);
        assert!(!call(&s2, "propose_issue", json!({"title": topics[4], "summary": "s"})).0);
        let (err, text) = call(&s2, "propose_issue", json!({"title": topics[5], "summary": "s"}));
        assert!(err && text.contains("还没处理完"), "未处理的提议上限：{text}");
        // 人类处理掉一个之后，又可以提了
        let id = db.list_issues(&p).unwrap()[0].id.clone();
        db.set_issue_status(&id, "cancelled").unwrap();
        assert!(!call(&s2, "propose_issue", json!({"title": topics[5], "summary": "s"})).0);
    }

    #[test]
    fn identity_is_fixed_and_membership_is_rechecked_every_call() {
        let (db, a, _b, p) = setup();
        let outsider = db.add_employee("zed", "全栈", "p").unwrap();
        let (err, text) = call(&server(&db, &outsider.id, &p, "t"), "propose_issue", json!({"title": "x", "summary": "s"}));
        assert!(err && text.contains("不是这个项目的成员"));
        let (err, _) = call(&server(&db, "ghost", &p, "t"), "list_issues", json!({}));
        assert!(err);
        // 中途被移出项目 / 归档后立刻失效
        let s = server(&db, &a, &p, "t");
        assert!(!call(&s, "list_issues", json!({})).0);
        db.set_employee_archived(&a, true).unwrap();
        assert!(call(&s, "list_issues", json!({})).0);
        // 不能操作别的项目的议题
        let p2 = db.create_project("Q", "", "/q").unwrap();
        let other = db.create_issue(&p2.id, "别的项目", "", "technical", false, None, None, None).unwrap();
        db.set_employee_archived(&a, false).unwrap();
        let (err, text) = call(&server(&db, &a, &p, "t"), "add_option", json!({"issue_id": other.id, "title": "x"}));
        assert!(err && text.contains("不属于当前项目"), "{text}");
        assert!(call(&server(&db, &a, &p, "t"), "bogus", json!({})).1.contains("未知工具"));
    }

    #[test]
    fn add_option_and_request_decision_follow_the_issue_state_machine() {
        let (db, a, b, p) = setup();
        let i = db.create_issue(&p, "选库", "s", "technical", false, None, None, None).unwrap();
        let sa = server(&db, &a, &p, "t");
        let sb = server(&db, &b, &p, "t");
        // 补充方案：可以，重复不行
        assert!(!call(&sb, "add_option", json!({"issue_id": i.id, "title": "serde", "description": "生态好"})).0);
        let (err, text) = call(&sa, "add_option", json!({"issue_id": i.id, "title": "Serde"}));
        assert!(err && text.contains("类似的方案"), "{text}");
        assert_eq!(db.issue_options(&i.id).unwrap()[0].proposed_by.as_deref(), Some(b.as_str()));
        // 还没被人类确认讨论：不能请求拍板
        let (err, text) = call(&sa, "request_decision", json!({"issue_id": i.id}));
        assert!(err && text.contains("还没有被人类确认"), "{text}");
        db.set_issue_status(&i.id, "discussing").unwrap();
        let (err, text) = call(&sa, "request_decision", json!({"issue_id": i.id, "note": "我倾向 serde"}));
        assert!(!err, "{text}");
        assert_eq!(db.get_issue(&i.id).unwrap().unwrap().status, "pending_decision");
        assert_eq!(db.pending_decisions().unwrap().len(), 1, "出现在人类的待办里");
        assert!(call(&sa, "request_decision", json!({"issue_id": i.id})).1.contains("已经在等待拍板"));
        // 已有结论后不能再补充方案
        db.decide_issue(&i.id, &crate::board::Actor::Human, None, "用 serde", "", &Default::default()).unwrap();
        assert!(call(&sa, "add_option", json!({"issue_id": i.id, "title": "另一个"})).0);
        assert!(call(&sa, "request_decision", json!({"issue_id": i.id})).0);
        // list_issues 能看到决议，帮助 AI 避免重复
        assert!(call(&sa, "list_issues", json!({})).1.contains("决议：用 serde"));
    }

    #[test]
    fn proposed_memories_are_unverified_never_company_level_and_not_injected() {
        let (db, a, b, p) = setup();
        let s = server(&db, &a, &p, "task:t1");
        let (err, text) = call(&s, "propose_memory", json!({"title": "Redis 先启动", "content": "集成测试前必须先启动 Redis 缓存服务", "kind": "pitfall"}));
        assert!(!err, "{text}");
        let m = &db.list_memories(&Default::default()).unwrap()[0];
        assert_eq!((m.status.as_str(), m.source_kind.as_str(), m.layer.as_str(), m.created_by.as_str()), ("unverified", "agent", "project", a.as_str()));
        assert_eq!(m.source_ref.as_deref(), Some("task:t1"));
        assert!(m.task_id.is_none(), "不关联任务：不会因为任务合并而被自动验证");
        assert!(db.retrieve_memories(Some(&p), &b, None, "集成测试要启动 redis", 5, 3000).unwrap().is_empty(), "验证之前不会注入任何人");
        db.verify_memory(&m.id, "human").unwrap();
        assert_eq!(db.retrieve_memories(Some(&p), &b, None, "集成测试要启动 redis", 5, 3000).unwrap().len(), 1);
        // 个人记忆；公司级不开放；重复内容；过长
        let (err, _) = call(&s, "propose_memory", json!({"content": "我习惯先写失败的测试", "layer": "personal"}));
        assert!(!err);
        let personal = db.list_memories(&crate::memory::MemoryFilter { layer: Some("employee".into()), ..Default::default() }).unwrap();
        assert_eq!(personal[0].employee_id.as_deref(), Some(a.as_str()));
        assert!(call(&s, "propose_memory", json!({"content": "x", "layer": "company"})).1.contains("公司级记忆不对 AI 开放"));
        assert!(call(&s, "propose_memory", json!({"content": "集成测试前必须先启动 Redis 缓存服务"})).1.contains("完全相同"));
        assert!(call(&s, "propose_memory", json!({"content": "字".repeat(601)})).1.contains("太长"));
        assert!(call(&s, "propose_memory", json!({"content": "y", "kind": "rumor"})).1.contains("未知的记忆类型"));
    }

    #[test]
    fn propose_plan_creates_pending_items_with_dependencies_by_index_and_is_atomic() {
        let (db, a, b, p) = setup();
        let s = server(&db, &a, &p, "meeting:m1");
        let (err, text) = call(&s, "propose_plan", json!({"items": [
            {"title": "定义引擎接口", "description": "d", "acceptance": "cargo test 通过", "kind": "characterize", "module": "engine", "assignee": "alice"},
            {"title": "实现渲染", "depends_on": [0], "assignee": "bob"},
            {"title": "联调", "depends_on": [0, 1], "assignee": "nobody"}]}));
        assert!(!err, "{text}");
        let v = db.plan_view(&p).unwrap();
        assert_eq!((v.items.len(), v.counts.proposed), (3, 3), "全部是待确认，等人类批准");
        let by = |t: &str| v.items.iter().find(|i| i.title == t).unwrap();
        assert_eq!((by("定义引擎接口").kind.as_str(), by("定义引擎接口").assignee_id.as_deref(), by("定义引擎接口").proposed_by.as_deref()), ("characterize", Some(a.as_str()), Some(a.as_str())));
        assert_eq!(by("实现渲染").assignee_id.as_deref(), Some(b.as_str()));
        assert_eq!(by("实现渲染").deps, vec![by("定义引擎接口").id.clone()]);
        assert_eq!(by("联调").deps.len(), 2);
        assert!(by("联调").assignee_id.is_none(), "不存在的名字被忽略");
        assert!(db.recent_events_for_project(&p, 50).unwrap().iter().any(|e| e.kind == "agent_proposed" && e.payload["type"] == "plan" && e.payload["count"] == 3));

        // 校验失败时一个条目都不会创建
        let s2 = server(&db, &b, &p, "meeting:m2");
        for (args, needle) in [
            (json!({"items": []}), "1 到"),
            (json!({"items": [{"title": "x", "depends_on": [0]}]}), "排在它前面"),
            (json!({"items": [{"title": "ok1"}, {"title": "x", "depends_on": ["不存在的id"]}]}), "无效"),
            (json!({"items": [{"title": "ok2"}, {"title": "y".repeat(81)}]}), "太长"),
            (json!({"items": [{"title": "ok3"}, {"title": "z", "kind": "weird"}]}), "未知"),
            (json!({"items": [{"title": "ok4"}, {"description": "没有标题"}]}), "缺少 title"),
        ] {
            let (err, text) = call(&s2, "propose_plan", args);
            assert!(err && text.contains(needle), "{text}");
        }
        assert_eq!(db.plan_view(&p).unwrap().items.len(), 3, "被拒绝的提交不留任何记录");
        // list_plan 能看到现有计划和依赖，帮助模型补充而不是重复
        let (_, listing) = call(&s2, "list_plan", json!({}));
        assert!(listing.contains("《实现渲染》") && listing.contains("等待：定义引擎接口") && listing.contains("待确认 3"), "{listing}");
    }

    #[test]
    fn propose_plan_skips_duplicates_remaps_dependencies_and_limits_calls() {
        let (db, a, b, p) = setup();
        let existing = db.create_plan_item(&p, &crate::plan::NewPlanItem { title: "实现游戏引擎".into(), ..Default::default() }, None).unwrap();
        let s = server(&db, &a, &p, "meeting:m1");
        let (err, text) = call(&s, "propose_plan", json!({"items": [
            {"title": "实现游戏引擎！"}, {"title": "做界面", "depends_on": [0]}]}));
        assert!(!err && text.contains("跳过了 1 个重复"), "{text}");
        let v = db.plan_view(&p).unwrap();
        assert_eq!(v.items.len(), 2, "重复的被跳过");
        assert_eq!(v.items.iter().find(|i| i.title == "做界面").unwrap().deps, vec![existing], "依赖被改指向已有的那个条目");
        // 同一次会议最多提交两次计划
        let (e2, _) = call(&s, "propose_plan", json!({"items": [{"title": "第二份里的新条目"}]}));
        assert!(!e2);
        let (e3, t3) = call(&s, "propose_plan", json!({"items": [{"title": "第三份"}]}));
        assert!(e3 && t3.contains("已经提交过计划"), "{t3}");
        let _ = b;
    }

    #[test]
    fn pending_memory_cap_applies() {
        let (db, a, _b, p) = setup();
        for i in 0..5 {
            let (err, t) = call(&server(&db, &a, &p, &format!("task:{i}")), "propose_memory", json!({"content": format!("第 {i} 条互不相同的经验内容")}));
            assert!(!err, "{t}");
        }
        let (err, text) = call(&server(&db, &a, &p, "task:9"), "propose_memory", json!({"content": "第六条经验内容"}));
        assert!(err && text.contains("等待人类验证"), "{text}");
    }
}
