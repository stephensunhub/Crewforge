//! 卡住检测：员工的这一轮跑了很久、很久没有新动作、或者在反复折腾网络限制时，给人一个待办，
//! 说明卡在哪、建议怎么办，而不是让人对着“工作中”干等。
//!
//! 只看已有的事件（任务开始、工具调用、沙箱网络审批），不调用模型。

use crate::db::Db;
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

pub const DEFAULT_LONG_MINUTES: i64 = 45;
pub const DEFAULT_SILENT_MINUTES: i64 = 15;

/// 命令里出现这些，说明员工在绕过网络或证书限制（自己起代理、跳过证书校验等）。
const BYPASS_PATTERNS: &[&str] = &[
    "goinsecure", "--insecure", "curl -k", "curl -sk", "curl -ssk", "gosumdb=off", "node_tls_reject_unauthorized",
    "goproxy=http", "https_proxy=", "http_proxy=",
];

#[derive(Debug, Clone, Serialize)]
pub struct StuckItem {
    pub employee: String,
    pub project_id: Option<String>,
    /// network（被网络限制卡住）| bypass（在绕过限制）| silent（太久没动作）| long（这一轮太久）
    pub kind: String,
    pub title: String,
    pub detail: String,
    /// 建议加入白名单的域名（kind = network 时）
    pub hosts: Vec<String>,
    pub elapsed_min: i64,
    pub since: i64,
    /// 点“去看”该去的页面
    pub tab: String,
    pub meeting_id: Option<String>,
}

impl Db {
    pub fn stuck_thresholds(&self) -> (i64, i64) {
        let get = |k: &str, d: i64| self.get_setting(k).ok().flatten().and_then(|v| v.trim().parse::<i64>().ok()).filter(|n| *n > 0).unwrap_or(d);
        (get("stuck_long_minutes", DEFAULT_LONG_MINUTES), get("stuck_silent_minutes", DEFAULT_SILENT_MINUTES))
    }

    /// 给定正在工作的员工名，找出看起来卡住的。`now` 是当前时间（毫秒）。
    pub fn stuck_items(&self, names: &[String], now: i64) -> Result<Vec<StuckItem>> {
        let (long_min, silent_min) = self.stuck_thresholds();
        let acts = self.activities(names)?;
        let mut out = Vec::new();
        for a in acts {
            let (eid, sid): (String, i64) = {
                let conn = self.conn.lock().unwrap();
                let Some(eid): Option<String> = conn.query_row("SELECT id FROM employees WHERE name = ?1", [&a.employee], |r| r.get(0)).optional()? else { continue };
                let Some(sid): Option<i64> = conn.query_row("SELECT id FROM events WHERE employee_id = ?1 AND kind = 'task_started' ORDER BY id DESC LIMIT 1", [&eid], |r| r.get(0)).optional()? else { continue };
                (eid, sid)
            };
            let (hosts, bypass) = {
                let conn = self.conn.lock().unwrap();
                let mut hosts: Vec<String> = Vec::new();
                let mut st = conn.prepare("SELECT payload FROM events WHERE employee_id = ?1 AND kind = 'approval_requested' AND id > ?2")?;
                for p in st.query_map(params![eid, sid], |r| r.get::<_, String>(0))?.flatten() {
                    let v: serde_json::Value = serde_json::from_str(&p).unwrap_or_default();
                    if v["tool"] == "SandboxNetworkAccess" {
                        if let Some(h) = v["input"]["host"].as_str() {
                            if !hosts.iter().any(|x| x == h) { hosts.push(h.to_string()); }
                        }
                    }
                }
                let mut bypass: Option<String> = None;
                let mut st = conn.prepare("SELECT json_extract(payload, '$.input.command') FROM events WHERE employee_id = ?1 AND kind = 'tool_use' AND id > ?2 AND json_extract(payload, '$.tool') = 'Bash'")?;
                for cmd in st.query_map(params![eid, sid], |r| r.get::<_, Option<String>>(0))?.flatten().flatten() {
                    let low = cmd.to_lowercase();
                    if let Some(p) = BYPASS_PATTERNS.iter().find(|p| low.contains(**p)) { bypass = Some(p.to_string()); }
                }
                (hosts, bypass)
            };
            let to_allow: Vec<String> = hosts.iter().filter(|h| !self.is_sandbox_domain_allowed(h)).cloned().collect();
            let elapsed = (now - a.started_at).max(0) / 60_000;
            let quiet = (now - a.last_tool_at.unwrap_or(a.started_at)).max(0) / 60_000;
            let (kind, title, detail) = if !to_allow.is_empty() && (bypass.is_some() || elapsed >= 10) {
                ("network", format!("{} 疑似被网络限制卡住", a.employee),
                 format!("这一轮已运行 {elapsed} 分钟，期间请求过连接 {}，{}。把域名加入白名单后，下次执行起不再被拦；当前这一轮仍需要你在审批弹窗里放行，或停止批次后重新开始。如果是下载依赖，也可以自己在终端里把依赖下好。",
                    to_allow.join("、"), if bypass.is_some() { "还在用代理、跳过证书等办法绕过限制" } else { "一直没能连上" }))
            } else if let Some(p) = &bypass {
                ("bypass", format!("{} 在尝试绕过网络或证书限制", a.employee),
                 format!("这一轮已运行 {elapsed} 分钟，命令里出现了「{p}」这类绕过限制的做法。看一下工作日志，确认它卡在什么上；缺依赖的话可以自己在终端里下好。"))
            } else if quiet >= silent_min {
                ("silent", format!("{} 已经 {quiet} 分钟没有新动作", a.employee),
                 format!("这一轮已运行 {elapsed} 分钟，最近一次动作是{}。可能在等模型响应，也可能卡住了；看一下工作日志。", a.last_tool.as_deref().map(|t| format!(" {t}")).unwrap_or_else(|| "（还没有任何动作）".into())))
            } else if elapsed >= long_min {
                ("long", format!("{} 这一轮已运行 {elapsed} 分钟", a.employee),
                 format!("最近一次动作在 {quiet} 分钟前。本地模型本身慢也会这样；如果一直在重复同一类操作，看一下工作日志。"))
            } else {
                continue;
            };
            out.push(StuckItem { employee: a.employee, project_id: a.project_id, kind: kind.into(), title, detail, hosts: to_allow, elapsed_min: elapsed, since: a.started_at, tab: a.tab, meeting_id: a.meeting_id });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn setup() -> (Db, crate::db::Employee) {
        let db = Db::open_memory().unwrap();
        let e = db.add_employee("dev", "工程师", "p").unwrap();
        (db, e)
    }

    fn at(db: &Db, id: &str, ts: i64, kind: &str, p: serde_json::Value) {
        db.conn.lock().unwrap().execute("INSERT INTO events (ts, employee_id, kind, payload) VALUES (?1,?2,?3,?4)", params![ts, id, kind, p.to_string()]).unwrap();
    }

    const MIN: i64 = 60_000;

    #[test]
    fn a_healthy_run_is_not_reported() {
        let (db, e) = setup();
        let now = 100 * MIN;
        at(&db, &e.id, now - 5 * MIN, "task_started", json!({"prompt": "实现登录"}));
        at(&db, &e.id, now - MIN, "tool_use", json!({"tool": "Read", "input": {"file_path": "a"}}));
        assert!(db.stuck_items(&["dev".into()], now).unwrap().is_empty());
    }

    #[test]
    fn silence_and_long_runs_are_reported_with_thresholds_from_settings() {
        let (db, e) = setup();
        let now = 200 * MIN;
        at(&db, &e.id, now - 60 * MIN, "task_started", json!({"prompt": "实现登录"}));
        at(&db, &e.id, now - 30 * MIN, "tool_use", json!({"tool": "Bash", "input": {"command": "go test"}}));
        let s = db.stuck_items(&["dev".into()], now).unwrap();
        assert_eq!((s.len(), s[0].kind.as_str()), (1, "silent"), "{s:?}");
        assert!(s[0].detail.contains("Bash") && s[0].title.contains("30 分钟"));
        db.set_setting("stuck_silent_minutes", "40").unwrap();
        let s = db.stuck_items(&["dev".into()], now).unwrap();
        assert_eq!(s[0].kind, "long", "没到静默阈值，但这一轮已经 60 分钟（默认 45）：{s:?}");
        db.set_setting("stuck_long_minutes", "90").unwrap();
        assert!(db.stuck_items(&["dev".into()], now).unwrap().is_empty());
    }

    #[test]
    fn repeated_network_trouble_becomes_a_todo_that_names_the_hosts_to_whitelist() {
        let (db, e) = setup();
        let now = 300 * MIN;
        at(&db, &e.id, now - 90 * MIN, "task_started", json!({"prompt": "SQLite 存储实现"}));
        at(&db, &e.id, now - 80 * MIN, "approval_requested", json!({"tool": "SandboxNetworkAccess", "input": {"host": "storage.googleapis.com"}}));
        at(&db, &e.id, now - 70 * MIN, "approval_requested", json!({"tool": "SandboxNetworkAccess", "input": {"host": "proxy.golang.org"}}));
        at(&db, &e.id, now - 3 * MIN, "tool_use", json!({"tool": "Bash", "input": {"command": "GOINSECURE=x GOFLAGS=-mod=mod go get modernc.org/sqlite"}}));
        let s = db.stuck_items(&["dev".into()], now).unwrap();
        assert_eq!(s[0].kind, "network", "{s:?}");
        assert_eq!(s[0].hosts, vec!["storage.googleapis.com"], "已在内置白名单里的 proxy.golang.org 不再建议");
        assert!(s[0].detail.contains("绕过限制"));
        // 加入白名单后，这个域名不再是待办里的建议；剩下的是在绕过限制
        db.add_sandbox_domain("storage.googleapis.com").unwrap();
        let s = db.stuck_items(&["dev".into()], now).unwrap();
        assert_eq!((s[0].kind.as_str(), s[0].hosts.len()), ("bypass", 0), "{s:?}");
    }

    #[test]
    fn ordinary_flags_that_look_like_k_are_not_mistaken_for_bypassing() {
        let (db, e) = setup();
        let now = 100 * MIN;
        at(&db, &e.id, now - 5 * MIN, "task_started", json!({"prompt": "测试"}));
        at(&db, &e.id, now - 4 * MIN, "tool_use", json!({"tool": "Bash", "input": {"command": "pytest -k login && go test ./..."}}));
        assert!(db.stuck_items(&["dev".into()], now).unwrap().is_empty());
    }
}
