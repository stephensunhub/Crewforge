//! 负责人整理“之后再说”的行动项：驳回、转计划、保留；编号不对或重复的指令被拒绝；数量太少不调用模型。

use crewforge_core::db::Db;
use crewforge_core::orchestrator::Orchestrator;
use crewforge_core::provider::claude::{ApprovalRequest, Decision};
use crewforge_core::runner::Approver;
use crewforge_core::workspace::WorkspaceManager;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

struct Deny;
impl Approver for Deny {
    async fn ask(&self, _: &str, _: &ApprovalRequest, _: &str) -> Decision {
        Decision::Deny("t".into())
    }
}

#[tokio::test]
async fn lead_dismisses_plans_and_keeps_later_actions_within_limits() {
    let d = tempfile::tempdir().unwrap();
    let data = d.path().join("home");
    let proj = d.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let db = Arc::new(Db::open(&data).unwrap());
    let ws = Arc::new(WorkspaceManager::new(&data));
    ws.init_repo(&proj).await.unwrap();
    let repo = ws.repo_info(&proj).await.unwrap().root;
    let project = db.create_project("Todo", "", repo.to_str().unwrap()).unwrap();
    let alice = db.add_employee("alice", "全栈", "p").unwrap();
    let bob = db.add_employee("bob", "全栈", "p").unwrap();
    db.add_member(&project.id, &alice.id, "lead").unwrap();
    db.add_member(&project.id, &bob.id, "developer").unwrap();
    let mut orch = Orchestrator::new(db.clone(), ws);
    let log = d.path().join("prompts.log");
    let reply = serde_json::json!({"type":"result","subtype":"success","is_error":false,"result":
        r#"{"items":[{"n":1,"op":"dismiss","reason":"和第 2 条重复"},{"n":2,"op":"plan","title":"补充边界测试","description":"d","acceptance":"python3 -m unittest","assignee":"bob","files":["tests/test_edge.py"]},{"n":3,"op":"keep"},{"n":4,"op":"dismiss","reason":"已被现有计划覆盖"},{"n":99,"op":"dismiss","reason":"编号不存在"},{"n":1,"op":"dismiss","reason":"重复出现"}]}"#,
        "total_cost_usd":0.01,"usage":{"input_tokens":1,"output_tokens":1}});
    let p = d.path().join("fake");
    std::fs::write(&p, format!("#!/bin/sh\nread -r line\nprintf '%s\\n' \"$line\" >> '{}'\necho '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"x\"}}'\necho '{}'\n", log.display(), reply)).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    orch.program = p.to_string_lossy().into_owned();

    // 不够 5 条：不调用模型
    let m = db.create_meeting(&project.id, "retro", "复盘", None, None, &[], 5).unwrap();
    db.set_minutes(&m.id, "## 行动项\n- [之后] alice：补 A 的测试\n- [之后] bob：补 B 的测试").unwrap();
    assert!(orch.lead_triage_later(&project.id, &alice, &Deny).await.unwrap().is_none());
    assert!(!log.exists(), "数量太少时没有调用模型");

    // 6 条（再加一条给人的和一条拦合并的，它们不在整理范围里）
    db.set_minutes(&m.id, "## 行动项\n- [之后] alice：补 A 的测试\n- [之后] bob：补 B 的测试\n- [之后] alice：补 A 的边界测试\n- [之后] bob：重命名变量\n- [之后] alice：补文档\n- [之后] bob：补日志\n- [后续] 人类：这条是给人的").unwrap();
    assert_eq!(orch.later_actions(&project.id).unwrap().len(), 6, "给人的不算");
    // 已经裁决的议题要告诉负责人：和它重复的行动项直接驳回
    let issue = db.create_issue(&project.id, "创建时 status 缺省语义", "", "technical", false, Some(&bob.id), None, None).unwrap();
    db.set_issue_status(&issue.id, "discussing").unwrap();
    db.set_issue_status(&issue.id, "pending_decision").unwrap();
    db.decide_issue(&issue.id, &crewforge_core::board::Actor::Employee(alice.id.clone()), None, "缺省为 todo", "更简单", &Default::default()).unwrap();
    let out = orch.lead_triage_later(&project.id, &alice, &Deny).await.unwrap().unwrap();
    let prompt = std::fs::read_to_string(&log).unwrap();
    assert!(prompt.contains("已经裁决的议题") && prompt.contains("创建时 status 缺省语义") && prompt.contains("缺省为 todo") && prompt.contains("事实已经变化而过期"), "{prompt}");
    assert_eq!((out.dismissed, out.planned.len(), out.kept), (2, 1, 1), "{out:?}");
    assert!(out.rejected.iter().any(|r| r.contains("99")) && out.rejected.iter().any(|r| r.contains("重复出现")), "{:?}", out.rejected);
    let all = db.actions_for_meeting(&m.id).unwrap();
    let by = |t: &str| all.iter().find(|a| a.text.contains(t)).unwrap();
    assert!(by("补 A 的测试").status == "dismissed" && by("补 A 的测试").note.contains("和第 2 条重复"));
    assert!(by("补 B 的测试").status == "done" && by("补 B 的测试").note.contains("补充边界测试"));
    assert_eq!(by("补 A 的边界测试").status, "open", "保留");
    assert_eq!(by("重命名变量").status, "dismissed");
    assert_eq!(by("补文档").status, "open", "没提到的原样保留");
    assert!(by("这条是给人的").for_human && by("这条是给人的").status == "open", "给人的不动");
    let plan = db.plan_view(&project.id).unwrap();
    let it = plan.items.iter().find(|i| i.title == "补充边界测试").unwrap();
    assert_eq!((it.status.as_str(), it.assignee_name.as_deref()), ("planned", Some("bob")));
    assert_eq!(db.plan_files(&project.id).unwrap().get(&it.id), Some(&vec!["tests/test_edge.py".to_string()]));
    assert!(db.recent_events(100).unwrap().iter().any(|e| e.kind == "lead_triaged_actions"));
}

#[test]
fn the_plan_cap_grows_with_the_backlog_but_stays_between_3_and_8() {
    use crewforge_core::lead_triage::max_plan_items;
    assert_eq!((max_plan_items(5), max_plan_items(9), max_plan_items(15), max_plan_items(60)), (3, 3, 5, 8));
}
