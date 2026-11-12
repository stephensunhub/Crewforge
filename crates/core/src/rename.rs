//! 改员工的名字。
//!
//! 名字被用在：唯一键、工作区目录和分支前缀（`safe_slug`）、「正在工作」标记（内存里按名字）、历史文字（聊天里的 @名字、
//! 动态、纪要、记忆）、身份说明（默认写着「你是 X」）。任务、计划、会话、成员关系、费用都按 id 关联，不受影响。
//!
//! 规则：
//! - 员工必须空闲：没有进行中的任务、没有在执行；
//! - 新名字不含空格和 @、不重复；
//! - 工作区目录用 `git worktree move` 搬到新名字下，并同步任务表里存的路径；搬不动的保留原样并给出警告；
//! - 身份说明里的旧名字换成新名字；
//! - 历史文字一律不改，只记一条 `employee_renamed` 事件。

use crate::db::{Db, Employee};
use crate::orchestrator::Orchestrator;
use crate::workspace::safe_slug;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::json;
use std::path::Path;

/// 名字规则：去掉首尾空白后不能为空，不能含空白和 @。
pub fn valid_employee_name(name: &str) -> Result<String> {
    let n = name.trim();
    if n.is_empty() || n.contains(char::is_whitespace) || n.contains('@') {
        bail!("员工名称不能为空，且不能包含空格或 @");
    }
    Ok(n.to_string())
}

fn is_name_char(c: char) -> bool { c.is_ascii_alphanumeric() || c == '-' || c == '_' }

/// 把文字里的旧名字换成新名字。前后紧挨着英文字母、数字、-、_ 的不换（`Ollama-1` 不会误伤 `Ollama-10`）。
pub fn replace_name(text: &str, old: &str, new: &str) -> String {
    if old.is_empty() || old == new { return text.to_string(); }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut prev: Option<char> = None;
    while let Some(i) = rest.find(old) {
        let before = rest[..i].chars().last().or(prev);
        let after = rest[i + old.len()..].chars().next();
        out.push_str(&rest[..i]);
        if before.is_some_and(is_name_char) || after.is_some_and(is_name_char) {
            out.push_str(old);
        } else {
            out.push_str(new);
        }
        prev = old.chars().last();
        rest = &rest[i + old.len()..];
    }
    out.push_str(rest);
    out
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct RenameReport {
    pub from: String,
    pub to: String,
    /// 搬过去的工作区目录
    pub moved: Vec<String>,
    /// 没搬成的（保留原样）和其他需要你知道的事
    pub warnings: Vec<String>,
    pub persona_changed: bool,
}

impl Db {
    /// 改名前的检查：名字合法、不重复、员工空闲。返回员工和规整后的新名字。
    pub fn check_rename(&self, id: &str, new_name: &str) -> Result<(Employee, String)> {
        let name = valid_employee_name(new_name)?;
        let emp = self.employee_by_id(id)?.context("员工不存在")?;
        if emp.role == "guest" { bail!("顾问不能改名"); }
        if emp.name == name { bail!("名字没有变化"); }
        let conn = self.conn.lock().unwrap();
        let dup: i64 = conn.query_row("SELECT COUNT(*) FROM employees WHERE name = ?1", [&name], |r| r.get(0))?;
        if dup > 0 { bail!("已经有叫 {name} 的员工了"); }
        let active: i64 = conn.query_row("SELECT COUNT(*) FROM tasks WHERE employee_id = ?1 AND status = 'active'", [id], |r| r.get(0))?;
        if active > 0 { bail!("{} 有进行中的任务，请先合并或放弃，再改名", emp.name); }
        let running: i64 = conn.query_row("SELECT COUNT(*) FROM sessions WHERE employee_id = ?1 AND status = 'running'", [id], |r| r.get(0))?;
        if running > 0 { bail!("{} 正在执行，请等 TA 做完再改名", emp.name); }
        Ok((emp, name))
    }
}

impl Orchestrator {
    pub async fn rename_employee(&self, id: &str, new_name: &str) -> Result<RenameReport> {
        let (emp, name) = self.db.check_rename(id, new_name)?;
        let mut report = RenameReport { from: emp.name.clone(), to: name.clone(), ..Default::default() };
        let (old_slug, new_slug) = (safe_slug(&emp.name, &emp.id), safe_slug(&name, &emp.id));
        if old_slug != new_slug {
            let rows: Vec<(String, String)> = {
                let conn = self.db.conn.lock().unwrap();
                let mut st = conn.prepare("SELECT DISTINCT repo_path, worktree_path FROM tasks WHERE employee_id = ?1")?;
                st.query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?
            };
            for (repo, wt) in rows {
                let old = Path::new(&wt);
                if old.file_name().map(|n| n.to_string_lossy() != old_slug.as_str()).unwrap_or(true) || !old.exists() { continue; }
                let Some(parent) = old.parent() else { continue };
                let new = parent.join(&new_slug);
                if new.exists() {
                    report.warnings.push(format!("{} 已经存在，没有搬动 {}，TA 的新任务会用新目录", new.display(), old.display()));
                    continue;
                }
                match self.ws.move_worktree(Path::new(&repo), old, &new).await {
                    Ok(()) => {
                        let conn = self.db.conn.lock().unwrap();
                        conn.execute("UPDATE tasks SET worktree_path = ?3 WHERE employee_id = ?1 AND worktree_path = ?2", rusqlite::params![id, wt, new.to_string_lossy()])?;
                        report.moved.push(new.to_string_lossy().into_owned());
                    }
                    Err(e) => report.warnings.push(format!("没能把工作区 {} 搬到 {}：{e}（保留原样，TA 的新任务会在新目录里重新建）", old.display(), new.display())),
                }
            }
        }
        let persona = replace_name(&emp.persona, &emp.name, &name);
        report.persona_changed = persona != emp.persona;
        self.db.conn.lock().unwrap().execute("UPDATE employees SET name = ?2, persona = ?3 WHERE id = ?1", rusqlite::params![id, name, persona])
            .map_err(|e| anyhow::anyhow!("改名失败：{e}"))?;
        let text = format!("{} 改名为 {}{}", emp.name, name, if report.warnings.is_empty() { String::new() } else { format!("（有 {} 条提示，见员工页）", report.warnings.len()) });
        let payload = json!({"from": emp.name, "to": name, "moved": report.moved, "warnings": report.warnings, "persona_changed": report.persona_changed, "text": text});
        let projects = self.db.employee_projects(id)?;
        if projects.is_empty() {
            self.db.append_event("employee_renamed", Some(id), None, &payload)?;
        }
        for (pid, _) in projects {
            self.db.append_event_for(Some(&pid), "employee_renamed", Some(id), None, &payload)?;
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_name_respects_name_boundaries() {
        assert_eq!(replace_name("你是 Ollama-1，资深全栈。Ollama-1 先写测试。", "Ollama-1", "小王"), "你是 小王，资深全栈。小王 先写测试。");
        assert_eq!(replace_name("Ollama-10 和 xOllama-1 和 Ollama-1_a", "Ollama-1", "X"), "Ollama-10 和 xOllama-1 和 Ollama-1_a", "前后紧挨着名字字符的不换");
        assert_eq!(replace_name("@Ollama-1 你好", "Ollama-1", "Bob"), "@Bob 你好");
        assert_eq!(replace_name("没有旧名字", "Ollama-1", "Bob"), "没有旧名字");
        assert_eq!(replace_name("abc", "", "Bob"), "abc");
    }

    #[test]
    fn name_rules() {
        assert_eq!(valid_employee_name("  小王 ").unwrap(), "小王");
        for bad in ["", "  ", "a b", "@x", "x@y"] { assert!(valid_employee_name(bad).is_err(), "{bad}"); }
    }

    #[test]
    fn check_rename_rejects_duplicates_unchanged_guests_and_busy() {
        let db = Db::open_memory().unwrap();
        let a = db.add_employee("alice", "全栈", "p").unwrap();
        let b = db.add_employee("bob", "全栈", "p").unwrap();
        assert_eq!((a.number, b.number), (1, 2), "按创建顺序编号");
        assert!(db.check_rename(&a.id, "bob").unwrap_err().to_string().contains("已经有叫 bob"));
        assert!(db.check_rename(&a.id, "alice").unwrap_err().to_string().contains("没有变化"));
        assert!(db.check_rename(&a.id, "a b").is_err());
        assert_eq!(db.check_rename(&a.id, " carol ").unwrap().1, "carol");
        db.conn.lock().unwrap().execute("INSERT INTO sessions (id, employee_id, project_path, status, started, created_at, updated_at) VALUES ('s', ?1, '/p', 'running', 1, 1, 1)", [&a.id]).unwrap();
        assert!(db.check_rename(&a.id, "carol").unwrap_err().to_string().contains("正在执行"));
        let g = db.add_employee("guest1", "guest", "p").unwrap();
        assert_eq!(g.number, 0, "顾问不编号");
        assert!(db.check_rename(&g.id, "x").unwrap_err().to_string().contains("顾问"));
        let c = db.add_employee("carol2", "全栈", "p").unwrap();
        assert_eq!(c.number, 3, "顾问不占号");
    }

    #[test]
    fn migration_24_numbers_existing_employees_in_creation_order_and_skips_guests() {
        let d = tempfile::tempdir().unwrap();
        {
            let db = Db::open(d.path()).unwrap();
            for (n, r) in [("a", "全栈"), ("g", "guest"), ("b", "全栈"), ("c", "QA")] { db.add_employee(n, r, "p").unwrap(); }
            let conn = db.conn.lock().unwrap();
            conn.execute("ALTER TABLE employees DROP COLUMN number", []).unwrap(); // 回到迁移 24 之前的结构
            conn.pragma_update(None, "user_version", 23).unwrap();
        }
        let db = Db::open(d.path()).unwrap();
        let num = |n: &str| db.employee_by_name(n).unwrap().unwrap().number;
        assert_eq!((num("a"), num("g"), num("b"), num("c")), (1, 0, 2, 3));
        assert_eq!(db.add_employee("d", "全栈", "p").unwrap().number, 4, "新员工接着往后排");
    }
}
