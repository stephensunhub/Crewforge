//! 计划派活端到端：依赖门、派活建任务、合并后自动完成、放弃后回到待开始、目标注入。真实 git + 假 CLI。

use crewforge_core::db::Db;
use crewforge_core::orchestrator::Orchestrator;
use crewforge_core::plan::NewPlanItem;
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

fn item(title: &str, deps: &[&str]) -> NewPlanItem {
    NewPlanItem { title: title.into(), description: format!("{title}的说明"), acceptance: format!("{title}的验收：cargo test 通过"), module: "engine".into(),
        deps: deps.iter().map(|s| s.to_string()).collect(), ..Default::default() }
}

#[tokio::test]
async fn dispatch_respects_dependencies_one_task_per_employee_and_completes_on_merge() {
    let mut e = setup().await;
    let alice = e.db.add_employee("alice", "全栈", "p").unwrap();
    let bob = e.db.add_employee("bob", "全栈", "p").unwrap();
    for x in [&alice, &bob] { e.db.add_member(&e.pid, &x.id, "developer").unwrap(); }
    e.db.set_project_goal(&e.pid, "用 Rust 重写核心逻辑，业务行为必须保持不变").unwrap();
    let a = e.db.create_plan_item(&e.pid, &item("引擎", &[]), None).unwrap();
    let b = e.db.create_plan_item(&e.pid, &item("界面", &[&a]), None).unwrap();
    let c = e.db.create_plan_item(&e.pid, &item("另一个独立条目", &[]), None).unwrap();
    let (program, log) = cli(&e.tools, "work", "out.txt");
    e.orch.program = program;

    // 前置没完成，依赖它的不能开始；没有负责人又没指定也不行
    let err = e.orch.prepare_plan_dispatch(&b, Some(&bob.id)).unwrap_err().to_string();
    assert!(err.contains("还要等前置条目完成：引擎"), "{err}");
    assert!(e.orch.prepare_plan_dispatch(&a, None).unwrap_err().to_string().contains("没有指定负责人"));

    // 派给 alice：新建任务，条目进行中并关联任务；提示里有说明、验收标准，并带上项目目标
    e.orch.dispatch_plan_item(&a, Some(&alice.id), &Deny, &NoopObserver).await.unwrap();
    let prompt = std::fs::read_to_string(&log).unwrap();
    for needle in ["[项目目标与硬性约束]", "业务行为必须保持不变", "[计划条目]", "引擎的说明", "引擎的验收：cargo test 通过", "所属模块：engine"] {
        assert!(prompt.contains(needle), "提示里缺少 {needle}");
    }
    let it = e.db.get_plan_item(&a).unwrap().unwrap();
    assert_eq!(it.status, "in_progress");
    let task = e.db.get_task(it.task_id.as_deref().unwrap()).unwrap().unwrap();
    assert_eq!((task.employee_id.as_str(), task.status.as_str()), (alice.id.as_str(), "active"));

    // alice 手上有进行中的任务：不能再派新条目给她（bob 可以，因为 c 没有依赖）
    let err = e.orch.prepare_plan_dispatch(&c, Some(&alice.id)).unwrap_err().to_string();
    assert!(err.contains("还有进行中的任务"), "{err}");
    assert!(e.orch.prepare_plan_dispatch(&c, Some(&bob.id)).is_ok());
    // 进行中的条目不能重复派
    assert!(e.orch.prepare_plan_dispatch(&a, Some(&bob.id)).unwrap_err().to_string().contains("进行中"));

    // 合并后：条目自动完成，依赖它的条目变为可开始
    assert!(matches!(e.orch.merge_task(&task.id).await.unwrap(), MergeOutcome::Merged { .. }));
    assert_eq!(e.db.get_plan_item(&a).unwrap().unwrap().status, "done");
    assert!(e.db.get_plan_item(&b).unwrap().unwrap().ready);
    assert_eq!(e.db.plan_view(&e.pid).unwrap().percent_done, 33);
    e.orch.dispatch_plan_item(&b, Some(&bob.id), &Deny, &NoopObserver).await.unwrap();
    assert_eq!(e.db.get_plan_item(&b).unwrap().unwrap().status, "in_progress");
}

#[tokio::test]
async fn abandoning_the_task_puts_the_item_back_and_goal_is_only_sent_with_a_fresh_task() {
    let mut e = setup().await;
    let alice = e.db.add_employee("alice", "全栈", "p").unwrap();
    e.db.add_member(&e.pid, &alice.id, "developer").unwrap();
    e.db.set_project_goal(&e.pid, "目标：业务不变").unwrap();
    let a = e.db.create_plan_item(&e.pid, &item("引擎", &[]), None).unwrap();
    let (program, log) = cli(&e.tools, "work", "out.txt");
    e.orch.program = program;
    e.orch.dispatch_plan_item(&a, Some(&alice.id), &Deny, &NoopObserver).await.unwrap();
    // 继续同一个任务的追问不再重复发送目标（会话里已经有）
    e.orch.assign(&alice, &e.repo, "再检查一下", &Deny, &NoopObserver).await.unwrap();
    let lines: Vec<String> = std::fs::read_to_string(&log).unwrap().lines().map(String::from).collect();
    assert!(lines[0].contains("目标：业务不变") && !lines[1].contains("目标：业务不变"));

    let task = e.db.get_plan_item(&a).unwrap().unwrap().task_id.unwrap();
    // 任务进行中，不能取消条目
    assert!(e.db.set_plan_status(&a, "cancelled", "").unwrap_err().to_string().contains("还在进行中"));
    e.orch.abandon_task(&task).await.unwrap();
    let it = e.db.get_plan_item(&a).unwrap().unwrap();
    assert_eq!((it.status.as_str(), it.task_id), ("planned", None), "放弃后回到待开始，可以重新派");
    assert!(it.note.contains("被放弃"));
    e.orch.dispatch_plan_item(&a, Some(&alice.id), &Deny, &NoopObserver).await.unwrap();
    assert_eq!(e.db.get_plan_item(&a).unwrap().unwrap().status, "in_progress");
}

#[tokio::test]
async fn planning_meeting_gets_goal_scan_and_existing_plan() {
    let mut e = setup().await;
    let alice = e.db.add_employee("alice", "全栈", "p").unwrap();
    e.db.add_member(&e.pid, &alice.id, "lead").unwrap();
    e.db.set_project_goal(&e.pid, "把旧系统迁移到新技术栈，业务行为不变").unwrap();
    e.db.create_plan_item(&e.pid, &item("已有的条目", &[]), None).unwrap();
    e.orch.scan_project(&e.pid).await.unwrap();
    let (program, log) = cli(&e.tools, "plan", "unused.txt");
    e.orch.program = program;
    let m = e.db.create_meeting(&e.pid, "planning", "迁移规划", None, None, &[], 5).unwrap();
    e.db.post_message(&m.id, "human", None, "@alice 请制定迁移计划").unwrap();
    e.orch.meeting_reply(&m.id, &alice, &Deny).await.unwrap();
    let prompt = std::fs::read_to_string(&log).unwrap();
    for needle in ["迁移规划", "项目目标与硬性约束", "业务行为不变", "项目扫描结果", "现有计划（1 条）", "已有的条目", "propose_plan"] {
        assert!(prompt.contains(needle), "规划会议的提示里缺少 {needle}");
    }
}
