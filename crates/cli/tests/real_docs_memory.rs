//! 真实 Claude：负责人撰写项目概览并更新文档、整理项目记忆（默认忽略；2 次调用）。
use crewforge_core::db::Db;
use crewforge_core::memory::NewMemory;
use crewforge_core::orchestrator::Orchestrator;
use crewforge_core::provider::claude::{ApprovalRequest, Decision};
use crewforge_core::runner::Approver;
use crewforge_core::workspace::WorkspaceManager;
use std::sync::Arc;

struct Deny;
impl Approver for Deny {
    async fn ask(&self, _: &str, _: &ApprovalRequest, _: &str) -> Decision { Decision::Deny("只读".into()) }
}

#[tokio::test]
#[ignore]
async fn real_lead_writes_docs_and_curates_memory() {
    let d = tempfile::tempdir().unwrap();
    let repo_dir = d.path().join("greeter");
    std::fs::create_dir_all(repo_dir.join("tests")).unwrap();
    std::fs::write(repo_dir.join("greet.py"), "def greet(name):\n    name = (name or '').strip()\n    return f\"Hello, {name or 'stranger'}!\"\n").unwrap();
    std::fs::write(repo_dir.join("tests/test_greet.py"), "import unittest, sys, os\nsys.path.insert(0, os.path.join(os.path.dirname(__file__), '..'))\nfrom greet import greet\n\nclass T(unittest.TestCase):\n    def test_basic(self):\n        self.assertEqual(greet('Bob'), 'Hello, Bob!')\n    def test_empty(self):\n        self.assertEqual(greet(''), 'Hello, stranger!')\n").unwrap();
    std::fs::write(repo_dir.join("README.md"), "# greeter\n\n运行测试：`python3 -m unittest discover -s tests`\n").unwrap();
    let db = Arc::new(Db::open(&d.path().join("home")).unwrap());
    let ws = Arc::new(WorkspaceManager::new(&d.path().join("home")));
    ws.init_repo(&repo_dir).await.unwrap();
    for a in [vec!["add", "-A"], vec!["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "seed"]] {
        assert!(std::process::Command::new("git").arg("-C").arg(&repo_dir).args(a).status().unwrap().success());
    }
    let repo = ws.repo_info(&repo_dir).await.unwrap().root;
    let project = db.create_project("greeter", "", repo.to_str().unwrap()).unwrap();
    db.set_project_goal(&project.id, "一个很小的问候库，只用标准库，每个函数都有 unittest。").unwrap();
    let alice = db.add_employee("alice", "全栈工程师", "你是 Alice，项目负责人，务实。").unwrap();
    let bob = db.add_employee("bob", "全栈工程师", "p").unwrap();
    db.add_member(&project.id, &alice.id, "lead").unwrap();
    db.add_member(&project.id, &bob.id, "developer").unwrap();
    let mut orch = Orchestrator::new(db.clone(), ws);
    orch.mcp_exe = Some(env!("CARGO_BIN_EXE_crewforge").into());

    // 文档
    let r = orch.update_project_docs(&project.id, &alice, true, &Deny).await.unwrap();
    println!("文档：{r:?}");
    for f in orch.list_docs(&project.id).await.unwrap() { println!("  {} ({} 字节)", f.path, f.size); }
    println!("---- overview.md ----\n{}\n---------------------", orch.read_doc(&project.id, "docs/crewforge/overview.md").await.unwrap_or_else(|e| format!("（没有：{e}）")));

    // 记忆：一条有依据（README 里确实有）、一条空泛的状态描述、一条会被推翻的、一条人写的
    let mem = |title: &str, content: &str, source: &str, by: &str| db.add_memory(NewMemory { layer: "project".into(), project_id: Some(project.id.clone()), kind: "lesson".into(), title: title.into(), content: content.into(), scope: String::new(), source_kind: source.into(), created_by: by.into(), ..Default::default() }).unwrap();
    let good = mem("运行测试的方法", "用 `python3 -m unittest discover -s tests` 运行全部测试，tests 目录里的测试通过 sys.path 导入上级目录的 greet.py。", "agent", &bob.id);
    let state = mem("目前进度", "现在已经做完了一半，还剩一些事情没做。", "agent", &bob.id);
    let wrong = mem("依赖管理", "项目用 poetry 管理依赖，改依赖要改 pyproject.toml。", "agent", &bob.id);
    let human = mem("人写的约定", "提交信息用中文。", "human", "human");
    let c = orch.lead_curate_memory(&project.id, &alice, &Deny).await.unwrap();
    println!("\n整理：{c:#?}");
    for (n, m) in [("有依据", &good), ("空泛状态", &state), ("与代码矛盾", &wrong), ("人写的", &human)] {
        let m2 = db.get_memory(&m.id).unwrap().unwrap();
        println!("  {n}《{}》→ {} {:?}", m2.title, m2.status, m2.expired_reason);
    }
    let cost = db.cost_report(None, 0).unwrap();
    println!("费用 ${:.3}，{} 次调用", cost.totals.cost_usd, cost.totals.turns);
    assert_eq!(db.get_memory(&human.id).unwrap().unwrap().status, "verified");
}
