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

    // 上次停留在 running，说明客户端或 CLI 曾异常退出：用 --resume 接上，并给 Agent 一条精简说明。
    let recovered = session.status == "running";
    let mut effective_prompt = prompt.to_string();
    if recovered {
        db.append_event("session_recovered", eid, sid, &json!({"reason": "上次任务未正常结束"}))?;
        effective_prompt = format!(
            "[系统提示] 上一次任务被意外中断。请先用只读方式核对工作区当前状态（例如 git status、git diff），\
             确认哪些修改已经完成，再继续下面的任务。\n\n{prompt}"
        );
    }

    let mut spec = ClaudeSpec::new(session.id.clone(), session.started, employee.persona.clone(), cfg.project_path.clone());
    spec.program = cfg.program.clone();
    let policy = Policy::new(&cfg.project_path);

    db.set_session_status(&session.id, "running")?;
    db.append_event("task_started", eid, sid, &json!({"prompt": prompt, "resume": session.started, "task_id": cfg.task_id}))?;

    let mut proc = match ClaudeProcess::spawn(&spec, &effective_prompt).await {
        Ok(p) => p,
        Err(e) => {
            db.set_session_status(&session.id, "idle")?;
            db.append_event("task_failed", eid, sid, &json!({"error": e.to_string()}))?;
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
                    db.append_event("agent_text", eid, sid, &json!({"text": t}))?;
                }
                AgentEvent::ToolUse { name, input, .. } => {
                    db.append_event("tool_use", eid, sid, &json!({"tool": name, "input": input}))?;
                }
                AgentEvent::RateLimit { status, five_hour, seven_day } => {
                    db.append_event("rate_limit", eid, sid,
                        &json!({"status": status, "five_hour": five_hour, "seven_day": seven_day}))?;
                }
                AgentEvent::Approval(req) => {
                    let (decision, by, reason) = match policy.decide(&req.tool_name, &req.input) {
                        PolicyDecision::Allow(why) => (Decision::Allow, "policy", why.to_string()),
                        PolicyDecision::Ask(why) => {
                            db.append_event("approval_requested", eid, sid,
                                &json!({"tool": req.tool_name, "input": req.input, "reason": why}))?;
                            (approver.ask(&employee.name, req, &why).await, "human", why)
                        }
                    };
                    db.append_event("approval_decided", eid, sid, &json!({
                        "tool": req.tool_name, "input": req.input, "decided_by": by, "reason": reason,
                        "decision": match &decision { Decision::Allow => "allow", Decision::Deny(_) => "deny" },
                    }))?;
                    proc.respond(req, &decision).await?;
                }
                AgentEvent::Result(r) => {
                    db.append_event("turn_result", eid, sid, &json!({
                        "task_id": cfg.task_id, "is_error": r.is_error, "subtype": r.subtype, "cost_usd": r.cost_usd,
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
            db.append_event("task_failed", eid, sid, &json!({"error": e.to_string()}))?;
            Err(e)
        }
    }
}
