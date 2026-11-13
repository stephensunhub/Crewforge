//! 长期运转：上线之后的 bug、需求迭代、周期性检查，以及跨项目的总览。
//!
//! - **报告 bug**：一张结构化的 bug 单变成一个 `bugfix` 计划条目。bug 条目不被设计阶段挡住、排在其他条目前面，
//!   派活提示词要求「先复现、再找根因、最小修复、补回归测试」（见 `plan::BUGFIX_RULES`）；
//! - **功能规格库**：每个功能一份 `docs/specs/<名>/requirements.md`（需求与可检查的验收标准）和 `design.md`（方案）。
//!   规格是活文档：需求迭代时「修订规格」，在原文件上改并追加变更记录，再由负责人据此拆出新的实现条目；
//!   实现条目的提示词会指向规格，验收以规格为准；
//! - **定时任务**：每天 / 每周到点，为项目建一个计划条目（依赖与安全检查、测试健康检查……），不重复堆积；
//! - **公司总览**：每个项目的进度、待办、bug、等待、最近一批自主推进，一页看完。

use crate::autopilot::AutopilotRun;
use crate::cost::Totals;
use crate::db::{Db, now_ms};
use crate::orchestrator::Orchestrator;
use crate::plan::{NewPlanItem, PLAN_KINDS};
use crate::usage::UsageFilter;
use anyhow::{Context, Result, bail};
use rusqlite::params;
use serde::Serialize;
use serde_json::json;

pub const MIGRATION_26: &str = r#"
CREATE TABLE specs (
    id             TEXT PRIMARY KEY,
    project_id     TEXT NOT NULL REFERENCES projects(id),
    slug           TEXT NOT NULL,
    title          TEXT NOT NULL,
    brief          TEXT NOT NULL DEFAULT '',
    rounds         INTEGER NOT NULL DEFAULT 0,          -- 建过几轮规格条目（首次 + 每次修订）
    drafted_rounds INTEGER NOT NULL DEFAULT 0,          -- 其中已经由负责人拆出实现条目的轮数
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    UNIQUE (project_id, slug)
);
ALTER TABLE plan_items ADD COLUMN spec_id TEXT NOT NULL DEFAULT '';
CREATE TABLE schedules (
    id           TEXT PRIMARY KEY,
    project_id   TEXT NOT NULL REFERENCES projects(id),
    title        TEXT NOT NULL,
    description  TEXT NOT NULL DEFAULT '',
    kind         TEXT NOT NULL DEFAULT 'other',
    cadence      TEXT NOT NULL,                         -- daily | weekly
    weekday      INTEGER NOT NULL DEFAULT 0,            -- 0 = 周一 … 6 = 周日（weekly 用）
    hour         INTEGER NOT NULL DEFAULT 9,            -- 本地时间的几点
    enabled      INTEGER NOT NULL DEFAULT 1,
    next_run_at  INTEGER NOT NULL,
    last_run_at  INTEGER,
    last_item_id TEXT,
    created_at   INTEGER NOT NULL
);
CREATE INDEX idx_schedules_next ON schedules(enabled, next_run_at);
"#;

// ───────────────────────────── bug ─────────────────────────────

pub const SEVERITIES: &[(&str, &str)] = &[("blocker", "阻断（线上不可用 / 数据错误，要马上修）"), ("major", "严重（主要功能受影响）"), ("minor", "一般（有影响但有绕过办法）")];

#[derive(Debug, Clone, Default)]
pub struct BugReport {
    pub title: String,
    /// 复现步骤
    pub steps: String,
    pub expected: String,
    pub actual: String,
    /// blocker | major | minor
    pub severity: String,
    /// 环境 / 版本 / 日志等补充信息
    pub extra: String,
}

/// 把 bug 单整理成条目说明。
pub fn bug_description(b: &BugReport) -> String {
    let sev = SEVERITIES.iter().find(|(k, _)| *k == b.severity).map(|(_, v)| *v).unwrap_or("未标注");
    let mut s = format!("【Bug 报告】严重程度：{sev}\n");
    for (label, v) in [("复现步骤", &b.steps), ("期望行为", &b.expected), ("实际行为", &b.actual), ("补充信息（版本 / 环境 / 日志）", &b.extra)] {
        if !v.trim().is_empty() { s.push_str(&format!("{label}：{}\n", v.trim())); }
    }
    s.trim_end().to_string()
}

pub const BUG_ACCEPTANCE: &str = "按报告里的步骤，修复前能复现、修复后不再出现；新增或更新了会在修复前失败、修复后通过的自动化测试（确实无法自动化测试的，在说明里写清原因和手工验证步骤）；原有测试仍然全部通过";

impl Db {
    /// 报告一个 bug：建一个待开始的 `bugfix` 条目。
    pub fn report_bug(&self, project_id: &str, b: &BugReport) -> Result<String> {
        if b.title.trim().is_empty() { bail!("请写一句话说明是什么问题"); }
        if b.steps.trim().is_empty() && b.actual.trim().is_empty() { bail!("请至少写明复现步骤或实际看到的现象"); }
        if !b.severity.is_empty() && !SEVERITIES.iter().any(|(k, _)| *k == b.severity) { bail!("未知的严重程度：{}", b.severity); }
        let id = self.create_plan_item(project_id, &NewPlanItem {
            title: format!("修复：{}", b.title.trim()), description: bug_description(b), acceptance: BUG_ACCEPTANCE.into(),
            kind: "bugfix".into(), module: String::new(), assignee_id: None, deps: vec![],
        }, None)?;
        self.set_plan_note(&id, "由 bug 报告创建：不受设计阶段限制，排在其他条目前面")?;
        self.append_event_for(Some(project_id), "bug_reported", None, None, &json!({"item_id": id, "severity": b.severity, "title": b.title.trim(), "text": format!("收到 bug 报告：{}", b.title.trim())}))?;
        Ok(id)
    }
}

// ───────────────────────────── 规格 ─────────────────────────────

pub const SPEC_KIND: &str = "spec";

#[derive(Debug, Clone, Serialize)]
pub struct Spec {
    pub id: String,
    pub project_id: String,
    pub slug: String,
    pub title: String,
    pub brief: String,
    pub rounds: i64,
    pub drafted_rounds: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpecItemRef {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpecView {
    pub spec: Spec,
    pub requirements_path: String,
    pub design_path: String,
    pub items: Vec<SpecItemRef>,
    /// writing（规格撰写中）| to_split（规格做完，等负责人拆实现条目）| building（实现中）| delivered（已交付）
    pub stage: String,
}

pub fn spec_dir(slug: &str) -> String { format!("docs/specs/{slug}") }

/// 标题转成目录名：保留 ASCII 字母数字，其余变 `-`；全是中文等得到空串，由调用方换成编号。
pub fn slugify(title: &str) -> String {
    let mut out = String::new();
    for c in title.trim().chars() {
        if c.is_ascii_alphanumeric() { out.push(c.to_ascii_lowercase()); }
        else if !out.ends_with('-') && !out.is_empty() { out.push('-'); }
    }
    out.trim_matches('-').chars().take(40).collect::<String>().trim_matches('-').to_string()
}

/// 规格条目的说明模板。`change` 为 Some 时是修订：在现有文档上改，并追加变更记录。
pub fn spec_item_descriptions(slug: &str, title: &str, brief: &str, change: Option<&str>) -> (String, String) {
    let dir = spec_dir(slug);
    let rules = "你只写文档：只改 docs/specs 下的 Markdown，不写任何代码。先读项目目标、已有文档和相关代码，没有查证的写“未确认”，不要编造。";
    let (req_head, design_head) = match change {
        None => (format!("为功能「{title}」写需求规格 {dir}/requirements.md。功能简述：\n{brief}\n"),
                 format!("为功能「{title}」写设计 {dir}/design.md（依据已合并的 requirements.md）。")),
        Some(c) => (format!("功能「{title}」的需求变了，请**在现有的** {dir}/requirements.md 上修订（不要推倒重写，保留未变的部分）。本次变更：\n{c}\n"),
                    format!("需求已修订，请**在现有的** {dir}/design.md 上修订，只改受影响的部分。本次变更：\n{}\n", change.unwrap_or(""))),
    };
    let changelog = if change.is_some() { "文末「## 变更记录」追加一条：日期不用写，写“这次改了什么、为什么改、影响哪些验收标准 / 模块”。" } else { "最后一节「## 变更记录」先写一行“初版”，以后每次需求迭代在这里追加。" };
    let req = format!("{rules}\n{req_head}\
写成这些小节：## 背景与目标、## 用户故事（作为…我希望…以便…）、## 验收标准（编号 AC-1、AC-2…，每条都要能用一个具体的操作或命令检查，写明输入和期望结果，不要写“体验良好”这类无法检查的话）、## 不做什么、## 待确认的问题。{changelog}");
    let design = format!("{rules}\n{design_head}\
写成这些小节：## 方案概述、## 涉及的模块和文件（指出具体路径）、## 数据与接口变更、## 实现步骤建议（拆成一名员工一次能做完的小步，标明先后顺序，每步对应哪几条 AC）、## 测试方案（每条 AC 怎么验证）、## 风险与回滚。{changelog}");
    (req, design)
}

pub fn spec_acceptance(slug: &str, file: &str) -> String {
    format!("{}/{file} 存在且含上述各节；只改了文档，没有改动任何非文档文件", spec_dir(slug))
}

fn row_to_spec(r: &rusqlite::Row<'_>) -> rusqlite::Result<Spec> {
    Ok(Spec { id: r.get(0)?, project_id: r.get(1)?, slug: r.get(2)?, title: r.get(3)?, brief: r.get(4)?, rounds: r.get(5)?, drafted_rounds: r.get(6)?, created_at: r.get(7)?, updated_at: r.get(8)? })
}
const SPEC_COLS: &str = "id, project_id, slug, title, brief, rounds, drafted_rounds, created_at, updated_at";

impl Db {
    pub fn get_spec(&self, id: &str) -> Result<Option<Spec>> {
        Ok(self.conn.lock().unwrap().query_row(&format!("SELECT {SPEC_COLS} FROM specs WHERE id = ?1"), [id], row_to_spec).ok())
    }

    /// 新建一个功能规格：建「需求规格」「设计」两个条目（后者依赖前者）。
    pub fn create_spec(&self, project_id: &str, title: &str, brief: &str, slug: Option<&str>) -> Result<String> {
        if title.trim().is_empty() { bail!("请写功能名称"); }
        if brief.trim().is_empty() { bail!("请简要写明这个功能要解决什么问题"); }
        self.get_project(project_id)?.context("项目不存在")?;
        let n: i64 = self.conn.lock().unwrap().query_row("SELECT COUNT(*) FROM specs WHERE project_id = ?1", [project_id], |r| r.get(0))?;
        let slug = match slug.map(slugify).filter(|s| !s.is_empty()) {
            Some(s) => s,
            None => { let s = slugify(title); if s.is_empty() { format!("feature-{}", n + 1) } else { s } }
        };
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_ms();
        let dup = self.conn.lock().unwrap().execute(
            "INSERT INTO specs (id, project_id, slug, title, brief, created_at, updated_at) VALUES (?1,?2,?3,?4,?5,?6,?6)",
            params![id, project_id, slug, title.trim(), brief.trim(), now]);
        if dup.is_err() { bail!("已经有一个目录名为 {slug} 的规格；要改需求请用「修订规格」，或换一个名字"); }
        self.add_spec_round(&id, None)?;
        Ok(id)
    }

    /// 需求迭代：在原规格上再建一轮「修订」条目。
    pub fn revise_spec(&self, spec_id: &str, change: &str) -> Result<()> {
        if change.trim().is_empty() { bail!("请写明需求有什么变化"); }
        let spec = self.get_spec(spec_id)?.context("规格不存在")?;
        if self.spec_view(&spec)?.stage == "writing" { bail!("这个规格的文档还在撰写中，等它做完再修订，或直接取消那两个条目"); }
        self.add_spec_round(spec_id, Some(change.trim()))
    }

    fn add_spec_round(&self, spec_id: &str, change: Option<&str>) -> Result<()> {
        let s = self.get_spec(spec_id)?.context("规格不存在")?;
        let (req, design) = spec_item_descriptions(&s.slug, &s.title, &s.brief, change);
        let who = self.architects(&s.project_id)?.into_iter().next().map(|m| m.employee_id);
        let rev = if change.is_some() { "修订" } else { "" };
        let mut prev: Option<String> = None;
        for (what, file, desc) in [("需求规格", "requirements.md", req), ("设计", "design.md", design)] {
            let deps = prev.iter().cloned().collect();
            let id = self.create_plan_item(&s.project_id, &NewPlanItem {
                title: format!("{rev}{what}：{}", s.title), description: desc, acceptance: spec_acceptance(&s.slug, file),
                kind: SPEC_KIND.into(), module: "docs".into(), assignee_id: who.clone(), deps,
            }, None)?;
            self.set_plan_item_files(&id, &[format!("{}/{file}", spec_dir(&s.slug))])?;
            self.set_plan_item_level(&id, 4)?;
            self.conn.lock().unwrap().execute("UPDATE plan_items SET spec_id = ?2 WHERE id = ?1", params![id, spec_id])?;
            self.set_plan_note(&id, "功能规格：先写清需求和验收标准，再据此拆实现条目")?;
            prev = Some(id);
        }
        self.conn.lock().unwrap().execute("UPDATE specs SET rounds = rounds + 1, updated_at = ?2 WHERE id = ?1", params![spec_id, now_ms()])?;
        self.append_event_for(Some(&s.project_id), if change.is_some() { "spec_revised" } else { "spec_created" }, None, None,
            &json!({"spec_id": spec_id, "text": format!("{}功能规格「{}」", if change.is_some() { "修订" } else { "新建" }, s.title)}))?;
        Ok(())
    }

    /// 给条目挂上规格（负责人按规格拆出的实现条目）。
    pub fn set_plan_item_spec(&self, item_id: &str, spec_id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute("UPDATE plan_items SET spec_id = ?2 WHERE id = ?1", params![item_id, spec_id])?;
        Ok(())
    }

    /// 条目所属的规格。
    pub fn spec_of_item(&self, item_id: &str) -> Result<Option<Spec>> {
        let sid: String = self.conn.lock().unwrap().query_row("SELECT spec_id FROM plan_items WHERE id = ?1", [item_id], |r| r.get(0)).unwrap_or_default();
        if sid.is_empty() { Ok(None) } else { self.get_spec(&sid) }
    }

    fn spec_items(&self, spec_id: &str) -> Result<Vec<SpecItemRef>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id, title, kind, status FROM plan_items WHERE spec_id = ?1 AND status != 'cancelled' ORDER BY position")?;
        let rows = stmt.query_map([spec_id], |r| Ok(SpecItemRef { id: r.get(0)?, title: r.get(1)?, kind: r.get(2)?, status: r.get(3)? }))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    fn spec_view(&self, s: &Spec) -> Result<SpecView> {
        let items = self.spec_items(&s.id)?;
        let docs_open = items.iter().any(|i| i.kind == SPEC_KIND && i.status != "done");
        let impl_items: Vec<&SpecItemRef> = items.iter().filter(|i| i.kind != SPEC_KIND).collect();
        let stage = if docs_open { "writing" }
            else if s.drafted_rounds < s.rounds { "to_split" }
            else if impl_items.iter().any(|i| i.status != "done") { "building" }
            else { "delivered" };
        Ok(SpecView { requirements_path: format!("{}/requirements.md", spec_dir(&s.slug)), design_path: format!("{}/design.md", spec_dir(&s.slug)), spec: s.clone(), items, stage: stage.into() })
    }

    pub fn list_specs(&self, project_id: &str) -> Result<Vec<SpecView>> {
        let specs: Vec<Spec> = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare(&format!("SELECT {SPEC_COLS} FROM specs WHERE project_id = ?1 ORDER BY created_at"))?;
            let rows = stmt.query_map([project_id], row_to_spec)?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        specs.iter().map(|s| self.spec_view(s)).collect()
    }

    /// 规格文档都做完、还没拆实现条目的规格（负责人该拆了）。
    pub fn specs_to_split(&self, project_id: &str) -> Result<Vec<Spec>> {
        Ok(self.list_specs(project_id)?.into_iter().filter(|v| v.stage == "to_split").map(|v| v.spec).collect())
    }

    pub fn mark_spec_split(&self, spec_id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute("UPDATE specs SET drafted_rounds = rounds, updated_at = ?2 WHERE id = ?1", params![spec_id, now_ms()])?;
        Ok(())
    }
}

impl Orchestrator {
    /// 写进负责人拆条目提示词的规格节选；读不到文件就写明路径让负责人自己读。
    pub async fn spec_docs_block(&self, spec: &Spec) -> String {
        let mut out = String::new();
        for f in ["requirements.md", "design.md"] {
            let path = format!("{}/{f}", spec_dir(&spec.slug));
            match self.read_doc(&spec.project_id, &path).await {
                Ok(md) => out.push_str(&format!("\n【{path}】\n{}\n", md.chars().take(5000).collect::<String>())),
                Err(_) => out.push_str(&format!("\n（{path} 读不到，请自己查看仓库里的这个文件）\n")),
            }
        }
        out
    }
}

/// 实现条目提示词里的规格说明。
pub fn spec_prompt_note(spec: &Spec) -> String {
    let dir = spec_dir(&spec.slug);
    format!("\n[功能规格] 这个条目属于功能「{}」，规格已经合并在仓库里：{dir}/requirements.md（需求与验收标准）、{dir}/design.md（方案）。先读它们，你的实现要满足其中和本条目相关的验收标准（AC-n）。发现规格有错、遗漏或和代码现状矛盾，不要自己改规格，在最终说明里写出来。", spec.title)
}

// ───────────────────────────── 定时任务 ─────────────────────────────

pub const CADENCES: &[&str] = &["daily", "weekly"];
const DAY_MS: i64 = 86_400_000;

#[derive(Debug, Clone, Serialize)]
pub struct Schedule {
    pub id: String,
    pub project_id: String,
    pub project_name: String,
    pub title: String,
    pub description: String,
    pub kind: String,
    pub cadence: String,
    pub weekday: i64,
    pub hour: i64,
    pub enabled: bool,
    pub next_run_at: i64,
    pub last_run_at: Option<i64>,
    /// 上次建出来的条目；它还没做完时不会再建新的
    pub last_item_id: Option<String>,
    /// 上次建的条目现在的状态
    pub last_item_status: Option<String>,
}

/// 本机时区相对 UTC 的偏移（分钟）。
pub fn local_offset_minutes(now_ms: i64) -> i64 {
    #[cfg(unix)]
    {
        let t: libc::time_t = (now_ms / 1000) as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        if !unsafe { libc::localtime_r(&t, &mut tm) }.is_null() { return tm.tm_gmtoff as i64 / 60; }
    }
    0
}

/// `after_ms` 之后的下一次触发时间（毫秒，UTC）。`offset_min` 是时区偏移，`weekday` 0 = 周一。
pub fn next_run(after_ms: i64, cadence: &str, weekday: i64, hour: i64, offset_min: i64) -> i64 {
    let off = offset_min * 60_000;
    let local = after_ms + off;
    let day = local.div_euclid(DAY_MS);
    for k in 0..=7 {
        let d = day + k;
        // 1970-01-01 是周四，周一 = 0 时它是 3
        if cadence == "weekly" && (d + 3).rem_euclid(7) != weekday { continue; }
        let cand = d * DAY_MS + hour * 3_600_000;
        if cand > local { return cand - off; }
    }
    after_ms + 7 * DAY_MS
}

/// 常用的定时任务模板：(名称, 类型, 说明, 默认周期)。
pub const SCHEDULE_PRESETS: &[(&str, &str, &str, &str)] = &[
    ("依赖与安全检查", "other", "检查项目依赖有没有已知漏洞或明显过期的版本（用项目自己的工具，例如 cargo audit / npm audit / pip-audit；没有就读依赖清单判断）。只列出确实需要处理的，并升级其中风险低、测试能覆盖的；有破坏性升级的写明原因，不要硬升。", "weekly"),
    ("测试健康检查", "characterize", "完整跑一遍测试和构建，找出偶发失败（多跑几次）、被跳过或长期失败的测试、覆盖明显薄弱的核心模块，补最有价值的几条测试，或修好有问题的测试。不要改业务代码。", "weekly"),
    ("文档与实现一致性检查", "other", "对照代码检查 README 和 docs 里的安装、使用、配置说明是否仍然正确，修正过时的内容；只改文档。", "weekly"),
    ("每日构建冒烟", "characterize", "拉取最新代码，完整构建并运行测试；如果失败，找出是哪次提交引入的，在说明里写清原因和修复建议，能直接修就修。", "daily"),
];

fn row_to_schedule(r: &rusqlite::Row<'_>) -> rusqlite::Result<Schedule> {
    Ok(Schedule {
        id: r.get(0)?, project_id: r.get(1)?, project_name: r.get(2)?, title: r.get(3)?, description: r.get(4)?, kind: r.get(5)?, cadence: r.get(6)?,
        weekday: r.get(7)?, hour: r.get(8)?, enabled: r.get::<_, i64>(9)? != 0, next_run_at: r.get(10)?, last_run_at: r.get(11)?, last_item_id: r.get(12)?, last_item_status: r.get(13)?,
    })
}
const SCHEDULE_SELECT: &str = "SELECT s.id, s.project_id, p.name, s.title, s.description, s.kind, s.cadence, s.weekday, s.hour, s.enabled, s.next_run_at, s.last_run_at, s.last_item_id,
    (SELECT status FROM plan_items WHERE id = s.last_item_id) FROM schedules s JOIN projects p ON p.id = s.project_id";

impl Db {
    pub fn add_schedule(&self, project_id: &str, title: &str, description: &str, kind: &str, cadence: &str, weekday: i64, hour: i64) -> Result<String> {
        if title.trim().is_empty() { bail!("请写定时任务的名称"); }
        if description.trim().is_empty() { bail!("请写清每次要做什么"); }
        if !CADENCES.contains(&cadence) { bail!("周期只能是 daily 或 weekly"); }
        if !PLAN_KINDS.contains(&kind) || kind == crate::design::DESIGN_KIND || kind == SPEC_KIND { bail!("条目类型不合适：{kind}"); }
        if !(0..=6).contains(&weekday) || !(0..=23).contains(&hour) { bail!("星期或小时超出范围"); }
        self.get_project(project_id)?.context("项目不存在")?;
        let now = now_ms();
        let id = uuid::Uuid::new_v4().to_string();
        let next = next_run(now, cadence, weekday, hour, local_offset_minutes(now));
        self.conn.lock().unwrap().execute(
            "INSERT INTO schedules (id, project_id, title, description, kind, cadence, weekday, hour, next_run_at, created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![id, project_id, title.trim(), description.trim(), kind, cadence, weekday, hour, next, now])?;
        Ok(id)
    }

    pub fn list_schedules(&self, project_id: Option<&str>) -> Result<Vec<Schedule>> {
        let conn = self.conn.lock().unwrap();
        let sql = format!("{SCHEDULE_SELECT} WHERE p.status = 'active' {} ORDER BY s.created_at", if project_id.is_some() { "AND s.project_id = ?1" } else { "AND ?1 IS NULL" });
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![project_id], row_to_schedule)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn set_schedule_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        let now = now_ms();
        let s = self.list_schedules(None)?.into_iter().find(|s| s.id == id).context("定时任务不存在")?;
        // 重新启用时从现在起算下一次，不补跑停用期间错过的
        let next = next_run(now, &s.cadence, s.weekday, s.hour, local_offset_minutes(now));
        self.conn.lock().unwrap().execute("UPDATE schedules SET enabled = ?2, next_run_at = CASE WHEN ?2 = 1 THEN ?3 ELSE next_run_at END WHERE id = ?1", params![id, enabled as i64, next])?;
        Ok(())
    }

    pub fn delete_schedule(&self, id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute("DELETE FROM schedules WHERE id = ?1", [id])?;
        Ok(())
    }

    /// 到点的定时任务各建一个条目；上次建的条目还没做完（或还在待开始）就不再堆。返回 (项目 id, 项目名, 条目标题)。
    pub fn run_due_schedules(&self, now: i64) -> Result<Vec<(String, String, String)>> {
        let due: Vec<Schedule> = self.list_schedules(None)?.into_iter().filter(|s| s.enabled && s.next_run_at <= now).collect();
        let mut fired = vec![];
        for s in due {
            let next = next_run(now, &s.cadence, s.weekday, s.hour, local_offset_minutes(now));
            let still_open = matches!(s.last_item_status.as_deref(), Some("proposed" | "planned" | "in_progress" | "blocked"));
            if still_open {
                self.conn.lock().unwrap().execute("UPDATE schedules SET next_run_at = ?2 WHERE id = ?1", params![s.id, next])?;
                self.append_event_for(Some(&s.project_id), "schedule_skipped", None, None, &json!({"schedule_id": s.id, "text": format!("定时任务「{}」到点了，但上一次建的条目还没做完，这次不再重复建", s.title)}))?;
                continue;
            }
            let title = format!("定时：{}", s.title);
            let item = self.create_plan_item(&s.project_id, &NewPlanItem { title: title.clone(), description: s.description.clone(), acceptance: "按说明检查并处理；没发现问题也要在最终说明里写“已检查，无需处理”，并列出检查了什么".into(), kind: s.kind.clone(), module: String::new(), assignee_id: None, deps: vec![] }, None)?;
            self.set_plan_note(&item, &format!("由定时任务「{}」创建", s.title))?;
            self.conn.lock().unwrap().execute("UPDATE schedules SET next_run_at = ?2, last_run_at = ?3, last_item_id = ?4 WHERE id = ?1", params![s.id, next, now, item])?;
            self.append_event_for(Some(&s.project_id), "schedule_fired", None, None, &json!({"schedule_id": s.id, "item_id": item, "text": format!("定时任务「{}」到点，已在计划里建了条目", s.title)}))?;
            fired.push((s.project_id.clone(), s.project_name.clone(), title));
        }
        Ok(fired)
    }
}

// ───────────────────────────── 公司总览 ─────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ProjectOverview {
    pub project_id: String,
    pub name: String,
    pub lead_name: Option<String>,
    pub member_count: usize,
    pub percent_done: u32,
    pub planned: usize,
    pub in_progress: usize,
    pub done: usize,
    pub blocked: usize,
    pub proposed: usize,
    /// 现在就能开始、在等人 / 等自主推进的条目
    pub ready: usize,
    pub bugs_open: usize,
    pub open_issues: i64,
    pub specs: usize,
    pub schedules: usize,
    pub doing: Vec<String>,
    pub last_event_at: Option<i64>,
    pub last_run: Option<AutopilotRun>,
    pub calls_7d: u64,
    pub cost_7d: f64,
    /// 需要你关注的原因；空 = 一切正常
    pub attention: Vec<String>,
}

impl Db {
    /// 每个项目一行的总览。`live` 是自主推进正在跑的项目 id。
    pub fn company_overview(&self, live: &[String]) -> Result<Vec<ProjectOverview>> {
        let now = now_ms();
        let usage = self.usage_stats(&UsageFilter { since: Some(now - 7 * DAY_MS), ..Default::default() }, "project").unwrap_or_default();
        let totals_of = |pid: &str| usage.iter().find(|g| g.key == pid).map(|g| g.totals.clone()).unwrap_or_else(Totals::default);
        let goal_pending: Vec<String> = self.goal_proposal_counts().unwrap_or_default().into_iter().map(|(id, _, _)| id).collect();
        let mut out = vec![];
        for s in self.project_summaries()? {
            let pid = s.project.id.clone();
            let view = self.plan_view(&pid)?;
            let live_now = live.contains(&pid);
            let ready = view.items.iter().filter(|i| i.status == "planned" && i.ready).count();
            let bugs_open = view.items.iter().filter(|i| i.kind == "bugfix" && !matches!(i.status.as_str(), "done" | "cancelled")).count();
            let (open_issues, last_event_at): (i64, Option<i64>) = {
                let conn = self.conn.lock().unwrap();
                let n = conn.query_row("SELECT COUNT(*) FROM issues WHERE project_id = ?1 AND status = 'open'", [&pid], |r| r.get(0)).unwrap_or(0);
                let ts = conn.query_row("SELECT MAX(ts) FROM events WHERE project_id = ?1 OR json_extract(payload,'$.attr_project') = ?1", [&pid], |r| r.get(0)).ok().flatten();
                (n, ts)
            };
            let last_run = self.recent_autopilot_run(&pid)?;
            let t = totals_of(&pid);
            let mut attention = vec![];
            if view.counts.blocked > 0 { attention.push(format!("{} 个条目已阻塞", view.counts.blocked)); }
            if bugs_open > 0 { attention.push(format!("{bugs_open} 个 bug 待修")); }
            if view.counts.proposed > 0 { attention.push(format!("{} 个条目待你确认", view.counts.proposed)); }
            if goal_pending.contains(&pid) { attention.push("有目标变更等你批准".into()); }
            if !live_now && ready > 0 { attention.push(format!("{ready} 个条目可以开始，但自主推进没在运行")); }
            if let Some(r) = &last_run { if !live_now && matches!(r.status.as_str(), "failed" | "escalated") { attention.push(format!("最近一批自主推进以「{}」结束：{}", r.status, r.reason.chars().take(60).collect::<String>())); } }
            out.push(ProjectOverview {
                project_id: pid.clone(), name: s.project.name.clone(), lead_name: s.lead_name.clone(), member_count: s.member_count, percent_done: view.percent_done,
                planned: view.counts.planned, in_progress: view.counts.in_progress, done: view.counts.done, blocked: view.counts.blocked, proposed: view.counts.proposed,
                ready, bugs_open, open_issues, specs: self.list_specs(&pid)?.len(), schedules: self.list_schedules(Some(&pid))?.iter().filter(|x| x.enabled).count(),
                doing: view.items.iter().filter(|i| i.status == "in_progress").map(|i| i.title.clone()).collect(),
                last_event_at, last_run, calls_7d: t.turns, cost_7d: t.cost_usd, attention,
            });
        }
        Ok(out)
    }

    fn recent_autopilot_run(&self, project_id: &str) -> Result<Option<AutopilotRun>> {
        Ok(self.conn.lock().unwrap().query_row(
            "SELECT id, project_id, status, max_calls, max_minutes, calls, started_at, ended_at, reason, uncounted_calls, chain FROM autopilot_runs WHERE project_id = ?1 ORDER BY started_at DESC LIMIT 1",
            [project_id],
            |r| Ok(AutopilotRun { id: r.get(0)?, project_id: r.get(1)?, status: r.get(2)?, max_calls: r.get(3)?, max_minutes: r.get(4)?, calls: r.get(5)?, started_at: r.get(6)?, ended_at: r.get(7)?, reason: r.get(8)?, uncounted_calls: r.get(9)?, chain: r.get(10)? }),
        ).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (Db, String) {
        let db = Db::open_memory().unwrap();
        let p = db.create_project("p", "", "/tmp/p-maint").unwrap().id;
        (db, p)
    }

    #[test]
    fn bug_report_becomes_a_bugfix_item_with_a_regression_test_acceptance() {
        let (db, p) = setup();
        assert!(db.report_bug(&p, &BugReport { title: "  ".into(), ..Default::default() }).is_err());
        assert!(db.report_bug(&p, &BugReport { title: "崩溃".into(), ..Default::default() }).is_err(), "没有步骤也没有现象");
        assert!(db.report_bug(&p, &BugReport { title: "崩溃".into(), actual: "闪退".into(), severity: "huge".into(), ..Default::default() }).is_err());
        let id = db.report_bug(&p, &BugReport { title: "导出时崩溃".into(), steps: "点导出".into(), expected: "生成文件".into(), actual: "闪退".into(), severity: "major".into(), extra: String::new() }).unwrap();
        let it = db.get_plan_item(&id).unwrap().unwrap();
        assert_eq!((it.kind.as_str(), it.status.as_str(), it.title.as_str()), ("bugfix", "planned", "修复：导出时崩溃"));
        assert!(it.description.contains("严重（主要功能受影响）") && it.description.contains("复现步骤：点导出"));
        assert!(it.acceptance.contains("修复前失败、修复后通过的自动化测试"));
    }

    #[test]
    fn slugs_keep_ascii_and_fall_back_to_a_number() {
        assert_eq!(slugify("  User Login & SSO! "), "user-login-sso");
        assert_eq!(slugify("导出报表"), "");
        let (db, p) = setup();
        let a = db.create_spec(&p, "导出报表", "把数据导出成 CSV", None).unwrap();
        assert_eq!(db.get_spec(&a).unwrap().unwrap().slug, "feature-1");
        let b = db.create_spec(&p, "Export CSV", "同上", None).unwrap();
        assert_eq!(db.get_spec(&b).unwrap().unwrap().slug, "export-csv");
        assert!(db.create_spec(&p, "Export CSV", "重名", None).is_err(), "目录名不能重复");
    }

    #[test]
    fn a_spec_goes_through_writing_split_building_delivered_and_can_be_revised() {
        let (db, p) = setup();
        let sid = db.create_spec(&p, "Export CSV", "导出", None).unwrap();
        let v = &db.list_specs(&p).unwrap()[0];
        assert_eq!(v.stage, "writing");
        assert_eq!(v.items.len(), 2);
        let items = db.plan_view(&p).unwrap().items;
        assert!(items.iter().all(|i| i.kind == "spec" && i.level == 4));
        assert_eq!(items[1].deps, vec![items[0].id.clone()], "设计依赖需求");
        assert!(items[0].description.contains("AC-1") && items[0].description.contains("初版"));
        assert!(db.revise_spec(&sid, "加 Excel").is_err(), "撰写中不能修订");
        for i in &items { db.set_plan_status(&i.id, "done", "").unwrap(); }
        assert_eq!(db.list_specs(&p).unwrap()[0].stage, "to_split");
        assert_eq!(db.specs_to_split(&p).unwrap().len(), 1);
        let impl_item = db.create_plan_item(&p, &NewPlanItem { title: "实现导出".into(), kind: "migrate".into(), ..Default::default() }, None).unwrap();
        db.set_plan_item_spec(&impl_item, &sid).unwrap();
        db.mark_spec_split(&sid).unwrap();
        assert_eq!(db.list_specs(&p).unwrap()[0].stage, "building");
        assert_eq!(db.spec_of_item(&impl_item).unwrap().unwrap().id, sid);
        db.set_plan_status(&impl_item, "done", "").unwrap();
        assert_eq!(db.list_specs(&p).unwrap()[0].stage, "delivered");
        // 需求迭代：再来一轮，修订条目说明里保留“在现有文档上改”
        assert!(db.revise_spec(&sid, "").is_err());
        db.revise_spec(&sid, "加 Excel 导出").unwrap();
        let v = &db.list_specs(&p).unwrap()[0];
        assert_eq!((v.stage.as_str(), v.spec.rounds), ("writing", 2));
        let rev = db.plan_view(&p).unwrap().items.into_iter().find(|i| i.title.starts_with("修订需求规格")).unwrap();
        assert!(rev.description.contains("在现有的") && rev.description.contains("加 Excel 导出") && rev.description.contains("追加一条"));
    }

    #[test]
    fn spec_items_go_to_the_architect_when_there_is_one() {
        let (db, p) = setup();
        let arch = db.add_employee("arch", "架构师", "x").unwrap();
        db.add_member(&p, &arch.id, "architect").unwrap();
        db.create_spec(&p, "Feature", "x", None).unwrap();
        assert!(db.plan_view(&p).unwrap().items.iter().all(|i| i.assignee_id.as_deref() == Some(arch.id.as_str())));
    }

    #[test]
    fn next_run_handles_daily_weekly_and_time_zones() {
        // 2026-01-05 是周一。UTC 08:00
        let mon_8 = 1_767_600_000_000i64 - (1_767_600_000_000i64 % DAY_MS) + 8 * 3_600_000;
        let (dow, _) = (((mon_8.div_euclid(DAY_MS)) + 3).rem_euclid(7), ());
        assert_eq!(dow, 0, "测试日期应该是周一");
        assert_eq!(next_run(mon_8, "daily", 0, 9, 0), mon_8 + 3_600_000, "今天 9 点还没到");
        assert_eq!(next_run(mon_8, "daily", 0, 7, 0), mon_8 + 23 * 3_600_000, "今天 7 点已过，明天 7 点");
        assert_eq!(next_run(mon_8, "weekly", 0, 9, 0), mon_8 + 3_600_000, "今天就是周一");
        assert_eq!(next_run(mon_8, "weekly", 0, 7, 0), mon_8 - 3_600_000 + 7 * DAY_MS, "下周一");
        assert_eq!(next_run(mon_8, "weekly", 2, 9, 0), mon_8 + 2 * DAY_MS + 3_600_000, "周三");
        // 东八区本地 16:00（UTC 08:00）：本地 9 点已过，明天本地 9 点 = 明天 UTC 01:00
        assert_eq!(next_run(mon_8, "daily", 0, 9, 480), mon_8 - 7 * 3_600_000 + DAY_MS);
    }

    #[test]
    fn due_schedules_create_one_item_and_do_not_pile_up() {
        let (db, p) = setup();
        assert!(db.add_schedule(&p, "", "x", "other", "daily", 0, 9).is_err());
        assert!(db.add_schedule(&p, "t", "x", "design", "daily", 0, 9).is_err(), "设计类不能定时");
        assert!(db.add_schedule(&p, "t", "x", "other", "monthly", 0, 9).is_err());
        let sid = db.add_schedule(&p, "依赖检查", "查依赖", "other", "daily", 0, 9).unwrap();
        assert!(db.run_due_schedules(now_ms()).unwrap().is_empty(), "还没到点");
        db.conn.lock().unwrap().execute("UPDATE schedules SET next_run_at = 1 WHERE id = ?1", [&sid]).unwrap();
        let fired = db.run_due_schedules(now_ms()).unwrap();
        assert_eq!(fired.len(), 1);
        let items = db.plan_view(&p).unwrap().items;
        assert_eq!((items.len(), items[0].title.as_str(), items[0].status.as_str()), (1, "定时：依赖检查", "planned"));
        let s = &db.list_schedules(Some(&p)).unwrap()[0];
        assert!(s.next_run_at > now_ms() && s.last_item_id.is_some());
        // 上次的条目没做完，下一次到点不再建
        db.conn.lock().unwrap().execute("UPDATE schedules SET next_run_at = 1 WHERE id = ?1", [&sid]).unwrap();
        assert!(db.run_due_schedules(now_ms()).unwrap().is_empty());
        assert_eq!(db.plan_view(&p).unwrap().items.len(), 1);
        // 做完之后再到点就建新的
        db.set_plan_status(&items[0].id, "done", "").unwrap();
        db.conn.lock().unwrap().execute("UPDATE schedules SET next_run_at = 1 WHERE id = ?1", [&sid]).unwrap();
        assert_eq!(db.run_due_schedules(now_ms()).unwrap().len(), 1);
        assert_eq!(db.plan_view(&p).unwrap().items.len(), 2);
        // 停用的不触发
        db.set_schedule_enabled(&sid, false).unwrap();
        db.conn.lock().unwrap().execute("UPDATE schedules SET next_run_at = 1 WHERE id = ?1", [&sid]).unwrap();
        assert!(db.run_due_schedules(now_ms()).unwrap().is_empty());
    }

    #[test]
    fn company_overview_flags_what_needs_attention() {
        let (db, p) = setup();
        let ov = db.company_overview(&[]).unwrap();
        assert_eq!(ov.len(), 1);
        assert!(ov[0].attention.is_empty(), "空项目没有需要关注的");
        db.report_bug(&p, &BugReport { title: "崩".into(), actual: "闪退".into(), ..Default::default() }).unwrap();
        let ov = &db.company_overview(&[]).unwrap()[0];
        assert_eq!((ov.bugs_open, ov.ready), (1, 1));
        assert!(ov.attention.iter().any(|a| a.contains("bug")) && ov.attention.iter().any(|a| a.contains("没在运行")));
        let live = &db.company_overview(&[p.clone()]).unwrap()[0];
        assert!(!live.attention.iter().any(|a| a.contains("没在运行")), "自主推进在跑就不提醒");
    }
}
