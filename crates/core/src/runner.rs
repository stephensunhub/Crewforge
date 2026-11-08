//! 单轮任务执行：员工 + 项目 → 会话 → CLI 进程 → 事件落库 → 审批 → 结束/崩溃恢复。

use crate::db::{Db, Employee};
use crate::policy::{Policy, PolicyDecision};
use crate::provider::claude::{AgentEvent, ApprovalRequest, ClaudeProcess, ClaudeSpec, Decision, TurnResult};
use anyhow::{Result, bail};
use serde_json::json;
use std::path::PathBuf;

/// 需要人类决定时由宿主（命令行或 UI）实现。
pub trait Approver {
    fn ask(&self, employee: &str, req: &ApprovalRequest, reason: &str) -> impl Future<Output = Decision> + Send;
}

/// 运行期事件回调，便于命令行/UI 实时展示。
pub trait Observer: Send + Sync {
    fn on_event(&self, employee: &str, event: &AgentEvent);
}

/// 不需要实时回调时使用（事件已通过 Db 钩子推送）。
pub struct NoopObserver;
impl Observer for NoopObserver {
    fn on_event(&self, _employee: &str, _event: &AgentEvent) {}
}

pub struct TurnConfig {
    /// 员工的工作目录（任务所在的 worktree）
    pub project_path: PathBuf,
    /// 关联的任务，写入事件，便于追溯每次执行属于哪个任务
    pub task_id: Option<String>,
    /// 事件归属的项目（群聊按它过滤）
    pub project_id: Option<String>,
    /// 只读模式（会议）：写入类操作直接拒绝，不询问人类
    pub read_only: bool,
    /// 会议回复时关联的会议，写入事件，便于统计成本
    pub meeting_id: Option<String>,
    /// 写入事件的任务内容；会议中完整提示很长，只记录简短标签
    pub log_prompt: Option<String>,
    /// 用途：task | meeting | minutes | onboarding，用于成本归类
    pub purpose: String,
    /// 成本归属的项目（与事件在哪个群里可见无关）
    pub attr_project: Option<String>,
    /// 提供给员工的团队协作工具（议题、记忆提议等）；None 表示不提供
    pub mcp: Option<crate::provider::claude::McpLaunch>,
    /// 测试时可替换为假的 CLI
    pub program: String,
}

pub async fn run_turn<A: Approver, O: Observer>(
    db: &Db,
    employee: &Employee,
    cfg: &TurnConfig,
    prompt: &str,
    approver: &A,
    observer: &O,
) -> Result<TurnResult> {
    let project = cfg.project_path.to_string_lossy().to_string();
    let session = db.get_or_create_session(&employee.id, &project)?;
    let (eid, sid) = (Some(employee.id.as_str()), Some(session.id.as_str()));
    let pid = cfg.project_id.as_deref();

    // 上次停留在 running，说明客户端或 CLI 曾异常退出：用 --resume 接上，并给 Agent 一条精简说明。
    let recovered = session.status == "running";
    let mut effective_prompt = prompt.to_string();
    if recovered {
        db.append_event_for(pid, "session_recovered", eid, sid, &json!({"reason": "上次任务未正常结束"}))?;
        effective_prompt = format!(
            "[系统提示] 上一次任务被意外中断。请先用只读方式核对工作区当前状态（例如 git status、git diff），\
             确认哪些修改已经完成，再继续下面的任务。\n\n{prompt}"
        );
    }

    let mut spec = ClaudeSpec::new(session.id.clone(), session.started, employee.persona.clone(), cfg.project_path.clone());
    spec.program = cfg.program.clone();
    if cfg.mcp.is_some() {
        spec.persona = format!("{}\n\n{}", spec.persona, crate::mcp::TEAM_TOOLS_HINT);
        spec.mcp = cfg.mcp.clone();
    }
    if !cfg.read_only && db.get_setting("sandbox")?.as_deref() != Some("off") {
        spec.sandbox = Some(sandbox_writable_dirs(&cfg.project_path));
    }
    let policy = if cfg.read_only { Policy::read_only(&cfg.project_path) } else { Policy::new(&cfg.project_path) };

    db.set_session_status(&session.id, "running")?;
    db.append_event_for(pid, "task_started", eid, sid, &json!({
        "prompt": cfg.log_prompt.as_deref().unwrap_or(prompt), "resume": session.started,
        "task_id": cfg.task_id, "meeting_id": cfg.meeting_id,
    }))?;

    let started_at = std::time::Instant::now();
    let mut proc = match ClaudeProcess::spawn(&spec, &effective_prompt).await {
        Ok(p) => p,
        Err(e) => {
            db.set_session_status(&session.id, "idle")?;
            db.append_event_for(pid, "task_failed", eid, sid, &json!({"error": e.to_string()}))?;
            return Err(e);
        }
    };

    let outcome: Result<TurnResult> = async {
        loop {
            let Some(ev) = proc.next_event().await? else {
                bail!("CLI 在返回结果前退出。stderr: {}", proc.stderr_tail());
            };
            observer.on_event(&employee.name, &ev);
            match &ev {
                AgentEvent::Init { .. } => db.mark_session_started(&session.id)?,
                AgentEvent::Text(t) => {
                    db.append_event_for(pid, "agent_text", eid, sid, &json!({"text": t}))?;
                }
                AgentEvent::ToolUse { name, input, .. } => {
                    db.append_event_for(pid, "tool_use", eid, sid, &json!({"tool": name, "input": input}))?;
                }
                AgentEvent::RateLimit { status, five_hour, seven_day, five_resets_at, seven_resets_at } => {
                    db.append_event_for(pid, "rate_limit", eid, sid, &json!({
                        "status": status, "five_hour": five_hour, "seven_day": seven_day,
                        "five_resets_at": five_resets_at, "seven_resets_at": seven_resets_at,
                    }))?;
                }
                AgentEvent::Approval(req) => {
                    let (decision, by, reason) = match policy.decide(&req.tool_name, &req.input) {
                        PolicyDecision::Allow(why) => (Decision::Allow, "policy", why.to_string()),
                        PolicyDecision::Deny(why) => (Decision::Deny(why.clone()), "policy", why),
                        PolicyDecision::Ask(why) => {
                            db.append_event_for(pid, "approval_requested", eid, sid,
                                &json!({"tool": req.tool_name, "input": req.input, "reason": why}))?;
                            (approver.ask(&employee.name, req, &why).await, "human", why)
                        }
                    };
                    db.append_event_for(pid, "approval_decided", eid, sid, &json!({
                        "tool": req.tool_name, "input": req.input, "decided_by": by, "reason": reason,
                        "decision": match &decision { Decision::Allow => "allow", Decision::Deny(_) => "deny" },
                    }))?;
                    proc.respond(req, &decision).await?;
                }
                AgentEvent::Result(r) => {
                    db.append_event_for(pid, "turn_result", eid, sid, &json!({
                        "task_id": cfg.task_id, "meeting_id": cfg.meeting_id, "purpose": cfg.purpose, "project_id": cfg.attr_project,
                        "duration_ms": started_at.elapsed().as_millis() as u64, "is_error": r.is_error, "subtype": r.subtype, "cost_usd": r.cost_usd,
                        "input_tokens": r.input_tokens, "output_tokens": r.output_tokens,
                        "cache_read_tokens": r.cache_read_tokens, "cache_creation_tokens": r.cache_creation_tokens,
                    }))?;
                    return Ok(r.clone());
                }
            }
        }
    }
    .await;

    match outcome {
        Ok(r) => {
            proc.finish().await?;
            db.set_session_status(&session.id, "idle")?;
            Ok(r)
        }
        Err(e) => {
            proc.kill().await;
            // 保持 running：下次启动会被识别为需要恢复
            db.append_event_for(pid, "task_failed", eid, sid, &json!({"error": e.to_string()}))?;
            Err(e)
        }
    }
}

/// 沙箱里除工作区外还需要可写的目录：worktree 的 git 公共目录（提交要写那里）与常见包缓存。
fn sandbox_writable_dirs(workdir: &std::path::Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(o) = std::process::Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(workdir)
        .output()
    {
        let d = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if o.status.success() && !d.is_empty() {
            v.push(PathBuf::from(d));
        }
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for sub in [".cargo", ".npm", ".cache", ".rustup"] {
            v.push(home.join(sub));
        }
    }
    v
}
