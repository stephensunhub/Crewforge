//! 冒烟测试（默认忽略，会消耗一轮真实的 Claude 额度）：
//! 真实的 Claude 启动我们的 MCP 服务器，调用工具提议议题和记忆，并且工具调用不需要人类审批。
//! 运行：cargo test -p crewforge-cli --test real_mcp -- --ignored --nocapture

use crewforge_core::db::Db;
use crewforge_core::orchestrator::Orchestrator;
use crewforge_core::provider::claude::{ApprovalRequest, Decision};
use crewforge_core::runner::{Approver, NoopObserver};
use crewforge_core::workspace::WorkspaceManager;
use std::sync::Arc;

/// 任何需要询问人类的操作都直接记为失败：团队协作工具必须被策略自动放行。
struct DenyAndRecord(std::sync::Mutex<Vec<String>>);
impl Approver for DenyAndRecord {
    async fn ask(&self, _: &str, req: &ApprovalRequest, _: &str) -> Decision {
        self.0.lock().unwrap().push(req.tool_name.clone());
        Decision::Deny("测试中不允许".into())
    }
}

#[tokio::test]
#[ignore]
async fn real_claude_proposes_an_issue_and_a_memory_through_mcp() {
    let data = tempfile::tempdir().unwrap();
    let repo = data.path().join("proj");
    std::fs::create_dir_all(&repo).unwrap();
    let db = Arc::new(Db::open(&data.path().join("home")).unwrap());
    let ws = Arc::new(WorkspaceManager::new(&data.path().join("home")));
    ws.init_repo(&repo).await.unwrap();
    let repo = ws.repo_info(&repo).await.unwrap().root;
    let alice = db.add_employee("alice", "全栈", "你是 Alice，一名全栈工程师。回答简短。").unwrap();
    let project = db.create_project("P", "", repo.to_str().unwrap()).unwrap();
    db.add_member(&project.id, &alice.id, "lead").unwrap();

    let mut orch = Orchestrator::new(db.clone(), ws);
    orch.mcp_exe = Some(env!("CARGO_BIN_EXE_crewforge").into());
    let approver = DenyAndRecord(Default::default());
    let prompt = "这是团队协作工具的联调测试，请严格按顺序做三件事：\
        1) 调用 list_issues；\
        2) 调用 propose_issue，title 为“联调测试：日志要不要统一用 JSON 格式”，summary 为“这是联调测试产生的议题，用来验证提议接口”，kind 为 process；\
        3) 调用 propose_memory，content 为“联调测试经验：这条记忆只用于验证提议接口”。\
        三件事做完后只回复 done。不要修改任何文件。";
    let r = orch.assign(&alice, &repo, prompt, &approver, &NoopObserver).await.unwrap();
    println!("回复：{}\n费用：{:?}", r.text, r.cost_usd);

    assert!(approver.0.lock().unwrap().is_empty(), "团队协作工具不应触发人类审批，却询问了：{:?}", approver.0.lock().unwrap());
    let issues = db.issue_views(&project.id).unwrap();
    assert_eq!(issues.len(), 1, "应有一个议题");
    assert_eq!((issues[0].issue.status.as_str(), issues[0].issue.raised_by.as_deref(), issues[0].issue.kind.as_str()), ("open", Some(alice.id.as_str()), "process"));
    let mems = db.list_memories(&Default::default()).unwrap();
    assert_eq!((mems.len(), mems[0].status.as_str(), mems[0].source_kind.as_str()), (1, "unverified", "agent"));
    let proposed: Vec<_> = db.recent_events_for_project(&project.id, 200).unwrap().into_iter().filter(|e| e.kind == "agent_proposed").collect();
    assert_eq!(proposed.len(), 2);
    assert!(proposed[0].payload["origin"].as_str().unwrap().starts_with("task:"));
}
