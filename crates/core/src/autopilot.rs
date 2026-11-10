//! 自主推进：讨论定稿之后，由项目负责人主导推进项目，人类只旁观和纠偏。
//!
//! 一个“批次”有预算（最多多少次调用、多长时间），用完自动暂停并汇报，人看过再决定是否继续。
//! 负责人这一圈做的事（和之前手动做的一样，只是不再等人点）：
//!   按依赖派计划条目 → 验证（对比基线，失败让负责的员工修）→ 评审会议 + 纪要（负责人主持）→
//!   处理行动项（拦合并的交给任务负责人，其余写明原因留到之后）→ 合并（冲突交给负责人员工解决）→
//!   有两个以上并行任务时先开一次站会。
//! 必须停下来等人的情况：预算用尽、条目反复失败、需求类或高风险议题等人拍板、人手动暂停或停止。
//! 说明：流程由规则驱动，AI 负责人用在会议主持、纪要和站会；让 AI 负责人用模型自己做每一步判断是下一步。

use crate::db::{Db, Employee, Task, now_ms};
use crate::lead_actions::ActionOutcome;
use crate::onboarding::baseline::{CommandResult, Origin, run_commands};
use crate::onboarding::scan::scan_dir;
use crate::orchestrator::Orchestrator;
use crate::plan::PlanItem;
use crate::planning::{AdjustTrigger, Applied};
use crate::ruling::RulingOutcome;
use crate::runner::{Approver, NoopObserver};
use crate::scheduler::{Permit, Scheduler};
use crate::workspace::MergeOutcome;
use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub const MIGRATION_14: &str = r#"
CREATE TABLE autopilot_runs (
    id          TEXT PRIMARY KEY,
    project_id  TEXT NOT NULL REFERENCES projects(id),
    status      TEXT NOT NULL,            -- running | paused | budget | escalated | finished | stopped | failed
    max_calls   INTEGER NOT NULL,
    max_minutes INTEGER NOT NULL,
    calls       INTEGER NOT NULL DEFAULT 0,
    started_at  INTEGER NOT NULL,
    ended_at    INTEGER,
    reason      TEXT NOT NULL DEFAULT ''
);
CREATE INDEX idx_autopilot_project ON autopilot_runs(project_id, started_at);
"#;

/// 迁移 20：批次预算只统计“限制”的调用（另记不限制的）、自动续批次的链、项目级自主推进设置；端点上的“限制批次预算”开关。
pub const MIGRATION_20: &str = r#"
ALTER TABLE llm_endpoints ADD COLUMN limit_budget INTEGER NOT NULL DEFAULT 1;
ALTER TABLE autopilot_runs ADD COLUMN uncounted_calls INTEGER NOT NULL DEFAULT 0;
ALTER TABLE autopilot_runs ADD COLUMN chain INTEGER NOT NULL DEFAULT 0;
CREATE TABLE autopilot_config (
    project_id    TEXT PRIMARY KEY REFERENCES projects(id),
    max_calls     INTEGER NOT NULL DEFAULT 30,
    max_minutes   INTEGER NOT NULL DEFAULT 60,   -- 0 = 不限时间
    auto_continue INTEGER NOT NULL DEFAULT 0,
    max_batches   INTEGER NOT NULL DEFAULT 6     -- 自动续批次最多连续几个（含第一个）
);
"#;

/// 应用重启后被打断的批次的处理。
#[derive(Debug, Clone, PartialEq)]
pub enum ResumeDecision {
    Skip,
    Resume { calls: i64, minutes: i64 },
    FreshBatch { calls: i64, minutes: i64, chain: i64 },
}

/// 批次结束后的下一步。
#[derive(Debug, Clone, PartialEq)]
pub enum AutoContinue {
    No,
    Next { max_calls: i64, max_minutes: i64, chain: i64 },
    LimitReached { batches: i64 },
}

#[derive(Debug, Clone, Serialize)]
pub struct AutopilotConfig {
    pub max_calls: i64,
    pub max_minutes: i64,
    pub auto_continue: bool,
    pub max_batches: i64,
}

impl Default for AutopilotConfig {
    fn default() -> Self { Self { max_calls: 30, max_minutes: 60, auto_continue: false, max_batches: 6 } }
}

#[derive(Debug, Clone, Serialize)]
pub struct AutopilotRun {
    pub id: String,
    pub project_id: String,
    pub status: String,
    pub max_calls: i64,
    pub max_minutes: i64,
    pub calls: i64,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub reason: String,
    /// 没有计入预算的调用数（连的是不限制的端点），只统计
    pub uncounted_calls: i64,
    /// 自动续批次的第几个（人手动开的是 0）
    pub chain: i64,
}

pub fn status_label(s: &str) -> &'static str {
    match s {
        "running" => "推进中",
        "paused" => "已暂停",
        "budget" => "预算用尽，已暂停",
        "escalated" => "等你处理",
        "finished" => "已完成",
        "stopped" => "已停止",
        _ => "出错",
    }
}

impl Db {
    fn run_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<AutopilotRun> {
        Ok(AutopilotRun { id: r.get(0)?, project_id: r.get(1)?, status: r.get(2)?, max_calls: r.get(3)?, max_minutes: r.get(4)?, calls: r.get(5)?, started_at: r.get(6)?, ended_at: r.get(7)?, reason: r.get(8)?, uncounted_calls: r.get(9)?, chain: r.get(10)? })
    }

    /// 项目的自主推进设置（每批次预算、自动续批次）；没设置过用默认值。
    pub fn autopilot_config(&self, project_id: &str) -> Result<AutopilotConfig> {
        Ok(self.conn.lock().unwrap().query_row(
            "SELECT max_calls, max_minutes, auto_continue, max_batches FROM autopilot_config WHERE project_id = ?1", [project_id],
            |r| Ok(AutopilotConfig { max_calls: r.get(0)?, max_minutes: r.get(1)?, auto_continue: r.get::<_, i64>(2)? != 0, max_batches: r.get(3)? })).optional()?.unwrap_or_default())
    }

    pub fn set_autopilot_config(&self, project_id: &str, c: &AutopilotConfig) -> Result<()> {
        if !(1..=1000).contains(&c.max_calls) {
            bail!("每批次的调用预算要在 1 到 1000 之间");
        }
        if !(0..=2880).contains(&c.max_minutes) {
            bail!("每批次的时间预算要在 0 到 2880 分钟之间（0 表示不限时间）");
        }
        if !(1..=50).contains(&c.max_batches) {
            bail!("自动续批次最多连续的批次数要在 1 到 50 之间");
        }
        self.get_project(project_id)?.context("项目不存在")?;
        self.conn.lock().unwrap().execute(
            "INSERT INTO autopilot_config (project_id, max_calls, max_minutes, auto_continue, max_batches) VALUES (?1,?2,?3,?4,?5)
             ON CONFLICT(project_id) DO UPDATE SET max_calls=?2, max_minutes=?3, auto_continue=?4, max_batches=?5",
            params![project_id, c.max_calls, c.max_minutes, c.auto_continue as i64, c.max_batches])?;
        Ok(())
    }

    /// 批次结束后要不要自动开下一个：只在“预算正常用完”时续；遇到升级、失败、完成、人停止都不续。
    pub fn auto_continue_decision(&self, run: &AutopilotRun) -> Result<AutoContinue> {
        if run.status != "budget" {
            return Ok(AutoContinue::No);
        }
        let c = self.autopilot_config(&run.project_id)?;
        if !c.auto_continue {
            return Ok(AutoContinue::No);
        }
        if run.chain + 1 >= c.max_batches {
            return Ok(AutoContinue::LimitReached { batches: run.chain + 1 });
        }
        Ok(AutoContinue::Next { max_calls: c.max_calls, max_minutes: c.max_minutes, chain: run.chain + 1 })
    }

    pub fn create_autopilot_run(&self, project_id: &str, max_calls: i64, max_minutes: i64) -> Result<AutopilotRun> {
        self.create_autopilot_run_chained(project_id, max_calls, max_minutes, 0)
    }

    pub fn create_autopilot_run_chained(&self, project_id: &str, max_calls: i64, max_minutes: i64, chain: i64) -> Result<AutopilotRun> {
        let id = uuid::Uuid::new_v4().to_string();
        self.conn.lock().unwrap().execute(
            "INSERT INTO autopilot_runs (id, project_id, status, max_calls, max_minutes, started_at, chain) VALUES (?1,?2,'running',?3,?4,?5,?6)",
            params![id, project_id, max_calls, max_minutes, now_ms(), chain])?;
        Ok(self.get_autopilot_run(&id)?.context("批次创建失败")?)
    }

    pub fn get_autopilot_run(&self, id: &str) -> Result<Option<AutopilotRun>> {
        Ok(self.conn.lock().unwrap().query_row("SELECT id, project_id, status, max_calls, max_minutes, calls, started_at, ended_at, reason, uncounted_calls, chain FROM autopilot_runs WHERE id = ?1", [id], Self::run_row).ok())
    }

    /// 项目最近一个批次。
    pub fn latest_autopilot_run(&self, project_id: &str) -> Result<Option<AutopilotRun>> {
        Ok(self.conn.lock().unwrap().query_row(
            "SELECT id, project_id, status, max_calls, max_minutes, calls, started_at, ended_at, reason, uncounted_calls, chain FROM autopilot_runs WHERE project_id = ?1 ORDER BY started_at DESC, rowid DESC LIMIT 1",
            [project_id], Self::run_row).ok())
    }

    /// 运行中更新两个计数：计入预算的调用数和只统计不限制的调用数。
    pub fn set_autopilot_counts(&self, id: &str, calls: i64, uncounted: i64) -> Result<()> {
        self.conn.lock().unwrap().execute("UPDATE autopilot_runs SET calls = ?2, uncounted_calls = ?3 WHERE id = ?1", params![id, calls, uncounted])?;
        Ok(())
    }

    pub fn set_autopilot_run(&self, id: &str, status: &str, calls: i64, reason: &str) -> Result<()> {
        let end = !matches!(status, "running");
        self.conn.lock().unwrap().execute(
            "UPDATE autopilot_runs SET status = ?2, calls = ?3, reason = ?4, ended_at = CASE WHEN ?5 = 1 THEN ?6 ELSE NULL END WHERE id = ?1",
            params![id, status, calls, reason, end as i64, now_ms()])?;
        Ok(())
    }

    /// 应用重启后是否自动接着推进（按项目设置，默认开启）。
    pub fn autopilot_resume_enabled(&self, project_id: &str) -> bool {
        self.get_setting(&format!("autopilot_resume.{project_id}")).ok().flatten().as_deref() != Some("off")
    }

    pub fn set_autopilot_resume(&self, project_id: &str, enabled: bool) -> Result<()> {
        self.set_setting(&format!("autopilot_resume.{project_id}"), if enabled { "on" } else { "off" })
    }

    /// 应用启动时调用：把上次还在“推进中”的批次标为已停止（进程已经随应用退出），并返回它们，
    /// 由调用方决定要不要接着推进。
    pub fn take_orphaned_autopilot_runs(&self) -> Result<Vec<AutopilotRun>> {
        let runs: Vec<AutopilotRun> = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare("SELECT id, project_id, status, max_calls, max_minutes, calls, started_at, ended_at, reason, uncounted_calls, chain FROM autopilot_runs WHERE status = 'running'")?;
            let rows = stmt.query_map([], Self::run_row)?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        self.close_orphaned_autopilot_runs()?;
        Ok(runs)
    }

    /// 被中断的批次要不要自动恢复，以及恢复时的剩余预算 (调用数, 分钟)。
    /// 不恢复的情况：项目里关了这个选项；剩余预算太少；这个批次启动不到一分钟就被中断
    /// （避免应用反复崩溃时不停地重新开始）。
    pub fn plan_autopilot_resume(&self, run: &AutopilotRun, now: i64) -> Option<(i64, i64)> {
        if !self.autopilot_resume_enabled(&run.project_id) {
            return None;
        }
        let ran_ms = now - run.started_at;
        if ran_ms < 60_000 {
            return None;
        }
        let calls = run.max_calls - run.calls;
        // 时间预算为 0 表示不限时间：只看调用数
        let minutes = if run.max_minutes == 0 { 0 } else { run.max_minutes - ran_ms / 60_000 };
        (calls >= 3 && (run.max_minutes == 0 || minutes >= 5)).then_some((calls, minutes))
    }

    /// 应用重启后被打断的批次怎么办：剩余预算够就接着做；不够但项目开了自动续批次就开一个新批次（预算按项目设置）；否则不动。
    pub fn resume_decision(&self, run: &AutopilotRun, now: i64) -> Result<ResumeDecision> {
        if !self.autopilot_resume_enabled(&run.project_id) || now - run.started_at < 60_000 {
            return Ok(ResumeDecision::Skip);
        }
        if let Some((calls, minutes)) = self.plan_autopilot_resume(run, now) {
            return Ok(ResumeDecision::Resume { calls, minutes });
        }
        let c = self.autopilot_config(&run.project_id)?;
        if c.auto_continue && run.chain + 1 < c.max_batches {
            return Ok(ResumeDecision::FreshBatch { calls: c.max_calls, minutes: c.max_minutes, chain: run.chain + 1 });
        }
        Ok(ResumeDecision::Skip)
    }

    /// 应用重启后，上次还在“推进中”的批次已经没有进程在跑：标为已停止。
    pub fn close_orphaned_autopilot_runs(&self) -> Result<usize> {
        Ok(self.conn.lock().unwrap().execute("UPDATE autopilot_runs SET status = 'stopped', reason = '应用重启，推进已停止', ended_at = ?1 WHERE status = 'running'", [now_ms()])?)
    }
}

/// 人对一个批次的控制：暂停（让手上的做完就停）和停止。
#[derive(Default)]
pub struct AutopilotCtl {
    pub pause: AtomicBool,
    pub stop: AtomicBool,
}

pub struct Autopilot<A: Approver + Send + Sync + 'static> {
    pub orch: Arc<Orchestrator>,
    pub sched: Arc<Scheduler>,
    pub approver: Arc<A>,
    pub ctl: Arc<AutopilotCtl>,
    pub project_id: String,
    pub run_id: String,
    repo: std::path::PathBuf,
    calls: AtomicUsize,
    started: Instant,
    max_calls: usize,
    max_minutes: u64,
    merge_lock: tokio::sync::Mutex<()>,
    standup_lock: tokio::sync::Mutex<()>,
    merged: AtomicUsize,
    last_standup_at_merged: AtomicUsize,
    last_curated_at_merged: AtomicUsize,
    last_triaged_at_merged: AtomicUsize,
    /// 没有计入预算的调用数（员工连的是不限制的端点）
    uncounted: AtomicUsize,
    standups: AtomicUsize,
    /// 正在被占用的员工名（与应用里“工作中”的集合共用，避免人和负责人同时驱动同一员工的会话）
    /// 本批次里已经派出去处理的评审意见文本（并行的任务里相同的意见只处理一次）
    handled_actions: std::sync::Mutex<Vec<String>>,
    busy: Arc<std::sync::Mutex<HashSet<String>>>,
    on_busy_change: Arc<dyn Fn(Vec<String>) + Send + Sync>,
}

/// 每个条目最多让员工处理几条拦合并的评审意见；超出的降级为“之后再说”。
pub const MAX_ACTIONS_PER_TASK: usize = 3;
/// 调用数或时间用到预算的这个比例之后，进入“软预算”：不再处理评审意见、不再开站会和整理记忆。
pub const SOFT_BUDGET_RATIO: f64 = 0.8;
/// 给收尾（整理记忆、更新文档）留的调用数：开始新条目前先扣掉，保证预算用尽时收尾也能做。
pub const WRAP_UP_RESERVE: usize = 4;

fn bigrams(s: &str) -> std::collections::HashSet<(char, char)> {
    let cs: Vec<char> = s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect();
    cs.windows(2).map(|w| (w[0], w[1])).collect()
}

/// 两条评审意见是不是同一件事：字符二元组的 Jaccard 相似度。
pub fn similar_actions(a: &str, b: &str) -> bool {
    let (x, y) = (bigrams(a), bigrams(b));
    if x.is_empty() || y.is_empty() { return false; }
    let inter = x.intersection(&y).count() as f64;
    inter / x.union(&y).count() as f64 >= 0.55
}

fn clip(s: &str, n: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= n { one } else { format!("{}…", one.chars().take(n).collect::<String>()) }
}

/// 见 `Autopilot::busy_scope`。
struct BusyScope {
    busy: Arc<std::sync::Mutex<HashSet<String>>>,
    notify: Arc<dyn Fn(Vec<String>) + Send + Sync>,
    name: String,
    owned: bool,
}

impl Drop for BusyScope {
    fn drop(&mut self) {
        if !self.owned { return; }
        let names: Vec<String> = {
            let mut b = self.busy.lock().unwrap();
            b.remove(&self.name);
            b.iter().cloned().collect()
        };
        (self.notify)(names);
    }
}

impl<A: Approver + Send + Sync + 'static> Autopilot<A> {
    /// 开始前的检查：项目要有 AI 负责人；此时点“开始”就是人对计划的最后确认——提议的条目一并批准。
    pub fn prepare(orch: &Arc<Orchestrator>, project_id: &str, max_calls: i64, max_minutes: i64) -> Result<AutopilotRun> {
        Self::prepare_inner(orch, project_id, max_calls, max_minutes, true, 0)
    }

    /// 应用重启后接着推进：条件与 `prepare` 相同，但不批准计划——人当初批准的是那时的计划，
    /// 之后 AI 新提议的条目仍然要等人确认。
    pub fn prepare_resume(orch: &Arc<Orchestrator>, project_id: &str, max_calls: i64, max_minutes: i64) -> Result<AutopilotRun> {
        Self::prepare_inner(orch, project_id, max_calls, max_minutes, false, 0)
    }

    /// 自动续批次：同样不批准计划；`chain` 是这一串自动续批次里的第几个。
    pub fn prepare_continue(orch: &Arc<Orchestrator>, project_id: &str, max_calls: i64, max_minutes: i64, chain: i64) -> Result<AutopilotRun> {
        Self::prepare_inner(orch, project_id, max_calls, max_minutes, false, chain)
    }

    fn prepare_inner(orch: &Arc<Orchestrator>, project_id: &str, max_calls: i64, max_minutes: i64, approve: bool, chain: i64) -> Result<AutopilotRun> {
        let db = &orch.db;
        let project = db.get_project(project_id)?.context("项目不存在")?;
        if project.status != "active" {
            bail!("项目已归档");
        }
        if project.onboarding == "in_progress" {
            bail!("项目还在导入阶段，请先在“导入”页确认导入");
        }
        let lead = db.list_members(project_id)?.into_iter().find(|m| m.role == "lead");
        let Some(lead) = lead else { bail!("项目还没有负责人。请先在“成员”页指定一位 AI 员工为负责人（由人担任负责人时，仍由你手动推进）") };
        let _ = db.employee_by_id(&lead.employee_id)?.context("负责人不存在")?;
        if let Some(run) = db.latest_autopilot_run(project_id)? {
            if run.status == "running" {
                bail!("已经有一个批次在推进中");
            }
        }
        if max_calls < 1 || max_minutes < 0 {
            bail!("调用预算必须大于 0；时间预算不能为负（0 表示不限时间）");
        }
        let view = db.plan_view(project_id)?;
        if view.items.iter().filter(|i| i.status != "cancelled").count() == 0 {
            bail!("项目还没有计划。请先在“计划”页点“让负责人起草计划”，或手动添加条目");
        }
        let approved = if approve { db.approve_proposed_plan(project_id)? } else { 0 };
        let run = db.create_autopilot_run_chained(project_id, max_calls, max_minutes, chain)?;
        db.append_event_for(Some(project_id), "autopilot_started", None, None, &json!({"run_id": run.id, "text": format!("负责人{}自主推进（预算 {max_calls} 次调用 / {}）{}", if approve { "开始" } else if chain > 0 { "自动续批次" } else { "接着" }, if max_minutes > 0 { format!("{max_minutes} 分钟") } else { "不限时间".to_string() }, if approved > 0 { format!("，并批准了 {approved} 个提议的计划条目") } else { String::new() }), "max_calls": max_calls, "max_minutes": max_minutes}))?;
        Ok(run)
    }

    pub fn new(orch: Arc<Orchestrator>, sched: Arc<Scheduler>, approver: Arc<A>, ctl: Arc<AutopilotCtl>, run: &AutopilotRun) -> Result<Arc<Self>> {
        Self::new_with_busy(orch, sched, approver, ctl, run, Arc::new(std::sync::Mutex::new(HashSet::new())), Arc::new(|_| {}))
    }

    /// 与应用共用“员工是否忙碌”的集合：忙碌的员工不派活，派出去的会登记为忙碌并通知界面。
    pub fn new_with_busy(orch: Arc<Orchestrator>, sched: Arc<Scheduler>, approver: Arc<A>, ctl: Arc<AutopilotCtl>, run: &AutopilotRun,
        busy: Arc<std::sync::Mutex<HashSet<String>>>, on_busy_change: Arc<dyn Fn(Vec<String>) + Send + Sync>) -> Result<Arc<Self>> {
        let project = orch.db.get_project(&run.project_id)?.context("项目不存在")?;
        // 自主推进里派出的活是 AI 负责人发起的，群里不能显示成人说的话
        let orch = { let mut o = (*orch).clone(); o.origin = "autopilot".into(); Arc::new(o) };
        Ok(Arc::new(Self {
            orch, sched, approver, ctl, project_id: run.project_id.clone(), run_id: run.id.clone(), repo: std::path::PathBuf::from(project.path),
            calls: AtomicUsize::new(0), started: Instant::now(), max_calls: run.max_calls as usize, max_minutes: run.max_minutes as u64, merge_lock: tokio::sync::Mutex::new(()), standup_lock: tokio::sync::Mutex::new(()),
            merged: AtomicUsize::new(0), last_standup_at_merged: AtomicUsize::new(0), last_curated_at_merged: AtomicUsize::new(0), last_triaged_at_merged: AtomicUsize::new(0), uncounted: AtomicUsize::new(0), standups: AtomicUsize::new(0),
            handled_actions: std::sync::Mutex::new(Vec::new()), busy, on_busy_change,
        }))
    }

    fn db(&self) -> &Db {
        &self.orch.db
    }

    fn say(&self, kind: &str, text: impl std::fmt::Display) {
        let _ = self.db().append_event_for(Some(&self.project_id), kind, None, None, &json!({"run_id": self.run_id, "text": text.to_string()}));
    }

    /// 这位员工的调用是否计入批次预算：订阅员工一定计入；连自建端点的看端点上的“限制批次预算”开关（默认限制）。
    fn counts_toward_budget(&self, emp: Option<&Employee>) -> bool {
        match emp.map(|e| self.db().resolve_endpoint(e)) {
            Some(Ok(Some(r))) => r.endpoint.limit_budget,
            _ => true,
        }
    }

    /// 拿一个并发名额并记一次调用：计入预算的加到 `calls`，不限制的只加到 `uncounted`（统计，不限制）。
    async fn permit_for(&self, emp: Option<&Employee>) -> Permit {
        let p = self.sched.acquire().await;
        if self.counts_toward_budget(emp) { self.calls.fetch_add(1, Ordering::SeqCst); } else { self.uncounted.fetch_add(1, Ordering::SeqCst); }
        let _ = self.db().set_autopilot_counts(&self.run_id, self.calls.load(Ordering::SeqCst) as i64, self.uncounted.load(Ordering::SeqCst) as i64);
        p
    }

    fn over_budget(&self) -> Option<String> {
        if self.calls.load(Ordering::SeqCst) >= self.max_calls {
            return Some(format!("调用次数达到预算 {}", self.max_calls));
        }
        if self.max_minutes > 0 && self.started.elapsed() > Duration::from_secs(self.max_minutes * 60) {
            return Some(format!("时间达到预算 {} 分钟", self.max_minutes));
        }
        None
    }

    /// 软预算：用到预算的 80% 之后，只做必须做的（开发、验证、评审、合并），可选的步骤一律跳过。
    fn soft_budget(&self) -> bool {
        self.calls.load(Ordering::SeqCst) as f64 >= self.max_calls as f64 * SOFT_BUDGET_RATIO
            || (self.max_minutes > 0 && self.started.elapsed().as_secs_f64() >= self.max_minutes as f64 * 60.0 * SOFT_BUDGET_RATIO)
    }

    /// 剩下的调用数不够再做一个条目（按本批次已合并条目的平均调用数估计）：不再开始新条目，避免超出预算。
    fn no_room_for_another_item(&self) -> Option<String> {
        let merged = self.merged.load(Ordering::SeqCst);
        if merged == 0 { return None; }
        let used = self.calls.load(Ordering::SeqCst);
        let avg = (used as f64 / merged as f64).ceil() as usize;
        let remaining = self.max_calls.saturating_sub(used + WRAP_UP_RESERVE);
        (remaining < avg).then(|| format!("预算（扣掉收尾要留的 {WRAP_UP_RESERVE} 次）只剩 {remaining} 次调用，不够再做一个条目（本批次每个条目平均约 {avg} 次）"))
    }

    /// 这条评审意见是不是和已经处理（或正在处理）的某条是同一件事。
    fn is_duplicate_action(&self, text: &str) -> bool {
        if self.handled_actions.lock().unwrap().iter().any(|t| similar_actions(t, text)) { return true; }
        self.db().recent_done_action_texts(&self.project_id, 80).unwrap_or_default().iter().any(|t| similar_actions(t, text))
    }

    fn members(&self) -> Vec<(Employee, String)> {
        self.db().list_members(&self.project_id).unwrap_or_default().into_iter().filter_map(|m| self.db().employee_by_id(&m.employee_id).ok().flatten().map(|e| (e, m.role))).collect()
    }

    fn pick(&self, item: &PlanItem, busy: &HashSet<String>) -> Option<Employee> {
        let ms = self.members();
        let human_busy = self.busy.lock().unwrap().clone();
        let busy_for = |e: &Employee| busy.contains(&e.id) || human_busy.contains(&e.name);
        if let Some(a) = &item.assignee_id {
            return ms.iter().find(|(e, _)| &e.id == a && !busy_for(e)).map(|(e, _)| e.clone());
        }
        let qa_like = item.kind == "characterize" || ["测试", "QA", "质检", "评审", "验收"].iter().any(|k| item.title.contains(k));
        let mut order: Vec<&(Employee, String)> = ms.iter().filter(|(_, r)| (r == "qa") == qa_like).collect();
        order.extend(ms.iter().filter(|(_, r)| (r == "qa") != qa_like));
        order.into_iter().find(|(e, _)| !busy_for(e)).map(|(e, _)| e.clone())
    }

    fn is_merged(&self, task_id: &str) -> bool {
        self.db().get_task(task_id).ok().flatten().map(|t| t.status == "merged").unwrap_or(false)
    }

    async fn work(&self, emp: &Employee, prompt: &str) -> Result<()> {
        let _p = self.permit_for(Some(emp)).await;
        let r = self.orch.assign(emp, &self.repo, prompt, &*self.approver, &NoopObserver).await?;
        if r.is_error {
            bail!("{} 本轮出错：{}", emp.name, r.subtype);
        }
        Ok(())
    }

    /// 在干净检出里验证一个分支（没有基线时用）。
    async fn verify_branch(&self, branch: &str) -> (bool, String) {
        let dest = self.orch.ws.scratch_dir("verify").join(branch.replace('/', "_"));
        self.orch.ws.remove_checkout(&self.repo, &dest).await;
        if let Err(e) = self.orch.ws.add_detached(&self.repo, &dest, branch).await {
            return (false, format!("无法检出 {branch}: {e}"));
        }
        let cmds: Vec<_> = scan_dir(&dest).commands.into_iter().filter(|c| c.default_on && c.kind != "lint").collect();
        let results = if cmds.is_empty() { vec![] } else { run_commands(&dest, &cmds, Duration::from_secs(900), |_| {}).await };
        self.orch.ws.remove_checkout(&self.repo, &dest).await;
        let bad: Vec<&CommandResult> = results.iter().filter(|r| r.status != "passed" && r.status != "skipped").collect();
        let summary = if cmds.is_empty() { "没有可运行的构建/测试命令".to_string() } else {
            bad.iter().map(|r| format!("命令 `{}` {}：\n{}", r.command, r.status, r.output_tail.chars().rev().take(1800).collect::<Vec<_>>().into_iter().rev().collect::<String>())).collect::<Vec<_>>().join("\n---\n")
        };
        (bad.is_empty(), summary)
    }

    /// 验证一个进行中的任务：有基线就与基线对比（引入新失败或无法确认都算不通过），否则直接构建测试。
    async fn verify(&self, task: &Task) -> (bool, String) {
        if self.is_merged(&task.id) {
            return (true, "任务已被合并".into());
        }
        if self.db().current_baseline(&self.project_id).ok().flatten().is_some() {
            match self.orch.check_task(&task.id, Duration::from_secs(900)).await {
                Ok(rep) => {
                    let ok = rep.introduced == 0 && rep.unknown == 0;
                    let detail = rep.findings.iter().filter(|f| f.origin != Origin::Fixed).map(|f| format!("{:?} · {} · {} · {}", f.origin, f.command_label, f.item, f.note)).collect::<Vec<_>>().join("\n");
                    let tails: String = rep.results.iter().filter(|r| r.status != "passed" && r.status != "skipped").map(|r| format!("`{}` 输出末尾：\n{}", r.command, r.output_tail.chars().rev().take(1500).collect::<Vec<_>>().into_iter().rev().collect::<String>())).collect::<Vec<_>>().join("\n---\n");
                    (ok, format!("与基线对比：引入 {} · 已存在 {} · 已修复 {} · 无法确认 {}\n{detail}\n{tails}", rep.introduced, rep.pre_existing, rep.fixed, rep.unknown))
                }
                Err(e) => (false, format!("检查失败：{e}")),
            }
        } else {
            self.verify_branch(&task.branch).await
        }
    }

    /// 第一个任务合并之后建立测试基线，之后的任务用“对比基线检查”把关。
    async fn ensure_baseline(&self) {
        if self.db().current_baseline(&self.project_id).ok().flatten().is_some() {
            return;
        }
        let Ok(scan) = self.orch.scan_project(&self.project_id).await else { return };
        let cmds: Vec<_> = scan.commands.into_iter().filter(|c| c.default_on && c.kind != "lint").collect();
        if cmds.is_empty() {
            return;
        }
        self.say("autopilot_step", format!("建立测试基线：{}", cmds.iter().map(|c| c.command.clone()).collect::<Vec<_>>().join("、")));
        if let Err(e) = self.orch.run_baseline(&self.project_id, cmds, Duration::from_secs(900)).await {
            self.say("autopilot_step", format!("基线没有建立：{e}"));
        }
    }

    /// 员工在做后台工作（裁决、整理、写文档、开会发言…）期间标记为“工作中”，离开作用域自动取消。
    /// 已经因为别的活被标记的不重复标记，也不会替它取消。
    fn busy_scope(&self, name: &str) -> BusyScope {
        let owned = {
            let mut b = self.busy.lock().unwrap();
            let inserted = b.insert(name.to_string());
            if inserted { let names: Vec<String> = b.iter().cloned().collect(); drop(b); (self.on_busy_change)(names); }
            inserted
        };
        BusyScope { busy: self.busy.clone(), notify: self.on_busy_change.clone(), name: name.to_string(), owned }
    }

    fn mark_busy(&self, name: &str, on: bool) {
        let names: Vec<String> = {
            let mut b = self.busy.lock().unwrap();
            if on { b.insert(name.to_string()); } else { b.remove(name); }
            b.iter().cloned().collect()
        };
        (self.on_busy_change)(names);
    }

    async fn process_item(self: &Arc<Self>, item_id: String, emp: Employee) -> Result<()> {
        self.mark_busy(&emp.name, true);
        let r = self.process_item_inner(item_id, emp.clone()).await;
        self.mark_busy(&emp.name, false);
        r
    }

    /// 一个计划条目的完整流程：开发 → 验证（失败则修）→ 评审 → 处理行动项 → 合并。
    async fn process_item_inner(self: &Arc<Self>, item_id: String, emp: Employee) -> Result<()> {
        let item = self.db().get_plan_item(&item_id)?.context("条目不存在")?;
        self.say("autopilot_step", format!("派《{}》给 {}", item.title, emp.name));
        let (emp, path, prompt) = self.orch.prepare_plan_dispatch(&item_id, Some(&emp.id))?;
        {
            let _p = self.permit_for(Some(&emp)).await;
            let r = self.orch.assign_for_plan(&emp, &path, &prompt, &item_id, &*self.approver, &NoopObserver).await?;
            self.say("autopilot_step", format!("{} 做完《{}》第一轮：{}", emp.name, item.title, clip(&r.text, 140)));
        }
        let task = self.db().get_task(self.db().get_plan_item(&item_id)?.context("条目不存在")?.task_id.as_deref().context("条目没有任务")?)?.context("任务不存在")?;
        self.maybe_standup().await;

        let mut fixes = 0;
        let mut verdict = self.verify(&task).await;
        while !verdict.0 && fixes < 3 && !self.ctl.stop.load(Ordering::SeqCst) {
            self.say("autopilot_step", format!("《{}》验证未通过（第 {} 次），让 {} 修：{}", item.title, fixes + 1, emp.name, clip(&verdict.1, 160)));
            self.work(&emp, &format!("验证没有通过，请修复。验证输出：\n{}\n如果失败是因为发现了代码里真实的 bug、而修复超出了这个条目的范围，请把对应的测试用 #[ignore]（或对应语言的跳过标记）标注并在说明里写清复现方式，不要提交会失败的测试。修好后运行测试确认。", verdict.1)).await?;
            fixes += 1;
            verdict = self.verify(&task).await;
        }
        if !verdict.0 {
            self.db().set_plan_status(&item_id, "blocked", &format!("验证仍未通过：{}", clip(&verdict.1, 300)))?;
            bail!("《{}》验证仍未通过", item.title);
        }

        // 评审：QA 评审开发者；QA 自己的任务由一位非 QA 的成员评审；会议由负责人主持
        let ms = self.members();
        let lead = ms.iter().find(|(_, r)| r == "lead").map(|(e, _)| e.clone());
        let reviewer = if ms.iter().any(|(e, r)| r == "qa" && e.id == emp.id) {
            ms.iter().find(|(e, r)| r != "qa" && e.id != emp.id).map(|(e, _)| e.clone())
        } else {
            ms.iter().find(|(e, r)| r == "qa" && e.id != emp.id).or_else(|| ms.iter().find(|(e, _)| e.id != emp.id)).map(|(e, _)| e.clone())
        };
        if let Some(rv) = reviewer {
            let m = self.db().create_meeting(&self.project_id, "code_review", &format!("评审：{}", item.title), lead.as_ref().map(|l| l.id.as_str()), Some(&task.id), &[], 6)?;
            self.db().post_message(&m.id, "human", None, &format!("@{} 请评审 {} 的改动《{}》。重点看：逻辑是否正确、边界情况、测试是否充分、是否符合项目目标；发现需要团队决定的设计问题请先 list_issues 再 propose_issue。（这条消息由负责人流程自动发出）", rv.name, emp.name, item.title))?;
            {
                let _p = self.permit_for(Some(&rv)).await;
                let _busy = self.busy_scope(&rv.name);
                match self.orch.meeting_reply(&m.id, &rv, &*self.approver).await {
                    Ok(t) => self.say("autopilot_step", format!("{} 评审《{}》：{}", rv.name, item.title, clip(&t, 200))),
                    Err(e) => self.say("autopilot_step", format!("评审没有完成：{e}")),
                }
            }
            {
                let _p = self.permit_for(lead.as_ref()).await;
                if let Err(e) = self.orch.draft_minutes(&m.id, &*self.approver).await {
                    self.say("autopilot_step", format!("纪要没有起草：{e}"));
                }
            }
            let _ = self.db().close_meeting(&m.id);
            // 行动项：拦合并的交给任务负责人；给人的和其他的写明原因留到之后（不挡合并，也不进人的待办）
            let mut fixed: Vec<String> = vec![];
            for a in self.db().actions_for_meeting(&m.id)?.into_iter().filter(|a| a.blocking && a.status == "open") {
                if a.for_human {
                    self.db().set_action_status(&a.id, "dismissed", "负责人：需要人来决定的事，不影响这次合并的正确性，转入后续")?;
                    continue;
                }
                let mine = a.assignee_id.as_deref().map(|x| x == emp.id).unwrap_or(true);
                if !mine {
                    self.db().set_action_status(&a.id, "dismissed", "负责人：不是这个任务负责人的事项，转入后续跟进")?;
                    continue;
                }
                // 并行任务里相同的意见只处理一次：修复会随主分支合并过来，不要每个任务各改一遍
                if self.is_duplicate_action(&a.text) {
                    self.say("autopilot_step", format!("评审意见与已处理的重复，不再重复改：{}", clip(&a.text, 80)));
                    self.db().set_action_status(&a.id, "dismissed", "负责人：与已经处理（或正在处理）的另一条评审意见重复，只处理一次；修复会随主分支合并过来")?;
                    continue;
                }
                // 每个条目最多处理几条；软预算期间不再处理——都降级为“之后再说”，仍被跟踪
                if fixed.len() >= MAX_ACTIONS_PER_TASK || self.soft_budget() {
                    let why = if fixed.len() >= MAX_ACTIONS_PER_TASK { format!("超过每个条目最多处理 {MAX_ACTIONS_PER_TASK} 条的上限") } else { "预算接近上限".to_string() };
                    self.db().downgrade_action(&a.id, &format!("负责人：{why}，降级为之后再说（不拦合并）"))?;
                    continue;
                }
                self.handled_actions.lock().unwrap().push(a.text.clone());
                self.say("autopilot_step", format!("评审意见交给 {}：{}", emp.name, clip(&a.text, 100)));
                {
                    let _p = self.permit_for(Some(&emp)).await;
                    let (e2, p2, pr2) = self.orch.prepare_action_dispatch(&a.id, Some(&emp.id))?;
                    self.orch.assign_for_action(&e2, &p2, &pr2, &a.id, &*self.approver, &NoopObserver).await?;
                }
                fixed.push(a.id.clone());
            }
            if !fixed.is_empty() {
                let v = self.verify(&task).await;
                if !v.0 {
                    self.say("autopilot_step", format!("处理评审意见后验证未通过：{}", clip(&v.1, 160)));
                    self.work(&emp, &format!("处理评审意见之后验证没有通过，请修复：\n{}", v.1)).await?;
                    if !self.verify(&task).await.0 {
                        self.db().set_plan_status(&item_id, "blocked", "处理评审意见后验证仍未通过")?;
                        bail!("《{}》处理评审意见后验证仍未通过", item.title);
                    }
                }
                for id in &fixed {
                    self.db().set_action_status(id, "done", "")?;
                }
                self.say("autopilot_step", format!("{} 条评审意见已处理并通过验证", fixed.len()));
            }
        }

        // 合并（串行）；冲突交给任务负责人解决
        for attempt in 0..3 {
            if self.is_merged(&task.id) {
                return Ok(());
            }
            let outcome = {
                let _g = self.merge_lock.lock().await;
                self.orch.merge_task(&task.id).await?
            };
            match outcome {
                MergeOutcome::Merged { commit } => {
                    self.merged.fetch_add(1, Ordering::SeqCst);
                    self.say("autopilot_merged", format!("已合并《{}》{}", item.title, &commit[..commit.len().min(8)]));
                    return Ok(());
                }
                MergeOutcome::Empty => {
                    self.say("autopilot_merged", format!("《{}》没有改动", item.title));
                    return Ok(());
                }
                MergeOutcome::Conflict { files } if attempt < 2 => {
                    self.say("autopilot_step", format!("《{}》合并冲突 {files:?}，让 {} 解决", item.title, emp.name));
                    self.work(&emp, &format!("你的分支与主分支（{}）有冲突：{}。请在你的工作区运行 `git merge {}`，解决冲突并保证两边的功能都保留、测试通过，然后提交。", task.base_branch, files.join("、"), task.base_branch)).await?;
                    let v = self.verify(&task).await;
                    if !v.0 {
                        bail!("解决冲突后验证未通过：{}", clip(&v.1, 200));
                    }
                }
                MergeOutcome::OpenActionItems { items } => {
                    // 还有拦合并的意见没有处理：负责人不放行，转给任务负责人
                    bail!("还有 {} 条拦合并的评审意见没有处理", items.len());
                }
                other => bail!("合并失败：{other:?}"),
            }
        }
        bail!("多次尝试后仍无法合并")
    }

    /// 负责人裁决 AI 员工提议的一般技术议题（代码细节类和人提的议题不碰）。
    /// 裁决不了的升级给人（议题进入“待决策”，出现在人的待办里），同一个议题不会反复裁决。
    async fn rule_issues(&self, tried: &mut HashSet<String>) {
        let Some(lead) = self.members().into_iter().find(|(_, r)| r == "lead").map(|(e, _)| e) else { return };
        let issues = self.db().list_issues(&self.project_id).unwrap_or_default();
        let mut n = 0;
        for i in issues {
            if n >= 3 || self.over_budget().is_some() || self.ctl.pause.load(Ordering::SeqCst) || self.ctl.stop.load(Ordering::SeqCst) { break; }
            if i.raised_by.is_none() || !matches!(i.status.as_str(), "open" | "discussing" | "pending_decision") || Orchestrator::lead_can_rule(&i).is_err() || tried.contains(&i.id) { continue; }
            let escalated: i64 = self.db().conn.lock().unwrap().query_row(
                "SELECT COUNT(*) FROM events WHERE kind = 'autopilot_issue_escalated' AND json_extract(payload,'$.issue_id') = ?1", [&i.id], |r| r.get(0)).unwrap_or(0);
            if escalated > 0 { continue; }
            tried.insert(i.id.clone());
            n += 1;
            let _p = self.permit_for(Some(&lead)).await;
            let _busy = self.busy_scope(&lead.name);
            match self.orch.lead_rule_issue(&i.id, &lead, &*self.approver).await {
                Ok(RulingOutcome::Decided { decision, reason }) => self.say("autopilot_decided", format!("{} 裁决《{}》：{}（原因：{}）", lead.name, i.title, clip(&decision, 120), clip(&reason, 120))),
                Ok(RulingOutcome::Escalated { reason }) => {
                    let _ = self.db().append_event_for(Some(&self.project_id), "autopilot_issue_escalated", Some(&lead.id), None, &json!({"run_id": self.run_id, "issue_id": i.id, "title": i.title, "reason": reason, "text": format!("{} 没有裁决《{}》，交给你：{}", lead.name, i.title, clip(&reason, 160))}));
                }
                Ok(RulingOutcome::NotForLead { .. }) => {}
                Err(e) => self.say("autopilot_step", format!("裁决《{}》没有完成：{e}", i.title)),
            }
        }
    }

    /// 标给人的行动项：负责人先处理一遍（能定的定、不必做的驳回、必须是人的才留下并写明原因）。每轮最多 3 条。
    async fn handle_human_actions(&self, tried: &mut HashSet<String>) {
        let Some(lead) = self.lead() else { return };
        let ids = self.orch.human_actions_for_lead(&self.project_id).unwrap_or_default();
        let mut n = 0;
        for id in ids {
            if n >= 3 || self.soft_budget() || self.over_budget().is_some() || self.ctl.pause.load(Ordering::SeqCst) || self.ctl.stop.load(Ordering::SeqCst) { break; }
            if tried.contains(&id) { continue; }
            tried.insert(id.clone());
            n += 1;
            let _p = self.permit_for(Some(&lead)).await;
            let _busy = self.busy_scope(&lead.name);
            match self.orch.lead_handle_action(&id, &lead, &*self.approver).await {
                Ok(Some(ActionOutcome::Decided { decision, follow_ups })) => self.say("autopilot_decided", format!("{} 处理了一项标给人的事项：{}{}", lead.name, clip(&decision, 120), if follow_ups.is_empty() { String::new() } else { format!("（追加了计划条目：{}）", follow_ups.join("、")) })),
                Ok(Some(ActionOutcome::Dismissed { reason })) => self.say("autopilot_decided", format!("{} 判断一项标给人的事项不必做：{}", lead.name, clip(&reason, 120))),
                Ok(Some(ActionOutcome::Escalated { reason })) => self.say("autopilot_step", format!("{} 把一项事项留给你，原因：{}", lead.name, clip(&reason, 160))),
                Ok(None) => {}
                Err(e) => self.say("autopilot_step", format!("处理标给人的事项没有完成：{e}")),
            }
        }
    }

    /// 收尾：整理“之后再说”的行动项（积累到一定数量才调用）。
    /// 持续整理“之后再说”的行动项：批次开始时一次，之后每合并 2 个条目、积压够多时一次；收尾时再整理一次。
    async fn maybe_triage(&self, force: bool) {
        let merged = self.merged.load(Ordering::SeqCst);
        if !force && (self.soft_budget() || merged < self.last_triaged_at_merged.load(Ordering::SeqCst) + 2) { return; }
        self.last_triaged_at_merged.store(merged, Ordering::SeqCst);
        self.triage_later().await;
    }

    async fn triage_later(&self) {
        let Some(lead) = self.lead() else { return };
        if self.over_budget().is_some() || self.ctl.stop.load(Ordering::SeqCst) { return; }
        if self.orch.later_actions(&self.project_id).map(|v| v.len()).unwrap_or(0) < crate::lead_triage::MIN_ITEMS_TO_TRIAGE { return; }
        let _p = self.permit_for(Some(&lead)).await;
        let _busy = self.busy_scope(&lead.name);
        match self.orch.lead_triage_later(&self.project_id, &lead, &*self.approver).await {
            Ok(Some(t)) => {
                self.say("autopilot_step", format!("{} 整理了“之后再说”的行动项：驳回 {} 条，转为计划条目 {} 个{}，保留 {} 条的编号未提及或被保留", lead.name, t.dismissed, t.planned.len(), if t.planned.is_empty() { String::new() } else { format!("（{}）", t.planned.join("、")) }, t.kept));
                for r in &t.rejected { self.say("autopilot_step", format!("整理行动项被拒绝：{r}")); }
            }
            Ok(None) => {}
            Err(e) => self.say("autopilot_step", format!("整理行动项没有完成：{e}")),
        }
    }

    fn lead(&self) -> Option<Employee> {
        self.members().into_iter().find(|(_, r)| r == "lead").map(|(e, _)| e)
    }

    /// 负责人调整计划（失败后或完成度复盘）。返回应用的结果；把改动、被拒绝的改动和升级都写进动态。
    async fn adjust_plan(&self, trigger: AdjustTrigger, failure: &str) -> Option<Applied> {
        let lead = self.lead()?;
        let _p = self.permit_for(Some(&lead)).await;
        let _busy = self.busy_scope(&lead.name);
        match self.orch.lead_adjust_plan(&self.project_id, &lead, trigger, failure, &*self.approver).await {
            Ok(a) => {
                let mut parts = Vec::new();
                if !a.added.is_empty() { parts.push(format!("追加 {}", a.added.iter().map(|t| format!("《{t}》")).collect::<Vec<_>>().join("、"))); }
                if !a.cancelled.is_empty() { parts.push(format!("取消 {}", a.cancelled.iter().map(|t| format!("《{t}》")).collect::<Vec<_>>().join("、"))); }
                if !a.reassigned.is_empty() { parts.push(format!("改派 {}", a.reassigned.join("、"))); }
                if !a.retried.is_empty() { parts.push(format!("重试 {}", a.retried.iter().map(|t| format!("《{t}》")).collect::<Vec<_>>().join("、"))); }
                if !parts.is_empty() {
                    self.say("autopilot_plan_changed", format!("{} 调整了计划：{}{}", lead.name, parts.join("；"), if a.summary.is_empty() { String::new() } else { format!("（{}）", clip(&a.summary, 100)) }));
                } else if trigger == AdjustTrigger::FinalReview && a.escalate.is_none() {
                    self.say("autopilot_step", format!("{} 做了完成度复盘：不需要追加条目", lead.name));
                }
                for r in &a.rejected { self.say("autopilot_step", format!("计划调整被拒绝：{r}")); }
                if let Some(e) = &a.escalate { self.say("autopilot_step", format!("{} 认为这需要你来定：{}", lead.name, clip(e, 200))); }
                Some(a)
            }
            Err(e) => { self.say("autopilot_step", format!("计划调整没有完成：{e}")); None }
        }
    }

    /// 负责人整理项目记忆：待验证的经验攒到 3 条以上、且自上次整理后至少合并了 2 个任务才整理（`force` 时只要有待验证的就整理）。
    async fn maybe_curate(&self, force: bool) {
        let Some(lead) = self.lead() else { return };
        if self.over_budget().is_some() || self.ctl.stop.load(Ordering::SeqCst) || (!force && self.soft_budget()) { return; }
        let unverified = self.orch.curation_candidates(&self.project_id).map(|c| c.iter().filter(|m| m.status == "unverified").count()).unwrap_or(0);
        let merged = self.merged.load(Ordering::SeqCst);
        if unverified == 0 || (!force && (unverified < 3 || merged < self.last_curated_at_merged.load(Ordering::SeqCst) + 2)) { return; }
        self.last_curated_at_merged.store(merged, Ordering::SeqCst);
        let _p = self.permit_for(Some(&lead)).await;
        let _busy = self.busy_scope(&lead.name);
        match self.orch.lead_curate_memory(&self.project_id, &lead, &*self.approver).await {
            Ok(c) => {
                let mut parts = Vec::new();
                if !c.verified.is_empty() { parts.push(format!("验证 {} 条", c.verified.len())); }
                if !c.expired.is_empty() { parts.push(format!("让 {} 条过期", c.expired.len())); }
                if !c.merged.is_empty() { parts.push(format!("合并 {}", c.merged.join("、"))); }
                self.say("autopilot_memory", format!("{} 整理了项目记忆：{}{}", lead.name, if parts.is_empty() { "没有改动（拿不准的留给你）".to_string() } else { parts.join("；") }, if c.summary.is_empty() { String::new() } else { format!("（{}）", clip(&c.summary, 100)) }));
                for r in &c.rejected { self.say("autopilot_step", format!("记忆整理被拒绝：{r}")); }
                if let Some(e) = &c.escalate { self.say("autopilot_step", format!("{} 认为记忆里有需要你判断的矛盾：{}", lead.name, clip(e, 200))); }
            }
            Err(e) => self.say("autopilot_step", format!("整理记忆没有完成：{e}")),
        }
    }

    /// 收尾：更新负责人维护的项目文档（决策记录、已知问题、计划进度由数据生成；概览由负责人读代码撰写）。
    async fn update_docs(&self) {
        let Some(lead) = self.lead() else { return };
        if self.over_budget().is_some() || self.ctl.stop.load(Ordering::SeqCst) { return; }
        let _p = self.permit_for(Some(&lead)).await;
        let _busy = self.busy_scope(&lead.name);
        match self.orch.update_project_docs(&self.project_id, &lead, true, &*self.approver).await {
            Ok(r) => self.say("autopilot_docs", format!("{} 更新了项目文档（docs/crewforge/）：{}", lead.name, r.note)),
            Err(e) => self.say("autopilot_step", format!("文档没有更新：{e}")),
        }
    }

    /// 有两个以上的任务同时在做时开一次站会，先对齐接口和进度。限流：第一次直接开，之后至少合并过一个任务才再开。
    async fn maybe_standup(&self) {
        let Ok(_g) = self.standup_lock.try_lock() else { return };
        let active = self.db().list_tasks(&self.repo.to_string_lossy()).map(|t| t.into_iter().filter(|t| t.status == "active").count()).unwrap_or(0);
        let merged = self.merged.load(Ordering::SeqCst);
        if active < 2 || (self.standups.load(Ordering::SeqCst) > 0 && merged <= self.last_standup_at_merged.load(Ordering::SeqCst)) || self.over_budget().is_some() || self.soft_budget() {
            return;
        }
        let Ok((m, who)) = self.orch.start_standup(&self.project_id).await else { return };
        self.standups.fetch_add(1, Ordering::SeqCst);
        self.last_standup_at_merged.store(merged, Ordering::SeqCst);
        self.say("autopilot_step", format!("{active} 个任务并行，开站会对齐：{}", who.iter().map(|e| e.name.clone()).collect::<Vec<_>>().join("、")));
        for w in &who {
            let _p = self.permit_for(Some(w)).await;
            let _busy = self.busy_scope(&w.name);
            if let Err(e) = self.orch.meeting_reply(&m.id, w, &*self.approver).await {
                self.say("autopilot_step", format!("站会：{}：{e}", w.name));
            }
        }
        if let Ok(text) = self.orch.standup_alignment_check(&m.id).await {
            self.say("autopilot_step", clip(&text, 240));
        }
        let _p = self.permit_for(self.lead().as_ref()).await;
        let _ = self.orch.draft_minutes(&m.id, &*self.approver).await;
        let _ = self.db().close_meeting(&m.id);
    }

    /// 主循环：按依赖并行推进计划条目，直到做完、预算用尽、需要人，或被暂停/停止。
    /// 批次开始时，“进行中”但没有人在做的条目（应用重启或崩溃时干活的进程没了）放回“待开始”，
    /// 员工会接着自己那个任务继续做；不然这些条目永远卡在“进行中”，后面等它的条目也开不了工。
    /// 员工此刻正被别处占着（比如你手动派的活）的不动。返回放回了几条。
    fn requeue_orphaned_items(&self) -> usize {
        let Ok(view) = self.db().plan_view(&self.project_id) else { return 0 };
        let busy_names = self.busy.lock().unwrap().clone();
        let mut n = 0;
        for it in view.items.iter().filter(|i| i.status == "in_progress") {
            let owner = it.task_id.as_deref().and_then(|t| self.db().get_task(t).ok().flatten()).and_then(|t| self.db().employee_by_id(&t.employee_id).ok().flatten());
            if owner.as_ref().is_some_and(|e| busy_names.contains(&e.name)) { continue; }
            if self.db().requeue_plan_item(&it.id).unwrap_or(false) {
                self.say("autopilot_step", format!("《{}》上次做到一半被中断（没有人在做），放回待开始，由 {} 接着做", it.title, owner.map(|e| e.name).or(it.assignee_name.clone()).unwrap_or_else(|| "员工".into())));
                n += 1;
            }
        }
        n
    }

    pub async fn run(self: Arc<Self>) -> AutopilotRun {
        self.requeue_orphaned_items();
        // 已经积压的行动项，批次一开始先清一遍
        self.maybe_triage(true).await;
        let mut set: tokio::task::JoinSet<(String, Result<()>)> = tokio::task::JoinSet::new();
        let (mut running, mut failed): (HashSet<String>, HashSet<String>) = Default::default();
        let mut ruled: HashSet<String> = HashSet::new();
        let mut handled_human: HashSet<String> = HashSet::new();
        let mut final_review_done = false;
        let mut replans = 0usize;
        let mut escalation: Option<String> = None;
        let (status, reason) = loop {
            if self.ctl.stop.load(Ordering::SeqCst) { break ("stopped", "你停止了推进".to_string()); }
            let paused = self.ctl.pause.load(Ordering::SeqCst);
            let budget = self.over_budget().or_else(|| if running.is_empty() { self.no_room_for_another_item() } else { None });
            let human: Option<String> = None; // 议题都由负责人裁决，不再因需求类/高风险议题停下等人
            if (paused || budget.is_some() || human.is_some()) && set.is_empty() {
                let end = if paused { ("paused", "你暂停了推进".to_string()) } else if let Some(b) = budget { ("budget", b) } else { ("escalated", human.unwrap_or_default()) };
                // 预算是因为“留给收尾的调用数”而停的（还没真正用光）：收尾照做，不让文档和记忆总被挤掉
                if end.0 == "budget" && !paused && self.over_budget().is_none() && !self.ctl.stop.load(Ordering::SeqCst) {
                    self.maybe_curate(true).await;
                    self.triage_later().await;
                    self.update_docs().await;
                }
                break end;
            }
            let view = match self.db().plan_view(&self.project_id) { Ok(v) => v, Err(e) => break ("failed", e.to_string()) };
            if view.items.iter().filter(|i| i.status != "cancelled").all(|i| i.status == "done") {
                // 做完后让负责人对照目标复盘一次：还有必要的工作就追加，继续推进
                if !final_review_done && !paused && budget.is_none() && human.is_none() && set.is_empty() {
                    final_review_done = true;
                    if let Some(a) = self.adjust_plan(AdjustTrigger::FinalReview, "").await {
                        if !a.added.is_empty() { continue; }
                        if let Some(e) = a.escalate { escalation = Some(e); }
                    }
                }
                if let Some(e) = escalation.take() { break ("escalated", format!("负责人复盘后认为需要你来定：{e}")); }
                // 收尾：整理记忆、更新文档（各一次）
                if !paused && budget.is_none() && human.is_none() && set.is_empty() {
                    self.maybe_curate(true).await;
                    self.triage_later().await;
                    self.update_docs().await;
                }
                break ("finished", "计划全部完成".to_string());
            }
            // 暂停、预算用尽或有事等人时，不再派新活，只等手上的做完
            if !(paused || budget.is_some() || human.is_some()) && !(self.no_room_for_another_item().is_some()) {
                self.rule_issues(&mut ruled).await;
                self.handle_human_actions(&mut handled_human).await;
                let busy: HashSet<String> = running.iter().filter_map(|rid| self.db().get_plan_item(rid).ok().flatten().and_then(|i| i.task_id).and_then(|t| self.db().get_task(&t).ok().flatten()).map(|t| t.employee_id)).collect();
                let mut busy = busy;
                let ready: Vec<PlanItem> = view.items.iter().filter(|i| i.status == "planned" && i.ready && !running.contains(&i.id) && !failed.contains(&i.id)).cloned().collect();
                let mut starts: Vec<(String, Employee)> = vec![];
                // 预计会改同一个文件的条目不并行（并行必然合并冲突，每次解决要多花调用）
                let files = self.db().plan_files(&self.project_id).unwrap_or_default();
                let mut claimed: HashSet<String> = running.iter().filter_map(|id| files.get(id)).flatten().cloned().collect();
                for it in &ready {
                    let mine = files.get(&it.id).cloned().unwrap_or_default();
                    if mine.iter().any(|f| claimed.contains(f)) { continue; }
                    if let Some(emp) = self.pick(it, &busy) {
                        busy.insert(emp.id.clone());
                        claimed.extend(mine);
                        starts.push((it.id.clone(), emp));
                    }
                }
                for (id, emp) in starts {
                    running.insert(id.clone());
                    let (me, i2) = (self.clone(), id.clone());
                    set.spawn(async move { let r = me.process_item(i2.clone(), emp).await; (i2, r) });
                }
            }
            if set.is_empty() {
                if let Some(e) = escalation.take() { break ("escalated", format!("负责人认为需要你来定：{e}")); }
                break ("escalated", "没有可以开始的条目了（前置条目失败或被阻塞），需要你看一下".to_string());
            }
            if let Some(Ok((id, res))) = set.join_next().await {
                running.remove(&id);
                match res {
                    Ok(()) => { self.ensure_baseline().await; self.maybe_curate(false).await; self.maybe_triage(false).await; }
                    Err(e) => {
                        let title = self.db().get_plan_item(&id).ok().flatten().map(|i| i.title).unwrap_or_default();
                        self.say("autopilot_step", format!("《{title}》没做成：{e}"));
                        failed.insert(id);
                        // 负责人看一眼：拆小、补前置、取消、改派或换个说明重试（每批次最多 2 次）
                        if replans < 2 && !self.ctl.stop.load(Ordering::SeqCst) && self.over_budget().is_none() {
                            replans += 1;
                            if let Some(a) = self.adjust_plan(AdjustTrigger::ItemFailed, &format!("《{title}》：{e}")).await {
                                for rid in &a.retried_ids { failed.remove(rid); }
                                if a.escalate.is_some() { escalation = a.escalate; }
                            }
                        }
                    }
                }
            }
        };
        while set.join_next().await.is_some() {}
        let calls = self.calls.load(Ordering::SeqCst) as i64;
        let _ = self.db().set_autopilot_run(&self.run_id, status, calls, &reason);
        let done = self.db().plan_view(&self.project_id).map(|v| format!("计划完成 {}/{}", v.counts.done, v.items.iter().filter(|i| i.status != "cancelled").count())).unwrap_or_default();
        let _ = self.db().append_event_for(Some(&self.project_id), "autopilot_finished", None, None, &json!({"run_id": self.run_id, "status": status, "text": format!("{}：{reason}（{done}，本批次 {calls} 次调用）", status_label(status))}));
        self.db().get_autopilot_run(&self.run_id).ok().flatten().expect("批次存在")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similar_review_comments_are_recognised_but_different_ones_are_not() {
        let a = "让 load 处理空文件或损坏 JSON（todo.py load，捕获 JSONDecodeError，打印清楚的错误并以退出码 1 结束）";
        let b = "让 `load` 处理空文件或损坏 JSON（todo.py `load`，捕获 JSONDecodeError，打印清楚的错误并以退出码 1 结束，add、list 不再出现堆栈）";
        assert!(similar_actions(a, b), "同一件事的不同写法");
        assert!(!similar_actions(a, "补编号 0 和 -1 的用例（tests/test_done.py，各 2 个用例，均断言退出码 1）"));
        assert!(!similar_actions(a, "让 parse_due 拒绝非 ASCII 日期（加 isascii 判断）"));
        assert!(!similar_actions("", "x") && !similar_actions("a", "b"));
    }
}
