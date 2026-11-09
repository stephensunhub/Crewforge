//! 负责人处理“标给人”的行动项：能定的定（并追加计划条目、沉淀决策记忆）、不必做的驳回、必须是人的才留下。

use crewforge_core::db::Db;
use crewforge_core::orchestrator::Orchestrator;
use crewforge_core::lead_actions::ActionOutcome;
use crewforge_core::provider::claude::{ApprovalRequest, Decision};
use crewforge_core::runner::Approver;
use crewforge_core::workspace::WorkspaceManager;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

struct Deny;
impl Approver for Deny {
    async fn ask(&self, _: &str, _: &ApprovalRequest, _: &str) -> Decision {
        Decision::Deny("t".into())
    }
}

/// 假 CLI：按提示里出现的关键字回不同的 JSON；都不匹配就回一段不是 JSON 的话。
fn fake_cli(dir: &Path) -> String {
    let reply = |s: &str| format!("echo '{}'", serde_json::json!({"type":"result","subtype":"success","is_error":false,"result":s,"total_cost_usd":0.01,"usage":{"input_tokens":1,"output_tokens":1}}));
    let decide = reply(r#"{"action":"decide","decision":"回车后直接开始新一局","reason":"需求原文写了结束后按回车重新开始","follow_up":[{"title":"回车直接开始","description":"restart 后直接 start","acceptance":"cargo test","assignee":"bob","files":["src/game.rs"]},{"title":"同步 README 按键表","description":"d","acceptance":"a","assignee":"不存在的人","files":[]}]}"#);
    let dismiss = reply(r#"{"action":"dismiss","reason":"现状已经满足需求，不必再改"}"#);
    let escalate = reply(r#"{"action":"escalate","reason":"要不要换成多人联机是业务方向，必须由人定"}"#);
    let junk = reply("我觉得都行");
    let p = dir.join("fake");
    std::fs::write(&p, format!(
        "#!/bin/sh\nread -r line\necho '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"x\"}}'\n\
         case \"$line\" in\n*行动项：决定回车重开*) {decide} ;;\n*行动项：决定界面配色*) {dismiss} ;;\n*行动项：决定是否改成多人联机*) {escalate} ;;\n*) {junk} ;;\nesac\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p.to_string_lossy().into_owned()
}

#[tokio::test]
async fn lead_decides_dismisses_or_escalates_human_flagged_actions() {
    let d = tempfile::tempdir().unwrap();
    let data = d.path().join("home");
    let proj = d.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let db = Arc::new(Db::open(&data).unwrap());
    let ws = Arc::new(WorkspaceManager::new(&data));
    ws.init_repo(&proj).await.unwrap();
    let repo = ws.repo_info(&proj).await.unwrap().root;
    let project = db.create_project("Snake", "", repo.to_str().unwrap()).unwrap();
    db.set_project_goal(&project.id, "结束后按回车重新开始").unwrap();
    let alice = db.add_employee("alice", "全栈", "p").unwrap();
    let bob = db.add_employee("bob", "全栈", "p").unwrap();
    db.add_member(&project.id, &alice.id, "lead").unwrap();
    db.add_member(&project.id, &bob.id, "developer").unwrap();
    let mut orch = Orchestrator::new(db.clone(), ws);
    orch.program = fake_cli(d.path());

    let m = db.create_meeting(&project.id, "retro", "复盘", None, None, &[], 5).unwrap();
    db.set_minutes(&m.id, "## 行动项\n- [后续] 人类：决定回车重开后是否仍需再按方向键\n- [后续] 人类：决定界面配色是否换深色\n- [后续] 人类：决定是否改成多人联机\n- [后续] 人类：别的什么事").unwrap();
    let ids = orch.human_actions_for_lead(&project.id).unwrap();
    assert_eq!(ids.len(), 4);
    let find = |t: &str| db.open_actions_for_project(&project.id).unwrap().into_iter().find(|a| a.text.contains(t)).unwrap().id;

    // 1. 能定的：决定 + 追加计划条目（带文件）+ 决策记忆；负责人不是成员的条目被拒绝并写明
    let id = find("回车重开");
    let out = orch.lead_handle_action(&id, &alice, &Deny).await.unwrap().unwrap();
    assert!(matches!(&out, ActionOutcome::Decided { follow_ups, .. } if follow_ups == &vec!["回车直接开始".to_string(), "同步 README 按键表".to_string()]), "{out:?}");
    let a = db.get_action(&id).unwrap().unwrap();
    assert_eq!(a.status, "done");
    assert!(a.note.contains("决定") && a.note.contains("需求原文") && a.note.contains("回车直接开始"), "{}", a.note);
    let plan = db.plan_view(&project.id).unwrap();
    let item = plan.items.iter().find(|i| i.title == "回车直接开始").unwrap();
    assert_eq!((item.status.as_str(), item.assignee_name.as_deref()), ("planned", Some("bob")), "直接是待开始：负责人的决定不用再等人批准");
    assert_eq!(db.plan_files(&project.id).unwrap().get(&item.id), Some(&vec!["src/game.rs".to_string()]));
    assert!(plan.items.iter().any(|i| i.title == "同步 README 按键表" && i.assignee_name.is_none()), "不存在的负责人 → 不指定，而不是整条丢掉");
    let mem = db.list_memories(&Default::default()).unwrap();
    assert!(mem.iter().any(|m| m.kind == "decision" && m.status == "verified" && m.content.contains("回车后直接开始")), "沉淀为已验证的决策记忆");

    // 2. 不必做的：驳回并写明原因
    let id = find("界面配色");
    let out = orch.lead_handle_action(&id, &alice, &Deny).await.unwrap().unwrap();
    assert!(matches!(out, ActionOutcome::Dismissed { .. }));
    let a = db.get_action(&id).unwrap().unwrap();
    assert!(a.status == "dismissed" && a.note.contains("不必"), "{a:?}");

    // 3. 必须是人的：留着，写明为什么，并且不会再被反复问
    let id = find("多人联机");
    let out = orch.lead_handle_action(&id, &alice, &Deny).await.unwrap().unwrap();
    assert!(matches!(out, ActionOutcome::Escalated { .. }));
    let a = db.get_action(&id).unwrap().unwrap();
    assert!(a.status == "open" && a.for_human && a.note.contains("必须由你决定") && a.note.contains("业务方向"), "{a:?}");
    assert!(orch.action_was_escalated(&id));
    // 还没处理的只剩那条“别的什么事”（回复不是 JSON 的）；多人联机已看过，不再列入
    let rest = orch.human_actions_for_lead(&project.id).unwrap();
    assert_eq!(rest.len(), 1);

    // 4. 回复格式不对：不丢，交给人，并写明
    let out = orch.lead_handle_action(&rest[0], &alice, &Deny).await.unwrap().unwrap();
    assert!(matches!(&out, ActionOutcome::Escalated { reason } if reason.contains("格式不对")), "{out:?}");
    assert!(orch.human_actions_for_lead(&project.id).unwrap().is_empty());
    // 已经处理过的再处理：什么都不做
    assert!(orch.lead_handle_action(&find("多人联机"), &alice, &Deny).await.unwrap().is_some(), "仍是待处理的会再问一次（由调用方用 human_actions_for_lead 去重）");
    let done = db.open_actions_for_project(&project.id).unwrap().iter().filter(|a| a.for_human).count();
    assert_eq!(done, 2, "留给人的两条");
}
