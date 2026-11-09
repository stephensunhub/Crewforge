//! 执行引擎适配器：Claude Code 与 GitHub Copilot CLI。
//! 两者对外给出同一种 `AgentEvent` 流，运行器不关心底层是谁；
//! 差别只在权限（Claude 可以边跑边询问人，Copilot 只能启动时定好允许与拒绝的清单）和计量方式。

pub mod claude;
pub mod copilot;

use anyhow::Result;
use claude::{AgentEvent, ApprovalRequest, ClaudeProcess, Decision};
use copilot::CopilotProcess;

/// 正在运行的员工进程。
pub enum AgentProcess {
    Claude(ClaudeProcess),
    Copilot(CopilotProcess),
}

impl AgentProcess {
    pub async fn next_event(&mut self) -> Result<Option<AgentEvent>> {
        match self {
            Self::Claude(p) => p.next_event().await,
            Self::Copilot(p) => p.next_event().await,
        }
    }
    pub async fn respond(&mut self, req: &ApprovalRequest, d: &Decision) -> Result<()> {
        match self {
            Self::Claude(p) => p.respond(req, d).await,
            Self::Copilot(p) => p.respond(req, d).await,
        }
    }
    pub async fn finish(&mut self) -> Result<()> {
        match self {
            Self::Claude(p) => p.finish().await,
            Self::Copilot(p) => p.finish().await,
        }
    }
    pub async fn kill(&mut self) {
        match self {
            Self::Claude(p) => p.kill().await,
            Self::Copilot(p) => p.kill().await,
        }
    }
    pub fn stderr_tail(&self) -> String {
        match self {
            Self::Claude(p) => p.stderr_tail(),
            Self::Copilot(p) => p.stderr_tail(),
        }
    }
}
