//! 真实 Claude 的自主推进冒烟（默认忽略；预算很小：最多 12 次调用）。
//! 一个极小的 Python 项目：两个有依赖的计划条目，负责人 alice 主持评审，bob 开发，carol 评审。
use crewforge_core::autopilot::{Autopilot, AutopilotCtl};
use crewforge_core::db::Db;
use crewforge_core::onboarding::scan::BaselineCommand;
use crewforge_core::orchestrator::Orchestrator;
use crewforge_core::plan::NewPlanItem;
use crewforge_core::provider::claude::{ApprovalRequest, Decision};
use crewforge_core::runner::Approver;
use crewforge_core::scheduler::Scheduler;
use crewforge_core::workspace::WorkspaceManager;
use std::sync::Arc;
use std::time::Duration;

struct Allow;
impl Approver for Allow {
    async fn ask(&self, e: &str, req: &ApprovalRequest, why: &str) -> Decision {
        println!("[审批] {e} {} {why}", req.tool_name);
        if matches!(req.tool_name.as_str(), "Bash" | "Edit" | "Write" | "MultiEdit") { Decision::Allow } else { Decision::Deny("不允许".into()) }
    }
}

#[tokio::test]
#[ignore]
async fn lead_loop_with_real_claude_on_a_tiny_project() {
    let d = tempfile::tempdir().unwrap();
    let repo_dir = d.path().join("greeter");
    std::fs::create_dir_all(repo_dir.join("tests")).unwrap();
    std::fs::write(repo_dir.join("README.md"), "greeter\n").unwrap();
    std::fs::write(repo_dir.join("tests/test_smoke.py"), "import unittest\n\nclass T(unittest.TestCase):\n    def test_ok(self):\n        self.assertTrue(True)\n\nif __name__ == '__main__':\n    unittest.main()\n").unwrap();
    std::fs::write(repo_dir.join(".gitignore"), "__pycache__/\n*.pyc\n").unwrap();
    let db = Arc::new(Db::open(&d.path().join("home")).unwrap());
    let ws = Arc::new(WorkspaceManager::new(&d.path().join("home")));
    ws.init_repo(&repo_dir).await.unwrap();
    for a in [vec!["add", "-A"], vec!["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "seed"]] {
        assert!(std::process::Command::new("git").arg("-C").arg(&repo_dir).args(a).status().unwrap().success());
    }
    let repo = ws.repo_info(&repo_dir).await.unwrap().root;
    let project = db.create_project("Greeter", "极小的问候库", repo.to_str().unwrap()).unwrap();
    db.set_project_goal(&project.id, "用 Python 做一个很小的问候库 greet.py，只用标准库，每个函数都要有 unittest 测试（放在 tests/ 下，用 `python3 -m unittest discover -s tests` 运行）。").unwrap();
    let alice = db.add_employee("alice", "全栈工程师", "你是 Alice，项目负责人，简洁务实。").unwrap();
    let bob = db.add_employee("bob", "全栈工程师", "你是 Bob，全栈工程师。小步提交，先写测试。").unwrap();
    let carol = db.add_employee("carol", "QA 测试工程师", "你是 Carol，AI 质检员，评审要具体、简短。").unwrap();
    db.add_member(&project.id, &alice.id, "lead").unwrap();
    db.add_member(&project.id, &bob.id, "developer").unwrap();
    db.add_member(&project.id, &carol.id, "qa").unwrap();
    let mut orch = Orchestrator::new(db.clone(), ws);
    orch.mcp_exe = Some(env!("CARGO_BIN_EXE_crewforge").into());
    let orch = Arc::new(orch);
    // 基线：初始提交上的 unittest
    let cmd = BaselineCommand { id: "ut".into(), label: "unittest".into(), kind: "test".into(), command: "python3 -m unittest discover -s tests".into(), reason: String::new(), default_on: true, candidate: None };
    orch.run_baseline(&project.id, vec![cmd], Duration::from_secs(120)).await.unwrap();
    let item = |t: &str, desc: &str, deps: Vec<String>| db.create_plan_item(&project.id, &NewPlanItem { title: t.into(), description: desc.into(), acceptance: "python3 -m unittest discover -s tests 通过".into(), kind: "migrate".into(), module: String::new(), assignee_id: Some(bob.id.clone()), deps }, None).unwrap();
    let a = item("实现 greet", "新增 greet.py，函数 greet(name) 返回 \"Hello, <name>!\"；空名字返回 \"Hello, stranger!\"。在 tests/test_greet.py 里写测试。", vec![]);
    item("实现 farewell", "在 greet.py 里增加 farewell(name) 返回 \"Goodbye, <name>!\"（空名字同样用 stranger），并补测试。", vec![a]);

    let sched = Scheduler::new(db.clone(), 2);
    let run = Autopilot::<Allow>::prepare(&orch, &project.id, 12, 20).unwrap();
    let r = Autopilot::new(orch.clone(), sched, Arc::new(Allow), Arc::new(AutopilotCtl::default()), &run).unwrap().run().await;
    println!("\n结果：{} · {} · {} 次调用", r.status, r.reason, r.calls);
    for e in db.recent_events(2000).unwrap().into_iter().filter(|e| e.kind.starts_with("autopilot_")) {
        println!("  [{}] {}", e.kind, e.payload["text"].as_str().unwrap_or(""));
    }
    let cost = db.cost_report(None, 0).unwrap();
    println!("费用 ${:.2}，{} 次模型调用", cost.totals.cost_usd, cost.totals.turns);
    println!("主分支文件：{}", String::from_utf8_lossy(&std::process::Command::new("git").arg("-C").arg(&repo).args(["ls-tree", "-r", "--name-only", "HEAD"]).output().unwrap().stdout).replace('\n', " "));
    assert_eq!(r.status, "finished", "{r:?}");
    assert!(repo.join("greet.py").exists());
}
