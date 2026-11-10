//! 真实 Claude 冒烟：站会（默认忽略）。任务用假 CLI 造出两个改同一文件的进行中任务，发言与纪要用真实 Claude。
use crewforge_core::db::Db;
use crewforge_core::orchestrator::Orchestrator;
use crewforge_core::provider::claude::{ApprovalRequest, Decision};
use crewforge_core::runner::{Approver, NoopObserver};
use crewforge_core::workspace::WorkspaceManager;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

struct Deny;
impl Approver for Deny {
    async fn ask(&self, _: &str, req: &ApprovalRequest, _: &str) -> Decision {
        println!("[审批请求] {}", req.tool_name);
        Decision::Deny("只读".into())
    }
}

#[tokio::test]
#[ignore]
async fn real_standup_flags_the_shared_file() {
    let data = tempfile::tempdir().unwrap();
    let repo_dir = data.path().join("proj");
    std::fs::create_dir_all(&repo_dir).unwrap();
    let db = Arc::new(Db::open(&data.path().join("home")).unwrap());
    let ws = Arc::new(WorkspaceManager::new(&data.path().join("home")));
    ws.init_repo(&repo_dir).await.unwrap();
    std::fs::write(repo_dir.join("README.md"), "demo\n").unwrap();
    for a in [vec!["add", "-A"], vec!["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "seed"]] {
        assert!(std::process::Command::new("git").arg("-C").arg(&repo_dir).args(a).status().unwrap().success());
    }
    let repo = ws.repo_info(&repo_dir).await.unwrap().root;
    let project = db.create_project("Demo", "", repo.to_str().unwrap()).unwrap();
    let alice = db.add_employee("alice", "全栈", "你是 alice，后端工程师。").unwrap();
    let bob = db.add_employee("bob", "全栈", "你是 bob，前端工程师。").unwrap();
    for e in [&alice, &bob] { db.add_member(&project.id, &e.id, "developer").unwrap(); }
    let mut orch = Orchestrator::new(db.clone(), ws);
    orch.mcp_exe = Some(env!("CARGO_BIN_EXE_crewforge").into());

    let fake = data.path().join("fake");
    std::fs::write(&fake, "#!/bin/sh\nread -r l\necho '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"x\"}'\necho x > api.md\nprintf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"ok\",\"usage\":{}}'\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    orch.program = fake.to_string_lossy().into_owned();
    orch.assign(&alice, &repo, "后端：实现用户接口，字段 userId", &Deny, &NoopObserver).await.unwrap();
    orch.assign(&bob, &repo, "前端：调用用户接口并展示，字段 user_id", &Deny, &NoopObserver).await.unwrap();
    orch.program = "claude".into();

    let (m, who) = orch.start_standup(&project.id).await.unwrap();
    for w in &who { let r = orch.meeting_reply(&m.id, w, &Deny).await.unwrap(); println!("{}：{r}\n", w.name); }
    println!("{}\n", orch.standup_alignment_check(&m.id).await.unwrap());
    let minutes = orch.draft_minutes(&m.id, &Deny).await.unwrap();
    println!("纪要：\n{minutes}");
    assert!(minutes.contains("## 行动项") && minutes.contains("api.md"));
    let actions = db.actions_for_meeting(&m.id).unwrap();
    println!("行动项 {} 条：{:?}", actions.len(), actions.iter().map(|a| a.text.clone()).collect::<Vec<_>>());
}

/// 对齐会的目标检查：目标与现实一致时说“仍然适用”；目标与现实不符时，负责人提交目标变更提议（目标不变，等人点头），受托主持人只在纪要里指出。
#[tokio::test]
#[ignore]
async fn real_alignment_goal_check_for_lead_and_proxy_host() {
    let data = tempfile::tempdir().unwrap();
    let repo_dir = data.path().join("proj");
    std::fs::create_dir_all(&repo_dir).unwrap();
    let db = Arc::new(Db::open(&data.path().join("home")).unwrap());
    let ws = Arc::new(WorkspaceManager::new(&data.path().join("home")));
    ws.init_repo(&repo_dir).await.unwrap();
    std::fs::write(repo_dir.join("README.md"), "demo\n").unwrap();
    for a in [vec!["add", "-A"], vec!["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "seed"]] {
        assert!(std::process::Command::new("git").arg("-C").arg(&repo_dir).args(a).status().unwrap().success());
    }
    let repo = ws.repo_info(&repo_dir).await.unwrap().root;
    let project = db.create_project("Demo", "", repo.to_str().unwrap()).unwrap();
    let alice = db.add_employee("alice", "全栈", "你是 alice，后端工程师兼项目负责人。").unwrap();
    let bob = db.add_employee("bob", "全栈", "你是 bob，前端工程师。").unwrap();
    db.add_member(&project.id, &alice.id, "lead").unwrap();
    db.add_member(&project.id, &bob.id, "developer").unwrap();
    let mut orch = Orchestrator::new(db.clone(), ws);
    orch.mcp_exe = Some(env!("CARGO_BIN_EXE_crewforge").into());
    let fake = data.path().join("fake");
    std::fs::write(&fake, "#!/bin/sh\nread -r l\necho '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"x\"}'\necho x > api.md\nprintf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"ok\",\"usage\":{}}'\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    orch.program = fake.to_string_lossy().into_owned();
    orch.assign(&alice, &repo, "后端：实现用户接口", &Deny, &NoopObserver).await.unwrap();
    orch.assign(&bob, &repo, "前端：做用户列表页面，调用用户接口", &Deny, &NoopObserver).await.unwrap();
    orch.program = "claude".into();
    let (m, who) = orch.start_standup_hosted(&project.id, Some(&alice.id)).await.unwrap();
    for w in &who { orch.meeting_reply(&m.id, w, &Deny).await.unwrap(); }
    let _ = orch.standup_alignment_check(&m.id).await.unwrap();

    // 目标和现实一致
    db.set_project_goal(&project.id, "做一个用户管理系统：后端用户接口，加上调用它的前端用户列表页面。").unwrap();
    let ok = orch.align_goal_check(&m.id, &alice, &Deny).await.unwrap();
    println!("一致时：{ok}");
    assert!(ok.contains("仍然适用") && db.pending_goal_proposal(&project.id).unwrap().is_none());

    // 目标明确排除了前端，但 bob 在做前端：这是工作偏离目标，应该指出纠正，而不是改目标
    db.set_project_goal(&project.id, "只做后端用户接口。不做任何前端页面，前端由别的团队负责。").unwrap();
    let dev = orch.align_goal_check(&m.id, &alice, &Deny).await.unwrap();
    println!("工作偏离目标（负责人）：{dev}");
    assert!(db.pending_goal_proposal(&project.id).unwrap().is_none(), "工作偏离目标不应改目标");

    // 会上出现了需求变化：目标过时，负责人提议改目标
    db.post_message(&m.id, "human", None, "需求变更：别的团队的前端排期取消了，用户列表页面也由我们团队做，bob 的前端任务是对的。").unwrap();
    let bad = orch.align_goal_check(&m.id, &alice, &Deny).await.unwrap();
    println!("目标过时（负责人）：{bad}");
    let g = db.pending_goal_proposal(&project.id).unwrap();
    println!("提议：{:?}", g.as_ref().map(|g| (&g.new_goal, &g.reason)));
    assert!(g.is_some(), "负责人应提交目标变更提议");
    assert!(db.get_project(&project.id).unwrap().unwrap().goal.contains("不做任何前端"), "批准前目标不变");

    // 受托主持人（非负责人）不能提议，只指出
    db.decide_goal_proposal(&g.unwrap().id, false, None, "测试").unwrap();
    let proxy = orch.align_goal_check(&m.id, &bob, &Deny).await.unwrap();
    println!("目标过时（受托主持）：{proxy}");
    assert!(db.pending_goal_proposal(&project.id).unwrap().is_none(), "非负责人不会产生提议");
}
