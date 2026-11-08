//! 评审行动项与合并：合并门、明确确认后合并、把行动项派给员工处理。真实 git + 假 CLI。

use crewforge_core::db::Db;
use crewforge_core::orchestrator::Orchestrator;
use crewforge_core::provider::claude::{ApprovalRequest, Decision};
use crewforge_core::runner::{Approver, NoopObserver};
use crewforge_core::workspace::{MergeOutcome, WorkspaceManager};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

struct Deny;
impl Approver for Deny {
    async fn ask(&self, _: &str, _: &ApprovalRequest, _: &str) -> Decision { Decision::Deny("x".into()) }
}

fn sh(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// 假 CLI：记录收到的提示，并在工作目录写一个文件，使任务有改动可合并。
fn cli(dir: &Path, name: &str, file: &str) -> (String, PathBuf) {
    let log = dir.join(format!("{name}.prompts"));
    let path = dir.join(name);
    std::fs::write(&path, format!(
        "#!/bin/sh\nread -r line\necho '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"x\"}}'\nprintf '%s\\n' \"$line\" >> '{}'\necho work >> {file}\necho '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"ok\",\"usage\":{{}}}}'\n",
        log.display())).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (path.to_string_lossy().into_owned(), log)
}

struct Env { _d: tempfile::TempDir, repo: PathBuf, tools: PathBuf, db: Arc<Db>, orch: Orchestrator, pid: String }

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
    let pid = db.create_project("P", "", repo.to_str().unwrap()).unwrap().id;
    Env { _d: data, repo, tools, db: db.clone(), orch: Orchestrator::new(db, ws), pid }
}

const MINUTES: &str = "## 行动项\n- [合并前] alice：把 to.balance += amount 改成 checked_add\n- [之后] bob：单开 PR 修透支\n- [合并前] 补一个边界测试";

/// alice 完成了一个任务，并有一场评审它的会议、纪要里有行动项。
async fn reviewed_task(e: &mut Env) -> (crewforge_core::db::Employee, crewforge_core::db::Employee, String, String) {
    let alice = e.db.add_employee("alice", "全栈", "p").unwrap();
    let bob = e.db.add_employee("bob", "全栈", "p").unwrap();
    e.db.add_member(&e.pid, &alice.id, "developer").unwrap();
    e.db.add_member(&e.pid, &bob.id, "reviewer").unwrap();
    let (program, _) = cli(&e.tools, "work", "feature.txt");
    e.orch.program = program;
    e.orch.assign(&alice, &e.repo, "实现 transfer", &Deny, &NoopObserver).await.unwrap();
    let task = e.orch.task_views(&e.repo).await.unwrap().remove(0).task;
    let m = e.db.create_meeting(&e.pid, "code_review", "评审：transfer", None, Some(&task.id), &[], 5).unwrap();
    e.db.set_minutes(&m.id, MINUTES).unwrap();
    (alice, bob, task.id, m.id)
}

#[tokio::test]
async fn open_blocking_items_stop_the_merge_until_a_human_resolves_them() {
    let mut e = setup().await;
    let (_a, _b, task, meeting) = reviewed_task(&mut e).await;
    let head = sh(&e.repo, &["rev-parse", "HEAD"]);

    let out = e.orch.merge_task(&task).await.unwrap();
    let MergeOutcome::OpenActionItems { items } = &out else { panic!("应被行动项拦住：{out:?}") };
    assert_eq!(items.len(), 2, "两条合并前（含没写标签的），[之后] 不算");
    assert!(items.iter().any(|t| t.contains("checked_add")) && items.iter().any(|t| t.contains("边界测试")));
    assert_eq!(sh(&e.repo, &["rev-parse", "HEAD"]), head, "什么都没合并");
    assert_eq!(e.db.get_task(&task).unwrap().unwrap().status, "active");
    assert!(e.db.recent_events_for_project(&e.pid, 100).unwrap().iter().any(|x| x.kind == "task_merge_blocked" && x.payload["kind"] == "action_items"));

    // 任务卡能看到这些条目及其状态
    let view = e.orch.task_views(&e.repo).await.unwrap().remove(0);
    assert_eq!(view.reviews[0].items.len(), 3);

    // 人类逐条处理：一条已处理，一条驳回（写原因）→ 放行；“之后”那条一直是待处理，也不影响
    let acts = e.db.actions_for_meeting(&meeting).unwrap();
    let id = |t: &str| acts.iter().find(|a| a.text.contains(t)).unwrap().id.clone();
    e.db.set_action_status(&id("checked_add"), "done", "").unwrap();
    assert!(matches!(e.orch.merge_task(&task).await.unwrap(), MergeOutcome::OpenActionItems { items } if items.len() == 1));
    e.db.set_action_status(&id("边界测试"), "dismissed", "不需要，已有覆盖").unwrap();
    let out = e.orch.merge_task(&task).await.unwrap();
    assert!(matches!(out, MergeOutcome::Merged { .. }), "{out:?}");
    assert_eq!(e.db.get_action(&id("透支")).unwrap().unwrap().status, "open", "后续事项仍在，等人跟进");
}

#[tokio::test]
async fn merging_anyway_requires_explicit_acknowledgement_and_leaves_a_record() {
    let mut e = setup().await;
    let (_a, _b, task, _m) = reviewed_task(&mut e).await;
    let out = e.orch.merge_task_ack(&task, true).await.unwrap();
    assert!(matches!(out, MergeOutcome::Merged { .. }), "{out:?}");
    let ev = e.db.recent_events_for_project(&e.pid, 200).unwrap().into_iter().find(|x| x.kind == "merged_with_open_items").expect("留下明知有未处理项仍合并的记录");
    assert_eq!(ev.payload["items"].as_array().unwrap().len(), 2);
    assert_eq!(ev.payload["task_id"], task.as_str());
}

#[tokio::test]
async fn a_task_without_a_review_merges_as_before() {
    let mut e = setup().await;
    let alice = e.db.add_employee("alice", "全栈", "p").unwrap();
    e.db.add_member(&e.pid, &alice.id, "developer").unwrap();
    let (program, _) = cli(&e.tools, "work", "f.txt");
    e.orch.program = program;
    e.orch.assign(&alice, &e.repo, "x", &Deny, &NoopObserver).await.unwrap();
    let task = e.orch.task_views(&e.repo).await.unwrap().remove(0).task;
    assert!(matches!(e.orch.merge_task(&task.id).await.unwrap(), MergeOutcome::Merged { .. }));
}

#[tokio::test]
async fn dispatching_an_item_continues_the_same_task_and_keeps_blocking_until_a_human_confirms() {
    let mut e = setup().await;
    let (alice, _b, task, meeting) = reviewed_task(&mut e).await;
    let item = e.db.actions_for_meeting(&meeting).unwrap().into_iter().find(|a| a.text.contains("checked_add")).unwrap();
    let (program, log) = cli(&e.tools, "fix", "fix.txt");
    e.orch.program = program;

    e.orch.dispatch_action_item(&item.id, None, &Deny, &NoopObserver).await.unwrap();
    let prompts = std::fs::read_to_string(&log).unwrap();
    assert!(prompts.contains("评审行动项") && prompts.contains("checked_add") && prompts.contains("评审：transfer"), "提示里有条目和会议名");
    // 在同一个任务、同一个分支里继续：没有新任务，改动在 alice 原来的 worktree
    let views = e.orch.task_views(&e.repo).await.unwrap();
    assert_eq!(views.len(), 1, "没有另起任务");
    assert!(views[0].files.iter().any(|f| f.path == "fix.txt"));
    let after = e.db.get_action(&item.id).unwrap().unwrap();
    assert_eq!((after.status.as_str(), after.work_task_id.as_deref()), ("working", Some(task.as_str())));
    assert_eq!(e.db.employee_by_id(&e.db.get_task(&task).unwrap().unwrap().employee_id).unwrap().unwrap().id, alice.id);

    // 员工改完不等于解决了：处理中仍然拦合并，要人确认
    assert!(matches!(e.orch.merge_task(&task).await.unwrap(), MergeOutcome::OpenActionItems { items } if items.len() == 2));
    e.db.set_action_status(&item.id, "done", "").unwrap();
    assert!(matches!(e.orch.merge_task(&task).await.unwrap(), MergeOutcome::OpenActionItems { items } if items.len() == 1));
    // 已处理的不能再派
    assert!(e.orch.prepare_action_dispatch(&item.id, None).unwrap_err().to_string().contains("已处理"));
}

#[tokio::test]
async fn dispatch_rules_review_comments_stay_with_the_task_owner() {
    let mut e = setup().await;
    let (_a, bob, task, meeting) = reviewed_task(&mut e).await;
    let acts = e.db.actions_for_meeting(&meeting).unwrap();
    let by = |t: &str| acts.iter().find(|a| a.text.contains(t)).unwrap().clone();

    // 对 alice 进行中任务的评审意见，不能交给 bob（修复会落到别的分支，被评审的代码并没有变）
    let err = e.orch.prepare_action_dispatch(&by("checked_add").id, Some(&bob.id)).unwrap_err().to_string();
    assert!(err.contains("必须由 alice"), "{err}");
    // 没有负责人又没指定：要求先选人
    let err = e.orch.prepare_action_dispatch(&by("边界测试").id, None).unwrap_err().to_string();
    assert!(err.contains("没有指定负责人"), "{err}");
    // 指定的人不是在职成员
    let outsider = e.db.add_employee("zed", "全栈", "p").unwrap();
    assert!(e.orch.prepare_action_dispatch(&by("边界测试").id, Some(&outsider.id)).unwrap_err().to_string().contains("不是本项目的在职成员"));
    // 没有负责人的条目，可以现场指定给任务的负责人
    assert!(e.orch.prepare_action_dispatch(&by("边界测试").id, Some(&e.db.get_task(&task).unwrap().unwrap().employee_id)).is_ok());

    // 任务合并之后，遗留的后续事项可以交给别人另开任务跟进
    e.db.set_action_status(&by("checked_add").id, "done", "").unwrap();
    e.db.set_action_status(&by("边界测试").id, "done", "").unwrap();
    assert!(matches!(e.orch.merge_task(&task).await.unwrap(), MergeOutcome::Merged { .. }));
    let (program, _) = cli(&e.tools, "follow", "followup.txt");
    e.orch.program = program;
    e.orch.dispatch_action_item(&by("透支").id, None, &Deny, &NoopObserver).await.unwrap();
    let views = e.orch.task_views(&e.repo).await.unwrap();
    let new_task = views.iter().find(|v| v.employee_name == "bob" && v.task.status == "active").expect("bob 新开了一个跟进任务");
    assert_eq!(e.db.get_action(&by("透支").id).unwrap().unwrap().work_task_id.as_deref(), Some(new_task.task.id.as_str()));
}
