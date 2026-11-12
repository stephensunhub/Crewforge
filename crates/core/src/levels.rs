//! 任务等级体系。
//!
//! 计划条目有等级 `level`（1–5，0 = 未定级，行为和以前一样），按完成它需要的推理、联想和记忆来定，不是按工作量。
//! 员工有能力上限 `level_cap`（1–5，0 = 按模型自动给默认值，也可以手动改）。
//! 派活时：条目等级 ≤ 员工上限；没有明确指派时，选「够用的最低档」的人（便宜的活给便宜的模型，贵的模型留给难的活）。
//! 同一个条目因为做不好（验证、评审没过）连续失败两次，等级自动加一，换更强的人。

use crate::db::{Db, Employee};
use anyhow::{Result, bail};
use rusqlite::params;
use serde_json::Value;

pub const MAX_LEVEL: i64 = 5;

pub const MIGRATION_25: &str = r#"
ALTER TABLE plan_items ADD COLUMN level INTEGER NOT NULL DEFAULT 0;
ALTER TABLE employees ADD COLUMN level_cap INTEGER NOT NULL DEFAULT 0;
"#;

/// 写进负责人提示词的评分标准。
pub const LEVEL_RUBRIC: &str = "条目等级 level（1–5），按完成它需要的推理、联想和记忆来定，不是按工作量：\n\
1 = 机械改动：单个文件或函数，规格完全明确，不需要了解其他代码（改文案、补一个简单用例、改配置）\n\
2 = 简单：同一模块里的少量改动，目标明确，读一两个文件就够（补一组测试、加一个字段）\n\
3 = 中等：多个文件，要先读懂现有代码再改，需要一些判断（新增一个接口、重构一个函数）\n\
4 = 较难：跨模块，涉及接口约定、错误处理或数据流，要记住前后文并权衡（一个完整特性、改公共接口）\n\
5 = 最难：架构取舍、并发与数据一致性、数据迁移、疑难 bug 的根因定位\n\
拿不准就往高一级定；指派的人的能力上限不能低于条目等级。";

/// 从 JSON 里读 `level`（数字或 "L3"/"3"），不在 1–5 之内就是 0（未定级）。
pub fn parse_level(v: &Value) -> i64 {
    let l = v["level"].as_i64().or_else(|| v["level"].as_str().and_then(|s| s.trim().trim_start_matches(['L', 'l']).parse().ok()));
    l.filter(|l| (1..=MAX_LEVEL).contains(l)).unwrap_or(0)
}

/// 按模型给员工能力上限的默认值：连自建端点的本地模型 2；Haiku 3；Sonnet 4；Opus、Fable 5；其他按 3。
pub fn default_level_cap(model: &str, on_endpoint: bool) -> i64 {
    if on_endpoint { return 2; }
    let m = model.to_lowercase();
    if m.contains("opus") || m.contains("fable") { 5 }
    else if m.contains("sonnet") { 4 }
    else if m.contains("haiku") { 3 }
    else { 3 }
}

/// 这次失败是不是「做不好」（验证、评审没过）。环境问题（CLI 没起来、名额、网络）不算，不该升级。
pub fn is_capability_failure(err: &str) -> bool {
    ["验证仍未通过", "评审意见没有处理", "处理评审意见后验证仍未通过"].iter().any(|k| err.contains(k))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Escalation { pub title: String, pub old: i64, pub new: i64, pub assignee_cleared: bool }

impl Db {
    /// 条目升一级：指派的人能力不够了就清掉指派，放回待开始。没定级或已经最高级返回 None。
    pub fn escalate_plan_item_level(&self, id: &str) -> Result<Option<Escalation>> {
        let Some(item) = self.get_plan_item(id)? else { return Ok(None) };
        if item.level == 0 || item.level >= MAX_LEVEL { return Ok(None); }
        let new = item.level + 1;
        self.set_plan_item_level(id, new)?;
        let weak = match item.assignee_id.as_deref().and_then(|a| self.employee_by_id(a).ok().flatten()) {
            Some(e) => self.level_cap(&e)? < new,
            None => false,
        };
        if weak { self.clear_plan_item_assignee(id)?; }
        self.set_plan_status(id, "planned", &format!("连续两次没做好，等级由 {} 升到 {}{}", item.level, new, if weak { "，原指派的人能力不够，改由更强的人接" } else { "" }))?;
        Ok(Some(Escalation { title: item.title, old: item.level, new, assignee_cleared: weak }))
    }

    /// 员工此刻的能力上限：手动设置的优先；否则按他做开发任务用的模型取默认值。
    pub fn level_cap(&self, emp: &Employee) -> Result<i64> {
        if emp.level_cap > 0 { return Ok(emp.level_cap); }
        let model = if emp.model.is_empty() { self.purpose_model(&emp.provider, "task").unwrap_or_default() } else { emp.model.clone() };
        // 订阅员工没指定模型、用途默认也没设时是引擎默认（目前是 Sonnet 档）
        let model = if model.is_empty() && emp.provider == "claude" { "sonnet".to_string() } else { model };
        Ok(default_level_cap(&model, emp.endpoint_id.is_some()))
    }

    /// 手动设置员工的能力上限；0 = 恢复按模型自动。
    pub fn set_employee_level_cap(&self, id: &str, cap: i64) -> Result<()> {
        if !(0..=MAX_LEVEL).contains(&cap) { bail!("能力上限只能是 0（自动）或 1–{MAX_LEVEL}"); }
        self.conn.lock().unwrap().execute("UPDATE employees SET level_cap = ?2 WHERE id = ?1", params![id, cap])?;
        Ok(())
    }

    pub fn set_plan_item_level(&self, id: &str, level: i64) -> Result<()> {
        if !(0..=MAX_LEVEL).contains(&level) { bail!("等级只能是 0（未定级）或 1–{MAX_LEVEL}"); }
        self.conn.lock().unwrap().execute("UPDATE plan_items SET level = ?2, updated_at = ?3 WHERE id = ?1", params![id, level, crate::db::now_ms()])?;
        Ok(())
    }

    pub fn clear_plan_item_assignee(&self, id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute("UPDATE plan_items SET assignee_id = NULL, updated_at = ?2 WHERE id = ?1", params![id, crate::db::now_ms()])?;
        Ok(())
    }

    /// 写进提示词的成员列表后缀：「，能力上限 N 级」。
    pub fn cap_suffix(&self, employee_id: &str) -> String {
        match self.employee_by_id(employee_id).ok().flatten().and_then(|e| self.level_cap(&e).ok()) {
            Some(c) => format!("，能力上限 {c} 级"),
            None => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_level_accepts_numbers_and_l_prefix_and_rejects_the_rest() {
        assert_eq!(parse_level(&json!({"level": 3})), 3);
        assert_eq!(parse_level(&json!({"level": "L4"})), 4);
        assert_eq!(parse_level(&json!({"level": "2"})), 2);
        for bad in [json!({"level": 0}), json!({"level": 6}), json!({"level": -1}), json!({"level": "高"}), json!({}), json!({"level": null})] {
            assert_eq!(parse_level(&bad), 0, "{bad}");
        }
    }

    #[test]
    fn capability_failures_are_told_apart_from_environment_failures() {
        assert!(is_capability_failure("《A》验证仍未通过"));
        assert!(is_capability_failure("还有 2 条拦合并的评审意见没有处理"));
        assert!(!is_capability_failure("CLI 在返回结果前退出。stderr: unrecognized_model"));
        assert!(!is_capability_failure("bob 还有进行中的任务《X》，请先合并或放弃它"));
    }

    #[test]
    fn escalation_raises_the_level_clears_a_too_weak_assignee_and_stops_at_the_top() {
        use crate::plan::NewPlanItem;
        let db = Db::open_memory().unwrap();
        let p = db.create_project("p", "", "/tmp/p-levels").unwrap().id;
        let weak = db.add_employee("weak", "全栈", "x").unwrap();
        let strong = db.add_employee("strong", "全栈", "x").unwrap();
        db.add_member(&p, &weak.id, "developer").unwrap();
        db.add_member(&p, &strong.id, "developer").unwrap();
        db.set_employee_level_cap(&weak.id, 3).unwrap();
        db.set_employee_level_cap(&strong.id, 5).unwrap();
        let mk = |t: &str, who: &str, lvl: i64| { let id = db.create_plan_item(&p, &NewPlanItem { title: t.into(), assignee_id: Some(who.into()), ..Default::default() }, None).unwrap(); db.set_plan_item_level(&id, lvl).unwrap(); db.set_plan_status(&id, "blocked", "验证没过").unwrap(); id };
        let a = mk("A", &weak.id, 2);
        let e = db.escalate_plan_item_level(&a).unwrap().unwrap();
        assert_eq!((e.old, e.new, e.assignee_cleared), (2, 3, false), "上限 3 的人还够格，保留指派");
        let it = db.get_plan_item(&a).unwrap().unwrap();
        assert_eq!((it.level, it.status.as_str(), it.assignee_id.as_deref()), (3, "planned", Some(weak.id.as_str())));
        db.set_plan_status(&a, "blocked", "又没过").unwrap();
        let e = db.escalate_plan_item_level(&a).unwrap().unwrap();
        assert_eq!((e.old, e.new, e.assignee_cleared), (3, 4, true), "升到 4，上限 3 的人不够了，清掉指派");
        assert!(db.get_plan_item(&a).unwrap().unwrap().assignee_id.is_none());
        let top = mk("T", &strong.id, 5);
        assert!(db.escalate_plan_item_level(&top).unwrap().is_none(), "已经最高级");
        let none = mk("N", &strong.id, 0);
        assert!(db.escalate_plan_item_level(&none).unwrap().is_none(), "没定级不升级");
    }

    #[test]
    fn default_caps_follow_the_model() {
        assert_eq!(default_level_cap("qwen3.8:27b", true), 2, "本地模型");
        assert_eq!(default_level_cap("claude-haiku-5-5", false), 3);
        assert_eq!(default_level_cap("claude-sonnet-5-5", false), 4);
        assert_eq!(default_level_cap("claude-opus-5-5", false), 5);
        assert_eq!(default_level_cap("claude-fable-5-2", false), 5);
        assert_eq!(default_level_cap("gpt-6.1-sol", false), 3, "不认识的按 3");
    }

    #[test]
    fn manual_cap_overrides_the_model_default_and_zero_restores_auto() {
        let db = Db::open_memory().unwrap();
        let e = db.add_employee("a", "全栈", "p").unwrap();
        assert_eq!(db.level_cap(&e).unwrap(), 4, "没指定模型：引擎默认按 Sonnet 档");
        db.update_employee_full(&e.id, "全栈", "p", "claude", "claude-haiku-5-5").unwrap();
        let e = db.employee_by_id(&e.id).unwrap().unwrap();
        assert_eq!(db.level_cap(&e).unwrap(), 3);
        db.set_employee_level_cap(&e.id, 5).unwrap();
        assert_eq!(db.level_cap(&db.employee_by_id(&e.id).unwrap().unwrap()).unwrap(), 5);
        db.set_employee_level_cap(&e.id, 0).unwrap();
        assert_eq!(db.level_cap(&db.employee_by_id(&e.id).unwrap().unwrap()).unwrap(), 3);
        assert!(db.set_employee_level_cap(&e.id, 6).is_err());
    }
}
