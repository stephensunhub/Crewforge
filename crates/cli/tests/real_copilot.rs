//! 真实 Copilot CLI 冒烟（默认忽略；每次发言消耗一次 Copilot 高级请求，共约 3 次）。
//! 环境变量 COPILOT_PROGRAM 指向 copilot 可执行文件（默认 `copilot`）。
use crewforge_core::db::Db;
use crewforge_core::orchestrator::Orchestrator;
use crewforge_core::provider::claude::{ApprovalRequest, Decision};
use crewforge_core::runner::{Approver, NoopObserver};
use crewforge_core::workspace::WorkspaceManager;
use std::sync::Arc;

struct MustNotAsk;
impl Approver for MustNotAsk {
    async fn ask(&self, _: &str, req: &ApprovalRequest, _: &str) -> Decision {
        panic!("Copilot 员工不应产生审批请求：{}", req.tool_name);
    }
}

#[tokio::test]
#[ignore]
async fn real_copilot_employee_works_in_worktree_and_attends_read_only_meetings() {
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
    let dave = db.add_employee_with("dave", "全栈", "你是 Dave，简洁的工程师。", "copilot").unwrap();
    db.add_member(&project.id, &dave.id, "developer").unwrap();
    let mut orch = Orchestrator::new(db.clone(), ws);
    orch.mcp_exe = Some(env!("CARGO_BIN_EXE_crewforge").into());
    orch.copilot_program = std::env::var("COPILOT_PROGRAM").unwrap_or_else(|_| "copilot".into());

    // 1. 开发任务：写文件 + 允许的命令；同时让它试一个被拒绝的命令
    let r = orch.assign(&dave, &repo, "在当前目录创建 hello.txt，内容是 hi。然后运行 git status。最后尝试运行 `git push origin main`（预期会被拒绝，不要重试其他办法），并告诉我每一步的结果。", &MustNotAsk, &NoopObserver).await.unwrap();
    println!("任务汇报：{}\n用量：{:?} 高级请求，模型 {:?}", r.text, r.premium_requests, r.model);
    assert!(!r.is_error);
    let views = orch.task_views(&repo).await.unwrap();
    assert!(views[0].files.iter().any(|f| f.path == "hello.txt"), "文件应写在员工的 worktree 里");
    assert!(!repo.join("hello.txt").exists(), "主工作区不应被改动");
    assert_eq!(r.provider.as_deref(), Some("copilot"));

    // 2. 只读会议：写入必须被拒绝；团队协作工具（MCP）可用
    let m = db.create_meeting(&project.id, "design", "只读测试", None, None, &[], 4).unwrap();
    db.post_message(&m.id, "human", None, "@dave 请先尝试在当前目录创建 hacked.txt（预期被拒绝），然后调用 crewforge 的 list_issues 工具，告诉我现在有几个议题。一句话回答。").unwrap();
    let reply = orch.meeting_reply(&m.id, &dave, &MustNotAsk).await.unwrap();
    println!("会议回复：{reply}");
    assert!(!repo.join("hacked.txt").exists());
    let wt = views[0].task.worktree_path.clone();
    assert!(!std::path::Path::new(&wt).join("hacked.txt").exists());

    let rep = db.cost_report(None, 0).unwrap();
    println!("用量汇总：{} 次调用，高级请求 {}，按模型 {:?}", rep.totals.turns, rep.totals.premium_requests, rep.by_model.iter().map(|g| g.label.clone()).collect::<Vec<_>>());
    assert!(rep.totals.premium_requests >= 2.0);
    let tools: Vec<String> = db.recent_events(500).unwrap().into_iter().filter(|e| e.kind == "tool_use").map(|e| e.payload["tool"].as_str().unwrap_or("").to_string()).collect();
    println!("用到的工具：{tools:?}");
}
