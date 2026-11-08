//! Crewforge 命令行：验证核心链路（多员工并发会话、审批、恢复、事件落库）。
//!
//! 用法：
//!   crewforge employee add <名字> <角色> <身份说明>
//!   crewforge employee list
//!   crewforge run [--cwd 目录] [--max 并发数] <员工> <任务> [<员工> <任务> ...]
//!   crewforge sessions
//!   crewforge events [条数]
//!
//! 数据目录：环境变量 CREWFORGE_HOME，默认为系统数据目录下的 Crewforge。

use anyhow::{Result, bail};
use crewforge_core::db::Db;
use crewforge_core::provider::claude::{self, AgentEvent, ApprovalRequest, Decision};
use crewforge_core::runner::{Approver, Observer, TurnConfig, run_turn};
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
            AgentEvent::RateLimit { status, five_hour, seven_day } if status != "allowed" => {
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

fn home() -> PathBuf {
    std::env::var_os("CREWFORGE_HOME").map(PathBuf::from).unwrap_or_else(|| {
        dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("Crewforge")
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let db = Arc::new(Db::open(&home())?);
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
        ["run", rest @ ..] => run(db, rest).await?,
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

async fn run(db: Arc<Db>, mut args: &[&str]) -> Result<()> {
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
        let (db, sem, approver, cwd) = (db.clone(), sem.clone(), approver.clone(), cwd.clone());
        set.spawn(async move {
            let r = async {
                let _permit = sem.acquire_owned().await?;
                let cfg = TurnConfig { project_path: cwd, program: "claude".into() };
                run_turn(&db, &emp, &cfg, &prompt, &*approver, &Printer).await
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
