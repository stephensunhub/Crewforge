//! 真实 Claude：让负责人整理一个项目里积累的“之后再说”行动项（默认忽略）。会真的改动数据。
//! 环境变量：LEAD_PATH（项目目录的结尾部分，如 todo-cli-4）、CREWFORGE_HOME（默认真实数据目录）。
use crewforge_core::db::Db;
use crewforge_core::orchestrator::Orchestrator;
use crewforge_core::provider::claude::{ApprovalRequest, Decision};
use crewforge_core::runner::Approver;
use crewforge_core::workspace::WorkspaceManager;
use std::path::PathBuf;
use std::sync::Arc;

struct Deny;
impl Approver for Deny {
    async fn ask(&self, _: &str, _: &ApprovalRequest, _: &str) -> Decision {
        Decision::Deny("只读".into())
    }
}

#[tokio::test]
#[ignore]
async fn lead_triages_later_actions_with_real_claude() {
    let data = std::env::var("CREWFORGE_HOME").map(PathBuf::from).unwrap_or_else(|_| crewforge_core::db::default_data_dir());
    let db = Arc::new(Db::open(&data).unwrap());
    let tail = std::env::var("LEAD_PATH").expect("设置 LEAD_PATH");
    let project = db.list_projects().unwrap().into_iter().find(|p| p.path.ends_with(&tail)).expect("找不到项目");
    let lead_id = db.list_members(&project.id).unwrap().into_iter().find(|m| m.role == "lead").expect("项目没有负责人").employee_id;
    let lead = db.employee_by_id(&lead_id).unwrap().unwrap();
    let orch = Orchestrator::new(db.clone(), Arc::new(WorkspaceManager::new(&data)));
    let before = orch.later_actions(&project.id).unwrap();
    println!("整理前：{} 条“之后再说”", before.len());
    let out = orch.lead_triage_later(&project.id, &lead, &Deny).await.unwrap();
    println!("结果：{out:?}");
    println!("整理后：{} 条", orch.later_actions(&project.id).unwrap().len());
    for a in db.open_actions_for_project(&project.id).unwrap().iter().take(0) { println!("{}", a.text); }
    let plan = db.plan_view(&project.id).unwrap();
    for it in plan.items.iter().filter(|i| i.note.contains("负责人整理行动项追加")) {
        println!("转成的计划条目：{}（{}）→ {} 文件 {:?}", it.title, it.status, it.assignee_name.clone().unwrap_or_default(), db.plan_files(&project.id).unwrap().get(&it.id));
    }
    // 被驳回的原因抽样
    let conn_notes: Vec<String> = before.iter().filter_map(|a| db.get_action(&a.id).ok().flatten()).filter(|a| a.status == "dismissed").take(6).map(|a| format!("  ✗ {} —— {}", a.text.chars().take(60).collect::<String>(), a.note.chars().take(100).collect::<String>())).collect();
    println!("驳回抽样：\n{}", conn_notes.join("\n"));
}
