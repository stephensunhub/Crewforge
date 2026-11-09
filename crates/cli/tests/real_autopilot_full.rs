//! 真实 Claude 的完整自主推进演练（默认忽略）：写目标 → 负责人起草计划 → 开始自主推进 → 收尾（记忆、文档）。
//! 项目登记在真实数据目录里，可以在应用里查看。环境变量：AUTOPILOT_DIR（项目目录）、CREWFORGE_HOME（数据目录）、
//! AUTOPILOT_MCP_EXE（团队协作工具的可执行文件）、AUTOPILOT_CALLS / AUTOPILOT_MINUTES（预算）。
use crewforge_core::autopilot::{Autopilot, AutopilotCtl};
use crewforge_core::db::Db;
use crewforge_core::onboarding::scan::BaselineCommand;
use crewforge_core::orchestrator::Orchestrator;
use crewforge_core::provider::claude::{ApprovalRequest, Decision};
use crewforge_core::runner::Approver;
use crewforge_core::scheduler::Scheduler;
use crewforge_core::workspace::WorkspaceManager;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Logging(Mutex<Vec<String>>);
impl Approver for Logging {
    async fn ask(&self, e: &str, req: &ApprovalRequest, why: &str) -> Decision {
        let cmd = req.input.get("command").and_then(|c| c.as_str()).unwrap_or("");
        let bad = ["rm -rf /", "rm -rf ~", "sudo", "git push", "ssh ", "| sh", "| bash", "curl", "wget"].iter().any(|p| cmd.contains(p));
        let allow = matches!(req.tool_name.as_str(), "Bash" | "Edit" | "Write" | "MultiEdit") && !bad;
        self.0.lock().unwrap().push(format!("[{e}] {} {} → {} ({why})", req.tool_name, cmd.chars().take(100).collect::<String>().replace('\n', " ⏎ "), if allow { "放行" } else { "拒绝" }));
        if allow { Decision::Allow } else { Decision::Deny("演练中不允许".into()) }
    }
}

const GOAL: &str = "做一个命令行待办清单工具 todo.py：只用 Python 标准库；数据存在 JSON 文件里（默认 ~/.todo.json，可以用环境变量 TODO_FILE 指定，方便测试）。\n\
命令：`add 内容`（可选 `--due YYYY-MM-DD`）、`list`（默认只显示未完成的，`--all` 显示全部；按到期日排序，没到期日的排最后）、`done 编号`、`rm 编号`、`clear-done`（删除所有已完成的）。\n\
行为要求：编号从 1 开始、删除后不复用；对不存在的编号给出清楚的错误信息并以退出码 1 结束；日期格式不对时报错并以退出码 2 结束；`list` 的输出格式固定、便于测试。\n\
质量：每个命令都有 unittest（放在 tests/ 下，用 `python3 -m unittest discover -s tests` 运行，测试里用临时文件，不碰真实的 ~/.todo.json）；README 写明用法和运行测试的方法；.gitignore 忽略 __pycache__。";

#[tokio::test]
#[ignore]
async fn full_autopilot_run_on_a_real_small_project() {
    let dir = PathBuf::from(std::env::var("AUTOPILOT_DIR").unwrap_or_else(|_| "/Users/stephen/Documents/workspace/autopilot-demo/todo-cli".into()));
    assert!(!dir.exists() || std::fs::read_dir(&dir).map(|mut d| d.next().is_none()).unwrap_or(true), "{} 已存在且不为空", dir.display());
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    std::fs::write(dir.join("README.md"), "# todo-cli\n\n（等待负责人和团队完善）\n").unwrap();
    std::fs::write(dir.join(".gitignore"), "__pycache__/\n*.pyc\n").unwrap();
    std::fs::write(dir.join("tests/test_smoke.py"), "import unittest\n\nclass Smoke(unittest.TestCase):\n    def test_ok(self):\n        self.assertTrue(True)\n\nif __name__ == '__main__':\n    unittest.main()\n").unwrap();
    let dir = dir.canonicalize().unwrap();
    let data = std::env::var("CREWFORGE_HOME").map(PathBuf::from).unwrap_or_else(|_| crewforge_core::db::default_data_dir());
    let db = Arc::new(Db::open(&data).unwrap());
    let ws = Arc::new(WorkspaceManager::new(&data));
    ws.init_repo(&dir).await.unwrap();
    for a in [vec!["add", "-A"], vec!["-c", "user.name=Crewforge", "-c", "user.email=crewforge@local", "commit", "-q", "-m", "初始提交"]] {
        assert!(std::process::Command::new("git").arg("-C").arg(&dir).args(a).status().unwrap().success());
    }
    let repo = ws.repo_info(&dir).await.unwrap().root;
    let project = db.create_project("todo-cli（自主推进演练）", "命令行待办清单，用来完整演练自主推进", repo.to_str().unwrap()).unwrap();
    db.set_project_goal(&project.id, GOAL).unwrap();
    let mk = |n: &str| db.employee_by_name(n).unwrap().unwrap_or_else(|| db.add_employee(n, "全栈工程师", &format!("你是 {n}。")).unwrap());
    let (alice, bob, carol) = (mk("alice"), mk("bob"), mk("carol"));
    db.add_member(&project.id, &alice.id, "lead").unwrap();
    db.add_member(&project.id, &bob.id, "developer").unwrap();
    db.add_member(&project.id, &carol.id, "qa").unwrap();
    let mut orch = Orchestrator::new(db.clone(), ws);
    orch.mcp_exe = Some(PathBuf::from(std::env::var("AUTOPILOT_MCP_EXE").unwrap_or_else(|_| env!("CARGO_BIN_EXE_crewforge").into())));
    let orch = Arc::new(orch);
    let t0 = std::time::Instant::now();

    // 1. 基线：初始提交上的 unittest
    let cmd = BaselineCommand { id: "ut".into(), label: "unittest".into(), kind: "test".into(), command: "python3 -m unittest discover -s tests".into(), reason: String::new(), default_on: true, candidate: None };
    orch.run_baseline(&project.id, vec![cmd], Duration::from_secs(120)).await.unwrap();
    println!("[{:>4}s] 项目 {} 已建立，基线完成", t0.elapsed().as_secs(), repo.display());

    // 2. 负责人起草计划
    let approver = Arc::new(Logging(Mutex::new(vec![])));
    let n = orch.lead_draft_plan(&project.id, &alice, &*approver).await.unwrap();
    println!("[{:>4}s] 负责人起草了 {n} 个条目：", t0.elapsed().as_secs());
    let v = db.plan_view(&project.id).unwrap();
    for (i, it) in v.items.iter().enumerate() {
        println!("   {i}. {} → {}  依赖 {:?}", it.title, it.assignee_name.clone().unwrap_or_default(), it.deps.iter().filter_map(|d| v.items.iter().position(|x| &x.id == d)).collect::<Vec<_>>());
    }

    // 3. 开始自主推进（预算有限）
    let calls: i64 = std::env::var("AUTOPILOT_CALLS").ok().and_then(|x| x.parse().ok()).unwrap_or(40);
    let minutes: i64 = std::env::var("AUTOPILOT_MINUTES").ok().and_then(|x| x.parse().ok()).unwrap_or(45);
    let run = Autopilot::<Logging>::prepare(&orch, &project.id, calls, minutes).unwrap();
    let sched = Scheduler::new(db.clone(), 2);
    let ap = Autopilot::new(orch.clone(), sched, approver.clone(), Arc::new(AutopilotCtl::default()), &run).unwrap();
    let progress_db = db.clone();
    let pid = project.id.clone();
    let printer = tokio::spawn(async move {
        let mut seen = 0i64;
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let floor = seen;
            for e in progress_db.recent_events(4000).unwrap_or_default().into_iter().filter(|e| e.id > floor && e.project_id.as_deref() == Some(pid.as_str()) && e.kind.starts_with("autopilot_")) {
                println!("   · [{}] {}", e.kind.trim_start_matches("autopilot_"), e.payload["text"].as_str().unwrap_or(""));
                seen = seen.max(e.id);
            }
        }
    });
    let r = ap.run().await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    printer.abort();
    println!("\n[{:>4}s] 结果：{} · {} · {} 次调用", t0.elapsed().as_secs(), r.status, r.reason, r.calls);

    // 4. 验收
    let v = db.plan_view(&project.id).unwrap();
    println!("计划：完成 {}/{}，阻塞 {}", v.counts.done, v.items.iter().filter(|i| i.status != "cancelled").count(), v.counts.blocked);
    let sh = |args: &[&str]| String::from_utf8_lossy(&std::process::Command::new("git").arg("-C").arg(&repo).args(args).output().unwrap().stdout).to_string();
    println!("主分支文件：{}", sh(&["ls-tree", "-r", "--name-only", "HEAD"]).replace('\n', " "));
    println!("提交：\n{}", sh(&["log", "--format=  %an | %s", "-n", "30"]));
    let test = std::process::Command::new("python3").args(["-m", "unittest", "discover", "-s", "tests"]).current_dir(&repo).output().unwrap();
    println!("项目自己的测试：{}", String::from_utf8_lossy(&test.stderr).lines().last().unwrap_or(""));
    let tf = std::env::temp_dir().join(format!("todo-{}.json", std::process::id()));
    let todo = |args: &[&str]| { let o = std::process::Command::new("python3").arg("todo.py").args(args).env("TODO_FILE", &tf).current_dir(&repo).output().unwrap(); (o.status.code(), String::from_utf8_lossy(&o.stdout).trim().to_string(), String::from_utf8_lossy(&o.stderr).trim().to_string()) };
    println!("烟测 add：{:?}", todo(&["add", "买牛奶", "--due", "2026-12-01"]));
    println!("烟测 add：{:?}", todo(&["add", "写报告"]));
    println!("烟测 list：{:?}", todo(&["list"]));
    println!("烟测 done 1：{:?}", todo(&["done", "1"]));
    println!("烟测 list：{:?}", todo(&["list"]));
    println!("烟测 list --all：{:?}", todo(&["list", "--all"]));
    println!("烟测 rm 99：{:?}", todo(&["rm", "99"]));
    println!("烟测 坏日期：{:?}", todo(&["add", "x", "--due", "2026/13/45"]));
    let _ = std::fs::remove_file(&tf);
    let cost = db.cost_report(None, 0).unwrap();
    let mine: Vec<_> = cost.by_project.iter().filter(|g| g.label.contains("todo-cli")).collect();
    for g in mine { println!("本项目：{} 次调用，估算 ${:.2}，上下文 {} token", g.totals.turns, g.totals.cost_usd, g.totals.context_tokens()); }
    println!("员工请求过 {} 次审批：", approver.0.lock().unwrap().len());
    for a in approver.0.lock().unwrap().iter().take(12) { println!("   {a}"); }
    for m in db.list_memories(&Default::default()).unwrap().iter().filter(|m| m.project_id.as_deref() == Some(project.id.as_str())) { println!("记忆[{}｜{}] {}", m.status, m.source_kind, m.title); }
    for i in db.list_issues(&project.id).unwrap() { println!("议题《{}》{}（{}）", i.title, if i.raised_by.is_some() { "AI 提议" } else { "" }, i.status); }
    // 预算有限时以“预算”结束也是正常结果（收尾已照做）；不能是失败、升级或停止
    assert!(matches!(r.status.as_str(), "finished" | "budget"), "{r:?}");
}
