//! Crewforge 命令行：验证核心链路（多员工并发会话、审批、恢复、事件落库）。
//!
//! 用法：
//!   crewforge employee add <名字> <角色> <身份说明>
//!   crewforge employee list
//!   crewforge run [--cwd 目录] [--max 并发数] <员工> <任务> [<员工> <任务> ...]
//!   crewforge sessions
//!   crewforge mcp-serve --data-dir D --employee E --project P --origin O   （内部使用：团队协作工具的 MCP 服务器）
//!   crewforge cost [天数]                 成本与额度汇总（费用为 CLI 报告的等值估算，未知的数据不计入）
//!   crewforge scan [目录]                 只读扫描项目（技术栈、结构、Git 历史、建议的基线命令）
//!   crewforge init-repo [目录]            初始化 Git 仓库（worktree 需要至少一个提交）
//!   crewforge tasks [目录]                列出项目的任务、改动文件、冲突预检
//!   crewforge diff <任务ID>               查看任务改动
//!   crewforge merge <任务ID>              合并任务（squash）
//!   crewforge abandon <任务ID>            放弃任务（保留分支）
//!   crewforge events [条数]
//!
//! 数据目录：环境变量 CREWFORGE_HOME，默认为系统数据目录下的 Crewforge。

use anyhow::{Result, bail};
use crewforge_core::db::Db;
use crewforge_core::provider::claude::{self, AgentEvent, ApprovalRequest, Decision};
use crewforge_core::orchestrator::Orchestrator;
use crewforge_core::runner::{Approver, Observer};
use crewforge_core::workspace::{MergeOutcome, WorkspaceManager};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{Mutex, Semaphore};

struct TerminalApprover {
    /// 多个员工并发时，同一时刻只向人类提一个问题
    gate: Mutex<()>,
}

impl Approver for TerminalApprover {
    async fn ask(&self, employee: &str, req: &ApprovalRequest, reason: &str) -> Decision {
        let _g = self.gate.lock().await;
        let input = req.input.to_string();
        let input = if input.chars().count() > 300 { format!("{}…", input.chars().take(300).collect::<String>()) } else { input };
        eprintln!("\n⚠️  [{employee}] 请求审批: {} — {reason}\n    参数: {input}", req.tool_name);
        eprint!("    允许? [y/N] ");
        let mut line = String::new();
        let mut stdin = BufReader::new(tokio::io::stdin());
        let _ = stdin.read_line(&mut line).await;
        if matches!(line.trim().to_lowercase().as_str(), "y" | "yes") {
            Decision::Allow
        } else {
            Decision::Deny("人类拒绝了该操作".into())
        }
    }
}

struct Printer;
impl Observer for Printer {
    fn on_event(&self, who: &str, e: &AgentEvent) {
        match e {
            AgentEvent::Text(t) => eprintln!("💬 [{who}] {t}"),
            AgentEvent::ToolUse { name, .. } => eprintln!("🔧 [{who}] 调用 {name}"),
            AgentEvent::RateLimit { status, five_hour, seven_day, .. } if status != "allowed" => {
                eprintln!("📊 [{who}] 额度状态 {status}（5h {five_hour:?} / 7d {seven_day:?}）")
            }
            AgentEvent::Result(r) => eprintln!(
                "✅ [{who}] 完成 cost≈{:?} in={:?} out={:?}",
                r.cost_usd, r.input_tokens, r.output_tokens
            ),
            _ => {}
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // MCP 服务器模式：由 Claude Code 启动，stdout 只能输出协议消息，所以要在任何其他初始化之前处理
    if args.first().map(String::as_str) == Some("mcp-serve") {
        return crewforge_core::mcp::run_from_args(&args[1..]);
    }
    let data_dir = crewforge_core::db::default_data_dir();
    let db = Arc::new(Db::open(&data_dir)?);
    let orch = Arc::new(Orchestrator::new(db.clone(), Arc::new(WorkspaceManager::new(&data_dir))));
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["employee", "add", name, role, persona] => {
            let e = db.add_employee(name, role, persona)?;
            println!("已创建员工 {} ({})", e.name, e.id);
        }
        ["employee", "list"] => {
            for e in db.list_employees()? {
                println!("{:<10} {:<10} {}", e.name, e.role, e.persona);
            }
        }
        ["sessions"] => {
            for s in db.list_sessions()? {
                println!("{} employee={} status={} started={} {}", s.id, s.employee_id, s.status, s.started, s.project_path);
            }
        }
        ["events"] => print_events(&db, 20)?,
        ["events", n] => print_events(&db, n.parse()?)?,
        ["run", rest @ ..] => run(db, orch, rest).await?,
        ["cost"] => cost(&db, None)?,
        ["cost", d] => cost(&db, Some(d.parse()?))?,
        ["scan"] => scan(&std::env::current_dir()?).await?,
        ["scan", dir] => scan(&PathBuf::from(dir)).await?,
        ["init-repo"] => init_repo(&orch, &std::env::current_dir()?).await?,
        ["init-repo", dir] => init_repo(&orch, &PathBuf::from(dir)).await?,
        ["tasks"] => tasks(&orch, &std::env::current_dir()?).await?,
        ["tasks", dir] => tasks(&orch, &PathBuf::from(dir)).await?,
        ["diff", id] => print!("{}", orch.task_diff(id).await?),
        ["abandon", id] => {
            orch.abandon_task(id).await?;
            println!("已放弃，分支保留");
        }
        ["merge", id] => match orch.merge_task(id).await? {
            MergeOutcome::Merged { commit } => println!("已合并: {commit}"),
            MergeOutcome::Empty => println!("没有任何改动，无需合并"),
            MergeOutcome::Conflict { files } => {
                eprintln!("存在冲突，未合并: {}", files.join(", "));
                std::process::exit(1);
            }
            MergeOutcome::Blocked { reason } => {
                eprintln!("无法合并: {reason}");
                std::process::exit(1);
            }
        },
        _ => {
            eprintln!("用法见 crates/cli/src/main.rs 顶部注释");
            std::process::exit(2);
        }
    }
    Ok(())
}

fn print_events(db: &Db, n: usize) -> Result<()> {
    for e in db.recent_events(n)? {
        println!("#{} {} {} {}", e.id, e.kind, e.employee_id.unwrap_or_default(), e.payload);
    }
    Ok(())
}

fn cost(db: &Db, days: Option<i64>) -> Result<()> {
    let since = days.map(|d| crewforge_core::db::now_ms() - d * 86_400_000);
    let r = db.cost_report(since, 0)?;
    let t = &r.totals;
    println!("调用 {} 次（{} 次出错）· 估算费用 {} · 上下文 token {} · 输出 {}",
        t.turns, t.errors,
        if t.turns_with_cost == 0 { "未知".to_string() } else { format!("${:.3}", t.cost_usd) },
        t.context_tokens(), t.output_tokens);
    for (title, groups) in [("员工", &r.by_employee), ("项目", &r.by_project), ("用途", &r.by_purpose)] {
        for g in groups {
            println!("  [{title}] {:<14} {} 次 ${:.3} 上下文 {}", g.label, g.totals.turns, g.totals.cost_usd, g.totals.context_tokens());
        }
    }
    if let Some(q) = &r.quota_now {
        println!("额度：5h {:?} / 7d {:?}（{}）", q.five_hour, q.seven_day, q.status);
    }
    let e = &r.efficiency;
    println!("效率：平均上下文 {:?} · 缓存命中 {:?} · 会话复用 {}/{} · 失败 {} · 恢复 {}",
        e.avg_context_per_turn.map(|v| v.round()), e.cache_hit_ratio.map(|v| (v * 100.0).round()), e.resumed_starts, e.total_starts, e.failed_runs, e.recovered_sessions);
    Ok(())
}

async fn scan(dir: &PathBuf) -> Result<()> {
    let r = crewforge_core::onboarding::scan::scan_project(&dir.canonicalize()?).await?;
    print!("{}", crewforge_core::onboarding::summarize_scan(&r));
    println!("扫描了 {} 个文件{}", r.total_files, if r.truncated { "（已截断）" } else { "" });
    for c in &r.commands {
        println!("建议命令 [{}] {}{}", c.kind, c.command, if c.default_on { "" } else { "（默认不勾选）" });
    }
    for n in &r.notes {
        println!("注意：{n}");
    }
    Ok(())
}

async fn init_repo(orch: &Orchestrator, dir: &PathBuf) -> Result<()> {
    orch.ws.init_repo(dir).await?;
    println!("已初始化 Git 仓库: {}", dir.display());
    Ok(())
}

async fn tasks(orch: &Orchestrator, dir: &PathBuf) -> Result<()> {
    for v in orch.task_views(dir).await? {
        let merge = match &v.merge {
            Some(m) if m.clean => "可合并".to_string(),
            Some(m) => format!("冲突: {}", m.conflicts.join(",")),
            None => "-".into(),
        };
        println!("{} [{}] {} · {} · 改动 {} 个文件 · {}", v.task.id, v.task.status, v.employee_name, v.task.title, v.files.len(), merge);
        for o in &v.overlaps {
            println!("    ⚠ 与其他任务同时修改: {}", o.path);
        }
    }
    Ok(())
}

async fn run(db: Arc<Db>, orch: Arc<Orchestrator>, mut args: &[&str]) -> Result<()> {
    let mut cwd = std::env::current_dir()?;
    let mut max = 3usize;
    loop {
        match args {
            ["--cwd", d, rest @ ..] => { cwd = PathBuf::from(d).canonicalize()?; args = rest; }
            ["--max", n, rest @ ..] => { max = n.parse()?; args = rest; }
            _ => break,
        }
    }
    if args.is_empty() || args.len() % 2 != 0 {
        bail!("需要成对的 <员工> <任务> 参数");
    }
    let version = claude::detect("claude").await?;
    eprintln!("使用 {version}，工作目录 {}，并发上限 {max}", cwd.display());

    let sem = Arc::new(Semaphore::new(max.max(1)));
    let approver = Arc::new(TerminalApprover { gate: Mutex::new(()) });
    let mut set = tokio::task::JoinSet::new();
    for pair in args.chunks(2) {
        let (name, prompt) = (pair[0].to_string(), pair[1].to_string());
        let Some(emp) = db.employee_by_name(&name)? else { bail!("员工不存在: {name}") };
        let (orch, sem, approver, cwd) = (orch.clone(), sem.clone(), approver.clone(), cwd.clone());
        set.spawn(async move {
            let r = async {
                let _permit = sem.acquire_owned().await?;
                orch.assign(&emp, &cwd, &prompt, &*approver, &Printer).await
            }
            .await;
            (name, r)
        });
    }
    let mut failed = false;
    while let Some(res) = set.join_next().await {
        match res {
            Ok((name, Ok(_))) => eprintln!("— {name} 本轮结束"),
            Ok((name, Err(e))) => { failed = true; eprintln!("❌ {name} 失败: {e}") }
            Err(e) => { failed = true; eprintln!("❌ 任务异常: {e}") }
        }
    }
    if failed { std::process::exit(1); }
    Ok(())
}
