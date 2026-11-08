//! 测试基线：在干净的检出里运行构建/测试命令，记录原始结果，并解析出失败的测试。
//! 之后用同样的命令检查 AI 的修改，逐项对比，区分"导入前就失败"与"AI 引入的新问题"。

use super::scan::BaselineCommand;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::Command;

const OUTPUT_TAIL_CHARS: usize = 20_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommandResult {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub command: String,
    /// passed | failed | timeout | skipped | error
    pub status: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub output_tail: String,
    pub failed_tests: Vec<String>,
    /// 是否解析出了逐项的测试结果；false 时只能做到命令级的比较
    pub parsed: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// 导入前就已失败，现在仍失败
    PreExisting,
    /// 基线时通过，现在失败：AI 修改引入
    Introduced,
    /// 基线时失败，现在通过
    Fixed,
    /// 无法确认来源
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Finding {
    pub command_id: String,
    pub command_label: String,
    /// 失败项；命令级别的失败用 "（整条命令）"
    pub item: String,
    pub origin: Origin,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckReport {
    pub findings: Vec<Finding>,
    pub results: Vec<CommandResult>,
    pub pre_existing: usize,
    pub introduced: usize,
    pub fixed: usize,
    pub unknown: usize,
}

pub const WHOLE: &str = "（整条命令）";

fn dedupe(v: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    v.into_iter().filter(|x| seen.insert(x.clone())).collect()
}

/// 从命令输出里解析失败的测试。`None` 表示没有适用的解析器（不是"没有失败"）。
pub fn parse_failures(command: &str, output: &str) -> Option<Vec<String>> {
    let c = command;
    if c.contains("cargo test") {
        // test path::to::name ... FAILED
        return Some(dedupe(output.lines().filter_map(|l| {
            let l = l.trim();
            let rest = l.strip_prefix("test ")?;
            let name = rest.strip_suffix(" ... FAILED")?;
            Some(name.trim().to_string())
        }).collect()));
    }
    if c.contains("pytest") {
        // FAILED tests/test_x.py::test_y - AssertionError   /   ERROR tests/test_x.py
        return Some(dedupe(output.lines().filter_map(|l| {
            let rest = l.strip_prefix("FAILED ").or_else(|| l.strip_prefix("ERROR "))?;
            Some(rest.split(" - ").next()?.trim().to_string())
        }).collect()));
    }
    if c.contains("go test") {
        return Some(dedupe(output.lines().filter_map(|l| {
            let rest = l.trim().strip_prefix("--- FAIL: ")?;
            Some(rest.split_whitespace().next()?.to_string())
        }).collect()));
    }
    if c.contains("mvn") || c.contains("mvnw") {
        // [ERROR] Failures: / [ERROR] Errors: 之后缩进三格的条目：[ERROR]   FooTest.bar:42 ...
        let mut out = Vec::new();
        let mut inside = false;
        for l in output.lines() {
            let t = l.trim_end();
            if t.starts_with("[ERROR] Failures:") || t.starts_with("[ERROR] Errors:") {
                inside = true;
                continue;
            }
            if inside {
                if let Some(rest) = t.strip_prefix("[ERROR]   ") {
                    let id = rest.split([':', ' ']).next().unwrap_or("").trim();
                    if id.contains('.') {
                        out.push(id.to_string());
                    }
                } else if !t.starts_with("[ERROR]") || t == "[ERROR] " || t == "[ERROR]" {
                    inside = false;
                }
            }
        }
        return Some(dedupe(out));
    }
    if c.contains("gradle") {
        return Some(dedupe(output.lines().filter_map(|l| {
            let t = l.trim();
            (t.ends_with(" FAILED") && t.contains(" > ")).then(|| t.trim_end_matches(" FAILED").to_string())
        }).collect()));
    }
    if c.contains("npm") || c.contains("pnpm") || c.contains("yarn") || c.contains("jest") || c.contains("vitest") {
        // jest: "FAIL path/to/x.test.ts"；vitest: " FAIL  path > name"
        return Some(dedupe(output.lines().filter_map(|l| {
            let rest = l.trim().strip_prefix("FAIL ")?;
            let rest = rest.trim();
            (!rest.is_empty()).then(|| rest.to_string())
        }).collect()));
    }
    None
}

fn tail(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n { s.to_string() } else { format!("…（前面的输出已省略）\n{}", chars[chars.len() - n..].iter().collect::<String>()) }
}

/// 平台相关的 shell 调用集中在这里。
fn shell(command: &str) -> Command {
    let mut c = Command::new("sh");
    c.arg("-c").arg(command);
    c
}

pub enum Progress<'a> {
    Started(&'a BaselineCommand),
    Finished(&'a CommandResult),
}

/// 依次运行命令。setup 失败则其余命令标记为跳过（依赖没装好，结果没有意义）。
pub async fn run_commands(dir: &Path, cmds: &[BaselineCommand], timeout: Duration, mut on: impl FnMut(Progress)) -> Vec<CommandResult> {
    let mut results = Vec::new();
    let mut setup_failed = false;
    for bc in cmds {
        on(Progress::Started(bc));
        let r = if setup_failed && bc.kind != "setup" {
            CommandResult { id: bc.id.clone(), label: bc.label.clone(), kind: bc.kind.clone(), command: bc.command.clone(),
                status: "skipped".into(), exit_code: None, duration_ms: 0, output_tail: "依赖安装失败，已跳过".into(), failed_tests: vec![], parsed: false }
        } else {
            run_one(dir, bc, timeout).await
        };
        if bc.kind == "setup" && r.status != "passed" {
            setup_failed = true;
        }
        on(Progress::Finished(&r));
        results.push(r);
    }
    results
}

async fn run_one(dir: &Path, bc: &BaselineCommand, timeout: Duration) -> CommandResult {
    let started = Instant::now();
    let mut c = shell(&bc.command);
    c.current_dir(dir)
        .env("CI", "true").env("GIT_TERMINAL_PROMPT", "0").env("NO_COLOR", "1").env("TERM", "dumb")
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    #[cfg(unix)]
    c.process_group(0);
    let mut base = CommandResult {
        id: bc.id.clone(), label: bc.label.clone(), kind: bc.kind.clone(), command: bc.command.clone(),
        status: "error".into(), exit_code: None, duration_ms: 0, output_tail: String::new(), failed_tests: vec![], parsed: false,
    };
    let child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => { base.output_tail = format!("无法启动命令: {e}"); return base; }
    };
    let pid = child.id();
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => {
            let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            let ok = out.status.success();
            let parsed = parse_failures(&bc.command, &text);
            let failed_tests = parsed.clone().unwrap_or_default();
            base.status = if ok { "passed" } else { "failed" }.into();
            base.exit_code = out.status.code();
            // 只有"通过"或"确实解析出失败项"时才算逐项解析成功；命令失败却没有失败项，多半是编译/环境错误
            base.parsed = parsed.is_some() && (ok || !failed_tests.is_empty());
            base.failed_tests = failed_tests;
            base.output_tail = tail(&text, OUTPUT_TAIL_CHARS);
        }
        Ok(Err(e)) => base.output_tail = format!("等待命令结束时出错: {e}"),
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = pid {
                unsafe { libc::killpg(pid as i32, libc::SIGKILL) };
            }
            let _ = pid;
            base.status = "timeout".into();
            base.output_tail = format!("命令超过 {} 秒仍未结束，已终止", timeout.as_secs());
        }
    }
    base.duration_ms = started.elapsed().as_millis() as u64;
    base
}

/// 把当前结果与基线逐命令、逐项对比，判定每个失败的来源。
pub fn classify(baseline: &[CommandResult], current: &[CommandResult]) -> CheckReport {
    let mut findings = Vec::new();
    for cur in current {
        if cur.status == "skipped" {
            continue;
        }
        let base = baseline.iter().find(|b| b.id == cur.id);
        let mk = |item: &str, origin: Origin, note: &str| Finding {
            command_id: cur.id.clone(), command_label: cur.label.clone(), item: item.to_string(), origin, note: note.to_string(),
        };
        let cur_failed = cur.status != "passed";
        let Some(base) = base.filter(|b| b.status != "skipped") else {
            if cur_failed {
                findings.push(mk(WHOLE, Origin::Unknown, "基线里没有运行过这条命令，无法对比"));
            }
            continue;
        };
        let base_failed = base.status != "passed";
        match (base_failed, cur_failed) {
            (false, false) => {}
            (true, false) => findings.push(mk(WHOLE, Origin::Fixed, "基线时失败，现在通过")),
            (false, true) => {
                if cur.parsed && !cur.failed_tests.is_empty() {
                    for t in &cur.failed_tests {
                        findings.push(mk(t, Origin::Introduced, "基线时这条命令整体通过，这个测试是新失败的"));
                    }
                } else {
                    findings.push(mk(WHOLE, Origin::Introduced, "基线时这条命令通过，现在失败"));
                }
            }
            (true, true) => {
                if base.parsed && cur.parsed {
                    let before: BTreeSet<&String> = base.failed_tests.iter().collect();
                    let now: BTreeSet<&String> = cur.failed_tests.iter().collect();
                    for t in &cur.failed_tests {
                        if before.contains(t) {
                            findings.push(mk(t, Origin::PreExisting, "导入前就已失败，现在仍然失败"));
                        } else {
                            findings.push(mk(t, Origin::Introduced, "基线时这个测试没有失败，是新失败的"));
                        }
                    }
                    for t in &base.failed_tests {
                        if !now.contains(t) {
                            findings.push(mk(t, Origin::Fixed, "基线时失败，现在不再失败"));
                        }
                    }
                } else {
                    findings.push(mk(WHOLE, Origin::Unknown, "基线时和现在都失败，但无法逐项比较，不能确认是不是同一个原因"));
                }
            }
        }
    }
    let count = |o: Origin| findings.iter().filter(|f| f.origin == o).count();
    CheckReport {
        pre_existing: count(Origin::PreExisting), introduced: count(Origin::Introduced), fixed: count(Origin::Fixed), unknown: count(Origin::Unknown),
        findings, results: current.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn res(id: &str, status: &str, failed: &[&str], parsed: bool) -> CommandResult {
        CommandResult { id: id.into(), label: id.into(), kind: "test".into(), command: "x".into(), status: status.into(), exit_code: Some(0),
            duration_ms: 1, output_tail: String::new(), failed_tests: failed.iter().map(|s| s.to_string()).collect(), parsed }
    }
    fn origins(r: &CheckReport) -> Vec<(String, Origin)> { r.findings.iter().map(|f| (f.item.clone(), f.origin)).collect() }

    #[test]
    fn parses_cargo_pytest_go_maven_gradle_and_jest() {
        let cargo = "running 3 tests\ntest a::ok ... ok\ntest a::bad ... FAILED\ntest b::worse ... FAILED\n\nfailures:\n    a::bad\n";
        assert_eq!(parse_failures("cargo test --workspace", cargo).unwrap(), vec!["a::bad", "b::worse"]);
        assert_eq!(parse_failures("cargo test", "error[E0432]: unresolved import").unwrap(), Vec::<String>::new());
        let py = "FAILED tests/test_a.py::test_x - AssertionError: 1 != 2\nERROR tests/test_b.py\n";
        assert_eq!(parse_failures("python3 -m pytest -q", py).unwrap(), vec!["tests/test_a.py::test_x", "tests/test_b.py"]);
        let go = "--- FAIL: TestA (0.00s)\n    --- FAIL: TestA/sub (0.00s)\nFAIL\tpkg\t0.1s\n";
        assert_eq!(parse_failures("go test ./...", go).unwrap(), vec!["TestA", "TestA/sub"]);
        let mvn = "[ERROR] Failures: \n[ERROR]   OrderTest.shouldPay:42 expected 1 but was 2\n[ERROR] Errors: \n[ERROR]   UserTest.load » NullPointer\n[ERROR] \n[ERROR] Tests run: 9, Failures: 1\n";
        assert_eq!(parse_failures("./mvnw -B test", mvn).unwrap(), vec!["OrderTest.shouldPay", "UserTest.load"]);
        assert_eq!(parse_failures("./gradlew test", "OrderTest > shouldPay FAILED\n").unwrap(), vec!["OrderTest > shouldPay"]);
        assert_eq!(parse_failures("pnpm test", " FAIL  src/a.test.ts > adds\n").unwrap(), vec!["src/a.test.ts > adds"]);
        assert!(parse_failures("make check", "x").is_none(), "没有解析器时返回 None 而不是空列表");
    }

    #[test]
    fn classify_separates_pre_existing_introduced_and_fixed() {
        let base = vec![res("t", "failed", &["a", "b"], true)];
        let cur = vec![res("t", "failed", &["b", "c"], true)];
        let r = classify(&base, &cur);
        assert_eq!(origins(&r), vec![("b".into(), Origin::PreExisting), ("c".into(), Origin::Introduced), ("a".into(), Origin::Fixed)]);
        assert_eq!((r.pre_existing, r.introduced, r.fixed, r.unknown), (1, 1, 1, 0));
    }

    #[test]
    fn classify_command_level_and_unknown_cases_honestly() {
        // 基线通过、现在失败：未解析 → 命令级“AI 引入”
        let r = classify(&[res("t", "passed", &[], true)], &[res("t", "failed", &[], false)]);
        assert_eq!(origins(&r), vec![(WHOLE.into(), Origin::Introduced)]);
        // 基线通过、现在有失败项：逐项标为引入
        let r = classify(&[res("t", "passed", &[], true)], &[res("t", "failed", &["x"], true)]);
        assert_eq!(origins(&r), vec![("x".into(), Origin::Introduced)]);
        // 基线就失败（如编译错误，未解析），现在仍失败：无法确认，不归咎于任何一方
        let r = classify(&[res("b", "failed", &[], false)], &[res("b", "failed", &[], false)]);
        assert_eq!(origins(&r), vec![(WHOLE.into(), Origin::Unknown)]);
        // 基线失败、现在通过
        assert_eq!(classify(&[res("b", "failed", &[], false)], &[res("b", "passed", &[], true)]).fixed, 1);
        // 基线没跑过这条命令
        assert_eq!(classify(&[], &[res("n", "failed", &[], false)]).unknown, 1);
        // 基线里被跳过的命令也不能当作通过
        assert_eq!(classify(&[res("n", "skipped", &[], false)], &[res("n", "failed", &[], false)]).unknown, 1);
        // 都通过、或现在被跳过：没有发现
        assert!(classify(&[res("t", "passed", &[], true)], &[res("t", "passed", &[], true), res("s", "skipped", &[], false)]).findings.is_empty());
        // 超时算失败
        assert_eq!(classify(&[res("t", "passed", &[], true)], &[res("t", "timeout", &[], false)]).introduced, 1);
    }

    fn bc(id: &str, kind: &str, command: &str) -> BaselineCommand {
        BaselineCommand { id: id.into(), label: id.into(), kind: kind.into(), command: command.into(), reason: String::new(), default_on: true }
    }

    #[tokio::test]
    async fn runs_commands_records_output_exit_codes_timeouts_and_skips_after_setup_failure() {
        let d = tempfile::tempdir().unwrap();
        let mut events = Vec::new();
        let r = run_commands(d.path(), &[
            bc("ok", "build", "echo hello && echo $CI"),
            bc("bad", "test", "echo 'test a::b ... FAILED' ; exit 3"),
            bc("slow", "test", "sleep 5"),
        ], Duration::from_millis(600), |p| events.push(match p { Progress::Started(c) => format!("start:{}", c.id), Progress::Finished(r) => format!("end:{}:{}", r.id, r.status) })).await;
        assert_eq!((r[0].status.as_str(), r[0].exit_code), ("passed", Some(0)));
        assert!(r[0].output_tail.contains("hello") && r[0].output_tail.contains("true"), "CI=true");
        assert_eq!((r[1].status.as_str(), r[1].exit_code), ("failed", Some(3)));
        assert_eq!(r[2].status, "timeout");
        assert!(r[2].duration_ms < 4000, "超时后立即终止");
        assert_eq!(events.len(), 6);

        let r = run_commands(d.path(), &[bc("install", "setup", "exit 1"), bc("t", "test", "echo hi")], Duration::from_secs(5), |_| {}).await;
        assert_eq!((r[0].status.as_str(), r[1].status.as_str()), ("failed", "skipped"));

        // 输出被解析为逐项失败（命令字符串含 "cargo test" 即选用对应解析器）
        let c = bc("ct", "test", "printf 'test x::y ... FAILED\\n'; exit 101 # cargo test");
        let r = run_commands(d.path(), &[c], Duration::from_secs(5), |_| {}).await;
        assert!(r[0].parsed && r[0].failed_tests == vec!["x::y"], "{:?}", r[0]);
    }

    #[tokio::test]
    async fn missing_command_is_reported_as_failure_not_crash() {
        let d = tempfile::tempdir().unwrap();
        let r = run_commands(d.path(), &[bc("x", "build", "definitely-not-a-real-command-xyz")], Duration::from_secs(5), |_| {}).await;
        assert_eq!(r[0].status, "failed");
        assert_eq!(r[0].exit_code, Some(127));
    }
}
