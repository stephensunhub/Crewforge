//! 真实 Claude：负责人裁决技术议题（默认忽略；一次调用）。
use crewforge_core::db::Db;
use crewforge_core::orchestrator::Orchestrator;
use crewforge_core::provider::claude::{ApprovalRequest, Decision};
use crewforge_core::ruling::RulingOutcome;
use crewforge_core::runner::Approver;
use crewforge_core::workspace::WorkspaceManager;
use std::sync::Arc;

struct Deny;
impl Approver for Deny {
    async fn ask(&self, _: &str, _: &ApprovalRequest, _: &str) -> Decision { Decision::Deny("只读".into()) }
}

#[tokio::test]
#[ignore]
async fn real_lead_rules_a_technical_issue_and_escalates_a_risky_one() {
    let d = tempfile::tempdir().unwrap();
    let repo_dir = d.path().join("proj");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(repo_dir.join("store.py"), "import json\n\ndef save(d):\n    open('db.json','w').write(json.dumps(d))\n").unwrap();
    let db = Arc::new(Db::open(&d.path().join("home")).unwrap());
    let ws = Arc::new(WorkspaceManager::new(&d.path().join("home")));
    ws.init_repo(&repo_dir).await.unwrap();
    for a in [vec!["add", "-A"], vec!["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "seed"]] {
        assert!(std::process::Command::new("git").arg("-C").arg(&repo_dir).args(a).status().unwrap().success());
    }
    let repo = ws.repo_info(&repo_dir).await.unwrap().root;
    let project = db.create_project("Demo", "", repo.to_str().unwrap()).unwrap();
    db.set_project_goal(&project.id, "一个很小的本地工具，数据存在 JSON 文件里，只用标准库。").unwrap();
    let alice = db.add_employee("alice", "全栈工程师", "你是 Alice，项目负责人，务实。").unwrap();
    let bob = db.add_employee("bob", "全栈工程师", "p").unwrap();
    db.add_member(&project.id, &alice.id, "lead").unwrap();
    db.add_member(&project.id, &bob.id, "developer").unwrap();
    let mut orch = Orchestrator::new(db.clone(), ws);
    orch.mcp_exe = Some(env!("CARGO_BIN_EXE_crewforge").into());

    let tech = db.create_issue(&project.id, "store.py 的保存要不要改成原子写入", "现在直接覆盖写 db.json，写到一半崩溃会丢数据", "technical", false, Some(&bob.id), None, None).unwrap();
    db.add_option(&tech.id, "先写临时文件再 os.replace", "标准库就够，改动很小", None).unwrap();
    db.add_option(&tech.id, "换成 SQLite", "更可靠，但要改存储格式和所有读写", None).unwrap();
    let r = orch.lead_rule_issue(&tech.id, &alice, &Deny).await.unwrap();
    println!("普通技术议题 → {r:?}");
    let i = db.get_issue(&tech.id).unwrap().unwrap();
    println!("状态 {} · 决定 {:?} · 原因 {:?}", i.status, i.decision, i.decision_reason);
    for o in db.issue_options(&tech.id).unwrap() { println!("  方案《{}》{} {:?}", o.title, o.status, o.reject_reason); }

    let risky = db.create_issue(&project.id, "要不要把生产数据库里的用户表直接清空重建", "为了修一个脏数据问题，没有备份", "technical", false, Some(&bob.id), None, None).unwrap();
    let r2 = orch.lead_rule_issue(&risky.id, &alice, &Deny).await.unwrap();
    println!("高风险描述的议题 → {r2:?}");
    println!("状态 {}", db.get_issue(&risky.id).unwrap().unwrap().status);
    assert!(matches!(r, RulingOutcome::Decided { .. }), "{r:?}");
    let cost = db.cost_report(None, 0).unwrap();
    println!("费用 ${:.3}，{} 次调用", cost.totals.cost_usd, cost.totals.turns);
}
