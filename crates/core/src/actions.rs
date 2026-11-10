//! 评审行动项：把会议纪要“行动项”一节里的每一条变成可跟踪的记录，并和任务的合并挂钩。
//!
//! - 纪要保存时自动同步：新增的条目为“待处理”，改过纪要不会丢掉已有状态，被删掉且还没处理的标为“已撤回”；
//! - 每条带 `[合并前]` / `[之后]` 标签：`[合并前]`（以及没写标签的）会拦住它所评审的那个任务的合并；
//! - 只有人类能把行动项标为已处理或驳回，AI 员工没有这个工具；
//! - 行动项可以直接派给员工处理。对进行中任务的评审意见只能交给该任务的员工，在同一个分支里继续修。

use crate::db::{Db, Employee, now_ms};
use crate::memory::extract_section_items;
use crate::orchestrator::Orchestrator;
use crate::provider::claude::TurnResult;
use crate::runner::{Approver, Observer};
use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use serde_json::json;
use std::path::PathBuf;

pub const MIGRATION_8: &str = r#"
CREATE TABLE action_items (
    id           TEXT PRIMARY KEY,
    project_id   TEXT NOT NULL REFERENCES projects(id),
    meeting_id   TEXT NOT NULL REFERENCES meetings(id),
    task_id      TEXT REFERENCES tasks(id),          -- 评审的是哪个任务（没有则不拦任何合并）
    text         TEXT NOT NULL,
    assignee_id  TEXT REFERENCES employees(id),
    for_human    INTEGER NOT NULL DEFAULT 0,
    blocking     INTEGER NOT NULL,                   -- 是否拦住 task_id 的合并
    status       TEXT NOT NULL DEFAULT 'open',       -- open | working | done | dismissed | dropped
    work_task_id TEXT REFERENCES tasks(id),          -- 派给员工处理时所在的任务
    note         TEXT NOT NULL DEFAULT '',
    created_at   INTEGER NOT NULL,
    resolved_at  INTEGER
);
CREATE INDEX idx_actions_meeting ON action_items(meeting_id);
CREATE INDEX idx_actions_task ON action_items(task_id, status);
CREATE INDEX idx_actions_project ON action_items(project_id, status);
"#;

#[derive(Debug, Clone, Serialize)]
pub struct ActionItem {
    pub id: String,
    pub project_id: String,
    pub meeting_id: String,
    pub meeting_title: String,
    pub task_id: Option<String>,
    pub text: String,
    pub assignee_id: Option<String>,
    pub assignee_name: Option<String>,
    pub for_human: bool,
    pub blocking: bool,
    pub status: String,
    pub work_task_id: Option<String>,
    pub note: String,
    pub created_at: i64,
    pub resolved_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAction {
    pub text: String,
    pub assignee: Option<String>,
    pub for_human: bool,
    pub blocking: bool,
}

/// 解析纪要里的一条行动项：`[合并前] alice：修改 transfer`。标签和负责人都是可选的。
/// 没写标签时按“合并前”处理——宁可多问一句，也不要漏掉。
pub fn parse_action_item(raw: &str, member_names: &[String]) -> ParsedAction {
    let mut s = raw.replace("**", "").trim().to_string();
    let mut blocking = true;
    for (open, close) in [("[", "]"), ("【", "】"), ("(", ")"), ("（", "）")] {
        if let Some(rest) = s.strip_prefix(open) {
            if let Some(end) = rest.find(close) {
                let tag = &rest[..end];
                let tagged = if ["合并前", "阻塞", "必须", "前置"].iter().any(|k| tag.contains(k)) {
                    Some(true)
                } else if ["之后", "后续", "合并后", "可选", "跟进"].iter().any(|k| tag.contains(k)) {
                    Some(false)
                } else {
                    None
                };
                if let Some(b) = tagged {
                    blocking = b;
                    s = rest[end + close.len()..].trim_start().to_string();
                    break;
                }
            }
        }
    }
    let (mut assignee, mut for_human) = (None, false);
    if let Some(i) = s.find(['：', ':']) {
        let head = s[..i].trim();
        if head.chars().count() <= 20 {
            if head == "人类" || head == "我" {
                for_human = true;
                s = s[i..].trim_start_matches(['：', ':']).trim().to_string();
            } else if member_names.iter().any(|n| n == head) {
                assignee = Some(head.to_string());
                s = s[i..].trim_start_matches(['：', ':']).trim().to_string();
            }
        }
    }
    ParsedAction { text: s, assignee, for_human, blocking }
}

const SELECT: &str = "SELECT a.id, a.project_id, a.meeting_id, m.title, a.task_id, a.text, a.assignee_id, e.name, a.for_human, a.blocking,
    a.status, a.work_task_id, a.note, a.created_at, a.resolved_at
    FROM action_items a JOIN meetings m ON m.id = a.meeting_id LEFT JOIN employees e ON e.id = a.assignee_id";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ActionItem> {
    Ok(ActionItem {
        id: r.get(0)?, project_id: r.get(1)?, meeting_id: r.get(2)?, meeting_title: r.get(3)?, task_id: r.get(4)?, text: r.get(5)?,
        assignee_id: r.get(6)?, assignee_name: r.get(7)?, for_human: r.get::<_, i64>(8)? != 0, blocking: r.get::<_, i64>(9)? != 0,
        status: r.get(10)?, work_task_id: r.get(11)?, note: r.get(12)?, created_at: r.get(13)?, resolved_at: r.get(14)?,
    })
}

impl Db {
    fn query_actions(&self, filter: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<ActionItem>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!("{SELECT} {filter} ORDER BY a.created_at, a.rowid"))?;
        let rows = stmt.query_map(args, row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn actions_for_meeting(&self, meeting_id: &str) -> Result<Vec<ActionItem>> {
        self.query_actions("WHERE a.meeting_id = ?1", &[&meeting_id])
    }

    /// 项目里全部行动项（含已处理、已驳回；不含从纪要里撤回的），新的在前。
    pub fn actions_for_project(&self, project_id: &str) -> Result<Vec<ActionItem>> {
        let mut v = self.query_actions("WHERE a.project_id = ?1 AND a.status != 'dropped'", &[&project_id])?;
        v.reverse();
        Ok(v)
    }

    pub fn actions_for_task(&self, task_id: &str) -> Result<Vec<ActionItem>> {
        self.query_actions("WHERE a.task_id = ?1 AND a.status != 'dropped'", &[&task_id])
    }

    pub fn get_action(&self, id: &str) -> Result<Option<ActionItem>> {
        Ok(self.query_actions("WHERE a.id = ?1", &[&id])?.into_iter().next())
    }

    /// 会让这个任务不能顺利合并的行动项：标为合并前、还没处理完（待处理或处理中）。
    pub fn blocking_actions_for_task(&self, task_id: &str) -> Result<Vec<ActionItem>> {
        self.query_actions("WHERE a.task_id = ?1 AND a.blocking = 1 AND a.status IN ('open','working')", &[&task_id])
    }

    /// 各项目还没处理完的行动项数量（待办收件箱使用）。
    pub fn open_action_counts(&self) -> Result<Vec<(String, String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT p.id, p.name, COUNT(*) FROM action_items a JOIN projects p ON p.id = a.project_id
             WHERE a.status IN ('open','working') AND p.status = 'active'
               AND (a.for_human = 1 OR (a.blocking = 1 AND a.task_id IN (SELECT id FROM tasks WHERE status = 'active')))
             GROUP BY p.id, p.name ORDER BY p.name",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// 按纪要同步行动项：新增的加入；仍在纪要里的保持原状态；从纪要里删掉且还没开始处理的标为“已撤回”。
    pub(crate) fn sync_action_items(&self, meeting_id: &str) -> Result<()> {
        let Some(m) = self.get_meeting(meeting_id)? else { return Ok(()) };
        let members = self.list_members(&m.project_id)?;
        let names: Vec<String> = members.iter().map(|x| x.name.clone()).collect();
        let parsed: Vec<ParsedAction> = extract_section_items(&m.minutes, "行动项").iter().map(|raw| parse_action_item(raw, &names)).collect();
        let existing = self.actions_for_meeting(meeting_id)?;
        let now = now_ms();
        for p in &parsed {
            if existing.iter().any(|e| e.text == p.text && e.status != "dropped") {
                continue;
            }
            let assignee_id = p.assignee.as_ref().and_then(|n| members.iter().find(|x| &x.name == n)).map(|x| x.employee_id.clone());
            self.conn.lock().unwrap().execute(
                "INSERT INTO action_items (id, project_id, meeting_id, task_id, text, assignee_id, for_human, blocking, created_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![uuid::Uuid::new_v4().to_string(), m.project_id, meeting_id, m.task_id, p.text, assignee_id, p.for_human as i64,
                        // 只有评审某个任务的会议，行动项才有“合并前”的含义
                        (p.blocking && m.task_id.is_some()) as i64, now],
            )?;
        }
        for e in existing.iter().filter(|e| e.status == "open" && !parsed.iter().any(|p| p.text == e.text)) {
            self.conn.lock().unwrap().execute("UPDATE action_items SET status = 'dropped', resolved_at = ?2 WHERE id = ?1", params![e.id, now])?;
        }
        Ok(())
    }

    /// 人类处理行动项：done（已处理）、dismissed（驳回，必须写原因）、open（重新打开）。
    pub fn set_action_status(&self, id: &str, status: &str, note: &str) -> Result<()> {
        let a = self.get_action(id)?.context("行动项不存在")?;
        if a.status == "dropped" {
            bail!("这条行动项已经从纪要里撤回了");
        }
        match status {
            "done" | "open" => {}
            "dismissed" if note.trim().is_empty() => bail!("驳回行动项请写明原因"),
            "dismissed" => {}
            other => bail!("未知的状态: {other}"),
        }
        let resolved = (status != "open").then(now_ms);
        self.conn.lock().unwrap().execute(
            "UPDATE action_items SET status = ?2, note = CASE WHEN ?3 = '' THEN note ELSE ?3 END, resolved_at = ?4 WHERE id = ?1",
            params![id, status, note.trim(), resolved],
        )?;
        self.append_event_for(Some(&a.project_id), "action_item_status", None, None, &json!({"action_id": id, "text": a.text, "to": status, "task_id": a.task_id}))?;
        Ok(())
    }

    /// 只写备注（比如负责人说明为什么留给人），不改状态。
    pub fn set_action_note(&self, id: &str, note: &str) -> Result<()> {
        self.conn.lock().unwrap().execute("UPDATE action_items SET note = ?2 WHERE id = ?1", params![id, note.trim()])?;
        Ok(())
    }

    /// 行动项被派给员工、并开始在某个任务里处理。
    pub fn mark_action_working(&self, id: &str, task_id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE action_items SET status = 'working', work_task_id = ?2 WHERE id = ?1 AND status IN ('open','working')",
            params![id, task_id],
        )?;
        Ok(())
    }

    pub fn task_title(&self, id: &str) -> Result<Option<String>> {
        Ok(self.conn.lock().unwrap().query_row("SELECT title FROM tasks WHERE id = ?1", [id], |r| r.get(0)).optional()?)
    }
}

impl Orchestrator {
    /// 校验并准备把行动项派给员工：返回 (员工, 项目目录, 提示)。
    pub fn prepare_action_dispatch(&self, item_id: &str, employee_override: Option<&str>) -> Result<(Employee, PathBuf, String)> {
        let item = self.db.get_action(item_id)?.context("行动项不存在")?;
        if !matches!(item.status.as_str(), "open" | "working") {
            bail!("这条行动项已经是「{}」，不需要再派了", status_label(&item.status));
        }
        let project = self.db.get_project(&item.project_id)?.context("项目不存在")?;
        let emp_id = employee_override.map(String::from).or(item.assignee_id.clone()).context("这条行动项没有指定负责人，请先选择由谁处理")?;
        let emp = self.db.employee_by_id(&emp_id)?.context("员工不存在")?;
        if emp.archived || !self.db.is_member(&project.id, &emp.id)? {
            bail!("{} 不是本项目的在职成员", emp.name);
        }
        // 对进行中任务的评审意见，必须在同一个任务（同一个分支）里改，否则修复会落在别处，评审的那份代码并没有变
        if let Some(tid) = &item.task_id {
            if let Some(task) = self.db.get_task(tid)? {
                if task.status == "active" && task.employee_id != emp.id {
                    let owner = self.db.employee_by_id(&task.employee_id)?.map(|e| e.name).unwrap_or_default();
                    bail!("这是对 {owner} 的任务的评审意见，必须由 {owner} 在同一个任务里处理；其他人请另开任务");
                }
            }
        }
        let prompt = format!(
            "评审行动项：{}\n（来自评审会议《{}》。请只处理这一项，改完运行测试确认，并简要说明做了什么。）",
            item.text, item.meeting_title
        );
        Ok((emp, PathBuf::from(project.path), prompt))
    }

    /// 以行动项的名义派活：继续员工进行中的任务（或新开一个），并把行动项标为“处理中”。
    pub async fn assign_for_action<A: Approver, O: Observer>(
        &self, employee: &Employee, project_path: &std::path::Path, prompt: &str, item_id: &str, approver: &A, observer: &O,
    ) -> Result<TurnResult> {
        self.assign_linked(employee, project_path, prompt, Some(crate::orchestrator::TaskLink::Action(item_id)), approver, observer).await
    }

    pub async fn dispatch_action_item<A: Approver, O: Observer>(
        &self, item_id: &str, employee_override: Option<&str>, approver: &A, observer: &O,
    ) -> Result<TurnResult> {
        let (emp, path, prompt) = self.prepare_action_dispatch(item_id, employee_override)?;
        self.assign_for_action(&emp, &path, &prompt, item_id, approver, observer).await
    }
}

pub fn status_label(s: &str) -> &'static str {
    match s { "open" => "待处理", "working" => "处理中", "done" => "已处理", "dismissed" => "已驳回", "dropped" => "已撤回", _ => "?" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> { vec!["alice".into(), "bob".into()] }

    fn setup() -> (Db, String, String, String, String) {
        let db = Db::open_memory().unwrap();
        let alice = db.add_employee("alice", "全栈", "p").unwrap();
        let bob = db.add_employee("bob", "全栈", "p").unwrap();
        let p = db.create_project("P", "", "/r").unwrap();
        db.add_member(&p.id, &alice.id, "lead").unwrap();
        db.add_member(&p.id, &bob.id, "developer").unwrap();
        db.create_task(&crate::db::Task { id: "t1".into(), repo_path: "/r".into(), employee_id: alice.id.clone(), title: "transfer".into(), branch: "b".into(),
            base_branch: "main".into(), base_commit: "c".into(), worktree_path: "/w".into(), status: "active".into(), merged_commit: None, created_at: 1, updated_at: 1 }).unwrap();
        (db, p.id, alice.id, bob.id, "t1".into())
    }
    const MINUTES: &str = "## 决议\n- 修溢出\n## 行动项\n- [合并前] alice：把 to.balance += amount 改成 checked_add\n- [之后] bob：单开 PR 修透支\n- 补一个边界测试\n- [后续] 人类：确认议题\n## 待验证的经验\n无";

    #[test]
    fn minutes_are_synced_into_trackable_items_with_blocking_only_for_reviewed_tasks() {
        let (db, pid, alice, bob, t1) = setup();
        let review = db.create_meeting(&pid, "code_review", "评审", None, Some(&t1), &[], 5).unwrap();
        db.set_minutes(&review.id, MINUTES).unwrap();
        let items = db.actions_for_meeting(&review.id).unwrap();
        assert_eq!(items.len(), 4);
        let by = |t: &str| items.iter().find(|a| a.text.contains(t)).unwrap();
        assert_eq!((by("checked_add").assignee_id.as_deref(), by("checked_add").blocking, by("checked_add").status.as_str()), (Some(alice.as_str()), true, "open"));
        assert_eq!((by("透支").assignee_id.as_deref(), by("透支").blocking), (Some(bob.as_str()), false), "[之后] 不拦合并");
        assert!(by("边界测试").blocking && by("边界测试").assignee_id.is_none(), "没写标签：按合并前处理，没写负责人：无");
        assert!(by("确认议题").for_human && !by("确认议题").blocking);
        assert_eq!(db.blocking_actions_for_task(&t1).unwrap().len(), 2, "只有两条会拦合并");
        assert!(db.actions_for_task(&t1).unwrap().len() == 4);

        // 没有评审对象的会议：行动项永远不拦任何合并
        let retro = db.create_meeting(&pid, "retro", "复盘", None, None, &[], 5).unwrap();
        db.set_minutes(&retro.id, "## 行动项\n- [合并前] alice：做点事").unwrap();
        let r = db.actions_for_meeting(&retro.id).unwrap();
        assert_eq!((r.len(), r[0].blocking, r[0].task_id.clone()), (1, false, None));
        assert_eq!(db.blocking_actions_for_task(&t1).unwrap().len(), 2);
    }

    #[test]
    fn editing_minutes_keeps_progress_and_retracts_removed_open_items() {
        let (db, pid, _a, _b, t1) = setup();
        let m = db.create_meeting(&pid, "code_review", "评审", None, Some(&t1), &[], 5).unwrap();
        db.set_minutes(&m.id, MINUTES).unwrap();
        let id_of = |db: &Db, t: &str| db.actions_for_meeting(&m.id).unwrap().into_iter().find(|a| a.text.contains(t)).unwrap().id;
        db.set_action_status(&id_of(&db, "checked_add"), "done", "").unwrap();
        // 重复保存同样的纪要：不产生重复，状态不变
        db.set_minutes(&m.id, MINUTES).unwrap();
        assert_eq!(db.actions_for_meeting(&m.id).unwrap().len(), 4);
        assert_eq!(db.get_action(&id_of(&db, "checked_add")).unwrap().unwrap().status, "done");
        // 删掉一条已处理的、一条未处理的，再新增一条
        db.set_minutes(&m.id, "## 行动项\n- [之后] bob：单开 PR 修透支\n- [合并前] alice：补文档").unwrap();
        let all = db.actions_for_meeting(&m.id).unwrap();
        let st = |t: &str| all.iter().find(|a| a.text.contains(t)).unwrap().status.clone();
        assert_eq!(st("checked_add"), "done", "已处理的不会因为纪要改动而被撤回");
        assert_eq!(st("边界测试"), "dropped", "未处理且被删掉的标为已撤回");
        assert_eq!(st("补文档"), "open");
        assert!(db.set_action_status(&id_of(&db, "边界测试"), "done", "").is_err(), "已撤回的不能再操作");
        assert_eq!(db.actions_for_task(&t1).unwrap().iter().filter(|a| a.status == "dropped").count(), 0, "撤回的不在任务视图里");
        assert_eq!(db.blocking_actions_for_task(&t1).unwrap().len(), 1, "只剩“补文档”拦合并");
    }

    #[test]
    fn only_unresolved_blocking_items_block_and_dismissal_needs_a_reason() {
        let (db, pid, _a, _b, t1) = setup();
        let m = db.create_meeting(&pid, "code_review", "评审", None, Some(&t1), &[], 5).unwrap();
        db.set_minutes(&m.id, "## 行动项\n- [合并前] alice：甲\n- [合并前] alice：乙\n- [合并前] alice：丙").unwrap();
        let ids: Vec<String> = db.actions_for_meeting(&m.id).unwrap().into_iter().map(|a| a.id).collect();
        assert!(db.set_action_status(&ids[0], "dismissed", "  ").is_err(), "驳回必须写原因");
        assert!(db.set_action_status(&ids[0], "bogus", "x").is_err());
        db.set_action_status(&ids[0], "dismissed", "不是问题，借用规则已经挡住").unwrap();
        db.mark_action_working(&ids[1], &t1).unwrap();
        assert_eq!(db.get_action(&ids[1]).unwrap().unwrap().work_task_id.as_deref(), Some(t1.as_str()));
        assert_eq!(db.blocking_actions_for_task(&t1).unwrap().len(), 2, "处理中仍然拦合并，只有人确认已处理才放行");
        db.set_action_status(&ids[1], "done", "").unwrap();
        assert_eq!(db.blocking_actions_for_task(&t1).unwrap().len(), 1);
        db.set_action_status(&ids[1], "open", "").unwrap(); // 重新打开
        assert_eq!(db.blocking_actions_for_task(&t1).unwrap().len(), 2);
        assert_eq!(db.get_action(&ids[0]).unwrap().unwrap().note, "不是问题，借用规则已经挡住", "驳回原因被记录");
        assert_eq!(db.open_action_counts().unwrap(), vec![(pid, "P".to_string(), 2)]);
    }

    #[test]
    fn parses_tags_assignee_and_markdown() {
        let p = parse_action_item("**alice**:修改 `transfer`,先 checked_add 再扣款", &names());
        assert_eq!((p.assignee.as_deref(), p.blocking, p.for_human), (Some("alice"), true, false), "没写标签：按合并前处理");
        assert_eq!(p.text, "修改 `transfer`,先 checked_add 再扣款", "行内代码保留");
        let p = parse_action_item("[合并前] alice：补溢出测试", &names());
        assert_eq!((p.assignee.as_deref(), p.blocking, p.text.as_str()), (Some("alice"), true, "补溢出测试"));
        let p = parse_action_item("【之后】bob: 单开 PR 修透支", &names());
        assert_eq!((p.assignee.as_deref(), p.blocking, p.text.as_str()), (Some("bob"), false, "单开 PR 修透支"));
        let p = parse_action_item("[后续] 人类：确认议题", &names());
        assert_eq!((p.for_human, p.blocking, p.assignee), (true, false, None));
        // 不是成员的名字不当负责人，文字保持原样
        let p = parse_action_item("待确认(未指定负责人)：是否有外部使用方", &names());
        assert_eq!((p.assignee, p.for_human, p.text.as_str()), (None, false, "待确认(未指定负责人)：是否有外部使用方"));
        // 方括号不是标签（比如引用代码）时不吞掉
        let p = parse_action_item("[i64::MAX] 的情况也要测", &names());
        assert_eq!((p.blocking, p.text.as_str()), (true, "[i64::MAX] 的情况也要测"));
        let p = parse_action_item("普通一句话", &names());
        assert_eq!((p.assignee, p.blocking), (None, true));
    }
}

impl Db {
    /// 把拦合并的行动项降级为“之后再说”：仍然开着、仍被跟踪，只是不再拦合并，也不进人的待办。
    pub fn downgrade_action(&self, id: &str, note: &str) -> Result<()> {
        let n = self.conn.lock().unwrap().execute("UPDATE action_items SET blocking = 0, note = ?2 WHERE id = ?1 AND status = 'open'", params![id, note])?;
        if n == 0 { anyhow::bail!("行动项不存在或已不是待处理"); }
        Ok(())
    }

    /// 项目里最近已处理的行动项文本（负责人用来识别重复的评审意见）。
    pub fn recent_done_action_texts(&self, project_id: &str, limit: usize) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut s = conn.prepare("SELECT text FROM action_items WHERE project_id = ?1 AND status = 'done' ORDER BY resolved_at DESC, created_at DESC LIMIT ?2")?;
        Ok(s.query_map(params![project_id, limit as i64], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?)
    }

    /// 项目里还没处理完的行动项（文档“已知问题”用）。
    pub fn open_actions_for_project(&self, project_id: &str) -> Result<Vec<ActionItem>> {
        self.query_actions("WHERE a.project_id = ?1 AND a.status IN ('open','working')", &[&project_id])
    }
}
