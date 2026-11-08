//! 技术栈迁移的对照检查端到端：旧实现的输出是标准答案，新实现必须逐字一致；旧代码删掉之后依然有效。

use crewforge_core::db::Db;
use crewforge_core::onboarding::baseline::Origin;
use crewforge_core::onboarding::scan::BaselineCommand;
use crewforge_core::orchestrator::Orchestrator;
use crewforge_core::provider::claude::{ApprovalRequest, Decision};
use crewforge_core::runner::{Approver, NoopObserver};
use crewforge_core::workspace::WorkspaceManager;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

struct Deny;
impl Approver for Deny {
    async fn ask(&self, _: &str, _: &ApprovalRequest, _: &str) -> Decision { Decision::Deny("x".into()) }
}

fn sh(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// 假 CLI：在员工的工作目录里写出“新实现”new.sh（内容由参数决定），并删掉旧实现 legacy.sh。
fn migrator(dir: &Path, name: &str, new_script: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, format!(
        "#!/bin/sh\nread -r line\necho '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"x\"}}'\nprintf '%s\\n' '{new_script}' > new.sh\nrm -f legacy.sh\necho '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"ok\",\"usage\":{{}}}}'\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

struct Env { _d: tempfile::TempDir, repo: PathBuf, tools: PathBuf, db: Arc<Db>, orch: Orchestrator, pid: String }

async fn setup() -> Env {
    let data = tempfile::tempdir().unwrap();
    let repo_dir = data.path().join("legacy-app");
    let tools = data.path().join("bin");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::create_dir_all(&tools).unwrap();
    let db = Arc::new(Db::open(&data.path().join("home")).unwrap());
    let ws = Arc::new(WorkspaceManager::new(&data.path().join("home")));
    ws.init_repo(&repo_dir).await.unwrap();
    // 旧实现：打印一份固定的“发票汇总”
    std::fs::write(repo_dir.join("legacy.sh"), "printf 'invoice=1001\\ntotal=42.50\\ntax=3.40\\n'\n").unwrap();
    sh(&repo_dir, &["add", "-A"]);
    sh(&repo_dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "legacy"]);
    let repo = ws.repo_info(&repo_dir).await.unwrap().root;
    let pid = db.create_project("Legacy", "", repo.to_str().unwrap()).unwrap().id;
    Env { _d: data, repo, tools, db: db.clone(), orch: Orchestrator::new(db, ws), pid }
}

fn parity() -> BaselineCommand {
    BaselineCommand { id: "invoice".into(), label: "发票汇总输出".into(), kind: "parity".into(), command: "sh legacy.sh".into(), reason: String::new(), default_on: true,
        candidate: Some("sh new.sh".into()) }
}

#[tokio::test]
async fn migrated_implementation_must_reproduce_the_legacy_output_even_after_the_legacy_code_is_gone() {
    let mut e = setup().await;
    let alice = e.db.add_employee("alice", "全栈", "p").unwrap();
    let bob = e.db.add_employee("bob", "全栈", "p").unwrap();
    for x in [&alice, &bob] { e.db.add_member(&e.pid, &x.id, "developer").unwrap(); }
    // 基线：旧实现的输出被记下来作为标准答案
    let base = e.orch.run_baseline(&e.pid, vec![parity()], Duration::from_secs(30)).await.unwrap();
    assert_eq!(base.results[0].golden.as_deref(), Some("invoice=1001\ntotal=42.50\ntax=3.40"));
    e.db.set_onboarding(&e.pid, "none").unwrap();

    // alice 的新实现输出完全一致（旧实现 legacy.sh 在她的分支里已经删除）
    e.orch.program = migrator(&e.tools, "good", "printf \"invoice=1001\\ntotal=42.50\\ntax=3.40\\n\"");
    e.orch.assign(&alice, &e.repo, "把汇总迁移到新实现", &Deny, &NoopObserver).await.unwrap();
    let t1 = e.orch.task_views(&e.repo).await.unwrap().into_iter().find(|v| v.employee_name == "alice").unwrap().task;
    let ok = e.orch.check_task(&t1.id, Duration::from_secs(30)).await.unwrap();
    assert_eq!((ok.introduced, ok.unknown, ok.findings.len()), (0, 0, 0), "输出一致：业务行为没变。{:?}", ok.findings);
    assert_eq!(ok.results[0].command, "sh new.sh");

    // bob 的新实现在税额上有细微差别：被抓到，并指出第一处差异
    e.orch.program = migrator(&e.tools, "bad", "printf \"invoice=1001\\ntotal=42.50\\ntax=3.41\\n\"");
    e.orch.assign(&bob, &e.repo, "另一个迁移", &Deny, &NoopObserver).await.unwrap();
    let t2 = e.orch.task_views(&e.repo).await.unwrap().into_iter().find(|v| v.employee_name == "bob").unwrap().task;
    let bad = e.orch.check_task(&t2.id, Duration::from_secs(30)).await.unwrap();
    assert_eq!((bad.introduced, bad.findings[0].origin, bad.findings[0].item.as_str()), (1, Origin::Introduced, "输出与基线不一致"));
    let tail = &bad.results[0].output_tail;
    assert!(tail.contains("第 3 行") && tail.contains("tax=3.40") && tail.contains("tax=3.41"), "差异说明要能直接定位：{tail}");
}
