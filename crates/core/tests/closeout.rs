//! 结束项目：已完成 / 归档 / 不做了（彻底删除）。不调用任何模型。

use crewforge_core::db::Db;
use crewforge_core::orchestrator::Orchestrator;
use crewforge_core::provider::claude::{ApprovalRequest, Decision};
use crewforge_core::runner::{Approver, NoopObserver};
use crewforge_core::workspace::WorkspaceManager;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

struct Deny;
impl Approver for Deny {
    async fn ask(&self, _: &str, _: &ApprovalRequest, _: &str) -> Decision {
        Decision::Deny("t".into())
    }
}

fn sh(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn fake_cli(dir: &Path) -> String {
    let path = dir.join("fake");
    std::fs::write(&path, "#!/bin/sh\nread -r line\necho '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"x\"}'\nprintf 'x\\n' > out.txt\necho '{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"done\"}]}}'\necho '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\",\"total_cost_usd\":0.01,\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}'\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

struct Env {
    _d: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
    data: PathBuf,
    db: Arc<Db>,
    orch: Orchestrator,
    pid: String,
}

/// 一个有成员、有进行中任务、有会议/议题/记忆/计划的项目。
async fn setup() -> Env {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().canonicalize().unwrap();
    let repo_dir = root.join("proj");
    let data = root.join("home");
    std::fs::create_dir_all(&repo_dir).unwrap();
    let db = Arc::new(Db::open(&data).unwrap());
    let ws = Arc::new(WorkspaceManager::new(&data));
    ws.init_repo(&repo_dir).await.unwrap();
    std::fs::write(repo_dir.join("README.md"), "hi\n").unwrap();
    sh(&repo_dir, &["add", "-A"]);
    sh(&repo_dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "seed"]);
    let repo = ws.repo_info(&repo_dir).await.unwrap().root;
    let project = db.create_project("Shop", "", repo.to_str().unwrap()).unwrap();
    let alice = db.add_employee("alice", "全栈", "p").unwrap();
    db.add_member(&project.id, &alice.id, "lead").unwrap();
    let mut orch = Orchestrator::new(db.clone(), ws);
    orch.program = fake_cli(&root);
    orch.assign(&alice, &repo, "做点事", &Deny, &NoopObserver).await.unwrap();
    db.add_memory(crewforge_core::memory::NewMemory { layer: "project".into(), project_id: Some(project.id.clone()), title: "约定".into(), content: "用 4 空格缩进".into(), source_kind: "human".into(), created_by: "human".into(), ..Default::default() }).unwrap();
    db.create_meeting(&project.id, "retro", "复盘", None, None, &[], 5).unwrap();
    db.create_plan_item(&project.id, &crewforge_core::plan::NewPlanItem { title: "A".into(), kind: "migrate".into(), ..Default::default() }, None).unwrap();
    Env { _d: d, root, repo, data, db, orch, pid: project.id }
}

fn active_tasks(e: &Env) -> usize {
    e.db.list_tasks(&e.repo.to_string_lossy()).unwrap().iter().filter(|t| t.status == "active").count()
}

#[tokio::test]
async fn completed_requires_no_active_tasks_while_archive_abandons_them_and_keeps_all_directories() {
    let e = setup().await;
    assert_eq!(active_tasks(&e), 1);
    let wt = PathBuf::from(&e.db.list_tasks(&e.repo.to_string_lossy()).unwrap()[0].worktree_path);
    assert!(wt.exists());
    // 已完成：有进行中的任务就拒绝，什么都没变
    let err = e.orch.end_project(&e.pid, "completed", "").await.unwrap_err().to_string();
    assert!(err.contains("进行中") && err.contains("归档"), "{err}");
    assert_eq!(e.db.get_project(&e.pid).unwrap().unwrap().status, "active");
    // 归档：自动放弃进行中的任务，工作区和分支都留着
    e.orch.end_project(&e.pid, "archived", "客户砍掉了需求").await.unwrap();
    assert_eq!(active_tasks(&e), 0);
    assert!(wt.exists() && e.repo.exists(), "归档不删任何目录");
    let ended = e.db.list_ended_projects().unwrap();
    assert_eq!((ended.len(), ended[0].end_kind.as_str(), ended[0].end_reason.as_str()), (1, "archived", "客户砍掉了需求"));
    assert!(e.db.list_projects().unwrap().is_empty());
    // 不能重复结束；可以恢复
    assert!(e.orch.end_project(&e.pid, "archived", "").await.is_err());
    e.db.restore_project(&e.pid).unwrap();
    assert_eq!(e.db.list_projects().unwrap().len(), 1);
    assert!(e.db.list_ended_projects().unwrap().is_empty());
    // 任务都放弃了，现在可以标记已完成
    e.orch.end_project(&e.pid, "completed", "").await.unwrap();
    assert_eq!(e.db.list_ended_projects().unwrap()[0].end_kind, "completed");
}

#[tokio::test]
async fn discard_needs_the_exact_name_removes_worktrees_and_records_but_keeps_the_folder_and_usage() {
    let e = setup().await;
    let wt = PathBuf::from(&e.db.list_tasks(&e.repo.to_string_lossy()).unwrap()[0].worktree_path);
    let branch = e.db.list_tasks(&e.repo.to_string_lossy()).unwrap()[0].branch.clone();
    assert!(e.orch.discard_project(&e.pid, "shop", false).await.unwrap_err().to_string().contains("完整的项目名称"));
    assert!(wt.exists(), "名称不对时什么都没动");

    let r = e.orch.discard_project(&e.pid, "Shop", false).await.unwrap();
    assert_eq!((r.tasks_removed, r.folder_trashed), (1, None));
    assert!(!wt.exists(), "员工的 worktree 被清理");
    assert!(e.repo.join("README.md").exists(), "项目文件夹还在");
    assert!(!sh(&e.repo, &["branch", "--list", &branch]).contains(&branch), "任务分支被删除");
    assert!(e.db.get_project(&e.pid).unwrap().is_none());
    assert!(e.db.list_tasks(&e.repo.to_string_lossy()).unwrap().is_empty());
    assert!(e.db.list_sessions().unwrap().is_empty(), "员工在这个项目里的会话记录被清理");
    assert!(e.db.list_ended_projects().unwrap().is_empty());
    assert!(e.db.employee_by_name("alice").unwrap().is_some(), "员工本身（公司的人）不受影响");
    // 用量记录保留（那是真实花掉的额度）
    assert!(e.db.recent_events(200).unwrap().iter().any(|x| x.kind == "turn_result"));
    // 同一个目录可以再次登记为新项目
    e.db.create_project("Shop2", "", e.repo.to_str().unwrap()).unwrap();
}

#[tokio::test]
async fn deleting_the_folder_is_limited_to_clean_new_projects_and_goes_to_the_trash() {
    let e = setup().await;
    // 把家目录指到临时目录，废纸篓也在里面
    let home = e.root.join("fakehome");
    std::fs::create_dir_all(&home).unwrap();
    unsafe { std::env::set_var("HOME", &home); }

    // 有未提交的改动 → 拒绝，并且什么都没删
    std::fs::write(e.repo.join("scratch.txt"), "未保存").unwrap();
    let err = e.orch.discard_project(&e.pid, "Shop", true).await.unwrap_err().to_string();
    assert!(err.contains("未提交") && err.contains("scratch.txt"), "{err}");
    assert!(e.db.get_project(&e.pid).unwrap().is_some() && e.repo.exists());
    std::fs::remove_file(e.repo.join("scratch.txt")).unwrap();

    // 导入的已有项目 → 拒绝连文件夹一起删
    e.db.set_onboarding(&e.pid, "confirmed").unwrap();
    let err = e.orch.discard_project(&e.pid, "Shop", true).await.unwrap_err().to_string();
    assert!(err.contains("导入"), "{err}");
    e.db.set_onboarding(&e.pid, "none").unwrap();

    // 干净的新项目 → 文件夹移到废纸篓，数据目录之外的东西没被删
    let r = e.orch.discard_project(&e.pid, "Shop", true).await.unwrap();
    let trashed = PathBuf::from(r.folder_trashed.unwrap());
    assert!(!e.repo.exists() && trashed.join("README.md").exists() && trashed.starts_with(home.join(".Trash")));
    assert!(e.data.exists(), "数据目录还在");
    assert!(e.db.get_project(&e.pid).unwrap().is_none());
}
