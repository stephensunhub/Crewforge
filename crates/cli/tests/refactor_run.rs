//! 重构演练：把一个遗留的 Python 命令行工具（ledger）迁移到 Rust，业务行为不变，以旧实现的输出为标准答案。
//! 团队：alice（全栈，偏 Rust/wasm）、bob（全栈，偏 TypeScript/界面）、carol（AI 质检员）；人类 CTO 由本驱动扮演。
//!
//! 运行（默认忽略，会消耗大量真实额度）：
//!   cargo test -p crewforge-cli --test snake_run -- --ignored --nocapture
//! 环境变量：SNAKE_DIR（项目目录，默认 /Users/stephen/Documents/workspace/snake）、CREWFORGE_HOME（数据目录，默认真实目录）、
//!   SNAKE_PROGRAM（替换 claude，用于空跑）、SNAKE_DRYRUN=1（空跑：用假 CLI 和固定计划走通驱动自身的流程）、SNAKE_MCP_EXE。
//!
//! CTO 的决策规则（写死、可审计）：验证没通过不合并；评审里“合并前”的行动项交给负责人修，其余写明原因驳回；
//! 合并冲突交给负责人解决；每个任务最多 3 次修复派发；总调用数与总时长有上限。

use crewforge_core::db::{Db, Employee, Task};
use crewforge_core::onboarding::baseline::{CommandResult, run_commands};
use crewforge_core::onboarding::scan::scan_dir;
use crewforge_core::orchestrator::Orchestrator;
use crewforge_core::plan::{NewPlanItem, PlanItem};
use crewforge_core::provider::claude::{ApprovalRequest, Decision};
use crewforge_core::runner::{Approver, NoopObserver};
use crewforge_core::scheduler::{Permit, Scheduler};
use crewforge_core::workspace::{MergeOutcome, WorkspaceManager};
use std::collections::HashSet;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crewforge_core::onboarding::baseline::{Origin, apply_parity, run_commands_mode};
use crewforge_core::onboarding::scan::BaselineCommand;

const GOAL: &str = "把遗留的 Python 命令行工具 ledger（ledger.py，读取流水 CSV 输出汇总报表）迁移到 Rust，技术栈升级、业务行为完全不变。\n\
新实现放在仓库的 rust/ 目录（独立的 Cargo 项目，二进制名 ledger），Python 版本保持原样不删，它的输出就是标准答案。\n\
硬性要求：对任何输入文件，新实现的 stdout 必须与旧实现逐字一致（包括金额的千分位与四舍五入、按字符宽度的列对齐、排序的并列规则、无效行的跳过方式），退出码一致（0 正常、1 找不到文件、2 用法错误），用法错误与跳过提示写 stderr。\n\
对照检查：仓库的对照命令把 `python3 ledger.py ARGS` 与 `cargo run -q --manifest-path rust/Cargo.toml -- ARGS` 的输出逐字比较，比较通过才算迁移正确。\n\
不要引入第三方依赖，除非确有必要（CSV 解析可自己写，注意带引号字段与转义引号）；Rust 里要有单元测试；README 增加迁移说明（如何构建、如何运行对照）。\n\
提交不要包含 target/ 等构建产物。";

struct LoggingApprover(Mutex<Vec<String>>);
impl Approver for LoggingApprover {
    async fn ask(&self, employee: &str, req: &ApprovalRequest, reason: &str) -> Decision {
        let cmd = req.input.get("command").and_then(|c| c.as_str()).unwrap_or("");
        let local_only = !cmd.contains("curl") || cmd.contains("localhost") || cmd.contains("127.0.0.1");
        let dangerous = ["rm -rf /", "rm -rf ~", "sudo", "wget", "git push", "ssh ", "| sh", "| bash"].iter().any(|p| cmd.contains(p)) || !local_only;
        let allow = req.tool_name == "Bash" && !dangerous;
        let short: String = cmd.chars().take(140).collect::<String>().replace('\n', " ⏎ ");
        self.0.lock().unwrap().push(format!("[{employee}] {} {} → {}（{reason}）", req.tool_name, if short.is_empty() { req.input.to_string().chars().take(100).collect() } else { short }, if allow { "放行" } else { "拒绝" }));
        if allow { Decision::Allow } else { Decision::Deny("演练中不允许这个操作".into()) }
    }
}

struct Ctl {
    orch: Arc<Orchestrator>,
    db: Arc<Db>,
    sched: Arc<Scheduler>,
    approver: LoggingApprover,
    repo: PathBuf,
    pid: String,
    merge_lock: tokio::sync::Mutex<()>,
    calls: AtomicUsize,
    started: Instant,
    report: Mutex<Vec<String>>,
    report_path: PathBuf,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

impl Ctl {
    fn say(&self, msg: impl std::fmt::Display) {
        let line = format!("[{:>5}s] {msg}", self.started.elapsed().as_secs());
        println!("{line}");
        let mut r = self.report.lock().unwrap();
        r.push(line);
        let _ = std::fs::write(&self.report_path, format!("# 重构演练进度（自动生成）\n\n```\n{}\n```\n", r.join("\n")));
    }

    async fn permit(&self) -> Permit {
        let t = Instant::now();
        let p = self.sched.acquire().await;
        if t.elapsed() > Duration::from_secs(30) {
            self.say(format!("（在调度器排队等了 {} 秒：{}）", t.elapsed().as_secs(), self.sched.status().reason));
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        p
    }

    fn over_budget(&self) -> Option<String> {
        if self.calls.load(Ordering::SeqCst) >= 60 { return Some("真实调用次数达到上限 60".into()); }
        if self.started.elapsed() > Duration::from_secs(6 * 3600) { return Some("总时长超过 6 小时".into()); }
        None
    }

    fn members(&self) -> Vec<(Employee, String)> {
        self.db.list_members(&self.pid).unwrap().into_iter().map(|m| (self.db.employee_by_id(&m.employee_id).unwrap().unwrap(), m.role)).collect()
    }

    /// 在干净检出里验证某个分支：扫描出构建/测试命令并运行（没有基线时用）。
    async fn verify_branch(&self, branch: &str) -> (bool, String, Vec<CommandResult>) {
        let dest = self.orch.ws.scratch_dir("verify").join(branch.replace('/', "_"));
        self.orch.ws.remove_checkout(&self.repo, &dest).await;
        if let Err(e) = self.orch.ws.add_detached(&self.repo, &dest, branch).await { return (false, format!("无法检出 {branch}: {e}"), vec![]); }
        let scan = scan_dir(&dest);
        let cmds: Vec<_> = scan.commands.into_iter().filter(|c| c.default_on && c.kind != "lint").collect();
        let results = if cmds.is_empty() { vec![] } else { run_commands(&dest, &cmds, Duration::from_secs(900), |_| {}).await };
        self.orch.ws.remove_checkout(&self.repo, &dest).await;
        let bad: Vec<&CommandResult> = results.iter().filter(|r| r.status != "passed" && r.status != "skipped").collect();
        let summary = if cmds.is_empty() { "没有可运行的构建/测试命令".to_string() } else {
            bad.iter().map(|r| format!("命令 `{}` {}：\n{}", r.command, r.status, r.output_tail.chars().rev().take(1800).collect::<Vec<_>>().into_iter().rev().collect::<String>())).collect::<Vec<_>>().join("\n---\n")
        };
        (bad.is_empty(), summary, results)
    }

    /// 验证一个进行中的任务：有基线就和基线对比（引入新失败或无法确认都算不通过），否则直接构建测试。
    /// 与基线对比：对照命令只在“当前条目已经负责”的范围内要求通过（后面的条目还没实现对应的子命令）。
    async fn verify(&self, task: &Task) -> (bool, String) {
        let title = self.db.plan_view(&self.pid).unwrap().items.into_iter().find(|i| i.task_id.as_deref() == Some(task.id.as_str())).map(|i| i.title).unwrap_or_default();
        let in_scope = |label: &str| SCOPES.iter().find(|(t, _)| *t == title).map(|(_, ls)| ls.iter().any(|l| format!("对照:{l}") == label)).unwrap_or(true);
        let rep = match self.orch.check_task(&task.id, Duration::from_secs(900)).await { Ok(r) => r, Err(e) => return (false, format!("检查失败：{e}")) };
        let bad: Vec<_> = rep.findings.iter().filter(|f| matches!(f.origin, Origin::Introduced | Origin::Unknown) && (!f.command_label.starts_with("对照:") || in_scope(&f.command_label))).collect();
        let mut detail = bad.iter().map(|f| format!("{:?} · {} · {} · {}", f.origin, f.command_label, f.item, f.note)).collect::<Vec<_>>().join("\n");
        for r in rep.results.iter().filter(|r| r.status != "passed" && r.status != "skipped" && (!r.label.starts_with("对照:") || in_scope(&r.label))) {
            detail.push_str(&format!("\n`{}` 输出末尾：\n{}", r.command, r.output_tail.chars().rev().take(1500).collect::<Vec<_>>().into_iter().rev().collect::<String>()));
        }
        // 新实现自己的单元测试也要过
        let dest = self.orch.ws.scratch_dir("verify").join(task.branch.replace('/', "_"));
        self.orch.ws.remove_checkout(&self.repo, &dest).await;
        let mut cargo_ok = true;
        if self.orch.ws.add_detached(&self.repo, &dest, &task.branch).await.is_ok() && dest.join("rust/Cargo.toml").exists() {
            let c = BaselineCommand { id: "ct".into(), label: "cargo test".into(), kind: "test".into(), command: "cargo test --manifest-path rust/Cargo.toml".into(), reason: String::new(), default_on: true, candidate: None };
            let rs = run_commands(&dest, &[c], Duration::from_secs(900), |_| {}).await;
            if let Some(r) = rs.first() { if r.status != "passed" { cargo_ok = false; detail.push_str(&format!("\ncargo test {}：\n{}", r.status, r.output_tail.chars().rev().take(1800).collect::<Vec<_>>().into_iter().rev().collect::<String>())); } }
        }
        self.orch.ws.remove_checkout(&self.repo, &dest).await;
        let ok = bad.is_empty() && cargo_ok && !detail.contains("输出末尾");
        (ok, format!("与基线对比：引入 {} · 已存在 {} · 已修复 {} · 无法确认 {}（范围内未通过 {}）\n{detail}", rep.introduced, rep.pre_existing, rep.fixed, rep.unknown, bad.len()))
    }

    async fn work(&self, emp: &Employee, prompt: &str) -> anyhow::Result<()> {
        let _p = self.permit().await;
        let r = self.orch.assign(emp, &self.repo, prompt, &self.approver, &NoopObserver).await?;
        if r.is_error { anyhow::bail!("{} 本轮出错：{}", emp.name, r.subtype); }
        Ok(())
    }

    fn pick(&self, item: &PlanItem, busy: &HashSet<String>) -> Option<Employee> {
        let ms = self.members();
        if let Some(a) = &item.assignee_id { return ms.iter().find(|(e, _)| &e.id == a && !busy.contains(&e.id)).map(|(e, _)| e.clone()); }
        let qa_like = item.kind == "characterize" || ["测试", "QA", "质检", "评审", "验收"].iter().any(|k| item.title.contains(k));
        let mut order: Vec<&(Employee, String)> = ms.iter().filter(|(_, r)| (r == "qa") == qa_like).collect();
        order.extend(ms.iter().filter(|(_, r)| (r == "qa") != qa_like));
        order.into_iter().find(|(e, _)| !busy.contains(&e.id)).map(|(e, _)| e.clone())
    }

    /// 一个计划条目的完整流水线：开发 → 验证（失败则修）→ 评审 → 处理行动项 → 合并。
    async fn process_item(self: &Arc<Self>, item_id: String, emp: Employee) -> anyhow::Result<()> {
        let item = self.db.get_plan_item(&item_id)?.unwrap();
        self.say(format!("▶ 《{}》交给 {}", item.title, emp.name));
        let (emp, path, prompt) = self.orch.prepare_plan_dispatch(&item_id, Some(&emp.id))?;
        {
            let _p = self.permit().await;
            let r = self.orch.assign_for_plan(&emp, &path, &prompt, &item_id, &self.approver, &NoopObserver).await?;
            self.say(format!("  {} 完成《{}》第一轮（费用≈{:?}）：{}", emp.name, item.title, r.cost_usd, clip(&r.text, 160)));
        }
        let task = self.db.get_task(self.db.get_plan_item(&item_id)?.unwrap().task_id.as_deref().unwrap())?.unwrap();

        // 验证，不通过就让负责人修（最多 3 次）
        let mut dispatches = 0;
        let mut verdict = self.verify(&task).await;
        while !verdict.0 && dispatches < 3 {
            self.say(format!("  ✗ 验证未通过（第 {} 次），让 {} 修：{}", dispatches + 1, emp.name, clip(&verdict.1, 200)));
            self.work(&emp, &format!("验证没有通过，请修复。验证输出：\n{}\n如果失败是因为你发现了代码里真实的 bug、而修复超出了这个条目的范围，请把对应的测试用 #[ignore] 标注并在说明里写清复现方式，不要提交会失败的测试。修好后运行测试确认。", verdict.1)).await?;
            dispatches += 1;
            verdict = self.verify(&task).await;
        }
        if !verdict.0 {
            self.db.set_plan_status(&item_id, "blocked", &format!("验证仍未通过：{}", clip(&verdict.1, 300)))?;
            anyhow::bail!("《{}》验证仍未通过", item.title);
        }
        self.say(format!("  ✓ 验证通过：{}", clip(&verdict.1.lines().next().unwrap_or(""), 120)));

        // 评审：QA 评审开发者的任务；QA 自己的任务由一位全栈评审
        let ms = self.members();
        let reviewer = if ms.iter().any(|(e, r)| r == "qa" && e.id == emp.id) {
            ms.iter().find(|(e, r)| r != "qa" && e.id != emp.id).map(|(e, _)| e.clone())
        } else { ms.iter().find(|(_, r)| r == "qa").map(|(e, _)| e.clone()) };
        if let Some(rv) = reviewer {
            let m = self.db.create_meeting(&self.pid, "code_review", &format!("评审：{}", item.title), Some(&rv.id), Some(&task.id), &[], 6)?;
            self.db.post_message(&m.id, "human", None, &format!("@{} 请评审 {} 的改动《{}》。重点看：逻辑是否正确、边界情况、测试是否充分、是否符合项目目标；发现需要团队决定的设计问题请先 list_issues 再 propose_issue。", rv.name, emp.name, item.title))?;
            {
                let _p = self.permit().await;
                match self.orch.meeting_reply(&m.id, &rv, &self.approver).await {
                    Ok(t) => self.say(format!("  💬 {} 评审：{}", rv.name, clip(&t, 220))),
                    Err(e) => self.say(format!("  （评审失败：{e}）")),
                }
            }
            {
                let _p = self.permit().await;
                if let Err(e) = self.orch.draft_minutes(&m.id, &self.approver).await { self.say(format!("  （纪要失败：{e}）")); }
            }
            let _ = self.db.close_meeting(&m.id);
            // 合并前的行动项：交给负责人修；不是这个任务负责人的、或超出预算的，写明原因驳回
            let mut fixed: Vec<String> = vec![];
            for a in self.db.actions_for_meeting(&m.id)?.into_iter().filter(|a| a.blocking && a.status == "open") {
                if a.for_human { self.db.set_action_status(&a.id, "dismissed", "CTO：需要人类决定的事，不影响这次合并的正确性，转入后续")?; continue; }
                let mine = a.assignee_id.as_deref().map(|x| x == emp.id).unwrap_or(true);
                if !mine || dispatches >= 3 + 3 {
                    self.db.set_action_status(&a.id, "dismissed", "CTO：不是这个任务负责人的事项，或本任务的修复预算已用完，转入后续跟进")?;
                    continue;
                }
                self.say(format!("  🔧 行动项交给 {}：{}", emp.name, clip(&a.text, 100)));
                {
                    let _p = self.permit().await;
                    let (e2, p2, pr2) = self.orch.prepare_action_dispatch(&a.id, Some(&emp.id))?;
                    self.orch.assign_for_action(&e2, &p2, &pr2, &a.id, &self.approver, &NoopObserver).await?;
                }
                dispatches += 1;
                fixed.push(a.id.clone());
            }
            if !fixed.is_empty() {
                let v = self.verify(&task).await;
                if !v.0 {
                    self.say(format!("  ✗ 修完行动项后验证未通过：{}", clip(&v.1, 200)));
                    self.work(&emp, &format!("处理评审意见之后验证没有通过，请修复：\n{}", v.1)).await?;
                    let v2 = self.verify(&task).await;
                    if !v2.0 { self.db.set_plan_status(&item_id, "blocked", "处理评审意见后验证仍未通过")?; anyhow::bail!("《{}》处理评审意见后验证仍未通过", item.title); }
                }
                for id in &fixed { self.db.set_action_status(id, "done", "")?; }
                self.say(format!("  ✓ {} 条评审行动项已处理并通过验证", fixed.len()));
            }
        }

        // 合并（串行），冲突交给负责人解决
        for attempt in 0..3 {
            let outcome = { let _g = self.merge_lock.lock().await; self.orch.merge_task(&task.id).await? };
            match outcome {
                MergeOutcome::Merged { commit } => { self.say(format!("  🔀 已合并《{}》 {}", item.title, &commit[..8])); return Ok(()); }
                MergeOutcome::Empty => { self.say(format!("  🔀《{}》没有改动", item.title)); return Ok(()); }
                MergeOutcome::Conflict { files } if attempt < 2 => {
                    self.say(format!("  ⚔ 合并冲突 {files:?}，让 {} 解决", emp.name));
                    self.work(&emp, &format!("你的分支与主分支（{}）有冲突：{}。请在你的工作区运行 `git merge {}`，解决冲突并保证两边的功能都保留、测试通过，然后提交。", task.base_branch, files.join("、"), task.base_branch)).await?;
                    let v = self.verify(&task).await;
                    if !v.0 { anyhow::bail!("解决冲突后验证未通过：{}", clip(&v.1, 200)); }
                }
                other => anyhow::bail!("合并失败：{other:?}"),
            }
        }
        anyhow::bail!("多次尝试后仍无法合并")
    }

    /// 第一个任务合并后建立基线，之后的任务用“对比基线检查”把关。
    async fn ensure_baseline(&self) {
        if self.db.current_baseline(&self.pid).unwrap().is_some() { return; }
        let Ok(scan) = self.orch.scan_project(&self.pid).await else { return };
        let cmds: Vec<_> = scan.commands.into_iter().filter(|c| c.default_on && c.kind != "lint").collect();
        if cmds.is_empty() { return; }
        self.say(format!("📏 建立测试基线：{:?}", cmds.iter().map(|c| c.command.clone()).collect::<Vec<_>>()));
        match self.orch.run_baseline(&self.pid, cmds, Duration::from_secs(900)).await {
            Ok(b) => self.say(format!("  基线结果：{:?}", b.results.iter().map(|r| format!("{}:{}", r.label, r.status)).collect::<Vec<_>>())),
            Err(e) => self.say(format!("  基线失败：{e}")),
        }
    }
}


fn clip(s: &str, n: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= n { one } else { format!("{}…", one.chars().take(n).collect::<String>()) }
}


/// 每个条目负责的对照命令（后面的条目还没实现的子命令，不拿来卡前面的条目）。
const SCOPES: &[(&str, &[&str])] = &[
    ("Rust 脚手架、CSV 解析与 summary", &["summary-sample", "summary-edge"]),
    ("Rust：by-category、monthly、top、report", &["summary-sample", "summary-edge", "bycat-name", "bycat-amount-edge", "monthly-edge", "top-sample", "report-sample"]),
    ("移植单元测试到 Rust", &[]),
    ("QA：用更多边界数据做新旧对拍", &["summary-sample", "summary-edge", "bycat-name", "bycat-amount-edge", "monthly-edge", "top-sample", "report-sample"]),
    ("迁移说明与收尾", &["summary-sample", "summary-edge", "bycat-name", "bycat-amount-edge", "monthly-edge", "top-sample", "report-sample"]),
];

fn parity(id: &str, args: &str) -> BaselineCommand {
    BaselineCommand { id: id.into(), label: format!("对照:{id}"), kind: "parity".into(), command: format!("python3 ledger.py {args}"),
        reason: "旧实现的输出是标准答案".into(), default_on: true, candidate: Some(format!("cargo run -q --manifest-path rust/Cargo.toml -- {args}")) }
}

fn parity_commands() -> Vec<BaselineCommand> {
    vec![
        BaselineCommand { id: "unittest".into(), label: "Python 单元测试".into(), kind: "test".into(), command: "python3 -m unittest discover -s tests".into(), reason: "遗留测试".into(), default_on: true, candidate: None },
        parity("summary-sample", "summary data/sample.csv"),
        parity("summary-edge", "summary data/edge.csv"),
        parity("bycat-name", "by-category data/sample.csv"),
        parity("bycat-amount-edge", "by-category data/edge.csv --sort amount"),
        parity("monthly-edge", "monthly data/edge.csv"),
        parity("top-sample", "top 5 data/sample.csv"),
        parity("report-sample", "report data/sample.csv --month 2024-03"),
    ]
}

#[tokio::test]
#[ignore]
async fn refactor_python_to_rust_with_a_real_team() {
    let dir = PathBuf::from(std::env::var("REFACTOR_DIR").unwrap_or_else(|_| "/Users/stephen/Documents/workspace/refactor-demo/ledger".into())).canonicalize().unwrap();
    let hidden = std::env::var("REFACTOR_HIDDEN").ok().map(PathBuf::from);
    let data = std::env::var("CREWFORGE_HOME").map(PathBuf::from).unwrap_or_else(|_| crewforge_core::db::default_data_dir());
    let db = Arc::new(Db::open(&data).unwrap());
    let ws = Arc::new(WorkspaceManager::new(&data));
    let mut orch = Orchestrator::new(db.clone(), ws.clone());
    orch.mcp_exe = Some(std::env::var("SNAKE_MCP_EXE").map(PathBuf::from).unwrap_or_else(|_| env!("CARGO_BIN_EXE_crewforge").into()));
    let sched = Scheduler::new(db.clone(), 2);
    let report_path = dir.parent().unwrap().join("refactor-REPORT.md");

    ws.ensure_ready(&dir, true).await.unwrap();
    let repo = ws.repo_info(&dir).await.unwrap().root;
    let _ = db.update_company("重构工作室", "把遗留系统迁移到新技术栈的小团队", "业务行为不变；新旧实现必须逐字对照；代码要有测试；提交信息说明做了什么。");
    let mk = |n: &str, role: &str, persona: &str| db.employee_by_name(n).unwrap().unwrap_or_else(|| db.add_employee(n, role, persona).unwrap());
    let alice = mk("alice", "全栈工程师", "你是 Alice，资深全栈工程师，偏 Rust。做迁移时先读懂旧实现的每个细节（舍入、排序、对齐、错误处理），再写新实现；用对照命令自己验证。沟通简洁。");
    let bob = mk("bob", "全栈工程师", "你是 Bob，全栈工程师。擅长把测试和文档迁移到新实现，细心核对行为一致。沟通简洁，小步提交。");
    let carol = mk("carol", "QA 测试工程师", "你是 Carol，AI 质检员：设计新旧实现的对拍用例，专找边界差异（浮点舍入、Unicode 宽度、空输入、异常行），报告具体复现命令。");
    let project = match db.project_by_path(repo.to_str().unwrap()).unwrap() {
        Some(p) => p,
        None => db.create_project("ledger 迁移", "Python → Rust 的技术栈升级重构", repo.to_str().unwrap()).unwrap(),
    };
    for (e, r) in [(&alice, "developer"), (&bob, "developer"), (&carol, "qa")] { let _ = db.add_member(&project.id, &e.id, r); }
    db.set_onboarding(&project.id, "in_progress").unwrap();
    db.set_project_goal(&project.id, GOAL).unwrap();

    let ctl = Arc::new(Ctl { orch: Arc::new(orch), db: db.clone(), sched, approver: LoggingApprover(Mutex::new(vec![])), repo: repo.clone(), pid: project.id.clone(),
        merge_lock: tokio::sync::Mutex::new(()), calls: AtomicUsize::new(0), started: Instant::now(), report: Mutex::new(vec![]), report_path });
    ctl.say(format!("项目目录 {} · 数据目录 {}", repo.display(), data.display()));
    let dry = false;

    // ---------- 0. 导入：扫描 → 建立基线（含对照命令）→ 确认 ----------
    let scan = ctl.orch.scan_project(&ctl.pid).await.unwrap();
    ctl.say(format!("🔍 扫描：语言 {:?} · {} 个文件", scan.languages, scan.total_files));
    let base = ctl.orch.run_baseline(&ctl.pid, parity_commands(), Duration::from_secs(300)).await.unwrap();
    for r in &base.results { ctl.say(format!("  基线 {} → {}", r.label, r.status)); }
    assert!(base.results.iter().all(|r| r.status == "passed"), "旧实现的基线应全部通过");
    ctl.orch.confirm_onboarding(&ctl.pid, false).unwrap();
    ctl.say("✅ 导入已确认");

    // ---------- 1. 迁移计划（有顺序、有依赖）----------
    let mut ids: Vec<String> = vec![];
    let steps: [(&str, &str, &str, &[usize], &str); 5] = [
        ("Rust 脚手架、CSV 解析与 summary", "在 rust/ 下建立独立 Cargo 项目（二进制 ledger，.gitignore 忽略 target），实现 CSV 读取（含带引号字段与转义引号、无效行跳过并向 stderr 报告、与旧实现相同的行号与提示文字）、金额/日期解析、金额格式化（千分位、四舍五入规则与 Python 的 f\"{x:,.2f}\" 完全一致）和 summary 子命令；退出码与用法错误处理与旧实现一致。", "summary 两个对照命令通过；cargo test 通过", &[], "alice"),
        ("Rust：by-category、monthly、top、report", "实现其余子命令：by-category（--sort name|amount，列对齐按字符数 ljust/rjust，注意中文）、monthly（含累计）、top N、report --month。排序并列规则、空分类显示“未分类”、分类名转小写等细节与旧实现一致。", "全部 7 个对照命令通过；cargo test 通过", &[0], "alice"),
        ("移植单元测试到 Rust", "把 tests/test_ledger.py 里的每个用例移植为 Rust 测试（单元或集成测试），断言与旧实现一致的行为；另外补充金额格式化、CSV 解析的单元测试。", "cargo test 通过且覆盖旧测试的每个用例", &[0], "bob"),
        ("QA：用更多边界数据做新旧对拍", "在 data/ 下新增若干边界数据（空文件、只有表头、全是无效行、巨大金额、负零、极小数、Unicode 全角字符与表情、行尾空白、CRLF 换行、重复行、同日同额并列），对 7 种子命令逐一做新旧对拍（stdout 和退出码）。发现差异就在 Rust 一侧修复（旧实现是标准答案），并把对拍写成可重复运行的脚本 scripts/parity.sh。", "scripts/parity.sh 运行全部通过", &[1], "carol"),
        ("迁移说明与收尾", "README 增加迁移说明：如何构建 Rust 版本、如何运行对照（scripts/parity.sh）、与旧实现的已知差异（应为无）。确认 cargo test 与全部对照命令通过，仓库没有提交构建产物。", "cargo test 通过；全部对照命令通过", &[1, 2], "bob"),
    ];
    for (t, d, a, deps, who) in steps {
        let deps: Vec<String> = deps.iter().map(|i| ids[*i].clone()).collect();
        let aid = db.employee_by_name(who).unwrap().map(|e| e.id);
        ids.push(db.create_plan_item(&ctl.pid, &NewPlanItem { title: t.into(), description: d.into(), acceptance: a.into(), kind: "migrate".into(), module: "".into(), assignee_id: aid, deps }, None).unwrap());
    }
    for i in db.plan_view(&ctl.pid).unwrap().items { ctl.say(format!("  · {} → {}", i.title, i.assignee_name.clone().unwrap_or_default())); }

    // ---------- 2. 按依赖并行推进 ----------
    let mut set: tokio::task::JoinSet<(String, anyhow::Result<()>)> = tokio::task::JoinSet::new();
    let (mut running, mut busy, mut failed): (HashSet<String>, HashSet<String>, HashSet<String>) = Default::default();
    let mut stop_reason = String::new();
    loop {
        if let Some(r) = ctl.over_budget() { stop_reason = r; break; }
        let view = db.plan_view(&ctl.pid).unwrap();
        if view.items.iter().filter(|i| i.status != "cancelled").all(|i| i.status == "done") { break; }
        let candidates: Vec<&PlanItem> = view.items.iter().filter(|i| i.status == "planned" && i.ready && !running.contains(&i.id) && !failed.contains(&i.id)).collect();
        for it in candidates {
            if let Some(emp) = ctl.pick(it, &busy) {
                running.insert(it.id.clone());
                busy.insert(emp.id.clone());
                let (c, id) = (ctl.clone(), it.id.clone());
                set.spawn(async move { let r = c.process_item(id.clone(), emp).await; (id, r) });
            }
        }
        if set.is_empty() { stop_reason = "没有可以开始的条目了（前置失败或被阻塞）".into(); break; }
        if let Some(Ok((id, res))) = set.join_next().await {
            running.remove(&id);
            let item = db.get_plan_item(&id).unwrap().unwrap();
            if let Some(a) = &item.assignee_id { busy.remove(a); }
            // 释放这个条目占用的员工（assignee 可能是系统挑的）
            busy.clear();
            for rid in &running { if let Some(t) = db.get_plan_item(rid).unwrap().and_then(|i| i.task_id) { if let Some(t) = db.get_task(&t).unwrap() { busy.insert(t.employee_id); } } }
            if let Err(e) = res {
                ctl.say(format!("✗ 《{}》失败：{e}", item.title));
                failed.insert(id);
            } else { ctl.ensure_baseline().await; }
        }
    }
    while set.join_next().await.is_some() {}
    if !stop_reason.is_empty() { ctl.say(format!("⏹ 停止推进：{stop_reason}")); }


    // ---------- 3. 最终验收（独立于员工）----------
    ctl.say("🏁 最终验收：在主目录里运行 cargo test、全部对照命令，并用员工没见过的数据对拍");
    let ct = BaselineCommand { id: "ct".into(), label: "cargo test".into(), kind: "test".into(), command: "cargo test --manifest-path rust/Cargo.toml".into(), reason: String::new(), default_on: true, candidate: None };
    let cargo_res = if repo.join("rust/Cargo.toml").exists() { run_commands(&repo, &[ct], Duration::from_secs(900), |_| {}).await } else { vec![] };
    for r in &cargo_res { ctl.say(format!("  {} → {}", r.command, r.status)); }
    let baseline = db.current_baseline(&ctl.pid).unwrap().unwrap();
    let mut par = run_commands_mode(&repo, &parity_commands(), Duration::from_secs(600), true, |_| {}).await;
    apply_parity(&baseline.results, &mut par);
    for r in &par { ctl.say(format!("  对照 {} → {}", r.label, r.status)); }
    let mut hidden_report = String::new();
    let mut hidden_bad = 0;
    if let Some(h) = &hidden {
        let build = Command::new("cargo").args(["build", "-q", "--release", "--manifest-path", "rust/Cargo.toml"]).current_dir(&repo).status().map(|s| s.success()).unwrap_or(false);
        let bin = repo.join("rust/target/release/ledger");
        let mut files: Vec<_> = std::fs::read_dir(h).map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).collect()).unwrap_or_default();
        files.sort();
        for f in files.iter().filter(|f| f.extension().map(|e| e == "csv").unwrap_or(false)) {
            let fs = f.to_string_lossy().to_string();
            for args in [vec!["summary", &fs], vec!["by-category", &fs], vec!["by-category", &fs, "--sort", "amount"], vec!["monthly", &fs], vec!["top", "4", &fs], vec!["report", &fs, "--month", "2024-02"]] {
                let a = Command::new("python3").arg("ledger.py").args(&args).current_dir(&repo).output().unwrap();
                let (same, note) = if build && bin.exists() {
                    let b = Command::new(&bin).args(&args).current_dir(&repo).output().unwrap();
                    (a.stdout == b.stdout && a.status.code() == b.status.code(), format!("{}", if a.stdout == b.stdout { "退出码不同" } else { "stdout 不同" }))
                } else { (false, "Rust 版本未能构建".into()) };
                if !same { hidden_bad += 1; hidden_report.push_str(&format!("- ✗ {} {} ：{note}\n", args[0], f.file_name().unwrap().to_string_lossy())); }
                else { hidden_report.push_str(&format!("- ✓ {} {}\n", args[0], f.file_name().unwrap().to_string_lossy())); }
            }
        }
        ctl.say(format!("  隐藏数据对拍：{} 项不一致", hidden_bad));
    }

    // ---------- 4. 汇总 ----------
    let cost = db.cost_report(None, 480).unwrap();
    let view = db.plan_view(&ctl.pid).unwrap();
    let par_ok = !par.is_empty() && par.iter().all(|r| r.status == "passed");
    let mut rep = String::new();
    rep.push_str(&format!("\n\n## 结果\n- 计划完成 {}%（完成 {} / 总 {}）\n- 真实调用 {} 次，估算费用 ${:.2}，缓存命中 {:?}\n- cargo test：{}\n- 对照命令：{}（{} 条）\n- 隐藏数据对拍：{}\n",
        view.percent_done, view.counts.done, view.items.len(), cost.totals.turns, cost.totals.cost_usd, cost.efficiency.cache_hit_ratio.map(|v| format!("{:.0}%", v * 100.0)),
        cargo_res.first().map(|r| r.status.clone()).unwrap_or_else(|| "未运行".into()), if par_ok { "全部一致" } else { "有不一致" }, par.len(),
        if hidden.is_some() { format!("{} 项不一致", hidden_bad) } else { "未配置".into() }));
    if !hidden_report.is_empty() { rep.push_str(&format!("\n### 隐藏数据对拍明细\n{hidden_report}")); }
    for g in &cost.by_employee { rep.push_str(&format!("- {}：{} 次 · ${:.2}\n", g.label, g.totals.turns, g.totals.cost_usd)); }
    rep.push_str("\n## 计划条目\n");
    for i in &view.items { rep.push_str(&format!("- [{}] {} → {}{}\n", crewforge_core::plan::status_label(&i.status), i.title, i.assignee_name.clone().unwrap_or_default(), if i.note.is_empty() { String::new() } else { format!("（{}）", i.note) })); }
    rep.push_str("\n## 议题 / 记忆\n");
    for i in db.list_issues(&ctl.pid).unwrap() { rep.push_str(&format!("- 议题《{}》{}（{}）\n", i.title, if i.raised_by.is_some() { "AI 提议" } else { "" }, i.status)); }
    for m in db.list_memories(&Default::default()).unwrap().iter().filter(|m| m.project_id.as_deref() == Some(ctl.pid.as_str())) { rep.push_str(&format!("- 记忆[{}] {}\n", m.status, clip(&m.content, 80))); }
    let asks = ctl.approver.0.lock().unwrap();
    rep.push_str(&format!("\n## 员工请求过 {} 次需要审批的操作\n", asks.len()));
    for a in asks.iter() { rep.push_str(&format!("- {a}\n")); }
    let mut f = std::fs::OpenOptions::new().append(true).open(&ctl.report_path).unwrap();
    let _ = writeln!(f, "{rep}");
    println!("{rep}");
    drop(asks);
    let _ = dry;
    assert!(view.percent_done >= 80, "计划完成度不足：{}%", view.percent_done);
    assert!(par_ok, "对照命令没有全部一致");
}
