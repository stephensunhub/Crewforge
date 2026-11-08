//! 旧项目导入（Project Onboarding）：只读扫描 → 测试基线 → AI 联合评估 → 人类确认。
//! 确认之前项目处于"导入中"，不能给员工派开发任务。

pub mod baseline;
pub mod scan;

use crate::db::{Db, Employee, now_ms};
use crate::memory::NewMemory;
use crate::orchestrator::Orchestrator;
use crate::runner::{Approver, NoopObserver, TurnConfig, run_turn};
use crate::workspace::git_output;
use anyhow::{Context, Result, bail};
use baseline::{CheckReport, CommandResult, Progress, classify, run_commands};
use rusqlite::{OptionalExtension, params};
use scan::{BaselineCommand, ScanReport};
use serde::Serialize;
use serde_json::json;
use std::path::Path;
use std::time::Duration;

pub const MIGRATION_6: &str = r#"
ALTER TABLE projects ADD COLUMN onboarding TEXT NOT NULL DEFAULT 'none';
CREATE TABLE project_scans (
    id          TEXT PRIMARY KEY,
    project_id  TEXT NOT NULL REFERENCES projects(id),
    created_at  INTEGER NOT NULL,
    commit_sha  TEXT,
    report      TEXT NOT NULL
);
CREATE INDEX idx_scans_project ON project_scans(project_id, created_at);
CREATE TABLE baselines (
    id          TEXT PRIMARY KEY,
    project_id  TEXT NOT NULL REFERENCES projects(id),
    commit_sha  TEXT NOT NULL,
    status      TEXT NOT NULL,                 -- running | done | failed
    started_at  INTEGER NOT NULL,
    finished_at INTEGER,
    commands    TEXT NOT NULL,                 -- 本次运行的命令（JSON）
    results     TEXT NOT NULL DEFAULT '[]'     -- 每条命令的原始结果（JSON）
);
CREATE INDEX idx_baselines_project ON baselines(project_id, started_at);
CREATE TABLE onboarding_reports (
    id          TEXT PRIMARY KEY,
    project_id  TEXT NOT NULL REFERENCES projects(id),
    kind        TEXT NOT NULL,                 -- architect | qa
    employee_id TEXT REFERENCES employees(id),
    content     TEXT NOT NULL,
    commit_sha  TEXT,
    created_at  INTEGER NOT NULL
);
CREATE TABLE task_checks (
    id          TEXT PRIMARY KEY,
    task_id     TEXT NOT NULL REFERENCES tasks(id),
    baseline_id TEXT NOT NULL REFERENCES baselines(id),
    created_at  INTEGER NOT NULL,
    report      TEXT NOT NULL
);
CREATE INDEX idx_checks_task ON task_checks(task_id, created_at);
"#;

pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(900);

#[derive(Debug, Clone, Serialize)]
pub struct BaselineRun {
    pub id: String,
    pub project_id: String,
    pub commit_sha: String,
    pub status: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub commands: Vec<BaselineCommand>,
    pub results: Vec<CommandResult>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OnboardingReport {
    pub id: String,
    pub kind: String,
    pub employee_id: Option<String>,
    pub content: String,
    pub commit_sha: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct OnboardingState {
    pub status: String,
    pub scan: Option<ScanReport>,
    pub baseline: Option<BaselineRun>,
    pub reports: Vec<OnboardingReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoredCheck {
    pub created_at: i64,
    pub report: CheckReport,
}

impl Db {
    pub fn save_scan(&self, project_id: &str, commit: Option<&str>, report: &ScanReport) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO project_scans (id, project_id, created_at, commit_sha, report) VALUES (?1,?2,?3,?4,?5)",
            params![uuid::Uuid::new_v4().to_string(), project_id, now_ms(), commit, serde_json::to_string(report)?],
        )?;
        Ok(())
    }

    pub fn latest_scan(&self, project_id: &str) -> Result<Option<ScanReport>> {
        let text: Option<String> = self.conn.lock().unwrap().query_row(
            "SELECT report FROM project_scans WHERE project_id = ?1 ORDER BY created_at DESC, rowid DESC LIMIT 1", [project_id], |r| r.get(0)).optional()?;
        Ok(match text { Some(t) => Some(serde_json::from_str(&t)?), None => None })
    }

    pub fn create_baseline(&self, project_id: &str, commit: &str, commands: &[BaselineCommand]) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let conn = self.conn.lock().unwrap();
        // 上次异常退出遗留的"运行中"记录不可能再完成，标记为失败
        conn.execute("UPDATE baselines SET status = 'failed', finished_at = ?2 WHERE project_id = ?1 AND status = 'running'", params![project_id, now_ms()])?;
        conn.execute(
            "INSERT INTO baselines (id, project_id, commit_sha, status, started_at, commands) VALUES (?1,?2,?3,'running',?4,?5)",
            params![id, project_id, commit, now_ms(), serde_json::to_string(commands)?],
        )?;
        Ok(id)
    }

    pub fn finish_baseline(&self, id: &str, status: &str, results: &[CommandResult]) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE baselines SET status = ?2, finished_at = ?3, results = ?4 WHERE id = ?1",
            params![id, status, now_ms(), serde_json::to_string(results)?],
        )?;
        Ok(())
    }

    fn baseline_where(&self, project_id: &str, only_done: bool) -> Result<Option<BaselineRun>> {
        let sql = format!(
            "SELECT id, project_id, commit_sha, status, started_at, finished_at, commands, results FROM baselines
             WHERE project_id = ?1 {} ORDER BY started_at DESC, rowid DESC LIMIT 1",
            if only_done { "AND status = 'done'" } else { "" }
        );
        let row = self.conn.lock().unwrap().query_row(&sql, [project_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, i64>(4)?, r.get::<_, Option<i64>>(5)?, r.get::<_, String>(6)?, r.get::<_, String>(7)?))
        }).optional()?;
        Ok(match row {
            Some((id, project_id, commit_sha, status, started_at, finished_at, commands, results)) => Some(BaselineRun {
                id, project_id, commit_sha, status, started_at, finished_at,
                commands: serde_json::from_str(&commands)?, results: serde_json::from_str(&results)?,
            }),
            None => None,
        })
    }
    /// 最近一次基线（含进行中/失败的），用于界面展示。
    pub fn latest_baseline(&self, project_id: &str) -> Result<Option<BaselineRun>> { self.baseline_where(project_id, false) }
    /// 最近一次完成的基线，用于对比。
    pub fn current_baseline(&self, project_id: &str) -> Result<Option<BaselineRun>> { self.baseline_where(project_id, true) }

    pub fn save_report(&self, project_id: &str, kind: &str, employee_id: Option<&str>, content: &str, commit: Option<&str>) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO onboarding_reports (id, project_id, kind, employee_id, content, commit_sha, created_at) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![uuid::Uuid::new_v4().to_string(), project_id, kind, employee_id, content, commit, now_ms()],
        )?;
        Ok(())
    }

    /// 每种报告的最新一份。
    pub fn latest_reports(&self, project_id: &str) -> Result<Vec<OnboardingReport>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, kind, employee_id, content, commit_sha, created_at FROM onboarding_reports r
             WHERE project_id = ?1 AND created_at = (SELECT MAX(created_at) FROM onboarding_reports WHERE project_id = r.project_id AND kind = r.kind)
             ORDER BY kind",
        )?;
        let rows = stmt.query_map([project_id], |r| Ok(OnboardingReport { id: r.get(0)?, kind: r.get(1)?, employee_id: r.get(2)?, content: r.get(3)?, commit_sha: r.get(4)?, created_at: r.get(5)? }))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn save_task_check(&self, task_id: &str, baseline_id: &str, report: &CheckReport) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO task_checks (id, task_id, baseline_id, created_at, report) VALUES (?1,?2,?3,?4,?5)",
            params![uuid::Uuid::new_v4().to_string(), task_id, baseline_id, now_ms(), serde_json::to_string(report)?],
        )?;
        Ok(())
    }

    pub fn latest_task_check(&self, task_id: &str) -> Result<Option<StoredCheck>> {
        let row: Option<(i64, String)> = self.conn.lock().unwrap().query_row(
            "SELECT created_at, report FROM task_checks WHERE task_id = ?1 ORDER BY created_at DESC, rowid DESC LIMIT 1", [task_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        Ok(match row { Some((created_at, t)) => Some(StoredCheck { created_at, report: serde_json::from_str(&t)? }), None => None })
    }

    pub fn onboarding_state(&self, project_id: &str) -> Result<OnboardingState> {
        let p = self.get_project(project_id)?.context("项目不存在")?;
        Ok(OnboardingState { status: p.onboarding, scan: self.latest_scan(project_id)?, baseline: self.latest_baseline(project_id)?, reports: self.latest_reports(project_id)? })
    }
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

/// 扫描结果的文字摘要，用于提示和记忆。
pub fn summarize_scan(s: &ScanReport) -> String {
    let join = |v: &[String]| if v.is_empty() { "未识别".to_string() } else { v.join("、") };
    let mut out = String::new();
    out.push_str(&format!("语言：{}\n", if s.languages.is_empty() { "未识别".into() } else { s.languages.iter().take(6).map(|(l, n)| format!("{l}（{n} 个文件）")).collect::<Vec<_>>().join("、") }));
    out.push_str(&format!("包管理/构建：{} / {}\n", join(&s.package_managers), join(&s.build_tools)));
    out.push_str(&format!("框架：{}\n数据库与中间件：{}\n测试框架：{}\n", join(&s.frameworks), join(&s.databases), join(&s.test_frameworks)));
    out.push_str(&format!("CI：{}\nAPI 定义：{}\n数据库 Schema：{}\n", join(&s.ci), join(&s.api_definitions), join(&s.db_schemas)));
    if !s.modules.is_empty() {
        out.push_str("模块：");
        out.push_str(&s.modules.iter().take(25).map(|m| format!("{}（{}，{} 个文件）", m.name, m.path, m.files)).collect::<Vec<_>>().join("；"));
        out.push('\n');
    }
    let g = &s.git;
    if g.is_repo {
        out.push_str(&format!("Git：分支 {}，{} 次提交，{} 个分支", g.branch.as_deref().unwrap_or("（游离）"), g.commit_count.map_or("?".into(), |n| n.to_string()), g.branch_count.map_or("?".into(), |n| n.to_string())));
        if let Some(l) = &g.last_commit { out.push_str(&format!("，最近一次：{l}")); }
        out.push('\n');
    }
    clip(&out, 5000)
}

/// 基线结果的文字摘要。
pub fn summarize_baseline(b: &BaselineRun) -> String {
    let mut out = format!("基线提交 {}：\n", &b.commit_sha[..b.commit_sha.len().min(8)]);
    for r in &b.results {
        out.push_str(&format!("- {}（{}）：{}", r.label, r.command, match r.status.as_str() { "passed" => "通过", "failed" => "失败", "timeout" => "超时", "skipped" => "跳过", _ => "出错" }));
        if !r.failed_tests.is_empty() {
            out.push_str(&format!("，{} 个测试失败：{}", r.failed_tests.len(), r.failed_tests.iter().take(20).cloned().collect::<Vec<_>>().join("、")));
        } else if r.status == "failed" {
            out.push_str("（没有解析出逐项失败，可能是编译或环境问题）");
        }
        out.push('\n');
    }
    clip(&out, 5000)
}

impl Orchestrator {
    fn project_for_onboarding(&self, project_id: &str) -> Result<crate::db::Project> {
        self.db.get_project(project_id)?.context("项目不存在")
    }

    pub async fn scan_project(&self, project_id: &str) -> Result<ScanReport> {
        let p = self.project_for_onboarding(project_id)?;
        let report = scan::scan_project(Path::new(&p.path)).await?;
        self.db.save_scan(project_id, report.git.head.as_deref(), &report)?;
        self.db.append_event_for(Some(project_id), "onboarding_scanned", None, None, &json!({"files": report.total_files, "modules": report.modules.len()}))?;
        Ok(report)
    }

    /// 在已提交 HEAD 的干净检出里运行基线命令。不触碰用户的工作目录。
    pub async fn run_baseline(&self, project_id: &str, commands: Vec<BaselineCommand>, timeout: Duration) -> Result<BaselineRun> {
        if commands.is_empty() {
            bail!("请至少选择一条要运行的命令");
        }
        let p = self.project_for_onboarding(project_id)?;
        let repo = Path::new(&p.path);
        let head = git_output(repo, &["rev-parse", "HEAD"]).await.map_err(|_| anyhow::anyhow!("仓库还没有任何提交，无法建立基线"))?.trim().to_string();
        let id = self.db.create_baseline(project_id, &head, &commands)?;
        self.db.append_event_for(Some(project_id), "baseline_started", None, None, &json!({"baseline_id": id, "commands": commands.len()}))?;
        let dest = self.ws.scratch_dir("baselines").join(project_id).join(&id);
        let outcome: Result<Vec<CommandResult>> = async {
            self.ws.add_detached(repo, &dest, &head).await?;
            let (db, pid) = (self.db.clone(), project_id.to_string());
            Ok(run_commands(&dest, &commands, timeout, |ev| {
                let (kind, id, label, status) = match ev {
                    Progress::Started(c) => ("baseline_command_started", c.id.clone(), c.label.clone(), "running".to_string()),
                    Progress::Finished(r) => ("baseline_command_finished", r.id.clone(), r.label.clone(), r.status.clone()),
                };
                let _ = db.append_event_for(Some(&pid), kind, None, None, &json!({"command_id": id, "label": label, "status": status}));
            }).await)
        }.await;
        self.ws.remove_checkout(repo, &dest).await;
        match outcome {
            Ok(results) => {
                self.db.finish_baseline(&id, "done", &results)?;
                self.db.append_event_for(Some(project_id), "baseline_finished", None, None, &json!({"baseline_id": id, "failed": results.iter().filter(|r| r.status != "passed" && r.status != "skipped").count()}))?;
                Ok(self.db.latest_baseline(project_id)?.context("基线记录丢失")?)
            }
            Err(e) => {
                self.db.finish_baseline(&id, "failed", &[])?;
                self.db.append_event_for(Some(project_id), "baseline_finished", None, None, &json!({"baseline_id": id, "error": e.to_string()}))?;
                Err(e)
            }
        }
    }

    /// 用基线同样的命令检查某个任务的改动，逐项判定失败来源。
    /// 在任务分支的干净检出里运行（不是员工的 worktree），避免构建产物污染员工的改动。
    pub async fn check_task(&self, task_id: &str, timeout: Duration) -> Result<CheckReport> {
        let task = self.db.get_task(task_id)?.context("任务不存在")?;
        if task.status != "active" {
            bail!("只能检查进行中的任务");
        }
        let emp = self.db.employee_by_id(&task.employee_id)?.context("员工不存在")?;
        let project = self.db.project_by_path(&task.repo_path)?.context("任务所属目录不是已登记的项目")?;
        let base = self.db.current_baseline(&project.id)?.context("还没有测试基线，请先在“导入”页建立基线")?;
        // 先保存员工的未提交改动，才能检查到最新状态
        let msg = format!("[crewforge] {}: {} (检查前保存)\n\nCrewforge-Task: {}", emp.name, task.title, task.id);
        let _ = self.ws.checkpoint(Path::new(&task.worktree_path), &emp.name, &msg).await;
        let repo = Path::new(&task.repo_path);
        let dest = self.ws.scratch_dir("checks").join(&task.id);
        self.ws.remove_checkout(repo, &dest).await;
        self.ws.add_detached(repo, &dest, &task.branch).await?;
        let results = run_commands(&dest, &base.commands, timeout, |_| {}).await;
        self.ws.remove_checkout(repo, &dest).await;
        let report = classify(&base.results, &results);
        self.db.save_task_check(task_id, &base.id, &report)?;
        self.db.append_event_for(Some(&project.id), "task_checked", Some(&emp.id), None, &json!({
            "task_id": task_id, "pre_existing": report.pre_existing, "introduced": report.introduced, "fixed": report.fixed, "unknown": report.unknown,
        }))?;
        Ok(report)
    }

    /// AI 联合评估：全栈（architect）和 QA（qa）各出一份只读报告。
    pub async fn evaluate_project<A: Approver>(&self, project_id: &str, kind: &str, employee: &Employee, approver: &A) -> Result<String> {
        if !matches!(kind, "architect" | "qa") {
            bail!("未知的评估类型: {kind}");
        }
        let p = self.project_for_onboarding(project_id)?;
        if !self.db.is_member(project_id, &employee.id)? {
            bail!("{} 不是本项目成员", employee.name);
        }
        let scan = self.db.latest_scan(project_id)?.context("请先完成项目扫描")?;
        let baseline = self.db.current_baseline(project_id)?;
        let mut prompt = format!("你正在评估一个刚导入的既有项目「{}」，请只读地查看代码，基于事实给出评估。\n\n【自动扫描结果】\n{}", p.name, summarize_scan(&scan));
        match &baseline {
            Some(b) => prompt.push_str(&format!("\n【测试基线（导入时的原始结果）】\n{}\n注意：基线里已有的失败是导入前就存在的历史问题，不是 AI 引入的，请在评估里如实记录，不要归咎于后续修改。\n", summarize_baseline(b))),
            None => prompt.push_str("\n【测试基线】尚未建立。\n"),
        }
        prompt.push_str(if kind == "architect" {
            "\n你的职责是全栈工程师视角。请用 Markdown 输出，严格包含这些标题：\n## 项目概览\n## 架构说明\n## 模块地图\n## 技术债清单\n## 建议的开发计划\n"
        } else {
            "\n你的职责是 QA 视角。请用 Markdown 输出，严格包含这些标题：\n## 测试覆盖与缺失的回归测试\n## 潜在缺陷\n## 兼容性问题\n## 安全风险\n## 风险清单\n"
        });
        prompt.push_str("每条结论尽量指出具体的文件或模块；没有查证的内容写“未确认”，不要编造；不要修改任何文件。");
        let cfg = TurnConfig {
            project_path: Path::new(&p.path).to_path_buf(), task_id: None, project_id: None, read_only: true, meeting_id: None, mcp: None, purpose: "onboarding".into(), attr_project: Some(project_id.to_string()),
            log_prompt: Some(format!("[导入评估:{kind}] {}", p.name)), program: self.program.clone(),
        };
        let r = run_turn(&self.db, employee, &cfg, &prompt, approver, &NoopObserver).await?;
        if r.is_error || r.text.trim().is_empty() {
            bail!("{} 没有给出有效的评估（{}）", employee.name, r.subtype);
        }
        self.db.save_report(project_id, kind, Some(&employee.id), r.text.trim(), scan.git.head.as_deref())?;
        self.db.append_event_for(Some(project_id), "onboarding_report", Some(&employee.id), None, &json!({"kind": kind}))?;
        Ok(r.text)
    }

    /// 人类确认：项目进入 AI 团队维护阶段，并把技术栈与基线沉淀为项目记忆。
    pub fn confirm_onboarding(&self, project_id: &str, skip_baseline: bool) -> Result<()> {
        let p = self.project_for_onboarding(project_id)?;
        if p.onboarding != "in_progress" {
            bail!("这个项目不在导入阶段");
        }
        let scan = self.db.latest_scan(project_id)?.context("请先完成项目扫描")?;
        let baseline = self.db.current_baseline(project_id)?;
        if baseline.is_none() && !skip_baseline {
            bail!("还没有测试基线。没有基线就无法区分历史问题和 AI 引入的问题；如确实要跳过，请明确选择跳过");
        }
        let mem = |title: &str, content: String| NewMemory {
            layer: "project".into(), project_id: Some(project_id.into()), kind: "fact".into(), title: title.into(), content,
            source_kind: "onboarding".into(), created_by: "human".into(), status: "verified".into(), scope: "导入时确认".into(), ..Default::default()
        };
        self.db.add_memory(mem("技术栈与项目结构", summarize_scan(&scan)))?;
        if let Some(b) = &baseline {
            let failing: Vec<&CommandResult> = b.results.iter().filter(|r| r.status != "passed" && r.status != "skipped").collect();
            let body = if failing.is_empty() {
                format!("导入时测试基线全部通过。\n{}", summarize_baseline(b))
            } else {
                format!("以下失败在导入时就已存在，不是 AI 修改引入的：\n{}", summarize_baseline(b))
            };
            self.db.add_memory(mem("导入时的测试基线", body))?;
        } else {
            self.db.add_memory(mem("导入时没有建立测试基线", "导入时跳过了测试基线，之后无法区分历史问题与新引入的问题。".into()))?;
        }
        self.db.set_onboarding(project_id, "confirmed")?;
        self.db.append_event_for(Some(project_id), "onboarding_confirmed", None, None, &json!({"skipped_baseline": baseline.is_none()}))?;
        Ok(())
    }
}
