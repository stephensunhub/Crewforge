//! 人工介入中心：把每一次需要人出手的事汇总在一起——人批准的审批、加白名单、拍板议题、批准计划、
//! 决定目标变更、合并被拦、卡住了、升级给人的事，以及人在应用之外手动做的事（手动记录）。
//!
//! 目的有两个：眼下有哪些事等着人；回头看哪一类最频繁，才知道该先把哪类自动化掉。
//! 数据全部来自已有事件，不新增表；人在应用外做的事用“手动记录”（事件 `human_intervention`）。

use crate::db::Db;
use anyhow::{Result, bail};
use rusqlite::params;
use serde::Serialize;
use std::collections::BTreeMap;

/// 一条人工介入旁边可以直接点的操作。`kind`：goto（去某页，tab）| models（模型页，检查 LLM 端点）| settings（设置页）
/// | whitelist（加入沙箱白名单，domain）| merge（重新合并，task_id）| log（看员工工作日志，employee）
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct IvAction {
    pub kind: String,
    pub label: String,
    pub tab: Option<String>,
    pub domain: Option<String>,
    pub task_id: Option<String>,
    pub employee: Option<String>,
}

impl IvAction {
    fn new(kind: &str, label: &str) -> Self { Self { kind: kind.into(), label: label.into(), tab: None, domain: None, task_id: None, employee: None } }
}

/// 按报错内容推荐能直接在应用里做的操作。
pub fn error_actions(text: &str, employee: Option<&str>, project_tab: &str) -> Vec<IvAction> {
    let low = text.to_lowercase();
    let any = |ks: &[&str]| ks.iter().any(|k| low.contains(k));
    let mut v = Vec::new();
    if any(&["ehostunreach", "econnrefused", "no internet route", "connection refused", "连不上", "无路由", "timed out", "超时", "fetch failed", "dial tcp"]) {
        v.push(IvAction::new("models", "检查 LLM 端点（测试连接）"));
    }
    if (low.contains("已停用") || low.contains("停用")) && (low.contains("引擎") || low.contains("端点")) {
        v.push(IvAction::new("models", "去模型页启用"));
    }
    if low.contains("未提交") || low.contains("合并失败") {
        let mut a = IvAction::new("goto", "去任务页处理合并");
        a.tab = Some("tasks".into());
        v.push(a);
    }
    if any(&["沙箱", "sandbox", "operation not permitted", "permission denied", "network"]) {
        v.push(IvAction::new("settings", "去设置看沙箱白名单"));
    }
    if let Some(e) = employee {
        let mut a = IvAction::new("log", "查看工作日志");
        a.employee = Some(e.into());
        v.push(a);
    }
    let mut g = IvAction::new("goto", "去项目看");
    g.tab = Some(project_tab.into());
    v.push(g);
    // 同样的操作只留一个
    let mut seen = std::collections::HashSet::new();
    v.retain(|a| seen.insert((a.kind.clone(), a.label.clone())));
    v
}

#[derive(Debug, Clone, Serialize)]
pub struct Intervention {
    pub id: i64,
    pub ts: i64,
    /// approval | whitelist | issue | plan | goal | escalated | merge | stuck | import | note
    pub category: String,
    pub label: String,
    pub title: String,
    pub detail: String,
    pub project_id: Option<String>,
    pub project_name: Option<String>,
    pub employee: Option<String>,
    /// 点“去看”该去的页面
    pub tab: String,
    /// 出错类的介入带着原始报错
    pub error: Option<String>,
    /// 可以直接点的操作
    pub actions: Vec<IvAction>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InterventionReport {
    pub items: Vec<Intervention>,
    /// 各类别的数量（同一时间范围内），按数量从多到少
    pub counts: Vec<(String, String, usize)>,
}

pub const CATEGORIES: &[(&str, &str)] = &[
    ("approval", "审批"), ("whitelist", "白名单"), ("issue", "拍板议题"), ("plan", "批准计划"), ("goal", "目标变更"),
    ("error", "执行出错"), ("escalated", "升级给人"), ("merge", "合并被拦"), ("stuck", "卡住了"), ("import", "确认导入"), ("note", "手动记录"),
];

fn label_of(cat: &str) -> &'static str {
    CATEGORIES.iter().find(|(k, _)| *k == cat).map(|(_, l)| *l).unwrap_or("其他")
}

fn clip(s: &str, n: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= n { one } else { format!("{}…", one.chars().take(n).collect::<String>()) }
}

impl Db {
    /// 手动记录人在应用之外做的事（比如在终端里手动 `go get`、改了系统设置）。
    pub fn add_intervention_note(&self, project_id: Option<&str>, text: &str) -> Result<()> {
        let t = text.trim();
        if t.is_empty() {
            bail!("写一下你做了什么");
        }
        if t.chars().count() > 1000 {
            bail!("内容太长了（最多 1000 字）");
        }
        self.append_event_for(project_id, "human_intervention", None, None, &serde_json::json!({"text": t}))?;
        Ok(())
    }

    /// 记录界面上弹出的出错提示（同样的内容 60 秒内只记一次），人工介入里能回头看。
    pub fn record_ui_error(&self, text: &str) -> Result<()> {
        let t: String = text.trim().chars().take(600).collect();
        if t.is_empty() { return Ok(()); }
        let recent: i64 = self.conn.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM events WHERE kind = 'ui_error' AND json_extract(payload,'$.text') = ?1 AND ts > ?2", params![t, crate::db::now_ms() - 60_000], |r| r.get(0))?;
        if recent == 0 { self.append_event("ui_error", None, None, &serde_json::json!({"text": t}))?; }
        Ok(())
    }

    /// 把新出现的“卡住了”记进历史（同一员工、同一轮、同一种只记一次），待办消失后仍能回头看。
    pub fn record_stuck(&self, items: &[crate::stuck::StuckItem]) -> Result<Vec<crate::stuck::StuckItem>> {
        let mut fresh = Vec::new();
        for x in items {
            let exists: i64 = self.conn.lock().unwrap().query_row(
                "SELECT COUNT(*) FROM events WHERE kind = 'stuck_detected' AND json_extract(payload,'$.employee') = ?1 AND json_extract(payload,'$.since') = ?2 AND json_extract(payload,'$.kind') = ?3",
                params![x.employee, x.since, x.kind], |r| r.get(0))?;
            if exists == 0 {
                self.append_event_for(x.project_id.as_deref(), "stuck_detected", None, None, &serde_json::json!({"employee": x.employee, "since": x.since, "kind": x.kind, "title": x.title, "detail": x.detail, "hosts": x.hosts}))?;
                fresh.push(x.clone());
            }
        }
        Ok(fresh)
    }

    /// 汇总 `since` 之后的人工介入。`category` 不为空时只要这一类（计数仍是全部类别）。
    pub fn interventions(&self, since: i64, category: Option<&str>, limit: usize) -> Result<InterventionReport> {
        let rows: Vec<(i64, i64, String, String, Option<String>, Option<String>, Option<String>)> = {
            let conn = self.conn.lock().unwrap();
            let mut st = conn.prepare(
                "SELECT e.id, e.ts, e.kind, e.payload, e.project_id, p.name, m.name
                 FROM events e LEFT JOIN projects p ON p.id = e.project_id LEFT JOIN employees m ON m.id = e.employee_id
                 WHERE e.ts >= ?1 AND (
                    (e.kind = 'approval_decided' AND json_extract(e.payload,'$.decided_by') = 'human')
                    OR (e.kind = 'issue_decided' AND json_extract(e.payload,'$.decided_by') = 'human')
                    OR e.kind IN ('sandbox_domain_added','sandbox_domain_removed','plan_approved','plan_goal_decided','plan_goal_proposed',
                                  'task_merge_blocked','stuck_detected','autopilot_issue_escalated','lead_escalated_action',
                                  'onboarding_confirmed','human_intervention','autopilot_chain_limit','task_failed','task_error','checkpoint_failed','ui_error')
                    OR (e.kind = 'autopilot_step' AND (json_extract(e.payload,'$.text') LIKE '%没做成%' OR json_extract(e.payload,'$.text') LIKE '%没有完成%'
                         OR json_extract(e.payload,'$.text') LIKE '%没有自动恢复%' OR json_extract(e.payload,'$.text') LIKE '%被拒绝%')))
                 ORDER BY e.id DESC LIMIT 3000")?;
            st.query_map([since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?.collect::<std::result::Result<_, _>>()?
        };
        let mut all = Vec::new();
        for (id, ts, kind, payload, project_id, project_name, employee) in rows {
            let v: serde_json::Value = serde_json::from_str(&payload).unwrap_or_default();
            let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
            let mut error: Option<String> = None;
            let mut extra: Vec<IvAction> = Vec::new();
            let (cat, title, detail, tab): (&str, String, String, &str) = match kind.as_str() {
                "approval_decided" => {
                    let allow = v["decision"] == "allow";
                    let tool = s("tool");
                    let what = if tool == "SandboxNetworkAccess" { format!("连接 {}", v["input"]["host"].as_str().unwrap_or("?")) }
                        else if let Some(c) = v["input"]["command"].as_str() { format!("命令 {}", clip(c, 80)) }
                        else if let Some(f) = v["input"]["file_path"].as_str() { format!("{tool} {}", clip(f, 80)) } else { tool.clone() };
                    ("approval", format!("{}{}：{what}", if allow { "允许了 " } else { "拒绝了 " }, employee.clone().unwrap_or_default()), clip(&s("reason"), 120), "chat")
                }
                "issue_decided" => ("issue", format!("拍板《{}》", clip(&s("title"), 40)), clip(&s("decision"), 140), "issues"),
                "sandbox_domain_added" => ("whitelist", format!("把 {} 加入沙箱白名单", s("domain")), if v["warning"] == true { "共享托管域名，有数据外传风险".into() } else { String::new() }, "chat"),
                "sandbox_domain_removed" => ("whitelist", format!("把 {} 移出沙箱白名单", s("domain")), String::new(), "chat"),
                "plan_approved" => ("plan", format!("批准了 {} 个提议的计划条目", v["count"].as_i64().unwrap_or(0)), String::new(), "plan"),
                "plan_goal_decided" => ("goal", if v["approved"] == true { "批准了目标变更".into() } else { "驳回了目标变更".into() }, clip(&s("note"), 120), "plan"),
                "plan_goal_proposed" => ("goal", "负责人提议修改项目目标，等你决定".into(), clip(&s("reason"), 140), "plan"),
                "task_merge_blocked" => {
                    let k = s("kind");
                    if k != "blocked" && k != "conflict" { continue; }
                    if let Some(t) = v["task_id"].as_str() {
                        let mut a = IvAction::new("merge", "重新合并"); a.task_id = Some(t.into()); extra.push(a);
                    }
                    ("merge", if k == "conflict" { "合并冲突".to_string() } else { "合并被拦".to_string() },
                     if k == "conflict" { format!("冲突文件：{}", v["files"].as_array().into_iter().flatten().filter_map(|f| f.as_str()).collect::<Vec<_>>().join("、")) } else { clip(&s("reason"), 140) }, "tasks")
                }
                "stuck_detected" => {
                    for h in v["hosts"].as_array().into_iter().flatten().filter_map(|h| h.as_str()) {
                        let mut a = IvAction::new("whitelist", &format!("把 {h} 加入白名单")); a.domain = Some(h.into()); extra.push(a);
                    }
                    ("stuck", s("title"), clip(&s("detail"), 160), "chat")
                }
                "task_failed" | "task_error" => { error = Some(s("error")); ("error", format!("{} 执行出错", employee.clone().unwrap_or_else(|| "员工".into())), clip(&s("error"), 300), "chat") }
                "checkpoint_failed" => { error = Some(s("error")); ("error", format!("{} 保存检查点失败", employee.clone().unwrap_or_else(|| "员工".into())), clip(&s("error"), 300), "tasks") }
                "ui_error" => { error = Some(s("text")); ("error", "操作出错".into(), clip(&s("text"), 300), "chat") }
                "autopilot_step" => { error = Some(s("text")); ("error", "自主推进遇到问题".into(), clip(&s("text"), 300), "plan") }
                "autopilot_issue_escalated" => ("escalated", clip(&s("text"), 160), String::new(), "issues"),
                "autopilot_chain_limit" => ("escalated", clip(&s("text"), 200), String::new(), "duties"),
                "lead_escalated_action" => ("escalated", "负责人把一条行动项交给你".into(), clip(&s("reason"), 140), "actions"),
                "onboarding_confirmed" => ("import", "确认导入项目".into(), String::new(), "onboarding"),
                "human_intervention" => ("note", clip(&s("text"), 200), String::new(), "chat"),
                _ => continue,
            };
            // 可点的操作：该类专属的 + 出错类按报错内容推荐的 + 去看
            let mut actions = extra;
            if let Some(e) = &error { actions.extend(error_actions(e, employee.as_deref(), tab)); }
            else { let mut g = IvAction::new("goto", "去看"); g.tab = Some(tab.into()); actions.push(g); }
            if cat == "merge" || cat == "stuck" { let mut g = IvAction::new("goto", "去看"); g.tab = Some(tab.into()); actions.push(g); }
            all.push(Intervention { id, ts, category: cat.into(), label: label_of(cat).into(), title, detail, project_id, project_name, employee, tab: tab.into(), error, actions });
        }
        let mut by: BTreeMap<&str, usize> = BTreeMap::new();
        for i in &all { *by.entry(CATEGORIES.iter().find(|(k, _)| *k == i.category).map(|(k, _)| *k).unwrap_or("note")).or_default() += 1; }
        let mut counts: Vec<(String, String, usize)> = by.into_iter().map(|(k, n)| (k.to_string(), label_of(k).to_string(), n)).collect();
        counts.sort_by(|a, b| b.2.cmp(&a.2));
        let items = all.into_iter().filter(|i| category.is_none_or(|c| c == i.category)).take(limit).collect();
        Ok(InterventionReport { items, counts })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn human_involvement_is_collected_from_events_and_notes_and_counted_by_category() {
        let db = Db::open_memory().unwrap();
        let p = db.create_project("Todo", "", "/tmp/iv-p").unwrap().id;
        let e = db.add_employee("Ollama-1", "工程师", "x").unwrap();
        let ap = |by: &str, tool: &str, input: serde_json::Value| db.append_event_for(Some(&p), "approval_decided", Some(&e.id), None, &json!({"decided_by": by, "decision": "allow", "tool": tool, "input": input, "reason": "越界"})).unwrap();
        ap("human", "SandboxNetworkAccess", json!({"host": "storage.googleapis.com"}));
        ap("policy", "Read", json!({"file_path": "/x"})); // 策略自动放行的不算人工
        ap("human", "Bash", json!({"command": "go get modernc.org/sqlite"}));
        db.add_sandbox_domain("storage.googleapis.com").unwrap();
        db.append_event_for(Some(&p), "issue_decided", None, None, &json!({"decided_by": "human", "title": "乐观锁", "decision": "采用 version 字段"})).unwrap();
        db.append_event_for(Some(&p), "issue_decided", None, None, &json!({"decided_by": e.id, "title": "AI 自己定的", "decision": "x"})).unwrap();
        db.append_event_for(Some(&p), "task_merge_blocked", Some(&e.id), None, &json!({"task_id": "t", "kind": "blocked", "reason": "主工作区有未提交的改动"})).unwrap();
        db.append_event_for(Some(&p), "task_merge_blocked", Some(&e.id), None, &json!({"task_id": "t", "kind": "action_items", "items": []})).unwrap(); // 负责人处理的不算
        db.append_event_for(Some(&p), "autopilot_issue_escalated", Some(&e.id), None, &json!({"text": "Ollama-2 没有裁决《X》，交给你：格式不对"})).unwrap();
        db.append_event_for(Some(&p), "plan_approved", None, None, &json!({"count": 3})).unwrap();
        db.add_intervention_note(Some(&p), "在终端里手动执行了 go get modernc.org/sqlite，之后还原了 go.mod").unwrap();
        assert!(db.add_intervention_note(None, "   ").is_err());

        let r = db.interventions(0, None, 100).unwrap();
        let cats: Vec<&str> = r.items.iter().map(|i| i.category.as_str()).collect();
        assert_eq!(r.items.len(), 8, "{cats:?}");
        assert_eq!(cats.iter().filter(|c| **c == "approval").count(), 2, "只算人批准的两条");
        assert_eq!(cats.iter().filter(|c| **c == "issue").count(), 1, "AI 自己定的不算");
        assert_eq!(cats.iter().filter(|c| **c == "merge").count(), 1, "行动项类的拦截不算");
        assert!(r.items.iter().any(|i| i.category == "approval" && i.title.contains("连接 storage.googleapis.com") && i.project_name.as_deref() == Some("Todo")));
        assert!(r.items.iter().any(|i| i.category == "whitelist" && i.detail.contains("外传风险")));
        assert!(r.items.iter().any(|i| i.category == "note" && i.title.contains("go get")));
        assert_eq!(r.counts[0], ("approval".to_string(), "审批".to_string(), 2), "数量最多的排前面");

        // 只看一类，计数仍是全部；时间范围外的不算
        let only = db.interventions(0, Some("note"), 100).unwrap();
        assert_eq!((only.items.len(), only.counts.len() > 1), (1, true));
        assert!(db.interventions(crate::db::now_ms() + 10_000, None, 100).unwrap().items.is_empty());
    }

    #[test]
    fn stuck_detections_are_recorded_once_per_run_and_kind() {
        let db = Db::open_memory().unwrap();
        let item = crate::stuck::StuckItem { employee: "dev".into(), project_id: None, kind: "silent".into(), title: "dev 已经 20 分钟没有新动作".into(), detail: "d".into(), hosts: vec![], elapsed_min: 30, since: 1234, tab: "chat".into(), meeting_id: None };
        db.record_stuck(&[item.clone()]).unwrap();
        db.record_stuck(&[item.clone()]).unwrap();
        assert_eq!(db.interventions(0, Some("stuck"), 10).unwrap().items.len(), 1, "同一轮同一种只记一次");
        let mut other = item;
        other.kind = "long".into();
        db.record_stuck(&[other]).unwrap();
        assert_eq!(db.interventions(0, Some("stuck"), 10).unwrap().items.len(), 2);
    }

    #[test]
    fn errors_come_with_the_raw_message_and_actions_you_can_take_right_in_the_app() {
        let db = Db::open_memory().unwrap();
        let p = db.create_project("Todo", "", "/tmp/iv-err").unwrap().id;
        let e = db.add_employee("Ollama-1", "工程师", "x").unwrap();
        db.append_event_for(Some(&p), "task_failed", Some(&e.id), None, &json!({"error": "API Error: No internet route — check your connection or VPN (EHOSTUNREACH)"})).unwrap();
        db.append_event_for(Some(&p), "autopilot_step", None, None, &json!({"text": "《HTTP API 处理器》没做成：合并失败：Blocked { reason: \"主工作区有未提交的改动\" }"})).unwrap();
        db.append_event_for(Some(&p), "autopilot_step", None, None, &json!({"text": "派《X》给 Ollama-1"})).unwrap(); // 普通动态不算
        db.record_ui_error("无法合并：主工作区有未提交的改动").unwrap();
        db.record_ui_error("无法合并：主工作区有未提交的改动").unwrap(); // 60 秒内同样的只记一次
        db.append_event_for(Some(&p), "task_merge_blocked", Some(&e.id), None, &json!({"task_id": "t9", "kind": "blocked", "reason": "主工作区有未提交的改动"})).unwrap();

        let r = db.interventions(0, None, 50).unwrap();
        let errs: Vec<_> = r.items.iter().filter(|i| i.category == "error").collect();
        assert_eq!(errs.len(), 3, "{:?}", errs.iter().map(|i| &i.title).collect::<Vec<_>>());
        let net = errs.iter().find(|i| i.title.contains("执行出错")).unwrap();
        assert!(net.error.as_deref().unwrap().contains("EHOSTUNREACH"), "带着原始报错");
        let kinds: Vec<&str> = net.actions.iter().map(|a| a.kind.as_str()).collect();
        assert_eq!(kinds, vec!["models", "log", "goto"], "连接类错误：检查端点 + 看日志 + 去项目：{:?}", net.actions);
        let merge_err = errs.iter().find(|i| i.title == "自主推进遇到问题").unwrap();
        assert!(merge_err.actions.iter().any(|a| a.kind == "goto" && a.tab.as_deref() == Some("tasks")));
        let blocked = r.items.iter().find(|i| i.category == "merge").unwrap();
        assert!(blocked.actions.iter().any(|a| a.kind == "merge" && a.task_id.as_deref() == Some("t9")), "合并被拦：直接重新合并");
        assert_eq!(r.counts.iter().find(|c| c.0 == "error").unwrap().2, 3);
    }

    #[test]
    fn a_stuck_network_item_offers_whitelist_buttons_per_host() {
        let db = Db::open_memory().unwrap();
        let item = crate::stuck::StuckItem { employee: "dev".into(), project_id: None, kind: "network".into(), title: "dev 疑似被网络限制卡住".into(), detail: "d".into(), hosts: vec!["storage.googleapis.com".into()], elapsed_min: 90, since: 1, tab: "chat".into(), meeting_id: None };
        db.record_stuck(&[item]).unwrap();
        let i = &db.interventions(0, Some("stuck"), 5).unwrap().items[0];
        assert!(i.actions.iter().any(|a| a.kind == "whitelist" && a.domain.as_deref() == Some("storage.googleapis.com")), "{:?}", i.actions);
    }
}
