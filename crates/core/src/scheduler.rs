//! 额度感知的调度：所有模型调用（开发任务、会议发言、导入评估）都先在这里取得许可。
//!
//! 所有员工共享同一份订阅额度，所以并发不能只看固定上限，还要看额度还剩多少：
//! 按最紧张的那个窗口（5 小时 / 7 天）的使用率分级——正常 → 限速 → 暂停新任务，窗口重置后自动恢复。
//!
//! 额度读数只在有调用时才更新，所以必须处理“读数过期”：已知的重置时间到了就当窗口已重置；
//! 不知道何时重置的暂停，超过 30 分钟后先放行 1 个任务去确认当前额度，而不是永远卡住。

use crate::cost::QuotaNow;
use crate::db::{Db, now_ms};
use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

pub const MIGRATION_7: &str = r#"
CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

/// 读数超过这个时间、又不知道何时重置，就认为已经过期。
const STALE_MS: i64 = 30 * 60 * 1000;
const RECHECK: Duration = Duration::from_secs(5);
pub const MAX_CONCURRENCY: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SchedulerConfig {
    /// 关闭后只按固定并发上限调度，不看额度
    pub enabled: bool,
    /// 达到后并发减半
    pub soft: f64,
    /// 达到后并发降为 1
    pub hard: f64,
    /// 达到后暂停新任务
    pub stop: f64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self { enabled: true, soft: 0.70, hard: 0.85, stop: 0.95 }
    }
}

impl SchedulerConfig {
    pub fn validate(&self) -> Result<()> {
        if !(0.0 < self.soft && self.soft < self.hard && self.hard < self.stop && self.stop <= 1.0) {
            bail!("阈值需要满足 0 < 限速线 < 严格限速线 < 暂停线 ≤ 100%");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Normal,
    Throttled,
    Paused,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Decision {
    pub level: Level,
    pub effective_limit: usize,
    pub reason: String,
    /// 预计恢复时间（毫秒时间戳）
    pub resume_at: Option<i64>,
}

fn normal(limit: usize, reason: &str) -> Decision {
    Decision { level: Level::Normal, effective_limit: limit, reason: reason.into(), resume_at: None }
}

fn blocked_status(s: &str) -> bool {
    let s = s.to_lowercase();
    ["reject", "limit", "block", "exceed", "denied"].iter().any(|k| s.contains(k))
}

/// 纯函数：根据额度读数、当前时间和配置，决定现在最多能同时跑几个。
pub fn decide(q: Option<&QuotaNow>, now: i64, cfg: &SchedulerConfig, user_limit: usize) -> Decision {
    if !cfg.enabled {
        return normal(user_limit, "额度调度已关闭");
    }
    let Some(q) = q else { return normal(user_limit, "还没有额度数据") };
    let pct = |u: f64| format!("{:.0}%", u * 100.0);

    // 各窗口：已知的重置时间已过 → 窗口已重置，不再计入
    struct W { label: &'static str, util: f64, reset_ms: Option<i64> }
    let mut live: Vec<W> = Vec::new();
    for (label, util, reset) in [("5 小时", q.five_hour, q.five_resets_at), ("7 天", q.seven_day, q.seven_resets_at)] {
        let Some(util) = util else { continue };
        let reset_ms = reset.map(|r| r * 1000);
        if reset_ms.is_some_and(|r| now >= r) {
            continue;
        }
        live.push(W { label, util, reset_ms });
    }

    // 暂停：达到暂停线，或者 CLI 明确报告被限流
    let mut blockers: Vec<&W> = live.iter().filter(|w| w.util >= cfg.stop).collect();
    let blocked = blocked_status(&q.status);
    if blocked && blockers.is_empty() {
        blockers = live.iter().collect(); // 被限流但不知道是哪个窗口：都算
    }
    if blocked || !blockers.is_empty() {
        // 要所有拦路的窗口都恢复才行；不知道何时重置的，用“读数过期”兜底
        let resume = blockers.iter().map(|w| w.reset_ms.unwrap_or(q.ts + STALE_MS)).max().unwrap_or(q.ts + STALE_MS);
        let why = if blockers.is_empty() {
            "CLI 报告订阅额度已被限流".to_string()
        } else {
            blockers.iter().map(|w| format!("{}窗口已用 {}", w.label, pct(w.util))).collect::<Vec<_>>().join("，") + &format!("，超过暂停线 {}", pct(cfg.stop))
        };
        if now < resume {
            return Decision { level: Level::Paused, effective_limit: 0, reason: why, resume_at: Some(resume) };
        }
        // 读数过期且不知道真实情况：先放行 1 个，让它带回新的额度读数
        return Decision { level: Level::Throttled, effective_limit: 1, reason: format!("{why}，但读数已过期，先放行 1 个任务确认当前额度"), resume_at: None };
    }

    let worst = live.iter().max_by(|a, b| a.util.partial_cmp(&b.util).unwrap_or(std::cmp::Ordering::Equal));
    match worst {
        Some(w) if w.util >= cfg.hard => Decision {
            level: Level::Throttled, effective_limit: 1.min(user_limit),
            reason: format!("{}窗口已用 {}，达到严格限速线 {}，并发降为 1", w.label, pct(w.util), pct(cfg.hard)), resume_at: w.reset_ms,
        },
        Some(w) if w.util >= cfg.soft => {
            let half = user_limit.div_ceil(2).max(1);
            Decision {
                level: if half < user_limit { Level::Throttled } else { Level::Normal }, effective_limit: half.min(user_limit),
                reason: format!("{}窗口已用 {}，达到限速线 {}，并发减半", w.label, pct(w.util), pct(cfg.soft)), resume_at: w.reset_ms,
            }
        }
        _ => normal(user_limit, "额度充足"),
    }
}

struct St {
    cfg: SchedulerConfig,
    user_limit: usize,
    running: usize,
    waiting: VecDeque<u64>,
    next_ticket: u64,
    manual_pause: bool,
    override_until: i64,
    last_published: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SchedStatus {
    pub config: SchedulerConfig,
    pub user_limit: usize,
    pub effective_limit: usize,
    pub running: usize,
    pub waiting: usize,
    pub level: Level,
    pub reason: String,
    pub resume_at: Option<i64>,
    pub manual_pause: bool,
    /// 临时忽略额度限制，直到这个时间
    pub override_until: Option<i64>,
    pub quota: Option<QuotaNow>,
}

pub struct Scheduler {
    db: Arc<Db>,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    st: Mutex<St>,
    notify: Notify,
}

/// 持有期间占用一个并发名额，丢弃时归还。
pub struct Permit {
    sched: Arc<Scheduler>,
}
impl Drop for Permit {
    fn drop(&mut self) {
        self.sched.st.lock().unwrap().running -= 1;
        self.sched.notify.notify_waiters();
    }
}

/// 等待中的任务被取消时，把它从队列里摘掉，不能占着位置。
struct TicketGuard {
    sched: Arc<Scheduler>,
    ticket: u64,
    armed: bool,
}
impl Drop for TicketGuard {
    fn drop(&mut self) {
        if self.armed {
            self.sched.st.lock().unwrap().waiting.retain(|t| *t != self.ticket);
            self.sched.notify.notify_waiters();
        }
    }
}

impl Db {
    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.lock().unwrap().query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0)).optional()?)
    }
    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value", params![key, value])?;
        Ok(())
    }
}

impl Scheduler {
    /// 从设置里读取配置和并发上限；没有设置过就用传入的默认并发上限。
    pub fn new(db: Arc<Db>, default_limit: usize) -> Arc<Self> {
        Self::with_clock(db, default_limit, Arc::new(now_ms))
    }

    pub fn with_clock(db: Arc<Db>, default_limit: usize, clock: Arc<dyn Fn() -> i64 + Send + Sync>) -> Arc<Self> {
        let cfg = db.get_setting("scheduler").ok().flatten().and_then(|s| serde_json::from_str::<SchedulerConfig>(&s).ok()).filter(|c| c.validate().is_ok()).unwrap_or_default();
        let limit = db.get_setting("concurrency").ok().flatten().and_then(|s| s.parse::<usize>().ok()).unwrap_or(default_limit).clamp(1, MAX_CONCURRENCY);
        Arc::new(Self {
            db, clock,
            st: Mutex::new(St { cfg, user_limit: limit, running: 0, waiting: VecDeque::new(), next_ticket: 0, manual_pause: false, override_until: 0, last_published: None }),
            notify: Notify::new(),
        })
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    pub fn decision(&self) -> Decision {
        let (cfg, limit, manual, over) = {
            let s = self.st.lock().unwrap();
            (s.cfg.clone(), s.user_limit, s.manual_pause, s.override_until)
        };
        let now = self.now();
        if manual {
            return Decision { level: Level::Paused, effective_limit: 0, reason: "你手动暂停了调度，新任务会排队等待".into(), resume_at: None };
        }
        if now < over {
            return normal(limit, "你选择了临时忽略额度限制");
        }
        decide(self.db.latest_quota().ok().flatten().as_ref(), now, &cfg, limit)
    }

    pub fn status(&self) -> SchedStatus {
        let d = self.decision();
        let s = self.st.lock().unwrap();
        SchedStatus {
            config: s.cfg.clone(), user_limit: s.user_limit, effective_limit: d.effective_limit, running: s.running, waiting: s.waiting.len(),
            level: d.level, reason: d.reason, resume_at: d.resume_at, manual_pause: s.manual_pause,
            override_until: (s.override_until > self.now()).then_some(s.override_until), quota: self.db.latest_quota().ok().flatten(),
        }
    }

    /// 现在新来一个任务，能不能立刻开始（没有人在排队、有空位、没有暂停）。
    pub fn would_start_now(&self) -> bool {
        let d = self.decision();
        let s = self.st.lock().unwrap();
        s.waiting.is_empty() && d.level != Level::Paused && s.running < d.effective_limit
    }

    /// 取得一个并发名额，先来先服务；必要时一直等到额度允许。可以被取消（丢弃 future）。
    pub async fn acquire(self: &Arc<Self>) -> Permit {
        let ticket = {
            let mut s = self.st.lock().unwrap();
            let t = s.next_ticket;
            s.next_ticket += 1;
            s.waiting.push_back(t);
            t
        };
        let mut guard = TicketGuard { sched: self.clone(), ticket, armed: true };
        loop {
            // 先登记通知再检查条件，避免错过检查与等待之间发生的变化
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let d = self.decision();
            {
                let mut s = self.st.lock().unwrap();
                if s.waiting.front() == Some(&ticket) && d.level != Level::Paused && s.running < d.effective_limit {
                    s.waiting.pop_front();
                    s.running += 1;
                    guard.armed = false;
                    drop(s);
                    self.notify.notify_waiters(); // 轮到下一个排队者检查
                    return Permit { sched: self.clone() };
                }
            }
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep(RECHECK) => {}
            }
        }
    }

    /// 交互式场景（会议发言、导入评估）：暂停时直接说明原因，而不是悄悄排几个小时的队。
    pub async fn acquire_interactive(self: &Arc<Self>) -> Result<Permit, String> {
        let d = self.decision();
        if d.level == Level::Paused {
            return Err(format!("现在不能调用模型：{}。{}", d.reason, if d.resume_at.is_some() { "额度窗口重置后会自动恢复。" } else { "请在设置里取消暂停后再试。" }));
        }
        Ok(self.acquire().await)
    }

    pub fn set_user_limit(&self, n: usize) -> Result<usize> {
        let n = n.clamp(1, MAX_CONCURRENCY);
        self.db.set_setting("concurrency", &n.to_string())?;
        self.st.lock().unwrap().user_limit = n;
        self.notify.notify_waiters();
        Ok(n)
    }

    /// 只对当前进程生效、不写入设置（命令行的 --max）。
    pub fn set_user_limit_transient(&self, n: usize) {
        self.st.lock().unwrap().user_limit = n.clamp(1, MAX_CONCURRENCY);
        self.notify.notify_waiters();
    }

    pub fn set_config(&self, cfg: SchedulerConfig) -> Result<()> {
        cfg.validate()?;
        self.db.set_setting("scheduler", &serde_json::to_string(&cfg)?)?;
        self.st.lock().unwrap().cfg = cfg;
        self.notify.notify_waiters();
        Ok(())
    }

    pub fn set_manual_pause(&self, paused: bool) {
        self.st.lock().unwrap().manual_pause = paused;
        self.notify.notify_waiters();
    }

    /// 临时忽略额度限制 `minutes` 分钟（0 表示取消）。不影响手动暂停。
    pub fn set_override(&self, minutes: u32) {
        self.st.lock().unwrap().override_until = if minutes == 0 { 0 } else { self.now() + minutes as i64 * 60_000 };
        self.notify.notify_waiters();
    }

    /// 有新的额度读数时调用，让排队的任务立刻重新评估。
    pub fn nudge(&self) {
        self.notify.notify_waiters();
    }

    /// 调度状态变化时写入一条事件，界面据此更新（只在变化时写，避免刷屏）。
    pub fn publish_if_changed(&self) {
        let s = self.status();
        let key = format!("{:?}|{}|{}|{}|{}", s.level, s.effective_limit, s.reason, s.manual_pause, s.override_until.is_some());
        {
            let mut st = self.st.lock().unwrap();
            if st.last_published.as_deref() == Some(&key) {
                return;
            }
            st.last_published = Some(key);
        }
        let _ = self.db.append_event("scheduler_state", None, None, &json!({
            "level": s.level, "effective_limit": s.effective_limit, "user_limit": s.user_limit, "reason": s.reason, "resume_at": s.resume_at,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};

    fn q(five: f64, seven: f64) -> QuotaNow {
        QuotaNow { ts: 1_000_000, status: "allowed".into(), five_hour: Some(five), seven_day: Some(seven), five_resets_at: None, seven_resets_at: None }
    }
    fn cfg() -> SchedulerConfig { SchedulerConfig::default() }

    #[test]
    fn levels_follow_the_tightest_window() {
        let now = 1_000_000;
        let d = |a, b, lim| decide(Some(&q(a, b)), now, &cfg(), lim);
        assert_eq!((d(0.3, 0.2, 4).level, d(0.3, 0.2, 4).effective_limit), (Level::Normal, 4));
        assert_eq!((d(0.75, 0.2, 4).level, d(0.75, 0.2, 4).effective_limit), (Level::Throttled, 2), "限速线：减半");
        assert_eq!(d(0.75, 0.2, 3).effective_limit, 2, "向上取整：3 的一半是 2");
        assert_eq!((d(0.75, 0.2, 1).level, d(0.75, 0.2, 1).effective_limit), (Level::Normal, 1), "上限本来就是 1，无需再限速");
        assert_eq!(d(0.2, 0.9, 4).effective_limit, 1, "7 天窗口紧张同样生效");
        assert!(d(0.2, 0.9, 4).reason.contains("7 天"));
        assert_eq!((d(0.97, 0.2, 4).level, d(0.97, 0.2, 4).effective_limit), (Level::Paused, 0));
        assert_eq!(d(0.85, 0.2, 4).effective_limit, 1, "刚好等于严格限速线");
        assert_eq!(d(0.95, 0.2, 4).level, Level::Paused, "刚好等于暂停线");
    }

    #[test]
    fn missing_disabled_and_reset_windows_do_not_throttle() {
        assert_eq!(decide(None, 0, &cfg(), 3).level, Level::Normal);
        let off = SchedulerConfig { enabled: false, ..cfg() };
        assert_eq!(decide(Some(&q(0.99, 0.99)), 1_000_000, &off, 3).effective_limit, 3);
        // 5 小时窗口的重置时间已过：它不再计入，只看 7 天
        let mut r = q(0.97, 0.3);
        r.five_resets_at = Some(2_000);
        let d = decide(Some(&r), 2_000_001, &cfg(), 3);
        assert_eq!((d.level, d.effective_limit), (Level::Normal, 3));
        // 没到重置时间：仍然暂停，并给出恢复时间
        let d = decide(Some(&r), 1_999_000, &cfg(), 3);
        assert_eq!((d.level, d.resume_at), (Level::Paused, Some(2_000_000)));
    }

    #[test]
    fn pause_waits_for_every_blocking_window_and_falls_back_to_a_probe_when_reset_is_unknown() {
        let mut r = q(0.97, 0.96);
        r.five_resets_at = Some(2_000);
        r.seven_resets_at = Some(9_000);
        let d = decide(Some(&r), 3_000_000, &cfg(), 3);
        assert_eq!((d.level, d.resume_at), (Level::Paused, Some(9_000_000)), "两个窗口都要恢复，取较晚的");
        assert!(d.reason.contains("5 小时") || d.reason.contains("7 天"));
        // 不知道何时重置：读数 30 分钟内保持暂停
        let r = q(0.97, 0.2);
        assert_eq!(decide(Some(&r), r.ts + 10 * 60_000, &cfg(), 3).level, Level::Paused);
        // 过期后不会永远卡住：先放行 1 个任务去确认当前额度
        let d = decide(Some(&r), r.ts + 31 * 60_000, &cfg(), 3);
        assert_eq!((d.level, d.effective_limit), (Level::Throttled, 1));
        assert!(d.reason.contains("读数已过期"));
        // CLI 明确报告限流，也要暂停
        let mut r = q(0.5, 0.5);
        r.status = "rejected".into();
        assert_eq!(decide(Some(&r), r.ts + 1000, &cfg(), 3).level, Level::Paused);
        let mut r = q(0.5, 0.5);
        r.status = "allowed_warning".into();
        assert_eq!(decide(Some(&r), r.ts, &cfg(), 3).level, Level::Normal, "接近上限的提示本身不暂停");
    }

    #[test]
    fn config_validation_and_persistence() {
        assert!(cfg().validate().is_ok());
        for bad in [(0.9, 0.8, 0.95), (0.7, 0.85, 1.2), (0.0, 0.5, 0.9), (0.7, 0.7, 0.9)] {
            assert!(SchedulerConfig { enabled: true, soft: bad.0, hard: bad.1, stop: bad.2 }.validate().is_err(), "{bad:?}");
        }
        let db = Arc::new(Db::open_memory().unwrap());
        let s = Scheduler::new(db.clone(), 3);
        assert_eq!(s.status().user_limit, 3);
        assert_eq!(s.set_user_limit(99).unwrap(), MAX_CONCURRENCY, "上限被截断");
        assert!(s.set_config(SchedulerConfig { soft: 0.9, ..cfg() }).is_err());
        s.set_config(SchedulerConfig { soft: 0.5, hard: 0.6, stop: 0.7, enabled: true }).unwrap();
        // 重新创建调度器，设置还在
        let s2 = Scheduler::new(db, 3);
        assert_eq!((s2.status().user_limit, s2.status().config.stop), (MAX_CONCURRENCY, 0.7));
    }

    fn quota_event(db: &Db, five: f64, seven: f64, five_reset: Option<i64>) {
        db.append_event("rate_limit", None, None, &json!({"status": "allowed", "five_hour": five, "seven_day": seven, "five_resets_at": five_reset, "seven_resets_at": null})).unwrap();
    }
    fn setup(limit: usize) -> (Arc<Db>, Arc<Scheduler>, Arc<AtomicI64>) {
        let db = Arc::new(Db::open_memory().unwrap());
        let clock = Arc::new(AtomicI64::new(1_000_000));
        let c = clock.clone();
        (db.clone(), Scheduler::with_clock(db, limit, Arc::new(move || c.load(Ordering::SeqCst))), clock)
    }
    async fn settle() { tokio::time::sleep(Duration::from_millis(50)).await; }

    #[tokio::test(start_paused = true)]
    async fn permits_are_limited_fifo_and_released_on_drop() {
        let (_db, s, _c) = setup(2);
        let a = s.acquire().await;
        let _b = s.acquire().await;
        assert_eq!((s.status().running, s.status().waiting), (2, 0));
        assert!(!s.would_start_now());
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut hs = vec![];
        for i in 0..3 {
            let (s2, o) = (s.clone(), order.clone());
            hs.push(tokio::spawn(async move { let p = s2.acquire().await; o.lock().unwrap().push(i); p }));
            settle().await; // 保证入队顺序
        }
        assert_eq!(s.status().waiting, 3);
        drop(a);
        settle().await;
        assert_eq!(*order.lock().unwrap(), vec![0], "先来先服务");
        assert_eq!((s.status().running, s.status().waiting), (2, 2));
        let p0 = hs.remove(0).await.unwrap();
        drop(p0);
        settle().await;
        assert_eq!(*order.lock().unwrap(), vec![0, 1]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_waiter_does_not_hold_its_place() {
        let (_db, s, _c) = setup(1);
        let held = s.acquire().await;
        let s2 = s.clone();
        let waiter = tokio::spawn(async move { s2.acquire().await });
        settle().await;
        assert_eq!(s.status().waiting, 1);
        waiter.abort();
        settle().await;
        assert_eq!(s.status().waiting, 0, "被取消的任务从队列里摘掉");
        let s3 = s.clone();
        let next = tokio::spawn(async move { s3.acquire().await });
        drop(held);
        settle().await;
        assert!(next.is_finished(), "后来的任务不会被已取消的卡住");
    }

    #[tokio::test(start_paused = true)]
    async fn throttling_reacts_to_new_quota_readings() {
        let (db, s, _c) = setup(3);
        quota_event(&db, 0.9, 0.2, None); // 严格限速：并发 1
        assert_eq!(s.status().effective_limit, 1);
        let first = s.acquire().await;
        let s2 = s.clone();
        let second = tokio::spawn(async move { s2.acquire().await });
        settle().await;
        assert!(!second.is_finished(), "额度紧张：第二个要等");
        quota_event(&db, 0.3, 0.2, None); // 额度恢复
        s.nudge();
        settle().await;
        assert!(second.is_finished(), "新读数一到，排队的立刻放行");
        drop(first);
    }

    #[tokio::test(start_paused = true)]
    async fn paused_work_resumes_automatically_when_the_window_resets() {
        let (db, s, clock) = setup(2);
        quota_event(&db, 0.97, 0.2, Some(2_000)); // 重置时间 = 2,000,000 ms
        assert_eq!(s.decision().level, Level::Paused);
        let s2 = s.clone();
        let h = tokio::spawn(async move { s2.acquire().await });
        settle().await;
        assert!(!h.is_finished());
        tokio::time::sleep(Duration::from_secs(60)).await; // 时间流逝但时钟没到重置点
        assert!(!h.is_finished(), "没到重置时间不能开始");
        clock.store(2_000_001, Ordering::SeqCst);
        tokio::time::sleep(RECHECK + Duration::from_secs(1)).await; // 不需要任何人通知，定时复查就会发现
        assert!(h.is_finished(), "窗口重置后自动恢复");
    }

    #[tokio::test(start_paused = true)]
    async fn manual_pause_and_override_behave_as_the_user_expects() {
        let (db, s, clock) = setup(2);
        s.set_manual_pause(true);
        let st = s.status();
        assert_eq!((st.level, st.effective_limit, st.manual_pause), (Level::Paused, 0, true));
        let s2 = s.clone();
        let h = tokio::spawn(async move { s2.acquire().await });
        settle().await;
        assert!(!h.is_finished());
        s.set_manual_pause(false);
        settle().await;
        assert!(h.is_finished(), "取消手动暂停后立刻开始");
        drop(h.await.unwrap());

        // 额度暂停时，用户选择临时忽略 30 分钟
        quota_event(&db, 0.99, 0.2, Some(9_999_999));
        assert_eq!(s.decision().level, Level::Paused);
        s.set_override(30);
        assert_eq!((s.decision().level, s.decision().effective_limit), (Level::Normal, 2));
        assert!(s.status().override_until.is_some());
        clock.fetch_add(31 * 60_000, Ordering::SeqCst);
        assert_eq!(s.decision().level, Level::Paused, "到期后恢复额度限制");
        // 手动暂停优先于临时忽略
        s.set_override(30);
        s.set_manual_pause(true);
        assert_eq!(s.decision().level, Level::Paused);
    }

    #[tokio::test(start_paused = true)]
    async fn interactive_requests_are_refused_while_paused_but_queue_when_only_throttled() {
        let (db, s, _c) = setup(1);
        quota_event(&db, 0.97, 0.2, Some(9_999_999));
        let err = s.acquire_interactive().await.err().unwrap();
        assert!(err.contains("5 小时窗口已用 97%") && err.contains("自动恢复"), "{err}");
        assert_eq!(s.status().waiting, 0, "被拒绝的请求不占队列");
        s.set_manual_pause(true);
        assert!(s.acquire_interactive().await.err().unwrap().contains("取消暂停"));
        s.set_manual_pause(false);
        quota_event(&db, 0.9, 0.2, None);
        let held = s.acquire_interactive().await.unwrap();
        assert_eq!(s.status().running, 1);
        drop(held);
    }

    #[tokio::test(start_paused = true)]
    async fn state_changes_are_published_once() {
        let (db, s, _c) = setup(2);
        let count = |db: &Db| db.recent_events(100).unwrap().iter().filter(|e| e.kind == "scheduler_state").count();
        s.publish_if_changed();
        s.publish_if_changed();
        assert_eq!(count(&db), 1, "没有变化就不重复写");
        quota_event(&db, 0.9, 0.2, None);
        s.publish_if_changed();
        assert_eq!(count(&db), 2);
        let e = db.recent_events(100).unwrap().into_iter().filter(|e| e.kind == "scheduler_state").last().unwrap();
        assert_eq!((e.payload["level"].as_str(), e.payload["effective_limit"].as_u64()), (Some("throttled"), Some(1)));
    }
}
