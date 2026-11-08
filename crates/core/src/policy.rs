//! 权限策略（MVP 三档）：
//! - 只读工具：自动允许
//! - 工作区内的文件写入、白名单内的无副作用命令：默认允许
//! - 其他（工作区外写入、任意 Shell、网络等）：需要人类审批
//!
//! 注意：这里只做词法层面的路径判断，不解析符号链接；
//! 高风险场景的更严格规则留待后续阶段。

use serde_json::Value;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    Allow(&'static str),
    Ask(String),
}

pub struct Policy {
    workdir: PathBuf,
}

const READ_ONLY_TOOLS: &[&str] = &["Read", "Glob", "Grep", "LS", "NotebookRead", "TodoWrite"];
const WRITE_TOOLS: &[&str] = &["Write", "Edit", "MultiEdit", "NotebookEdit"];
const SAFE_COMMANDS: &[&str] = &[
    "ls", "pwd", "git status", "git diff", "git log", "git branch",
    "cargo check", "cargo build", "cargo test", "cargo fmt", "cargo clippy",
];
const SHELL_META: &[char] = &[';', '&', '|', '<', '>', '`', '$', '(', ')', '\n', '\\'];

impl Policy {
    pub fn new(workdir: impl Into<PathBuf>) -> Self {
        Self { workdir: normalize(&workdir.into()) }
    }

    pub fn decide(&self, tool: &str, input: &Value) -> PolicyDecision {
        if READ_ONLY_TOOLS.contains(&tool) {
            return PolicyDecision::Allow("只读工具");
        }
        if WRITE_TOOLS.contains(&tool) {
            let path = ["file_path", "notebook_path", "path"]
                .iter()
                .find_map(|k| input.get(*k).and_then(Value::as_str));
            return match path {
                Some(p) if self.inside_workdir(p) => PolicyDecision::Allow("工作区内写入"),
                Some(p) => PolicyDecision::Ask(format!("写入工作区之外的路径: {p}")),
                None => PolicyDecision::Ask("无法确定写入路径".into()),
            };
        }
        if tool == "Bash" {
            let cmd = input.get("command").and_then(Value::as_str).unwrap_or("").trim();
            if is_safe_command(cmd) {
                return PolicyDecision::Allow("白名单命令");
            }
            return PolicyDecision::Ask(format!("执行命令: {cmd}"));
        }
        PolicyDecision::Ask(format!("使用工具 {tool}"))
    }

    fn inside_workdir(&self, p: &str) -> bool {
        let path = Path::new(p);
        let abs = if path.is_absolute() { path.to_path_buf() } else { self.workdir.join(path) };
        normalize(&abs).starts_with(&self.workdir)
    }
}

fn is_safe_command(cmd: &str) -> bool {
    if cmd.is_empty() || cmd.contains(SHELL_META) {
        return false;
    }
    SAFE_COMMANDS.iter().any(|p| cmd == *p || cmd.strip_prefix(p).is_some_and(|r| r.starts_with(' ')))
}

/// 词法归一化：处理 `.` 与 `..`，不访问文件系统。
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pol() -> Policy {
        Policy::new("/work/proj")
    }

    #[test]
    fn read_tools_allowed() {
        assert!(matches!(pol().decide("Read", &json!({})), PolicyDecision::Allow(_)));
    }

    #[test]
    fn write_inside_workdir_allowed() {
        let d = pol().decide("Write", &json!({"file_path": "/work/proj/src/a.rs"}));
        assert!(matches!(d, PolicyDecision::Allow(_)));
        let d = pol().decide("Edit", &json!({"file_path": "src/a.rs"}));
        assert!(matches!(d, PolicyDecision::Allow(_)));
    }

    #[test]
    fn write_outside_or_traversal_asks() {
        for p in ["/etc/passwd", "/work/proj/../other/x", "../x", "/work/project2/x"] {
            let d = pol().decide("Write", &json!({"file_path": p}));
            assert!(matches!(d, PolicyDecision::Ask(_)), "{p}");
        }
    }

    #[test]
    fn bash_whitelist_and_metacharacters() {
        assert!(matches!(pol().decide("Bash", &json!({"command": "cargo test --workspace"})), PolicyDecision::Allow(_)));
        assert!(matches!(pol().decide("Bash", &json!({"command": "git status"})), PolicyDecision::Allow(_)));
        for c in ["rm -rf /", "ls; rm -rf /", "cargo test && curl x", "ls $(whoami)", "git statusx", "cat a > b", ""] {
            assert!(matches!(pol().decide("Bash", &json!({"command": c})), PolicyDecision::Ask(_)), "{c}");
        }
    }

    #[test]
    fn unknown_tool_asks() {
        assert!(matches!(pol().decide("WebFetch", &json!({})), PolicyDecision::Ask(_)));
    }
}
