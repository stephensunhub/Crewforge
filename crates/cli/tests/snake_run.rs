//! 用真实的 Claude 带领一个小团队做完一个新项目：贪吃蛇（Rust → WebAssembly + TypeScript）。
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

const GOAL: &str = "用 Rust + TypeScript 做一个贪吃蛇小游戏，能在浏览器里玩，并且一条命令就能运行。\n\
技术栈：游戏逻辑全部用 Rust 实现并编译为 WebAssembly（wasm-bindgen；用 `wasm-pack build --target web --release` 构建；wasm-pack 作为 npm devDependency 装在项目里，不要全局安装）；界面、键盘输入、渲染用 TypeScript + Vite + HTML canvas。\n\
运行方式（硬性要求）：在项目根目录 `npm install` 之后，`npm run dev` 一条命令完成 wasm 构建并启动开发服务器（并打印访问地址）；`npm run build` 生成可部署的静态文件；`npm test` 运行 Rust 逻辑的单元测试（cargo test）并做 TypeScript 类型检查。\n\
功能：方向键或 WASD 控制；吃食物变长并加分；撞墙或撞到自己游戏结束；显示当前分数和最高分（localStorage 保存）；空格暂停/继续；结束后按回车重新开始；速度随分数逐渐加快。\n\
质量：游戏规则（移动、转向不能 180° 掉头、吃食物、碰撞、食物不会生成在蛇身上）必须是纯 Rust 逻辑，随机数可以注入以便测试，并有单元测试；仓库根目录的 README 写明怎么运行、怎么测试、怎么玩；.gitignore 要忽略 target、node_modules、dist、pkg 等构建产物；提交不要包含构建产物。";

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
        let _ = std::fs::write(&self.report_path, format!("# 贪吃蛇演练进度（自动生成）\n\n```\n{}\n```\n", r.join("\n")));
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
    async fn verify(&self, task: &Task) -> (bool, String) {
        if self.db.current_baseline(&self.pid).unwrap().is_some() {
            match self.orch.check_task(&task.id, Duration::from_secs(900)).await {
                Ok(rep) => {
                    let ok = rep.introduced == 0 && rep.unknown == 0;
                    let detail = rep.findings.iter().filter(|f| f.origin != crewforge_core::onboarding::baseline::Origin::Fixed).map(|f| format!("{:?} · {} · {} · {}", f.origin, f.command_label, f.item, f.note)).collect::<Vec<_>>().join("\n");
                    let tails: String = rep.results.iter().filter(|r| r.status != "passed" && r.status != "skipped").map(|r| format!("`{}` 输出末尾：\n{}", r.command, r.output_tail.chars().rev().take(1500).collect::<Vec<_>>().into_iter().rev().collect::<String>())).collect::<Vec<_>>().join("\n---\n");
                    (ok, format!("与基线对比：引入 {} · 已存在 {} · 已修复 {} · 无法确认 {}\n{detail}\n{tails}", rep.introduced, rep.pre_existing, rep.fixed, rep.unknown))
                }
                Err(e) => (false, format!("检查失败：{e}")),
            }
        } else {
            let (ok, summary, _) = self.verify_branch(&task.branch).await;
            (ok, summary)
        }
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

/// 假 CLI（空跑用）：每次在工作目录写一个文件；遇到“整理纪要”返回带行动项的纪要；遇到评审返回意见。
fn write_fake_cli(dir: &Path) -> String {
    let path = dir.join("fake-claude");
    std::fs::write(&path, r###"#!/bin/sh
read -r line
echo '{"type":"system","subtype":"init","session_id":"x"}'
f="w_$$_$(date +%s%N).txt"
case "$line" in
  *整理纪要*) printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"## 决议\n- 同意\n## 被否决的方案及原因\n无\n## 行动项\n- [合并前] alice：补一个边界说明\n- [之后] 人类：以后再看\n## 待验证的经验\n无","usage":{}}' ;;
  *请评审*) printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"评审意见：整体可以，有一处边界说明可以补充。","usage":{}}' ;;
  *) echo "x" > "$f"; printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"已完成。","usage":{}}' ;;
esac
"###).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

#[tokio::test]
#[ignore]
async fn build_snake_with_a_real_team() {
    let dry = std::env::var("SNAKE_DRYRUN").is_ok();
    let dir = PathBuf::from(std::env::var("SNAKE_DIR").unwrap_or_else(|_| "/Users/stephen/Documents/workspace/snake".into()));
    if dir.exists() && std::fs::read_dir(&dir).map(|mut d| d.next().is_some()).unwrap_or(false) {
        panic!("{} 已存在且不为空，请先处理（本演练只在全新的目录里创建项目）", dir.display());
    }
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let data = std::env::var("CREWFORGE_HOME").map(PathBuf::from).unwrap_or_else(|_| crewforge_core::db::default_data_dir());
    let db = Arc::new(Db::open(&data).unwrap());
    let ws = Arc::new(WorkspaceManager::new(&data));
    let mut orch = Orchestrator::new(db.clone(), ws.clone());
    orch.mcp_exe = Some(std::env::var("SNAKE_MCP_EXE").map(PathBuf::from).unwrap_or_else(|_| env!("CARGO_BIN_EXE_crewforge").into()));
    if dry { orch.program = write_fake_cli(&data); }
    else if let Ok(p) = std::env::var("SNAKE_PROGRAM") { orch.program = p; }
    let sched = Scheduler::new(db.clone(), 2);
    let report_path = dir.parent().unwrap().join("snake-REPORT.md");

    ws.ensure_ready(&dir, true).await.unwrap();
    let repo = ws.repo_info(&dir).await.unwrap().root;
    let _ = db.update_company("贪吃蛇工作室", "一个 2 全栈 + 1 质检的小团队", "代码要有测试；提交信息说明做了什么；不提交构建产物。");
    let mk = |n: &str, role: &str, persona: &str| db.employee_by_name(n).unwrap().unwrap_or_else(|| db.add_employee(n, role, persona).unwrap());
    let alice = mk("alice", "全栈工程师", "你是 Alice，资深全栈工程师，偏 Rust 与 WebAssembly：游戏逻辑与 wasm 绑定。先写测试，函数小而纯，沟通简洁，先给结论再给理由。");
    let bob = mk("bob", "全栈工程师", "你是 Bob，全栈工程师，偏 TypeScript 与前端：Vite、canvas 渲染、键盘输入、界面与手感。沟通简洁，改动小步提交。");
    let carol = mk("carol", "QA 测试工程师", "你是 Carol，AI 质检员（QA）：独立审查代码，设计并补充测试，寻找边界情况和 bug。不迁就开发者的结论，指出具体文件、行和复现方式。");
    let project = db.create_project("贪吃蛇", "Rust(WebAssembly) + TypeScript 的贪吃蛇小游戏", repo.to_str().unwrap()).unwrap();
    for (e, r) in [(&alice, "developer"), (&bob, "developer"), (&carol, "qa")] { db.add_member(&project.id, &e.id, r).unwrap(); }
    db.set_project_goal(&project.id, GOAL).unwrap();

    let ctl = Arc::new(Ctl { orch: Arc::new(orch), db: db.clone(), sched, approver: LoggingApprover(Mutex::new(vec![])), repo: repo.clone(), pid: project.id.clone(),
        merge_lock: tokio::sync::Mutex::new(()), calls: AtomicUsize::new(0), started: Instant::now(), report: Mutex::new(vec![]), report_path });
    ctl.say(format!("项目目录 {} · 数据目录 {} · {}", repo.display(), data.display(), if dry { "空跑" } else { "真实调用" }));

    // ---------- 1. 制定计划（AI 起草，CTO 批准）----------
    ctl.say("📋 让 alice 制定计划");
    let m = db.create_meeting(&ctl.pid, "planning", "制定计划", None, None, &[], 12).unwrap();
    db.post_message(&m.id, "human", None, "@alice 请为贪吃蛇项目制定一份计划，并用 propose_plan 一次性提交（先 list_plan 看看）。团队：alice（全栈，偏 Rust/wasm）、bob（全栈，偏 TypeScript/界面）、carol（QA）。\n\
        要求：6 到 10 个条目，每个条目一名员工一次任务能做完；第一个条目搭好脚手架（Cargo wasm 库 + Vite + TypeScript + 一键运行的 npm 脚本 + .gitignore + README 骨架）；用 depends_on 标明先后顺序，让 alice 和 bob 在脚手架之后能尽量并行（比如 Rust 游戏逻辑和 TypeScript 渲染/输入分开做，先约定好 wasm 暴露的接口）；至少一个条目交给 carol：为 Rust 逻辑补充边界测试并报告 bug；最后一个条目是整体联调：确认 `npm install && npm run dev` 能跑起来并能玩，README 写明运行、测试、玩法。每个条目写清 acceptance（最好能用命令验证）和 assignee。").unwrap();
    if !dry {
        let _p = ctl.permit().await;
        match ctl.orch.meeting_reply(&m.id, &alice, &ctl.approver).await { Ok(t) => ctl.say(format!("  alice：{}", clip(&t, 200))), Err(e) => ctl.say(format!("  （规划失败：{e}）")) }
    }
    let proposed = db.plan_view(&ctl.pid).unwrap().counts.proposed;
    if proposed >= 3 {
        ctl.say(format!("  alice 起草了 {proposed} 个条目，CTO 批准"));
        db.approve_proposed_plan(&ctl.pid).unwrap();
    } else {
        ctl.say(format!("  ⚠ AI 没有起草出可用的计划（{proposed} 条）；CTO 改用一份手写的备用计划{}", if dry { "（空跑本来就用备用计划）" } else { "" }));
        let mut ids: Vec<String> = vec![];
        let steps: [(&str, &str, &str, &[usize], &str); 5] = [
            ("搭建脚手架", "Cargo wasm 库 + Vite + TypeScript + npm 脚本（dev/build/test）+ .gitignore + README 骨架", "npm install && npm run build 成功", &[], "alice"),
            ("Rust 游戏逻辑", "蛇移动、转向（禁止 180° 掉头）、吃食物、碰撞、食物不生成在蛇身上；随机数可注入；wasm-bindgen 接口", "cargo test 通过且覆盖上述规则", &[0], "alice"),
            ("TypeScript 渲染与输入", "canvas 渲染、键盘输入、分数与最高分、暂停与重新开始", "npm run build 成功，npm run dev 能打开游戏", &[0], "bob"),
            ("QA：补充边界测试", "为 Rust 逻辑补充边界测试并报告 bug", "cargo test 通过", &[1], "carol"),
            ("整体联调与 README", "确认一键运行并能玩，README 写明运行、测试、玩法", "npm install && npm run dev 能玩", &[1, 2, 3], "bob"),
        ];
        for (t, d, a, deps, who) in steps {
            let deps: Vec<String> = deps.iter().map(|i| ids[*i].clone()).collect();
            let aid = db.employee_by_name(who).unwrap().map(|e| e.id);
            ids.push(db.create_plan_item(&ctl.pid, &NewPlanItem { title: t.into(), description: d.into(), acceptance: a.into(), kind: "migrate".into(), module: "".into(), assignee_id: aid, deps }, None).unwrap());
        }
    }
    for i in db.plan_view(&ctl.pid).unwrap().items { ctl.say(format!("  · [{}] {} → {}{}", i.kind, i.title, i.assignee_name.clone().unwrap_or_else(|| "未指定".into()), if i.deps.is_empty() { String::new() } else { format!("（依赖 {} 项）", i.deps.len()) })); }

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

    // ---------- 3. 最终验收：一键运行 ----------
    ctl.say("🏁 最终验收：在主目录里 npm install / build / test，并启动开发服务器");
    let st = git(&repo, &["status", "--porcelain"]);
    ctl.say(format!("  主分支工作区状态：{}", if st.is_empty() { "干净".to_string() } else { format!("有未提交内容：{}", clip(&st, 200)) }));
    let final_cmds: Vec<crewforge_core::onboarding::scan::BaselineCommand> = [("install", "setup", "npm install"), ("build", "build", "npm run build"), ("test", "test", "npm test")].iter()
        .map(|(id, k, c)| crewforge_core::onboarding::scan::BaselineCommand { id: (*id).into(), label: (*c).into(), kind: (*k).into(), command: (*c).into(), reason: String::new(), default_on: true, candidate: None }).collect();
    let results = if repo.join("package.json").exists() { run_commands(&repo, &final_cmds, Duration::from_secs(1200), |_| {}).await } else { vec![] };
    for r in &results { ctl.say(format!("  {} → {} ({}s)", r.command, r.status, r.duration_ms / 1000)); }
    let mut dev_ok = false;
    if repo.join("package.json").exists() && results.iter().all(|r| r.status == "passed") {
        let mut child = Command::new("sh").arg("-c").arg("npm run dev 2>&1").current_dir(&repo).stdout(Stdio::piped()).stdin(Stdio::null()).process_group(0).spawn().unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || { use std::io::BufRead; for l in std::io::BufReader::new(out).lines().map_while(Result::ok) { let _ = tx.send(l); } });
        let t0 = Instant::now();
        let mut url = None;
        let mut lines = vec![];
        while t0.elapsed() < Duration::from_secs(120) && url.is_none() {
            if let Ok(l) = rx.recv_timeout(Duration::from_secs(2)) {
                lines.push(l.clone());
                if let Some(i) = l.find("http://") { url = Some(l[i..].split_whitespace().next().unwrap_or("").trim_end_matches(['/', '\u{1b}']).to_string() + "/"); }
            }
        }
        if let Some(u) = &url {
            std::thread::sleep(Duration::from_secs(2));
            let o = Command::new("curl").args(["-s", "-m", "10", "-w", "\n%{http_code}", u]).output().unwrap();
            let body = String::from_utf8_lossy(&o.stdout).to_string();
            dev_ok = body.trim_end().ends_with("200") && (body.contains("<canvas") || body.contains("<script"));
            ctl.say(format!("  开发服务器 {u} → {}", if dev_ok { "200，页面里有 canvas/script" } else { "响应不符合预期" }));
        } else { ctl.say(format!("  ✗ 120 秒内没有启动开发服务器。输出：{}", clip(&lines.join(" ⏎ "), 300))); }
        unsafe { libc_killpg(child.id() as i32) };
        let _ = child.wait();
    }

    // ---------- 4. 汇总 ----------
    let cost = db.cost_report(None, 480).unwrap();
    let view = db.plan_view(&ctl.pid).unwrap();
    let mut rep = String::new();
    rep.push_str(&format!("\n\n## 结果\n- 计划完成 {}%（完成 {} / 总 {}）\n- 真实调用 {} 次，估算费用 ${:.2}，缓存命中 {:?}\n- 一键运行验收：{}\n- 开发服务器：{}\n",
        view.percent_done, view.counts.done, view.items.len(), cost.totals.turns, cost.totals.cost_usd, cost.efficiency.cache_hit_ratio.map(|v| format!("{:.0}%", v * 100.0)),
        if !results.is_empty() && results.iter().all(|r| r.status == "passed") { "npm install / build / test 全部通过" } else { "未通过" }, if dev_ok { "能启动并返回页面" } else { "未验证通过" }));
    for g in &cost.by_employee { rep.push_str(&format!("- {}：{} 次 · ${:.2}\n", g.label, g.totals.turns, g.totals.cost_usd)); }
    rep.push_str("\n## 计划条目\n");
    for i in &view.items { rep.push_str(&format!("- [{}] {} → {}{}\n", crewforge_core::plan::status_label(&i.status), i.title, i.assignee_name.clone().unwrap_or_default(), if i.note.is_empty() { String::new() } else { format!("（{}）", i.note) })); }
    rep.push_str("\n## 议题 / 记忆 / 行动项\n");
    for i in db.list_issues(&ctl.pid).unwrap() { rep.push_str(&format!("- 议题《{}》{}（{}）\n", i.title, if i.raised_by.is_some() { "AI 提议" } else { "" }, i.status)); }
    for m in db.list_memories(&Default::default()).unwrap().iter().filter(|m| m.project_id.as_deref() == Some(ctl.pid.as_str())) { rep.push_str(&format!("- 记忆[{}] {}\n", m.status, clip(&m.content, 80))); }
    let asks = ctl.approver.0.lock().unwrap();
    rep.push_str(&format!("\n## 员工请求过 {} 次需要审批的操作\n", asks.len()));
    for a in asks.iter() { rep.push_str(&format!("- {a}\n")); }
    ctl.say(format!("演练结束。{}", clip(&rep, 0)));
    let mut f = std::fs::OpenOptions::new().append(true).open(&ctl.report_path).unwrap();
    let _ = writeln!(f, "{rep}");
    println!("{rep}");
    drop(asks);
    if !dry {
        assert!(view.percent_done >= 80, "计划完成度不足：{}%", view.percent_done);
        assert!(!results.is_empty() && results.iter().all(|r| r.status == "passed"), "最终的 npm install/build/test 没有全部通过");
    }
}

unsafe fn libc_killpg(pid: i32) {
    unsafe extern "C" { fn killpg(pgrp: i32, sig: i32) -> i32; }
    unsafe { killpg(pid, 9) };
}
