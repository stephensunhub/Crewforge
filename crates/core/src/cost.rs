//! 成本与额度统计：从事件流汇总每轮调用的用量，按员工、项目、用途、日期、任务拆分，并给出效率指标。
//!
//! 口径说明：
//! - `cost_usd` 是 CLI 报告的"等值估算"，不是订阅账单上的实际扣费；
//! - CLI 没给出的数据就是"未知"，不会被当成 0 计入，也不会被估算；
//! - `input_tokens` 只是未命中缓存的那部分，真正处理的上下文还包括缓存读写。

use crate::db::Db;
use anyhow::Result;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Totals {
    pub turns: u64,
    /// CLI 报告本轮出错的次数
    pub errors: u64,
    pub cost_usd: f64,
    /// 其中有费用数据的轮数（其余为未知）
    pub turns_with_cost: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub turns_with_tokens: u64,
    pub duration_ms: u64,
    pub turns_with_duration: u64,
}

impl Totals {
    fn add(&mut self, r: &Row) {
        self.turns += 1;
        self.errors += r.is_error as u64;
        if let Some(c) = r.cost {
            self.cost_usd += c;
            self.turns_with_cost += 1;
        }
        if [r.input, r.output, r.cache_read, r.cache_creation].iter().any(Option::is_some) {
            self.turns_with_tokens += 1;
        }
        self.input_tokens += r.input.unwrap_or(0);
        self.output_tokens += r.output.unwrap_or(0);
        self.cache_read_tokens += r.cache_read.unwrap_or(0);
        self.cache_creation_tokens += r.cache_creation.unwrap_or(0);
        if let Some(d) = r.duration {
            self.duration_ms += d;
            self.turns_with_duration += 1;
        }
    }
    /// 实际处理的上下文 token：未命中缓存的输入 + 缓存读 + 缓存写
    pub fn context_tokens(&self) -> u64 {
        self.input_tokens + self.cache_read_tokens + self.cache_creation_tokens
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Group {
    pub key: String,
    pub label: String,
    pub totals: Totals,
}

#[derive(Debug, Clone, Serialize)]
pub struct DayPoint {
    pub day: String,
    pub totals: Totals,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskCost {
    pub task_id: String,
    pub title: String,
    pub employee: String,
    pub project: String,
    pub totals: Totals,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Efficiency {
    /// 平均每轮处理的上下文 token（越小越省）
    pub avg_context_per_turn: Option<f64>,
    /// 缓存命中占比：缓存读 /（未缓存输入 + 缓存读 + 缓存写）
    pub cache_hit_ratio: Option<f64>,
    /// 开始执行时复用了已有会话的次数 / 总次数（越高越省）
    pub resumed_starts: u64,
    pub total_starts: u64,
    /// 执行失败（进程崩溃、中途退出等）的次数
    pub failed_runs: u64,
    pub recovered_sessions: u64,
    /// 记忆注入次数、注入条数、注入字数合计
    pub memory_injections: u64,
    pub memory_items: u64,
    pub memory_chars: u64,
    /// 每个开发任务平均用了几轮
    pub avg_turns_per_task: Option<f64>,
    /// 会议里的发言：人类 / AI（AI 发言才产生模型调用）
    pub meeting_human_messages: u64,
    pub meeting_agent_replies: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct QuotaPoint {
    pub ts: i64,
    pub five_hour: Option<f64>,
    pub seven_day: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct QuotaNow {
    pub ts: i64,
    pub status: String,
    pub five_hour: Option<f64>,
    pub seven_day: Option<f64>,
    pub five_resets_at: Option<i64>,
    pub seven_resets_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CostReport {
    pub since: Option<i64>,
    pub totals: Totals,
    pub by_employee: Vec<Group>,
    pub by_project: Vec<Group>,
    pub by_purpose: Vec<Group>,
    pub by_day: Vec<DayPoint>,
    pub top_tasks: Vec<TaskCost>,
    pub efficiency: Efficiency,
    pub quota_now: Option<QuotaNow>,
    pub quota_series: Vec<QuotaPoint>,
}

struct Row {
    ts: i64,
    employee_id: Option<String>,
    task_id: Option<String>,
    meeting_id: Option<String>,
    purpose: Option<String>,
    project_id: Option<String>,
    cost: Option<f64>,
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    cache_creation: Option<u64>,
    duration: Option<u64>,
    is_error: bool,
}

pub fn purpose_label(p: &str) -> &'static str {
    match p {
        "task" => "开发任务",
        "meeting" => "会议发言",
        "minutes" => "纪要起草",
        "onboarding" => "导入评估",
        _ => "其他",
    }
}

/// 距 1970-01-01 的天数 → "YYYY-MM-DD"（公历，Howard Hinnant 算法），避免引入日期库。
pub fn days_to_ymd(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", if m <= 2 { y + 1 } else { y }, m, d)
}

fn sorted(map: HashMap<String, (String, Totals)>) -> Vec<Group> {
    let mut v: Vec<Group> = map.into_iter().map(|(key, (label, totals))| Group { key, label, totals }).collect();
    v.sort_by(|a, b| {
        b.totals.cost_usd.partial_cmp(&a.totals.cost_usd).unwrap_or(std::cmp::Ordering::Equal)
            .then(b.totals.context_tokens().cmp(&a.totals.context_tokens()))
            .then(a.label.cmp(&b.label))
    });
    v
}

impl Db {
    /// 最近一次额度读数（调度器与成本页共用）。
    pub fn latest_quota(&self) -> Result<Option<QuotaNow>> {
        let row: Option<(i64, String)> = self.conn.lock().unwrap()
            .query_row("SELECT ts, payload FROM events WHERE kind = 'rate_limit' ORDER BY id DESC LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?))).ok();
        Ok(row.and_then(|(ts, p)| {
            let v: serde_json::Value = serde_json::from_str(&p).ok()?;
            Some(QuotaNow {
                ts, status: v["status"].as_str().unwrap_or("unknown").to_string(), five_hour: v["five_hour"].as_f64(), seven_day: v["seven_day"].as_f64(),
                five_resets_at: v["five_resets_at"].as_i64(), seven_resets_at: v["seven_resets_at"].as_i64(),
            })
        }))
    }

    /// `since`：只统计该时间（毫秒）之后的事件；`tz_offset_minutes`：本地时区相对 UTC 的分钟数（东八区为 480），用于按天归并。
    pub fn cost_report(&self, since: Option<i64>, tz_offset_minutes: i64) -> Result<CostReport> {
        let since_v = since.unwrap_or(0);
        let conn = self.conn.lock().unwrap();

        // 查找表：任务、项目、会议、员工（含已归档）
        let tasks: HashMap<String, (String, String, String)> = {
            let mut s = conn.prepare("SELECT id, repo_path, title, employee_id FROM tasks")?;
            s.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))?.collect::<std::result::Result<_, _>>()?
        };
        let projects_by_path: HashMap<String, (String, String)> = {
            let mut s = conn.prepare("SELECT path, id, name FROM projects")?;
            s.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?))))?.collect::<std::result::Result<_, _>>()?
        };
        let project_names: HashMap<String, String> = projects_by_path.values().cloned().collect();
        let meeting_project: HashMap<String, String> = {
            let mut s = conn.prepare("SELECT id, project_id FROM meetings")?;
            s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?
        };
        let employees: HashMap<String, String> = {
            let mut s = conn.prepare("SELECT id, name FROM employees")?;
            s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?
        };

        let rows: Vec<Row> = {
            let mut s = conn.prepare(
                "SELECT e.ts, e.employee_id,
                        json_extract(e.payload,'$.task_id'), json_extract(e.payload,'$.meeting_id'),
                        json_extract(e.payload,'$.purpose'), json_extract(e.payload,'$.project_id'),
                        json_extract(e.payload,'$.cost_usd'), json_extract(e.payload,'$.input_tokens'),
                        json_extract(e.payload,'$.output_tokens'), json_extract(e.payload,'$.cache_read_tokens'),
                        json_extract(e.payload,'$.cache_creation_tokens'), json_extract(e.payload,'$.duration_ms'),
                        json_extract(e.payload,'$.is_error')
                 FROM events e WHERE e.kind = 'turn_result' AND e.ts >= ?1 ORDER BY e.id",
            )?;
            let it = s.query_map([since_v], |r| {
                Ok(Row {
                    ts: r.get(0)?, employee_id: r.get(1)?, task_id: r.get(2)?, meeting_id: r.get(3)?, purpose: r.get(4)?,
                    project_id: r.get(5)?, cost: r.get(6)?, input: r.get::<_, Option<i64>>(7)?.map(|v| v.max(0) as u64),
                    output: r.get::<_, Option<i64>>(8)?.map(|v| v.max(0) as u64), cache_read: r.get::<_, Option<i64>>(9)?.map(|v| v.max(0) as u64),
                    cache_creation: r.get::<_, Option<i64>>(10)?.map(|v| v.max(0) as u64), duration: r.get::<_, Option<i64>>(11)?.map(|v| v.max(0) as u64),
                    is_error: r.get::<_, Option<i64>>(12)?.unwrap_or(0) != 0,
                })
            })?;
            it.collect::<std::result::Result<_, _>>()?
        };

        let mut totals = Totals::default();
        let mut by_emp: HashMap<String, (String, Totals)> = HashMap::new();
        let mut by_proj: HashMap<String, (String, Totals)> = HashMap::new();
        let mut by_purpose: HashMap<String, (String, Totals)> = HashMap::new();
        let mut by_day: BTreeMap<String, Totals> = BTreeMap::new();
        let mut by_task: HashMap<String, Totals> = HashMap::new();
        let mut task_turns = 0u64;
        for r in &rows {
            // 项目归属：事件里写明的；旧事件则由任务或会议反查
            let project = r.project_id.clone()
                .or_else(|| r.task_id.as_ref().and_then(|t| tasks.get(t)).and_then(|(path, _, _)| projects_by_path.get(path)).map(|(id, _)| id.clone()))
                .or_else(|| r.meeting_id.as_ref().and_then(|m| meeting_project.get(m)).cloned());
            let purpose = r.purpose.clone().unwrap_or_else(|| if r.meeting_id.is_some() { "meeting".into() } else { "task".into() });
            totals.add(r);
            let ek = r.employee_id.clone().unwrap_or_default();
            by_emp.entry(ek.clone()).or_insert_with(|| (employees.get(&ek).cloned().unwrap_or_else(|| "（未知员工）".into()), Totals::default())).1.add(r);
            let pk = project.clone().unwrap_or_default();
            by_proj.entry(pk.clone()).or_insert_with(|| (project_names.get(&pk).cloned().unwrap_or_else(|| "（未归属项目）".into()), Totals::default())).1.add(r);
            by_purpose.entry(purpose.clone()).or_insert_with(|| (purpose_label(&purpose).to_string(), Totals::default())).1.add(r);
            let day = days_to_ymd((r.ts + tz_offset_minutes * 60_000).div_euclid(86_400_000));
            by_day.entry(day).or_default().add(r);
            if purpose == "task" {
                if let Some(t) = &r.task_id {
                    by_task.entry(t.clone()).or_default().add(r);
                    task_turns += 1;
                }
            }
        }

        let mut top_tasks: Vec<TaskCost> = by_task.iter().map(|(id, t)| {
            let (path, title, emp) = tasks.get(id).cloned().unwrap_or_default();
            TaskCost {
                task_id: id.clone(), title: if title.is_empty() { "（任务已不存在）".into() } else { title },
                employee: employees.get(&emp).cloned().unwrap_or_default(),
                project: projects_by_path.get(&path).map(|(_, n)| n.clone()).unwrap_or_default(), totals: t.clone(),
            }
        }).collect();
        top_tasks.sort_by(|a, b| b.totals.cost_usd.partial_cmp(&a.totals.cost_usd).unwrap_or(std::cmp::Ordering::Equal).then(b.totals.context_tokens().cmp(&a.totals.context_tokens())));
        top_tasks.truncate(10);

        // 效率指标
        let count = |sql: &str| -> Result<u64> { Ok(conn.query_row(sql, [since_v], |r| r.get::<_, i64>(0))?.max(0) as u64) };
        let mut eff = Efficiency {
            total_starts: count("SELECT COUNT(*) FROM events WHERE kind = 'task_started' AND ts >= ?1")?,
            resumed_starts: count("SELECT COUNT(*) FROM events WHERE kind = 'task_started' AND ts >= ?1 AND json_extract(payload,'$.resume') = 1")?,
            failed_runs: count("SELECT COUNT(*) FROM events WHERE kind IN ('task_failed','task_error') AND ts >= ?1")?,
            recovered_sessions: count("SELECT COUNT(*) FROM events WHERE kind = 'session_recovered' AND ts >= ?1")?,
            memory_injections: count("SELECT COUNT(*) FROM events WHERE kind = 'memory_injected' AND ts >= ?1")?,
            memory_items: count("SELECT COALESCE(SUM(json_extract(payload,'$.count')),0) FROM events WHERE kind = 'memory_injected' AND ts >= ?1")?,
            memory_chars: count("SELECT COALESCE(SUM(json_extract(payload,'$.chars')),0) FROM events WHERE kind = 'memory_injected' AND ts >= ?1")?,
            meeting_human_messages: count("SELECT COUNT(*) FROM meeting_messages WHERE author_kind = 'human' AND created_at >= ?1")?,
            meeting_agent_replies: count("SELECT COUNT(*) FROM meeting_messages WHERE author_kind = 'employee' AND created_at >= ?1")?,
            ..Default::default()
        };
        if totals.turns_with_tokens > 0 {
            eff.avg_context_per_turn = Some(totals.context_tokens() as f64 / totals.turns_with_tokens as f64);
        }
        if totals.context_tokens() > 0 {
            eff.cache_hit_ratio = Some(totals.cache_read_tokens as f64 / totals.context_tokens() as f64);
        }
        let task_ids: HashSet<&String> = by_task.keys().collect();
        if !task_ids.is_empty() {
            eff.avg_turns_per_task = Some(task_turns as f64 / task_ids.len() as f64);
        }

        // 额度
        let quota_rows: Vec<(i64, String)> = {
            let mut s = conn.prepare("SELECT ts, payload FROM events WHERE kind = 'rate_limit' AND ts >= ?1 ORDER BY id")?;
            s.query_map([since_v], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?
        };
        let parse = |ts: i64, p: &str| -> Option<(QuotaPoint, serde_json::Value)> {
            let v: serde_json::Value = serde_json::from_str(p).ok()?;
            Some((QuotaPoint { ts, five_hour: v["five_hour"].as_f64(), seven_day: v["seven_day"].as_f64() }, v))
        };
        let mut series: Vec<QuotaPoint> = quota_rows.iter().filter_map(|(ts, p)| parse(*ts, p).map(|x| x.0)).collect();
        if series.len() > 240 {
            let step = series.len() as f64 / 240.0;
            series = (0..240).map(|i| series[((i as f64 * step) as usize).min(series.len() - 1)].clone()).collect();
        }
        // 当前额度：不受时间范围限制，取最近一条
        let latest: Option<(i64, String)> = conn.query_row("SELECT ts, payload FROM events WHERE kind = 'rate_limit' ORDER BY id DESC LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?))).ok();
        let quota_now = latest.and_then(|(ts, p)| parse(ts, &p)).map(|(pt, v)| QuotaNow {
            ts: pt.ts, status: v["status"].as_str().unwrap_or("unknown").to_string(), five_hour: pt.five_hour, seven_day: pt.seven_day,
            five_resets_at: v["five_resets_at"].as_i64(), seven_resets_at: v["seven_resets_at"].as_i64(),
        });

        Ok(CostReport {
            since, totals, by_employee: sorted(by_emp), by_project: sorted(by_proj), by_purpose: sorted(by_purpose),
            by_day: by_day.into_iter().map(|(day, totals)| DayPoint { day, totals }).collect(),
            top_tasks, efficiency: eff, quota_now, quota_series: series,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use serde_json::json;

    fn turn(cost: Option<f64>, input: Option<u64>, out: Option<u64>, cr: Option<u64>, cc: Option<u64>) -> serde_json::Value {
        json!({"cost_usd": cost, "input_tokens": input, "output_tokens": out, "cache_read_tokens": cr, "cache_creation_tokens": cc,
               "is_error": false, "duration_ms": 2000})
    }

    #[test]
    fn dates_are_correct_including_leap_days_and_timezones() {
        assert_eq!(days_to_ymd(0), "1970-01-01");
        assert_eq!(days_to_ymd(20_000), "2024-10-04");
        assert_eq!(days_to_ymd(19_782), "2024-02-29");
        assert_eq!(days_to_ymd(-1), "1969-12-31");
        assert_eq!(days_to_ymd(20_401), "2025-11-09");
    }

    #[test]
    fn aggregates_by_employee_project_purpose_and_task_and_marks_unknowns() {
        let db = Db::open_memory().unwrap();
        let alice = db.add_employee("alice", "全栈", "p").unwrap();
        let bob = db.add_employee("bob", "全栈", "p").unwrap();
        let shop = db.create_project("Shop", "", "/r1").unwrap();
        let blog = db.create_project("Blog", "", "/r2").unwrap();
        db.add_member(&shop.id, &alice.id, "developer").unwrap();
        let t1 = crate::db::Task { id: "t1".into(), repo_path: "/r1".into(), employee_id: alice.id.clone(), title: "登录".into(), branch: "b".into(), base_branch: "main".into(),
            base_commit: "c".into(), worktree_path: "/w".into(), status: "active".into(), merged_commit: None, created_at: 1, updated_at: 1 };
        db.create_task(&t1).unwrap();
        let m = db.create_meeting(&shop.id, "design", "评审", None, None, &[], 5).unwrap();

        // 新格式：有用途和归属项目
        let mut p = turn(Some(0.10), Some(10), Some(100), Some(1000), Some(500)); p["task_id"] = json!("t1"); p["purpose"] = json!("task"); p["project_id"] = json!(shop.id);
        db.append_event("turn_result", Some(&alice.id), None, &p).unwrap();
        // 旧格式：没有 purpose / project_id，靠 task_id 反查
        let mut p = turn(Some(0.05), Some(5), Some(50), Some(500), Some(0)); p["task_id"] = json!("t1");
        db.append_event("turn_result", Some(&alice.id), None, &p).unwrap();
        // 旧格式会议事件：靠 meeting_id 反查
        let mut p = turn(Some(0.02), Some(2), Some(20), Some(200), Some(0)); p["meeting_id"] = json!(m.id);
        db.append_event("turn_result", Some(&bob.id), None, &p).unwrap();
        // 导入评估：新格式，归属 blog
        let mut p = turn(Some(0.30), Some(1), Some(10), Some(0), Some(3000)); p["purpose"] = json!("onboarding"); p["project_id"] = json!(blog.id);
        db.append_event("turn_result", Some(&bob.id), None, &p).unwrap();
        // CLI 没给出用量和费用：必须是未知，而不是 0
        db.append_event("turn_result", Some(&bob.id), None, &turn(None, None, None, None, None)).unwrap();

        let r = db.cost_report(None, 0).unwrap();
        assert_eq!(r.totals.turns, 5);
        assert!((r.totals.cost_usd - 0.47).abs() < 1e-9);
        assert_eq!((r.totals.turns_with_cost, r.totals.turns_with_tokens), (4, 4), "未知的轮数单独统计");
        assert_eq!(r.totals.cache_read_tokens, 1700);
        assert_eq!(r.totals.context_tokens(), 18 + 1700 + 3500);

        let emp = |k: &str| r.by_employee.iter().find(|g| g.label == k).unwrap().totals.clone();
        assert_eq!(emp("alice").turns, 2);
        assert!((emp("alice").cost_usd - 0.15).abs() < 1e-9);
        assert_eq!(emp("bob").turns, 3);
        assert_eq!(r.by_employee[0].label, "bob", "按费用从高到低");

        let proj = |k: &str| r.by_project.iter().find(|g| g.label == k).map(|g| g.totals.clone());
        assert_eq!(proj("Shop").unwrap().turns, 3, "新旧两种格式都归到 Shop（含会议）");
        assert_eq!(proj("Blog").unwrap().turns, 1);
        assert_eq!(proj("（未归属项目）").unwrap().turns, 1, "无法归属的单独列出，不丢");

        let purpose = |k: &str| r.by_purpose.iter().find(|g| g.label == k).map(|g| g.totals.turns);
        assert_eq!((purpose("开发任务"), purpose("会议发言"), purpose("导入评估")), (Some(3), Some(1), Some(1)));

        assert_eq!(r.top_tasks.len(), 1);
        assert_eq!((r.top_tasks[0].title.as_str(), r.top_tasks[0].project.as_str(), r.top_tasks[0].totals.turns), ("登录", "Shop", 2));
        assert_eq!(r.efficiency.avg_turns_per_task, Some(2.0));
        assert!((r.efficiency.cache_hit_ratio.unwrap() - 1700.0 / 5218.0).abs() < 1e-9);
        assert!((r.efficiency.avg_context_per_turn.unwrap() - 5218.0 / 4.0).abs() < 1e-9, "平均值只按有数据的轮数算");
        assert_eq!(r.totals.duration_ms, 10_000);
    }

    #[test]
    fn time_window_days_and_timezone_bucketing() {
        let db = Db::open_memory().unwrap();
        let e = db.add_employee("a", "r", "p").unwrap();
        let ins = |ts: i64, cost: f64| {
            db.conn.lock().unwrap().execute(
                "INSERT INTO events (ts, kind, employee_id, payload) VALUES (?1,'turn_result',?2,?3)",
                params![ts, e.id, turn(Some(cost), Some(1), Some(1), Some(1), Some(1)).to_string()]).unwrap();
        };
        let day = 86_400_000i64;
        ins(20_000 * day + 23 * 3_600_000, 1.0); // UTC 2024-10-04 23:00
        ins(20_000 * day + 25 * 3_600_000, 2.0); // UTC 2024-10-05 01:00
        ins(19_000 * day, 4.0);                  // 很久以前
        let all = db.cost_report(None, 0).unwrap();
        assert_eq!(all.totals.turns, 3);
        assert_eq!(all.by_day.iter().map(|d| d.day.as_str()).collect::<Vec<_>>(), vec!["2022-01-08", "2024-10-04", "2024-10-05"]);
        // 东八区：UTC 23:00 与次日 01:00 都落在本地的 10-05 和 10-05
        let cn = db.cost_report(None, 480).unwrap();
        let days: Vec<_> = cn.by_day.iter().map(|d| (d.day.as_str(), d.totals.turns)).collect();
        assert_eq!(days, vec![("2022-01-08", 1), ("2024-10-05", 2)]);
        // 时间窗口
        let recent = db.cost_report(Some(19_999 * day), 0).unwrap();
        assert_eq!((recent.totals.turns, recent.totals.cost_usd), (2, 3.0));
        assert_eq!(db.cost_report(Some(21_000 * day), 0).unwrap().totals.turns, 0);
    }

    #[test]
    fn efficiency_counts_resumes_failures_recoveries_memory_and_meetings() {
        let db = Db::open_memory().unwrap();
        let p = db.create_project("P", "", "/r").unwrap();
        for resume in [false, true, true] { db.append_event("task_started", None, None, &json!({"resume": resume})).unwrap(); }
        db.append_event("task_failed", None, None, &json!({"error": "x"})).unwrap();
        db.append_event("task_error", None, None, &json!({"error": "y"})).unwrap();
        db.append_event("session_recovered", None, None, &json!({})).unwrap();
        db.append_event("memory_injected", None, None, &json!({"count": 3, "chars": 400})).unwrap();
        db.append_event("memory_injected", None, None, &json!({"count": 2, "chars": 250})).unwrap();
        let e = db.add_employee("a", "r", "p").unwrap();
        db.add_member(&p.id, &e.id, "developer").unwrap();
        let m = db.create_meeting(&p.id, "retro", "m", None, None, &[], 5).unwrap();
        db.post_message(&m.id, "human", None, "hi").unwrap();
        db.post_message(&m.id, "human", None, "hi2").unwrap();
        db.post_message(&m.id, "employee", Some(&e.id), "reply").unwrap();
        db.post_message(&m.id, "system", None, "note").unwrap();
        let eff = db.cost_report(None, 0).unwrap().efficiency;
        assert_eq!((eff.total_starts, eff.resumed_starts, eff.failed_runs, eff.recovered_sessions), (3, 2, 2, 1));
        assert_eq!((eff.memory_injections, eff.memory_items, eff.memory_chars), (2, 5, 650));
        assert_eq!((eff.meeting_human_messages, eff.meeting_agent_replies), (2, 1), "系统提示不算发言");
        assert!(eff.avg_context_per_turn.is_none() && eff.cache_hit_ratio.is_none(), "没有数据时是未知，不是 0");
    }

    #[test]
    fn quota_series_and_current_state() {
        let db = Db::open_memory().unwrap();
        assert!(db.cost_report(None, 0).unwrap().quota_now.is_none());
        for i in 0..500 {
            db.append_event("rate_limit", None, None, &json!({"status": "allowed", "five_hour": i as f64 / 1000.0, "seven_day": 0.5, "five_resets_at": 1_700_000_000, "seven_resets_at": null})).unwrap();
        }
        let r = db.cost_report(None, 0).unwrap();
        assert_eq!(r.quota_series.len(), 240, "序列被降采样，界面不会因为点太多而卡");
        let now = r.quota_now.unwrap();
        assert_eq!((now.five_hour, now.five_resets_at, now.seven_resets_at), (Some(0.499), Some(1_700_000_000), None));
        // 当前额度不受时间窗口限制
        assert!(db.cost_report(Some(i64::MAX / 2), 0).unwrap().quota_now.is_some());
    }
}
