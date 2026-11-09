//! 真实 Claude：让某个项目的负责人处理“标给人”的行动项（默认忽略）。
//! 环境变量：LEAD_SET（指定负责人）、LEAD_PROJECT（项目名，默认“贪吃蛇”）、CREWFORGE_HOME（数据目录，默认真实数据目录）。会真的改动数据。
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
async fn lead_handles_human_flagged_actions_with_real_claude() {
    let data = std::env::var("CREWFORGE_HOME").map(PathBuf::from).unwrap_or_else(|_| crewforge_core::db::default_data_dir());
    let db = Arc::new(Db::open(&data).unwrap());
    let name = std::env::var("LEAD_PROJECT").unwrap_or_else(|_| "贪吃蛇".into());
    let project = db.list_projects().unwrap().into_iter().find(|p| p.name == name).expect("找不到项目");
    // 项目还没有 AI 负责人时，可以用 LEAD_SET 指定一位员工担任（会真的改项目角色）
    if let Ok(who) = std::env::var("LEAD_SET") {
        let e = db.employee_by_name(&who).unwrap().expect("员工不存在");
        db.set_member_role(&project.id, &e.id, "lead").unwrap();
    }
    let lead_id = db.list_members(&project.id).unwrap().into_iter().find(|m| m.role == "lead").expect("项目没有负责人").employee_id;
    let lead = db.employee_by_id(&lead_id).unwrap().unwrap();
    let ws = Arc::new(WorkspaceManager::new(&data));
    let orch = Orchestrator::new(db.clone(), ws);
    let ids = orch.human_actions_for_lead(&project.id).unwrap();
    println!("待处理的标给人的行动项：{} 条，负责人 {}", ids.len(), lead.name);
    for id in ids {
        let item = db.get_action(&id).unwrap().unwrap();
        println!("\n▶ {}", item.text);
        let out = orch.lead_handle_action(&id, &lead, &Deny).await.unwrap();
        println!("  结果：{out:?}");
        let after = db.get_action(&id).unwrap().unwrap();
        println!("  状态：{}；备注：{}", after.status, after.note);
    }
    for it in db.plan_view(&project.id).unwrap().items.iter().filter(|i| i.note.contains("负责人处理行动项追加")) {
        println!("追加的计划条目：{}（{}）→ {} 文件 {:?}", it.title, it.status, it.assignee_name.clone().unwrap_or_default(), db.plan_files(&project.id).unwrap().get(&it.id));
    }
}
