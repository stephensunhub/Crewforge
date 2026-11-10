//! LLM 端点：系统级配置“员工背后连哪个大模型服务”。
//!
//! 默认（员工没有选端点）就是本机 Claude Code 自己的订阅登录，什么都不注入。
//! 选了端点（Ollama 等提供 Anthropic 兼容接口的服务）时，只给这个员工的 CLI 子进程注入
//! `ANTHROPIC_*` 环境变量，不碰本机的 shell、`~/.claude` 登录状态和其他项目；
//! 启动参数里已有 `--setting-sources ""`，用户级 settings.json 里的 env 也不会覆盖它。
//!
//! 令牌存系统钥匙串（macOS），数据库里只有钥匙串条目的名字，不存明文。

use crate::db::{Db, now_ms};
use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub const MIGRATION_19: &str = r#"
CREATE TABLE llm_endpoints (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL UNIQUE,
    base_url        TEXT NOT NULL,
    models          TEXT NOT NULL DEFAULT '[]',   -- JSON：该端点上可用的模型名，第一个是默认
    extra_env       TEXT NOT NULL DEFAULT '{}',   -- JSON：额外环境变量
    max_concurrency INTEGER NOT NULL DEFAULT 1,   -- 同时最多几个员工在用（本地模型通常只能串行）
    context_tokens  INTEGER NOT NULL DEFAULT 0,   -- 模型上下文窗口，0 = 不限制；用来决定何时换新会话
    has_token       INTEGER NOT NULL DEFAULT 0,
    enabled         INTEGER NOT NULL DEFAULT 1,
    created_at      INTEGER NOT NULL
);
ALTER TABLE employees ADD COLUMN endpoint_id TEXT;
"#;

#[derive(Debug, Clone, Serialize)]
pub struct Endpoint {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub models: Vec<String>,
    pub extra_env: BTreeMap<String, String>,
    pub max_concurrency: i64,
    pub context_tokens: i64,
    pub has_token: bool,
    pub enabled: bool,
    /// 有几位员工在用
    pub employees: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EndpointInput {
    pub name: String,
    pub base_url: String,
    pub models: Vec<String>,
    #[serde(default)]
    pub extra_env: BTreeMap<String, String>,
    pub max_concurrency: i64,
    #[serde(default)]
    pub context_tokens: i64,
    /// None：不改令牌；Some("")：清除；Some(x)：设置
    #[serde(default)]
    pub token: Option<String>,
}

// ---------- 钥匙串 ----------
const KEYCHAIN_SERVICE: &str = "Crewforge LLM endpoint";

fn secret_set(id: &str, token: &str) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("目前只支持在 macOS 钥匙串里保存令牌");
    }
    let out = std::process::Command::new("security")
        .args(["add-generic-password", "-U", "-s", KEYCHAIN_SERVICE, "-a", id, "-w", token])
        .output().context("无法运行 security")?;
    if !out.status.success() {
        bail!("写入钥匙串失败：{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

fn secret_get(id: &str) -> Option<String> {
    let out = std::process::Command::new("security").args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-a", id, "-w"]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|t| !t.is_empty())
}

fn secret_delete(id: &str) {
    let _ = std::process::Command::new("security").args(["delete-generic-password", "-s", KEYCHAIN_SERVICE, "-a", id]).output();
}

/// 把 `http://host:11434/` 规整成不带结尾斜杠的形式；只接受 http(s)。
pub fn normalize_base_url(u: &str) -> Result<String> {
    let u = u.trim().trim_end_matches('/');
    if !(u.starts_with("http://") || u.starts_with("https://")) || u.len() < 9 || u.contains(char::is_whitespace) {
        bail!("地址要以 http:// 或 https:// 开头，例如 http://192.168.1.20:11434");
    }
    Ok(u.to_string())
}

/// 要注入给 CLI 的环境变量。`model` 是这次实际用的模型；三个档位别名都指向它，
/// 否则 Claude Code 的后台小任务会请求一个服务上不存在的 haiku 模型而失败。
pub fn build_env(ep: &Endpoint, token: Option<&str>, model: &str) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = vec![
        ("ANTHROPIC_BASE_URL".into(), ep.base_url.clone()),
        ("ANTHROPIC_AUTH_TOKEN".into(), token.unwrap_or("ollama").to_string()),
        ("ANTHROPIC_API_KEY".into(), String::new()),
        ("ANTHROPIC_MODEL".into(), model.into()),
        ("ANTHROPIC_DEFAULT_OPUS_MODEL".into(), model.into()),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL".into(), model.into()),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL".into(), model.into()),
        ("ANTHROPIC_SMALL_FAST_MODEL".into(), model.into()),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(), "1".into()),
    ];
    for (k, v) in &ep.extra_env {
        env.retain(|(ek, _)| ek != k);
        env.push((k.clone(), v.clone()));
    }
    env
}

/// 解析一次执行要用的端点：返回 (环境变量, 实际模型, 上下文窗口)。
pub struct Resolved {
    pub endpoint: Endpoint,
    pub env: Vec<(String, String)>,
    pub model: String,
}

// ---------- 并发控制 ----------
fn semaphores() -> &'static Mutex<HashMap<String, (i64, Arc<Semaphore>)>> {
    static S: OnceLock<Mutex<HashMap<String, (i64, Arc<Semaphore>)>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// 等一个名额：同一个端点同时最多 `max_concurrency` 个员工在用。返回的许可放手即归还。
pub async fn acquire_slot(ep: &Endpoint) -> OwnedSemaphorePermit {
    let sem = {
        let mut m = semaphores().lock().unwrap();
        let entry = m.entry(ep.id.clone()).or_insert_with(|| (ep.max_concurrency, Arc::new(Semaphore::new(ep.max_concurrency.max(1) as usize))));
        if entry.0 != ep.max_concurrency {
            *entry = (ep.max_concurrency, Arc::new(Semaphore::new(ep.max_concurrency.max(1) as usize)));
        }
        entry.1.clone()
    };
    sem.acquire_owned().await.expect("semaphore never closed")
}

fn row_endpoint(r: &rusqlite::Row<'_>) -> rusqlite::Result<Endpoint> {
    let models: String = r.get(3)?;
    let env: String = r.get(4)?;
    Ok(Endpoint {
        id: r.get(0)?, name: r.get(1)?, base_url: r.get(2)?,
        models: serde_json::from_str(&models).unwrap_or_default(),
        extra_env: serde_json::from_str(&env).unwrap_or_default(),
        max_concurrency: r.get(5)?, context_tokens: r.get(6)?,
        has_token: r.get::<_, i64>(7)? != 0, enabled: r.get::<_, i64>(8)? != 0, employees: r.get::<_, i64>(9)? as usize,
    })
}

const SELECT: &str = "SELECT e.id, e.name, e.base_url, e.models, e.extra_env, e.max_concurrency, e.context_tokens, e.has_token, e.enabled, \
    (SELECT COUNT(*) FROM employees m WHERE m.endpoint_id = e.id AND m.archived = 0) FROM llm_endpoints e";

fn validate(i: &EndpointInput) -> Result<(String, Vec<String>)> {
    let name = i.name.trim();
    if name.is_empty() {
        bail!("名称不能为空");
    }
    let url = normalize_base_url(&i.base_url)?;
    let models: Vec<String> = i.models.iter().map(|m| m.trim().to_string()).filter(|m| !m.is_empty()).collect();
    if models.is_empty() {
        bail!("至少填一个模型名（用 `ollama list` 看实际标签）");
    }
    if models.iter().any(|m| m.contains(char::is_whitespace)) {
        bail!("模型名不能含空格");
    }
    if !(1..=16).contains(&i.max_concurrency) {
        bail!("最大并发数要在 1 到 16 之间");
    }
    if i.context_tokens < 0 {
        bail!("上下文窗口不能为负");
    }
    for k in i.extra_env.keys() {
        if k.is_empty() || k.contains(['=', ' ']) {
            bail!("环境变量名不合法：{k}");
        }
    }
    Ok((url, models))
}

impl Db {
    pub fn list_endpoints(&self) -> Result<Vec<Endpoint>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!("{SELECT} ORDER BY e.created_at"))?;
        let rows = stmt.query_map([], row_endpoint)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn endpoint(&self, id: &str) -> Result<Option<Endpoint>> {
        Ok(self.conn.lock().unwrap().query_row(&format!("{SELECT} WHERE e.id = ?1"), [id], row_endpoint).optional()?)
    }

    pub fn add_endpoint(&self, i: &EndpointInput) -> Result<Endpoint> {
        let (url, models) = validate(i)?;
        let id = uuid::Uuid::new_v4().to_string();
        let token = i.token.as_deref().map(str::trim).filter(|t| !t.is_empty());
        {
            let conn = self.conn.lock().unwrap();
            let dup: i64 = conn.query_row("SELECT COUNT(*) FROM llm_endpoints WHERE name = ?1", [i.name.trim()], |r| r.get(0))?;
            if dup > 0 {
                bail!("已经有叫 {} 的端点了", i.name.trim());
            }
            conn.execute(
                "INSERT INTO llm_endpoints (id, name, base_url, models, extra_env, max_concurrency, context_tokens, has_token, created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![id, i.name.trim(), url, serde_json::to_string(&models)?, serde_json::to_string(&i.extra_env)?, i.max_concurrency, i.context_tokens, token.is_some() as i64, now_ms()])?;
        }
        if let Some(t) = token {
            if let Err(e) = secret_set(&id, t) {
                self.conn.lock().unwrap().execute("DELETE FROM llm_endpoints WHERE id = ?1", [&id])?;
                return Err(e);
            }
        }
        self.append_event("endpoint_created", None, None, &serde_json::json!({"name": i.name.trim(), "base_url": url}))?;
        Ok(self.endpoint(&id)?.expect("just inserted"))
    }

    pub fn update_endpoint(&self, id: &str, i: &EndpointInput) -> Result<()> {
        let (url, models) = validate(i)?;
        let old = self.endpoint(id)?.context("端点不存在")?;
        let mut has_token = old.has_token;
        match i.token.as_deref().map(str::trim) {
            None => {}
            Some("") => { secret_delete(id); has_token = false; }
            Some(t) => { secret_set(id, t)?; has_token = true; }
        }
        let conn = self.conn.lock().unwrap();
        let dup: i64 = conn.query_row("SELECT COUNT(*) FROM llm_endpoints WHERE name = ?1 AND id != ?2", params![i.name.trim(), id], |r| r.get(0))?;
        if dup > 0 {
            bail!("已经有叫 {} 的端点了", i.name.trim());
        }
        conn.execute(
            "UPDATE llm_endpoints SET name=?2, base_url=?3, models=?4, extra_env=?5, max_concurrency=?6, context_tokens=?7, has_token=?8 WHERE id=?1",
            params![id, i.name.trim(), url, serde_json::to_string(&models)?, serde_json::to_string(&i.extra_env)?, i.max_concurrency, i.context_tokens, has_token as i64])?;
        Ok(())
    }

    pub fn set_endpoint_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        let n = self.conn.lock().unwrap().execute("UPDATE llm_endpoints SET enabled = ?2 WHERE id = ?1", params![id, enabled as i64])?;
        if n == 0 {
            bail!("端点不存在");
        }
        Ok(())
    }

    /// 取端点的令牌（钥匙串）。
    pub fn endpoint_token(&self, id: &str) -> Option<String> {
        secret_get(id)
    }

    /// 还有员工在用的端点不能删。
    pub fn remove_endpoint(&self, id: &str) -> Result<()> {
        let ep = self.endpoint(id)?.context("端点不存在")?;
        if ep.employees > 0 {
            bail!("还有 {} 位员工在用这个端点，先给他们换一个", ep.employees);
        }
        self.conn.lock().unwrap().execute("DELETE FROM llm_endpoints WHERE id = ?1", [id])?;
        if ep.has_token {
            secret_delete(id);
        }
        Ok(())
    }

    /// 给员工选端点（None = 本机 Claude 订阅）。Copilot 员工不能用端点；换端点会清掉会话，
    /// 因为旧会话的上下文是另一个模型产生的。
    pub fn set_employee_endpoint(&self, employee_id: &str, endpoint_id: Option<&str>, model: &str) -> Result<()> {
        let emp = self.employee_by_id(employee_id)?.context("员工不存在")?;
        let model = model.trim();
        if let Some(eid) = endpoint_id {
            if emp.provider != "claude" {
                bail!("只有 Claude Code 员工能连自定义端点；Copilot 用自己的后端");
            }
            let ep = self.endpoint(eid)?.context("端点不存在")?;
            if !ep.enabled && emp.endpoint_id.as_deref() != Some(eid) {
                bail!("端点 {} 已停用", ep.name);
            }
            if !model.is_empty() && !ep.models.iter().any(|m| m == model) {
                bail!("模型 {model} 不在端点 {} 的模型清单里（{}）", ep.name, ep.models.join("、"));
            }
        }
        let changed = emp.endpoint_id.as_deref() != endpoint_id;
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE employees SET endpoint_id = ?2, model = ?3 WHERE id = ?1", params![employee_id, endpoint_id, model])?;
        if changed {
            conn.execute("DELETE FROM sessions WHERE employee_id = ?1", [employee_id])?;
        }
        drop(conn);
        if changed || emp.model != model {
            self.append_event("employee_updated", Some(employee_id), None, &serde_json::json!({"endpoint": [emp.endpoint_id, endpoint_id], "model": [emp.model, model], "sessions_reset": changed}))?;
        }
        Ok(())
    }

    /// 员工这次执行要用的端点；员工没选端点返回 None。
    pub fn resolve_endpoint(&self, emp: &crate::db::Employee) -> Result<Option<Resolved>> {
        let Some(eid) = emp.endpoint_id.as_deref() else { return Ok(None) };
        let ep = self.endpoint(eid)?.with_context(|| format!("{} 选的 LLM 端点已不存在，请到“模型”页重新选择", emp.name))?;
        if !ep.enabled {
            bail!("LLM 端点 {} 已停用，请到“模型”页启用，或给 {} 换一个", ep.name, emp.name);
        }
        let model = if emp.model.trim().is_empty() { ep.models[0].clone() } else { emp.model.trim().to_string() };
        let token = ep.has_token.then(|| secret_get(&ep.id)).flatten();
        let env = build_env(&ep, token.as_deref(), &model);
        Ok(Some(Resolved { endpoint: ep, env, model }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(name: &str) -> EndpointInput {
        EndpointInput { name: name.into(), base_url: "http://192.168.1.20:11434/".into(), models: vec!["qwen3:27b".into()], extra_env: BTreeMap::new(), max_concurrency: 1, context_tokens: 32768, token: None }
    }

    #[test]
    fn url_validation_and_normalizing() {
        assert_eq!(normalize_base_url(" http://a:11434/ ").unwrap(), "http://a:11434");
        assert!(normalize_base_url("a:11434").is_err() && normalize_base_url("ftp://x.y.z").is_err());
    }

    #[test]
    fn env_points_every_tier_at_the_endpoint_model_and_extra_env_wins() {
        let db = Db::open_memory().unwrap();
        let mut i = input("本地");
        i.extra_env.insert("API_TIMEOUT_MS".into(), "600000".into());
        i.extra_env.insert("ANTHROPIC_AUTH_TOKEN".into(), "mine".into());
        let ep = db.add_endpoint(&i).unwrap();
        assert_eq!(ep.base_url, "http://192.168.1.20:11434");
        let env: HashMap<_, _> = build_env(&ep, None, "qwen3:27b").into_iter().collect();
        assert_eq!(env["ANTHROPIC_BASE_URL"], "http://192.168.1.20:11434");
        assert_eq!(env["ANTHROPIC_API_KEY"], "");
        for k in ["ANTHROPIC_MODEL", "ANTHROPIC_DEFAULT_OPUS_MODEL", "ANTHROPIC_DEFAULT_SONNET_MODEL", "ANTHROPIC_DEFAULT_HAIKU_MODEL"] {
            assert_eq!(env[k], "qwen3:27b", "{k}");
        }
        assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "mine", "额外环境变量优先");
        assert_eq!(env["API_TIMEOUT_MS"], "600000");
    }

    #[test]
    fn endpoint_crud_validation_and_employee_binding() {
        let db = Db::open_memory().unwrap();
        let mut bad = input("x");
        bad.models = vec![];
        assert!(db.add_endpoint(&bad).is_err(), "没有模型");
        bad = input("x");
        bad.max_concurrency = 0;
        assert!(db.add_endpoint(&bad).is_err());
        let ep = db.add_endpoint(&input("本地 Ollama")).unwrap();
        assert!(db.add_endpoint(&input("本地 Ollama")).is_err(), "重名");

        let dev = db.add_employee("dev", "工程师", "p").unwrap();
        assert!(db.resolve_endpoint(&dev).unwrap().is_none(), "默认走订阅，不注入任何东西");

        assert!(db.set_employee_endpoint(&dev.id, Some(&ep.id), "other-model").is_err(), "模型不在清单里");
        db.get_or_create_session(&dev.id, "/p").unwrap();
        db.set_employee_endpoint(&dev.id, Some(&ep.id), "").unwrap();
        assert!(db.list_sessions().unwrap().is_empty(), "换端点清掉旧会话");
        let dev = db.employee_by_id(&dev.id).unwrap().unwrap();
        let r = db.resolve_endpoint(&dev).unwrap().unwrap();
        assert_eq!(r.model, "qwen3:27b", "没指定模型用端点的第一个");
        assert!(r.env.iter().any(|(k, v)| k == "ANTHROPIC_BASE_URL" && v.starts_with("http://192")));
        assert_eq!(db.endpoint(&ep.id).unwrap().unwrap().employees, 1);

        assert!(db.remove_endpoint(&ep.id).is_err(), "有人在用不能删");
        db.set_endpoint_enabled(&ep.id, false).unwrap();
        assert!(db.resolve_endpoint(&dev).is_err(), "停用后执行被拒绝并说明原因");
        db.set_employee_endpoint(&dev.id, None, "").unwrap();
        db.remove_endpoint(&ep.id).unwrap();

        let cp = db.add_employee_with("cp", "工程师", "p", "copilot").unwrap();
        let ep2 = db.add_endpoint(&input("另一个")).unwrap();
        assert!(db.set_employee_endpoint(&cp.id, Some(&ep2.id), "").is_err(), "Copilot 不能连自定义端点");
    }

    #[tokio::test]
    async fn concurrency_limit_is_enforced_per_endpoint() {
        let db = Db::open_memory().unwrap();
        let ep = db.add_endpoint(&input("串行")).unwrap();
        let first = acquire_slot(&ep).await;
        let second = tokio::time::timeout(std::time::Duration::from_millis(50), acquire_slot(&ep)).await;
        assert!(second.is_err(), "名额被占，第二个要等");
        drop(first);
        assert!(tokio::time::timeout(std::time::Duration::from_secs(5), acquire_slot(&ep)).await.is_ok(), "名额放回后应当很快拿到（给足时间，避免机器忙时误报）");
    }
}
