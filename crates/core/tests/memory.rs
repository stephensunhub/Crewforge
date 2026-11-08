//! 记忆端到端：新任务注入相关记忆、继续任务不重复注入、合并后自动验证关联经验。

use crewforge_core::db::Db;
use crewforge_core::memory::{MemoryFilter, NewMemory};
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

/// 假 CLI：把收到的提示追加到文件，并在工作目录写一个文件，使任务有改动可合并。
fn cli(dir: &Path, name: &str) -> (String, PathBuf) {
    let log = dir.join(format!("{name}.prompts"));
    let path = dir.join(name);
    std::fs::write(&path, format!(
        "#!/bin/sh\nread -r line\necho '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"x\"}}'\nprintf '%s\\n' \"$line\" >> '{}'\necho work > out.txt\necho '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"ok\",\"usage\":{{}}}}'\n",
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
    let pid = db.create_project("Shop", "", repo.to_str().unwrap()).unwrap().id;
    Env { _d: data, repo, tools, db: db.clone(), orch: Orchestrator::new(db, ws), pid }
}

fn new_mem(layer: &str, pid: &str, content: &str) -> NewMemory {
    NewMemory { layer: layer.into(), project_id: Some(pid.into()), title: String::new(), content: content.into(),
        source_kind: "human".into(), created_by: "human".into(), ..Default::default() }
}

#[tokio::test]
async fn new_task_gets_relevant_memory_once_and_logs_stay_small() {
    let mut e = setup().await;
    let alice = e.db.add_employee("alice", "全栈", "p").unwrap();
    e.db.add_member(&e.pid, &alice.id, "developer").unwrap();
    let relevant = e.db.add_memory(new_mem("project", &e.pid, "跑集成测试之前必须先启动 Redis 缓存服务")).unwrap();
    e.db.add_memory(new_mem("project", &e.pid, "前端组件统一放在 components 目录")).unwrap();
    let (program, log) = cli(&e.tools, "c");
    e.orch.program = program;

    e.orch.assign(&alice, &e.repo, "给订单接口补集成测试，注意 redis 依赖", &Deny, &NoopObserver).await.unwrap();
    let p1 = std::fs::read_to_string(&log).unwrap();
    assert!(p1.contains("相关经验") && p1.contains("必须先启动 Redis"), "新任务应注入相关记忆");
    assert!(!p1.contains("components 目录"), "不相关的记忆不能注入");

    // 事件里只记原始任务，注入量单独记录
    let events = e.db.recent_events(200).unwrap();
    let started = events.iter().find(|x| x.kind == "task_started").unwrap();
    assert_eq!(started.payload["prompt"], "给订单接口补集成测试，注意 redis 依赖");
    let inj = events.iter().find(|x| x.kind == "memory_injected").unwrap();
    assert_eq!(inj.payload["count"], 1);
    assert!(inj.payload["chars"].as_u64().unwrap() > 0);
    assert_eq!(inj.project_id.as_deref(), Some(e.pid.as_str()));
    assert_eq!(e.db.get_memory(&relevant.id).unwrap().unwrap().uses, 1, "记录被使用的次数");

    // 继续同一任务：不重复注入
    e.orch.assign(&alice, &e.repo, "再检查一下 redis 超时", &Deny, &NoopObserver).await.unwrap();
    let lines: Vec<String> = std::fs::read_to_string(&log).unwrap().lines().map(String::from).collect();
    assert_eq!(lines.len(), 2);
    assert!(!lines[1].contains("相关经验"), "继续任务时不应重复传递记忆");
    assert_eq!(e.db.get_memory(&relevant.id).unwrap().unwrap().uses, 1);
}

#[tokio::test]
async fn no_relevant_memory_means_no_injection() {
    let mut e = setup().await;
    let alice = e.db.add_employee("alice", "全栈", "p").unwrap();
    e.db.add_memory(new_mem("project", &e.pid, "跑集成测试之前必须先启动 Redis 缓存服务")).unwrap();
    let (program, log) = cli(&e.tools, "c");
    e.orch.program = program;
    e.orch.assign(&alice, &e.repo, "把首页按钮改成蓝色", &Deny, &NoopObserver).await.unwrap();
    assert!(!std::fs::read_to_string(&log).unwrap().contains("相关经验"));
    assert!(e.db.recent_events(100).unwrap().iter().all(|x| x.kind != "memory_injected"));
}

#[tokio::test]
async fn merging_a_task_verifies_its_linked_lessons() {
    let mut e = setup().await;
    let alice = e.db.add_employee("alice", "全栈", "p").unwrap();
    let (program, _log) = cli(&e.tools, "c");
    e.orch.program = program;
    e.orch.assign(&alice, &e.repo, "实现功能", &Deny, &NoopObserver).await.unwrap();
    let task = e.orch.task_views(&e.repo).await.unwrap().remove(0).task;

    // 一场 Code Review 会议产出的经验，关联到这个任务，初始为待验证
    let m = e.db.create_meeting(&e.pid, "code_review", "评审", None, Some(&task.id), &[], 5).unwrap();
    e.db.set_minutes(&m.id, "## 待验证的经验\n- 订单号生成必须保证全局唯一").unwrap();
    assert_eq!(e.db.save_meeting_lessons(&m.id).unwrap(), 1);
    let before = e.db.list_memories(&MemoryFilter { project_id: Some(e.pid.clone()), ..Default::default() }).unwrap();
    assert_eq!(before[0].status, "unverified");
    assert_eq!(before[0].task_id.as_deref(), Some(task.id.as_str()));

    let MergeOutcome::Merged { commit } = e.orch.merge_task(&task.id).await.unwrap() else { panic!() };
    let after = e.db.get_memory(&before[0].id).unwrap().unwrap();
    assert_eq!(after.status, "verified");
    assert_eq!(after.verified_by.as_deref(), Some("merge"));
    assert_eq!(after.related_commit.as_deref(), Some(commit.as_str()), "记下是哪次提交验证了它");
    // 验证之后才会被检索到
    let got = e.db.retrieve_memories(Some(&e.pid), &alice.id, None, "订单号生成要唯一", 5, 2000).unwrap();
    assert_eq!(got.len(), 1);
}
