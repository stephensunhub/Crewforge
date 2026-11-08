//! 冒烟测试（默认忽略）：在 Crewforge 自己的仓库上跑一次真实的基线，验证真实的 cargo 输出能被解析。
//! 运行：cargo test -p crewforge-core --test real_baseline -- --ignored --nocapture
//! 会在干净检出里从头编译，耗时较长。

use crewforge_core::db::Db;
use crewforge_core::onboarding::scan::BaselineCommand;
use crewforge_core::orchestrator::Orchestrator;
use crewforge_core::workspace::WorkspaceManager;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
#[ignore]
async fn real_cargo_baseline_on_this_repository() {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
    let data = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(&data.path().join("home")).unwrap());
    let orch = Orchestrator::new(db.clone(), Arc::new(WorkspaceManager::new(&data.path().join("home"))));
    let project = db.create_project("self", "", repo.to_str().unwrap()).unwrap();
    let cmd = BaselineCommand {
        id: "t".into(), label: "core 单元测试".into(), kind: "test".into(), command: "cargo test -p crewforge-core --lib".into(),
        reason: String::new(), default_on: true,
    };
    let run = orch.run_baseline(&project.id, vec![cmd], Duration::from_secs(900)).await.unwrap();
    let r = &run.results[0];
    println!("状态 {} · 退出码 {:?} · {}s · 已解析 {} · 失败项 {:?}", r.status, r.exit_code, r.duration_ms / 1000, r.parsed, r.failed_tests);
    println!("输出末尾：\n{}", r.output_tail.lines().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"));
    assert_eq!(r.status, "passed", "{}", r.output_tail);
    assert!(r.parsed && r.failed_tests.is_empty());
    assert!(r.output_tail.contains("test result: ok"));
}
