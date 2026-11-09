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
use crate::onboarding::baseline::{CommandResult, Origin, run_commands};
use crate::onboarding::scan::scan_dir;
use crate::orchestrator::Orchestrator;
use crate::plan::PlanItem;
use crate::runner::{Approver, NoopObserver};
use crate::scheduler::{Permit, Scheduler};
use crate::workspace::MergeOutcome;
use anyhow::{Context, Result, bail};
use rusqlite::params;
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
        Ok(AutopilotRun { id: r.get(0)?, project_id: r.get(1)?, status: r.get(2)?, max_calls: r.get(3)?, max_minutes: r.get(4)?, calls: r.get(5)?, started_at: r.get(6)?, ended_at: r.get(7)?, reason: r.get(8)? })
    }

    pub fn create_autopilot_run(&self, project_id: &str, max_calls: i64, max_minutes: i64) -> Result<AutopilotRun> {
        let id = uuid::Uuid::new_v4().to_string();
        self.conn.lock().unwrap().execute(
            "INSERT INTO autopilot_runs (id, project_id, status, max_calls, max_minutes, started_at) VALUES (?1,?2,'running',?3,?4,?5)",
            params![id, project_id, max_calls, max_minutes, now_ms()])?;
        Ok(self.get_autopilot_run(&id)?.context("批次创建失败")?)
    }

    pub fn get_autopilot_run(&self, id: &str) -> Result<Option<AutopilotRun>> {
        Ok(self.conn.lock().unwrap().query_row("SELECT id, project_id, status, max_calls, max_minutes, calls, started_at, ended_at, reason FROM autopilot_runs WHERE id = ?1", [id], Self::run_row).ok())
    }

    /// 项目最近一个批次。
    pub fn latest_autopilot_run(&self, project_id: &str) -> Result<Option<AutopilotRun>> {
        Ok(self.conn.lock().unwrap().query_row(
            "SELECT id, project_id, status, max_calls, max_minutes, calls, started_at, ended_at, reason FROM autopilot_runs WHERE project_id = ?1 ORDER BY started_at DESC, rowid DESC LIMIT 1",
            [project_id], Self::run_row).ok())
    }

    pub fn set_autopilot_run(&self, id: &str, status: &str, calls: i64, reason: &str) -> Result<()> {
        let end = !matches!(status, "running");
        self.conn.lock().unwrap().execute(
            "UPDATE autopilot_runs SET status = ?2, calls = ?3, reason = ?4, ended_at = CASE WHEN ?5 = 1 THEN ?6 ELSE NULL END WHERE id = ?1",
            params![id, status, calls, reason, end as i64, now_ms()])?;
        Ok(())
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
    standups: AtomicUsize,
    /// 正在被占用的员工名（与应用里“工作中”的集合共用，避免人和负责人同时驱动同一员工的会话）
    busy: Arc<std::sync::Mutex<HashSet<String>>>,
    on_busy_change: Arc<dyn Fn(Vec<String>) + Send + Sync>,
}

fn clip(s: &str, n: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= n { one } else { format!("{}…", one.chars().take(n).collect::<String>()) }
}

impl<A: Approver + Send + Sync + 'static> Autopilot<A> {
    /// 开始前的检查：项目要有 AI 负责人；此时点“开始”就是人对计划的最后确认——提议的条目一并批准。
    pub fn prepare(orch: &Arc<Orchestrator>, project_id: &str, max_calls: i64, max_minutes: i64) -> Result<AutopilotRun> {
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
        if max_calls < 1 || max_minutes < 1 {
            bail!("预算必须大于 0");
        }
        let view = db.plan_view(project_id)?;
        if view.items.iter().filter(|i| i.status != "cancelled").count() == 0 {
            bail!("项目还没有计划。请先在“计划”页让负责人起草计划，或手动添加");
        }
        let approved = db.approve_proposed_plan(project_id)?;
        let run = db.create_autopilot_run(project_id, max_calls, max_minutes)?;
        db.append_event_for(Some(project_id), "autopilot_started", None, None, &json!({"run_id": run.id, "text": format!("负责人开始自主推进（预算 {max_calls} 次调用 / {max_minutes} 分钟）{}", if approved > 0 { format!("，并批准了 {approved} 个提议的计划条目") } else { String::new() }), "max_calls": max_calls, "max_minutes": max_minutes}))?;
        Ok(run)
    }

    pub fn new(orch: Arc<Orchestrator>, sched: Arc<Scheduler>, approver: Arc<A>, ctl: Arc<AutopilotCtl>, run: &AutopilotRun) -> Result<Arc<Self>> {
        Self::new_with_busy(orch, sched, approver, ctl, run, Arc::new(std::sync::Mutex::new(HashSet::new())), Arc::new(|_| {}))
    }

    /// 与应用共用“员工是否忙碌”的集合：忙碌的员工不派活，派出去的会登记为忙碌并通知界面。
    pub fn new_with_busy(orch: Arc<Orchestrator>, sched: Arc<Scheduler>, approver: Arc<A>, ctl: Arc<AutopilotCtl>, run: &AutopilotRun,
        busy: Arc<std::sync::Mutex<HashSet<String>>>, on_busy_change: Arc<dyn Fn(Vec<String>) + Send + Sync>) -> Result<Arc<Self>> {
        let project = orch.db.get_project(&run.project_id)?.context("项目不存在")?;
        Ok(Arc::new(Self {
            orch, sched, approver, ctl, project_id: run.project_id.clone(), run_id: run.id.clone(), repo: std::path::PathBuf::from(project.path),
            calls: AtomicUsize::new(0), started: Instant::now(), max_calls: run.max_calls as usize, max_minutes: run.max_minutes as u64, merge_lock: tokio::sync::Mutex::new(()), standup_lock: tokio::sync::Mutex::new(()),
            merged: AtomicUsize::new(0), last_standup_at_merged: AtomicUsize::new(0), standups: AtomicUsize::new(0),
            busy, on_busy_change,
        }))
    }

    fn db(&self) -> &Db {
        &self.orch.db
    }

    fn say(&self, kind: &str, text: impl std::fmt::Display) {
        let _ = self.db().append_event_for(Some(&self.project_id), kind, None, None, &json!({"run_id": self.run_id, "text": text.to_string()}));
    }

    async fn permit(&self) -> Permit {
        let p = self.sched.acquire().await;
        self.calls.fetch_add(1, Ordering::SeqCst);
        let _ = self.db().set_autopilot_run(&self.run_id, "running", self.calls.load(Ordering::SeqCst) as i64, "");
        p
    }

    fn over_budget(&self) -> Option<String> {
        if self.calls.load(Ordering::SeqCst) >= self.max_calls {
            return Some(format!("调用次数达到预算 {}", self.max_calls));
        }
        if self.started.elapsed() > Duration::from_secs(self.max_minutes * 60) {
            return Some(format!("时间达到预算 {} 分钟", self.max_minutes));
        }
        None
    }

    /// 需求类或高风险议题等人拍板：这类事负责人不替人决定。
    fn needs_human(&self) -> Option<String> {
        let d = self.db().pending_decisions().ok()?;
        d.into_iter().find(|d| d.issue.project_id == self.project_id && (d.issue.high_risk || d.issue.kind == "requirement")).map(|d| format!("议题《{}》是需求类或高风险事项，只有你能拍板", d.issue.title))
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
        let _p = self.permit().await;
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
            let _p = self.permit().await;
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
                let _p = self.permit().await;
                match self.orch.meeting_reply(&m.id, &rv, &*self.approver).await {
                    Ok(t) => self.say("autopilot_step", format!("{} 评审《{}》：{}", rv.name, item.title, clip(&t, 200))),
                    Err(e) => self.say("autopilot_step", format!("评审没有完成：{e}")),
                }
            }
            {
                let _p = self.permit().await;
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
                if !mine || fixes >= 5 {
                    self.db().set_action_status(&a.id, "dismissed", "负责人：不是这个任务负责人的事项，或本任务的修复预算已用完，转入后续跟进")?;
                    continue;
                }
                self.say("autopilot_step", format!("评审意见交给 {}：{}", emp.name, clip(&a.text, 100)));
                {
                    let _p = self.permit().await;
                    let (e2, p2, pr2) = self.orch.prepare_action_dispatch(&a.id, Some(&emp.id))?;
                    self.orch.assign_for_action(&e2, &p2, &pr2, &a.id, &*self.approver, &NoopObserver).await?;
                }
                fixes += 1;
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

    /// 有两个以上的任务同时在做时开一次站会，先对齐接口和进度。限流：第一次直接开，之后至少合并过一个任务才再开。
    async fn maybe_standup(&self) {
        let Ok(_g) = self.standup_lock.try_lock() else { return };
        let active = self.db().list_tasks(&self.repo.to_string_lossy()).map(|t| t.into_iter().filter(|t| t.status == "active").count()).unwrap_or(0);
        let merged = self.merged.load(Ordering::SeqCst);
        if active < 2 || (self.standups.load(Ordering::SeqCst) > 0 && merged <= self.last_standup_at_merged.load(Ordering::SeqCst)) || self.over_budget().is_some() {
            return;
        }
        let Ok((m, who)) = self.orch.start_standup(&self.project_id).await else { return };
        self.standups.fetch_add(1, Ordering::SeqCst);
        self.last_standup_at_merged.store(merged, Ordering::SeqCst);
        self.say("autopilot_step", format!("{active} 个任务并行，开站会对齐：{}", who.iter().map(|e| e.name.clone()).collect::<Vec<_>>().join("、")));
        for w in &who {
            let _p = self.permit().await;
            if let Err(e) = self.orch.meeting_reply(&m.id, w, &*self.approver).await {
                self.say("autopilot_step", format!("站会：{}：{e}", w.name));
            }
        }
        if let Ok(text) = self.orch.standup_alignment_check(&m.id).await {
            self.say("autopilot_step", clip(&text, 240));
        }
        let _p = self.permit().await;
        let _ = self.orch.draft_minutes(&m.id, &*self.approver).await;
        let _ = self.db().close_meeting(&m.id);
    }

    /// 主循环：按依赖并行推进计划条目，直到做完、预算用尽、需要人，或被暂停/停止。
    pub async fn run(self: Arc<Self>) -> AutopilotRun {
        let mut set: tokio::task::JoinSet<(String, Result<()>)> = tokio::task::JoinSet::new();
        let (mut running, mut failed): (HashSet<String>, HashSet<String>) = Default::default();
        let (status, reason) = loop {
            if self.ctl.stop.load(Ordering::SeqCst) { break ("stopped", "你停止了推进".to_string()); }
            let paused = self.ctl.pause.load(Ordering::SeqCst);
            let budget = self.over_budget();
            let human = self.needs_human();
            if (paused || budget.is_some() || human.is_some()) && set.is_empty() {
                break if paused { ("paused", "你暂停了推进".to_string()) } else if let Some(b) = budget { ("budget", b) } else { ("escalated", human.unwrap_or_default()) };
            }
            let view = match self.db().plan_view(&self.project_id) { Ok(v) => v, Err(e) => break ("failed", e.to_string()) };
            if view.items.iter().filter(|i| i.status != "cancelled").all(|i| i.status == "done") { break ("finished", "计划全部完成".to_string()); }
            // 暂停、预算用尽或有事等人时，不再派新活，只等手上的做完
            if !(paused || budget.is_some() || human.is_some()) {
                let busy: HashSet<String> = running.iter().filter_map(|rid| self.db().get_plan_item(rid).ok().flatten().and_then(|i| i.task_id).and_then(|t| self.db().get_task(&t).ok().flatten()).map(|t| t.employee_id)).collect();
                let mut busy = busy;
                let ready: Vec<PlanItem> = view.items.iter().filter(|i| i.status == "planned" && i.ready && !running.contains(&i.id) && !failed.contains(&i.id)).cloned().collect();
                let mut starts: Vec<(String, Employee)> = vec![];
                for it in &ready {
                    if let Some(emp) = self.pick(it, &busy) {
                        busy.insert(emp.id.clone());
                        starts.push((it.id.clone(), emp));
                    }
                }
                for (id, emp) in starts {
                    running.insert(id.clone());
                    let (me, i2) = (self.clone(), id.clone());
                    set.spawn(async move { let r = me.process_item(i2.clone(), emp).await; (i2, r) });
                }
            }
            if set.is_empty() { break ("escalated", "没有可以开始的条目了（前置条目失败或被阻塞），需要你看一下".to_string()); }
            if let Some(Ok((id, res))) = set.join_next().await {
                running.remove(&id);
                match res {
                    Ok(()) => self.ensure_baseline().await,
                    Err(e) => {
                        let title = self.db().get_plan_item(&id).ok().flatten().map(|i| i.title).unwrap_or_default();
                        self.say("autopilot_step", format!("《{title}》没做成：{e}"));
                        failed.insert(id);
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
