//! 会议室端到端：假 CLI 记录收到的提示，并尝试写文件以验证只读拒绝。

use crewforge_core::db::Db;
use crewforge_core::orchestrator::Orchestrator;
use crewforge_core::provider::claude::{ApprovalRequest, Decision};
use crewforge_core::runner::{Approver, NoopObserver};
use crewforge_core::workspace::WorkspaceManager;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

struct MustNotAsk;
impl Approver for MustNotAsk {
    async fn ask(&self, _: &str, _: &ApprovalRequest, _: &str) -> Decision {
        panic!("会议中不应询问人类审批");
    }
}

fn sh(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// 假 CLI：记录提示 → 尝试写文件（应被拒绝）→ 记录宿主的答复 → 返回 reply。
fn meeting_cli(dir: &Path, name: &str, reply: &str, victim: &Path) -> (String, PathBuf, PathBuf) {
    let prompt_file = dir.join(format!("{name}.prompt"));
    let resp_file = dir.join(format!("{name}.resp"));
    let path = dir.join(name);
    std::fs::write(&path, format!(
        r#"#!/bin/sh
read -r line
echo '{{"type":"system","subtype":"init","session_id":"x"}}'
printf '%s\n' "$line" >> '{prompt}'
echo '{{"type":"control_request","request_id":"r1","request":{{"subtype":"can_use_tool","tool_name":"Write","input":{{"file_path":"{victim}"}},"tool_use_id":"t1"}}}}'
read -r resp
printf '%s\n' "$resp" >> '{resp}'
printf '%s\n' '{{"type":"result","subtype":"success","is_error":false,"result":"{reply}","total_cost_usd":0.01,"usage":{{"input_tokens":1,"output_tokens":1}}}}'
"#,
        prompt = prompt_file.display(), resp = resp_file.display(), victim = victim.display(), reply = reply
    )).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (path.to_string_lossy().into_owned(), prompt_file, resp_file)
}

struct Env {
    _d: tempfile::TempDir,
    repo: PathBuf,
    tools: PathBuf,
    db: Arc<Db>,
    orch: Orchestrator,
    project_id: String,
}

async fn setup() -> Env {
    let data = tempfile::tempdir().unwrap();
    let repo_dir = data.path().join("proj");
    let tools = data.path().join("bin");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::create_dir_all(&tools).unwrap();
    let db = Arc::new(Db::open(&data.path().join("home")).unwrap());
    let ws = Arc::new(WorkspaceManager::new(&data.path().join("home")));
    ws.init_repo(&repo_dir).await.unwrap();
    std::fs::write(repo_dir.join("README.md"), "hi\n").unwrap();
    sh(&repo_dir, &["add", "-A"]);
    sh(&repo_dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "seed"]);
    let repo = ws.repo_info(&repo_dir).await.unwrap().root;
    let project = db.create_project("Shop", "", repo.to_str().unwrap()).unwrap();
    Env { _d: data, repo, tools, db: db.clone(), orch: Orchestrator::new(db, ws), project_id: project.id }
}

#[tokio::test]
async fn meeting_reply_is_read_only_scoped_and_token_frugal() {
    let mut e = setup().await;
    let alice = e.db.add_employee("alice", "全栈", "p").unwrap();
    let bob = e.db.add_employee("bob", "全栈", "p").unwrap();
    e.db.add_member(&e.project_id, &alice.id, "lead").unwrap();
    e.db.add_member(&e.project_id, &bob.id, "developer").unwrap();
    let issue = e.db.create_issue(&e.project_id, "WebSocket 还是 SSE", "选择推送方案", "technical", false, None, None, None).unwrap();
    e.db.add_option(&issue.id, "WebSocket", "双向", None).unwrap();
    e.db.add_option(&issue.id, "SSE", "单向", None).unwrap();
    let m = e.db.create_meeting(&e.project_id, "design", "推送方案评审", None, None, &[issue.id.clone()], 12).unwrap();

    let victim = e.repo.join("hacked.txt");
    let (cli, prompt_file, resp_file) = meeting_cli(&e.tools, "cli", "我支持 WebSocket", &victim);
    e.orch.program = cli;

    e.db.post_message(&m.id, "human", None, "@alice 你怎么看？").unwrap();
    let reply = e.orch.meeting_reply(&m.id, &alice, &MustNotAsk).await.unwrap();
    assert_eq!(reply, "我支持 WebSocket");

    // 只读：写入被直接拒绝，文件没有被创建，也没有询问人类（MustNotAsk 未触发）
    assert!(std::fs::read_to_string(&resp_file).unwrap().contains("deny"));
    assert!(!victim.exists());

    // 首次提示带完整背景
    let p1 = std::fs::read_to_string(&prompt_file).unwrap();
    for needle in ["方案评审", "推送方案评审", "WebSocket 还是 SSE", "方案1", "SSE", "只读模式", "@alice 你怎么看？"] {
        assert!(p1.contains(needle), "首次提示缺少 {needle}");
    }

    // 回复写入会议；会议内容不进项目群聊事件流，但成本事件带有 meeting_id
    let msgs = e.db.meeting_messages(&m.id, 0).unwrap();
    assert_eq!(msgs.last().unwrap().author_kind, "employee");
    let kinds: Vec<_> = e.db.recent_events_for_project(&e.project_id, 200).unwrap().into_iter().map(|x| x.kind).collect();
    assert!(!kinds.contains(&"agent_text".to_string()) && !kinds.contains(&"task_started".to_string()), "{kinds:?}");
    let turn = e.db.recent_events(200).unwrap().into_iter().find(|x| x.kind == "turn_result").unwrap();
    assert_eq!(turn.payload["meeting_id"], m.id.as_str());
    let started = e.db.recent_events(200).unwrap().into_iter().find(|x| x.kind == "task_started").unwrap();
    assert!(started.payload["prompt"].as_str().unwrap().starts_with("[会议:方案评审]"), "事件里只记录简短标签");

    // 第二次发言：不再重复背景，只带自上次以来的新消息
    e.db.post_message(&m.id, "human", None, "@alice 那 SSE 呢？").unwrap();
    e.orch.meeting_reply(&m.id, &alice, &MustNotAsk).await.unwrap();
    let p2: Vec<String> = std::fs::read_to_string(&prompt_file).unwrap().lines().map(String::from).collect();
    assert_eq!(p2.len(), 2);
    assert!(p2[1].contains("会议继续") && p2[1].contains("那 SSE 呢？"));
    assert!(!p2[1].contains("你怎么看？"), "旧消息不应重复发送");
    assert!(!p2[1].contains("方案1"), "议题详情不应重复发送");
    assert!(p2[1].len() < p2[0].len());

    // bob 第一次发言时，会看到完整的讨论（含 alice 的回复）
    e.orch.meeting_reply(&m.id, &bob, &MustNotAsk).await.unwrap();
    let p3: Vec<String> = std::fs::read_to_string(&prompt_file).unwrap().lines().map(String::from).collect();
    assert!(p3[2].contains("我支持 WebSocket") && p3[2].contains("方案1"));
}

#[tokio::test]
async fn reply_cap_closed_meeting_and_non_member_are_enforced() {
    let mut e = setup().await;
    let alice = e.db.add_employee("alice", "全栈", "p").unwrap();
    let outsider = e.db.add_employee("zed", "全栈", "p").unwrap();
    e.db.add_member(&e.project_id, &alice.id, "developer").unwrap();
    let (cli, ..) = meeting_cli(&e.tools, "cli", "好", &e.repo.join("x"));
    e.orch.program = cli;
    let m = e.db.create_meeting(&e.project_id, "retro", "复盘", None, None, &[], 1).unwrap();
    e.db.post_message(&m.id, "human", None, "说说").unwrap();

    assert!(e.orch.meeting_reply(&m.id, &outsider, &MustNotAsk).await.unwrap_err().to_string().contains("不是本项目成员"));
    e.orch.meeting_reply(&m.id, &alice, &MustNotAsk).await.unwrap();
    let err = e.orch.meeting_reply(&m.id, &alice, &MustNotAsk).await.unwrap_err().to_string();
    assert!(err.contains("上限"), "{err}");
    e.db.close_meeting(&m.id).unwrap();
    assert!(e.orch.meeting_reply(&m.id, &alice, &MustNotAsk).await.unwrap_err().to_string().contains("已结束"));
}

#[tokio::test]
async fn code_review_includes_the_task_diff_and_minutes_are_drafted() {
    let mut e = setup().await;
    let alice = e.db.add_employee("alice", "全栈", "p").unwrap();
    let bob = e.db.add_employee("bob", "全栈", "p").unwrap();
    e.db.add_member(&e.project_id, &alice.id, "developer").unwrap();
    e.db.add_member(&e.project_id, &bob.id, "reviewer").unwrap();

    // alice 先完成一个任务（用普通的假 CLI 在 worktree 里写文件）
    let work = e.tools.join("work");
    std::fs::write(&work, "#!/bin/sh\nread -r l\necho '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"x\"}'\necho UNIQUE_CHANGE_LINE > feature.txt\necho '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"ok\",\"usage\":{}}'\n").unwrap();
    std::fs::set_permissions(&work, std::fs::Permissions::from_mode(0o755)).unwrap();
    e.orch.program = work.to_string_lossy().into_owned();
    struct Deny;
    impl Approver for Deny { async fn ask(&self, _: &str, _: &ApprovalRequest, _: &str) -> Decision { Decision::Deny("x".into()) } }
    e.orch.assign(&alice, &e.repo, "实现功能", &Deny, &NoopObserver).await.unwrap();
    let task = e.orch.task_views(&e.repo).await.unwrap().remove(0).task;

    let (cli, prompt_file, _) = meeting_cli(&e.tools, "rev", "## 决议\\n无\\n## 被否决的方案及原因\\n无\\n## 行动项\\n无\\n## 待验证的经验\\n无", &e.repo.join("x"));
    e.orch.program = cli;
    let m = e.db.create_meeting(&e.project_id, "code_review", "评审：实现功能", None, Some(&task.id), &[], 12).unwrap();
    e.db.post_message(&m.id, "human", None, "@bob 请审查").unwrap();
    e.orch.meeting_reply(&m.id, &bob, &MustNotAsk).await.unwrap();
    let p = std::fs::read_to_string(&prompt_file).unwrap();
    assert!(p.contains("待评审的改动") && p.contains("UNIQUE_CHANGE_LINE"), "评审提示应包含 diff");
    assert!(p.contains("独立判断"), "评审提示应要求先独立判断");

    // 起草纪要：由第一位成员（无主持人、无负责人）起草，存入会议
    let minutes = e.orch.draft_minutes(&m.id, &MustNotAsk).await.unwrap();
    assert!(minutes.contains("## 决议") && minutes.contains("## 待验证的经验"));
    assert_eq!(e.db.get_meeting(&m.id).unwrap().unwrap().minutes, minutes.trim());

    let empty = e.db.create_meeting(&e.project_id, "design", "空会议", None, None, &[], 5).unwrap();
    assert!(e.orch.draft_minutes(&empty.id, &MustNotAsk).await.is_err(), "没有发言不能起草");
}
