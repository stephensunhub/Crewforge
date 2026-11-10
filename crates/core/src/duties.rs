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
    /// 要去处理的会议（行动项所在的会议）；没有则只跳到页签
    pub meeting_id: Option<String>,
    /// 具体是哪一条行动项（打开会议后定位到它）
    pub action_id: Option<String>,
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
            items.push(DutyItem { kind: "plan".into(), title: format!("AI 起草了 {} 个计划条目", plan.counts.proposed), reason: "计划要你批准后才会派活，批准之前员工不会开始做".into(), goto: "plan".into(), count: plan.counts.proposed, meeting_id: None, action_id: None });
        }
        for d in self.pending_decisions()?.into_iter().filter(|d| d.issue.project_id == project_id) {
            let only_human = d.issue.high_risk || d.issue.kind == "requirement";
            // 负责人在自主推进里没有裁决、升级给人的议题：带上负责人写的原因
            let lead_reason: Option<String> = self.conn.lock().unwrap().query_row(
                "SELECT json_extract(payload,'$.reason') FROM events WHERE kind = 'autopilot_issue_escalated' AND json_extract(payload,'$.issue_id') = ?1 ORDER BY id DESC LIMIT 1",
                params![d.issue.id], |r| r.get(0)).ok().flatten();
            items.push(DutyItem { kind: "decision".into(), title: format!("议题待拍板：{}", d.issue.title),
                reason: if let Some(r) = lead_reason { format!("负责人没有裁决，交给你：{r}") }
                    else if only_human { "需求类或高风险议题，只有你能拍板（AI 没有拍板权限）".into() }
                    else { "讨论已结束，等有权限者拍板；你始终可以拍板".into() }, goto: "issues".into(), count: 1, meeting_id: None, action_id: None });
        }
        for d in self.ai_proposals()?.into_iter().filter(|d| d.issue.project_id == project_id) {
            items.push(DutyItem { kind: "proposal".into(), title: format!("员工提议了议题：{}", d.issue.title), reason: "AI 只能提议；是否值得讨论由你确认，确认前不会占用大家的额度".into(), goto: "issues".into(), count: 1, meeting_id: None, action_id: None });
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
                items.push(DutyItem { kind: "review".into(), title: format!("《{title}》有 {blocking} 条拦合并的评审意见"), reason: "员工改完不等于解决：处理中的意见仍拦着合并，需要你确认已解决（或写明原因后放行）".into(), goto: "meetings".into(), count: blocking as usize, meeting_id: None, action_id: None });
            } else if reviewed > 0 {
                items.push(DutyItem { kind: "merge".into(), title: format!("《{title}》评审已结束，等你确认合并"), reason: "合并到主分支由项目负责人最终确认；负责人是你，所以由你来点合并".into(), goto: "tasks".into(), count: 1, meeting_id: None, action_id: None });
            }
        }
        // 明确需要你决定的行动项：每一条单独列出，并指向它所在的会议（不然只知道“有 2 件事”，却不知道去哪里找）
        let mine: Vec<(String, String, String, String)> = {
            let mut s = conn.prepare("SELECT a.text, a.meeting_id, m.title, a.id FROM action_items a JOIN meetings m ON m.id = a.meeting_id
                WHERE a.project_id = ?1 AND a.for_human = 1 AND a.status IN ('open','working')
                  AND (NOT EXISTS (SELECT 1 FROM project_members pm WHERE pm.project_id = a.project_id AND pm.role = 'lead')
                       OR EXISTS (SELECT 1 FROM events ev WHERE ev.kind = 'lead_escalated_action' AND json_extract(ev.payload,'$.action_id') = a.id))
                ORDER BY a.created_at")?;
            s.query_map(params![project_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<std::result::Result<_, _>>()?
        };
        for (text, meeting_id, meeting_title, action_id) in mine {
            let shown: String = text.chars().take(120).collect();
            items.push(DutyItem { kind: "action".into(), title: format!("{}{}", shown, if text.chars().count() > 120 { "…" } else { "" }),
                reason: format!("会议《{meeting_title}》的纪要里有一件待决定的事，而这个项目的负责人是你（或负责人已经看过并留给你）；只有人能关闭行动项。点“去处理”会打开那场会议，在纪要下方的行动项里点“已处理”（或写明原因后“驳回”）"),
                goto: "meetings".into(), count: 1, meeting_id: Some(meeting_id), action_id: Some(action_id) });
        }
        let unverified: i64 = conn.query_row("SELECT COUNT(*) FROM memories WHERE project_id = ?1 AND status = 'unverified'", params![project_id], |r| r.get(0))?;
        if unverified > 0 {
            items.push(DutyItem { kind: "memory".into(), title: format!("{unverified} 条 AI 总结的经验等你验证"), reason: "没验证的经验不会注入给员工；你确认它真的有用，员工才会用上".into(), goto: "memory".into(), count: unverified as usize, meeting_id: None, action_id: None });
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
        let esc = db.create_issue(&p.id, "要不要升级数据库版本", "", "technical", false, Some(&alice.id), None, None).unwrap();
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

        // 负责人升级给人的技术议题：待办里带上负责人的原因
        db.set_issue_status(&esc.id, "discussing").unwrap();
        db.set_issue_status(&esc.id, "pending_decision").unwrap();
        db.append_event_for(Some(&p.id), "autopilot_issue_escalated", None, None, &serde_json::json!({"issue_id": esc.id, "reason": "涉及线上数据，依据不足"})).unwrap();
        let d = db.my_duties(&p.id).unwrap();
        assert!(d.items.iter().any(|i| i.kind == "decision" && i.reason.contains("负责人没有裁决") && i.reason.contains("涉及线上数据")), "{:?}", d.items);
        let kinds: Vec<&str> = d.items.iter().map(|i| i.kind.as_str()).collect();
        for k in ["plan", "decision", "proposal", "merge", "review"] {
            assert!(kinds.contains(&k), "缺少 {k}：{kinds:?}");
        }
        // 每一项都说明为什么轮到你，并指向页签
        assert!(d.items.iter().all(|i| !i.reason.is_empty() && !i.goto.is_empty()));
        assert!(d.items.iter().find(|i| i.kind == "decision").unwrap().reason.contains("只有你能拍板"));
        assert!(d.items.iter().find(|i| i.kind == "merge").unwrap().title.contains("转账"));
        assert!(d.items.iter().find(|i| i.kind == "review").unwrap().title.contains("提现"));
        // 负责人是 AI：会议里“人类”的决定归负责人处理，不进你的待办；“之后”类只计数
        assert!(!kinds.contains(&"action"), "AI 负责人在任时，行动项里的决定不进人的待办：{kinds:?}");
        assert_eq!(d.later_actions, 1, "alice 自己的“之后”事项");
        assert_eq!(db.open_action_counts().unwrap().iter().map(|x| x.2).sum::<i64>(), 1, "收件箱只算拦合并的 1 条");
        // 负责人看过之后留给人的，才进你的待办
        let a = db.actions_for_meeting(&m2.id).unwrap().into_iter().find(|a| a.for_human).unwrap();
        db.append_event_for(Some(&p.id), "lead_escalated_action", None, None, &serde_json::json!({"action_id": a.id, "reason": "缺少关键事实"})).unwrap();
        assert!(db.my_duties(&p.id).unwrap().items.iter().any(|i| i.kind == "action"));
        assert_eq!(db.open_action_counts().unwrap().iter().map(|x| x.2).sum::<i64>(), 2);
    }

    #[test]
    fn each_human_action_is_listed_separately_with_the_meeting_to_open() {
        let db = Db::open_memory().unwrap();
        let p = db.create_project("P", "", "/r").unwrap();
        let m = db.create_meeting(&p.id, "retro", "复盘会", None, None, &[], 5).unwrap();
        db.set_minutes(&m.id, "## 行动项\n- [后续] 人类：决定是否区分胜负\n- [后续] 人类：决定回车是否直接开始\n- [之后] 某人：不是你的事").unwrap();
        let d = db.my_duties(&p.id).unwrap();
        let actions: Vec<_> = d.items.iter().filter(|i| i.kind == "action").collect();
        assert_eq!(actions.len(), 2, "每条单独列出，而不是“2 件事”");
        assert!(actions.iter().all(|a| a.meeting_id.as_deref() == Some(m.id.as_str()) && a.reason.contains("复盘会")));
        assert!(actions.iter().any(|a| a.title.contains("区分胜负")) && actions.iter().any(|a| a.title.contains("回车")));
        assert_eq!(d.items.iter().map(|i| i.count).sum::<usize>(), 2, "和小铃铛的数字一致");
    }
}
