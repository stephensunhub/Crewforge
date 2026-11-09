//! 自主推进：用假 CLI 验证负责人循环——按依赖派活、验证、评审、合并，预算、暂停、需要人时停下来。不消耗额度。

use crewforge_core::autopilot::{Autopilot, AutopilotCtl};
use crewforge_core::db::Db;
use crewforge_core::orchestrator::Orchestrator;
use crewforge_core::plan::NewPlanItem;
use crewforge_core::provider::claude::{ApprovalRequest, Decision};
use crewforge_core::runner::Approver;
use crewforge_core::scheduler::Scheduler;
use crewforge_core::workspace::WorkspaceManager;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::Ordering;

struct Deny;
impl Approver for Deny {
    async fn ask(&self, _: &str, _: &ApprovalRequest, _: &str) -> Decision {
        Decision::Deny("测试".into())
    }
}

fn sh(dir: &Path, args: &[&str]) {
    assert!(Command::new("git").arg("-C").arg(dir).args(args).status().unwrap().success());
}

/// 假 CLI：开发任务写一个唯一文件；评审返回意见；整理纪要返回带两条行动项的纪要。
fn fake_cli(dir: &Path) -> String {
    let path = dir.join("fake-claude");
    std::fs::write(&path, r###"#!/bin/sh
read -r line
echo '{"type":"system","subtype":"init","session_id":"x"}'
f="w_$$_$(date +%s%N).txt"
case "$line" in
  *负责人裁决*线上数据库*) printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"{\"action\":\"escalate\",\"reason\":\"涉及线上数据迁移，我没有足够依据\"}","usage":{}}' ;;
  *负责人裁决*) printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"我的裁决：\n{\"action\":\"decide\",\"option\":1,\"decision\":\"推送用 SSE\",\"reason\":\"单向推送足够，实现更简单\",\"rejected\":{\"2\":\"WebSocket 要额外处理心跳\"}}","usage":{}}' ;;
  *整理纪要*) printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"## 决议\n- 同意\n## 被否决的方案及原因\n无\n## 行动项\n- [合并前] alice：补一个说明\n- [之后] 人类：以后再看\n## 待验证的经验\n无","usage":{}}' ;;
  *请评审*) printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"评审意见：可以。","usage":{}}' ;;
  *站会*) printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"做完了；无依赖。","usage":{}}' ;;
  *) echo x > "$f"; printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"已完成。","usage":{}}' ;;
esac
"###).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

struct Env {
    _d: tempfile::TempDir,
    db: Arc<Db>,
    orch: Arc<Orchestrator>,
    sched: Arc<Scheduler>,
    pid: String,
    repo: PathBuf,
    ids: Vec<String>,
}

/// 项目：alice（负责人）、bob（开发）、carol（QA）；计划：A(bob) → B(bob)，C(carol) 独立。
async fn setup() -> Env {
    let d = tempfile::tempdir().unwrap();
    let repo_dir = d.path().join("proj");
    let tools = d.path().join("bin");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::create_dir_all(&tools).unwrap();
    let db = Arc::new(Db::open(&d.path().join("home")).unwrap());
    let ws = Arc::new(WorkspaceManager::new(&d.path().join("home")));
    ws.init_repo(&repo_dir).await.unwrap();
    std::fs::write(repo_dir.join("README.md"), "hi\n").unwrap();
    sh(&repo_dir, &["add", "-A"]);
    sh(&repo_dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "seed"]);
    let repo = ws.repo_info(&repo_dir).await.unwrap().root;
    let project = db.create_project("Shop", "", repo.to_str().unwrap()).unwrap();
    let alice = db.add_employee("alice", "全栈", "p").unwrap();
    let bob = db.add_employee("bob", "全栈", "p").unwrap();
    let carol = db.add_employee("carol", "QA", "p").unwrap();
    db.add_member(&project.id, &alice.id, "lead").unwrap();
    db.add_member(&project.id, &bob.id, "developer").unwrap();
    db.add_member(&project.id, &carol.id, "qa").unwrap();
    let mut orch = Orchestrator::new(db.clone(), ws);
    orch.program = fake_cli(&tools);
    let item = |t: &str, who: &str, deps: Vec<String>| {
        let aid = db.employee_by_name(who).unwrap().map(|e| e.id);
        db.create_plan_item(&project.id, &NewPlanItem { title: t.into(), description: String::new(), acceptance: String::new(), kind: "migrate".into(), module: String::new(), assignee_id: aid, deps }, None).unwrap()
    };
    let a = item("A", "bob", vec![]);
    let b = item("B", "bob", vec![a.clone()]);
    let c = item("C", "carol", vec![]);
    let sched = Scheduler::new(db.clone(), 3);
    Env { _d: d, db, orch: Arc::new(orch), sched, pid: project.id, repo, ids: vec![a, b, c] }
}

async fn run(e: &Env, max_calls: i64, ctl: Arc<AutopilotCtl>) -> crewforge_core::autopilot::AutopilotRun {
    let run = Autopilot::<Deny>::prepare(&e.orch, &e.pid, max_calls, 30).unwrap();
    Autopilot::new(e.orch.clone(), e.sched.clone(), Arc::new(Deny), ctl, &run).unwrap().run().await
}

#[tokio::test]
async fn lead_loop_runs_the_whole_plan_by_dependency_and_merges_everything() {
    let e = setup().await;
    // 提议中的计划条目：点“开始”就是对计划的确认
    e.db.create_plan_item(&e.pid, &NewPlanItem { title: "D（AI 提议）".into(), description: String::new(), acceptance: String::new(), kind: "migrate".into(), module: String::new(), assignee_id: None, deps: vec![] }, Some(&e.db.employee_by_name("alice").unwrap().unwrap().id)).unwrap();
    let r = run(&e, 100, Arc::new(AutopilotCtl::default())).await;
    assert_eq!((r.status.as_str(), r.reason.as_str()), ("finished", "计划全部完成"), "{r:?}");
    let v = e.db.plan_view(&e.pid).unwrap();
    assert_eq!((v.counts.done, v.percent_done), (4, 100));
    // 每个条目的改动都合并进了主分支
    assert!(e.repo.join("README.md").exists());
    let log = String::from_utf8(Command::new("git").arg("-C").arg(&e.repo).args(["log", "--oneline"]).output().unwrap().stdout).unwrap();
    assert!(log.lines().count() >= 5, "{log}");
    // B 依赖 A：A 先于 B 合并
    let merged_order: Vec<String> = e.db.recent_events(500).unwrap().into_iter().filter(|x| x.kind == "task_merged").map(|x| x.payload["title"].as_str().unwrap().to_string()).collect();
    let (pa, pb) = (merged_order.iter().position(|t| t == "A").unwrap(), merged_order.iter().position(|t| t == "B").unwrap());
    assert!(pa < pb, "B 依赖 A：A 先合并 → {merged_order:?}");
    // 时间线事件：开始、步骤、合并、结束；评审意见里给人的和不属于任务负责人的事项被写明原因留到之后
    let kinds: Vec<String> = e.db.recent_events(500).unwrap().into_iter().map(|x| x.kind).collect();
    for k in ["autopilot_started", "autopilot_step", "autopilot_merged", "autopilot_finished"] { assert!(kinds.contains(&k.to_string()), "缺少 {k}"); }
    let dismissed: Vec<_> = e.db.actions_for_meeting(&e.db.list_meetings(&e.pid).unwrap()[0].id).unwrap().into_iter().filter(|a| a.status == "dismissed").collect();
    assert!(!dismissed.is_empty() && dismissed.iter().all(|a| !a.note.is_empty()));
    assert!(r.calls > 0);
}

#[tokio::test]
async fn budget_pause_and_human_needed_stop_the_loop_without_starting_new_work() {
    // 预算：1 次调用，手上的做完就停，剩下的不再派
    let e = setup().await;
    let r = run(&e, 1, Arc::new(AutopilotCtl::default())).await;
    assert_eq!(r.status, "budget", "{r:?}");
    assert!(e.db.plan_view(&e.pid).unwrap().counts.done < 3, "预算用尽后不会把计划做完");

    // 暂停：一开始就暂停，不派任何活
    let e = setup().await;
    let ctl = Arc::new(AutopilotCtl::default());
    ctl.pause.store(true, Ordering::SeqCst);
    let r = run(&e, 100, ctl).await;
    assert_eq!(r.status, "paused");
    assert_eq!(e.db.list_tasks(e.repo.to_str().unwrap()).unwrap().len(), 0);

    // 需求类议题等人拍板：负责人不替人决定，停下来
    let e = setup().await;
    let issue = e.db.create_issue(&e.pid, "要不要支持多币种", "", "requirement", false, None, None, None).unwrap();
    e.db.set_issue_status(&issue.id, "discussing").unwrap();
    e.db.set_issue_status(&issue.id, "pending_decision").unwrap();
    let r = run(&e, 100, Arc::new(AutopilotCtl::default())).await;
    assert_eq!(r.status, "escalated");
    assert!(r.reason.contains("只有你能拍板"), "{r:?}");
    assert_eq!(e.db.list_tasks(e.repo.to_str().unwrap()).unwrap().len(), 0);
}

#[tokio::test]
async fn preconditions_are_checked_before_starting() {
    let e = setup().await;
    // 没有负责人
    let lead = e.db.list_members(&e.pid).unwrap().into_iter().find(|m| m.role == "lead").unwrap();
    e.db.set_member_role(&e.pid, &lead.employee_id, "developer").unwrap();
    assert!(Autopilot::<Deny>::prepare(&e.orch, &e.pid, 10, 10).unwrap_err().to_string().contains("负责人"));
    e.db.set_member_role(&e.pid, &lead.employee_id, "lead").unwrap();
    // 预算必须大于 0
    assert!(Autopilot::<Deny>::prepare(&e.orch, &e.pid, 0, 10).is_err());
    // 正在推进时不能再开一个批次
    let run = Autopilot::<Deny>::prepare(&e.orch, &e.pid, 10, 10).unwrap();
    assert!(Autopilot::<Deny>::prepare(&e.orch, &e.pid, 10, 10).unwrap_err().to_string().contains("已经有一个批次"));
    // 应用重启后，上次“推进中”的批次被标为已停止
    assert_eq!(e.db.close_orphaned_autopilot_runs().unwrap(), 1);
    assert_eq!(e.db.get_autopilot_run(&run.id).unwrap().unwrap().status, "stopped");
}

#[tokio::test]
async fn lead_rules_ai_raised_technical_issues_escalates_the_rest_and_never_touches_human_or_requirement_issues() {
    let e = setup().await;
    let alice = e.db.employee_by_name("alice").unwrap().unwrap();
    let bob = e.db.employee_by_name("bob").unwrap().unwrap();
    // 1) 员工提议的技术议题，有两个方案 → 负责人裁决
    let tech = e.db.create_issue(&e.pid, "推送方案：SSE 还是 WebSocket", "选择推送方案", "technical", false, Some(&bob.id), None, None).unwrap();
    e.db.add_option(&tech.id, "SSE", "单向", None).unwrap();
    e.db.add_option(&tech.id, "WebSocket", "双向", None).unwrap();
    // 2) 负责人判断不了的 → 升级给人
    let hard = e.db.create_issue(&e.pid, "是否升级线上数据库版本", "", "technical", false, Some(&bob.id), None, None).unwrap();
    // 3) 需求类（AI 提议）、4) 人提的技术议题 → 不碰
    let req = e.db.create_issue(&e.pid, "要不要支持多币种", "", "requirement", false, Some(&bob.id), None, None).unwrap();
    let human = e.db.create_issue(&e.pid, "日志用哪个库", "", "technical", false, None, None, None).unwrap();

    let r = run(&e, 100, Arc::new(AutopilotCtl::default())).await;
    assert_eq!(r.status, "finished", "{r:?}");

    let t = e.db.get_issue(&tech.id).unwrap().unwrap();
    assert_eq!((t.status.as_str(), t.decided_by.as_deref(), t.decision.as_deref()), ("decided", Some(alice.id.as_str()), Some("推送用 SSE")), "{t:?}");
    assert_eq!(t.decision_reason.as_deref(), Some("单向推送足够，实现更简单"));
    let opts = e.db.issue_options(&tech.id).unwrap();
    assert_eq!(opts.iter().find(|o| o.title == "SSE").unwrap().status, "chosen");
    let ws = opts.iter().find(|o| o.title == "WebSocket").unwrap();
    assert_eq!((ws.status.as_str(), ws.reject_reason.as_deref()), ("rejected", Some("WebSocket 要额外处理心跳")), "没选的方案记录原因");
    // 决议沉淀为已验证的项目决策记忆
    let mem = e.db.list_memories(&Default::default()).unwrap();
    assert!(mem.iter().any(|m| m.kind == "decision" && m.status == "verified" && m.content.contains("SSE")), "{mem:?}");

    let h = e.db.get_issue(&hard.id).unwrap().unwrap();
    assert_eq!(h.status, "pending_decision", "升级给人：进入待决策");
    assert!(h.decision.is_none());
    assert_eq!(e.db.get_issue(&req.id).unwrap().unwrap().status, "open");
    assert_eq!(e.db.get_issue(&human.id).unwrap().unwrap().status, "open");

    let kinds: Vec<(String, String)> = e.db.recent_events(800).unwrap().into_iter().map(|x| (x.kind, x.payload["text"].as_str().unwrap_or("").to_string())).collect();
    assert!(kinds.iter().any(|(k, t)| k == "autopilot_decided" && t.contains("推送用 SSE")));
    assert!(kinds.iter().any(|(k, t)| k == "autopilot_issue_escalated" && t.contains("线上数据")));

    // 同一个议题不会在下一批里被反复裁决
    let r2 = run(&e, 100, Arc::new(AutopilotCtl::default())).await;
    let again = e.db.recent_events(800).unwrap().into_iter().filter(|x| x.kind == "autopilot_issue_escalated").count();
    assert_eq!((r2.status.as_str(), again), ("finished", 1), "{r2:?}");
}
