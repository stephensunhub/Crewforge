//! 使用真实 git 验证工作区隔离、检查点与合并。

use crewforge_core::workspace::{MergeOutcome, WorkspaceManager};
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

struct Env {
    _data: TempDir,
    _repo: TempDir,
    ws: WorkspaceManager,
    repo: PathBuf,
}

fn sh(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

async fn setup() -> Env {
    let data = tempfile::tempdir().unwrap();
    let repo_dir = tempfile::tempdir().unwrap();
    let ws = WorkspaceManager::new(data.path());
    ws.init_repo(repo_dir.path()).await.unwrap();
    std::fs::write(repo_dir.path().join("app.txt"), "line1\nline2\nline3\n").unwrap();
    std::fs::write(repo_dir.path().join("other.txt"), "x\n").unwrap();
    sh(repo_dir.path(), &["add", "-A"]);
    sh(repo_dir.path(), &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "seed"]);
    let repo = ws.repo_info(repo_dir.path()).await.unwrap().root;
    Env { _data: data, _repo: repo_dir, ws, repo }
}

#[tokio::test]
async fn non_repo_is_reported_not_errored() {
    let d = tempfile::tempdir().unwrap();
    let ws = WorkspaceManager::new(d.path());
    let st = ws.repo_status(d.path()).await.unwrap();
    assert!(!st.is_repo);
    assert!(ws.repo_info(d.path()).await.is_err());
    ws.init_repo(d.path()).await.unwrap();
    let st = ws.repo_status(d.path()).await.unwrap();
    assert!(st.is_repo && st.has_commits);
    assert_eq!(st.branch.as_deref(), Some("main"));
    assert!(ws.init_repo(d.path()).await.is_err(), "不能重复初始化");
}

#[tokio::test]
async fn employees_get_isolated_worktrees_outside_the_repo() {
    let e = setup().await;
    let (wa, _) = e.ws.prepare_task(&e.repo, "alice", "crewforge/alice/t1", "main").await.unwrap();
    let (wb, _) = e.ws.prepare_task(&e.repo, "bob", "crewforge/bob/t1", "main").await.unwrap();
    assert_ne!(wa, wb);
    assert!(!wa.starts_with(&e.repo), "worktree 不应位于项目仓库内");

    std::fs::write(wa.join("only_alice.txt"), "a").unwrap();
    assert!(!wb.join("only_alice.txt").exists(), "bob 看不到 alice 未合并的改动");
    assert!(!e.repo.join("only_alice.txt").exists(), "主工作区不受影响");
    assert_eq!(sh(&wb, &["rev-parse", "--abbrev-ref", "HEAD"]), "crewforge/bob/t1");
}

#[tokio::test]
async fn concurrent_prepare_on_same_repo_is_safe() {
    let e = setup().await;
    let p = |n: &'static str| e.ws.prepare_task(&e.repo, n, format!("crewforge/{n}/t").leak(), "main");
    let (a, b, c, d) = tokio::join!(p("e1"), p("e2"), p("e3"), p("e4"));
    for r in [a, b, c, d] {
        r.unwrap();
    }
    // 主工作区加 4 个员工 worktree
    assert_eq!(sh(&e.repo, &["worktree", "list"]).lines().count(), 5);
}

#[tokio::test]
async fn checkpoint_records_author_and_changes() {
    let e = setup().await;
    let (w, base) = e.ws.prepare_task(&e.repo, "alice", "crewforge/alice/t1", "main").await.unwrap();
    assert!(e.ws.checkpoint(&w, "alice", "nothing").await.unwrap().is_none());

    std::fs::write(w.join("app.txt"), "line1\nCHANGED\nline3\n").unwrap();
    std::fs::write(w.join("new.txt"), "n\n").unwrap();
    let files = e.ws.changed_files(&w, &base).await.unwrap();
    assert!(files.iter().any(|f| f.path == "app.txt" && !f.untracked));
    assert!(files.iter().any(|f| f.path == "new.txt" && f.untracked));

    let cp = e.ws.checkpoint(&w, "alice", "[crewforge] work\n\nCrewforge-Task: t1").await.unwrap().unwrap();
    assert_eq!(cp.files, 2);
    assert_eq!(sh(&w, &["log", "-1", "--format=%an <%ae>"]), "alice <alice@crewforge.local>");
    assert!(sh(&w, &["log", "-1", "--format=%B"]).contains("Crewforge-Task: t1"));
    assert!(e.ws.diff(&w, &base, 100_000).await.unwrap().contains("+CHANGED"));
    // 提交之后改动仍然可见（相对基础提交）
    assert_eq!(e.ws.changed_files(&w, &base).await.unwrap().len(), 2);
}

#[tokio::test]
async fn squash_merge_then_next_task_starts_from_new_base() {
    let e = setup().await;
    let (w, base) = e.ws.prepare_task(&e.repo, "alice", "crewforge/alice/t1", "main").await.unwrap();
    std::fs::write(w.join("feature.txt"), "feat\n").unwrap();
    e.ws.checkpoint(&w, "alice", "step 1").await.unwrap();
    std::fs::write(w.join("feature.txt"), "feat v2\n").unwrap();
    e.ws.checkpoint(&w, "alice", "step 2").await.unwrap();
    let before = sh(&e.repo, &["rev-list", "--count", "main"]);

    let out = e.ws.merge(&e.repo, "main", "crewforge/alice/t1", "alice", "alice: feature\n\nCrewforge-Task: t1").await.unwrap();
    let MergeOutcome::Merged { commit } = out else { panic!("{out:?}") };
    assert_eq!(std::fs::read_to_string(e.repo.join("feature.txt")).unwrap(), "feat v2\n");
    assert_eq!(sh(&e.repo, &["rev-parse", "HEAD"]), commit);
    // squash：两个检查点在主分支上只留下一个提交
    assert_eq!(sh(&e.repo, &["rev-list", "--count", "main"]).parse::<u32>().unwrap(), before.parse::<u32>().unwrap() + 1);
    assert_eq!(sh(&e.repo, &["log", "-1", "--format=%an"]), "alice");
    assert_ne!(base, commit);

    // 同一员工的下一个任务从合并后的新基础开始，且复用同一个 worktree
    let (w2, base2) = e.ws.prepare_task(&e.repo, "alice", "crewforge/alice/t2", "main").await.unwrap();
    assert_eq!(w, w2);
    assert_eq!(base2, commit);
    assert!(w2.join("feature.txt").exists());
    e.ws.delete_branch_best_effort(&e.repo, "crewforge/alice/t1").await;
    assert!(!sh(&e.repo, &["branch", "--list", "crewforge/alice/t1"]).contains("t1"));
}

#[tokio::test]
async fn conflicting_merge_is_detected_and_leaves_main_untouched() {
    let e = setup().await;
    let (wa, _) = e.ws.prepare_task(&e.repo, "alice", "crewforge/alice/t1", "main").await.unwrap();
    let (wb, _) = e.ws.prepare_task(&e.repo, "bob", "crewforge/bob/t1", "main").await.unwrap();
    std::fs::write(wa.join("app.txt"), "line1\nALICE\nline3\n").unwrap();
    std::fs::write(wb.join("app.txt"), "line1\nBOB\nline3\n").unwrap();
    e.ws.checkpoint(&wa, "alice", "a").await.unwrap();
    e.ws.checkpoint(&wb, "bob", "b").await.unwrap();

    let chk = e.ws.merge_check(&e.repo, "main", "crewforge/bob/t1").await.unwrap();
    assert!(chk.clean, "bob 先合并时没有冲突");
    assert!(matches!(e.ws.merge(&e.repo, "main", "crewforge/alice/t1", "alice", "m").await.unwrap(), MergeOutcome::Merged { .. }));

    let chk = e.ws.merge_check(&e.repo, "main", "crewforge/bob/t1").await.unwrap();
    assert!(!chk.clean);
    assert_eq!(chk.conflicts, vec!["app.txt"]);
    let head = sh(&e.repo, &["rev-parse", "HEAD"]);
    let out = e.ws.merge(&e.repo, "main", "crewforge/bob/t1", "bob", "m").await.unwrap();
    assert!(matches!(&out, MergeOutcome::Conflict { files } if files == &vec!["app.txt".to_string()]), "{out:?}");
    // 主工作区保持干净，没有遗留合并状态
    assert_eq!(sh(&e.repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(sh(&e.repo, &["status", "--porcelain"]), "");
    assert_eq!(std::fs::read_to_string(e.repo.join("app.txt")).unwrap(), "line1\nALICE\nline3\n");
}

#[tokio::test]
async fn merge_is_blocked_when_main_is_dirty_or_on_other_branch() {
    let e = setup().await;
    let (w, _) = e.ws.prepare_task(&e.repo, "alice", "crewforge/alice/t1", "main").await.unwrap();
    std::fs::write(w.join("f.txt"), "f").unwrap();
    e.ws.checkpoint(&w, "alice", "a").await.unwrap();

    std::fs::write(e.repo.join("other.txt"), "用户正在编辑\n").unwrap();
    let out = e.ws.merge(&e.repo, "main", "crewforge/alice/t1", "alice", "m").await.unwrap();
    assert!(matches!(out, MergeOutcome::Blocked { .. }), "{out:?}");
    assert_eq!(std::fs::read_to_string(e.repo.join("other.txt")).unwrap(), "用户正在编辑\n", "用户的改动不能被动到");
    sh(&e.repo, &["checkout", "-q", "--", "other.txt"]);

    sh(&e.repo, &["switch", "-q", "-c", "dev"]);
    let out = e.ws.merge(&e.repo, "main", "crewforge/alice/t1", "alice", "m").await.unwrap();
    assert!(matches!(out, MergeOutcome::Blocked { .. }), "{out:?}");
}

#[tokio::test]
async fn empty_merge_and_dirty_worktree_guard() {
    let e = setup().await;
    let (w, _) = e.ws.prepare_task(&e.repo, "alice", "crewforge/alice/t1", "main").await.unwrap();
    let out = e.ws.merge(&e.repo, "main", "crewforge/alice/t1", "alice", "m").await.unwrap();
    assert!(matches!(out, MergeOutcome::Empty), "{out:?}");
    assert_eq!(sh(&e.repo, &["status", "--porcelain"]), "");

    // 有未提交改动时不允许开始新任务（避免丢失上一个任务的工作）
    std::fs::write(w.join("wip.txt"), "wip").unwrap();
    assert!(e.ws.prepare_task(&e.repo, "alice", "crewforge/alice/t2", "main").await.is_err());
    assert!(w.join("wip.txt").exists());
}

#[tokio::test]
async fn ensure_ready_initializes_only_when_asked() {
    let d = tempfile::tempdir().unwrap();
    let ws = WorkspaceManager::new(d.path());
    let proj = d.path().join("p");
    std::fs::create_dir_all(&proj).unwrap();
    assert!(ws.ensure_ready(&proj, false).await.is_err(), "未授权时不得擅自初始化");
    assert!(!ws.repo_status(&proj).await.unwrap().is_repo);

    let info = ws.ensure_ready(&proj, true).await.unwrap();
    assert_eq!(info.branch, "main");
    // 已有仓库但没有提交：补一个空提交
    let q = d.path().join("q");
    std::fs::create_dir_all(&q).unwrap();
    sh(&q, &["init", "-q", "-b", "trunk"]);
    assert!(ws.ensure_ready(&q, false).await.is_err());
    assert_eq!(ws.ensure_ready(&q, true).await.unwrap().branch, "trunk");
    // 已就绪的仓库保持不变
    assert_eq!(ws.ensure_ready(&q, true).await.unwrap().branch, "trunk");
}
