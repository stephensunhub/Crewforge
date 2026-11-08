//! 真实的 `crewforge mcp-serve` 进程：stdio 上的 JSON-RPC，以及跨进程写入同一个 SQLite 库。

use crewforge_core::db::Db;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

struct Server { child: Child, stdin: ChildStdin, out: BufReader<ChildStdout>, next: u64, raw: Vec<String> }

impl Server {
    fn start(dir: &Path, emp: &str, project: &str, origin: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_crewforge"))
            .args(["mcp-serve", "--data-dir", &dir.to_string_lossy(), "--employee", emp, "--project", project, "--origin", origin])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        Self { child, stdin, out, next: 1, raw: vec![] }
    }
    fn send(&mut self, v: Value) { writeln!(self.stdin, "{v}").unwrap(); self.stdin.flush().unwrap(); }
    fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.next; self.next += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let mut line = String::new();
        self.out.read_line(&mut line).unwrap();
        self.raw.push(line.clone());
        let v: Value = serde_json::from_str(&line).unwrap_or_else(|_| panic!("stdout 里出现了非协议内容: {line:?}"));
        assert_eq!(v["id"], id);
        v
    }
    fn call(&mut self, name: &str, args: Value) -> (bool, String) {
        let r = self.rpc("tools/call", json!({"name": name, "arguments": args}));
        (r["result"]["isError"].as_bool().unwrap(), r["result"]["content"][0]["text"].as_str().unwrap().to_string())
    }
    fn finish(mut self) { drop(self.stdin); assert!(self.child.wait().unwrap().success(), "stdin 关闭后应正常退出"); }
}

fn setup() -> (tempfile::TempDir, Db, String, String, String) {
    let d = tempfile::tempdir().unwrap();
    let db = Db::open(d.path()).unwrap();
    let alice = db.add_employee("alice", "全栈", "p").unwrap();
    let bob = db.add_employee("bob", "全栈", "p").unwrap();
    let p = db.create_project("P", "", "/r").unwrap();
    db.add_member(&p.id, &alice.id, "lead").unwrap();
    db.add_member(&p.id, &bob.id, "developer").unwrap();
    (d, db, alice.id, bob.id, p.id)
}

#[test]
fn a_real_server_process_speaks_mcp_and_writes_to_the_shared_database() {
    let (d, db, alice, _bob, p) = setup();
    let mut s = Server::start(d.path(), &alice, &p, "task:t1");
    let init = s.rpc("initialize", json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}));
    assert_eq!(init["result"]["serverInfo"]["name"], "crewforge");
    s.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})); // 通知不应产生回复
    assert_eq!(s.rpc("tools/list", json!({}))["result"]["tools"].as_array().unwrap().len(), 7);

    let (err, text) = s.call("propose_issue", json!({"title": "推送方案", "summary": "WebSocket 还是 SSE", "options": [{"title": "SSE"}]}));
    assert!(!err, "{text}");
    // 应用这一侧（另一个进程）立刻能读到它，且提议人是被写死的 alice
    let issues = db.issue_views(&p).unwrap();
    assert_eq!((issues[0].issue.title.as_str(), issues[0].issue.raised_by.as_deref(), issues[0].options.len()), ("推送方案", Some(alice.as_str()), 1));
    assert!(db.recent_events_for_project(&p, 20).unwrap().iter().any(|e| e.kind == "agent_proposed"));
    assert!(s.raw.iter().all(|l| serde_json::from_str::<Value>(l).is_ok()), "stdout 只有协议消息");
    s.finish();
}

#[test]
fn identity_comes_from_the_launch_arguments_not_from_the_caller() {
    let (d, db, _alice, _bob, p) = setup();
    let outsider = db.add_employee("zed", "全栈", "p").unwrap();
    let mut s = Server::start(d.path(), &outsider.id, &p, "t");
    let (err, text) = s.call("propose_issue", json!({"title": "x", "summary": "s", "employee_id": db.employee_by_name("alice").unwrap().unwrap().id}));
    assert!(err && text.contains("不是这个项目的成员"), "{text}");
    assert!(db.list_issues(&p).unwrap().is_empty());
    s.finish();
}

#[test]
fn missing_arguments_fail_fast_without_polluting_stdout() {
    let o = Command::new(env!("CARGO_BIN_EXE_crewforge")).args(["mcp-serve", "--data-dir", "/tmp/nonexistent-x"]).output().unwrap();
    assert!(!o.status.success());
    assert!(o.stdout.is_empty(), "出错时 stdout 也必须保持干净");
}

#[test]
fn concurrent_writers_in_different_processes_do_not_hit_database_is_locked() {
    let (d, db, alice, bob, p) = setup();
    let topics_a = ["数据库迁移策略", "日志格式统一", "缓存失效方案"];
    let topics_b = ["接口版本管理", "错误码规范", "前端状态管理"];
    let (dir_a, dir_b) = (d.path().to_path_buf(), d.path().to_path_buf());
    let (pa, pb) = (p.clone(), p.clone());
    let ha = std::thread::spawn(move || {
        let mut s = Server::start(&dir_a, &alice, &pa, "task:a");
        for t in topics_a { let (e, m) = s.call("propose_issue", json!({"title": t, "summary": "s"})); assert!(!e, "{m}"); }
        s.finish();
    });
    let hb = std::thread::spawn(move || {
        let mut s = Server::start(&dir_b, &bob, &pb, "task:b");
        for t in topics_b { let (e, m) = s.call("propose_issue", json!({"title": t, "summary": "s"})); assert!(!e, "{m}"); }
        s.finish();
    });
    // 同时，测试进程（相当于应用）也在写事件
    for i in 0..60 { db.append_event("noise", None, None, &json!({"i": i})).unwrap(); }
    ha.join().unwrap();
    hb.join().unwrap();
    assert_eq!(db.list_issues(&p).unwrap().len(), 6);
}
