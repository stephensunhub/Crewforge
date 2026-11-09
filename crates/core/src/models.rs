//! 引擎与模型管理：用户激活哪些执行引擎（Claude Code、GitHub Copilot），每个引擎有哪些可选模型，
//! 以及“按用途的默认模型”（量大的活用强模型，量小的活用便宜模型）。
//!
//! 选模型的优先级：员工自己指定的模型 > 该引擎在这个用途下的默认模型 > 引擎默认。

use crate::db::{Db, PROVIDERS, now_ms};
use anyhow::{Result, bail};
use rusqlite::params;
use serde::Serialize;

pub const MIGRATION_13: &str = r#"
CREATE TABLE models (
    provider   TEXT NOT NULL,
    model_id   TEXT NOT NULL,             -- 传给 CLI 的 --model
    label      TEXT NOT NULL DEFAULT '',
    note       TEXT NOT NULL DEFAULT '',
    enabled    INTEGER NOT NULL DEFAULT 1,
    builtin    INTEGER NOT NULL DEFAULT 0, -- 内置或从 CLI 发现的；自己添加的为 0，可删除
    created_at INTEGER NOT NULL,
    PRIMARY KEY (provider, model_id)
);
"#;

/// 可以设默认模型的用途。
pub const PURPOSES: &[(&str, &str)] = &[("task", "开发任务"), ("meeting", "会议发言"), ("minutes", "纪要起草"), ("ruling", "负责人裁决"), ("planning", "负责人计划"), ("docs", "负责人文档"), ("curation", "负责人整理记忆"), ("onboarding", "导入评估")];

/// Claude Code 各用途的推荐模型：难判断、次数少的用强模型，日常开发和讨论用中档，纯整理用小模型。
/// 只是建议，由用户点“应用推荐”才会写入，而且只填还没设置的用途。
pub const RECOMMENDED_CLAUDE: &[(&str, &str, &str)] = &[
    ("task", "sonnet", "日常开发、写测试、修常规 bug，量最大，用中档"),
    ("meeting", "sonnet", "讨论要有判断力，但每次发言很短"),
    ("minutes", "haiku", "把已有的讨论整理成纪要，轻量"),
    ("ruling", "opus", "技术裁决影响后续方向，次数少，值得用强模型"),
    ("planning", "opus", "起草和调整计划决定整个批次的走向，次数少"),
    ("docs", "sonnet", "读代码写概览，要准确但不需要最强"),
    ("curation", "sonnet", "核实记忆是否与代码一致，需要读代码"),
    ("onboarding", "sonnet", "导入评估读大量代码，中档够用"),
];

/// Claude Code 内置可选项：别名（CLI 解析为最新版本）与完整模型名。
const CLAUDE_SEED: &[(&str, &str, &str)] = &[
    ("opus", "Opus（别名）", "最强，适合难任务；用量大时很贵"),
    ("sonnet", "Sonnet（别名）", "均衡，日常开发"),
    ("haiku", "Haiku（别名）", "最便宜最快，适合会议发言、纪要等轻量活"),
    ("claude-opus-5-5", "Claude Opus 5.5", ""),
    ("claude-sonnet-5-5", "Claude Sonnet 5.5", ""),
    ("claude-haiku-5-5", "Claude Haiku 5.5", ""),
    ("claude-fable-5-1", "Claude Fable 5.1", ""),
];

/// Copilot 的备用清单（`copilot help config` 里没解析到时使用；正常以“发现模型”的结果为准）。
const COPILOT_SEED: &[&str] = &["auto", "claude-sonnet-5.5", "claude-opus-5.5", "claude-haiku-5.5", "gpt-6.1-sol", "gemini-3.8-flash", "grok-4.7"];

#[derive(Debug, Clone, Serialize)]
pub struct ModelEntry {
    pub provider: String,
    pub model_id: String,
    pub label: String,
    pub note: String,
    pub enabled: bool,
    pub builtin: bool,
}

/// 从 `copilot help config` 的输出里解析出可用模型 id：`model` 一节下面以 `- "id"` 列出。
pub fn parse_copilot_models(help: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in help.lines() {
        let t = line.trim();
        if t.starts_with("`model`:") {
            inside = true;
            continue;
        }
        if inside {
            if let Some(id) = t.strip_prefix("- \"").and_then(|r| r.strip_suffix('"')) {
                out.push(id.to_string());
            } else if t.starts_with('`') || (t.is_empty() && !out.is_empty()) {
                break;
            }
        }
    }
    out
}

impl Db {
    fn model_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ModelEntry> {
        Ok(ModelEntry { provider: r.get(0)?, model_id: r.get(1)?, label: r.get(2)?, note: r.get(3)?, enabled: r.get::<_, i64>(4)? != 0, builtin: r.get::<_, i64>(5)? != 0 })
    }

    /// 首次使用时写入内置清单（只写一次，之后用户的启停与删除不会被覆盖）。
    fn ensure_model_seed(&self) -> Result<()> {
        if self.get_setting("models_seeded")?.is_some() {
            return Ok(());
        }
        let now = now_ms();
        {
            let conn = self.conn.lock().unwrap();
            for (id, label, note) in CLAUDE_SEED {
                conn.execute("INSERT OR IGNORE INTO models (provider, model_id, label, note, builtin, created_at) VALUES ('claude',?1,?2,?3,1,?4)", params![id, label, note, now])?;
            }
            for id in COPILOT_SEED {
                conn.execute("INSERT OR IGNORE INTO models (provider, model_id, label, builtin, created_at) VALUES ('copilot',?1,?1,1,?2)", params![id, now])?;
            }
        }
        self.set_setting("models_seeded", "1")
    }

    pub fn list_models(&self, provider: Option<&str>) -> Result<Vec<ModelEntry>> {
        self.ensure_model_seed()?;
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT provider, model_id, label, note, enabled, builtin FROM models WHERE (?1 IS NULL OR provider = ?1) ORDER BY provider, builtin DESC, rowid")?;
        let rows = stmt.query_map(params![provider], Self::model_row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn add_model(&self, provider: &str, model_id: &str, label: &str) -> Result<()> {
        self.ensure_model_seed()?;
        if !PROVIDERS.contains(&provider) {
            bail!("未知的执行引擎：{provider}");
        }
        let id = model_id.trim();
        if id.is_empty() || id.contains(char::is_whitespace) {
            bail!("模型名不能为空，也不能含空格");
        }
        let n = self.conn.lock().unwrap().execute(
            "INSERT OR IGNORE INTO models (provider, model_id, label, builtin, created_at) VALUES (?1,?2,?3,0,?4)",
            params![provider, id, if label.trim().is_empty() { id } else { label.trim() }, now_ms()])?;
        if n == 0 {
            bail!("{provider} 已经有模型 {id} 了");
        }
        Ok(())
    }

    pub fn set_model_enabled(&self, provider: &str, model_id: &str, enabled: bool) -> Result<()> {
        let n = self.conn.lock().unwrap().execute("UPDATE models SET enabled = ?3 WHERE provider = ?1 AND model_id = ?2", params![provider, model_id, enabled as i64])?;
        if n == 0 {
            bail!("没有这个模型");
        }
        Ok(())
    }

    /// 只能删除自己添加的；内置的只能停用。
    pub fn remove_model(&self, provider: &str, model_id: &str) -> Result<()> {
        let n = self.conn.lock().unwrap().execute("DELETE FROM models WHERE provider = ?1 AND model_id = ?2 AND builtin = 0", params![provider, model_id])?;
        if n == 0 {
            bail!("内置或发现的模型不能删除，可以停用");
        }
        Ok(())
    }

    /// 把从 CLI 发现的模型并入清单（新的加入，已有的不动）。返回新增数。
    pub fn merge_discovered_models(&self, provider: &str, ids: &[String]) -> Result<usize> {
        self.ensure_model_seed()?;
        let conn = self.conn.lock().unwrap();
        let mut added = 0;
        for id in ids {
            added += conn.execute("INSERT OR IGNORE INTO models (provider, model_id, label, builtin, created_at) VALUES (?1,?2,?2,1,?3)", params![provider, id, now_ms()])?;
        }
        Ok(added)
    }

    /// 引擎是否已激活（默认激活）。停用后不能新建或切换到它，已有员工执行时会被拒绝并说明原因。
    pub fn provider_enabled(&self, provider: &str) -> Result<bool> {
        Ok(self.get_setting(&format!("provider.{provider}.enabled"))?.as_deref() != Some("off"))
    }

    pub fn set_provider_enabled(&self, provider: &str, enabled: bool) -> Result<()> {
        if !PROVIDERS.contains(&provider) {
            bail!("未知的执行引擎：{provider}");
        }
        self.set_setting(&format!("provider.{provider}.enabled"), if enabled { "on" } else { "off" })
    }

    /// 某引擎在某用途下的默认模型（空 = 引擎默认）。
    pub fn purpose_model(&self, provider: &str, purpose: &str) -> Result<String> {
        Ok(self.get_setting(&format!("model.{provider}.{purpose}"))?.unwrap_or_default())
    }

    pub fn set_purpose_model(&self, provider: &str, purpose: &str, model: &str) -> Result<()> {
        if !PROVIDERS.contains(&provider) || !PURPOSES.iter().any(|(p, _)| *p == purpose) {
            bail!("未知的引擎或用途");
        }
        if model.contains(char::is_whitespace) {
            bail!("模型名不能包含空格");
        }
        self.set_setting(&format!("model.{provider}.{purpose}"), model.trim())
    }

    /// 把推荐模型填进 Claude 还没设置的用途（已设置的不动）。返回填了几项。
    pub fn apply_recommended_models(&self) -> Result<usize> {
        let mut n = 0;
        for (purpose, model, _) in RECOMMENDED_CLAUDE {
            if self.purpose_model("claude", purpose)?.is_empty() {
                self.set_purpose_model("claude", purpose, model)?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// 最终使用的模型：员工指定 > 用途默认 > 引擎默认（None）。
    pub fn effective_model(&self, provider: &str, employee_model: &str, purpose: &str) -> Result<Option<String>> {
        if !employee_model.trim().is_empty() {
            return Ok(Some(employee_model.trim().to_string()));
        }
        Ok(Some(self.purpose_model(provider, purpose)?).filter(|m| !m.is_empty()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applying_recommended_models_only_fills_blanks() {
        let db = Db::open_memory().unwrap();
        db.set_purpose_model("claude", "task", "opus").unwrap();
        assert_eq!(db.apply_recommended_models().unwrap(), RECOMMENDED_CLAUDE.len() - 1);
        assert_eq!(db.purpose_model("claude", "task").unwrap(), "opus", "已设置的不覆盖");
        assert_eq!(db.purpose_model("claude", "ruling").unwrap(), "opus");
        assert_eq!(db.purpose_model("claude", "minutes").unwrap(), "haiku");
        assert_eq!(db.purpose_model("copilot", "minutes").unwrap(), "", "Copilot 不动");
        assert_eq!(db.apply_recommended_models().unwrap(), 0);
        assert!(RECOMMENDED_CLAUDE.iter().all(|(p, _, _)| PURPOSES.iter().any(|(k, _)| k == p)));
    }

    #[test]
    fn parses_the_model_list_from_copilot_help() {
        let help = "  `model`: AI model to use.\n    - \"claude-sonnet-5.5\"\n    - \"grok-4.7\"\n    - \"kimi-k3\"\n\n  `contextTier`: x\n    - \"nope\"\n";
        assert_eq!(parse_copilot_models(help), vec!["claude-sonnet-5.5", "grok-4.7", "kimi-k3"]);
        assert!(parse_copilot_models("nothing here").is_empty());
    }

    #[test]
    fn catalog_seed_custom_models_enable_disable_and_discovery() {
        let db = Db::open_memory().unwrap();
        let claude = db.list_models(Some("claude")).unwrap();
        assert!(claude.iter().any(|m| m.model_id == "haiku") && claude.iter().all(|m| m.enabled && m.builtin));
        db.add_model("claude", "my-model", "").unwrap();
        assert!(db.add_model("claude", "my-model", "").is_err(), "重复");
        assert!(db.add_model("claude", "a b", "").is_err() && db.add_model("gemini", "x", "").is_err());
        db.set_model_enabled("claude", "opus", false).unwrap();
        assert!(!db.list_models(Some("claude")).unwrap().iter().find(|m| m.model_id == "opus").unwrap().enabled);
        assert!(db.remove_model("claude", "haiku").is_err(), "内置的只能停用");
        db.remove_model("claude", "my-model").unwrap();
        // 用户的启停不会被重复播种覆盖
        assert!(!db.list_models(Some("claude")).unwrap().iter().find(|m| m.model_id == "opus").unwrap().enabled);
        // 发现的模型并入，已有的不重复
        let added = db.merge_discovered_models("copilot", &["grok-4.7".into(), "kimi-k3".into()]).unwrap();
        assert_eq!(added, 1, "grok-4.7 已在清单里");
        assert!(db.list_models(Some("copilot")).unwrap().iter().any(|m| m.model_id == "kimi-k3"));
    }

    #[test]
    fn provider_activation_and_model_precedence() {
        let db = Db::open_memory().unwrap();
        assert!(db.provider_enabled("claude").unwrap() && db.provider_enabled("copilot").unwrap(), "默认激活");
        db.set_provider_enabled("copilot", false).unwrap();
        assert!(!db.provider_enabled("copilot").unwrap() && db.provider_enabled("claude").unwrap());
        assert!(db.set_provider_enabled("gemini", true).is_err());

        // 员工指定 > 用途默认 > 引擎默认
        assert_eq!(db.effective_model("claude", "", "task").unwrap(), None);
        db.set_purpose_model("claude", "minutes", "haiku").unwrap();
        db.set_purpose_model("claude", "task", "opus").unwrap();
        assert_eq!(db.effective_model("claude", "", "minutes").unwrap().as_deref(), Some("haiku"));
        assert_eq!(db.effective_model("claude", "sonnet", "minutes").unwrap().as_deref(), Some("sonnet"));
        assert_eq!(db.effective_model("claude", "", "meeting").unwrap(), None, "没设的用途用引擎默认");
        assert_eq!(db.effective_model("copilot", "", "task").unwrap(), None, "默认模型按引擎分开");
        db.set_purpose_model("claude", "minutes", "").unwrap();
        assert_eq!(db.effective_model("claude", "", "minutes").unwrap(), None);
        assert!(db.set_purpose_model("claude", "bogus", "x").is_err());
    }
}
