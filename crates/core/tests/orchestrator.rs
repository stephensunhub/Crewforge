//! 用假的 claude 脚本端到端验证编排：不消耗任何额度。

use crewforge_core::db::Db;
use crewforge_core::orchestrator::Orchestrator;
use crewforge_core::provider::claude::{ApprovalRequest, Decision};
use crewforge_core::runner::{Approver, NoopObserver};
use crewforge_core::workspace::{MergeOutcome, WorkspaceManager};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

struct AutoDeny;
impl Approver for AutoDeny {
    async fn ask(&self, _: &str, _: &ApprovalRequest, _: &str) -> Decision {
        Decision::Deny("test".into())
    }
}

/// 假 CLI：读入一行，输出 init，在当前目录（即员工 worktree）写入文件，然后输出结果。
fn fake_cli(dir: &Path, name: &str, file: &str, content: &str, fail: bool) -> String {
    let path = dir.join(name);
    let tail = if fail {
        "exit 3".to_string()
    } else {
        r#"echo '{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}'
echo '{"type":"result","subtype":"success","is_error":false,"result":"done","total_cost_usd":0.01,"usage":{"input_tokens":1,"output_tokens":1}}'"#
            .to_string()
    };
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nread -r line\necho '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"x\"}}'\nprintf '%s\\n' '{content}' > '{file}'\n{tail}\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

fn sh(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

struct Env {
    _d: tempfile::TempDir,
    repo: std::path::PathBuf,
    tools: std::path::PathBuf,
    db: Arc<Db>,
    orch: Orchestrator,
}

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
    let orch = Orchestrator::new(db.clone(), ws);
    Env { _d: data, repo, tools, db, orch }
}

#[tokio::test]
async fn assign_runs_in_isolated_worktree_and_checkpoints() {
    let mut e = setup().await;
    let alice = e.db.add_employee("alice", "fullstack", "p").unwrap();
    let bob = e.db.add_employee("bob", "fullstack", "p").unwrap();

    e.orch.program = fake_cli(&e.tools, "claude-a", "a.txt", "from alice", false);
    e.orch.assign(&alice, &e.repo, "实现登录\n细节...", &AutoDeny, &NoopObserver).await.unwrap();
    e.orch.program = fake_cli(&e.tools, "claude-b", "b.txt", "from bob", false);
    e.orch.assign(&bob, &e.repo, "实现注册", &AutoDeny, &NoopObserver).await.unwrap();

    // 主工作区没有被任何员工改动
    assert!(!e.repo.join("a.txt").exists() && !e.repo.join("b.txt").exists());
    assert_eq!(sh(&e.repo, &["status", "--porcelain"]), "");

    let views = e.orch.task_views(&e.repo).await.unwrap();
    assert_eq!(views.len(), 2);
    let va = views.iter().find(|v| v.employee_name == "alice").unwrap();
    assert_eq!(va.task.title, "实现登录");
    assert_eq!(va.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), vec!["a.txt"]);
    assert!(va.merge.as_ref().unwrap().clean);
    assert!(va.overlaps.is_empty());

    // 每个员工的会话都在各自的 worktree 里，互不混用
    let sessions = e.db.list_sessions().unwrap();
    assert_eq!(sessions.len(), 2);
    assert_ne!(sessions[0].project_path, sessions[1].project_path);
    assert!(sessions.iter().all(|s| !s.project_path.starts_with(e.repo.to_str().unwrap())));

    // 检查点以员工署名，并带有任务编号
    let log = sh(Path::new(&va.task.worktree_path), &["log", "-1", "--format=%an|%B"]);
    assert!(log.starts_with("alice|") && log.contains(&format!("Crewforge-Task: {}", va.task.id)), "{log}");
    let kinds: Vec<String> = e.db.recent_events(200).unwrap().into_iter().map(|x| x.kind).collect();
    assert!(kinds.contains(&"task_created".into()) && kinds.contains(&"checkpoint_committed".into()));
}

#[tokio::test]
async fn followup_continues_same_task_then_merge_and_next_task() {
    let mut e = setup().await;
    let alice = e.db.add_employee("alice", "fullstack", "p").unwrap();
    e.orch.program = fake_cli(&e.tools, "c1", "one.txt", "1", false);
    e.orch.assign(&alice, &e.repo, "第一步", &AutoDeny, &NoopObserver).await.unwrap();
    e.orch.program = fake_cli(&e.tools, "c2", "two.txt", "2", false);
    e.orch.assign(&alice, &e.repo, "第二步", &AutoDeny, &NoopObserver).await.unwrap();

    let views = e.orch.task_views(&e.repo).await.unwrap();
    assert_eq!(views.len(), 1, "追加消息应继续同一任务");
    assert_eq!(views[0].files.len(), 2);
    let id = views[0].task.id.clone();
    assert!(e.orch.task_diff(&id).await.unwrap().contains("+1"));

    let out = e.orch.merge_task(&id).await.unwrap();
    assert!(matches!(out, MergeOutcome::Merged { .. }), "{out:?}");
    assert!(e.repo.join("one.txt").exists() && e.repo.join("two.txt").exists());
    assert_eq!(e.db.get_task(&id).unwrap().unwrap().status, "merged");
    assert!(e.orch.merge_task(&id).await.is_err(), "不能重复合并");
    assert!(e.orch.task_diff(&id).await.unwrap().contains("+2"), "已合并任务仍可查看 diff");

    // 合并后再分配 → 新任务、新分支，基础是合并后的主分支
    e.orch.program = fake_cli(&e.tools, "c3", "three.txt", "3", false);
    e.orch.assign(&alice, &e.repo, "下一个任务", &AutoDeny, &NoopObserver).await.unwrap();
    let views = e.orch.task_views(&e.repo).await.unwrap();
    assert_eq!(views.len(), 2);
    let newest = &views[0];
    assert_eq!(newest.task.status, "active");
    assert!(Path::new(&newest.task.worktree_path).join("one.txt").exists());
}

#[tokio::test]
async fn overlapping_edits_are_flagged_and_conflict_blocks_merge() {
    let mut e = setup().await;
    let alice = e.db.add_employee("alice", "fullstack", "p").unwrap();
    let bob = e.db.add_employee("bob", "fullstack", "p").unwrap();
    e.orch.program = fake_cli(&e.tools, "ca", "README.md", "alice version", false);
    e.orch.assign(&alice, &e.repo, "改说明", &AutoDeny, &NoopObserver).await.unwrap();
    e.orch.program = fake_cli(&e.tools, "cb", "README.md", "bob version", false);
    e.orch.assign(&bob, &e.repo, "也改说明", &AutoDeny, &NoopObserver).await.unwrap();

    let views = e.orch.task_views(&e.repo).await.unwrap();
    assert!(views.iter().all(|v| v.overlaps.len() == 1 && v.overlaps[0].path == "README.md"));

    let ida = views.iter().find(|v| v.employee_name == "alice").unwrap().task.id.clone();
    let idb = views.iter().find(|v| v.employee_name == "bob").unwrap().task.id.clone();
    assert!(matches!(e.orch.merge_task(&ida).await.unwrap(), MergeOutcome::Merged { .. }));
    let out = e.orch.merge_task(&idb).await.unwrap();
    assert!(matches!(&out, MergeOutcome::Conflict { files } if files == &vec!["README.md".to_string()]), "{out:?}");
    assert_eq!(e.db.get_task(&idb).unwrap().unwrap().status, "active", "冲突时任务保持进行中");
    assert_eq!(sh(&e.repo, &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn interrupted_run_still_saves_work_and_abandon_keeps_branch() {
    let mut e = setup().await;
    let alice = e.db.add_employee("alice", "fullstack", "p").unwrap();
    e.orch.program = fake_cli(&e.tools, "boom", "partial.txt", "half", true);
    let r = e.orch.assign(&alice, &e.repo, "会失败的任务", &AutoDeny, &NoopObserver).await;
    assert!(r.is_err());

    let views = e.orch.task_views(&e.repo).await.unwrap();
    assert_eq!(views[0].files.len(), 1, "中断前的进展被保存");
    let (id, branch) = (views[0].task.id.clone(), views[0].task.branch.clone());
    assert!(sh(Path::new(&views[0].task.worktree_path), &["log", "-1", "--format=%s"]).contains("WIP"));

    e.orch.abandon_task(&id).await.unwrap();
    assert_eq!(e.db.get_task(&id).unwrap().unwrap().status, "abandoned");
    assert!(sh(&e.repo, &["branch", "--list", &branch]).contains(&branch), "放弃后分支仍保留");
    assert!(!e.repo.join("partial.txt").exists());
    assert!(e.orch.abandon_task(&id).await.is_err());
}

#[tokio::test]
async fn non_git_directory_is_rejected_with_clear_error() {
    let e = setup().await;
    let alice = e.db.add_employee("alice", "fullstack", "p").unwrap();
    let plain = tempfile::tempdir().unwrap();
    let err = e.orch.assign(&alice, plain.path(), "x", &AutoDeny, &NoopObserver).await.unwrap_err();
    assert!(err.to_string().contains("不是 Git 仓库"), "{err}");
}

#[tokio::test]
async fn events_of_a_registered_project_carry_its_id() {
    let mut e = setup().await;
    let alice = e.db.add_employee("alice", "fullstack", "p").unwrap();
    let proj = e.db.create_project("Shop", "", e.repo.to_str().unwrap()).unwrap();
    let other = e.db.create_project("Other", "", "/somewhere/else").unwrap();

    e.orch.program = fake_cli(&e.tools, "c", "f.txt", "x", false);
    e.orch.assign(&alice, &e.repo, "做点事", &AutoDeny, &NoopObserver).await.unwrap();

    let kinds: Vec<String> = e.db.recent_events_for_project(&proj.id, 100).unwrap().into_iter().map(|x| x.kind).collect();
    for k in ["task_created", "task_started", "checkpoint_committed", "turn_result"] {
        assert!(kinds.contains(&k.to_string()), "项目群聊里应有 {k}，实际 {kinds:?}");
    }
    assert!(e.db.recent_events_for_project(&other.id, 100).unwrap().iter().all(|x| x.kind == "project_created"), "别的项目看不到这些事件");
}

/// 假 Copilot：把收到的参数追加到 args.log（每次调用一段），在工作目录写文件，输出 Copilot 风格的 JSONL。
fn fake_copilot(dir: &Path, log: &Path) -> String {
    let path = dir.join("copilot");
    std::fs::write(&path, format!(
        r#"#!/bin/sh
{{ echo "=== call"; for a in "$@"; do printf '%s\n' "$a"; done; }} >> '{log}'
echo copilot-was-here > made-by-copilot.txt
echo '{{"type":"session.mcp_server_status_changed","data":{{}},"ephemeral":true}}'
echo '{{"type":"assistant.message","data":{{"model":"claude-sonnet-5.5","content":"","toolRequests":[{{"toolCallId":"t1","name":"bash","arguments":{{"command":"ls"}}}}]}}}}'
echo '{{"type":"assistant.message","data":{{"model":"claude-sonnet-5.5","content":"Copilot 做完了","toolRequests":[]}}}}'
echo '{{"type":"session.usage_checkpoint","data":{{"totalNanoAiu":7000000000,"totalPremiumRequests":1}}}}'
echo '{{"type":"result","exitCode":0,"usage":{{"premiumRequests":1,"totalApiDurationMs":900}}}}'
"#, log = log.display())).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

struct MustNotAsk;
impl Approver for MustNotAsk {
    async fn ask(&self, _: &str, _: &ApprovalRequest, _: &str) -> Decision {
        panic!("Copilot 员工不应产生审批请求");
    }
}

#[tokio::test]
async fn copilot_employee_runs_with_preset_permissions_and_metering_in_premium_requests() {
    let mut e = setup().await;
    let log = e.tools.join("args.log");
    e.orch.copilot_program = fake_copilot(&e.tools, &log);
    assert!(e.db.add_employee_with("x", "r", "p", "gemini").is_err(), "未知引擎要被拒绝");
    let dave = e.db.add_employee_with("dave", "全栈", "你是 Dave", "copilot").unwrap();
    assert_eq!(e.db.employee_by_name("dave").unwrap().unwrap().provider, "copilot");
    assert_eq!(e.db.add_employee("erin", "全栈", "p").unwrap().provider, "claude");

    let r = e.orch.assign(&dave, &e.repo, "实现登录", &MustNotAsk, &NoopObserver).await.unwrap();
    assert_eq!(r.text, "Copilot 做完了");
    assert_eq!((r.provider.as_deref(), r.model.as_deref(), r.premium_requests), (Some("copilot"), Some("claude-sonnet-5.5"), Some(1.0)));

    // 员工在自己的 worktree 里改了文件，检查点已提交，主工作区干净
    let views = e.orch.task_views(&e.repo).await.unwrap();
    assert!(views[0].files.iter().any(|f| f.path == "made-by-copilot.txt"));
    assert_eq!(sh(&e.repo, &["status", "--porcelain"]), "");

    // 第一次调用：权限与角色设定
    let first = std::fs::read_to_string(&log).unwrap();
    assert!(first.contains("--allow-tool=write") && first.contains("--allow-tool=shell(cargo:*)") && first.contains("--deny-tool=shell(git push:*)"));
    assert!(!first.contains("allow-all"));
    assert!(first.contains("[你的角色设定]\n你是 Dave") && first.contains("实现登录"));
    let sid = e.db.list_sessions().unwrap()[0].id.clone();
    assert!(first.contains(&sid), "使用我们自己的会话 id");

    // 追加消息：同一会话，不重复角色设定
    e.orch.assign(&dave, &e.repo, "再加一个注册", &MustNotAsk, &NoopObserver).await.unwrap();
    let all = std::fs::read_to_string(&log).unwrap();
    let second = all.rsplit("=== call").next().unwrap();
    assert!(second.contains(&sid) && !second.contains("[你的角色设定]") && second.contains("再加一个注册"));

    // 用量：没有 token 与美元费用（未知，不是 0），有高级请求数与模型
    let rep = e.db.cost_report(None, 0).unwrap();
    assert_eq!((rep.totals.turns, rep.totals.turns_with_cost, rep.totals.turns_with_tokens), (2, 0, 0));
    assert_eq!(rep.totals.premium_requests, 2.0);
    assert!(rep.by_model.iter().any(|g| g.label == "claude-sonnet-5.5（Copilot）" && g.totals.turns == 2));
    assert!(rep.by_provider.iter().any(|g| g.key == "copilot" && g.label == "GitHub Copilot" && g.totals.turns == 2));
    assert_eq!(e.db.cost_calls(None, &crewforge_core::cost::CallFilter { provider: Some("copilot".into()), ..Default::default() }, 10, 0).unwrap().total, 2);
    assert_eq!(e.db.cost_calls(None, &crewforge_core::cost::CallFilter { provider: Some("claude".into()), ..Default::default() }, 10, 0).unwrap().total, 0);
    let calls = e.db.cost_calls(None, &Default::default(), 10, 0).unwrap();
    assert_eq!((calls.rows[0].provider.as_str(), calls.rows[0].premium_requests), ("copilot", Some(1.0)));
    let kinds: Vec<String> = e.db.recent_events(300).unwrap().into_iter().map(|x| x.kind).collect();
    assert!(kinds.contains(&"tool_use".into()) && !kinds.contains(&"approval_requested".into()));
}

#[tokio::test]
async fn model_comes_from_employee_then_purpose_default_and_disabled_engine_is_refused() {
    let mut e = setup().await;
    let log = e.tools.join("args2.log");
    e.orch.copilot_program = fake_copilot(&e.tools, &log);
    let dave = e.db.add_employee_with("dave", "全栈", "p", "copilot").unwrap();
    let model_args = |text: &str| -> Vec<String> {
        let call = text.rsplit("=== call").next().unwrap();
        let lines: Vec<&str> = call.lines().collect();
        lines.iter().position(|l| *l == "--model").map(|i| vec![lines[i + 1].to_string()]).unwrap_or_default()
    };

    // 没设任何模型：不传 --model
    e.orch.assign(&dave, &e.repo, "任务一", &MustNotAsk, &NoopObserver).await.unwrap();
    assert!(model_args(&std::fs::read_to_string(&log).unwrap()).is_empty());

    // 用途默认：开发任务用 grok-4.7
    e.db.set_purpose_model("copilot", "task", "grok-4.7").unwrap();
    e.orch.assign(&dave, &e.repo, "任务二", &MustNotAsk, &NoopObserver).await.unwrap();
    assert_eq!(model_args(&std::fs::read_to_string(&log).unwrap()), vec!["grok-4.7"]);

    // 员工自己指定的模型优先于用途默认
    e.db.update_employee_full(&dave.id, "全栈", "p", "copilot", "claude-haiku-5.5").unwrap();
    let dave = e.db.employee_by_name("dave").unwrap().unwrap();
    e.orch.assign(&dave, &e.repo, "任务三", &MustNotAsk, &NoopObserver).await.unwrap();
    assert_eq!(model_args(&std::fs::read_to_string(&log).unwrap()), vec!["claude-haiku-5.5"]);

    // 引擎停用：不能新建、不能切换过去，已有员工执行时被拒绝并说明原因；重新激活后恢复
    e.db.set_provider_enabled("copilot", false).unwrap();
    assert!(e.db.add_employee_with("x", "r", "p", "copilot").is_err());
    let before = std::fs::read_to_string(&log).unwrap();
    let err = e.orch.assign(&dave, &e.repo, "任务四", &MustNotAsk, &NoopObserver).await.unwrap_err().to_string();
    assert!(err.contains("已停用"), "{err}");
    assert_eq!(std::fs::read_to_string(&log).unwrap(), before, "没有启动进程");
    e.db.set_provider_enabled("copilot", true).unwrap();
    e.orch.assign(&dave, &e.repo, "任务五", &MustNotAsk, &NoopObserver).await.unwrap();
}
