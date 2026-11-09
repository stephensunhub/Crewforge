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
