//! 员工连自定义 LLM 端点：假 CLI 把收到的环境变量和 --model 写下来，验证注入范围。

use crewforge_core::db::Db;
use crewforge_core::endpoint::EndpointInput;
use crewforge_core::provider::claude::{ApprovalRequest, Decision};
use crewforge_core::runner::{Approver, NoopObserver, TurnConfig, run_turn};
use std::os::unix::fs::PermissionsExt;

struct Deny;
impl Approver for Deny {
    async fn ask(&self, _: &str, _: &ApprovalRequest, _: &str) -> Decision {
        Decision::Deny("t".into())
    }
}

/// 假 CLI：记录 ANTHROPIC_* 环境变量与启动参数，上下文声称 `ctx`。
fn fake(dir: &std::path::Path, ctx: u64) -> String {
    let p = dir.join("fake");
    std::fs::write(&p, format!(
        "#!/bin/sh\nread -r line\n{{ env | grep '^ANTHROPIC_' | sort; echo \"ARGS=$*\"; echo ---; }} >> '{}/env.log'\n\
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
async fn only_employees_on_an_endpoint_get_its_env_and_model() {
    let d = tempfile::tempdir().unwrap();
    let work = d.path().join("w");
    std::fs::create_dir_all(&work).unwrap();
    let db = Db::open(&d.path().join("home")).unwrap();
    let lead = db.add_employee("lead", "负责人", "p").unwrap();
    let dev = db.add_employee("dev", "工程师", "p").unwrap();
    let ep = db.add_endpoint(&EndpointInput {
        name: "本地".into(), base_url: "http://10.0.0.5:11434".into(), models: vec!["qwen3:27b".into()],
        extra_env: Default::default(), max_concurrency: 1, context_tokens: 0, token: None,
    }).unwrap();
    db.set_employee_endpoint(&dev.id, Some(&ep.id), "").unwrap();
    let dev = db.employee_by_id(&dev.id).unwrap().unwrap();
    // 用途默认模型是 Claude 别名；对端点员工不能生效
    db.set_purpose_model("claude", "task", "opus").unwrap();

    run_turn(&db, &lead, &cfg(&work, fake(d.path(), 10)), "a", &Deny, &NoopObserver).await.unwrap();
    run_turn(&db, &dev, &cfg(&work, fake(d.path(), 10)), "b", &Deny, &NoopObserver).await.unwrap();

    let log = std::fs::read_to_string(d.path().join("env.log")).unwrap();
    let runs: Vec<&str> = log.split("---\n").filter(|s| !s.trim().is_empty()).collect();
    assert_eq!(runs.len(), 2);
    assert!(!runs[0].contains("ANTHROPIC_BASE_URL") && runs[0].contains("--model opus"), "负责人走订阅：不注入，仍用 opus\n{}", runs[0]);
    assert!(runs[1].contains("ANTHROPIC_BASE_URL=http://10.0.0.5:11434"), "{}", runs[1]);
    assert!(runs[1].contains("ANTHROPIC_DEFAULT_HAIKU_MODEL=qwen3:27b") && runs[1].contains("--model qwen3:27b"), "{}", runs[1]);
    assert!(!runs[1].contains("--model opus"));
}

#[tokio::test]
async fn small_endpoint_window_rotates_the_session_early() {
    let d = tempfile::tempdir().unwrap();
    let work = d.path().join("w");
    std::fs::create_dir_all(&work).unwrap();
    let db = Db::open(&d.path().join("home")).unwrap();
    let dev = db.add_employee("dev", "工程师", "p").unwrap();
    let ep = db.add_endpoint(&EndpointInput {
        name: "本地".into(), base_url: "http://10.0.0.5:11434".into(), models: vec!["qwen3:27b".into()],
        extra_env: Default::default(), max_concurrency: 1, context_tokens: 32_000, token: None,
    }).unwrap();
    db.set_employee_endpoint(&dev.id, Some(&ep.id), "").unwrap();
    let dev = db.employee_by_id(&dev.id).unwrap().unwrap();
    // 默认上限 15 万，不会触发；窗口 3.2 万的七成约 2.2 万，会触发
    run_turn(&db, &dev, &cfg(&work, fake(d.path(), 25_000)), "一", &Deny, &NoopObserver).await.unwrap();
    let first = db.get_or_create_session(&dev.id, &work.to_string_lossy()).unwrap().id;
    run_turn(&db, &dev, &cfg(&work, fake(d.path(), 1_000)), "二", &Deny, &NoopObserver).await.unwrap();
    assert_ne!(first, db.get_or_create_session(&dev.id, &work.to_string_lossy()).unwrap().id);
}

#[tokio::test]
async fn domains_added_to_the_global_whitelist_reach_the_sandbox_settings_of_the_next_run() {
    let d = tempfile::tempdir().unwrap();
    let work = d.path().join("w");
    std::fs::create_dir_all(&work).unwrap();
    let db = Db::open(&d.path().join("home")).unwrap();
    let e = db.add_employee("dev", "工程师", "p").unwrap();
    db.add_sandbox_domain("https://Extra.Example.com/").unwrap();
    run_turn(&db, &e, &cfg(&work, fake(d.path(), 10)), "a", &Deny, &NoopObserver).await.unwrap();
    let log = std::fs::read_to_string(d.path().join("env.log")).unwrap();
    assert!(log.contains("extra.example.com") && log.contains("proxy.golang.org"), "沙箱设置里应有内置和自己加的域名：{log}");
}
