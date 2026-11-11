//! 自主推进批次运行期间，让 Mac 不因“空闲”而睡眠。
//!
//! 系统睡眠时应用、员工的 CLI 子进程和到 LLM 端点的连接都会被暂停；屏保本身不影响进程，
//! 但很多人的“空闲多久睡眠”设得很短（1 分钟），一旦没有别的程序撑着，批次就停了。
//! 做法：批次开始时起一个 `caffeinate -i -s -w <本进程>`（`-w` 保证应用退出它也跟着退出），
//! 没有批次在跑或开关关闭时结束它。只防“空闲睡眠”，不防显示器熄屏，也不防你合上盖子或手动睡眠。
//!
//! 开关是全局设置 `keep_awake`（默认开）；改了立即生效，包括正在跑的批次。

use std::process::{Child, Command};
use std::sync::{Mutex, OnceLock};

struct State {
    /// 应用启动时按设置初始化；默认关，所以没初始化的场合（命令行、测试）不会起任何进程
    enabled: bool,
    runs: usize,
    child: Option<Child>,
    /// 可替换，测试用
    command: Option<(String, Vec<String>)>,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State { enabled: false, runs: 0, child: None, command: default_command() }))
}

#[cfg(target_os = "macos")]
fn default_command() -> Option<(String, Vec<String>)> {
    Some(("/usr/bin/caffeinate".into(), vec!["-i".into(), "-s".into(), "-w".into(), std::process::id().to_string()]))
}
#[cfg(not(target_os = "macos"))]
fn default_command() -> Option<(String, Vec<String>)> { None }

fn alive(c: &mut Child) -> bool { matches!(c.try_wait(), Ok(None)) }

fn sync(s: &mut State) {
    let want = s.enabled && s.runs > 0;
    if let Some(c) = s.child.as_mut() {
        if !alive(c) { s.child = None; }
    }
    if want && s.child.is_none() {
        if let Some((prog, args)) = &s.command {
            s.child = Command::new(prog).args(args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().ok();
        }
    } else if !want {
        if let Some(mut c) = s.child.take() { let _ = c.kill(); let _ = c.wait(); }
    }
}

/// 开关（应用启动时按设置调用一次，设置里改动时再调用）。
pub fn set_enabled(on: bool) {
    let mut s = state().lock().unwrap();
    s.enabled = on;
    sync(&mut s);
}

pub fn is_enabled() -> bool { state().lock().unwrap().enabled }

/// 此刻是不是真的有一个防睡眠进程在撑着。
pub fn holding() -> bool {
    let mut s = state().lock().unwrap();
    s.child.as_mut().map(alive).unwrap_or(false)
}

/// 一个批次在跑的凭证，放手（批次结束，不论怎么结束）时取消。多个批次同时跑时共用一个防睡眠进程。
pub struct Hold(());

pub fn hold() -> Hold {
    let mut s = state().lock().unwrap();
    s.runs += 1;
    sync(&mut s);
    Hold(())
}

impl Drop for Hold {
    fn drop(&mut self) {
        let mut s = state().lock().unwrap();
        s.runs = s.runs.saturating_sub(1);
        sync(&mut s);
    }
}

#[cfg(test)]
pub(crate) fn use_command_for_test(program: &str, args: &[&str]) {
    state().lock().unwrap().command = Some((program.into(), args.iter().map(|a| a.to_string()).collect()));
}

impl crate::db::Db {
    /// 设置“批次运行期间防止 Mac 空闲睡眠”，默认开。
    pub fn keep_awake_setting(&self) -> bool {
        self.get_setting("keep_awake").ok().flatten().map(|v| v != "off").unwrap_or(true)
    }
    pub fn set_keep_awake_setting(&self, on: bool) -> anyhow::Result<()> {
        self.set_setting("keep_awake", if on { "on" } else { "off" })?;
        set_enabled(on);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 状态是全局的，两个测试不能同时跑
    static SERIAL: Mutex<()> = Mutex::new(());

    // 全局状态，测试放在同一个函数里按顺序做，避免并行互相影响
    #[test]
    fn holds_only_while_a_batch_runs_and_the_switch_is_on() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        use_command_for_test("/bin/sleep", &["30"]);
        set_enabled(false);
        let h1 = hold();
        assert!(!holding(), "开关关着：有批次也不防睡眠");
        set_enabled(true);
        assert!(holding(), "打开开关：正在跑的批次立刻生效");
        let h2 = hold();
        drop(h1);
        assert!(holding(), "还有一个批次在跑，继续撑着");
        drop(h2);
        assert!(!holding(), "批次都结束了就放开");
        let h3 = hold();
        assert!(holding());
        set_enabled(false);
        assert!(!holding(), "跑的时候关掉开关：立刻放开");
        drop(h3);
        set_enabled(false);
    }

    #[test]
    fn the_setting_defaults_to_on_and_is_remembered() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let db = crate::db::Db::open_memory().unwrap();
        assert!(db.keep_awake_setting(), "默认开");
        db.set_keep_awake_setting(false).unwrap();
        assert!(!db.keep_awake_setting());
        db.set_keep_awake_setting(true).unwrap();
        assert!(db.keep_awake_setting());
        set_enabled(false);
    }
}
