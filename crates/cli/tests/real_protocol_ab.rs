//! 沟通规范的效果对比（默认忽略，真实调用）：同样的输入，规范开 / 关各跑一遍。
//! 场景：两个改同一个文件、字段命名不一致的任务 → 站会发言 + 纪要；再让一名员工做一个小任务并汇报。
use crewforge_core::db::Db;
use crewforge_core::orchestrator::Orchestrator;
use crewforge_core::provider::claude::{ApprovalRequest, Decision};
use crewforge_core::runner::{Approver, NoopObserver};
use crewforge_core::workspace::WorkspaceManager;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

struct AllowBash;
impl Approver for AllowBash {
    async fn ask(&self, _: &str, req: &ApprovalRequest, _: &str) -> Decision {
        if matches!(req.tool_name.as_str(), "Bash" | "Edit" | "Write" | "MultiEdit") { Decision::Allow } else { Decision::Deny("不允许".into()) }
    }
}

struct Outcome { label: &'static str, standup_chars: Vec<usize>, minutes: String, actions: Vec<String>, task_report: String, flags: usize, out_tokens: u64, cost: f64 }

async fn scenario(on: bool) -> Outcome {
    let data = tempfile::tempdir().unwrap();
    let repo_dir = data.path().join("proj");
    std::fs::create_dir_all(&repo_dir).unwrap();
    let db = Arc::new(Db::open(&data.path().join("home")).unwrap());
    db.set_setting("protocol", if on { "on" } else { "off" }).unwrap();
    let ws = Arc::new(WorkspaceManager::new(&data.path().join("home")));
    ws.init_repo(&repo_dir).await.unwrap();
    std::fs::write(repo_dir.join("README.md"), "demo\n").unwrap();
    std::fs::write(repo_dir.join("calc.py"), "def add(a, b):\n    return a + b\n").unwrap();
    std::fs::write(repo_dir.join("test_calc.py"), "from calc import add\n\ndef test_add():\n    assert add(1, 2) == 3\n").unwrap();
    for a in [vec!["add", "-A"], vec!["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "seed"]] {
        assert!(std::process::Command::new("git").arg("-C").arg(&repo_dir).args(a).status().unwrap().success());
    }
    let repo = ws.repo_info(&repo_dir).await.unwrap().root;
    let project = db.create_project("Demo", "", repo.to_str().unwrap()).unwrap();
    let alice = db.add_employee("alice", "全栈", "你是 alice，后端工程师。").unwrap();
    let bob = db.add_employee("bob", "全栈", "你是 bob，前端工程师。").unwrap();
    let carol = db.add_employee("carol", "全栈", "你是 carol，工程师。").unwrap();
    for e in [&alice, &bob, &carol] { db.add_member(&project.id, &e.id, "developer").unwrap(); }
    let mut orch = Orchestrator::new(db.clone(), ws);
    orch.mcp_exe = Some(env!("CARGO_BIN_EXE_crewforge").into());

    // 造两个进行中的任务（假 CLI，不计成本）
    let fake = data.path().join("fake");
    std::fs::write(&fake, "#!/bin/sh\nread -r l\necho '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"x\"}'\necho x > api.md\nprintf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"ok\",\"usage\":{}}'\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    orch.program = fake.to_string_lossy().into_owned();
    orch.assign(&alice, &repo, "后端：实现用户接口，字段 userId", &AllowBash, &NoopObserver).await.unwrap();
    orch.assign(&bob, &repo, "前端：调用用户接口并展示，字段 user_id", &AllowBash, &NoopObserver).await.unwrap();
    let fake_events = db.cost_report(None, 480).unwrap().totals.output_tokens;
    orch.program = "claude".into();

    // 站会 + 纪要
    let (m, who) = orch.start_standup(&project.id).await.unwrap();
    let mut chars = vec![];
    for w in &who { chars.push(orch.meeting_reply(&m.id, w, &AllowBash).await.unwrap().chars().count()); }
    orch.standup_alignment_check(&m.id).await.unwrap();
    let minutes = orch.draft_minutes(&m.id, &AllowBash).await.unwrap();
    let actions = db.actions_for_meeting(&m.id).unwrap().into_iter().map(|a| a.text).collect();

    // 一个真实的小任务和它的汇报
    let r = orch.assign(&carol, &repo, "在 calc.py 里加一个 sub(a, b) 函数，并在 test_calc.py 里加对应测试，运行测试确认通过。", &AllowBash, &NoopObserver).await.unwrap();
    let flags = db.recent_events(500).unwrap().into_iter().filter(|e| e.kind == "report_flags").count();
    let cost = db.cost_report(None, 480).unwrap();
    Outcome { label: if on { "规范 开" } else { "规范 关" }, standup_chars: chars, minutes, actions, task_report: r.text, flags,
        out_tokens: cost.totals.output_tokens - fake_events, cost: cost.totals.cost_usd }
}

#[tokio::test]
#[ignore]
async fn protocol_on_vs_off() {
    let off = scenario(false).await;
    let on = scenario(true).await;
    for o in [&off, &on] {
        println!("\n==================== {} ====================", o.label);
        println!("站会发言字数：{:?}（合计 {}）", o.standup_chars, o.standup_chars.iter().sum::<usize>());
        println!("纪要行动项 {} 条：", o.actions.len());
        for a in &o.actions { println!("  - {a}"); }
        println!("任务汇报（{} 字，lint 提醒 {} 次）：\n{}", o.task_report.chars().count(), o.flags, o.task_report);
        println!("输出 token {} · 费用 ${:.3}", o.out_tokens, o.cost);
    }
}
