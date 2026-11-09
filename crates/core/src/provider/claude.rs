//! Claude Code CLI 适配器：以 stream-json 双向协议驱动，不使用 PTY。
//!
//! 约定（已在 Claude Code 2.1.x 上实测）：
//! - `--input-format stream-json --output-format stream-json --verbose`
//! - `--permission-prompt-tool stdio`：权限请求以 `control_request` 出现在 stdout，
//!   宿主通过 stdin 回复 `control_response`
//! - 隔离用户的全局配置：`--setting-sources ""`（不加载任何设置来源，因此没有 hooks、
//!   用户安装的插件、CLAUDE.md）+ `--disable-slash-commands` + `--strict-mcp-config`。
//!   不用 `--safe-mode`：实测它会把 `--mcp-config` 一起关掉，而我们需要通过 MCP 给员工提供协作工具。
//!   登录（keychain）不受影响。
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
    /// 提供给员工的团队协作工具（MCP 服务器）；None 表示不提供
    pub mcp: Option<McpLaunch>,
    /// 命令沙箱（macOS Seatbelt，由 CLI 自带）：沙箱内的命令不再逐条审批，
    /// 工作区之外的写入由系统拦截。值为除工作区外额外可写的目录（如 git 公共目录、包缓存）。
    pub sandbox: Option<Vec<PathBuf>>,
}

/// 沙箱内允许访问的网络域名（包管理器与代码托管）；其他域名仍需审批。
const SANDBOX_DOMAINS: &[&str] = &[
    "registry.npmjs.org", "crates.io", "index.crates.io", "static.crates.io",
    "static.rust-lang.org", "github.com", "pypi.org", "files.pythonhosted.org",
];

/// 如何启动我们自己的 MCP 服务器进程（就是 crewforge 可执行文件的 `mcp-serve` 子命令）。
#[derive(Debug, Clone)]
pub struct McpLaunch {
    pub exe: PathBuf,
    pub args: Vec<String>,
}

impl McpLaunch {
    pub fn config_json(&self) -> String {
        json!({"mcpServers": {"crewforge": {"command": self.exe.to_string_lossy(), "args": self.args}}}).to_string()
    }
}

impl ClaudeSpec {
    pub fn new(session_id: String, resume: bool, persona: String, cwd: PathBuf) -> Self {
        Self { session_id, resume, persona, cwd, program: "claude".into(), mcp: None, sandbox: None }
    }

    pub fn args(&self) -> Vec<String> {
        let mut a: Vec<String> = [
            "-p",
            "--setting-sources", "",
            "--disable-slash-commands",
            "--strict-mcp-config",
            "--input-format", "stream-json",
            "--output-format", "stream-json",
            "--verbose",
            "--permission-mode", "default",
            "--permission-prompt-tool", "stdio",
        ]
        .map(String::from)
        .into();
        // 总是传 --mcp-config：配合 --strict-mcp-config，既屏蔽用户自己的 MCP 服务器，又只开放我们提供的
        a.push("--mcp-config".into());
        a.push(self.mcp.as_ref().map(McpLaunch::config_json).unwrap_or_else(|| r#"{"mcpServers":{}}"#.into()));
        if let Some(extra) = &self.sandbox {
            a.push("--settings".into());
            a.push(sandbox_settings(extra));
        }
        a.push(if self.resume { "--resume" } else { "--session-id" }.into());
        a.push(self.session_id.clone());
        if !self.persona.is_empty() {
            a.push("--append-system-prompt".into());
            a.push(self.persona.clone());
        }
        a
    }
}

pub fn sandbox_settings(extra_write: &[PathBuf]) -> String {
    let allow: Vec<String> = extra_write.iter().map(|p| p.to_string_lossy().to_string()).collect();
    json!({"sandbox": {
        "enabled": true,
        "autoAllowBashIfSandboxed": true,
        "allowUnsandboxedCommands": false,
        "filesystem": {"allowWrite": allow},
        "network": {"allowedDomains": SANDBOX_DOMAINS, "allowLocalBinding": true},
    }}).to_string()
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
    /// 按模型拆分的用量（CLI 的 modelUsage）；没有则为空
    pub models: Vec<ModelUse>,
    /// 首个内容帧的耗时（毫秒）
    pub ttft_ms: Option<u64>,
}

/// 一个模型在本轮里的用量与费用。
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct ModelUse {
    pub model: String,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
    pub cost_usd: Option<f64>,
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
                models: v["modelUsage"].as_object().map(|m| {
                    m.iter().map(|(name, x)| ModelUse {
                        model: name.clone(),
                        input: x["inputTokens"].as_u64().unwrap_or(0),
                        output: x["outputTokens"].as_u64().unwrap_or(0),
                        cache_read: x["cacheReadInputTokens"].as_u64().unwrap_or(0),
                        cache_creation: x["cacheCreationInputTokens"].as_u64().unwrap_or(0),
                        cost_usd: x["costUSD"].as_f64(),
                    }).collect()
                }).unwrap_or_default(),
                ttft_ms: v["ttft_ms"].as_u64(),
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
    fn sandbox_adds_settings() {
        let mut s = ClaudeSpec::new("sid".into(), false, String::new(), ".".into());
        assert!(!s.args().contains(&"--settings".to_string()));
        s.sandbox = Some(vec!["/repo/.git".into()]);
        let a = s.args();
        let i = a.iter().position(|x| x == "--settings").unwrap();
        let v: serde_json::Value = serde_json::from_str(&a[i + 1]).unwrap();
        assert_eq!(v["sandbox"]["enabled"], true);
        assert_eq!(v["sandbox"]["filesystem"]["allowWrite"][0], "/repo/.git");
    }

    #[test]
    fn args_new_vs_resume() {
        let s = ClaudeSpec::new("sid".into(), false, "你是 Alice".into(), ".".into());
        let a = s.args();
        assert!(a.windows(2).any(|w| w == ["--session-id", "sid"]));
        assert!(!a.contains(&"--safe-mode".to_string()), "--safe-mode 会关掉 MCP");
        assert!(a.windows(2).any(|w| w == ["--setting-sources", ""]), "空的设置来源");
        assert!(a.contains(&"--strict-mcp-config".to_string()) && a.contains(&"--disable-slash-commands".to_string()));
        assert!(a.windows(2).any(|w| w == ["--mcp-config", r#"{"mcpServers":{}}"#]), "没有 MCP 时也要屏蔽用户的 MCP");
        assert!(a.windows(2).any(|w| w == ["--permission-prompt-tool", "stdio"]));
        assert!(a.windows(2).any(|w| w == ["--append-system-prompt", "你是 Alice"]));
        let s = ClaudeSpec::new("sid".into(), true, String::new(), ".".into());
        let a = s.args();
        assert!(a.windows(2).any(|w| w == ["--resume", "sid"]));
        assert!(!a.contains(&"--append-system-prompt".to_string()));
    }

    #[test]
    fn mcp_config_points_at_our_own_server_with_fixed_identity() {
        let mut s = ClaudeSpec::new("sid".into(), false, String::new(), ".".into());
        s.mcp = Some(McpLaunch { exe: "/bin/crewforge".into(), args: vec!["mcp-serve".into(), "--employee".into(), "e1".into()] });
        let a = s.args();
        let i = a.iter().position(|x| x == "--mcp-config").unwrap();
        let v: Value = serde_json::from_str(&a[i + 1]).unwrap();
        assert_eq!(v["mcpServers"]["crewforge"]["command"], "/bin/crewforge");
        assert_eq!(v["mcpServers"]["crewforge"]["args"], json!(["mcp-serve", "--employee", "e1"]));
        assert_eq!(v["mcpServers"].as_object().unwrap().len(), 1, "只有我们自己的服务器");
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
    fn parse_result_reads_per_model_usage_and_ttft() {
        let l = r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","total_cost_usd":0.3,"ttft_ms":820,"usage":{"input_tokens":3},
            "modelUsage":{"claude-sonnet-5-5":{"inputTokens":2,"outputTokens":14,"cacheReadInputTokens":100,"cacheCreationInputTokens":50,"costUSD":0.25},
                          "claude-haiku-5-5":{"inputTokens":1,"outputTokens":2,"cacheReadInputTokens":0,"cacheCreationInputTokens":0,"costUSD":0.05}}}"#.replace('\n', "");
        let AgentEvent::Result(r) = &parse_line(&l)[0] else { panic!() };
        assert_eq!(r.ttft_ms, Some(820));
        assert_eq!(r.models.len(), 2);
        let sonnet = r.models.iter().find(|m| m.model == "claude-sonnet-5-5").unwrap();
        assert_eq!((sonnet.output, sonnet.cache_read, sonnet.cache_creation, sonnet.cost_usd), (14, 100, 50, Some(0.25)));
        // 没有 modelUsage 的旧输出照常解析
        let AgentEvent::Result(r2) = &parse_line(r#"{"type":"result","subtype":"success","is_error":false,"result":"ok"}"#)[0] else { panic!() };
        assert!(r2.models.is_empty() && r2.ttft_ms.is_none());
    }

    #[test]
    fn unknown_or_garbage_is_ignored() {
        assert!(parse_line("not json").is_empty());
        assert!(parse_line(r#"{"type":"something_new"}"#).is_empty());
    }
}
