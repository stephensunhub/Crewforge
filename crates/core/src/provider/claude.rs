//! Claude Code CLI 适配器：以 stream-json 双向协议驱动，不使用 PTY。
//!
//! 约定（已在 Claude Code 2.1.x 上实测）：
//! - `--input-format stream-json --output-format stream-json --verbose`
//! - `--permission-prompt-tool stdio`：权限请求以 `control_request` 出现在 stdout，
//!   宿主通过 stdin 回复 `control_response`
//! - `--safe-mode`：保留登录，关闭用户全局 hooks/插件/CLAUDE.md 等干扰
//! - 宿主必须一直持有 stdin，直到本轮结束，否则正在等待的工具调用会失败

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

#[derive(Debug, Clone)]
pub struct ClaudeSpec {
    pub session_id: String,
    /// true：`--resume` 恢复已有会话；false：`--session-id` 创建新会话
    pub resume: bool,
    /// 员工身份说明，通过 `--append-system-prompt` 注入
    pub persona: String,
    pub cwd: PathBuf,
    /// 可执行文件，默认 `claude`
    pub program: String,
}

impl ClaudeSpec {
    pub fn new(session_id: String, resume: bool, persona: String, cwd: PathBuf) -> Self {
        Self { session_id, resume, persona, cwd, program: "claude".into() }
    }

    pub fn args(&self) -> Vec<String> {
        let mut a: Vec<String> = [
            "-p", "--safe-mode",
            "--input-format", "stream-json",
            "--output-format", "stream-json",
            "--verbose",
            "--permission-mode", "default",
            "--permission-prompt-tool", "stdio",
        ]
        .map(String::from)
        .into();
        a.push(if self.resume { "--resume" } else { "--session-id" }.into());
        a.push(self.session_id.clone());
        if !self.persona.is_empty() {
            a.push("--append-system-prompt".into());
            a.push(self.persona.clone());
        }
        a
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    pub request_id: String,
    pub tool_name: String,
    pub tool_use_id: String,
    pub input: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Allow,
    Deny(String),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TurnResult {
    pub is_error: bool,
    pub subtype: String,
    pub text: String,
    pub cost_usd: Option<f64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_creation_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    Init { session_id: String },
    Text(String),
    ToolUse { id: String, name: String, input: Value },
    Approval(ApprovalRequest),
    RateLimit {
        status: String,
        five_hour: Option<f64>,
        seven_day: Option<f64>,
        /// 窗口重置时间（Unix 秒）
        five_resets_at: Option<i64>,
        seven_resets_at: Option<i64>,
    },
    Result(TurnResult),
}

/// 解析 CLI 输出的一行。无法识别的消息返回空，保证对协议新增字段向前兼容。
pub fn parse_line(line: &str) -> Vec<AgentEvent> {
    let Ok(v) = serde_json::from_str::<Value>(line) else { return vec![] };
    match v["type"].as_str() {
        Some("system") if v["subtype"] == "init" => vec![AgentEvent::Init {
            session_id: v["session_id"].as_str().unwrap_or_default().to_string(),
        }],
        Some("assistant") => v["message"]["content"]
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|b| match b["type"].as_str() {
                        Some("text") => Some(AgentEvent::Text(b["text"].as_str()?.to_string())),
                        Some("tool_use") => Some(AgentEvent::ToolUse {
                            id: b["id"].as_str()?.to_string(),
                            name: b["name"].as_str()?.to_string(),
                            input: b["input"].clone(),
                        }),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        Some("control_request") if v["request"]["subtype"] == "can_use_tool" => {
            let r = &v["request"];
            match (v["request_id"].as_str(), r["tool_name"].as_str()) {
                (Some(id), Some(tool)) => vec![AgentEvent::Approval(ApprovalRequest {
                    request_id: id.into(),
                    tool_name: tool.into(),
                    tool_use_id: r["tool_use_id"].as_str().unwrap_or_default().into(),
                    input: r["input"].clone(),
                })],
                _ => vec![],
            }
        }
        Some("rate_limit_event") => {
            let info = &v["rate_limit_info"];
            vec![AgentEvent::RateLimit {
                status: info["status"].as_str().unwrap_or("unknown").into(),
                five_hour: info["unifiedWindows"]["five_hour"]["utilization"].as_f64(),
                seven_day: info["unifiedWindows"]["seven_day"]["utilization"].as_f64(),
                five_resets_at: info["unifiedWindows"]["five_hour"]["resetsAt"].as_i64(),
                seven_resets_at: info["unifiedWindows"]["seven_day"]["resetsAt"].as_i64(),
            }]
        }
        Some("result") => {
            let u = &v["usage"];
            vec![AgentEvent::Result(TurnResult {
                is_error: v["is_error"].as_bool().unwrap_or(false),
                subtype: v["subtype"].as_str().unwrap_or_default().into(),
                text: v["result"].as_str().unwrap_or_default().into(),
                cost_usd: v["total_cost_usd"].as_f64(),
                input_tokens: u["input_tokens"].as_u64(),
                output_tokens: u["output_tokens"].as_u64(),
                cache_read_tokens: u["cache_read_input_tokens"].as_u64(),
                cache_creation_tokens: u["cache_creation_input_tokens"].as_u64(),
            })]
        }
        _ => vec![],
    }
}

pub struct ClaudeProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    pending: VecDeque<AgentEvent>,
    stderr_tail: Arc<Mutex<String>>,
}

impl ClaudeProcess {
    /// 启动进程并发送第一条用户消息。进程放入独立进程组，便于整体终止。
    pub async fn spawn(spec: &ClaudeSpec, prompt: &str) -> Result<Self> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(spec.args())
            .current_dir(&spec.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().with_context(|| format!("启动 {} 失败", spec.program))?;

        let stdin = child.stdin.take().context("无法获取 stdin")?;
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

        let mut p = Self { child, stdin: Some(stdin), stdout, pending: VecDeque::new(), stderr_tail };
        p.write_json(&json!({"type": "user", "message": {"role": "user", "content": prompt}}))
            .await?;
        Ok(p)
    }

    async fn write_json(&mut self, v: &Value) -> Result<()> {
        let stdin = self.stdin.as_mut().context("stdin 已关闭")?;
        stdin.write_all(format!("{v}\n").as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    /// 读取下一个事件；进程输出结束返回 `None`。
    pub async fn next_event(&mut self) -> Result<Option<AgentEvent>> {
        loop {
            if let Some(e) = self.pending.pop_front() {
                return Ok(Some(e));
            }
            let mut line = String::new();
            if self.stdout.read_line(&mut line).await? == 0 {
                return Ok(None);
            }
            self.pending.extend(parse_line(line.trim()));
        }
    }

    pub async fn respond(&mut self, req: &ApprovalRequest, decision: &Decision) -> Result<()> {
        let body = match decision {
            Decision::Allow => json!({"behavior": "allow", "updatedInput": req.input}),
            Decision::Deny(msg) => json!({"behavior": "deny", "message": msg}),
        };
        self.write_json(&json!({
            "type": "control_response",
            "response": {"subtype": "success", "request_id": req.request_id, "response": body}
        }))
        .await
    }

    /// 正常结束：关闭 stdin 并等待进程退出。
    pub async fn finish(&mut self) -> Result<()> {
        self.stdin.take();
        self.child.wait().await?;
        Ok(())
    }

    /// 异常终止：杀死整个进程组。
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

/// 检查 `claude` 是否可用，返回版本号。
pub async fn detect(program: &str) -> Result<String> {
    let out = Command::new(program).arg("--version").output().await;
    match out {
        Ok(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).trim().to_string()),
        Ok(o) => bail!("{program} --version 失败: {}", String::from_utf8_lossy(&o.stderr)),
        Err(e) => bail!("未找到 {program}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_new_vs_resume() {
        let s = ClaudeSpec::new("sid".into(), false, "你是 Alice".into(), ".".into());
        let a = s.args();
        assert!(a.windows(2).any(|w| w == ["--session-id", "sid"]));
        assert!(a.contains(&"--safe-mode".to_string()));
        assert!(a.windows(2).any(|w| w == ["--permission-prompt-tool", "stdio"]));
        assert!(a.windows(2).any(|w| w == ["--append-system-prompt", "你是 Alice"]));
        let s = ClaudeSpec::new("sid".into(), true, String::new(), ".".into());
        let a = s.args();
        assert!(a.windows(2).any(|w| w == ["--resume", "sid"]));
        assert!(!a.contains(&"--append-system-prompt".to_string()));
    }

    #[test]
    fn parse_init_and_text_and_tool() {
        let init = r#"{"type":"system","subtype":"init","session_id":"abc"}"#;
        assert_eq!(parse_line(init), vec![AgentEvent::Init { session_id: "abc".into() }]);
        let asst = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":""},
            {"type":"text","text":"hi"},{"type":"tool_use","id":"t1","name":"Write","input":{"file_path":"a"}}]}}"#
            .replace('\n', "");
        let ev = parse_line(&asst);
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0], AgentEvent::Text("hi".into()));
        assert!(matches!(&ev[1], AgentEvent::ToolUse { name, .. } if name == "Write"));
    }

    #[test]
    fn parse_approval() {
        let l = r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Write","input":{"file_path":"/x"},"tool_use_id":"t1"}}"#;
        let ev = parse_line(l);
        assert_eq!(
            ev,
            vec![AgentEvent::Approval(ApprovalRequest {
                request_id: "r1".into(),
                tool_name: "Write".into(),
                tool_use_id: "t1".into(),
                input: json!({"file_path": "/x"}),
            })]
        );
    }

    #[test]
    fn parse_result_and_ratelimit() {
        let l = r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","total_cost_usd":0.1,
            "usage":{"input_tokens":3,"output_tokens":4,"cache_read_input_tokens":5,"cache_creation_input_tokens":6}}"#
            .replace('\n', "");
        let AgentEvent::Result(r) = &parse_line(&l)[0] else { panic!() };
        assert_eq!(r.cost_usd, Some(0.1));
        assert_eq!((r.input_tokens, r.output_tokens), (Some(3), Some(4)));
        let l = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","unifiedWindows":{"five_hour":{"utilization":0.7,"resetsAt":1700000000},"seven_day":{"utilization":0.5}}}}"#;
        assert_eq!(
            parse_line(l),
            vec![AgentEvent::RateLimit { status: "allowed_warning".into(), five_hour: Some(0.7), seven_day: Some(0.5), five_resets_at: Some(1700000000), seven_resets_at: None }]
        );
    }

    #[test]
    fn unknown_or_garbage_is_ignored() {
        assert!(parse_line("not json").is_empty());
        assert!(parse_line(r#"{"type":"something_new"}"#).is_empty());
    }
}
