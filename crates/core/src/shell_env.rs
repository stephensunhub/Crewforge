//! 从 Finder/Dock 启动的 macOS 应用只有极简的 PATH（/usr/bin:/bin:...），
//! 找不到用户用 Homebrew、nvm、rustup 安装的 claude / cargo / git / npm。
//! 这里向用户的登录 shell 询问真实的 PATH，并补上常见目录，之后启动的所有子进程都继承它。

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const START: &str = "__CREWFORGE_PATH_START__";
const END: &str = "__CREWFORGE_PATH_END__";

/// 常见的工具安装位置（登录 shell 询问失败时的兜底）。
fn fallback_dirs(home: &str) -> Vec<String> {
    ["/opt/homebrew/bin", "/opt/homebrew/sbin", "/usr/local/bin"]
        .iter()
        .map(|s| s.to_string())
        .chain([".cargo/bin", ".local/bin", ".claude/local", ".bun/bin", ".volta/bin"].iter().map(|s| format!("{home}/{s}")))
        .collect()
}

/// 把 `shell_path`（登录 shell 给出的）、当前 PATH、兜底目录按此顺序合并去重。
pub fn merge_paths(shell_path: &str, current: &str, home: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    let fb = fallback_dirs(home);
    for d in shell_path.split(':').chain(current.split(':')).chain(fb.iter().map(String::as_str)) {
        if !d.is_empty() && seen.insert(d.to_string()) {
            out.push(d.to_string());
        }
    }
    out.join(":")
}

/// 从一段 shell 输出里取出标记之间的 PATH（rc 文件可能往 stdout 打印别的东西）。
pub fn extract_marked(output: &str) -> Option<String> {
    let s = output.find(START)? + START.len();
    let e = output[s..].find(END)? + s;
    let p = output[s..e].trim();
    (!p.is_empty()).then(|| p.to_string())
}

fn ask_login_shell() -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let mut child = Command::new(&shell)
        .args(["-ilc", &format!("printf '%s' {START}; printf '%s' \"$PATH\"; printf '%s' {END}")])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // 交互式 shell 的 rc 文件可能卡住，最多等 5 秒
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait().ok()? {
            Some(_) => break,
            None if Instant::now() > deadline => {
                let _ = child.kill();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    let mut buf = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut buf).ok()?;
    extract_marked(&buf)
}

/// 在应用启动时调用一次。
pub fn fix_path() {
    let home = std::env::var("HOME").unwrap_or_default();
    let current = std::env::var("PATH").unwrap_or_default();
    let shell = ask_login_shell().unwrap_or_default();
    // SAFETY: 在启动阶段、尚未创建其他线程前调用
    unsafe { std::env::set_var("PATH", merge_paths(&shell, &current, &home)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_in_order_without_duplicates() {
        let m = merge_paths("/a:/b", "/b:/usr/bin", "/h");
        let parts: Vec<&str> = m.split(':').collect();
        assert_eq!(&parts[..3], ["/a", "/b", "/usr/bin"]);
        assert_eq!(parts.iter().filter(|p| **p == "/b").count(), 1);
        assert!(parts.contains(&"/opt/homebrew/bin") && parts.contains(&"/h/.cargo/bin"));
    }

    #[test]
    fn extracts_path_between_markers_ignoring_noise() {
        let out = format!("welcome!\n{START}/x:/y{END}trailing");
        assert_eq!(extract_marked(&out).as_deref(), Some("/x:/y"));
        assert_eq!(extract_marked("nothing"), None);
        assert_eq!(extract_marked(&format!("{START}{END}")), None);
    }

    #[test]
    fn login_shell_gives_a_path() {
        assert!(ask_login_shell().is_some_and(|p| p.contains('/')));
    }
}
