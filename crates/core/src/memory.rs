//! 长期记忆：公司 / 项目 / 员工个人 / 任务四层，SQLite + FTS5（trigram，支持中文）。
//!
//! 原则（对应需求文档第 11 节）：
//! - 个人记忆默认独立，只有本人能检索；项目记忆项目成员共享；
//! - 不是人类亲手写入的记忆一律先标"待验证"，检索时只注入"已验证"的，
//!   防止一次未经验证的猜测被永久写入知识库；
//! - 关联到任务的待验证经验，在任务合并后自动转为已验证；
//! - 每次只检索少量相关条目，有条数和字数上限，并记录注入了什么、多少字。

use crate::board::Issue;
use crate::db::{Db, Employee, now_ms};
use crate::orchestrator::Orchestrator;
use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use serde_json::json;
use std::collections::BTreeSet;

pub const MIGRATION_5: &str = r#"
CREATE TABLE memories (
    id              TEXT PRIMARY KEY,
    layer           TEXT NOT NULL,                 -- company | project | employee | task
    project_id      TEXT REFERENCES projects(id),
    employee_id     TEXT REFERENCES employees(id),
    task_id         TEXT REFERENCES tasks(id),
    kind            TEXT NOT NULL DEFAULT 'lesson',-- lesson | decision | fact | convention | pitfall
    title           TEXT NOT NULL,
    content         TEXT NOT NULL,
    status          TEXT NOT NULL,                 -- unverified | verified | expired
    source_kind     TEXT NOT NULL,                 -- human | meeting | issue | promoted
    source_ref      TEXT,                          -- 会议/议题/原记忆的 id
    scope           TEXT NOT NULL DEFAULT '',      -- 适用范围
    related_commit  TEXT,                          -- 相关提交
    promoted_from   TEXT,                          -- 由哪条项目/公司记忆学习而来
    created_by      TEXT NOT NULL,                 -- 'human' | 'meeting' | 'issue' 或员工 id
    created_at      INTEGER NOT NULL,
    verified_at     INTEGER,
    verified_by     TEXT,                          -- 'human' | 'merge'
    expired_at      INTEGER,
    expired_reason  TEXT,
    updated_at      INTEGER NOT NULL
);
CREATE INDEX idx_mem_project ON memories(project_id, status);
CREATE INDEX idx_mem_employee ON memories(employee_id, status);
CREATE INDEX idx_mem_task ON memories(task_id);
CREATE INDEX idx_mem_source ON memories(source_kind, source_ref);

CREATE VIRTUAL TABLE memories_fts USING fts5(title, content, content='memories', content_rowid='rowid', tokenize='trigram');
CREATE TRIGGER memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, title, content) VALUES (new.rowid, new.title, new.content);
END;
CREATE TRIGGER memories_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, title, content) VALUES ('delete', old.rowid, old.title, old.content);
END;
CREATE TRIGGER memories_au AFTER UPDATE OF title, content ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, title, content) VALUES ('delete', old.rowid, old.title, old.content);
    INSERT INTO memories_fts(rowid, title, content) VALUES (new.rowid, new.title, new.content);
END;

-- 记忆被注入过几次，用于衡量记忆是否真的有用、成本多大
CREATE TABLE memory_uses (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    memory_id   TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    employee_id TEXT,
    ref_kind    TEXT NOT NULL,                     -- task | meeting
    ref_id      TEXT NOT NULL,
    ts          INTEGER NOT NULL
);
CREATE INDEX idx_uses_memory ON memory_uses(memory_id);
"#;

pub const LAYERS: &[&str] = &["company", "project", "employee", "task"];
pub const KINDS: &[&str] = &["lesson", "decision", "fact", "convention", "pitfall"];
const MAX_CONTENT_CHARS: usize = 4000;
/// 每次最多注入的条数与总字数
pub const DEFAULT_MAX_ITEMS: usize = 6;
pub const DEFAULT_MAX_CHARS: usize = 1800;
const PER_ITEM_CHARS: usize = 450;
/// 命中分数达到这个值才算相关，避免常见字组造成的误命中
const MIN_RELEVANCE: u32 = 2;

#[derive(Debug, Clone, Serialize)]
pub struct Memory {
    pub id: String,
    pub layer: String,
    pub project_id: Option<String>,
    pub employee_id: Option<String>,
    pub task_id: Option<String>,
    pub kind: String,
    pub title: String,
    pub content: String,
    pub status: String,
    pub source_kind: String,
    pub source_ref: Option<String>,
    pub scope: String,
    pub related_commit: Option<String>,
    pub promoted_from: Option<String>,
    pub created_by: String,
    pub created_at: i64,
    pub verified_at: Option<i64>,
    pub verified_by: Option<String>,
    pub expired_at: Option<i64>,
    pub expired_reason: Option<String>,
    pub updated_at: i64,
    /// 被注入过的次数
    pub uses: i64,
}

#[derive(Debug, Clone, Default)]
pub struct NewMemory {
    pub layer: String,
    pub project_id: Option<String>,
    pub employee_id: Option<String>,
    pub task_id: Option<String>,
    pub kind: String,
    pub title: String,
    pub content: String,
    pub scope: String,
    pub source_kind: String,
    pub source_ref: Option<String>,
    pub created_by: String,
    /// 留空：人类写入为已验证，其他来源为待验证
    pub status: String,
    pub related_commit: Option<String>,
    pub promoted_from: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct MemoryFilter {
    pub layer: Option<String>,
    pub project_id: Option<String>,
    pub employee_id: Option<String>,
    pub status: Option<String>,
    pub query: Option<String>,
}

const COLS: &str = "id, layer, project_id, employee_id, task_id, kind, title, content, status, source_kind, source_ref,
    scope, related_commit, promoted_from, created_by, created_at, verified_at, verified_by, expired_at, expired_reason, updated_at";

fn row_to_memory(r: &rusqlite::Row<'_>) -> rusqlite::Result<Memory> {
    Ok(Memory {
        id: r.get(0)?, layer: r.get(1)?, project_id: r.get(2)?, employee_id: r.get(3)?, task_id: r.get(4)?,
        kind: r.get(5)?, title: r.get(6)?, content: r.get(7)?, status: r.get(8)?, source_kind: r.get(9)?,
        source_ref: r.get(10)?, scope: r.get(11)?, related_commit: r.get(12)?, promoted_from: r.get(13)?,
        created_by: r.get(14)?, created_at: r.get(15)?, verified_at: r.get(16)?, verified_by: r.get(17)?,
        expired_at: r.get(18)?, expired_reason: r.get(19)?, updated_at: r.get(20)?, uses: 0,
    })
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3400..=0x4DBF | 0x4E00..=0x9FFF)
}

/// 把任务描述转成检索词：英文数字词（≥3 字符）和中文连续片段的 3 字滑窗。
/// 返回 (词, 权重)：英文词和刚好 3 字的中文词权重 2，其余 3 字滑窗权重 1。
pub fn query_terms(text: &str) -> Vec<(String, u32)> {
    let mut terms: Vec<(String, u32)> = Vec::new();
    let mut seen = BTreeSet::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if is_cjk(c) {
            let start = i;
            while i < chars.len() && is_cjk(chars[i]) {
                i += 1;
            }
            let run = &chars[start..i];
            if run.len() >= 3 {
                let w = if run.len() == 3 { 2 } else { 1 };
                for win in run.windows(3).take(40) {
                    let t: String = win.iter().collect();
                    if seen.insert(t.clone()) {
                        terms.push((t, w));
                    }
                }
            }
        } else if c.is_ascii_alphanumeric() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            if i - start >= 3 {
                let t: String = chars[start..i].iter().take(40).collect::<String>().to_lowercase();
                if seen.insert(t.clone()) {
                    terms.push((t, 2));
                }
            }
        } else {
            i += 1;
        }
    }
    terms.truncate(80);
    terms
}

fn fts_query(terms: &[(String, u32)]) -> Option<String> {
    if terms.is_empty() {
        return None;
    }
    Some(terms.iter().map(|(t, _)| format!("\"{}\"", t.replace('"', "\"\""))).collect::<Vec<_>>().join(" OR "))
}

/// “这一节没有内容”的各种写法：模型常写成“无。会上没有新增……”这样带一句解释，不能把它当成一条经验。
/// 以“无/暂无/没有”开头，且后面紧跟标点（或就此结束）才算；“无法……”“没有索引时……”这类正常的句子不受影响。
pub fn is_none_marker(item: &str) -> bool {
    // 去掉 Markdown 的加粗、斜体、行内代码标记再判断（只用于判断，不改动条目本身）
    let plain: String = item.chars().filter(|c| !matches!(c, '*' | '_' | '`')).collect();
    let t = plain.trim();
    let lower = t.to_lowercase();
    if matches!(lower.as_str(), "n/a" | "none" | "nil" | "（无）" | "(无)" | "-" | "—") || lower.starts_with("none.") || lower.starts_with("n/a") {
        return true;
    }
    // 固定的“这里没有内容”的说法（后面不一定紧跟标点）
    const NONE_PHRASES: &[&str] = &["没有新增", "没有新的", "没有需要", "没有可", "没有任何", "无新增", "无新的", "无需", "不涉及", "本次没有", "本次无", "会上没有", "会上无"];
    if NONE_PHRASES.iter().any(|p| t.starts_with(p)) {
        return true;
    }
    for p in ["暂无", "没有", "无"] {
        if let Some(rest) = t.strip_prefix(p) {
            return match rest.chars().next() {
                None => true,
                Some(c) => "。，,.、；;：:！!）)".contains(c) || c.is_whitespace(),
            };
        }
    }
    false
}

/// 从会议纪要里取出某一节（标题以 `heading_prefix` 开头）的条目。没有这一节、或写着“无”时返回空。
pub fn extract_section_items(minutes: &str, heading_prefix: &str) -> Vec<String> {
    let mut in_section = false;
    let mut out = Vec::new();
    for line in minutes.lines() {
        let t = line.trim();
        if t.starts_with('#') {
            in_section = t.trim_start_matches('#').trim().starts_with(heading_prefix);
            continue;
        }
        if !in_section || t.is_empty() {
            continue;
        }
        let item = t
            .trim_start_matches(|c: char| c == '-' || c == '*' || c == '•' || c == '·')
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .trim_start_matches(['.', '、', ')', '）'])
            .trim();
        if !item.is_empty() && !is_none_marker(item) {
            out.push(item.to_string());
        }
    }
    out
}

/// 从会议纪要的“待验证的经验”一节提取条目。
pub fn extract_lessons(minutes: &str) -> Vec<String> {
    extract_section_items(minutes, "待验证")
}

pub fn layer_label(layer: &str) -> &'static str {
    match layer { "company" => "公司", "project" => "项目", "employee" => "个人", "task" => "任务", _ => "?" }
}
pub fn kind_label(kind: &str) -> &'static str {
    match kind { "lesson" => "经验", "decision" => "决策", "fact" => "事实", "convention" => "规范", "pitfall" => "坑", _ => "?" }
}

/// 把检索到的记忆整理成注入提示的文字块。
pub fn format_block(items: &[Memory]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let mut s = String::from("[相关经验——来自团队已验证的记忆，仅供参考；与代码现状或人类要求冲突时，以后者为准]\n");
    for m in items {
        s.push_str(&format!("- [{}·{}] {}：{}", layer_label(&m.layer), kind_label(&m.kind), m.title, clip(&m.content, PER_ITEM_CHARS)));
        if !m.scope.is_empty() {
            s.push_str(&format!("（适用：{}）", m.scope));
        }
        s.push('\n');
    }
    s
}

impl Db {
    pub fn add_memory(&self, n: NewMemory) -> Result<Memory> {
        if !LAYERS.contains(&n.layer.as_str()) {
            bail!("未知的记忆层级: {}", n.layer);
        }
        let kind = if n.kind.is_empty() { "lesson".to_string() } else { n.kind.clone() };
        if !KINDS.contains(&kind.as_str()) {
            bail!("未知的记忆类型: {kind}");
        }
        let content = n.content.trim();
        if content.is_empty() {
            bail!("记忆内容不能为空");
        }
        if content.chars().count() > MAX_CONTENT_CHARS {
            bail!("记忆内容过长（上限 {MAX_CONTENT_CHARS} 字），请精简后再写入");
        }
        match n.layer.as_str() {
            "company" => {}
            "project" => {
                let pid = n.project_id.as_deref().context("项目记忆需要指定项目")?;
                self.get_project(pid)?.context("项目不存在")?;
            }
            "employee" => {
                let eid = n.employee_id.as_deref().context("个人记忆需要指定员工")?;
                self.employee_by_id(eid)?.context("员工不存在")?;
            }
            _ => {
                let tid = n.task_id.as_deref().context("任务记忆需要指定任务")?;
                self.get_task(tid)?.context("任务不存在")?;
                n.project_id.as_deref().context("任务记忆需要指定项目")?;
            }
        }
        let human = n.source_kind == "human";
        let status = match n.status.as_str() {
            "" => if human { "verified" } else { "unverified" },
            s @ ("verified" | "unverified") => s,
            s => bail!("未知的状态: {s}"),
        };
        let title = if n.title.trim().is_empty() { clip(content.lines().next().unwrap_or(""), 40) } else { n.title.trim().to_string() };
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_ms();
        let (verified_at, verified_by) = if status == "verified" { (Some(now), Some(if human { "human" } else { n.source_kind.as_str() })) } else { (None, None) };
        self.conn.lock().unwrap().execute(
            "INSERT INTO memories (id, layer, project_id, employee_id, task_id, kind, title, content, status, source_kind, source_ref,
                                   scope, related_commit, promoted_from, created_by, created_at, verified_at, verified_by, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?16)",
            params![id, n.layer, n.project_id, n.employee_id, n.task_id, kind, title, content, status, n.source_kind,
                    n.source_ref, n.scope, n.related_commit, n.promoted_from, n.created_by, now, verified_at, verified_by],
        )?;
        self.append_event_for(n.project_id.as_deref(), "memory_created", n.employee_id.as_deref(), None,
            &json!({"memory_id": id, "layer": n.layer, "kind": kind, "status": status, "title": title}))?;
        self.get_memory(&id)?.context("写入记忆失败")
    }

    pub fn get_memory(&self, id: &str) -> Result<Option<Memory>> {
        let conn = self.conn.lock().unwrap();
        let m = conn.query_row(&format!("SELECT {COLS} FROM memories WHERE id = ?1"), [id], row_to_memory).optional()?;
        Ok(match m {
            Some(mut m) => {
                m.uses = conn.query_row("SELECT COUNT(*) FROM memory_uses WHERE memory_id = ?1", [id], |r| r.get(0))?;
                Some(m)
            }
            None => None,
        })
    }

    pub fn list_memories(&self, f: &MemoryFilter) -> Result<Vec<Memory>> {
        let mut sql = format!("SELECT {COLS} FROM memories WHERE 1=1");
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        for (col, v) in [("layer", &f.layer), ("project_id", &f.project_id), ("employee_id", &f.employee_id), ("status", &f.status)] {
            if let Some(v) = v {
                args.push(Box::new(v.clone()));
                sql.push_str(&format!(" AND {col} = ?{}", args.len()));
            }
        }
        if let Some(q) = f.query.as_ref().filter(|q| !q.trim().is_empty()) {
            let like = format!("%{}%", q.trim().replace('%', "\\%").replace('_', "\\_"));
            args.push(Box::new(like));
            sql.push_str(&format!(" AND (title LIKE ?{0} ESCAPE '\\' OR content LIKE ?{0} ESCAPE '\\')", args.len()));
        }
        sql.push_str(" ORDER BY CASE status WHEN 'unverified' THEN 0 WHEN 'verified' THEN 1 ELSE 2 END, created_at DESC LIMIT 500");
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter().map(|a| a.as_ref())), row_to_memory)?;
        let mut v: Vec<Memory> = rows.collect::<std::result::Result<_, _>>()?;
        for m in &mut v {
            m.uses = conn.query_row("SELECT COUNT(*) FROM memory_uses WHERE memory_id = ?1", [&m.id], |r| r.get(0))?;
        }
        Ok(v)
    }

    pub fn update_memory(&self, id: &str, title: &str, content: &str, scope: &str) -> Result<()> {
        let m = self.get_memory(id)?.context("记忆不存在")?;
        if m.status == "expired" {
            bail!("已过期的记忆不能修改");
        }
        let content = content.trim();
        if content.is_empty() || title.trim().is_empty() {
            bail!("标题和内容都不能为空");
        }
        if content.chars().count() > MAX_CONTENT_CHARS {
            bail!("记忆内容过长（上限 {MAX_CONTENT_CHARS} 字）");
        }
        self.conn.lock().unwrap().execute(
            "UPDATE memories SET title = ?2, content = ?3, scope = ?4, updated_at = ?5 WHERE id = ?1",
            params![id, title.trim(), content, scope.trim(), now_ms()],
        )?;
        Ok(())
    }

    pub fn verify_memory(&self, id: &str, by: &str) -> Result<()> {
        let m = self.get_memory(id)?.context("记忆不存在")?;
        if m.status != "unverified" {
            bail!("只有待验证的记忆才能验证，当前是「{}」", m.status);
        }
        let now = now_ms();
        self.conn.lock().unwrap().execute(
            "UPDATE memories SET status = 'verified', verified_at = ?2, verified_by = ?3, updated_at = ?2 WHERE id = ?1",
            params![id, now, by],
        )?;
        self.append_event_for(m.project_id.as_deref(), "memory_verified", None, None, &json!({"memory_id": id, "title": m.title, "by": by}))?;
        Ok(())
    }

    /// 标记过期：不删除，保留历史，但不再被检索。
    pub fn expire_memory(&self, id: &str, reason: &str) -> Result<()> {
        let m = self.get_memory(id)?.context("记忆不存在")?;
        if m.status == "expired" {
            bail!("这条记忆已经过期");
        }
        let now = now_ms();
        self.conn.lock().unwrap().execute(
            "UPDATE memories SET status = 'expired', expired_at = ?2, expired_reason = ?3, updated_at = ?2 WHERE id = ?1",
            params![id, now, reason],
        )?;
        self.append_event_for(m.project_id.as_deref(), "memory_expired", None, None, &json!({"memory_id": id, "title": m.title, "reason": reason}))?;
        Ok(())
    }

    /// 删除只允许用于待验证的候选（相当于否决）；已验证或已过期的要保留历史。
    pub fn delete_memory(&self, id: &str) -> Result<()> {
        let m = self.get_memory(id)?.context("记忆不存在")?;
        if m.status != "unverified" {
            bail!("只能删除待验证的候选；已验证的记忆请标记为过期");
        }
        self.conn.lock().unwrap().execute("DELETE FROM memories WHERE id = ?1", [id])?;
        Ok(())
    }

    /// 让员工学习一条已验证的项目/公司记忆：复制为 TA 的个人记忆并保留来源。
    pub fn promote_memory(&self, id: &str, employee_id: &str) -> Result<Memory> {
        let src = self.get_memory(id)?.context("记忆不存在")?;
        if !matches!(src.layer.as_str(), "project" | "company") {
            bail!("只有项目或公司记忆可以被员工学习");
        }
        if src.status != "verified" {
            bail!("只有已验证的记忆才能进入个人长期记忆");
        }
        let emp = self.employee_by_id(employee_id)?.context("员工不存在")?;
        if emp.archived {
            bail!("员工已归档");
        }
        if let Some(pid) = &src.project_id {
            if !self.is_member(pid, employee_id)? {
                bail!("{} 不是该项目的成员", emp.name);
            }
        }
        let dup: i64 = self.conn.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM memories WHERE layer = 'employee' AND employee_id = ?1 AND promoted_from = ?2 AND status != 'expired'",
            params![employee_id, id], |r| r.get(0))?;
        if dup > 0 {
            bail!("{} 已经学过这条经验了", emp.name);
        }
        self.add_memory(NewMemory {
            layer: "employee".into(), employee_id: Some(employee_id.into()), project_id: src.project_id.clone(),
            kind: src.kind.clone(), title: src.title.clone(), content: src.content.clone(), scope: src.scope.clone(),
            source_kind: "promoted".into(), source_ref: Some(src.id.clone()), created_by: "human".into(),
            status: "verified".into(), related_commit: src.related_commit.clone(), promoted_from: Some(src.id.clone()),
            ..Default::default()
        })
    }

    /// 任务合并后，**显式关联到该任务**的待验证记忆自动转为已验证，并记下合并提交。
    /// 注意：纪要里的经验、AI 提议的记忆都不会关联任务，所以不会走这条路，必须人类验证。
    pub fn verify_task_memories(&self, task_id: &str, commit: Option<&str>) -> Result<usize> {
        let now = now_ms();
        let ids: Vec<(String, Option<String>)> = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare("SELECT id, project_id FROM memories WHERE task_id = ?1 AND status = 'unverified'")?;
            let rows = stmt.query_map([task_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        for (id, pid) in &ids {
            self.conn.lock().unwrap().execute(
                "UPDATE memories SET status = 'verified', verified_at = ?2, verified_by = 'merge', related_commit = COALESCE(?3, related_commit), updated_at = ?2 WHERE id = ?1",
                params![id, now, commit],
            )?;
            self.append_event_for(pid.as_deref(), "memory_verified", None, None, &json!({"memory_id": id, "by": "merge", "task_id": task_id}))?;
        }
        Ok(ids.len())
    }

    /// 议题形成决议后，沉淀为项目决策记忆；它取代的旧决议对应的记忆自动过期。
    pub(crate) fn record_decision_memory(&self, issue: &Issue, by: &str, rejected: &[(String, String)]) -> Result<()> {
        let mut content = format!("决定：{}", issue.decision.as_deref().unwrap_or(""));
        if let Some(r) = issue.decision_reason.as_deref().filter(|r| !r.is_empty()) {
            content.push_str(&format!("\n原因：{r}"));
        }
        for (t, why) in rejected {
            content.push_str(&format!("\n未采纳「{t}」：{why}"));
        }
        self.add_memory(NewMemory {
            layer: "project".into(), project_id: Some(issue.project_id.clone()), kind: "decision".into(),
            title: issue.title.clone(), content, source_kind: "issue".into(), source_ref: Some(issue.id.clone()),
            created_by: by.into(), status: "verified".into(), ..Default::default()
        })?;
        if let Some(old) = &issue.supersedes {
            let stale: Vec<String> = {
                let conn = self.conn.lock().unwrap();
                let mut stmt = conn.prepare("SELECT id FROM memories WHERE source_kind = 'issue' AND source_ref = ?1 AND status != 'expired'")?;
                let rows = stmt.query_map([old], |r| r.get(0))?;
                rows.collect::<std::result::Result<_, _>>()?
            };
            for id in stale {
                self.expire_memory(&id, &format!("被新决议《{}》取代", issue.title))?;
            }
        }
        Ok(())
    }

    /// 把会议纪要里“待验证的经验”存为待验证的项目记忆（由人类触发，不自动入库）。返回新增条数。
    pub fn save_meeting_lessons(&self, meeting_id: &str) -> Result<usize> {
        let m = self.get_meeting(meeting_id)?.context("会议不存在")?;
        let lessons = extract_lessons(&m.minutes);
        if lessons.is_empty() {
            bail!("纪要里没有“待验证的经验”可以保存");
        }
        let mut added = 0;
        for l in lessons {
            let exists: i64 = self.conn.lock().unwrap().query_row(
                "SELECT COUNT(*) FROM memories WHERE source_kind = 'meeting' AND source_ref = ?1 AND content = ?2",
                params![meeting_id, l], |r| r.get(0))?;
            if exists > 0 {
                continue;
            }
            self.add_memory(NewMemory {
                // 不关联任务：任务合并只能证明代码通过了评审和测试，证明不了模型写在纪要里的这句话，
                // 所以必须由人类验证；否则“目前没有运行结果”这类当时的状态会在合并后被当成已验证的事实
                layer: "project".into(), project_id: Some(m.project_id.clone()), task_id: None, kind: "lesson".into(),
                content: l, source_kind: "meeting".into(), source_ref: Some(meeting_id.into()), created_by: "meeting".into(),
                scope: format!("来自会议《{}》", m.title), ..Default::default()
            })?;
            added += 1;
        }
        Ok(added)
    }

    /// 各项目待验证的记忆数量（待办收件箱使用）。
    pub fn unverified_counts(&self) -> Result<Vec<(String, String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT p.id, p.name, COUNT(*) FROM memories m JOIN projects p ON p.id = m.project_id
             WHERE m.status = 'unverified' AND p.status = 'active' GROUP BY p.id, p.name ORDER BY p.name",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// 检索与当前任务相关的少量已验证记忆。
    /// 可见范围：公司记忆；该项目的项目记忆；该员工自己的个人记忆；该任务的任务记忆。
    /// 别的员工的个人记忆永远不会被检索到。
    pub fn retrieve_memories(
        &self, project_id: Option<&str>, employee_id: &str, task_id: Option<&str>, text: &str, max_items: usize, max_chars: usize,
    ) -> Result<Vec<Memory>> {
        let mut picked: Vec<Memory> = Vec::new();
        let mut used_chars = 0usize;
        let mut push = |m: Memory, picked: &mut Vec<Memory>| {
            let cost = clip(&m.content, PER_ITEM_CHARS).chars().count() + m.title.chars().count();
            if picked.len() < max_items && used_chars + cost <= max_chars && !picked.iter().any(|p| p.id == m.id) {
                used_chars += cost;
                picked.push(m);
            }
        };

        // 当前任务自己的记忆（交接信息等）总是相关
        if let Some(tid) = task_id {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare(&format!("SELECT {COLS} FROM memories WHERE layer = 'task' AND task_id = ?1 AND status = 'verified' ORDER BY created_at"))?;
            let rows: Vec<Memory> = stmt.query_map([tid], row_to_memory)?.collect::<std::result::Result<_, _>>()?;
            drop(stmt);
            drop(conn);
            for m in rows {
                push(m, &mut picked);
            }
        }

        let terms = query_terms(text);
        let Some(q) = fts_query(&terms) else { return Ok(picked) };
        let cands: Vec<Memory> = {
            let conn = self.conn.lock().unwrap();
            let cols: String = COLS.split(',').map(|c| format!("m.{}", c.trim())).collect::<Vec<_>>().join(", ");
            let mut stmt = conn.prepare(&format!(
                "SELECT {cols} FROM memories_fts JOIN memories m ON m.rowid = memories_fts.rowid
                 WHERE memories_fts MATCH ?1 AND m.status = 'verified' AND m.layer != 'task' AND (
                     m.layer = 'company' OR (m.layer = 'project' AND m.project_id = ?2) OR (m.layer = 'employee' AND m.employee_id = ?3))
                 ORDER BY bm25(memories_fts) LIMIT 40"
            ))?;
            let rows = stmt.query_map(params![q, project_id, employee_id], row_to_memory)?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        // 按命中词的加权分过滤并排序（同分保持 bm25 顺序）
        let mut scored: Vec<(u32, Memory)> = cands
            .into_iter()
            .map(|m| {
                let hay = format!("{} {}", m.title, m.content).to_lowercase();
                (terms.iter().filter(|(t, _)| hay.contains(t.as_str())).map(|(_, w)| *w).sum(), m)
            })
            .filter(|(s, _)| *s >= MIN_RELEVANCE)
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, m) in scored {
            push(m, &mut picked);
        }
        Ok(picked)
    }

    pub fn record_memory_uses(&self, ids: &[String], employee_id: &str, ref_kind: &str, ref_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        for id in ids {
            conn.execute(
                "INSERT INTO memory_uses (memory_id, employee_id, ref_kind, ref_id, ts) VALUES (?1,?2,?3,?4,?5)",
                params![id, employee_id, ref_kind, ref_id, now_ms()],
            )?;
        }
        Ok(())
    }
}

impl Orchestrator {
    /// 为一次任务/会议发言检索记忆，记录使用情况和注入量，返回要拼进提示的文字块（可能为空）。
    pub fn memory_block(&self, project_id: Option<&str>, employee: &Employee, task_id: Option<&str>, text: &str, ref_kind: &str, ref_id: &str) -> String {
        let items = match self.db.retrieve_memories(project_id, &employee.id, task_id, text, DEFAULT_MAX_ITEMS, DEFAULT_MAX_CHARS) {
            Ok(v) => v,
            Err(_) => return String::new(), // 记忆检索失败不应阻断工作
        };
        if items.is_empty() {
            return String::new();
        }
        let block = format_block(&items);
        let ids: Vec<String> = items.iter().map(|m| m.id.clone()).collect();
        let _ = self.db.record_memory_uses(&ids, &employee.id, ref_kind, ref_id);
        let _ = self.db.append_event_for(project_id, "memory_injected", Some(&employee.id), None, &json!({
            "ref_kind": ref_kind, "ref_id": ref_id, "count": items.len(), "chars": block.chars().count(), "memory_ids": ids,
        }));
        block
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn setup() -> (Db, String, String, String, String) {
        let db = Db::open_memory().unwrap();
        let alice = db.add_employee("alice", "全栈", "p").unwrap();
        let bob = db.add_employee("bob", "全栈", "p").unwrap();
        let p1 = db.create_project("Shop", "", "/r1").unwrap();
        let p2 = db.create_project("Blog", "", "/r2").unwrap();
        db.add_member(&p1.id, &alice.id, "lead").unwrap();
        db.add_member(&p1.id, &bob.id, "developer").unwrap();
        db.add_member(&p2.id, &alice.id, "developer").unwrap();
        (db, alice.id, bob.id, p1.id, p2.id)
    }

    fn mem(layer: &str, pid: Option<&str>, eid: Option<&str>, title: &str, content: &str) -> NewMemory {
        NewMemory { layer: layer.into(), project_id: pid.map(Into::into), employee_id: eid.map(Into::into),
            title: title.into(), content: content.into(), source_kind: "human".into(), created_by: "human".into(), ..Default::default() }
    }

    #[test]
    fn terms_handle_chinese_and_english() {
        let t = query_terms("给登录接口补测试，需要先启动Redis吗");
        let words: Vec<&str> = t.iter().map(|(w, _)| w.as_str()).collect();
        assert!(words.contains(&"redis") && words.contains(&"登录接"));
        assert_eq!(t.iter().find(|(w, _)| w == "redis").unwrap().1, 2, "英文词权重更高");
        assert!(query_terms("的 和 a 是").is_empty(), "过短的词没有检索价值");
        assert_eq!(query_terms("数据库").first().unwrap().1, 2, "刚好 3 字的中文词权重 2");
        // 查询里的引号不会破坏 FTS 语法
        assert!(fts_query(&[("a\"b".into(), 1)]).unwrap().contains("\"a\"\"b\""));
    }

    #[test]
    fn extracts_any_section_by_heading() {
        let m = "## 决议\n- 用 SSE\n## 行动项\n1. alice 修复溢出\n2、bob 补测试\n## 待验证的经验\n无";
        assert_eq!(extract_section_items(m, "行动项"), vec!["alice 修复溢出", "bob 补测试"]);
        assert_eq!(extract_section_items(m, "决议"), vec!["用 SSE"]);
        assert!(extract_section_items(m, "待验证").is_empty());
        assert!(extract_section_items(m, "不存在").is_empty());
    }

    #[test]
    fn extracts_lessons_from_minutes() {
        let m = "## 决议\n用 SSE\n## 待验证的经验\n- 测试前要先启动 Redis\n2. 迁移脚本要幂等\n\n## 其他\n- 不相关";
        assert_eq!(extract_lessons(m), vec!["测试前要先启动 Redis", "迁移脚本要幂等"]);
        assert!(extract_lessons("## 待验证的经验\n无").is_empty());
        // 演练中遇到的真实写法：无，后面跟一句解释
        assert!(extract_lessons("## 待验证的经验\n\n无。会上没有新增或被引用的经验需要验证。").is_empty());
        assert!(extract_lessons("## 待验证的经验\n- 没有新增，无需验证").is_empty());
        assert!(extract_lessons("## 待验证的经验\n- **无**：这次没有").is_empty());
        // 正常的句子即使以“无/没有”开头也要保留
        assert_eq!(extract_lessons("## 待验证的经验\n- 无法连接 Redis 时测试会超时\n- 没有索引时迁移会很慢"), vec!["无法连接 Redis 时测试会超时", "没有索引时迁移会很慢"]);
        assert!(extract_lessons("## 待验证的经验\n- 无").is_empty());
        assert!(extract_lessons("没有标题").is_empty());
    }

    #[test]
    fn retrieval_is_relevant_scoped_and_verified_only() {
        let (db, alice, bob, p1, p2) = setup();
        db.add_memory(mem("project", Some(&p1), None, "测试依赖", "跑集成测试之前必须先启动 Redis 缓存服务")).unwrap();
        db.add_memory(mem("project", Some(&p2), None, "博客部署", "博客项目的 Redis 配置在 deploy 目录")).unwrap();
        db.add_memory(mem("company", None, None, "提交规范", "所有提交信息使用中文，并关联任务编号")).unwrap();
        db.add_memory(mem("employee", None, Some(&alice), "alice 的习惯", "alice 习惯先写失败的测试再改 Redis 逻辑")).unwrap();
        let mut cand = mem("project", Some(&p1), None, "未验证猜测", "我猜 Redis 的超时应该设成 5 秒");
        cand.source_kind = "meeting".into();
        db.add_memory(cand).unwrap();

        let got = |eid: &str, pid: &str, text: &str| -> Vec<String> {
            db.retrieve_memories(Some(pid), eid, None, text, 10, 5000).unwrap().into_iter().map(|m| m.title).collect()
        };
        // 相关：命中 Redis
        let a = got(&alice, &p1, "补集成测试，需要启动 redis 吗");
        assert!(a.contains(&"测试依赖".to_string()), "{a:?}");
        // 项目隔离：另一个项目的记忆不会出现
        assert!(!a.contains(&"博客部署".to_string()));
        // 待验证的猜测不会被注入
        assert!(!a.contains(&"未验证猜测".to_string()));
        // 个人记忆独立：alice 能检索到自己的，bob 检索不到
        assert!(a.contains(&"alice 的习惯".to_string()));
        assert!(!got(&bob, &p1, "补集成测试，需要启动 redis 吗").contains(&"alice 的习惯".to_string()));
        // 公司记忆对所有项目生效
        assert!(got(&bob, &p2, "提交信息应该怎么写，要关联任务编号").contains(&"提交规范".to_string()));
        // 无关任务不注入任何东西
        assert!(got(&alice, &p1, "把按钮颜色改成蓝色").is_empty());
        // 过短、无检索词的描述也不注入
        assert!(got(&alice, &p1, "好的").is_empty());
    }

    #[test]
    fn expired_memories_are_not_retrieved_and_budget_is_enforced() {
        let (db, alice, _b, p1, _p2) = setup();
        let old = db.add_memory(mem("project", Some(&p1), None, "旧约定", "接口统一返回 xml 格式的响应")).unwrap();
        for i in 0..10 {
            db.add_memory(mem("project", Some(&p1), None, &format!("接口约定{i}"), &format!("接口响应格式约定第{i}条：统一使用 json 格式"))).unwrap();
        }
        db.expire_memory(&old.id, "改用 json").unwrap();
        let got = db.retrieve_memories(Some(&p1), &alice, None, "接口响应格式", 4, 5000).unwrap();
        assert_eq!(got.len(), 4, "条数上限");
        assert!(got.iter().all(|m| m.id != old.id), "过期的不再检索");
        let tiny = db.retrieve_memories(Some(&p1), &alice, None, "接口响应格式", 10, 60).unwrap();
        assert!(tiny.len() < 4, "字数上限");
        assert!(db.expire_memory(&old.id, "x").is_err(), "不能重复过期");
        assert!(db.update_memory(&old.id, "t", "c", "").is_err(), "过期的不能改");
        assert_eq!(db.get_memory(&old.id).unwrap().unwrap().expired_reason.as_deref(), Some("改用 json"), "历史保留");
    }

    #[test]
    fn validation_and_lifecycle() {
        let (db, alice, _b, p1, _p2) = setup();
        assert!(db.add_memory(mem("galaxy", None, None, "t", "c")).is_err());
        assert!(db.add_memory(mem("project", None, None, "t", "c")).is_err(), "项目记忆要指定项目");
        assert!(db.add_memory(mem("employee", None, None, "t", "c")).is_err());
        assert!(db.add_memory(mem("task", Some(&p1), None, "t", "c")).is_err(), "任务记忆要指定任务");
        assert!(db.add_memory(mem("company", None, None, "t", "  ")).is_err());
        assert!(db.add_memory(mem("company", None, None, "t", &"字".repeat(4001))).is_err());
        let mut bad = mem("company", None, None, "t", "c");
        bad.kind = "rumor".into();
        assert!(db.add_memory(bad).is_err());

        // 人类写入直接是已验证；其他来源是待验证
        let h = db.add_memory(mem("company", None, None, "", "第一行当标题\n第二行")).unwrap();
        assert_eq!((h.status.as_str(), h.title.as_str(), h.verified_by.as_deref()), ("verified", "第一行当标题", Some("human")));
        let mut c = mem("project", Some(&p1), None, "候选", "某个经验");
        c.source_kind = "meeting".into();
        let c = db.add_memory(c).unwrap();
        assert_eq!(c.status, "unverified");
        assert!(db.delete_memory(&h.id).is_err(), "已验证的不能删，只能过期");
        db.verify_memory(&c.id, "human").unwrap();
        assert!(db.verify_memory(&c.id, "human").is_err());
        assert!(db.delete_memory(&c.id).is_err());
        let mut c2 = mem("project", Some(&p1), None, "另一个候选", "否决我");
        c2.source_kind = "meeting".into();
        let c2 = db.add_memory(c2).unwrap();
        db.delete_memory(&c2.id).unwrap();
        assert!(db.get_memory(&c2.id).unwrap().is_none());
        // 编辑会同步全文索引
        db.update_memory(&c.id, "候选", "改成关于 postgres 连接池的经验", "仅限本项目").unwrap();
        let got = db.retrieve_memories(Some(&p1), &alice, None, "postgres 连接池怎么配", 5, 3000).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].scope, "仅限本项目");
        assert!(db.retrieve_memories(Some(&p1), &alice, None, "某个经验", 5, 3000).unwrap().is_empty(), "旧内容已从索引中移除");
    }

    #[test]
    fn employee_learns_only_verified_shared_memory() {
        let (db, alice, bob, p1, p2) = setup();
        let verified = db.add_memory(mem("project", Some(&p1), None, "约定", "统一使用 tracing 打日志")).unwrap();
        let mut c = mem("project", Some(&p1), None, "猜测", "也许要加缓存");
        c.source_kind = "meeting".into();
        let cand = db.add_memory(c).unwrap();
        assert!(db.promote_memory(&cand.id, &alice).is_err(), "未验证的不能进入个人长期记忆");
        let p = db.promote_memory(&verified.id, &bob).unwrap();
        assert_eq!((p.layer.as_str(), p.source_kind.as_str(), p.promoted_from.as_deref()), ("employee", "promoted", Some(verified.id.as_str())));
        assert!(db.promote_memory(&verified.id, &bob).is_err(), "不重复学习");
        // 不是项目成员不能学（bob 不在 p2）
        let other = db.add_memory(mem("project", Some(&p2), None, "博客约定", "博客文章用 markdown")).unwrap();
        assert!(db.promote_memory(&other.id, &bob).is_err());
        // 学到的只有 bob 自己检索得到
        let q = "日志统一用 tracing";
        assert!(db.retrieve_memories(None, &bob, None, q, 5, 3000).unwrap().iter().any(|m| m.layer == "employee"));
        assert!(db.retrieve_memories(None, &alice, None, q, 5, 3000).unwrap().is_empty());
        assert!(db.promote_memory(&p.id, &alice).is_err(), "个人记忆不能再被别人学");
    }

    #[test]
    fn decisions_become_memories_and_superseded_ones_expire() {
        let (db, _a, _b, p1, _p2) = setup();
        let old = db.create_issue(&p1, "推送方案", "", "technical", false, None, None, None).unwrap();
        db.add_option(&old.id, "WebSocket", "双向", None).unwrap();
        let sse = db.add_option(&old.id, "SSE", "单向", None).unwrap();
        db.set_issue_status(&old.id, "discussing").unwrap();
        db.set_issue_status(&old.id, "pending_decision").unwrap();
        let mut rej = HashMap::new();
        rej.insert(db.issue_options(&old.id).unwrap()[0].id.clone(), "客户端不上报".to_string());
        db.decide_issue(&old.id, &crate::board::Actor::Human, Some(&sse.id), "采用 SSE", "实现简单", &rej).unwrap();

        let mems = db.list_memories(&MemoryFilter { project_id: Some(p1.clone()), layer: Some("project".into()), ..Default::default() }).unwrap();
        assert_eq!(mems.len(), 1);
        let d = &mems[0];
        assert_eq!((d.kind.as_str(), d.status.as_str(), d.source_kind.as_str()), ("decision", "verified", "issue"));
        assert!(d.content.contains("采用 SSE") && d.content.contains("实现简单") && d.content.contains("未采纳「WebSocket」：客户端不上报"));

        // 新议题取代旧决议并形成新决议 → 旧决策记忆自动过期
        let new = db.create_issue(&p1, "推翻推送方案", "", "technical", false, None, None, Some(&old.id)).unwrap();
        db.set_issue_status(&new.id, "discussing").unwrap();
        db.set_issue_status(&new.id, "pending_decision").unwrap();
        db.decide_issue(&new.id, &crate::board::Actor::Human, None, "改用 WebSocket", "需要双向", &HashMap::new()).unwrap();
        let after = db.list_memories(&MemoryFilter { project_id: Some(p1.clone()), ..Default::default() }).unwrap();
        let old_mem = after.iter().find(|m| m.source_ref.as_deref() == Some(old.id.as_str())).unwrap();
        let new_mem = after.iter().find(|m| m.source_ref.as_deref() == Some(new.id.as_str())).unwrap();
        assert_eq!(old_mem.status, "expired");
        assert!(old_mem.expired_reason.as_deref().unwrap().contains("推翻推送方案"));
        assert_eq!(new_mem.status, "verified");
    }

    #[test]
    fn meeting_lessons_become_unverified_and_dedupe() {
        let (db, _a, _b, p1, _p2) = setup();
        let m = db.create_meeting(&p1, "retro", "复盘", None, None, &[], 5).unwrap();
        assert!(db.save_meeting_lessons(&m.id).is_err(), "没有纪要");
        db.set_minutes(&m.id, "## 待验证的经验\n- 迁移脚本必须幂等\n- 发布前先跑冒烟测试").unwrap();
        assert_eq!(db.save_meeting_lessons(&m.id).unwrap(), 2);
        assert_eq!(db.save_meeting_lessons(&m.id).unwrap(), 0, "重复保存不会产生重复条目");
        let list = db.list_memories(&MemoryFilter { project_id: Some(p1.clone()), status: Some("unverified".into()), ..Default::default() }).unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.iter().all(|x| x.source_kind == "meeting" && x.scope.contains("复盘")));
        assert_eq!(db.unverified_counts().unwrap(), vec![(p1.clone(), "Shop".to_string(), 2)]);
    }
}

#[cfg(test)]
mod none_marker_tests {
    use super::is_none_marker;

    #[test]
    fn recognizes_empty_section_phrasings_but_not_real_sentences() {
        for yes in ["无", "无。", "暂无", "暂无。", "没有", "没有，本次不需要", "无，本次没有新增", "无。会上没有新增或被引用的经验需要验证。",
                    "**无**", "（无）", "(无)", "N/A", "none", "无 ", "没有。", "没有新增，无需验证", "无需验证", "本次没有新的经验", "不涉及"] {
            assert!(is_none_marker(yes), "{yes:?} 应当被视为“空”");
        }
        for no in ["无法连接 Redis 时测试会超时", "没有索引时迁移会很慢", "无锁队列在这里更快", "暂停服务前要先排空队列", "测试前必须先启动 Redis", "none-blocking IO 更合适"] {
            assert!(!is_none_marker(no), "{no:?} 是正常的经验，不能被丢掉");
        }
    }
}
