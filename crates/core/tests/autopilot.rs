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
case "$line" in
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
"###.replace("@MARK@", &dir.join("final_add_once").to_string_lossy()).replace("@ACTIONS@", actions);
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
