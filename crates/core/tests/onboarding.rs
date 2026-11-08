//! 旧项目导入端到端：真实 git 仓库 + 假 CLI，不消耗额度。

use crewforge_core::db::Db;
use crewforge_core::onboarding::baseline::Origin;
use crewforge_core::onboarding::scan::BaselineCommand;
use crewforge_core::orchestrator::Orchestrator;
use crewforge_core::provider::claude::{ApprovalRequest, Decision};
use crewforge_core::runner::{Approver, NoopObserver};
use crewforge_core::workspace::{MergeOutcome, WorkspaceManager};
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

fn bc(id: &str, kind: &str, command: &str) -> BaselineCommand {
    BaselineCommand { id: id.into(), label: id.into(), kind: kind.into(), command: command.into(), reason: String::new(), default_on: true, candidate: None }
}

/// 假的"测试脚本"：每个 t_*.fail 文件代表一个失败的测试，按 cargo 的输出格式打印。
const TESTS_SH: &str = "for f in t_*.fail; do [ -e \"$f\" ] && echo \"test ${f%.fail} ... FAILED\"; done\nls t_*.fail >/dev/null 2>&1 && exit 1\nexit 0\n";

struct Env { _d: tempfile::TempDir, repo: PathBuf, tools: PathBuf, db: Arc<Db>, orch: Orchestrator, pid: String, ws: Arc<WorkspaceManager> }

/// 创建一个"旧项目"：已有历史，并且带一个导入前就失败的测试 t_old。
async fn setup() -> Env {
    let data = tempfile::tempdir().unwrap();
    let repo_dir = data.path().join("legacy");
    let tools = data.path().join("bin");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::create_dir_all(&tools).unwrap();
    let db = Arc::new(Db::open(&data.path().join("home")).unwrap());
    let ws = Arc::new(WorkspaceManager::new(&data.path().join("home")));
    ws.init_repo(&repo_dir).await.unwrap();
    std::fs::write(repo_dir.join("tests.sh"), TESTS_SH).unwrap();
    std::fs::write(repo_dir.join("t_old.fail"), "").unwrap();
    std::fs::write(repo_dir.join("main.py"), "print('hi')\n").unwrap();
    sh(&repo_dir, &["add", "-A"]);
    sh(&repo_dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "legacy"]);
    let repo = ws.repo_info(&repo_dir).await.unwrap().root;
    let pid = db.create_project("Legacy", "", repo.to_str().unwrap()).unwrap().id;
    db.set_onboarding(&pid, "in_progress").unwrap();
    Env { _d: data, repo, tools, db: db.clone(), orch: Orchestrator::new(db, ws.clone()), pid, ws }
}

fn cli(dir: &Path, name: &str, writes: &str) -> (String, PathBuf) {
    let log = dir.join(format!("{name}.prompts"));
    let path = dir.join(name);
    std::fs::write(&path, format!(
        "#!/bin/sh\nread -r line\necho '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"x\"}}'\nprintf '%s\\n' \"$line\" >> '{}'\n{}\nprintf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"## 项目概览\\\\n这是评估\",\"usage\":{{}}}}'\n",
        log.display(), if writes.is_empty() { String::new() } else { format!("echo x > {writes}") })).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (path.to_string_lossy().into_owned(), log)
}

const T: Duration = Duration::from_secs(30);

#[tokio::test]
async fn scan_records_stack_and_git_history() {
    let e = setup().await;
    let s = e.orch.scan_project(&e.pid).await.unwrap();
    assert_eq!(s.languages[0].0, "Python");
    assert!(s.git.is_repo && s.git.commit_count == Some(2) && s.git.branch.as_deref() == Some("main"), "{:?}", s.git); // 初始空提交 + legacy
    assert!(s.git.recent.iter().any(|l| l.contains("legacy")));
    assert_eq!(e.db.latest_scan(&e.pid).unwrap().unwrap().total_files, s.total_files, "扫描结果已持久化");
    assert!(e.db.onboarding_state(&e.pid).unwrap().scan.is_some());
}

#[tokio::test]
async fn baseline_runs_in_a_clean_checkout_and_never_touches_the_working_tree() {
    let e = setup().await;
    // 用户的工作目录里有未提交的改动：基线只针对已提交的 HEAD，不应包含它
    std::fs::write(e.repo.join("uncommitted.txt"), "wip").unwrap();
    let run = e.orch.run_baseline(&e.pid, vec![
        bc("sees", "build", "test ! -e uncommitted.txt && echo clean-checkout > artifact.txt"),
        bc("tests", "test", "sh tests.sh # cargo test"),
    ], T).await.unwrap();

    assert_eq!(run.status, "done");
    assert_eq!(run.commit_sha, sh(&e.repo, &["rev-parse", "HEAD"]));
    assert_eq!(run.results[0].status, "passed", "检出里看不到未提交的文件");
    assert_eq!((run.results[1].status.as_str(), run.results[1].failed_tests.clone()), ("failed", vec!["t_old".to_string()]));
    assert!(run.results[1].parsed);
    // 构建产物没有出现在用户的目录里，用户的改动原样保留
    assert!(!e.repo.join("artifact.txt").exists());
    assert_eq!(std::fs::read_to_string(e.repo.join("uncommitted.txt")).unwrap(), "wip");
    // 临时检出已清理，没有残留的 worktree 登记或目录
    assert_eq!(sh(&e.repo, &["worktree", "list"]).lines().count(), 1);
    assert!(!e.ws.scratch_dir("baselines").join(&e.pid).join(&run.id).exists());
    // 进度事件写入项目事件流
    let kinds: Vec<_> = e.db.recent_events_for_project(&e.pid, 100).unwrap().into_iter().map(|x| x.kind).collect();
    for k in ["baseline_started", "baseline_command_finished", "baseline_finished"] { assert!(kinds.contains(&k.to_string()), "{kinds:?}"); }
    assert!(e.orch.run_baseline(&e.pid, vec![], T).await.is_err(), "至少选一条命令");
}

#[tokio::test]
async fn project_in_onboarding_cannot_receive_tasks_until_confirmed() {
    let mut e = setup().await;
    let alice = e.db.add_employee("alice", "全栈", "p").unwrap();
    e.db.add_member(&e.pid, &alice.id, "developer").unwrap();
    let (program, _) = cli(&e.tools, "c", "t_new.fail");
    e.orch.program = program;

    let err = e.orch.assign(&alice, &e.repo, "做点事", &Deny, &NoopObserver).await.unwrap_err().to_string();
    assert!(err.contains("导入阶段"), "{err}");

    // 没扫描、没基线都不能确认
    assert!(e.orch.confirm_onboarding(&e.pid, false).unwrap_err().to_string().contains("扫描"));
    e.orch.scan_project(&e.pid).await.unwrap();
    assert!(e.orch.confirm_onboarding(&e.pid, false).unwrap_err().to_string().contains("基线"), "没有基线不能确认，除非明确跳过");
    e.orch.run_baseline(&e.pid, vec![bc("tests", "test", "sh tests.sh # cargo test")], T).await.unwrap();
    e.orch.confirm_onboarding(&e.pid, false).unwrap();
    assert_eq!(e.db.get_project(&e.pid).unwrap().unwrap().onboarding, "confirmed");
    assert!(e.orch.confirm_onboarding(&e.pid, false).is_err(), "不能重复确认");

    // 确认后：技术栈和基线沉淀为已验证的项目记忆，且明确写出历史失败不是 AI 引入的
    let mems = e.db.list_memories(&crewforge_core::memory::MemoryFilter { project_id: Some(e.pid.clone()), ..Default::default() }).unwrap();
    assert_eq!(mems.len(), 2);
    assert!(mems.iter().all(|m| m.status == "verified" && m.source_kind == "onboarding"));
    let base = mems.iter().find(|m| m.title.contains("基线")).unwrap();
    assert!(base.content.contains("不是 AI 修改引入的") && base.content.contains("t_old"), "{}", base.content);
    // 现在可以派任务了
    e.orch.assign(&alice, &e.repo, "做点事", &Deny, &NoopObserver).await.unwrap();
}

#[tokio::test]
async fn skipping_the_baseline_must_be_explicit_and_is_recorded() {
    let e = setup().await;
    e.orch.scan_project(&e.pid).await.unwrap();
    e.orch.confirm_onboarding(&e.pid, true).unwrap();
    let mems = e.db.list_memories(&crewforge_core::memory::MemoryFilter { project_id: Some(e.pid.clone()), ..Default::default() }).unwrap();
    assert!(mems.iter().any(|m| m.title.contains("没有建立测试基线")));
}

#[tokio::test]
async fn task_check_tells_pre_existing_failures_from_ones_the_ai_introduced() {
    let mut e = setup().await;
    let alice = e.db.add_employee("alice", "全栈", "p").unwrap();
    e.db.add_member(&e.pid, &alice.id, "developer").unwrap();
    let cmds = vec![bc("tests", "test", "sh tests.sh # cargo test")];
    assert!(e.orch.run_baseline(&e.pid, cmds, T).await.is_ok());
    e.orch.scan_project(&e.pid).await.unwrap();
    e.orch.confirm_onboarding(&e.pid, false).unwrap();

    // alice 的修改引入了一个新的失败测试 t_new
    let (program, _) = cli(&e.tools, "c", "t_new.fail");
    e.orch.program = program;
    e.orch.assign(&alice, &e.repo, "实现功能", &Deny, &NoopObserver).await.unwrap();
    let task = e.orch.task_views(&e.repo).await.unwrap().remove(0).task;

    let report = e.orch.check_task(&task.id, T).await.unwrap();
    let find = |item: &str| report.findings.iter().find(|f| f.item == item).unwrap_or_else(|| panic!("{:?}", report.findings)).origin;
    assert_eq!(find("t_old"), Origin::PreExisting, "导入前就失败的，不能算 AI 的");
    assert_eq!(find("t_new"), Origin::Introduced);
    assert_eq!((report.pre_existing, report.introduced, report.fixed, report.unknown), (1, 1, 0, 0));
    assert_eq!(e.db.latest_task_check(&task.id).unwrap().unwrap().report.introduced, 1, "结果已持久化");
    // 检查在干净检出里进行：员工的 worktree 没有被构建产物污染，临时检出也已清理
    assert_eq!(sh(Path::new(&task.worktree_path), &["status", "--porcelain"]), "");
    assert!(!e.ws.scratch_dir("checks").join(&task.id).exists());

    // 没有基线的项目不能做对比检查
    let other_dir = e._d.path().join("other");
    std::fs::create_dir_all(&other_dir).unwrap();
    e.ws.init_repo(&other_dir).await.unwrap();
    let other = e.db.create_project("Other", "", other_dir.canonicalize().unwrap().to_str().unwrap()).unwrap();
    e.db.add_member(&other.id, &alice.id, "developer").unwrap();
    e.orch.assign(&alice, &other_dir, "x", &Deny, &NoopObserver).await.unwrap();
    let t2 = e.orch.task_views(&other_dir).await.unwrap().remove(0).task;
    assert!(e.orch.check_task(&t2.id, T).await.unwrap_err().to_string().contains("基线"));

    // 合并后再检查会被拒绝（只检查进行中的任务）
    assert!(matches!(e.orch.merge_task(&task.id).await.unwrap(), MergeOutcome::Merged { .. }));
    assert!(e.orch.check_task(&task.id, T).await.is_err());
}

#[tokio::test]
async fn ai_evaluation_gets_scan_and_baseline_facts_and_is_read_only() {
    let mut e = setup().await;
    let alice = e.db.add_employee("alice", "全栈", "p").unwrap();
    let outsider = e.db.add_employee("zed", "QA", "p").unwrap();
    e.db.add_member(&e.pid, &alice.id, "lead").unwrap();
    let (program, log) = cli(&e.tools, "c", "");
    e.orch.program = program;

    assert!(e.orch.evaluate_project(&e.pid, "architect", &alice, &Deny).await.unwrap_err().to_string().contains("扫描"));
    e.orch.scan_project(&e.pid).await.unwrap();
    e.orch.run_baseline(&e.pid, vec![bc("tests", "test", "sh tests.sh # cargo test")], T).await.unwrap();
    assert!(e.orch.evaluate_project(&e.pid, "qa", &outsider, &Deny).await.unwrap_err().to_string().contains("不是本项目成员"));
    assert!(e.orch.evaluate_project(&e.pid, "janitor", &alice, &Deny).await.is_err());

    e.orch.evaluate_project(&e.pid, "architect", &alice, &Deny).await.unwrap();
    e.orch.evaluate_project(&e.pid, "qa", &alice, &Deny).await.unwrap();
    let prompts = std::fs::read_to_string(&log).unwrap();
    for needle in ["Python", "t_old", "不是 AI 引入的", "## 技术债清单", "## 安全风险", "只读"] {
        assert!(prompts.contains(needle), "提示里缺少 {needle}");
    }
    let reports = e.db.latest_reports(&e.pid).unwrap();
    assert_eq!(reports.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(), vec!["architect", "qa"]);
    assert!(reports[0].content.contains("项目概览"));
    // 评估走只读会话，不产生任务和 worktree
    assert!(e.orch.task_views(&e.repo).await.unwrap().is_empty());
    assert_eq!(sh(&e.repo, &["worktree", "list"]).lines().count(), 1);
}
