//! 用量页的数据层（参照 CC Switch 的用量统计）：同一套筛选条件下的概览指标、趋势、热力图、
//! 分维度统计和分页的调用明细。数据全部来自每次调用结束时记录的 `turn_result` 事件，不做估算。

use crate::cost::{Group, Latency, Totals, days_to_ymd, purpose_label};
use crate::db::{Db, now_ms};
use crate::provider::claude::ModelUse;
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UsageFilter {
    /// 时间范围（毫秒）；since 为空表示不限起点
    pub since: Option<i64>,
    pub until: Option<i64>,
    /// 执行引擎：claude | copilot
    pub provider: Option<String>,
    pub employee_id: Option<String>,
    pub project_id: Option<String>,
    pub model: Option<String>,
    pub purpose: Option<String>,
    /// ok | error
    pub status: Option<String>,
}

/// 一次调用（一次 `turn_result`）及其所属的员工、项目、任务或会议。
#[derive(Debug, Clone, Serialize)]
pub struct CallRec {
    pub event_id: i64,
    pub ts: i64,
    pub session_id: Option<String>,
    pub employee_id: String,
    pub employee: String,
    pub project_id: String,
    pub project: String,
    pub purpose: String,
    pub purpose_label: String,
    pub task_id: Option<String>,
    pub meeting_id: Option<String>,
    /// 任务或会议标题
    pub subject: String,
    pub provider: String,
    pub models: Vec<ModelUse>,
    /// 没有按模型拆分的数据（如 Copilot）时的单个模型名
    pub model: Option<String>,
    pub cost_usd: Option<f64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_creation_tokens: Option<u64>,
    pub premium_requests: Option<f64>,
    pub duration_ms: Option<u64>,
    pub ttft_ms: Option<u64>,
    pub is_error: bool,
    pub subtype: String,
}

impl CallRec {
    fn model_names(&self) -> Vec<String> {
        if !self.models.is_empty() {
            self.models.iter().map(|m| m.model.clone()).collect()
        } else {
            self.model.clone().into_iter().collect()
        }
    }

    /// 把这次调用折进 Totals（统计口径与成本页一致：缺失的数据是“未知”，不当成 0）。
    fn totals(&self) -> Totals {
        let mut t = Totals::default();
        t.add_call(self.cost_usd, [self.input_tokens, self.output_tokens, self.cache_read_tokens, self.cache_creation_tokens], self.duration_ms, self.is_error, self.premium_requests);
        t
    }
}

fn merge(into: &mut Totals, o: &Totals) {
    into.turns += o.turns;
    into.errors += o.errors;
    into.cost_usd += o.cost_usd;
    into.turns_with_cost += o.turns_with_cost;
    into.input_tokens += o.input_tokens;
    into.output_tokens += o.output_tokens;
    into.cache_read_tokens += o.cache_read_tokens;
    into.cache_creation_tokens += o.cache_creation_tokens;
    into.turns_with_tokens += o.turns_with_tokens;
    into.duration_ms += o.duration_ms;
    into.turns_with_duration += o.turns_with_duration;
    into.premium_requests += o.premium_requests;
}

#[derive(Debug, Clone, Serialize)]
pub struct SeriesPoint {
    pub label: String,
    pub ts: i64,
    pub totals: Totals,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageOverview {
    pub summary: Totals,
    /// hour | day
    pub granularity: String,
    pub series: Vec<SeriesPoint>,
    /// 最近 53 周的每日汇总（只受“非时间”的筛选影响），用于热力图
    pub heatmap: Vec<SeriesPoint>,
    pub latency: Latency,
    /// 各引擎的汇总（不受引擎筛选影响，用于引擎切换按钮上的数字）
    pub providers: Vec<Group>,
    /// 筛选条件下出现过的模型、员工、项目（供筛选菜单用，不受对应维度筛选影响）
    pub models: Vec<Group>,
    pub employees: Vec<Group>,
    pub projects: Vec<Group>,
    /// 速度（输出 token / 秒）的中位数，只算有耗时和输出的调用
    pub median_speed: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsagePage {
    pub total: usize,
    pub rows: Vec<CallRec>,
}

impl Db {
    /// 载入时间范围内的全部调用并补全名称。
    fn load_records(&self, since: Option<i64>, until: Option<i64>) -> Result<Vec<CallRec>> {
        let conn = self.conn.lock().unwrap();
        let names = |sql: &str| -> Result<HashMap<String, String>> {
            let mut s = conn.prepare(sql)?;
            Ok(s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<std::result::Result<_, _>>()?)
        };
        let employees = names("SELECT id, name FROM employees")?;
        let projects = names("SELECT id, name FROM projects")?;
        let tasks = names("SELECT id, title FROM tasks")?;
        let meetings = names("SELECT id, title FROM meetings")?;
        let meeting_project = names("SELECT id, project_id FROM meetings")?;
        let task_project: HashMap<String, String> = {
            let by_path = names("SELECT path, id FROM projects")?;
            let mut s = conn.prepare("SELECT id, repo_path FROM tasks")?;
            let v: Vec<(String, String)> = s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?;
            v.into_iter().filter_map(|(id, path)| by_path.get(&path).map(|p| (id, p.clone()))).collect()
        };
        let mut s = conn.prepare(
            "SELECT e.id, e.ts, e.session_id, e.employee_id, json_extract(e.payload,'$.task_id'), json_extract(e.payload,'$.meeting_id'),
                    json_extract(e.payload,'$.purpose'), json_extract(e.payload,'$.project_id'), json_extract(e.payload,'$.cost_usd'),
                    json_extract(e.payload,'$.input_tokens'), json_extract(e.payload,'$.output_tokens'), json_extract(e.payload,'$.cache_read_tokens'),
                    json_extract(e.payload,'$.cache_creation_tokens'), json_extract(e.payload,'$.duration_ms'), json_extract(e.payload,'$.is_error'),
                    json_extract(e.payload,'$.models'), json_extract(e.payload,'$.ttft_ms'), json_extract(e.payload,'$.subtype'),
                    json_extract(e.payload,'$.model'), json_extract(e.payload,'$.provider'), json_extract(e.payload,'$.premium_requests')
             FROM events e WHERE e.kind = 'turn_result' AND e.ts >= ?1 AND e.ts <= ?2 ORDER BY e.ts DESC, e.id DESC")?;
        let u = |v: Option<i64>| v.map(|x| x.max(0) as u64);
        let mut out = Vec::new();
        let it = s.query_map([since.unwrap_or(0), until.unwrap_or(i64::MAX)], |r| {
            Ok((
                r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?, r.get::<_, Option<String>>(5)?, r.get::<_, Option<String>>(6)?, r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<f64>>(8)?, [r.get::<_, Option<i64>>(9)?, r.get(10)?, r.get(11)?, r.get(12)?, r.get(13)?],
                r.get::<_, Option<i64>>(14)?.unwrap_or(0) != 0, r.get::<_, Option<String>>(15)?, r.get::<_, Option<i64>>(16)?,
                r.get::<_, Option<String>>(17)?, r.get::<_, Option<String>>(18)?, r.get::<_, Option<String>>(19)?, r.get::<_, Option<f64>>(20)?,
            ))
        })?;
        for row in it {
            let (event_id, ts, session_id, eid, task, meeting, purpose, project, cost, n, is_error, models, ttft, subtype, model, provider, premium) = row?;
            let project_id = project
                .or_else(|| task.as_ref().and_then(|t| task_project.get(t)).cloned())
                .or_else(|| meeting.as_ref().and_then(|m| meeting_project.get(m)).cloned())
                .unwrap_or_default();
            let purpose = purpose.unwrap_or_else(|| if meeting.is_some() { "meeting".into() } else { "task".into() });
            let eid = eid.unwrap_or_default();
            let subject = task.as_ref().and_then(|t| tasks.get(t)).or_else(|| meeting.as_ref().and_then(|m| meetings.get(m))).cloned().unwrap_or_default();
            out.push(CallRec {
                event_id, ts, session_id,
                employee: employees.get(&eid).cloned().unwrap_or_else(|| "（未知员工）".into()), employee_id: eid,
                project: projects.get(&project_id).cloned().unwrap_or_default(), project_id,
                purpose_label: purpose_label(&purpose).to_string(), purpose, task_id: task, meeting_id: meeting, subject,
                provider: provider.unwrap_or_else(|| "claude".into()),
                models: models.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default(), model,
                cost_usd: cost, input_tokens: u(n[0]), output_tokens: u(n[1]), cache_read_tokens: u(n[2]), cache_creation_tokens: u(n[3]),
                premium_requests: premium, duration_ms: u(n[4]), ttft_ms: ttft.map(|v| v.max(0) as u64), is_error, subtype: subtype.unwrap_or_default(),
            });
        }
        Ok(out)
    }

    /// 应用筛选。`skip` 指明忽略哪一个维度的筛选（用于各筛选菜单自己的选项统计）。
    fn filtered(recs: &[CallRec], f: &UsageFilter, skip: &str) -> Vec<CallRec> {
        recs.iter()
            .filter(|r| skip == "provider" || f.provider.as_ref().is_none_or(|x| &r.provider == x))
            .filter(|r| skip == "employee" || f.employee_id.as_ref().is_none_or(|x| &r.employee_id == x))
            .filter(|r| skip == "project" || f.project_id.as_ref().is_none_or(|x| &r.project_id == x))
            .filter(|r| skip == "model" || f.model.as_ref().is_none_or(|x| r.model_names().contains(x)))
            .filter(|r| f.purpose.as_ref().is_none_or(|x| &r.purpose == x))
            .filter(|r| match f.status.as_deref() { Some("ok") => !r.is_error, Some("error") => r.is_error, _ => true })
            .cloned()
            .collect()
    }

    fn group_by(recs: &[CallRec], per_model: bool, key: impl Fn(&CallRec) -> Vec<(String, String)>) -> Vec<Group> {
        let mut m: HashMap<String, (String, Totals)> = HashMap::new();
        for r in recs {
            let keys = key(r);
            let whole = r.totals();
            for (k, label) in keys {
                let e = m.entry(k.clone()).or_insert_with(|| (label, Totals::default()));
                // 按模型拆分时用该模型自己的用量；其他维度用整次调用
                match r.models.iter().find(|x| per_model && x.model == k) {
                    Some(mu) => {
                        let mut t = Totals::default();
                        t.add_call(mu.cost_usd, [Some(mu.input), Some(mu.output), Some(mu.cache_read), Some(mu.cache_creation)], None, r.is_error, None);
                        merge(&mut e.1, &t);
                    }
                    _ => merge(&mut e.1, &whole),
                }
            }
        }
        let mut v: Vec<Group> = m.into_iter().map(|(key, (label, totals))| Group { key, label, totals }).collect();
        v.sort_by(|a, b| b.totals.turns.cmp(&a.totals.turns).then(b.totals.cost_usd.partial_cmp(&a.totals.cost_usd).unwrap_or(std::cmp::Ordering::Equal)).then(a.label.cmp(&b.label)));
        v
    }

    /// 概览：指标、趋势（范围不超过 48 小时按小时，否则按天）、热力图、各筛选菜单的选项。
    pub fn usage_overview(&self, f: &UsageFilter, tz_offset_minutes: i64) -> Result<UsageOverview> {
        let now = now_ms();
        let all = self.load_records(f.since, f.until)?;
        let recs = Self::filtered(&all, f, "");
        let mut summary = Totals::default();
        let mut durations = Vec::new();
        let mut speeds = Vec::new();
        let (mut ttft_sum, mut ttft_n) = (0u64, 0u64);
        for r in &recs {
            merge(&mut summary, &r.totals());
            if let Some(d) = r.duration_ms { durations.push(d); }
            if let (Some(d), Some(o)) = (r.duration_ms, r.output_tokens) { if d > 0 && o > 0 { speeds.push(o as f64 / (d as f64 / 1000.0)); } }
            if let Some(t) = r.ttft_ms { ttft_sum += t; ttft_n += 1; }
        }
        durations.sort_unstable();
        speeds.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pick = |q: f64| if durations.is_empty() { None } else { Some(durations[(((durations.len() - 1) as f64) * q).round() as usize]) };

        // 趋势
        let end = f.until.unwrap_or(now).min(now.max(f.until.unwrap_or(0)));
        let start = f.since.or_else(|| recs.iter().map(|r| r.ts).min()).unwrap_or(end);
        let hourly = end - start <= 2 * 86_400_000;
        let step = if hourly { 3_600_000 } else { 86_400_000 };
        let tz = tz_offset_minutes * 60_000;
        let bucket = |ts: i64| ((ts + tz).div_euclid(step)) * step - tz;
        let label = |b: i64| {
            let local = b + tz;
            let day = days_to_ymd(local.div_euclid(86_400_000));
            if hourly { format!("{} {:02}:00", &day[5..], local.rem_euclid(86_400_000) / 3_600_000) } else { day }
        };
        let mut buckets: BTreeMap<i64, Totals> = BTreeMap::new();
        let mut b = bucket(start);
        let last = bucket(end);
        while b <= last && buckets.len() < 800 {
            buckets.insert(b, Totals::default());
            b += step;
        }
        for r in &recs {
            merge(buckets.entry(bucket(r.ts)).or_default(), &r.totals());
        }
        let series = buckets.into_iter().map(|(ts, totals)| SeriesPoint { label: label(ts), ts, totals }).collect();

        // 热力图：最近 53 周，只受非时间筛选影响
        let heat_all = self.load_records(Some(now - 371 * 86_400_000), None)?;
        let heat_recs = Self::filtered(&heat_all, &UsageFilter { since: None, until: None, ..f.clone() }, "");
        let mut days: BTreeMap<i64, Totals> = BTreeMap::new();
        let day_bucket = |ts: i64| (ts + tz).div_euclid(86_400_000) * 86_400_000 - tz;
        for r in &heat_recs {
            merge(days.entry(day_bucket(r.ts)).or_default(), &r.totals());
        }
        let heatmap = days.into_iter().map(|(ts, totals)| SeriesPoint { label: days_to_ymd((ts + tz).div_euclid(86_400_000)), ts, totals }).collect();

        let providers = Self::group_by(&Self::filtered(&all, f, "provider"), false, |r| vec![(r.provider.clone(), if r.provider == "copilot" { "GitHub Copilot".into() } else { "Claude Code".into() })]);
        let models = Self::group_by(&Self::filtered(&all, f, "model"), true, |r| {
            let names = r.model_names();
            if names.is_empty() { vec![("unknown".into(), "（未记录模型）".into())] } else { names.into_iter().map(|n| (n.clone(), n)).collect() }
        });
        let employees = Self::group_by(&Self::filtered(&all, f, "employee"), false, |r| vec![(r.employee_id.clone(), r.employee.clone())]);
        let projects = Self::group_by(&Self::filtered(&all, f, "project"), false, |r| vec![(r.project_id.clone(), if r.project.is_empty() { "（未归属项目）".into() } else { r.project.clone() })]);
        Ok(UsageOverview {
            summary, granularity: if hourly { "hour" } else { "day" }.into(), series, heatmap,
            latency: Latency { p50_ms: pick(0.5), p95_ms: pick(0.95), avg_ttft_ms: (ttft_n > 0).then(|| ttft_sum as f64 / ttft_n as f64), ttft_samples: ttft_n },
            providers, models, employees, projects,
            median_speed: (!speeds.is_empty()).then(|| speeds[speeds.len() / 2]),
        })
    }

    /// 分维度统计：model | employee | project | purpose | provider。
    pub fn usage_stats(&self, f: &UsageFilter, dim: &str) -> Result<Vec<Group>> {
        let recs = Self::filtered(&self.load_records(f.since, f.until)?, f, "");
        Ok(match dim {
            "model" => Self::group_by(&recs, true, |r| {
                let names = r.model_names();
                if names.is_empty() { vec![("unknown".into(), "（未记录模型）".into())] } else { names.into_iter().map(|n| (n.clone(), n)).collect() }
            }),
            "employee" => Self::group_by(&recs, false, |r| vec![(r.employee_id.clone(), r.employee.clone())]),
            "project" => Self::group_by(&recs, false, |r| vec![(r.project_id.clone(), if r.project.is_empty() { "（未归属项目）".into() } else { r.project.clone() })]),
            "purpose" => Self::group_by(&recs, false, |r| vec![(r.purpose.clone(), r.purpose_label.clone())]),
            "provider" => Self::group_by(&recs, false, |r| vec![(r.provider.clone(), if r.provider == "copilot" { "GitHub Copilot".into() } else { "Claude Code".into() })]),
            _ => anyhow::bail!("未知的统计维度：{dim}"),
        })
    }

    /// 分页的调用明细，最新的在前。
    pub fn usage_calls(&self, f: &UsageFilter, page: usize, page_size: usize) -> Result<UsagePage> {
        let recs = Self::filtered(&self.load_records(f.since, f.until)?, f, "");
        let total = recs.len();
        Ok(UsagePage { total, rows: recs.into_iter().skip(page * page_size).take(page_size.clamp(1, 200)).collect() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use serde_json::json;

    fn ins(db: &Db, ts: i64, emp: &str, payload: serde_json::Value) {
        db.conn.lock().unwrap().execute("INSERT INTO events (ts, kind, employee_id, payload) VALUES (?1,'turn_result',?2,?3)", params![ts, emp, payload.to_string()]).unwrap();
    }

    fn setup() -> (Db, String, String, String) {
        let db = Db::open_memory().unwrap();
        let a = db.add_employee("alice", "r", "p").unwrap();
        let d = db.add_employee_with("dave", "r", "p", "copilot").unwrap();
        let p = db.create_project("Shop", "", "/r1").unwrap();
        let now = now_ms();
        // alice：一次用了两个模型（含缓存），一次出错；dave（Copilot）：只有模型名与高级请求
        ins(&db, now - 3_600_000, &a.id, json!({"cost_usd": 0.30, "input_tokens": 3, "output_tokens": 16, "cache_read_tokens": 100, "cache_creation_tokens": 50, "duration_ms": 2000, "ttft_ms": 800,
            "purpose": "task", "project_id": p.id, "is_error": false, "provider": "claude",
            "models": [{"model":"sonnet","input":2,"output":14,"cache_read":100,"cache_creation":50,"cost_usd":0.25},{"model":"haiku","input":1,"output":2,"cache_read":0,"cache_creation":0,"cost_usd":0.05}]}));
        ins(&db, now - 1_800_000, &a.id, json!({"cost_usd": null, "duration_ms": 9000, "purpose": "meeting", "project_id": p.id, "is_error": true, "subtype": "error_during_execution", "provider": "claude"}));
        ins(&db, now - 600_000, &d.id, json!({"duration_ms": 3000, "purpose": "task", "project_id": p.id, "is_error": false, "provider": "copilot", "model": "grok-4.7", "premium_requests": 1.0}));
        ins(&db, now - 10 * 86_400_000, &a.id, json!({"cost_usd": 1.0, "input_tokens": 1, "output_tokens": 1, "cache_read_tokens": 0, "cache_creation_tokens": 0, "duration_ms": 1000, "purpose": "task", "project_id": p.id, "is_error": false, "provider": "claude"}));
        (db, a.id, d.id, p.id)
    }

    #[test]
    fn overview_summary_series_and_filter_menus() {
        let (db, a, _d, _p) = setup();
        let now = now_ms();
        // 最近 24 小时：按小时，3 次调用
        let f = UsageFilter { since: Some(now - 86_400_000), ..Default::default() };
        let o = db.usage_overview(&f, 0).unwrap();
        assert_eq!(o.summary.turns, 3);
        assert_eq!(o.granularity, "hour");
        assert!(o.series.len() >= 24 && o.series.iter().map(|p| p.totals.turns).sum::<u64>() == 3, "空的小时也有点，轴是连续的");
        assert_eq!((o.summary.errors, o.summary.premium_requests), (1, 1.0));
        assert_eq!((o.latency.p50_ms, o.latency.p95_ms), (Some(3000), Some(9000)));
        assert!(o.median_speed.is_some());
        // 近 30 天：按天，4 次调用
        let o30 = db.usage_overview(&UsageFilter { since: Some(now - 30 * 86_400_000), ..Default::default() }, 0).unwrap();
        assert_eq!((o30.granularity.as_str(), o30.summary.turns), ("day", 4));
        // 热力图不受时间范围影响
        assert_eq!(o.heatmap.iter().map(|p| p.totals.turns).sum::<u64>(), 4);
        // 引擎按钮上的数字不受引擎筛选影响
        let only_copilot = db.usage_overview(&UsageFilter { since: Some(now - 86_400_000), provider: Some("copilot".into()), ..Default::default() }, 0).unwrap();
        assert_eq!(only_copilot.summary.turns, 1);
        assert_eq!(only_copilot.providers.iter().map(|g| (g.key.as_str(), g.totals.turns)).collect::<Vec<_>>().len(), 2);
        // 员工筛选菜单的选项不受员工筛选影响
        let by_alice = db.usage_overview(&UsageFilter { since: Some(now - 86_400_000), employee_id: Some(a), ..Default::default() }, 0).unwrap();
        assert_eq!((by_alice.summary.turns, by_alice.employees.len()), (2, 2));
    }

    #[test]
    fn stats_split_models_and_filters_apply() {
        let (db, _a, d, p) = setup();
        let f = UsageFilter { since: Some(now_ms() - 86_400_000), ..Default::default() };
        let models = db.usage_stats(&f, "model").unwrap();
        let sonnet = models.iter().find(|g| g.key == "sonnet").unwrap();
        assert_eq!((sonnet.totals.output_tokens, sonnet.totals.cache_read_tokens), (14, 100));
        assert!((sonnet.totals.cost_usd - 0.25).abs() < 1e-9);
        assert!(models.iter().any(|g| g.key == "haiku") && models.iter().any(|g| g.key == "grok-4.7") && models.iter().any(|g| g.key == "unknown"));
        // 模型筛选：只留用过 haiku 的调用
        let haiku = db.usage_calls(&UsageFilter { model: Some("haiku".into()), ..f.clone() }, 0, 50).unwrap();
        assert_eq!(haiku.total, 1);
        assert_eq!(db.usage_stats(&f, "employee").unwrap().len(), 2);
        assert_eq!(db.usage_stats(&f, "project").unwrap()[0].label, "Shop");
        assert_eq!(db.usage_stats(&f, "purpose").unwrap().len(), 2);
        assert_eq!(db.usage_stats(&f, "provider").unwrap().len(), 2);
        assert!(db.usage_stats(&f, "bogus").is_err());
        let _ = (d, p);
    }

    #[test]
    fn calls_are_paged_newest_first_and_status_filter_works() {
        let (db, ..) = setup();
        let all = UsageFilter::default();
        let p0 = db.usage_calls(&all, 0, 3).unwrap();
        assert_eq!((p0.total, p0.rows.len()), (4, 3));
        assert!(p0.rows[0].ts > p0.rows[1].ts, "最新的在前");
        assert_eq!(db.usage_calls(&all, 1, 3).unwrap().rows.len(), 1);
        let errs = db.usage_calls(&UsageFilter { status: Some("error".into()), ..all.clone() }, 0, 10).unwrap();
        assert_eq!((errs.total, errs.rows[0].subtype.as_str()), (1, "error_during_execution"));
        assert_eq!(db.usage_calls(&UsageFilter { status: Some("ok".into()), ..all.clone() }, 0, 10).unwrap().total, 3);
        // 时间范围的起止
        let now = now_ms();
        assert_eq!(db.usage_calls(&UsageFilter { since: Some(now - 2 * 86_400_000), until: Some(now - 1_000_000), ..all }, 0, 10).unwrap().total, 2);
    }
}


/// 员工会话的当前上下文：用来发现哪个员工的会话快撑爆了。
#[derive(Debug, Clone, Serialize)]
pub struct SessionCtx {
    pub session_id: String,
    pub employee_id: String,
    pub employee: String,
    pub provider: String,
    /// 会话所在目录（任务 worktree 或项目目录）
    pub path: String,
    pub status: String,
    /// 最近一次模型调用时的上下文大小；引擎不提供或还没跑过为 None
    pub context_tokens: Option<u64>,
    pub updated_at: i64,
    /// 该员工因上下文过大被换新会话的次数
    pub rotations: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionsView {
    /// 超过这个值，下次任务会换新会话；0 表示不限
    pub limit: u64,
    pub sessions: Vec<SessionCtx>,
}

impl Db {
    pub fn session_contexts(&self) -> Result<SessionsView> {
        let limit = crate::runner::context_limit(self);
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT s.id, s.employee_id, e.name, e.provider, s.project_path, s.status, s.updated_at,
                    (SELECT json_extract(t.payload,'$.context_tokens') FROM events t
                      WHERE t.session_id = s.id AND t.kind = 'turn_result' ORDER BY t.id DESC LIMIT 1),
                    (SELECT COUNT(*) FROM events r WHERE r.employee_id = s.employee_id AND r.kind = 'session_rotated')
             FROM sessions s JOIN employees e ON e.id = s.employee_id
             WHERE e.archived = 0
             ORDER BY s.updated_at DESC")?;
        let rows = stmt.query_map([], |r| {
            Ok(SessionCtx {
                session_id: r.get(0)?, employee_id: r.get(1)?, employee: r.get(2)?, provider: r.get(3)?,
                path: r.get(4)?, status: r.get(5)?, updated_at: r.get(6)?,
                context_tokens: r.get::<_, Option<i64>>(7)?.map(|n| n.max(0) as u64),
                rotations: r.get::<_, i64>(8)?.max(0) as u64,
            })
        })?;
        Ok(SessionsView { limit, sessions: rows.collect::<std::result::Result<_, _>>()? })
    }
}

/// 一位正在工作的员工此刻在做什么（界面上“工作中”旁边显示）。
#[derive(Debug, Clone, Serialize)]
pub struct Activity {
    pub employee: String,
    /// 工作类型：任务 / 计划任务 / 会议发言 / 导入评估 / 起草计划 …
    pub what: String,
    /// 内容摘要（提示的第一行，去掉方括号标签）
    pub subject: String,
    pub project: Option<String>,
    pub started_at: i64,
    /// 最近一次动作（工具名）及时间；刚开始还没有动作则为空
    pub last_tool: Option<String>,
    pub last_tool_at: Option<i64>,
    pub origin: String,
    /// 点击后该去哪：项目、页签（chat | tasks | meetings | onboarding | plan）、会议
    pub project_id: Option<String>,
    pub tab: String,
    pub meeting_id: Option<String>,
    /// 员工连的是自建端点时，此刻端点上的排队情况（在等名额、还是几个请求同时在跑）
    pub queue: Option<crate::endpoint::QueueInfo>,
    /// 这一轮模型调用已经结束的时间。有值说明员工现在不是在“跑模型”，而是任务还在走后面的步骤
    /// （等评审、合并、验证…）；此时“多久没有新动作”没有意义，不该提示卡住。
    pub turn_finished_at: Option<i64>,
}

/// 工作类型对应的页面：会议发言去会议页，导入评估去导入页，起草计划去计划页，有任务的去任务页，其余去群聊。
pub fn work_tab(what: &str, has_meeting: bool, has_task: bool) -> &'static str {
    if has_meeting || what.starts_with("会议") || what.contains("纪要") || what.contains("对齐会") { "meetings" }
    else if what.starts_with("导入评估") { "onboarding" }
    else if what.contains("裁决") { "issues" }
    else if what.contains("计划") && !has_task { "plan" }
    else if what.contains("行动项") { "actions" }
    else if what.contains("记忆") { "memory" }
    else if what.contains("文档") { "docs" }
    else if has_task { "tasks" }
    else { "chat" }
}

/// 把任务开始事件里的提示归类成人能看懂的工作类型和摘要。
pub fn classify_prompt(prompt: &str, in_meeting: bool) -> (String, String) {
    let p = prompt.trim();
    let first = p.lines().next().unwrap_or("").trim();
    let (tag, rest) = match first.strip_prefix('[').and_then(|r| r.split_once(']')) {
        Some((t, r)) => (t.trim().to_string(), r.trim().to_string()),
        None => (String::new(), first.to_string()),
    };
    let what = if in_meeting { "会议发言".to_string() }
    else if let Some(kind) = tag.strip_prefix("导入评估:") { format!("导入评估（{kind}）") }
    else if p.contains("[计划条目]") { "计划任务".to_string() }
    else if tag.is_empty() { "任务".to_string() }
    else { tag.clone() };
    let subject = if tag.is_empty() || rest.is_empty() { first.to_string() } else { rest };
    (what, subject.chars().take(80).collect())
}

impl Db {
    /// 给定正在工作的员工名，查出各自此刻在做什么：取每人最近一次“任务开始”，
    /// 以及之后的最近一次工具调用。
    pub fn activities(&self, names: &[String]) -> Result<Vec<Activity>> {
        // 排队情况要用到数据库连接，先查好再往下持有连接（否则自己等自己）
        let mut queues: std::collections::HashMap<String, crate::endpoint::QueueInfo> = Default::default();
        for name in names {
            let eid: Option<String> = self.conn.lock().unwrap().query_row("SELECT id FROM employees WHERE name = ?1", [name], |r| r.get(0)).optional()?;
            if let Some(q) = eid.and_then(|e| self.queue_for(&e)) { queues.insert(name.clone(), q); }
        }
        let conn = self.conn.lock().unwrap();
        let mut out = Vec::new();
        for name in names {
            let Some(eid): Option<String> = conn.query_row("SELECT id FROM employees WHERE name = ?1", [name], |r| r.get(0)).optional()? else { continue };
            let start: Option<(i64, i64, Option<String>, String)> = conn.query_row(
                "SELECT id, ts, project_id, payload FROM events WHERE employee_id = ?1 AND kind = 'task_started' ORDER BY id DESC LIMIT 1",
                [&eid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?;
            let Some((sid, ts, pid, payload)) = start else { continue };
            let v: serde_json::Value = serde_json::from_str(&payload).unwrap_or_default();
            let (what, subject) = classify_prompt(v["prompt"].as_str().unwrap_or(""), v["meeting_id"].is_string());
            let tool: Option<(String, i64)> = conn.query_row(
                "SELECT json_extract(payload, '$.tool'), ts FROM events WHERE employee_id = ?1 AND kind = 'tool_use' AND id > ?2 ORDER BY id DESC LIMIT 1",
                params![eid, sid], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            // 所属项目：事件本身 → 任务开始时记的项目（系统发起的调用不进群聊，事件里没有项目）→ 会议所在项目
            // → 员工只在一个项目里时就是那个项目（兼容没有记录项目的旧事件）
            let pid = pid
                .or_else(|| v["project"].as_str().map(String::from))
                .or_else(|| v["meeting_id"].as_str().and_then(|m| conn.query_row("SELECT project_id FROM meetings WHERE id = ?1", [m], |r| r.get::<_, String>(0)).optional().ok().flatten()))
                .or_else(|| {
                    let mut st = conn.prepare("SELECT project_id FROM project_members WHERE employee_id = ?1").ok()?;
                    let ids: Vec<String> = st.query_map([&eid], |r| r.get(0)).ok()?.filter_map(|x| x.ok()).collect();
                    (ids.len() == 1).then(|| ids[0].clone())
                });
            let pid_for_link = pid.clone();
            let what_for_tab = what.clone();
            let project = match pid {
                Some(p) => conn.query_row("SELECT name FROM projects WHERE id = ?1", [p], |r| r.get(0)).optional()?,
                None => None,
            };
            let finished: Option<i64> = conn.query_row(
                "SELECT ts FROM events WHERE employee_id = ?1 AND kind IN ('turn_result','task_failed') AND id > ?2 ORDER BY id DESC LIMIT 1",
                params![eid, sid], |r| r.get(0)).optional()?;
            // 还在等端点名额时，“最近一次任务开始”是上一轮的：不要把上一轮的内容和时间当成现在在做的事
            let queue = queues.remove(name);
            let waiting = queue.as_ref().filter(|q| q.self_waiting);
            let (what, subject, ts, tool) = match waiting {
                Some(q) => ("等模型名额".to_string(), q.describe(), q.waiting_since.unwrap_or(ts), None),
                None => (what, subject, ts, tool),
            };
            out.push(Activity {
                employee: name.clone(), what, subject, project, started_at: ts, queue, turn_finished_at: finished,
                last_tool: tool.as_ref().map(|t| t.0.clone()), last_tool_at: tool.map(|t| t.1),
                origin: v["origin"].as_str().unwrap_or("").to_string(),
                tab: work_tab(&what_for_tab, v["meeting_id"].is_string(), v["task_id"].is_string()).to_string(),
                project_id: pid_for_link, meeting_id: v["meeting_id"].as_str().map(String::from),
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod activity_tests {
    use super::classify_prompt;

    #[test]
    fn prompts_are_classified_into_readable_work() {
        assert_eq!(classify_prompt("[导入评估:qa] Todo", false), ("导入评估（qa）".into(), "Todo".into()));
        assert_eq!(classify_prompt("[起草计划] Todo", false), ("起草计划".into(), "Todo".into()));
        assert_eq!(classify_prompt("脚手架与契约\n[计划条目] go mod init", false), ("计划任务".into(), "脚手架与契约".into()));
        assert_eq!(classify_prompt("请回复", true).0, "会议发言");
        assert_eq!(classify_prompt("实现登录\n细节…", false), ("任务".into(), "实现登录".into()));
    }
}

/// 员工工作日志里的一次执行：从“被派活”到这一轮结束。
#[derive(Debug, Clone, Serialize)]
pub struct LogRun {
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub what: String,
    pub subject: String,
    /// 派给员工的原始提示（不含系统注入的目标和记忆）
    pub prompt: String,
    pub project: Option<String>,
    pub origin: String,
    /// running | done | failed | interrupted
    pub status: String,
    pub error: Option<String>,
    pub duration_ms: Option<u64>,
    pub tool_count: usize,
    /// 工具调用流水（时间、工具名、一句话摘要），最多保留最近 300 条
    pub tools: Vec<LogTool>,
    /// 员工在这一轮里最后说的话（结果摘要）
    pub last_text: Option<String>,
    pub cost_usd: Option<f64>,
    pub tokens: Option<u64>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogTool {
    pub ts: i64,
    pub tool: String,
    pub summary: String,
}

fn tool_summary(tool: &str, input: &serde_json::Value) -> String {
    let pick = |k: &str| input[k].as_str().map(str::to_string);
    let s = match tool {
        "Bash" => pick("command"),
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => pick("file_path"),
        "Grep" | "Glob" => pick("pattern"),
        "Task" | "Agent" => pick("description"),
        _ => None,
    }
    .unwrap_or_else(|| input.to_string());
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    one.chars().take(160).collect()
}

impl Db {
    /// 员工的工作日志：最近 `limit` 次执行，新的在前。`busy` 表示员工此刻是否正在工作，
    /// 用来区分“进行中”和“被中断”（没有结束标记的旧执行）。
    pub fn employee_log(&self, employee_id: &str, limit: usize, busy: bool) -> Result<Vec<LogRun>> {
        let conn = self.conn.lock().unwrap();
        // 取最近 limit 次“开始”事件的位置，再读它之后的全部相关事件
        let from: Option<i64> = conn.query_row(
            "SELECT id FROM events WHERE employee_id = ?1 AND kind = 'task_started' ORDER BY id DESC LIMIT 1 OFFSET ?2",
            params![employee_id, limit.saturating_sub(1) as i64], |r| r.get(0)).optional()?;
        let from = from.unwrap_or(0);
        let mut stmt = conn.prepare(
            "SELECT e.id, e.ts, e.kind, e.payload, p.name FROM events e LEFT JOIN projects p ON p.id = e.project_id
             WHERE e.employee_id = ?1 AND e.id >= ?2 AND e.kind IN ('task_started','tool_use','agent_text','turn_result','task_failed')
             ORDER BY e.id")?;
        let rows = stmt.query_map(params![employee_id, from], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, Option<String>>(4)?)))?;
        let mut runs: Vec<LogRun> = Vec::new();
        for row in rows {
            let (_, ts, kind, payload, project) = row?;
            let v: serde_json::Value = serde_json::from_str(&payload).unwrap_or_default();
            match kind.as_str() {
                "task_started" => {
                    let prompt = v["prompt"].as_str().unwrap_or("").to_string();
                    let (what, subject) = classify_prompt(&prompt, v["meeting_id"].is_string());
                    runs.push(LogRun { started_at: ts, ended_at: None, what, subject, prompt, project, origin: v["origin"].as_str().unwrap_or("").into(), status: "interrupted".into(),
                        error: None, duration_ms: None, tool_count: 0, tools: vec![], last_text: None, cost_usd: None, tokens: None, model: None });
                }
                _ => {
                    let Some(run) = runs.last_mut() else { continue };
                    match kind.as_str() {
                        "tool_use" => {
                            let tool = v["tool"].as_str().unwrap_or("?").to_string();
                            run.tool_count += 1;
                            run.tools.push(LogTool { ts, summary: tool_summary(&tool, &v["input"]), tool });
                            if run.tools.len() > 300 { run.tools.remove(0); }
                        }
                        "agent_text" => run.last_text = v["text"].as_str().map(|t| t.chars().take(600).collect()),
                        "turn_result" => {
                            run.ended_at = Some(ts);
                            run.status = if v["is_error"].as_bool().unwrap_or(false) { "failed" } else { "done" }.into();
                            run.duration_ms = v["duration_ms"].as_u64();
                            run.cost_usd = v["cost_usd"].as_f64();
                            run.tokens = Some(v["input_tokens"].as_u64().unwrap_or(0) + v["output_tokens"].as_u64().unwrap_or(0));
                            run.model = v["models"].as_array().and_then(|m| m.first()).and_then(|m| m["model"].as_str()).map(String::from).or_else(|| v["model"].as_str().map(String::from));
                        }
                        "task_failed" => {
                            run.ended_at = Some(ts);
                            run.status = "failed".into();
                            run.error = v["error"].as_str().map(|e| e.chars().take(400).collect());
                        }
                        _ => {}
                    }
                }
            }
        }
        // 最新一次若没有结束标记且员工此刻在工作，就是进行中；更早的没有结束标记的是被中断
        if busy {
            if let Some(last) = runs.last_mut() {
                if last.ended_at.is_none() { last.status = "running".into(); }
            }
        }
        runs.reverse();
        runs.truncate(limit);
        Ok(runs)
    }
}

#[cfg(test)]
mod log_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn work_log_groups_events_into_runs_with_status_and_progress() {
        let db = Db::open_memory().unwrap();
        let e = db.add_employee("dev", "工程师", "p").unwrap();
        let ev = |k: &str, p: serde_json::Value| db.append_event(k, Some(&e.id), None, &p).unwrap();
        // 第一次：正常完成
        ev("task_started", json!({"prompt": "实现登录\n细节"}));
        ev("tool_use", json!({"tool": "Read", "input": {"file_path": "/a/b.rs"}}));
        ev("tool_use", json!({"tool": "Bash", "input": {"command": "cargo   test"}}));
        ev("agent_text", json!({"text": "做完了"}));
        ev("turn_result", json!({"is_error": false, "duration_ms": 1234, "cost_usd": 0.5, "input_tokens": 10, "output_tokens": 5, "model": "m"}));
        // 第二次：失败
        ev("task_started", json!({"prompt": "[导入评估:qa] Todo"}));
        ev("task_failed", json!({"error": "CLI 退出"}));
        // 第三次：没有结束标记
        ev("task_started", json!({"prompt": "再来一次"}));
        ev("tool_use", json!({"tool": "Grep", "input": {"pattern": "foo"}}));

        let runs = db.employee_log(&e.id, 10, true).unwrap();
        assert_eq!(runs.len(), 3);
        assert_eq!((runs[0].status.as_str(), runs[0].tool_count), ("running", 1), "最新一次、员工在忙 → 进行中");
        assert_eq!((runs[1].status.as_str(), runs[1].what.as_str(), runs[1].error.as_deref()), ("failed", "导入评估（qa）", Some("CLI 退出")));
        let done = &runs[2];
        assert_eq!((done.status.as_str(), done.subject.as_str(), done.duration_ms, done.tokens), ("done", "实现登录", Some(1234), Some(15)));
        assert_eq!(done.tools.iter().map(|t| t.summary.as_str()).collect::<Vec<_>>(), vec!["/a/b.rs", "cargo test"]);
        assert_eq!(done.last_text.as_deref(), Some("做完了"));
        assert_eq!(db.employee_log(&e.id, 10, false).unwrap()[0].status, "interrupted", "员工不忙时没有结束标记 → 被中断");
        assert_eq!(db.employee_log(&e.id, 2, false).unwrap().len(), 2, "只取最近 2 次");
    }
}

#[cfg(test)]
mod work_tab_tests {
    use super::work_tab;
    #[test]
    fn working_employees_link_to_the_page_where_the_work_is() {
        assert_eq!(work_tab("会议发言", true, false), "meetings");
        assert_eq!(work_tab("导入评估（qa）", false, false), "onboarding");
        assert_eq!(work_tab("起草计划", false, false), "plan");
        assert_eq!(work_tab("计划任务", false, true), "tasks");
        assert_eq!(work_tab("任务", false, false), "chat");
        assert_eq!(work_tab("负责人裁决", false, false), "issues");
        assert_eq!(work_tab("调整计划", false, false), "plan");
        assert_eq!(work_tab("整理行动项", false, false), "actions");
        assert_eq!(work_tab("整理记忆", false, false), "memory");
        assert_eq!(work_tab("撰写文档", false, false), "docs");
        assert_eq!(work_tab("纪要起草", false, false), "meetings");
        assert_eq!(work_tab("对齐会目标检查", false, false), "meetings");
    }
}

#[cfg(test)]
mod activity_link_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn system_started_work_still_knows_which_project_and_page_to_open() {
        let db = Db::open_memory().unwrap();
        let p1 = db.create_project("P1", "", "/tmp/link-p1").unwrap().id;
        let p2 = db.create_project("P2", "", "/tmp/link-p2").unwrap().id;
        let lead = db.add_employee("lead", "负责人", "x").unwrap();
        let solo = db.add_employee("solo", "工程师", "x").unwrap();
        db.add_member(&p1, &lead.id, "lead").unwrap();
        db.add_member(&p2, &lead.id, "developer").unwrap();
        db.add_member(&p1, &solo.id, "developer").unwrap();

        // 系统发起的调用不进群聊：事件没有项目，但任务开始时记了所属项目
        db.append_event("task_started", Some(&lead.id), None, &json!({"prompt": "[负责人裁决] Dashboard 空库语义", "project": p2, "origin": "system"})).unwrap();
        let a = &db.activities(&["lead".to_string()]).unwrap()[0];
        assert_eq!((a.project_id.as_deref(), a.tab.as_str(), a.what.as_str()), (Some(p2.as_str()), "issues", "负责人裁决"));

        // 旧事件没有记录项目：员工只在一个项目里时，就是那个项目
        db.append_event("task_started", Some(&solo.id), None, &json!({"prompt": "[整理记忆] P1"})).unwrap();
        let b = &db.activities(&["solo".to_string()]).unwrap()[0];
        assert_eq!((b.project_id.as_deref(), b.tab.as_str()), (Some(p1.as_str()), "memory"));

        // 在多个项目里又没有记录：不乱猜
        db.append_event("task_started", Some(&lead.id), None, &json!({"prompt": "[整理记忆] ?"})).unwrap();
        assert_eq!(db.activities(&["lead".to_string()]).unwrap()[0].project_id, None);
    }
}
