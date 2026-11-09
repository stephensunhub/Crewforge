//! 真实 Claude：负责人起草计划，再在条目失败后调整计划（默认忽略；2 次调用）。
use crewforge_core::db::Db;
use crewforge_core::orchestrator::Orchestrator;
use crewforge_core::planning::AdjustTrigger;
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
async fn real_lead_drafts_and_adjusts_a_plan() {
    let d = tempfile::tempdir().unwrap();
    let repo_dir = d.path().join("ledger-lite");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(repo_dir.join("ledger.py"), "import csv, sys\n\ndef summary(path):\n    rows = list(csv.DictReader(open(path)))\n    return sum(float(r['amount']) for r in rows)\n\nif __name__ == '__main__':\n    print(summary(sys.argv[1]))\n").unwrap();
    let db = Arc::new(Db::open(&d.path().join("home")).unwrap());
    let ws = Arc::new(WorkspaceManager::new(&d.path().join("home")));
    ws.init_repo(&repo_dir).await.unwrap();
    for a in [vec!["add", "-A"], vec!["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "seed"]] {
        assert!(std::process::Command::new("git").arg("-C").arg(&repo_dir).args(a).status().unwrap().success());
    }
    let repo = ws.repo_info(&repo_dir).await.unwrap().root;
    let project = db.create_project("ledger-lite", "", repo.to_str().unwrap()).unwrap();
    db.set_project_goal(&project.id, "把 ledger.py（Python）迁移到 Rust，技术栈升级、业务行为不变：对任何输入，输出必须逐字一致；新实现放在 rust/ 目录，Python 版本保留；不引入第三方依赖。").unwrap();
    let alice = db.add_employee("alice", "全栈工程师", "你是 Alice，项目负责人，务实。").unwrap();
    let bob = db.add_employee("bob", "全栈工程师", "p").unwrap();
    let carol = db.add_employee("carol", "QA 测试工程师", "p").unwrap();
    db.add_member(&project.id, &alice.id, "lead").unwrap();
    db.add_member(&project.id, &bob.id, "developer").unwrap();
    db.add_member(&project.id, &carol.id, "qa").unwrap();
    let mut orch = Orchestrator::new(db.clone(), ws);
    orch.mcp_exe = Some(env!("CARGO_BIN_EXE_crewforge").into());

    let n = orch.lead_draft_plan(&project.id, &alice, &Deny).await.unwrap();
    println!("负责人起草了 {n} 个条目：");
    let v = db.plan_view(&project.id).unwrap();
    for (i, it) in v.items.iter().enumerate() {
        println!("  {i}. [{}] {} → {}  依赖 {:?}\n       验收：{}", it.status, it.title, it.assignee_name.clone().unwrap_or_default(), it.deps.iter().filter_map(|d| v.items.iter().position(|x| &x.id == d)).collect::<Vec<_>>(), it.acceptance.chars().take(80).collect::<String>());
    }
    // 批准并模拟第一个条目失败 → 让负责人调整
    db.approve_proposed_plan(&project.id).unwrap();
    let first = v.items[0].id.clone();
    db.set_plan_status(&first, "blocked", "验证仍未通过：cargo build 报错，找不到 Cargo.toml").unwrap();
    let r = orch.lead_adjust_plan(&project.id, &alice, AdjustTrigger::ItemFailed, &format!("《{}》：验证仍未通过：cargo build 报错，找不到 Cargo.toml", v.items[0].title), &Deny).await.unwrap();
    println!("\n调整：{} \n  追加 {:?}\n  取消 {:?}\n  改派 {:?}\n  重试 {:?}\n  被拒绝 {:?}\n  升级 {:?}", r.summary, r.added, r.cancelled, r.reassigned, r.retried, r.rejected, r.escalate);
    let cost = db.cost_report(None, 0).unwrap();
    println!("费用 ${:.3}，{} 次调用", cost.totals.cost_usd, cost.totals.turns);
    assert!(n >= 3);
}
