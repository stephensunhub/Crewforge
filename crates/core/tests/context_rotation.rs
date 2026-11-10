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
