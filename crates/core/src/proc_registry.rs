//! 员工 CLI 进程登记：应用退出时终止它们，下次启动时清理上次遗留的孤儿。
//!
//! 员工进程放在独立的进程组里（便于整体终止），所以应用退出时它们不会跟着退出，
//! 会继续在员工的工作区里改文件，却没有任何人读它们的输出。这里登记每个进程的 pid（同时写到数据目录下的文件里，
//! 应用崩溃也不丢），退出时统一终止；启动时把文件里还活着、并且确实是员工 CLI 的进程终止掉。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

struct Reg {
    pids: BTreeSet<i32>,
    file: Option<PathBuf>,
}

fn reg() -> &'static Mutex<Reg> {
    static R: OnceLock<Mutex<Reg>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(Reg { pids: BTreeSet::new(), file: None }))
}

fn persist(r: &Reg) {
    if let Some(f) = &r.file {
        let text: String = r.pids.iter().map(|p| format!("{p}\n")).collect();
        let _ = std::fs::write(f, text);
    }
}

/// 指向登记文件，并清理上次遗留的孤儿进程。返回清理了几个。应用启动时调用一次。
pub fn init(data_dir: &Path) -> usize {
    let file = data_dir.join("children.pids");
    let n = reap_orphans(&file);
    let mut r = reg().lock().unwrap();
    r.file = Some(file);
    persist(&r);
    n
}

/// 登记一个员工进程；返回的守卫在进程结束（或被丢弃）时自动取消登记。
pub fn register(pid: u32) -> Registered {
    let mut r = reg().lock().unwrap();
    r.pids.insert(pid as i32);
    persist(&r);
    Registered { pid: pid as i32 }
}

pub struct Registered {
    pid: i32,
}

impl Drop for Registered {
    fn drop(&mut self) {
        let mut r = reg().lock().unwrap();
        r.pids.remove(&self.pid);
        persist(&r);
    }
}

/// 是不是员工 CLI：避免 pid 被系统复用给别的程序后误杀。
fn looks_like_employee_cli(pid: i32) -> bool {
    let Ok(o) = std::process::Command::new("ps").args(["-o", "command=", "-p", &pid.to_string()]).output() else { return false };
    let cmd = String::from_utf8_lossy(&o.stdout);
    (cmd.contains("--session-id") || cmd.contains("--resume")) && (cmd.contains("stream-json") || cmd.contains("--output-format"))
}

fn kill_group(pid: i32, sig: i32) {
    #[cfg(unix)]
    unsafe {
        libc::killpg(pid, sig);
        libc::kill(pid, sig);
    }
}

fn alive(pid: i32) -> bool {
    #[cfg(unix)]
    { unsafe { libc::kill(pid, 0) == 0 } }
    #[cfg(not(unix))]
    { false }
}

/// 终止所有登记的员工进程（应用退出时调用）：先请它们退出，稍等仍在的再强制终止。
pub fn terminate_all() -> usize {
    let pids: Vec<i32> = reg().lock().unwrap().pids.iter().copied().collect();
    for p in &pids { kill_group(*p, libc::SIGTERM); }
    std::thread::sleep(std::time::Duration::from_millis(300));
    for p in &pids { if alive(*p) { kill_group(*p, libc::SIGKILL); } }
    let mut r = reg().lock().unwrap();
    r.pids.clear();
    persist(&r);
    pids.len()
}

/// 清理登记文件里上次遗留、还活着并且确实是员工 CLI 的进程。
pub fn reap_orphans(file: &Path) -> usize {
    let Ok(text) = std::fs::read_to_string(file) else { return 0 };
    let mut n = 0;
    for p in text.lines().filter_map(|l| l.trim().parse::<i32>().ok()) {
        if alive(p) && looks_like_employee_cli(p) {
            kill_group(p, libc::SIGTERM);
            std::thread::sleep(std::time::Duration::from_millis(200));
            if alive(p) { kill_group(p, libc::SIGKILL); }
            n += 1;
        }
    }
    let _ = std::fs::write(file, "");
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    fn spawn_fake_cli() -> std::process::Child {
        // 命令行里带上员工 CLI 的特征参数，但实际只是 sleep
        let mut c = std::process::Command::new("sh");
        c.args(["-c", "sleep 60; :", "--session-id", "x", "--output-format", "stream-json"]);
        c.process_group(0);
        c.spawn().unwrap()
    }

    #[test]
    fn orphans_from_a_previous_run_are_reaped_but_unrelated_processes_are_not() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("children.pids");
        let mut orphan = spawn_fake_cli();
        let mut bystander = std::process::Command::new("sleep").arg("60").spawn().unwrap(); // 不是员工 CLI
        std::fs::write(&file, format!("{}\n{}\n999999\n", orphan.id(), bystander.id())).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(reap_orphans(&file), 1, "只清理看起来是员工 CLI 的");
        assert!(orphan.wait().is_ok(), "孤儿已终止");
        assert!(bystander.try_wait().unwrap().is_none(), "无关进程（pid 被复用的情形）不能误杀");
        let _ = bystander.kill();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "", "登记文件清空");
    }

    #[test]
    fn registered_processes_are_terminated_on_exit_and_unregistered_when_done() {
        let mut a = spawn_fake_cli();
        let guard = register(a.id());
        assert!(reg().lock().unwrap().pids.contains(&(a.id() as i32)));
        assert!(terminate_all() >= 1);
        assert!(a.wait().is_ok(), "退出时被终止");
        drop(guard);
        assert!(!reg().lock().unwrap().pids.contains(&(a.id() as i32)));
    }
}
