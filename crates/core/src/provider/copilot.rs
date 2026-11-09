//! GitHub Copilot CLI 适配器（`copilot -p ... --output-format json`，每行一个 JSON 事件）。
//!
//! 与 Claude 适配器的关键差异：
//! - 非交互模式没有“向人询问权限”的通道：权限在启动时用 `--allow-tool` / `--deny-tool` 一次定好，
//!   没有列入的操作会被 CLI 直接拒绝，模型能看到被拒的原因并改用别的办法。所以 Copilot 员工不会产生审批请求。
//! - 没有 `--append-system-prompt`：员工的角色设定放在新会话第一条消息的开头。
//! - 用量按“高级请求（premium requests）”和 AIU 计量，没有 token 和美元费用，这些数据记为“未知”，不会当成 0。
//! - 团队协作工具（MCP）用 `--additional-mcp-config` 提供，与 Claude 员工用的是同一个 `crewforge mcp-serve`。

use super::claude::{AgentEvent, ApprovalRequest, Decision, McpLaunch, TurnResult};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};

#[derive(Debug, Clone)]
pub struct CopilotSpec {
    pub session_id: String,
    pub resume: bool,
    pub persona: String,
    pub cwd: PathBuf,
    /// 可执行文件，默认 `copilot`
    pub program: String,
    pub mcp: Option<McpLaunch>,
    /// 只读（会议）：不允许写入，只允许查看类命令
    pub read_only: bool,
}

/// 开发任务里预先批准的命令（带任意参数）。没有列入的命令会被拒绝；需要删除文件等操作时请人类处理。
const DEV_COMMANDS: &[&str] = &[
    "ls", "pwd", "cat", "head", "tail", "wc", "grep", "rg", "find", "sort", "diff", "echo", "printf", "test", "mkdir", "touch", "cp", "mv", "sed", "awk",
    "git add", "git commit", "git status", "git diff", "git log", "git branch", "git show", "git restore", "git checkout", "git merge", "git stash",
    "cargo", "rustc", "rustfmt", "npm", "npx", "node", "tsc", "python3", "python", "pytest", "go", "make",
];
/// 只读（会议）里允许的命令。
const READ_ONLY_COMMANDS: &[&str] = &["ls", "pwd", "cat", "head", "tail", "wc", "grep", "rg", "git status", "git diff", "git log", "git show"];
/// 无论如何都拒绝（拒绝规则优先于一切允许规则）。
const ALWAYS_DENY: &[&str] = &["git push", "sudo", "ssh", "scp", "curl", "wget", "launchctl", "osascript", "rm"];

impl CopilotSpec {
    pub fn new(session_id: String, resume: bool, persona: String, cwd: PathBuf) -> Self {
        Self { session_id, resume, persona, cwd, program: "copilot".into(), mcp: None, read_only: false }
    }

    /// 权限参数：见模块说明。
    pub fn permission_args(&self) -> Vec<String> {
        let mut a = Vec::new();
        let allow = |a: &mut Vec<String>, rule: String| a.push(format!("--allow-tool={rule}"));
        let deny = |a: &mut Vec<String>, rule: String| a.push(format!("--deny-tool={rule}"));
        let cmds = if self.read_only { READ_ONLY_COMMANDS } else { DEV_COMMANDS };
        for c in cmds {
            allow(&mut a, format!("shell({c}:*)"));
        }
        if self.read_only {
            deny(&mut a, "write".into());
        } else {
            // 写入只限工作区：CLI 默认的路径校验只放行当前目录和临时目录，这里不放宽
            allow(&mut a, "write".into());
        }
        for c in ALWAYS_DENY {
            deny(&mut a, format!("shell({c}:*)"));
        }
        if self.mcp.is_some() {
            allow(&mut a, "crewforge".into());
        }
        a
    }

    pub fn args(&self, prompt: &str) -> Vec<String> {
        let mut a: Vec<String> = [
            "--output-format", "json",
            "--disable-client-telemetry",
            "--disable-builtin-mcps",
            "--no-custom-instructions",
            "--log-level", "none",
        ]
        .map(String::from)
        .into();
        a.push("--session-id".into());
        a.push(self.session_id.clone());
        if let Some(m) = &self.mcp {
            a.push("--additional-mcp-config".into());
            a.push(m.config_json());
        }
        a.extend(self.permission_args());
        // 新会话：把角色设定放在第一条消息开头（没有单独的系统提示参数）
        let text = if !self.resume && !self.persona.trim().is_empty() {
            format!("[你的角色设定]\n{}\n\n[任务]\n{prompt}", self.persona.trim())
        } else {
            prompt.to_string()
        };
        a.push("-p".into());
        a.push(text);
        a
    }
}

/// 把 Copilot 的事件流翻译成统一的 `AgentEvent`。
pub struct CopilotParser {
    session_id: String,
    started: bool,
    model: Option<String>,
    last_text: String,
    nano_aiu: Option<u64>,
    premium: Option<f64>,
}

impl CopilotParser {
    pub fn new(session_id: String) -> Self {
        Self { session_id, started: false, model: None, last_text: String::new(), nano_aiu: None, premium: None }
    }

    pub fn feed(&mut self, line: &str) -> Vec<AgentEvent> {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return vec![] };
        let mut out = Vec::new();
        if !self.started {
            self.started = true;
            out.push(AgentEvent::Init { session_id: self.session_id.clone() });
        }
        let d = &v["data"];
        match v["type"].as_str() {
            Some("assistant.message") => {
                if let Some(m) = d["model"].as_str() {
                    self.model = Some(m.to_string());
                }
                if let Some(t) = d["content"].as_str().filter(|t| !t.trim().is_empty()) {
                    self.last_text = t.to_string();
                    out.push(AgentEvent::Text(t.to_string()));
                }
                for r in d["toolRequests"].as_array().into_iter().flatten() {
                    out.push(AgentEvent::ToolUse {
                        id: r["toolCallId"].as_str().unwrap_or_default().into(),
                        name: r["name"].as_str().unwrap_or("tool").into(),
                        input: r["arguments"].clone(),
                    });
                }
            }
            Some("session.usage_checkpoint") => {
                self.nano_aiu = d["totalNanoAiu"].as_u64().or(self.nano_aiu);
                self.premium = d["totalPremiumRequests"].as_f64().or(self.premium);
            }
            Some("result") => {
                let code = v["exitCode"].as_i64().unwrap_or(0);
                out.push(AgentEvent::Result(TurnResult {
                    is_error: code != 0,
                    subtype: if code == 0 { "success".into() } else { format!("exit_{code}") },
                    text: self.last_text.clone(),
                    provider: Some("copilot".into()),
                    model: self.model.clone(),
                    premium_requests: v["usage"]["premiumRequests"].as_f64().or(self.premium),
                    nano_aiu: self.nano_aiu,
                    ..Default::default()
                }));
            }
            _ => {}
        }
        out
    }
}

pub struct CopilotProcess {
    child: Child,
    stdout: BufReader<ChildStdout>,
    pending: VecDeque<AgentEvent>,
    parser: CopilotParser,
    stderr_tail: Arc<Mutex<String>>,
}

impl CopilotProcess {
    pub async fn spawn(spec: &CopilotSpec, prompt: &str) -> Result<Self> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(spec.args(prompt))
            .current_dir(&spec.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().with_context(|| format!("启动 {} 失败", spec.program))?;
        let stdout = BufReader::new(child.stdout.take().context("无法获取 stdout")?);
        let mut stderr = child.stderr.take().context("无法获取 stderr")?;
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        let tail = stderr_tail.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                let mut t = tail.lock().unwrap();
                t.push_str(&String::from_utf8_lossy(&buf[..n]));
                if t.len() > 8192 {
                    let cut = t.len() - 8192;
                    let cut = (cut..t.len()).find(|i| t.is_char_boundary(*i)).unwrap_or(t.len());
                    t.drain(..cut);
                }
            }
        });
        Ok(Self { child, stdout, pending: VecDeque::new(), parser: CopilotParser::new(spec.session_id.clone()), stderr_tail })
    }

    pub async fn next_event(&mut self) -> Result<Option<AgentEvent>> {
        loop {
            if let Some(e) = self.pending.pop_front() {
                return Ok(Some(e));
            }
            let mut line = String::new();
            if self.stdout.read_line(&mut line).await? == 0 {
                return Ok(None);
            }
            self.pending.extend(self.parser.feed(line.trim()));
        }
    }

    /// Copilot 的非交互模式没有审批通道，不会产生审批请求；保留此方法以便与 Claude 适配器一致。
    pub async fn respond(&mut self, _req: &ApprovalRequest, _decision: &Decision) -> Result<()> {
        Ok(())
    }

    pub async fn finish(&mut self) -> Result<()> {
        self.child.wait().await?;
        Ok(())
    }

    pub async fn kill(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            unsafe { libc::killpg(pid as i32, libc::SIGKILL) };
        }
        let _ = self.child.kill().await;
    }

    pub fn stderr_tail(&self) -> String {
        self.stderr_tail.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> CopilotSpec {
        CopilotSpec::new("11111111-2222-4333-8444-555555555555".into(), false, "你是 Alice".into(), ".".into())
    }

    #[test]
    fn dev_args_allow_workspace_writes_and_dev_tools_but_always_deny_dangerous_ones() {
        let a = spec().args("做任务");
        assert!(a.contains(&"--allow-tool=write".to_string()));
        assert!(a.contains(&"--allow-tool=shell(cargo:*)".to_string()));
        for bad in ["git push", "sudo", "ssh", "curl", "rm"] {
            assert!(a.contains(&format!("--deny-tool=shell({bad}:*)")), "{bad}");
        }
        assert!(!a.iter().any(|x| x.contains("allow-all")), "不能使用 allow-all");
        assert!(a.windows(2).any(|w| w == ["--session-id", "11111111-2222-4333-8444-555555555555"]));
        assert!(a.contains(&"--disable-builtin-mcps".to_string()) && a.contains(&"--no-custom-instructions".to_string()));
        // 新会话：角色设定放在 -p 的内容开头
        let p = &a[a.iter().position(|x| x == "-p").unwrap() + 1];
        assert!(p.starts_with("[你的角色设定]\n你是 Alice") && p.ends_with("做任务"));
    }

    #[test]
    fn resume_does_not_repeat_the_persona_and_read_only_denies_writes() {
        let mut s = spec();
        s.resume = true;
        s.read_only = true;
        let a = s.args("继续");
        assert_eq!(a.last().unwrap(), "继续");
        assert!(a.contains(&"--deny-tool=write".to_string()) && !a.contains(&"--allow-tool=write".to_string()));
        assert!(!a.contains(&"--allow-tool=shell(cargo:*)".to_string()), "会议里不能跑构建");
        assert!(a.contains(&"--allow-tool=shell(git diff:*)".to_string()));
    }

    #[test]
    fn mcp_server_is_passed_and_allowed() {
        let mut s = spec();
        s.mcp = Some(McpLaunch { exe: "/bin/crewforge".into(), args: vec!["mcp-serve".into()] });
        let a = s.args("x");
        assert!(a.contains(&"--allow-tool=crewforge".to_string()));
        let i = a.iter().position(|x| x == "--additional-mcp-config").unwrap();
        assert!(a[i + 1].contains("crewforge") && a[i + 1].contains("mcp-serve"));
    }

    #[test]
    fn parses_the_real_event_stream_into_text_tools_and_result() {
        let mut p = CopilotParser::new("sid".into());
        let lines = [
            r#"{"type":"session.mcp_server_status_changed","data":{},"ephemeral":true}"#,
            r#"{"type":"assistant.message","data":{"model":"claude-sonnet-5.5","content":"","toolRequests":[{"toolCallId":"t1","name":"bash","arguments":{"command":"ls"}}]}}"#,
            r#"{"type":"assistant.message","data":{"model":"claude-sonnet-5.5","content":"Done. 已创建","toolRequests":[]}}"#,
            r#"{"type":"session.usage_checkpoint","data":{"totalNanoAiu":6840640000,"totalPremiumRequests":1}}"#,
            r#"{"type":"result","exitCode":0,"usage":{"premiumRequests":1,"totalApiDurationMs":4421}}"#,
        ];
        let ev: Vec<AgentEvent> = lines.iter().flat_map(|l| p.feed(l)).collect();
        assert!(matches!(&ev[0], AgentEvent::Init { session_id } if session_id == "sid"));
        assert!(ev.iter().any(|e| matches!(e, AgentEvent::ToolUse { name, input, .. } if name == "bash" && input["command"] == "ls")));
        let AgentEvent::Result(r) = ev.last().unwrap() else { panic!() };
        assert_eq!((r.is_error, r.text.as_str(), r.model.as_deref(), r.premium_requests, r.nano_aiu), (false, "Done. 已创建", Some("claude-sonnet-5.5"), Some(1.0), Some(6_840_640_000)));
        assert!(r.cost_usd.is_none() && r.input_tokens.is_none(), "没有 token 和费用数据就是未知，不是 0");
        // 退出码非 0 视为出错
        let mut p2 = CopilotParser::new("s".into());
        let AgentEvent::Result(r2) = p2.feed(r#"{"type":"result","exitCode":2}"#).pop().unwrap() else { panic!() };
        assert!(r2.is_error && r2.subtype == "exit_2");
        assert!(p2.feed("not json").is_empty());
    }
}
