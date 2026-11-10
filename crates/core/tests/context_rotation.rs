//! 会话上下文过大时换新会话：用假 CLI 验证，不消耗额度。

use crewforge_core::db::Db;
use crewforge_core::provider::claude::{ApprovalRequest, Decision};
use crewforge_core::runner::{Approver, NoopObserver, TurnConfig, run_turn};
use std::os::unix::fs::PermissionsExt;

struct Deny;
impl Approver for Deny {
    async fn ask(&self, _: &str, _: &ApprovalRequest, _: &str) -> Decision {
        Decision::Deny("t".into())
    }
}

/// 假 CLI：把收到的提示写到 prompts.log，并声称上下文为 `ctx` token。
fn fake(dir: &std::path::Path, ctx: u64) -> String {
    let p = dir.join("fake");
    std::fs::write(&p, format!(
        "#!/bin/sh\nread -r line\nprintf '%s\\n' \"$line\" >> '{}/prompts.log'\n\
         echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"x\"}}'\n\
         echo '{{\"type\":\"assistant\",\"message\":{{\"usage\":{{\"input_tokens\":10,\"cache_read_input_tokens\":{ctx},\"cache_creation_input_tokens\":0}},\"content\":[{{\"type\":\"text\",\"text\":\"ok\"}}]}}}}'\n\
         echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"ok\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}'\n",
        dir.display())).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p.to_string_lossy().into_owned()
}

fn cfg(dir: &std::path::Path, program: String) -> TurnConfig {
    TurnConfig {
        project_path: dir.to_path_buf(), task_id: None, project_id: None, read_only: false, meeting_id: None,
        log_prompt: None, purpose: "task".into(), attr_project: None, mcp: None, program, copilot_program: "copilot".into(), origin: String::new(),
    }
}

#[tokio::test]
async fn rotates_session_when_context_exceeds_limit() {
    let d = tempfile::tempdir().unwrap();
    let work = d.path().join("w");
    std::fs::create_dir_all(&work).unwrap();
    let db = Db::open(&d.path().join("home")).unwrap();
    let e = db.add_employee("alice", "dev", "p").unwrap();
    db.set_setting("context_limit", "100000").unwrap();

    // 第一轮：上下文 120K，建立会话
    run_turn(&db, &e, &cfg(&work, fake(d.path(), 120_000)), "任务一", &Deny, &NoopObserver).await.unwrap();
    let first = db.get_or_create_session(&e.id, &work.to_string_lossy()).unwrap().id;
    assert_eq!(db.session_context_tokens(&first).unwrap(), Some(120_010));

    // 第二轮：超过上限 → 换新会话，提示里带说明
    run_turn(&db, &e, &cfg(&work, fake(d.path(), 50_000)), "任务二", &Deny, &NoopObserver).await.unwrap();
    let second = db.get_or_create_session(&e.id, &work.to_string_lossy()).unwrap().id;
    assert_ne!(first, second, "应当换成新会话");
    let log = std::fs::read_to_string(d.path().join("prompts.log")).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert!(!lines[0].contains("已换成新会话"));
    assert!(lines[1].contains("已换成新会话") && lines[1].contains("120K") && lines[1].contains("任务二"));

    // 第三轮：新会话上下文只有 50K，不再换
    run_turn(&db, &e, &cfg(&work, fake(d.path(), 50_000)), "任务三", &Deny, &NoopObserver).await.unwrap();
    let third = db.get_or_create_session(&e.id, &work.to_string_lossy()).unwrap().id;
    assert_eq!(second, third);
    let v = db.session_contexts().unwrap();
    assert_eq!(v.limit, 100_000);
    assert_eq!(v.sessions.len(), 1);
    assert_eq!((v.sessions[0].context_tokens, v.sessions[0].rotations), (Some(50_010), 1));
    assert_eq!(db.recent_events(50).unwrap().iter().filter(|x| x.kind == "session_rotated").count(), 1);
}

#[tokio::test]
async fn limit_zero_disables_rotation() {
    let d = tempfile::tempdir().unwrap();
    let work = d.path().join("w");
    std::fs::create_dir_all(&work).unwrap();
    let db = Db::open(&d.path().join("home")).unwrap();
    let e = db.add_employee("bob", "dev", "p").unwrap();
    db.set_setting("context_limit", "0").unwrap();
    run_turn(&db, &e, &cfg(&work, fake(d.path(), 900_000)), "a", &Deny, &NoopObserver).await.unwrap();
    let a = db.get_or_create_session(&e.id, &work.to_string_lossy()).unwrap().id;
    run_turn(&db, &e, &cfg(&work, fake(d.path(), 900_000)), "b", &Deny, &NoopObserver).await.unwrap();
    assert_eq!(a, db.get_or_create_session(&e.id, &work.to_string_lossy()).unwrap().id);
}

#[tokio::test]
async fn endpoint_employees_hand_off_with_a_summary_and_get_goal_and_memory_again() {
    use crewforge_core::endpoint::EndpointInput;
    let d = tempfile::tempdir().unwrap();
    let work = d.path().join("w");
    std::fs::create_dir_all(&work).unwrap();
    let db = Db::open(&d.path().join("home")).unwrap();
    let e = db.add_employee("local", "dev", "p").unwrap();
    let ep = db.add_endpoint(&EndpointInput { name: "本地".into(), base_url: "http://10.0.0.5:11434".into(), models: vec!["qwen3:27b".into()], extra_env: Default::default(), max_concurrency: 1, context_tokens: 32_000, limit_budget: true, token: None }).unwrap();
    db.set_employee_endpoint(&e.id, Some(&ep.id), "").unwrap();
    let e = db.employee_by_id(&e.id).unwrap().unwrap();

    // 第一轮：上下文 25K，超过窗口 3.2 万的七成（22.4K）
    run_turn(&db, &e, &cfg(&work, fake(d.path(), 25_000)), "任务一", &Deny, &NoopObserver).await.unwrap();
    // 第二轮：换会话之前先让旧会话写交接摘要（第 2 条提示），再用新会话执行任务（第 3 条提示）
    run_turn(&db, &e, &cfg(&work, fake(d.path(), 1_000)), "任务二", &Deny, &NoopObserver).await.unwrap();
    let log = std::fs::read_to_string(d.path().join("prompts.log")).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 3, "任务一、交接摘要、任务二：{log}");
    assert!(lines[1].contains("交接摘要") && lines[1].contains("不要调用任何工具"), "{}", lines[1]);
    assert!(lines[2].contains("已换成新会话") && lines[2].contains("[上一会话写给你的交接摘要]") && lines[2].contains("任务二"), "{}", lines[2]);
    let rotated = db.recent_events(50).unwrap().into_iter().find(|x| x.kind == "session_rotated").unwrap();
    assert_eq!(rotated.payload["handoff"].as_str(), Some("ok"), "摘要写进了换会话事件，便于在日志里追溯");
}

#[tokio::test]
async fn a_turn_that_goes_silent_is_terminated_instead_of_hanging_forever() {
    let d = tempfile::tempdir().unwrap();
    let work = d.path().join("w");
    std::fs::create_dir_all(&work).unwrap();
    let db = Db::open(&d.path().join("home")).unwrap();
    let e = db.add_employee("stuck", "dev", "p").unwrap();
    // 假 CLI：开始后什么都不再输出，一直挂着
    let p = d.path().join("hang");
    std::fs::write(&p, "#!/bin/sh\nread -r line\necho '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"x\"}'\nsleep 120\n").unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    db.set_setting("turn_silence_minutes", "0.03").unwrap(); // 1.8 秒

    let t0 = std::time::Instant::now();
    let err = run_turn(&db, &e, &cfg(&work, p.to_string_lossy().into_owned()), "任务", &Deny, &NoopObserver).await.unwrap_err().to_string();
    assert!(t0.elapsed().as_secs() < 30, "不该一直等：{:?}", t0.elapsed());
    assert!(err.contains("没有任何输出") && err.contains("已终止"), "{err}");
    assert!(db.recent_events(50).unwrap().iter().any(|x| x.kind == "task_failed" && x.payload["error"].as_str().unwrap_or("").contains("没有任何输出")), "失败进入事件，人工介入里能看到");
    assert!(std::process::Command::new("pgrep").args(["-f", &p.to_string_lossy()]).output().map(|o| o.stdout.is_empty()).unwrap_or(true), "挂住的进程已被杀掉");
}

#[test]
fn silence_limit_defaults_depend_on_where_the_employee_runs_and_zero_turns_it_off() {
    use crewforge_core::runner::turn_silence_limit;
    let db = Db::open_memory().unwrap();
    assert_eq!(turn_silence_limit(&db, false).unwrap().as_secs(), 30 * 60);
    assert_eq!(turn_silence_limit(&db, true).unwrap().as_secs(), 45 * 60, "本地模型慢，给得更宽");
    db.set_setting("turn_silence_minutes", "10").unwrap();
    assert_eq!(turn_silence_limit(&db, true).unwrap().as_secs(), 600);
    db.set_setting("turn_silence_minutes", "0").unwrap();
    assert!(turn_silence_limit(&db, false).is_none());
}
