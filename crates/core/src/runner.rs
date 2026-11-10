//! 单轮任务执行：员工 + 项目 → 会话 → CLI 进程 → 事件落库 → 审批 → 结束/崩溃恢复。

use crate::db::{Db, Employee};
use crate::policy::{Policy, PolicyDecision};
use crate::provider::AgentProcess;
use crate::provider::claude::{AgentEvent, ApprovalRequest, ClaudeProcess, ClaudeSpec, Decision, TurnResult};
use crate::provider::copilot::{CopilotProcess, CopilotSpec};
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
    /// Copilot 员工使用的 CLI，默认 `copilot`
    pub copilot_program: String,
    /// 谁发起的这次执行：空 = 人（群里显示“你 → @员工”）；"autopilot" = 自主推进里 AI 负责人派的活；"system" = 系统或负责人的后台工作（会议、裁决、整理等）
    pub origin: String,
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
    let mut session = db.get_or_create_session(&employee.id, &project)?;
    let pid0 = cfg.project_id.as_deref();
    // 员工连的 LLM 端点（没选就是本机 Claude 订阅，什么都不注入）
    let resolved = db.resolve_endpoint(employee)?;
    // 同一个端点同时最多 max_concurrency 个员工在用（本地模型通常只能串行）；等待期间会话仍是空闲。
    // 放在换会话之前：交接摘要也要调用这个端点
    let _slot = match &resolved {
        Some(r) => Some(crate::endpoint::acquire_slot(&r.endpoint).await),
        None => None,
    };
    // 会话上下文过大时换新会话：每一步都要重读整个上下文，越大越贵、越容易跑偏。
    let mut rotated_from: Option<u64> = None;
    let mut handoff: Option<String> = None;
    let limit = rotation_limit(db, employee);
    if limit > 0 && session.started && session.status != "running" {
        if let Some(ctx) = db.session_context_tokens(&session.id)? {
            if ctx > limit {
                // 自建端点的员工换会话前先写交接摘要；订阅员工窗口大、换得少，不额外多花一次调用
                if let Some(r) = &resolved {
                    handoff = handoff_summary(employee, cfg, &session.id, r).await;
                }
                db.reset_session(&session.id)?;
                db.append_event_for(pid0, "session_rotated", Some(employee.id.as_str()), Some(session.id.as_str()),
                    &json!({"context_tokens": ctx, "limit": limit, "handoff": handoff}))?;
                session = db.get_or_create_session(&employee.id, &project)?;
                rotated_from = Some(ctx);
            }
        }
    }
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

    if let Some(ctx) = rotated_from {
        effective_prompt = format!(
            "[系统提示] 你的上一个会话上下文已达约 {}K，为节省用量已换成新会话，之前的对话不再可见。\
             请以下面的任务、项目记忆和工作区当前状态为准，先用只读方式（git status、git log）核对进度，再继续。{}\n\n{effective_prompt}",
            ctx / 1000,
            handoff.as_deref().map(|h| format!("\n\n[上一会话写给你的交接摘要]\n{h}")).unwrap_or_default()
        );
    }
    let mut spec = ClaudeSpec::new(session.id.clone(), session.started, employee.persona.clone(), cfg.project_path.clone());
    spec.program = cfg.program.clone();
    if !db.provider_enabled(&employee.provider)? {
        bail!("执行引擎 {} 已停用，请到“模型”页重新激活，或把 {} 切换到别的引擎", employee.provider, employee.name);
    }
    // 模型：员工指定 > 该引擎在这个用途下的默认模型 > 引擎默认
    spec.model = db.effective_model(&employee.provider, &employee.model, &cfg.purpose)?;
    if let Some(r) = &resolved {
        // 端点上的模型名与 Claude 的别名（opus/sonnet…）不通用，用途默认模型不适用
        spec.model = Some(r.model.clone());
        spec.env = r.env.clone();
    }
    if cfg.mcp.is_some() {
        spec.persona = format!("{}\n\n{}", spec.persona, crate::mcp::TEAM_TOOLS_HINT);
        spec.mcp = cfg.mcp.clone();
    }
    let copilot = employee.provider == "copilot";
    if !copilot && !cfg.read_only && db.get_setting("sandbox")?.as_deref() != Some("off") {
        spec.sandbox = Some(sandbox_writable_dirs(&cfg.project_path));
        spec.extra_domains = db.custom_sandbox_domains();
    }
    if crate::protocol::enabled(db) {
        let style = crate::protocol::style_for(&cfg.purpose);
        if !style.is_empty() {
            spec.persona = format!("{}\n\n{}", spec.persona, style);
        }
    }
    let policy = if cfg.read_only { Policy::read_only(&cfg.project_path) }
    else if spec.sandbox.is_some() { Policy::new(&cfg.project_path).sandboxed() }
    else { Policy::new(&cfg.project_path) };

    db.set_session_status(&session.id, "running")?;
    db.append_event_for(pid, "task_started", eid, sid, &json!({
        "prompt": cfg.log_prompt.as_deref().unwrap_or(prompt), "resume": session.started, "origin": cfg.origin, "project": cfg.attr_project,
        "task_id": cfg.task_id, "meeting_id": cfg.meeting_id,
    }))?;

    let started_at = std::time::Instant::now();
    let mut last_ctx: Option<u64> = None;
    let spawned: Result<AgentProcess> = if copilot {
        // Copilot：权限在启动时定好（见 provider::copilot），没有审批通道；设定、协作工具、规范与 Claude 员工一致
        let mut cs = CopilotSpec::new(session.id.clone(), session.started, spec.persona.clone(), cfg.project_path.clone());
        // 设置里指定了路径（应用自带安装的 copilot）就用它，否则用默认的 `copilot`（需要在 PATH 里）
        cs.program = db.get_setting("copilot_program")?.filter(|p| !p.trim().is_empty()).unwrap_or_else(|| cfg.copilot_program.clone());
        cs.mcp = spec.mcp.clone();
        cs.model = spec.model.clone();
        cs.read_only = cfg.read_only;
        CopilotProcess::spawn(&cs, &effective_prompt).await.map(AgentProcess::Copilot)
    } else {
        ClaudeProcess::spawn(&spec, &effective_prompt).await.map(AgentProcess::Claude)
    };
    let mut proc = match spawned {
        Ok(p) => p,
        Err(e) => {
            db.set_session_status(&session.id, "idle")?;
            db.append_event_for(pid, "task_failed", eid, sid, &json!({"error": e.to_string()}))?;
            return Err(e);
        }
    };

    let silence = turn_silence_limit(db, resolved.is_some());
    let outcome: Result<TurnResult> = async {
        loop {
            // 看门狗：连续太久没有任何输出就终止，不让一轮执行永远挂着（等人审批的时间在下面单独处理，不算）
            let next = match silence {
                Some(d) => match tokio::time::timeout(d, proc.next_event()).await {
                    Ok(r) => r?,
                    Err(_) => bail!("连续 {:.0} 分钟没有任何输出，已终止这一轮（可能卡住了）。工作区里已有的改动会保留", d.as_secs_f64() / 60.0),
                },
                None => proc.next_event().await?,
            };
            let Some(ev) = next else {
                bail!("CLI 在返回结果前退出。stderr: {}", proc.stderr_tail());
            };
            observer.on_event(&employee.name, &ev);
            match &ev {
                AgentEvent::Init { .. } => db.mark_session_started(&session.id)?,
                AgentEvent::Context(n) => last_ctx = Some(*n),
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
                        "duration_ms": started_at.elapsed().as_millis() as u64, "is_error": r.is_error, "subtype": r.subtype,
                        // 自建端点没有按量计费：Claude Code 按自己的价目表估算出的金额没有意义，记 0
                        "cost_usd": if resolved.is_some() { Some(0.0) } else { r.cost_usd }, "endpoint": resolved.as_ref().map(|x| x.endpoint.name.clone()),
                        "input_tokens": r.input_tokens, "output_tokens": r.output_tokens,
                        "cache_read_tokens": r.cache_read_tokens, "cache_creation_tokens": r.cache_creation_tokens,
                        "models": r.models, "ttft_ms": r.ttft_ms,
                        "provider": r.provider, "model": r.model, "premium_requests": r.premium_requests, "nano_aiu": r.nano_aiu,
                        "context_tokens": last_ctx,
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

/// 一轮执行里，CLI 连续这么久没有任何输出就当作卡住，终止这一轮（走失败流程，由负责人重试或升级）。
/// 等人审批的时间不算。默认：订阅员工 30 分钟、自建端点（本地模型慢）45 分钟；设置项 `turn_silence_minutes` 可改（0 = 不限）。
pub fn turn_silence_limit(db: &Db, on_endpoint: bool) -> Option<std::time::Duration> {
    let m = db.get_setting("turn_silence_minutes").ok().flatten().and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(if on_endpoint { 45.0 } else { 30.0 });
    (m > 0.0).then(|| std::time::Duration::from_secs_f64(m * 60.0))
}

/// 这位员工的会话换新阈值：全局上限，连了自建端点时取端点窗口的七成（本地模型窗口小得多）。0 表示不限。
pub fn rotation_limit(db: &Db, employee: &Employee) -> u64 {
    let mut limit = context_limit(db);
    if let Ok(Some(r)) = db.resolve_endpoint(employee) {
        let cap = (r.endpoint.context_tokens as u64) * 7 / 10;
        if cap > 0 && (limit == 0 || cap < limit) {
            limit = cap;
        }
    }
    limit
}

/// 下一次执行会不会用一个全新的会话（还没建立，或上下文已超限要换新）。
/// 全新会话里没有之前的对话，所以派活时要把项目目标和记忆重新带上。
pub fn session_restarts(db: &Db, employee: &Employee, project_path: &str) -> bool {
    let Ok(s) = db.get_or_create_session(&employee.id, project_path) else { return false };
    if !s.started {
        return true;
    }
    let limit = rotation_limit(db, employee);
    limit > 0 && s.status != "running" && db.session_context_tokens(&s.id).ok().flatten().is_some_and(|c| c > limit)
}

/// 换会话之前，让旧会话自己写一份交接摘要（做到哪、定了什么、试过什么失败了、下一步），
/// 新会话据此接着做，不容易跑偏。不允许调用任何工具；超时或失败就不带摘要。
async fn handoff_summary(employee: &Employee, cfg: &TurnConfig, old_session: &str, r: &crate::endpoint::Resolved) -> Option<String> {
    let mut spec = ClaudeSpec::new(old_session.to_string(), true, employee.persona.clone(), cfg.project_path.clone());
    spec.program = cfg.program.clone();
    spec.model = Some(r.model.clone());
    spec.env = r.env.clone();
    let prompt = "[系统提示] 你的会话上下文即将换成新会话，之后你的对话记录不再可见。请不要调用任何工具，直接写一份给“新会话里的你”的交接摘要，500 字以内，包含：\n\
        ① 当前任务的目标和做到哪了；② 已经确定的决定和原因；③ 试过但失败或被否决的做法（避免重复）；④ 下一步具体要做什么；⑤ 需要注意的坑或未验证的假设。\n只写事实，不要客套。";
    let mut p = ClaudeProcess::spawn(&spec, prompt).await.ok()?;
    let waited = tokio::time::timeout(std::time::Duration::from_secs(900), async {
        loop {
            match p.next_event().await {
                Ok(Some(AgentEvent::Result(res))) => return Some(res),
                Ok(Some(AgentEvent::Approval(req))) => { let _ = p.respond(&req, &Decision::Deny("交接摘要不需要调用工具，请直接回答".into())).await; }
                Ok(Some(_)) => {}
                _ => return None,
            }
        }
    }).await;
    p.kill().await;
    let res = waited.ok().flatten()?;
    (!res.is_error && !res.text.trim().is_empty()).then(|| res.text.trim().chars().take(1500).collect())
}

/// 会话上下文上限（token）。超过后下一次任务换新会话；设置项 `context_limit`，0 表示不限，默认 150000。
pub fn context_limit(db: &Db) -> u64 {
    db.get_setting("context_limit").ok().flatten().and_then(|v| v.trim().parse().ok()).unwrap_or(150_000)
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
        for sub in [".cargo", ".npm", ".cache", ".rustup", "Library/Caches", "go"] {
            v.push(home.join(sub));
        }
    }
    v
}
