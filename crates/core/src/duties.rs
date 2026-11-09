//! “我的职责”：人类在项目里是负责人（CTO），哪些事只能 TA 决定，现在有哪些事在等 TA、各自为什么轮到 TA。
//! 定义来自需求文档 20.4（里程碑验收、合并由负责人确认）与 20.6（拍板权限按类型）：
//! 需求与业务方向、高风险议题只能人类拍板；计划要人批准才派活；合并要负责人确认；行动项只有人能关闭；
//! 没验证的经验不会注入给员工。

use crate::db::Db;
use anyhow::Result;
use rusqlite::params;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct DutyItem {
    /// plan | decision | proposal | merge | review | action | memory
    pub kind: String,
    pub title: String,
    /// 为什么轮到你
    pub reason: String,
    /// 应该去的项目页签
    pub goto: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Duties {
    pub items: Vec<DutyItem>,
    /// 没有轮到你的评审行动项（不拦合并、也不需要你决定），留在会议页，AI 或负责人择机处理
    pub later_actions: usize,
    pub plan_done: usize,
    pub plan_total: usize,
    pub plan_percent: u32,
}

impl Db {
    pub fn my_duties(&self, project_id: &str) -> Result<Duties> {
        let mut items = Vec::new();
        let plan = self.plan_view(project_id)?;
        if plan.counts.proposed > 0 {
            items.push(DutyItem { kind: "plan".into(), title: format!("AI 起草了 {} 个计划条目", plan.counts.proposed), reason: "计划要你批准后才会派活，批准之前员工不会开始做".into(), goto: "plan".into(), count: plan.counts.proposed });
        }
        for d in self.pending_decisions()?.into_iter().filter(|d| d.issue.project_id == project_id) {
            let only_human = d.issue.high_risk || d.issue.kind == "requirement";
            items.push(DutyItem { kind: "decision".into(), title: format!("议题待拍板：{}", d.issue.title),
                reason: if only_human { "需求类或高风险议题，只有你能拍板（AI 没有拍板权限）".into() } else { "讨论已结束，等有权限者拍板；你始终可以拍板".into() }, goto: "issues".into(), count: 1 });
        }
        for d in self.ai_proposals()?.into_iter().filter(|d| d.issue.project_id == project_id) {
            items.push(DutyItem { kind: "proposal".into(), title: format!("员工提议了议题：{}", d.issue.title), reason: "AI 只能提议；是否值得讨论由你确认，确认前不会占用大家的额度".into(), goto: "issues".into(), count: 1 });
        }
        let conn = self.conn.lock().unwrap();
        // 评审已结束、没有拦合并的行动项：等你确认合并
        let project_path: String = conn.query_row("SELECT path FROM projects WHERE id = ?1", params![project_id], |r| r.get(0))?;
        let mut tasks = conn.prepare("SELECT id, title FROM tasks WHERE repo_path = ?1 AND status = 'active' ORDER BY created_at")?;
        let active: Vec<(String, String)> = tasks.query_map(params![project_path], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?;
        for (tid, title) in active {
            let blocking: i64 = conn.query_row("SELECT COUNT(*) FROM action_items WHERE task_id = ?1 AND blocking = 1 AND status IN ('open','working')", params![tid], |r| r.get(0))?;
            let reviewed: i64 = conn.query_row("SELECT COUNT(*) FROM meetings WHERE task_id = ?1 AND kind = 'code_review' AND status = 'closed'", params![tid], |r| r.get(0))?;
            if blocking > 0 {
                items.push(DutyItem { kind: "review".into(), title: format!("《{title}》有 {blocking} 条拦合并的评审意见"), reason: "员工改完不等于解决：处理中的意见仍拦着合并，需要你确认已解决（或写明原因后放行）".into(), goto: "meetings".into(), count: blocking as usize });
            } else if reviewed > 0 {
                items.push(DutyItem { kind: "merge".into(), title: format!("《{title}》评审已结束，等你确认合并"), reason: "合并到主分支由项目负责人最终确认；负责人是你，所以由你来点合并".into(), goto: "tasks".into(), count: 1 });
            }
        }
        // 明确需要你决定的行动项
        let mine: Vec<String> = {
            let mut s = conn.prepare("SELECT text FROM action_items WHERE project_id = ?1 AND for_human = 1 AND status IN ('open','working') ORDER BY created_at")?;
            s.query_map(params![project_id], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?
        };
        if !mine.is_empty() {
            let shown: Vec<String> = mine.iter().take(3).map(|t| t.chars().take(40).collect::<String>()).collect();
            items.push(DutyItem { kind: "action".into(), title: format!("{} 件事需要你决定：{}{}", mine.len(), shown.join("；"), if mine.len() > 3 { " …" } else { "" }),
                reason: "评审或会议里指明要由人来处理的事项；只有人能关闭行动项".into(), goto: "meetings".into(), count: mine.len() });
        }
        let unverified: i64 = conn.query_row("SELECT COUNT(*) FROM memories WHERE project_id = ?1 AND status = 'unverified'", params![project_id], |r| r.get(0))?;
        if unverified > 0 {
            items.push(DutyItem { kind: "memory".into(), title: format!("{unverified} 条 AI 总结的经验等你验证"), reason: "没验证的经验不会注入给员工；你确认它真的有用，员工才会用上".into(), goto: "memory".into(), count: unverified as usize });
        }
        let later: i64 = conn.query_row(
            "SELECT COUNT(*) FROM action_items WHERE project_id = ?1 AND status IN ('open','working') AND for_human = 0
               AND NOT (blocking = 1 AND task_id IN (SELECT id FROM tasks WHERE status = 'active'))", params![project_id], |r| r.get(0))?;
        Ok(Duties { items, later_actions: later as usize, plan_done: plan.counts.done, plan_total: plan.items.iter().filter(|i| i.status != "cancelled").count(), plan_percent: plan.percent_done })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::NewPlanItem;
    use rusqlite::params;

    #[test]
    fn duties_list_only_what_is_waiting_for_the_human_and_say_why() {
        let db = Db::open_memory().unwrap();
        let alice = db.add_employee("alice", "全栈", "p").unwrap();
        let p = db.create_project("P", "", "/r").unwrap();
        db.add_member(&p.id, &alice.id, "lead").unwrap();
        // 什么都没有：没有待办，也不假装有事
        let empty = db.my_duties(&p.id).unwrap();
        assert!(empty.items.is_empty() && empty.later_actions == 0);

        // 计划：AI 提议的条目等批准
        db.create_plan_item(&p.id, &NewPlanItem { title: "迁移账户模块".into(), description: String::new(), acceptance: String::new(), kind: "migrate".into(), module: String::new(), assignee_id: None, deps: vec![] }, Some(&alice.id)).unwrap();
        // 议题：一个需求类（只有人能拍）、一个 AI 提议待确认
        let req = db.create_issue(&p.id, "要不要支持多币种", "", "requirement", false, None, None, None).unwrap();
        db.conn.lock().unwrap().execute("UPDATE issues SET status = 'pending_decision' WHERE id = ?1", params![req.id]).unwrap();
        db.create_issue(&p.id, "换个存储引擎？", "", "technical", false, Some(&alice.id), None, None).unwrap();
        // 任务：t1 评审已结束且没有拦合并的意见 → 等确认合并；t2 有拦合并的意见
        for (id, title) in [("t1", "转账"), ("t2", "提现")] {
            db.create_task(&crate::db::Task { id: id.into(), repo_path: "/r".into(), employee_id: alice.id.clone(), title: title.into(), branch: format!("b-{id}"), base_branch: "main".into(),
                base_commit: "c".into(), worktree_path: "/w".into(), status: "active".into(), merged_commit: None, created_at: 1, updated_at: 1 }).unwrap();
        }
        let m1 = db.create_meeting(&p.id, "code_review", "评审转账", None, Some("t1"), &[], 5).unwrap();
        db.close_meeting(&m1.id).unwrap();
        let m2 = db.create_meeting(&p.id, "code_review", "评审提现", None, Some("t2"), &[], 5).unwrap();
        db.set_minutes(&m2.id, "## 行动项\n- [合并前] alice：补边界测试\n- [之后] alice：以后优化\n- [之后] 人类：决定是否支持负数").unwrap();
        db.close_meeting(&m2.id).unwrap();

        let d = db.my_duties(&p.id).unwrap();
        let kinds: Vec<&str> = d.items.iter().map(|i| i.kind.as_str()).collect();
        for k in ["plan", "decision", "proposal", "merge", "review", "action"] {
            assert!(kinds.contains(&k), "缺少 {k}：{kinds:?}");
        }
        // 每一项都说明为什么轮到你，并指向页签
        assert!(d.items.iter().all(|i| !i.reason.is_empty() && !i.goto.is_empty()));
        assert!(d.items.iter().find(|i| i.kind == "decision").unwrap().reason.contains("只有你能拍板"));
        assert!(d.items.iter().find(|i| i.kind == "merge").unwrap().title.contains("转账"));
        assert!(d.items.iter().find(|i| i.kind == "review").unwrap().title.contains("提现"));
        // “之后”类、不是你的行动项不进你的待办，只计数
        assert_eq!(d.later_actions, 1, "alice 自己的“之后”事项");
        assert_eq!(db.open_action_counts().unwrap().iter().map(|x| x.2).sum::<i64>(), 2, "收件箱只算拦合并的 1 条和指明给人的 1 条");
    }
}
