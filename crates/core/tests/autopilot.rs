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
    fake_cli_with(dir, "fake-claude", "- [合并前] alice：补一个说明\\n- [之后] 人类：以后再看")
}

/// 同上，但评审纪要里的行动项由调用方指定（`\\n` 分隔，已是 JSON 转义后的写法）。
fn fake_cli_with(dir: &Path, name: &str, actions: &str) -> String {
    let path = dir.join(name);
    let script = r###"#!/bin/sh
read -r line
echo '{"type":"system","subtype":"init","session_id":"x"}'
f="w_$$_$(date +%s%N).txt"
case "$line" in *"[角色交接]"*) printf '%s' "$line" > "@SEEN@" ;; esac
case "$line" in
  *"接手复盘"*) printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"{\"changes\":[{\"op\":\"set_level\",\"item\":1,\"level\":2},{\"op\":\"reassign\",\"item\":3,\"assignee\":\"bob\"},{\"op\":\"add\",\"title\":\"夹带\",\"reason\":\"x\"}],\"escalate\":null,\"summary\":\"复盘\"}","usage":{}}' ;;
  *"把这个角色交接给"*) printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"摘要：正在推进脚手架，决定用 SSE，下一步先看 C。","usage":{}}' ;;
  *"写 docs/architecture.md"*) mkdir -p docs; if [ -f "@BADDOC@" ]; then echo 'package main' > main.go; fi; printf '# 架构\n## 模块\nx\n## 给团队的要点\n- 模块之间只通过接口通信\n- 错误统一用 error 返回\n' > docs/architecture.md; printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"已写完架构文档。","usage":{}}' ;;
  *"写 docs/conventions.md"*) mkdir -p docs; printf '# 规范\n## 命名\nx\n## 给团队的要点\n- 函数小而纯\n- 先写测试\n' > docs/conventions.md; printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"已写完规范文档。","usage":{}}' ;;
  *清理计划里的重复条目*) printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"{\"changes\":[{\"op\":\"cancel\",\"item\":4,\"reason\":\"与 #1 重复\"},{\"op\":\"add\",\"title\":\"夹带\",\"reason\":\"x\"}],\"escalate\":null,\"summary\":\"取消重复\"}","usage":{}}' ;;
  *是否需要评审*) if [ -f "@SKIP@" ]; then printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"{\"review\":false,\"reason\":\"只改了一个小文件\"}","usage":{}}'; else printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"{\"review\":true,\"reason\":\"逻辑有改动\"}","usage":{}}'; fi ;;
  *起草计划*) printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"{\"items\":[{\"title\":\"搭脚手架\",\"description\":\"d\",\"acceptance\":\"a\",\"assignee\":\"bob\",\"depends_on\":[]},{\"title\":\"写核心逻辑\",\"description\":\"d\",\"acceptance\":\"a\",\"assignee\":\"bob\",\"depends_on\":[0]},{\"title\":\"补测试\",\"description\":\"d\",\"acceptance\":\"a\",\"assignee\":\"carol\",\"depends_on\":[1]}]}","usage":{}}' ;;
  *调整计划*完成度复盘*) if [ -f "@MARK@" ]; then printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"{\"changes\":[],\"escalate\":null,\"summary\":\"没有遗漏\"}","usage":{}}'; else touch "@MARK@"; printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"{\"changes\":[{\"op\":\"add\",\"title\":\"补收尾文档\",\"description\":\"d\",\"acceptance\":\"a\",\"assignee\":\"bob\",\"depends_on\":[\"#1\"],\"reason\":\"目标要求有说明文档\"},{\"op\":\"cancel\",\"item\":1,\"reason\":\"复盘时不该取消\"}],\"escalate\":null,\"summary\":\"补文档\"}","usage":{}}'; fi ;;
  *撰写文档*) printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"# 项目概览\n\n这是一个测试项目。\n\n## 怎么运行\n见 README。","usage":{}}' ;;
  *整理记忆*) printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"{\"ops\":[{\"op\":\"verify\",\"memory\":1,\"reason\":\"README 里有对应说明\"},{\"op\":\"expire\",\"memory\":2,\"reason\":\"只是当前状态描述，不是经验\"},{\"op\":\"verify\",\"memory\":99,\"reason\":\"不存在\"}],\"escalate\":null,\"summary\":\"整理 2 条\"}","usage":{}}' ;;
  *调整计划*有条目反复失败*) printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"{\"changes\":[{\"op\":\"add\",\"title\":\"拆小 A1\",\"reason\":\"A 太大\",\"assignee\":\"bob\",\"depends_on\":[]},{\"op\":\"add\",\"title\":\"拆小 A2\",\"reason\":\"A 太大\",\"assignee\":\"bob\",\"depends_on\":[\"N1\"]},{\"op\":\"add\",\"title\":\"带坏依赖\",\"reason\":\"x\",\"depends_on\":[\"#99\"]},{\"op\":\"add\",\"title\":\"拆小 A3\",\"reason\":\"A 太大\",\"depends_on\":[\"N2\"]},{\"op\":\"add\",\"title\":\"超过上限\",\"reason\":\"x\"},{\"op\":\"cancel\",\"item\":2,\"reason\":\"B 不再需要\"},{\"op\":\"cancel\",\"item\":99,\"reason\":\"x\"},{\"op\":\"reassign\",\"item\":3,\"assignee\":\"bob\"},{\"op\":\"retry\",\"item\":1,\"note\":\"先做最小可用版本\"}],\"escalate\":null,\"summary\":\"把 A 拆小\"}","usage":{}}' ;;
  *负责人裁决*线上数据库*) printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"{\"action\":\"escalate\",\"reason\":\"涉及线上数据迁移，我没有足够依据\"}","usage":{}}' ;;
  *负责人裁决*) printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"我的裁决：\n{\"action\":\"decide\",\"option\":1,\"decision\":\"推送用 SSE\",\"reason\":\"单向推送足够，实现更简单\",\"rejected\":{\"2\":\"WebSocket 要额外处理心跳\"}}","usage":{}}' ;;
  *整理纪要*) printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"## 决议\n- 同意\n## 被否决的方案及原因\n无\n## 行动项\n@ACTIONS@\n## 待验证的经验\n无","usage":{}}' ;;
  *请评审*) printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"评审意见：可以。","usage":{}}' ;;
  *站会*) printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"做完了；无依赖。","usage":{}}' ;;
  *) echo x > "$f"; printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"已完成。","usage":{}}' ;;
esac
"###.replace("@MARK@", &dir.join("final_add_once").to_string_lossy()).replace("@SKIP@", &dir.join("review_skip").to_string_lossy()).replace("@BADDOC@", &dir.join("bad_doc").to_string_lossy()).replace("@SEEN@", &dir.join("handover_seen").to_string_lossy()).replace("@ACTIONS@", actions);
    std::fs::write(&path, script).unwrap();
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
    assert_eq!((v.counts.done, v.percent_done), (5, 100), "4 个条目加上负责人复盘时追加的 1 个");
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
    // 自主推进里 AI 负责人派的活，群里不能显示成人说的话：任务开始事件都带来源，没有一条来源为空
    let started: Vec<String> = e.db.recent_events(500).unwrap().into_iter().filter(|x| x.kind == "task_started").map(|x| x.payload["origin"].as_str().unwrap_or("").to_string()).collect();
    assert!(!started.is_empty() && started.iter().all(|o| o == "autopilot" || o == "system"), "不应有来源为“人”的任务开始事件：{started:?}");
    assert!(started.iter().any(|o| o == "autopilot"));
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

    // 需求类议题（人提的、待拍板）不再让推进停下来：决策权跟着负责人走，人随时可以自己拍
    let e = setup().await;
    let issue = e.db.create_issue(&e.pid, "要不要支持多币种", "", "requirement", false, None, None, None).unwrap();
    e.db.set_issue_status(&issue.id, "discussing").unwrap();
    e.db.set_issue_status(&issue.id, "pending_decision").unwrap();
    let r = run(&e, 100, Arc::new(AutopilotCtl::default())).await;
    assert_ne!(r.status, "escalated", "{r:?}");
    assert_eq!(e.db.get_issue(&issue.id).unwrap().unwrap().status, "pending_decision", "人提的议题负责人不碰，留给提出的人");
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
async fn lead_rules_ai_raised_technical_issues_escalates_the_rest_and_never_touches_human_raised_issues() {
    let e = setup().await;
    let alice = e.db.employee_by_name("alice").unwrap().unwrap();
    let bob = e.db.employee_by_name("bob").unwrap().unwrap();
    // 1) 员工提议的技术议题，有两个方案 → 负责人裁决
    let tech = e.db.create_issue(&e.pid, "推送方案：SSE 还是 WebSocket", "选择推送方案", "technical", false, Some(&bob.id), None, None).unwrap();
    e.db.add_option(&tech.id, "SSE", "单向", None).unwrap();
    e.db.add_option(&tech.id, "WebSocket", "双向", None).unwrap();
    // 2) 负责人判断不了的 → 升级给人
    let hard = e.db.create_issue(&e.pid, "是否升级线上数据库版本", "", "technical", false, Some(&bob.id), None, None).unwrap();
    // 3) 需求类（AI 提议）→ 负责人也处理；4) 人提的技术议题 → 不碰
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
    assert_ne!(e.db.get_issue(&req.id).unwrap().unwrap().status, "open", "AI 提议的需求类议题也由负责人处理");
    assert_eq!(e.db.get_issue(&human.id).unwrap().unwrap().status, "open");

    let kinds: Vec<(String, String)> = e.db.recent_events(800).unwrap().into_iter().map(|x| (x.kind, x.payload["text"].as_str().unwrap_or("").to_string())).collect();
    assert!(kinds.iter().any(|(k, t)| k == "autopilot_decided" && t.contains("推送用 SSE")));
    assert!(kinds.iter().any(|(k, t)| k == "autopilot_issue_escalated" && t.contains("线上数据")));

    // 同一个议题不会在下一批里被反复裁决
    let r2 = run(&e, 100, Arc::new(AutopilotCtl::default())).await;
    let again = e.db.recent_events(800).unwrap().into_iter().filter(|x| x.kind == "autopilot_issue_escalated").count();
    assert_eq!((r2.status.as_str(), again), ("finished", 2), "{r2:?}"); // 升级过的两个议题都只升级一次
}

#[tokio::test]
async fn lead_drafts_a_plan_and_adjusts_it_within_limits() {
    let e = setup().await;
    let lead = e.db.employee_by_name("alice").unwrap().unwrap();
    // 起草：全新项目没有计划条目 → 清掉 setup 里的，再让负责人起草
    for id in &e.ids { e.db.set_plan_status(id, "cancelled", "测试").unwrap(); }
    assert!(e.orch.lead_draft_plan(&e.pid, &lead, &Deny).await.unwrap_err().to_string().contains("目标"), "没有项目目标时不起草");
    e.db.set_project_goal(&e.pid, "做一个小工具").unwrap();
    let n = e.orch.lead_draft_plan(&e.pid, &lead, &Deny).await.unwrap();
    assert_eq!(n, 3);
    let v = e.db.plan_view(&e.pid).unwrap();
    let drafted: Vec<_> = v.items.iter().filter(|i| i.status == "proposed").collect();
    assert_eq!(drafted.len(), 3, "起草的条目是“提议中”，等人确认");
    let core = drafted.iter().find(|i| i.title == "写核心逻辑").unwrap();
    assert_eq!((core.assignee_name.as_deref(), core.deps.len()), (Some("bob"), 1));
    assert!(e.db.recent_events(200).unwrap().iter().any(|x| x.kind == "plan_drafted_by_lead"));
}

#[tokio::test]
async fn final_review_may_only_add_and_the_plan_continues_with_the_added_item() {
    let e = setup().await;
    let r = run(&e, 200, Arc::new(AutopilotCtl::default())).await;
    assert_eq!(r.status, "finished", "{r:?}");
    let v = e.db.plan_view(&e.pid).unwrap();
    // 复盘追加了“补收尾文档”，并且也被做完、合并
    let added = v.items.iter().find(|i| i.title == "补收尾文档").expect("复盘追加的条目");
    assert_eq!(added.status, "done", "{:?}", v.items.iter().map(|i| (&i.title, &i.status)).collect::<Vec<_>>());
    assert!(added.note.contains("负责人追加") && added.note.contains("说明文档"));
    // 复盘时同时提出的“取消”被拒绝（复盘只能追加），原条目没被取消
    assert!(v.items.iter().filter(|i| ["A", "B", "C"].contains(&i.title.as_str())).all(|i| i.status == "done"));
    let texts: Vec<String> = e.db.recent_events(900).unwrap().into_iter().filter(|x| x.kind.starts_with("autopilot_")).map(|x| x.payload["text"].as_str().unwrap_or("").to_string()).collect();
    assert!(texts.iter().any(|t| t.contains("调整了计划") && t.contains("补收尾文档")), "{texts:?}");
    assert!(texts.iter().any(|t| t.contains("计划调整被拒绝") && t.contains("复盘时只能追加")));
}

#[tokio::test]
async fn failure_replan_adds_cancels_reassigns_and_retries_within_limits() {
    use crewforge_core::planning::AdjustTrigger;
    let e = setup().await;
    let lead = e.db.employee_by_name("alice").unwrap().unwrap();
    let (a, b, c) = (e.ids[0].clone(), e.ids[1].clone(), e.ids[2].clone());
    e.db.set_plan_status(&a, "blocked", "验证仍未通过").unwrap();
    let r = e.orch.lead_adjust_plan(&e.pid, &lead, AdjustTrigger::ItemFailed, "《A》：验证仍未通过", &Deny).await.unwrap();
    // 追加：最多 3 个；坏依赖和超上限的被拒绝，并说明原因
    assert_eq!(r.added, vec!["拆小 A1", "拆小 A2", "拆小 A3"], "{r:?}");
    assert!(r.rejected.iter().any(|x| x.contains("带坏依赖") && x.contains("不存在")), "{:?}", r.rejected);
    assert!(r.rejected.iter().any(|x| x.contains("超过上限")), "{:?}", r.rejected);
    assert!(r.rejected.iter().any(|x| x.contains("#99")), "不存在的条目：{:?}", r.rejected);
    // 取消、改派、重试
    assert_eq!(r.cancelled, vec!["B"]);
    assert_eq!(e.db.get_plan_item(&b).unwrap().unwrap().status, "cancelled");
    assert_eq!(r.reassigned.len(), 1);
    assert_eq!(e.db.get_plan_item(&c).unwrap().unwrap().assignee_name.as_deref(), Some("bob"));
    assert_eq!((r.retried.as_slice(), r.retried_ids.as_slice()), (&["A".to_string()][..], &[a.clone()][..]));
    let ra = e.db.get_plan_item(&a).unwrap().unwrap();
    assert_eq!(ra.status, "planned");
    assert!(ra.description.contains("先做最小可用版本"));
    // 追加的条目直接待开始，依赖关系正确（A2 依赖 A1，A3 依赖 A2），并写明了原因
    let v = e.db.plan_view(&e.pid).unwrap();
    let a2 = v.items.iter().find(|i| i.title == "拆小 A2").unwrap();
    let a1 = v.items.iter().find(|i| i.title == "拆小 A1").unwrap();
    assert_eq!((a2.status.as_str(), a2.deps.clone(), a2.note.contains("负责人追加")), ("planned", vec![a1.id.clone()], true));
    assert_eq!(r.summary, "把 A 拆小");
}

#[tokio::test]
async fn lead_publishes_project_docs_through_the_normal_merge_flow_and_republishing_is_a_noop() {
    let e = setup().await;
    let alice = e.db.employee_by_name("alice").unwrap().unwrap();
    let bob = e.db.employee_by_name("bob").unwrap().unwrap();
    // 一个被负责人裁决的议题和一个升级给人的议题 → 进入决策记录与已知问题
    let tech = e.db.create_issue(&e.pid, "推送方案：SSE 还是 WebSocket", "选择推送方案", "technical", false, Some(&bob.id), None, None).unwrap();
    e.db.add_option(&tech.id, "SSE", "单向", None).unwrap();
    e.db.add_option(&tech.id, "WebSocket", "双向", None).unwrap();
    e.db.create_issue(&e.pid, "是否升级线上数据库版本", "", "technical", false, Some(&bob.id), None, None).unwrap();

    let r = run(&e, 200, Arc::new(AutopilotCtl::default())).await;
    assert_eq!(r.status, "finished", "{r:?}");
    let docs = e.orch.list_docs(&e.pid).await.unwrap();
    let paths: Vec<&str> = docs.iter().map(|d| d.path.as_str()).collect();
    for p in ["docs/crewforge/overview.md", "docs/crewforge/decisions.md", "docs/crewforge/known-issues.md", "docs/crewforge/progress.md"] { assert!(paths.contains(&p), "缺少 {p}：{paths:?}"); }
    assert!(docs.iter().all(|d| d.managed));
    let decisions = e.orch.read_doc(&e.pid, "docs/crewforge/decisions.md").await.unwrap();
    assert!(decisions.contains("推送方案") && decisions.contains("推送用 SSE") && decisions.contains("WebSocket 要额外处理心跳") && decisions.contains("拍板人：alice"), "{decisions}");
    let known = e.orch.read_doc(&e.pid, "docs/crewforge/known-issues.md").await.unwrap();
    assert!(known.contains("是否升级线上数据库版本") && known.contains("待决策"), "{known}");
    assert!(e.orch.read_doc(&e.pid, "docs/crewforge/progress.md").await.unwrap().contains("完成"));
    assert!(e.orch.read_doc(&e.pid, "docs/crewforge/overview.md").await.unwrap().starts_with("# 项目概览"));
    // 走的是正常的合并流程：提交署名是负责人，任务记录在案
    let author = String::from_utf8(Command::new("git").arg("-C").arg(&e.repo).args(["log", "--format=%an|%s", "-n", "30"]).output().unwrap().stdout).unwrap();
    assert!(author.lines().any(|l| l.starts_with("alice|") && l.contains("文档")), "{author}");
    assert!(e.db.recent_events(900).unwrap().iter().any(|x| x.kind == "autopilot_docs"));

    // 路径与范围的限制：只能读 docs/ 下的 md；负责人只能写 docs/crewforge/
    assert!(e.orch.read_doc(&e.pid, "README.md").await.is_err());
    assert!(e.orch.read_doc(&e.pid, "docs/../README.md").await.is_err());
    assert!(e.orch.publish_docs(&e.pid, &alice, vec![("docs/other.md".into(), "x".into())], "越界").await.unwrap_err().to_string().contains("docs/crewforge"));
    // 内容没变就不产生新提交
    let again = e.orch.update_project_docs(&e.pid, &alice, true, &Deny).await.unwrap();
    assert!(again.commit.is_none() && again.note.contains("没有变化"), "{again:?}");
}

#[tokio::test]
async fn lead_curates_only_ai_sourced_project_memories_and_never_touches_human_or_decision_ones() {
    use crewforge_core::memory::NewMemory;
    let e = setup().await;
    let lead = e.db.employee_by_name("alice").unwrap().unwrap();
    let bob = e.db.employee_by_name("bob").unwrap().unwrap();
    let mem = |title: &str, source: &str, by: &str, status: &str| e.db.add_memory(NewMemory {
        layer: "project".into(), project_id: Some(e.pid.clone()), kind: "lesson".into(), title: title.into(), content: format!("{title} 的内容"), scope: String::new(),
        source_kind: source.into(), created_by: by.into(), status: status.into(), ..Default::default() }).unwrap();
    let a = mem("AI 经验甲", "agent", &bob.id, "");
    std::thread::sleep(std::time::Duration::from_millis(3));
    let b = mem("AI 经验乙", "agent", &bob.id, "");
    std::thread::sleep(std::time::Duration::from_millis(3));
    let c = mem("AI 经验丙", "meeting", &bob.id, "");
    let human = mem("人写的约定", "human", "human", "");
    let decision = mem("议题决议沉淀", "issue", &bob.id, "verified");
    assert_eq!((a.status.as_str(), human.status.as_str()), ("unverified", "verified"));
    // 候选只含负责人能动的
    let cands = e.orch.curation_candidates(&e.pid).unwrap();
    let ids: Vec<&str> = cands.iter().map(|m| m.id.as_str()).collect();
    assert!(ids.contains(&a.id.as_str()) && ids.contains(&c.id.as_str()) && !ids.contains(&human.id.as_str()) && !ids.contains(&decision.id.as_str()), "{ids:?}");

    let r = e.orch.lead_curate_memory(&e.pid, &lead, &Deny).await.unwrap();
    // 假负责人：验证 #1、让 #2 过期、#99 不存在被拒绝
    assert_eq!((r.verified.len(), r.expired.len()), (1, 1), "{r:?}");
    assert!(r.rejected.iter().any(|x| x.contains("#99")), "{:?}", r.rejected);
    let st = |id: &str| e.db.get_memory(id).unwrap().unwrap();
    assert_eq!((st(&a.id).status.as_str(), st(&a.id).verified_by.as_deref()), ("verified", Some(lead.id.as_str())));
    assert_eq!(st(&b.id).status, "expired");
    assert!(st(&b.id).expired_reason.unwrap_or_default().contains("负责人整理"));
    assert_eq!(st(&c.id).status, "unverified", "没提到的不动");
    assert_eq!((st(&human.id).status.as_str(), st(&decision.id).status.as_str()), ("verified", "verified"), "人写的和议题决议沉淀的不碰");
    // 没有待验证的时候不调用模型
    e.db.verify_memory(&c.id, "human").unwrap();
    let none = e.orch.lead_curate_memory(&e.pid, &lead, &Deny).await.unwrap();
    assert_eq!(none.summary, "没有待验证的记忆");
}

fn count_events(e: &Env, kind: &str, contains: &str) -> usize {
    e.db.recent_events(2000).unwrap().into_iter().filter(|x| x.kind == kind && x.payload["text"].as_str().unwrap_or("").contains(contains)).count()
}

#[tokio::test]
async fn at_most_three_review_comments_are_fixed_per_item_and_the_rest_become_later_items() {
    let mut e = setup().await;
    let tools = e._d.path().join("bin");
    // 5 条互不相同、没指明负责人的拦合并意见
    let many = "- [合并前] 补充甲方面的校验逻辑\\n- [合并前] 调整乙模块的错误提示文字\\n- [合并前] 增加丙场景的边界用例\\n- [合并前] 重命名丁函数使其更清楚\\n- [合并前] 删除戊目录里的废弃脚本";
    Arc::get_mut(&mut e.orch).unwrap().program = fake_cli_with(&tools, "fake-many", many);
    std::fs::write(tools.join("final_add_once"), "").unwrap(); // 复盘时不追加条目
    for id in &e.ids[1..] { e.db.set_plan_status(id, "cancelled", "测试").unwrap(); }
    let r = run(&e, 200, Arc::new(AutopilotCtl::default())).await;
    assert_eq!(r.status, "finished", "{r:?}");
    assert_eq!(count_events(&e, "autopilot_step", "评审意见交给"), 3, "最多处理 3 条");
    let m = e.db.list_meetings(&e.pid).unwrap();
    let acts = e.db.actions_for_meeting(&m[0].id).unwrap();
    let later: Vec<_> = acts.iter().filter(|a| a.status == "open" && !a.blocking).collect();
    assert_eq!(later.len(), 2, "超出的 2 条降级为之后再说，仍然开着、被跟踪：{acts:?}");
    assert!(later.iter().all(|a| a.note.contains("上限")), "{later:?}");
    assert_eq!(acts.iter().filter(|a| a.status == "done").count(), 3);
    // 降级的不进人的待办，只计入“之后再说”
    assert!(e.db.my_duties(&e.pid).unwrap().later_actions >= 2);
}

#[tokio::test]
async fn the_same_review_comment_in_parallel_tasks_is_fixed_only_once() {
    let mut e = setup().await;
    let tools = e._d.path().join("bin");
    let same = "- [合并前] 让 load 处理空文件或损坏 JSON（捕获 JSONDecodeError，报清楚的错误并以退出码 1 结束）";
    Arc::get_mut(&mut e.orch).unwrap().program = fake_cli_with(&tools, "fake-same", same);
    std::fs::write(tools.join("final_add_once"), "").unwrap(); // 复盘时不追加条目
    e.db.set_plan_status(&e.ids[1], "cancelled", "测试").unwrap(); // 只留并行的 A（bob）和 C（carol）
    let r = run(&e, 200, Arc::new(AutopilotCtl::default())).await;
    assert_eq!(r.status, "finished", "{r:?}");
    assert_eq!(count_events(&e, "autopilot_step", "评审意见交给"), 1, "两个任务收到同一条意见，只派出一次");
    assert_eq!(count_events(&e, "autopilot_step", "与已处理的重复"), 1);
    let all: Vec<_> = e.db.list_meetings(&e.pid).unwrap().into_iter().flat_map(|m| e.db.actions_for_meeting(&m.id).unwrap()).collect();
    let dup = all.iter().find(|a| a.status == "dismissed").expect("重复的那条被驳回");
    assert!(dup.note.contains("重复"), "{dup:?}");
}

#[tokio::test]
async fn near_the_budget_limit_no_new_item_is_started_and_the_budget_is_not_overshot() {
    let e = setup().await;
    e.db.set_plan_status(&e.ids[2], "cancelled", "测试").unwrap(); // 只留 A → B
    // A 约 3 次调用（开发、评审、纪要）；预算 5：A 做完后剩 2 次，不够再做一个条目（约 3 次），不应该开始 B
    let r = run(&e, 5, Arc::new(AutopilotCtl::default())).await;
    assert_eq!(r.status, "budget", "{r:?}");
    assert!(r.reason.contains("不够再做一个条目"), "{r:?}");
    assert!(r.calls <= 5, "没有超出预算：{r:?}");
    let v = e.db.plan_view(&e.pid).unwrap();
    assert_eq!(v.items.iter().find(|i| i.title == "A").unwrap().status, "done");
    assert_eq!(v.items.iter().find(|i| i.title == "B").unwrap().status, "planned", "B 没有被开始");
}

#[tokio::test]
async fn wrap_up_still_happens_when_the_batch_stops_because_of_the_budget() {
    let e = setup().await;
    e.db.set_plan_status(&e.ids[2], "cancelled", "测试").unwrap();
    // 预算 5：A 做完后扣掉收尾要留的调用数，不够再做 B → 以预算结束，但文档照常更新
    let r = run(&e, 5, Arc::new(AutopilotCtl::default())).await;
    assert_eq!(r.status, "budget", "{r:?}");
    assert!(r.reason.contains("收尾"), "原因里说明了为收尾留了预算：{r:?}");
    assert!(r.calls <= 5, "{r:?}");
    let docs = e.orch.list_docs(&e.pid).await.unwrap();
    assert!(docs.iter().any(|d| d.path == "docs/crewforge/progress.md"), "预算用尽时也应该写了文档：{docs:?}");
    assert!(e.db.recent_events(900).unwrap().iter().any(|x| x.kind == "autopilot_docs"));
}

fn parallel_standups(e: &Env) -> usize {
    e.db.recent_events(900).unwrap().iter().filter(|x| x.payload["text"].as_str().map(|t| t.contains("并行，开站会")).unwrap_or(false)).count()
}

#[tokio::test]
async fn a_standup_in_the_background_does_not_block_tasks_but_still_finishes_before_the_batch_ends() {
    use crewforge_core::autopilot::AutopilotConfig;
    let e = setup().await;
    e.db.set_plan_status(&e.ids[1], "cancelled", "测试").unwrap();
    e.db.set_autopilot_config(&e.pid, &AutopilotConfig { standup_blocks: false, ..AutopilotConfig::default() }).unwrap();
    let r = run(&e, 100, Arc::new(AutopilotCtl::default())).await;
    assert_eq!(r.status, "finished", "{r:?}");
    assert!(parallel_standups(&e) > 0, "关掉阻塞后对齐会照开");
    let evs = e.db.recent_events(900).unwrap();
    let started = evs.iter().filter(|x| x.kind == "meeting_started" && x.payload["kind"] == "standup").count();
    let closed = evs.iter().filter(|x| x.kind == "meeting_closed" && x.payload["title"].as_str().is_some_and(|t| t.contains("对齐会"))).count();
    assert!(started > 0 && closed == started, "批次结束前后台的对齐会都已收尾：开了 {started}，关了 {closed}");
    let v = e.db.plan_view(&e.pid).unwrap();
    assert!(v.items.iter().filter(|i| i.title == "A" || i.title == "C").all(|i| i.status == "done"), "任务照常完成");
}

#[tokio::test]
async fn items_expected_to_touch_the_same_file_never_run_in_parallel() {
    // 对照：不写文件时，A 和 C 互不依赖，会并行
    let free = setup().await;
    free.db.set_plan_status(&free.ids[1], "cancelled", "测试").unwrap();
    assert_eq!(run(&free, 100, Arc::new(AutopilotCtl::default())).await.status, "finished");
    assert!(parallel_standups(&free) > 0, "对照组应该有并行");

    // A 和 C 都声明要改 shop.py → 排成先后；B（依赖 A）改别的文件
    let e = setup().await;
    e.db.set_plan_status(&e.ids[1], "cancelled", "测试").unwrap();
    e.db.set_plan_item_files(&e.ids[0], &["shop.py".into()]).unwrap();
    e.db.set_plan_item_files(&e.ids[2], &["shop.py".into(), "tests/test_shop.py".into()]).unwrap();
    assert_eq!(e.db.plan_files(&e.pid).unwrap().len(), 2);
    let r = run(&e, 100, Arc::new(AutopilotCtl::default())).await;
    assert_eq!(r.status, "finished", "{r:?}");
    assert_eq!(parallel_standups(&e), 0, "改同一个文件的条目没有并行");
    let v = e.db.plan_view(&e.pid).unwrap();
    assert!(v.items.iter().filter(|i| i.title == "A" || i.title == "C").all(|i| i.status == "done"));
}

#[tokio::test]
async fn interrupted_batches_resume_with_the_remaining_budget_without_approving_new_proposals() {
    let e = setup().await;
    let ai = e.db.employee_by_name("alice").unwrap().unwrap().id;
    let new_item = |t: &str| NewPlanItem { title: t.into(), description: String::new(), acceptance: String::new(), kind: "migrate".into(), module: String::new(), assignee_id: None, deps: vec![] };

    // 应用被关掉时批次停在“推进中”
    let run = Autopilot::<Deny>::prepare(&e.orch, &e.pid, 40, 60).unwrap();
    e.db.set_autopilot_run(&run.id, "running", 10, "").unwrap();
    let orphans = e.db.take_orphaned_autopilot_runs().unwrap();
    assert_eq!(orphans.len(), 1);
    assert_eq!(e.db.latest_autopilot_run(&e.pid).unwrap().unwrap().status, "stopped", "重启后旧批次先标为已停止");

    // 恢复条件：默认开启；剩余预算 = 总预算 - 已用；刚启动不到一分钟的不恢复（防止崩溃循环）
    let old = orphans[0].clone();
    let later = old.started_at + 20 * 60_000;
    assert_eq!(e.db.plan_autopilot_resume(&old, later), Some((30, 40)));
    assert_eq!(e.db.plan_autopilot_resume(&old, old.started_at + 30_000), None, "刚启动就被中断：不恢复");
    assert_eq!(e.db.plan_autopilot_resume(&old, old.started_at + 58 * 60_000), None, "剩余时间太少：不恢复");
    let mut spent = old.clone();
    spent.calls = 39;
    assert_eq!(e.db.plan_autopilot_resume(&spent, later), None, "剩余调用太少：不恢复");
    e.db.set_autopilot_resume(&e.pid, false).unwrap();
    assert_eq!(e.db.plan_autopilot_resume(&old, later), None, "项目里关掉了这个选项");
    e.db.set_autopilot_resume(&e.pid, true).unwrap();

    // 恢复时不批准计划：重启期间 AI 新提议的条目仍要等人确认
    e.db.create_plan_item(&e.pid, &new_item("重启后 AI 才提议的"), Some(&ai)).unwrap();
    let resumed = Autopilot::<Deny>::prepare_resume(&e.orch, &e.pid, 30, 40).unwrap();
    assert_eq!((resumed.status.as_str(), resumed.max_calls), ("running", 30));
    let v = e.db.plan_view(&e.pid).unwrap();
    assert_eq!(v.counts.proposed, 1, "恢复不等于人批准了新提议");
    assert!(e.db.recent_events(50).unwrap().iter().any(|x| x.kind == "autopilot_started" && x.payload["text"].as_str().unwrap_or("").contains("接着")));
}

#[tokio::test]
async fn items_left_in_progress_by_a_restart_are_requeued_and_finished() {
    let e = setup().await;
    // 模拟应用重启：条目 A 停在“进行中”，但干活的进程已经没了
    let a = e.ids[0].clone();
    // alice 手上有一个真实的进行中任务（做到一半），条目 A 关联着它
    let alice = e.db.employee_by_name("alice").unwrap().unwrap();
    e.orch.assign(&alice, &e.repo, "A\n做到一半", &Deny, &crewforge_core::runner::NoopObserver).await.unwrap();
    let t = e.db.active_task(&alice.id, e.repo.to_str().unwrap()).unwrap().expect("alice 有进行中的任务");
    e.db.mark_plan_started(&a, &t.id).unwrap();
    // 条目指派给 alice：放回待开始后必须由 alice 接着做，不能因为“alice 还有进行中的任务”被拒绝
    let it = e.db.get_plan_item(&a).unwrap().unwrap();
    e.db.update_plan_item(&a, &NewPlanItem { title: it.title.clone(), description: it.description.clone(), acceptance: it.acceptance.clone(), kind: it.kind.clone(), module: it.module.clone(), assignee_id: Some(alice.id.clone()), deps: vec![] }).ok();
    assert_eq!(e.db.get_plan_item(&a).unwrap().unwrap().status, "in_progress");
    let r = run(&e, 100, Arc::new(AutopilotCtl::default())).await;
    assert_eq!((r.status.as_str(), r.reason.as_str()), ("finished", "计划全部完成"), "{r:?}");
    assert!(e.db.recent_events(500).unwrap().iter().any(|x| x.kind == "autopilot_step" && x.payload["text"].as_str().unwrap_or("").contains("放回待开始")));
    // 接着做员工手上那个任务，没有被“请先合并或放弃它”拒绝，条目最终完成
    assert!(!e.db.recent_events(500).unwrap().iter().any(|x| x.payload["text"].as_str().unwrap_or("").contains("请先合并或放弃")));
    assert_eq!(e.db.get_plan_item(&a).unwrap().unwrap().status, "done");
}

#[tokio::test]
async fn the_lead_shows_as_busy_while_ruling_issues_and_is_released_afterwards() {
    let e = setup().await;
    let bob = e.db.employee_by_name("bob").unwrap().unwrap();
    // 计划条目都派给 bob；alice（负责人）只会因为裁决议题而忙
    let tech = e.db.create_issue(&e.pid, "推送方案：SSE 还是 WebSocket", "选择推送方案", "technical", false, Some(&bob.id), None, None).unwrap();
    e.db.add_option(&tech.id, "SSE", "单向", None).unwrap();
    e.db.add_option(&tech.id, "WebSocket", "双向", None).unwrap();

    let snaps: Arc<std::sync::Mutex<Vec<Vec<String>>>> = Default::default();
    let rec = snaps.clone();
    let on_busy: Arc<dyn Fn(Vec<String>) + Send + Sync> = Arc::new(move |names| rec.lock().unwrap().push(names));
    let run = Autopilot::<Deny>::prepare(&e.orch, &e.pid, 100, 30).unwrap();
    let busy = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    let r = Autopilot::new_with_busy(e.orch.clone(), e.sched.clone(), Arc::new(Deny), Arc::new(AutopilotCtl::default()), &run, busy.clone(), on_busy).unwrap().run().await;
    assert_eq!(r.status, "finished", "{r:?}");

    let s = snaps.lock().unwrap();
    assert!(s.iter().any(|n| n.iter().any(|x| x == "alice")), "裁决议题期间负责人应显示为工作中：{s:?}");
    assert!(busy.lock().unwrap().is_empty(), "批次结束后没有人还被标记为忙");
}

#[tokio::test]
async fn calls_on_an_unlimited_endpoint_are_counted_for_statistics_but_do_not_use_up_the_budget() {
    use crewforge_core::endpoint::EndpointInput;
    let mk = |limit: bool| EndpointInput { name: format!("本地-{limit}"), base_url: "http://10.0.0.5:11434".into(), models: vec!["qwen3:27b".into()], extra_env: Default::default(), max_concurrency: 8, context_tokens: 0, limit_budget: limit, token: None };

    // 默认限制：员工连的端点限制预算 → 调用计入预算
    let e = setup().await;
    let ep = e.db.add_endpoint(&mk(true)).unwrap();
    for n in ["alice", "bob"] { let emp = e.db.employee_by_name(n).unwrap().unwrap(); e.db.set_employee_endpoint(&emp.id, Some(&ep.id), "").unwrap(); }
    let r = run(&e, 100, Arc::new(AutopilotCtl::default())).await;
    assert!(r.calls > 0 && r.uncounted_calls == 0, "限制的端点：计入预算 {r:?}");

    // 关掉限制：只统计，不计入预算，预算 3 次也能把整个计划做完
    let e = setup().await;
    let ep = e.db.add_endpoint(&mk(false)).unwrap();
    for n in ["alice", "bob", "carol"] { let emp = e.db.employee_by_name(n).unwrap().unwrap(); e.db.set_employee_endpoint(&emp.id, Some(&ep.id), "").unwrap(); }
    let r = run(&e, 3, Arc::new(AutopilotCtl::default())).await;
    assert_eq!((r.status.as_str(), r.calls), ("finished", 0), "预算只有 3 次，但这些调用不计入：{r:?}");
    assert!(r.uncounted_calls > 3, "调用照样统计：{r:?}");

    // 混合：只有订阅员工（carol，没连端点）的调用计入预算，其余只统计
    let e = setup().await;
    let ep = e.db.add_endpoint(&mk(false)).unwrap();
    for n in ["alice", "bob"] { let emp = e.db.employee_by_name(n).unwrap().unwrap(); e.db.set_employee_endpoint(&emp.id, Some(&ep.id), "").unwrap(); }
    let r = run(&e, 100, Arc::new(AutopilotCtl::default())).await;
    assert!(r.calls > 0 && r.uncounted_calls > 0, "混合项目：两边各记各的 {r:?}");
    let stored = e.db.get_autopilot_run(&r.id).unwrap().unwrap();
    assert_eq!(stored.uncounted_calls, r.uncounted_calls);
}

#[tokio::test]
async fn zero_minutes_means_no_time_limit() {
    let e = setup().await;
    let run = Autopilot::<Deny>::prepare(&e.orch, &e.pid, 100, 0).unwrap();
    assert_eq!(run.max_minutes, 0);
    let r = Autopilot::new(e.orch.clone(), e.sched.clone(), Arc::new(Deny), Arc::new(AutopilotCtl::default()), &run).unwrap().run().await;
    assert_eq!(r.status, "finished", "不限时间时不会因为时间预算停下：{r:?}");
    assert!(Autopilot::<Deny>::prepare(&e.orch, &e.pid, 0, 10).is_err(), "调用预算必须大于 0");
}

#[tokio::test]
async fn project_config_is_remembered_and_validated_and_drives_auto_continue_and_restart_decisions() {
    use crewforge_core::autopilot::{AutoContinue, AutopilotConfig, ResumeDecision};
    let e = setup().await;
    assert_eq!(e.db.autopilot_config(&e.pid).unwrap().max_calls, 30, "没设置过用默认值");
    let cfg = AutopilotConfig { max_calls: 40, max_minutes: 0, auto_continue: true, max_batches: 3, standup_blocks: true };
    assert!(!e.db.autopilot_config(&e.pid).unwrap().standup_blocks, "对齐会阻塞任务默认关（后台开）");
    e.db.set_autopilot_config(&e.pid, &cfg).unwrap();
    let got = e.db.autopilot_config(&e.pid).unwrap();
    assert_eq!((got.max_calls, got.max_minutes, got.auto_continue, got.max_batches), (40, 0, true, 3));
    e.db.set_autopilot_config(&e.pid, &AutopilotConfig { standup_blocks: false, ..cfg.clone() }).unwrap();
    assert!(!e.db.autopilot_config(&e.pid).unwrap().standup_blocks, "关掉后记住");
    e.db.set_autopilot_config(&e.pid, &cfg).unwrap();
    for bad in [AutopilotConfig { max_calls: 0, ..cfg.clone() }, AutopilotConfig { max_minutes: -1, ..cfg.clone() }, AutopilotConfig { max_batches: 0, ..cfg.clone() }] {
        assert!(e.db.set_autopilot_config(&e.pid, &bad).is_err());
    }

    // 预算正常用完 → 续；链到上限 → 停并说明；其他结束原因 → 不续
    let mk = |status: &str, chain: i64| { let mut r = e.db.create_autopilot_run_chained(&e.pid, 40, 0, chain).unwrap(); r.status = status.into(); r };
    assert_eq!(e.db.auto_continue_decision(&mk("budget", 0)).unwrap(), AutoContinue::Next { max_calls: 40, max_minutes: 0, chain: 1 });
    assert_eq!(e.db.auto_continue_decision(&mk("budget", 2)).unwrap(), AutoContinue::LimitReached { batches: 3 }, "第 3 个批次用完：到上限");
    for st in ["escalated", "finished", "stopped", "paused", "failed"] { assert_eq!(e.db.auto_continue_decision(&mk(st, 0)).unwrap(), AutoContinue::No, "{st} 不续"); }
    e.db.set_autopilot_config(&e.pid, &AutopilotConfig { auto_continue: false, ..cfg.clone() }).unwrap();
    assert_eq!(e.db.auto_continue_decision(&mk("budget", 0)).unwrap(), AutoContinue::No, "没开自动续");

    // 应用重启：剩余预算够就接着做；不够且开了自动续 → 新批次；不够没开 → 不动
    e.db.set_autopilot_config(&e.pid, &cfg).unwrap();
    let mut r = e.db.create_autopilot_run_chained(&e.pid, 40, 60, 0).unwrap();
    let later = r.started_at + 20 * 60_000;
    assert_eq!(e.db.resume_decision(&r, later).unwrap(), ResumeDecision::Resume { calls: 40, minutes: 40 });
    assert_eq!(e.db.resume_decision(&r, r.started_at + 30_000).unwrap(), ResumeDecision::Skip, "刚启动就被打断：不动（防崩溃循环）");
    r.calls = 39;
    assert_eq!(e.db.resume_decision(&r, later).unwrap(), ResumeDecision::FreshBatch { calls: 40, minutes: 0, chain: 1 }, "预算不够但开了自动续");
    e.db.set_autopilot_config(&e.pid, &AutopilotConfig { auto_continue: false, ..cfg.clone() }).unwrap();
    assert_eq!(e.db.resume_decision(&r, later).unwrap(), ResumeDecision::Skip);
    // 不限时间的批次：只看调用数
    let mut u = e.db.create_autopilot_run_chained(&e.pid, 40, 0, 0).unwrap();
    u.calls = 10;
    assert_eq!(e.db.resume_decision(&u, u.started_at + 500 * 60_000).unwrap(), ResumeDecision::Resume { calls: 30, minutes: 0 });
}

#[tokio::test]
async fn lead_can_skip_the_review_meeting_for_small_changes_and_reviews_otherwise() {
    // 负责人说不用评审：不开评审会，仍然合并，并留下原因
    let e = setup().await;
    std::fs::write(e._d.path().join("bin").join("review_skip"), "").unwrap();
    std::fs::write(e._d.path().join("bin").join("final_add_once"), "").unwrap();
    for id in &e.ids[1..] { e.db.set_plan_status(id, "cancelled", "测试").unwrap(); }
    let r = run(&e, 200, Arc::new(AutopilotCtl::default())).await;
    assert_eq!(r.status, "finished", "{r:?}");
    assert_eq!(e.db.list_meetings(&e.pid).unwrap().iter().filter(|m| m.title.starts_with("评审：")).count(), 0, "不应开评审会");
    assert_eq!(count_events(&e, "autopilot_step", "不开评审会：只改了一个小文件"), 1);
    assert!(e.db.recent_events(500).unwrap().iter().any(|x| x.kind == "review_decided" && x.payload["review"] == false));
    assert!(e.db.recent_events(500).unwrap().iter().any(|x| x.kind == "task_merged"));

    // 负责人说要评审：照常开会
    let e2 = setup().await;
    std::fs::write(e2._d.path().join("bin").join("final_add_once"), "").unwrap();
    for id in &e2.ids[1..] { e2.db.set_plan_status(id, "cancelled", "测试").unwrap(); }
    let r2 = run(&e2, 200, Arc::new(AutopilotCtl::default())).await;
    assert_eq!(r2.status, "finished", "{r2:?}");
    assert_eq!(e2.db.list_meetings(&e2.pid).unwrap().iter().filter(|m| m.title.starts_with("评审：")).count(), 1);
    assert_eq!(count_events(&e2, "autopilot_step", "开评审会：逻辑有改动"), 1);
}

#[tokio::test]
async fn lead_cancels_duplicate_items_at_batch_start_and_may_only_cancel() {
    let e = setup().await;
    std::fs::write(e._d.path().join("bin").join("final_add_once"), "").unwrap();
    let dup = e.db.create_plan_item(&e.pid, &NewPlanItem { title: "A".into(), description: String::new(), acceptance: String::new(), kind: "migrate".into(), module: String::new(), assignee_id: None, deps: vec![] }, None).unwrap();
    let r = run(&e, 200, Arc::new(AutopilotCtl::default())).await;
    assert_eq!(r.status, "finished", "{r:?}");
    assert_eq!(e.db.get_plan_item(&dup).unwrap().unwrap().status, "cancelled");
    assert!(e.db.get_plan_item(&dup).unwrap().unwrap().note.contains("与 #1 重复"));
    assert!(e.db.plan_view(&e.pid).unwrap().items.iter().all(|i| i.title != "夹带"), "清理时不能追加条目");
    assert_eq!(count_events(&e, "autopilot_step", "清理重复条目时只能取消"), 1);
    assert_eq!(count_events(&e, "autopilot_plan_changed", "取消 《A》"), 1);
}

#[tokio::test]
async fn background_housekeeping_is_deferred_while_ready_items_are_waiting() {
    let e = setup().await;
    // 四个互不依赖的条目，同时只允许 1 个并行，所以前面的合并时后面的一定还在等着派
    for t in ["D1", "D2", "D3"] {
        e.db.create_plan_item(&e.pid, &NewPlanItem { title: t.into(), description: String::new(), acceptance: String::new(), kind: "migrate".into(), module: String::new(), assignee_id: None, deps: vec![] }, None).unwrap();
    }
    std::fs::write(e._d.path().join("bin").join("final_add_once"), "").unwrap();
    let r = run(&e, 300, Arc::new(AutopilotCtl::default())).await;
    assert_eq!(r.status, "finished", "{r:?}");
    assert!(count_events(&e, "autopilot_step", "后台整理（记忆、行动项）先推迟") >= 1, "前面的条目合并时后面还有就绪的，应当推迟整理");
}

#[tokio::test]
async fn removing_a_member_returns_their_unstarted_work_to_unassigned_and_the_lead_assigns_it_by_role() {
    let e = setup().await;
    std::fs::write(e._d.path().join("bin").join("final_add_once"), "").unwrap();
    let carol = e.db.employee_by_name("carol").unwrap().unwrap().id;
    // C 指派给 carol；先看移出前提示用的数字
    assert_eq!(e.db.member_pending_work(&e.pid, &carol).unwrap(), (1, 0));
    assert_eq!(e.db.remove_member(&e.pid, &carol).unwrap(), (1, 0));
    let c = e.db.get_plan_item(&e.ids[2]).unwrap().unwrap();
    assert!(c.assignee_id.is_none() && c.note.contains("已移出项目"), "{c:?}");
    assert!(e.db.recent_events(200).unwrap().iter().any(|x| x.kind == "member_removed" && x.payload["plans"] == 1));
    assert_eq!(e.db.member_pending_work(&e.pid, &carol).unwrap(), (0, 0));
    // 批次照常做完，C 由剩下的人接
    let r = run(&e, 200, Arc::new(AutopilotCtl::default())).await;
    assert_eq!(r.status, "finished", "{r:?}");
    assert_eq!(e.db.get_plan_item(&e.ids[2]).unwrap().unwrap().status, "done");

    // 兜底：旧数据里指派的人已经不在项目里（之前移出时没清），也当作未指定，不会永远卡住
    let e2 = setup().await;
    std::fs::write(e2._d.path().join("bin").join("final_add_once"), "").unwrap();
    let carol2 = e2.db.employee_by_name("carol").unwrap().unwrap().id;
    rusqlite::Connection::open(e2._d.path().join("home").join("crewforge.db")).unwrap().execute("DELETE FROM project_members WHERE employee_id = ?1", [&carol2]).unwrap();
    assert!(e2.db.get_plan_item(&e2.ids[2]).unwrap().unwrap().assignee_id.is_some());
    let r2 = run(&e2, 200, Arc::new(AutopilotCtl::default())).await;
    assert_eq!(r2.status, "finished", "{r2:?}");
    assert_eq!(e2.db.get_plan_item(&e2.ids[2]).unwrap().unwrap().status, "done");
}

#[tokio::test]
async fn renaming_an_idle_employee_moves_the_worktree_and_updates_persona_and_task_paths() {
    let e = setup().await;
    std::fs::write(e._d.path().join("bin").join("final_add_once"), "").unwrap();
    let bob = e.db.employee_by_name("bob").unwrap().unwrap();
    e.db.update_employee_full(&bob.id, "全栈", "你是 bob，一名全栈工程师。bobby 不是 bob。", "claude", "").unwrap();
    let r = run(&e, 200, Arc::new(AutopilotCtl::default())).await;
    assert_eq!(r.status, "finished", "{r:?}");
    let conn = rusqlite::Connection::open(e._d.path().join("home").join("crewforge.db")).unwrap();
    let old_paths: Vec<String> = conn.prepare("SELECT DISTINCT worktree_path FROM tasks WHERE employee_id = ?1").unwrap().query_map([&bob.id], |r| r.get(0)).unwrap().flatten().collect();
    assert!(!old_paths.is_empty() && old_paths.iter().all(|p| p.ends_with("/bob") && std::path::Path::new(p).exists()), "{old_paths:?}");

    // 重名、非法名字被拒绝
    assert!(e.orch.rename_employee(&bob.id, "alice").await.unwrap_err().to_string().contains("已经有叫 alice"));
    assert!(e.orch.rename_employee(&bob.id, "a b").await.is_err());

    let rep = e.orch.rename_employee(&bob.id, "robert").await.unwrap();
    assert_eq!((rep.from.as_str(), rep.to.as_str()), ("bob", "robert"));
    assert!(rep.warnings.is_empty(), "{:?}", rep.warnings);
    assert_eq!(rep.moved.len(), old_paths.len());
    for p in &old_paths {
        assert!(!std::path::Path::new(p).exists(), "旧目录应已搬走：{p}");
        assert!(std::path::Path::new(&p.replace("/bob", "/robert")).exists());
    }
    let now: Vec<String> = conn.prepare("SELECT DISTINCT worktree_path FROM tasks WHERE employee_id = ?1").unwrap().query_map([&bob.id], |r| r.get(0)).unwrap().flatten().collect();
    assert!(now.iter().all(|p| p.ends_with("/robert")), "任务表里的路径同步更新：{now:?}");
    let after = e.db.employee_by_id(&bob.id).unwrap().unwrap();
    assert_eq!(after.name, "robert");
    assert_eq!(after.number, bob.number, "编号不变");
    assert_eq!(after.persona, "你是 robert，一名全栈工程师。bobby 不是 robert。", "只换整词");
    assert!(rep.persona_changed);
    assert!(e.db.employee_by_name("bob").unwrap().is_none());
    // git 登记的工作区也是新路径
    let list = String::from_utf8(Command::new("git").arg("-C").arg(&e.repo).args(["worktree", "list"]).output().unwrap().stdout).unwrap();
    assert!(list.contains("/robert") && !list.contains("/bob "), "{list}");
    assert!(e.db.recent_events(500).unwrap().iter().any(|x| x.kind == "employee_renamed" && x.payload["to"] == "robert"));
    // 改名后还能接着干活：再来一批新条目，由 robert 做
    let item = e.db.create_plan_item(&e.pid, &NewPlanItem { title: "改名后的新条目".into(), description: String::new(), acceptance: String::new(), kind: "migrate".into(), module: String::new(), assignee_id: Some(bob.id.clone()), deps: vec![] }, None).unwrap();
    let r2 = run(&e, 200, Arc::new(AutopilotCtl::default())).await;
    assert_eq!(r2.status, "finished", "{r2:?}");
    assert_eq!(e.db.get_plan_item(&item).unwrap().unwrap().status, "done");
}

#[tokio::test]
async fn unassigned_items_go_to_the_cheapest_employee_whose_cap_is_enough() {
    let e = setup().await;
    std::fs::write(e._d.path().join("bin").join("final_add_once"), "").unwrap();
    for id in &e.ids { e.db.set_plan_status(id, "cancelled", "测试").unwrap(); }
    let who = |n: &str| e.db.employee_by_name(n).unwrap().unwrap().id;
    e.db.set_employee_level_cap(&who("alice"), 5).unwrap();
    e.db.set_employee_level_cap(&who("bob"), 2).unwrap();
    e.db.set_employee_level_cap(&who("carol"), 3).unwrap();
    let mk = |t: &str, lvl: i64| {
        let id = e.db.create_plan_item(&e.pid, &NewPlanItem { title: t.into(), description: String::new(), acceptance: String::new(), kind: "migrate".into(), module: String::new(), assignee_id: None, deps: vec![] }, None).unwrap();
        e.db.set_plan_item_level(&id, lvl).unwrap();
        id
    };
    let (x1, x4, x5) = (mk("X1 机械改动", 1), mk("X4 跨模块特性", 4), mk("X5 架构取舍", 5));
    let r = run(&e, 300, Arc::new(AutopilotCtl::default())).await;
    assert_eq!(r.status, "finished", "{r:?}");
    let doer = |id: &str| {
        let t = e.db.get_plan_item(id).unwrap().unwrap().task_id.unwrap();
        e.db.employee_by_id(&e.db.get_task(&t).unwrap().unwrap().employee_id).unwrap().unwrap().name
    };
    assert_eq!(doer(&x1), "bob", "1 级：够用的最低档（上限 2），不占用 alice");
    assert_eq!(doer(&x4), "alice", "4 级：只有 alice（上限 5）够格");
    assert_eq!(doer(&x5), "alice", "5 级：同样只有 alice，等她空出来");
}

#[tokio::test]
async fn when_nobody_is_capable_the_strongest_free_employee_tries_anyway() {
    let e = setup().await;
    std::fs::write(e._d.path().join("bin").join("final_add_once"), "").unwrap();
    for id in &e.ids { e.db.set_plan_status(id, "cancelled", "测试").unwrap(); }
    for (n, c) in [("alice", 2), ("bob", 3), ("carol", 1)] { e.db.set_employee_level_cap(&e.db.employee_by_name(n).unwrap().unwrap().id, c).unwrap(); }
    let id = e.db.create_plan_item(&e.pid, &NewPlanItem { title: "超出所有人能力的活".into(), description: String::new(), acceptance: String::new(), kind: "migrate".into(), module: String::new(), assignee_id: None, deps: vec![] }, None).unwrap();
    e.db.set_plan_item_level(&id, 5).unwrap();
    let r = run(&e, 300, Arc::new(AutopilotCtl::default())).await;
    assert_eq!(r.status, "finished", "{r:?}");
    let t = e.db.get_plan_item(&id).unwrap().unwrap().task_id.unwrap();
    assert_eq!(e.db.employee_by_id(&e.db.get_task(&t).unwrap().unwrap().employee_id).unwrap().unwrap().name, "bob", "能力最高的 bob（3）试试");
    assert!(count_events(&e, "autopilot_step", "项目里没有人的能力上限够") >= 1);
}


/// 项目里加一位架构师，清掉 setup 里的条目，项目目标写好（负责人起草计划需要）。
fn with_architect(e: &Env) -> String {
    for id in &e.ids { e.db.set_plan_status(id, "cancelled", "测试").unwrap(); }
    e.db.set_project_goal(&e.pid, "做一个小工具").unwrap();
    std::fs::write(e._d.path().join("bin").join("final_add_once"), "").unwrap();
    let arch = e.db.add_employee("arch", "架构师", "你是 arch").unwrap();
    e.db.add_member(&e.pid, &arch.id, "architect").unwrap();
    arch.id
}

#[tokio::test]
async fn with_an_architect_design_comes_first_then_memories_are_published_and_the_lead_plans_from_the_design() {
    let e = setup().await;
    let arch = with_architect(&e);
    // 空计划也能开始：有架构师就先建设计条目
    let r = run(&e, 400, Arc::new(AutopilotCtl::default())).await;
    assert_eq!(r.status, "finished", "{r:?}");
    let v = e.db.plan_view(&e.pid).unwrap();
    let design: Vec<_> = v.items.iter().filter(|i| i.kind == "design").collect();
    assert_eq!(design.len(), 2);
    assert!(design.iter().all(|i| i.status == "done"), "设计条目都做完");
    // 设计条目由架构师做；实现条目一个都不给架构师
    let doer = |id: &str| { let t = e.db.get_plan_item(id).unwrap().unwrap().task_id.unwrap(); e.db.get_task(&t).unwrap().unwrap().employee_id };
    assert!(design.iter().all(|i| doer(&i.id) == arch));
    let imp: Vec<_> = v.items.iter().filter(|i| i.kind != "design" && i.status != "cancelled").collect();
    assert!(imp.len() >= 3 && imp.iter().all(|i| i.status == "done" && doer(&i.id) != arch), "负责人按设计起草了实现条目，都做完了，没有给架构师");
    // 设计先于实现：第一个实现条目开始之前，两个设计条目都已合并
    let events = e.db.recent_events(2000).unwrap();
    let pos = |pred: &dyn Fn(&crewforge_core::db::EventRow) -> bool| events.iter().position(|x| pred(x));
    let text = |x: &crewforge_core::db::EventRow| x.payload["text"].as_str().unwrap_or("").to_string();
    let last_design_merge = events.iter().rposition(|x| x.kind == "autopilot_merged" && (text(x).contains("架构设计") || text(x).contains("文档与代码规范"))).unwrap();
    let first_impl_start = pos(&|x| x.kind == "autopilot_step" && text(x).starts_with("派《搭脚手架》")).unwrap();
    // recent_events 按时间正序：下标小的更早
    assert!(last_design_merge < first_impl_start, "设计合并（{last_design_merge}）应早于实现开始（{first_impl_start}）");
    // 设计要点写进了项目记忆（规范类、已验证、来源架构师）
    let mems = e.db.list_memories(&crewforge_core::memory::MemoryFilter { layer: Some("project".into()), project_id: Some(e.pid.clone()), ..Default::default() }).unwrap();
    let arch_mems: Vec<_> = mems.iter().filter(|m| m.source_kind == "architect").collect();
    assert_eq!(arch_mems.len(), 2, "{mems:?}");
    assert!(arch_mems.iter().all(|m| m.kind == "convention" && m.status == "verified"));
    assert!(arch_mems.iter().any(|m| m.content.contains("模块之间只通过接口通信")) && arch_mems.iter().any(|m| m.content.contains("先写测试")));
    assert_eq!(count_events(&e, "autopilot_plan_changed", "按设计文档起草了"), 1);
}

#[tokio::test]
async fn other_items_wait_for_the_design_and_design_items_may_only_change_docs() {
    // 1) 已有实现条目的项目里加架构师：实现条目要等设计做完
    let e = setup().await;
    let arch = e.db.add_employee("arch", "架构师", "你是 arch").unwrap();
    e.db.add_member(&e.pid, &arch.id, "architect").unwrap();
    std::fs::write(e._d.path().join("bin").join("final_add_once"), "").unwrap();
    e.db.create_design_items(&e.pid, false).unwrap();
    let waiting = e.db.plan_waiting(&e.pid, &Default::default(), true).unwrap();
    assert!(waiting[&e.ids[0]].iter().any(|w| w.contains("等架构师完成设计")), "{:?}", waiting[&e.ids[0]]);
    let r = run(&e, 400, Arc::new(AutopilotCtl::default())).await;
    assert_eq!(r.status, "finished", "{r:?}");
    let events = e.db.recent_events(2000).unwrap();
    let text = |x: &crewforge_core::db::EventRow| x.payload["text"].as_str().unwrap_or("").to_string();
    let first_old_start = events.iter().position(|x| x.kind == "autopilot_step" && text(x).starts_with("派《A》")).unwrap();
    let last_design_merge = events.iter().rposition(|x| x.kind == "autopilot_merged" && (text(x).contains("架构设计") || text(x).contains("文档与代码规范"))).unwrap();
    assert!(last_design_merge < first_old_start, "已有的条目 A 也要等设计合并之后才开始");

    // 2) 设计条目改了代码文件：验证不过，架构师被要求撤销；做不到就阻塞
    let e2 = setup().await;
    with_architect(&e2);
    std::fs::write(e2._d.path().join("bin").join("bad_doc"), "").unwrap();
    let r2 = run(&e2, 400, Arc::new(AutopilotCtl::default())).await;
    assert_ne!(r2.status, "finished", "{r2:?}");
    let d1 = e2.db.plan_view(&e2.pid).unwrap().items.into_iter().find(|i| i.kind == "design" && i.title.contains("架构设计")).unwrap();
    assert_ne!(d1.status, "done", "改了代码文件的设计条目不能算完成：{d1:?}");
    assert!(count_events(&e2, "autopilot_step", "设计类条目只能改文档，但改了这些非文档文件：main.go") >= 1, "验证失败要说明改了哪些非文档文件");
    assert!(!e2.repo.join("main.go").exists(), "代码文件没有合并进主分支");
}


#[tokio::test]
async fn handing_over_the_lead_role_summarises_first_then_moves_the_role_and_gives_the_new_lead_a_one_shot_note() {
    let e = setup().await;
    std::fs::write(e._d.path().join("bin").join("final_add_once"), "").unwrap();
    // 先让项目跑一批，alice（负责人）在这个项目里有了会话
    let r = run(&e, 300, Arc::new(AutopilotCtl::default())).await;
    assert_eq!(r.status, "finished", "{r:?}");
    let who = |n: &str| e.db.employee_by_name(n).unwrap().unwrap();
    let (alice, bob) = (who("alice"), who("bob"));
    let newlead = e.db.add_employee("newlead", "全栈", "你是 newlead").unwrap();
    // 给 alice 名下留一个没开始的条目
    let mine = e.db.create_plan_item(&e.pid, &NewPlanItem { title: "alice 的后续活".into(), description: String::new(), acceptance: String::new(), kind: "migrate".into(), module: String::new(), assignee_id: Some(alice.id.clone()), deps: vec![] }, None).unwrap();

    // alice 手上还有一个进行中的任务：交接不会被它拦住，而是自动放弃，并写进给新人的说明
    e.orch.assign(&alice, &e.repo, "做点还没做完的事", &Deny, &crewforge_core::runner::NoopObserver).await.unwrap();
    let active = e.db.active_task(&alice.id, e.repo.to_str().unwrap()).unwrap().expect("alice 有进行中的任务");

    // 1) 准备：快照 + 原员工自己写的摘要
    let prep = e.orch.handover_prepare(&e.pid, &alice.id, &newlead.id, "lead", true).await.unwrap();
    assert!(prep.snapshot.contains("交出的角色：负责人") && prep.snapshot.contains("alice 的后续活"), "{}", prep.snapshot);
    assert!(prep.snapshot.contains("进行中的任务（交接时会自动放弃）") && prep.snapshot.contains(&active.branch), "{}", prep.snapshot);
    assert!(prep.warnings.iter().any(|w| w.contains("交接时会自动放弃")), "{:?}", prep.warnings);
    assert!(prep.model_summary.as_deref().unwrap_or("").contains("正在推进脚手架"), "{prep:?}");
    // 没有要摘要时只有快照
    let prep2 = e.orch.handover_prepare(&e.pid, &alice.id, &newlead.id, "lead", false).await.unwrap();
    assert!(prep2.model_summary.is_none() && prep2.summary_note.is_empty());

    // 2) 执行：newlead 不在项目里，直接加入并接任；alice 保留为开发；名下没开始的活一起交过去
    let text = format!("{}\n\n{}", prep.snapshot, prep.model_summary.unwrap());
    let rep = e.orch.handover_execute(&e.pid, &alice.id, &newlead.id, "lead", &text, "keep", "move").await.unwrap();
    assert_eq!((rep.moved_plan_items, rep.from.as_str(), rep.to.as_str()), (1, "alice", "newlead"));
    assert_eq!(rep.abandoned.len(), 1, "{:?}", rep.abandoned);
    assert_eq!(e.db.get_task(&active.id).unwrap().unwrap().status, "abandoned");
    assert!(e.db.active_task(&alice.id, e.repo.to_str().unwrap()).unwrap().is_none());
    let branch_exists = Command::new("git").arg("-C").arg(&e.repo).args(["rev-parse", "--verify", &active.branch]).status().unwrap().success();
    assert!(branch_exists, "分支保留，WIP 提交在上面");
    let members = e.db.list_members(&e.pid).unwrap();
    let role = |id: &str| members.iter().find(|m| m.employee_id == id).map(|m| m.role.clone());
    assert_eq!((role(&newlead.id).as_deref(), role(&alice.id).as_deref(), role(&bob.id).as_deref()), (Some("lead"), Some("developer"), Some("developer")));
    assert_eq!(e.db.get_plan_item(&mine).unwrap().unwrap().assignee_id.as_deref(), Some(newlead.id.as_str()));
    assert!(e.db.recent_events(2000).unwrap().iter().any(|x| x.kind == "role_handover" && x.payload["to"] == "newlead"));
    let mems = e.db.list_memories(&crewforge_core::memory::MemoryFilter { layer: Some("project".into()), project_id: Some(e.pid.clone()), ..Default::default() }).unwrap();
    assert!(mems.iter().any(|m| m.source_kind == "handover" && m.title.contains("alice → newlead")));

    // 3) 新负责人下一次被调用时，提示词最前面带着交接说明，用完即清
    let seen = e._d.path().join("bin").join("handover_seen");
    assert!(!seen.exists());
    e.db.create_plan_item(&e.pid, &NewPlanItem { title: "交接后的条目".into(), description: String::new(), acceptance: String::new(), kind: "migrate".into(), module: String::new(), assignee_id: Some(bob.id.clone()), deps: vec![] }, None).unwrap();
    let r2 = run(&e, 300, Arc::new(AutopilotCtl::default())).await;
    assert_eq!(r2.status, "finished", "{r2:?}");
    let line = std::fs::read_to_string(&seen).expect("新负责人的某次调用带着交接说明");
    assert!(line.contains("alice 把这个项目的「负责人」交接给了你") && line.contains("正在推进脚手架"), "{line}");
    assert!(line.contains("交接时放弃的进行中任务") && line.contains(&active.branch), "新人的说明里有放弃的任务和分支：{line}");
    assert!(e.db.take_handover_note(&e.pid, &newlead.id).is_none(), "已经被取走了");

    // 4) 再交接一次，这回原员工移出项目：他名下没开始的条目改回未指定（没选一起交接）
    let extra = e.db.create_plan_item(&e.pid, &NewPlanItem { title: "bob 的后续活".into(), description: String::new(), acceptance: String::new(), kind: "migrate".into(), module: String::new(), assignee_id: Some(bob.id.clone()), deps: vec![] }, None).unwrap();
    let rep2 = e.orch.handover_execute(&e.pid, &bob.id, &alice.id, "developer", "bob 交接给 alice", "remove", "keep").await.unwrap();
    assert_eq!(rep2.moved_plan_items, 0);
    assert!(!e.db.is_member(&e.pid, &bob.id).unwrap());
    assert!(e.db.get_plan_item(&extra).unwrap().unwrap().assignee_id.is_none(), "没一起交接的，移出时改回未指定");
}

#[tokio::test]
async fn handing_over_abandons_a_plan_linked_task_and_the_item_goes_back_to_the_new_owner_with_a_note() {
    let e = setup().await;
    let who = |n: &str| e.db.employee_by_name(n).unwrap().unwrap();
    let (bob, alice) = (who("bob"), who("alice"));
    // A 指派给 bob：派给他做，做完第一轮但还没合并（进行中）
    e.orch.dispatch_plan_item(&e.ids[0], Some(&bob.id), &Deny, &crewforge_core::runner::NoopObserver).await.unwrap();
    let a = e.db.get_plan_item(&e.ids[0]).unwrap().unwrap();
    assert_eq!(a.status, "in_progress");
    let task = e.db.get_task(a.task_id.as_deref().unwrap()).unwrap().unwrap();

    // bob 的开发角色交给 alice，bob 移出项目，名下没开始的活一起交过去
    let rep = e.orch.handover_execute(&e.pid, &bob.id, &alice.id, "developer", "bob 交接给 alice", "remove", "move").await.unwrap();
    assert_eq!(rep.abandoned.len(), 1);
    assert_eq!(e.db.get_task(&task.id).unwrap().unwrap().status, "abandoned");
    let a = e.db.get_plan_item(&e.ids[0]).unwrap().unwrap();
    assert_eq!((a.status.as_str(), a.assignee_id.as_deref()), ("planned", Some(alice.id.as_str())), "条目回到待开始，归新人");
    assert!(a.note.contains("原由 bob 做到一半") && a.note.contains(&task.branch), "{}", a.note);
    assert!(!e.db.is_member(&e.pid, &bob.id).unwrap());
}


#[tokio::test]
async fn after_a_lead_handover_the_new_lead_reviews_the_unstarted_plan_once_in_the_first_batch() {
    let e = setup().await;
    std::fs::write(e._d.path().join("bin").join("final_add_once"), "").unwrap();
    let who = |n: &str| e.db.employee_by_name(n).unwrap().unwrap();
    let (alice, bob) = (who("alice"), who("bob"));
    let newlead = e.db.add_employee("newlead", "全栈", "你是 newlead").unwrap();
    let mine = e.db.create_plan_item(&e.pid, &NewPlanItem { title: "alice 的活".into(), description: String::new(), acceptance: String::new(), kind: "migrate".into(), module: String::new(), assignee_id: Some(alice.id.clone()), deps: vec![] }, None).unwrap();

    // 交接负责人：名下还没开始的活改回「未指定」
    let rep = e.orch.handover_execute(&e.pid, &alice.id, &newlead.id, "lead", "交接", "keep", "unassign").await.unwrap();
    assert_eq!((rep.unassigned_plan_items, rep.moved_plan_items), (1, 0));
    let item = e.db.get_plan_item(&mine).unwrap().unwrap();
    assert!(item.assignee_id.is_none() && item.note.contains("改回未指定"), "{item:?}");
    assert!(e.orch.handover_execute(&e.pid, &alice.id, &newlead.id, "lead", "x", "keep", "bogus").await.is_err());

    // 第一个批次：新负责人先做接手复盘（定级 + 改派），多余的追加被拒绝
    let r = run(&e, 300, Arc::new(AutopilotCtl::default())).await;
    assert_eq!(r.status, "finished", "{r:?}");
    assert_eq!(count_events(&e, "autopilot_step", "刚接任负责人：先复盘"), 1);
    assert_eq!(e.db.get_plan_item(&e.ids[0]).unwrap().unwrap().level, 2, "A 定为 2 级");
    let c_task = e.db.get_plan_item(&e.ids[2]).unwrap().unwrap().task_id.unwrap();
    assert_eq!(e.db.get_task(&c_task).unwrap().unwrap().employee_id, bob.id, "C 改派给 bob 做");
    assert_eq!(count_events(&e, "autopilot_step", "接手复盘时只能定级和改派"), 1);
    assert_eq!(count_events(&e, "autopilot_plan_changed", "定级 《A》→ L2"), 1);
    assert!(e.db.plan_view(&e.pid).unwrap().items.iter().all(|i| i.title != "夹带"));

    // 只做这一次：再来一批不再触发
    e.db.create_plan_item(&e.pid, &NewPlanItem { title: "之后的条目".into(), description: String::new(), acceptance: String::new(), kind: "migrate".into(), module: String::new(), assignee_id: Some(bob.id.clone()), deps: vec![] }, None).unwrap();
    let r2 = run(&e, 300, Arc::new(AutopilotCtl::default())).await;
    assert_eq!(r2.status, "finished", "{r2:?}");
    assert_eq!(count_events(&e, "autopilot_step", "刚接任负责人：先复盘"), 1, "第二个批次不再复盘");
}
