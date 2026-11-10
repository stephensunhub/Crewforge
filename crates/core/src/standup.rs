//! 站会：同步进度、对齐理解，防止多个工程师之间进度与对接出现混乱。它不做决策。
//!
//! 与讨论会议的区别：议程固定；系统先从数据库给出每个人的事实（任务、分支、改动的文件、计划条目、行动项），
//! 不用模型；每个在做事的成员只发言一次，报告“做完了 / 在做 / 被卡住 / 依赖别人与对接假设”，
//! 后发言的人能看到前面的报告，从而发现理解不一致；发言结束后，系统再做一次确定性的对齐检查
//! （多人改了同一批文件、依赖的条目没完成、被阻塞的条目、悬着的行动项）。发现的问题写进纪要的行动项，由人和负责人决定。

use crate::board::Meeting;
use crate::db::{Employee, Task};
use crate::orchestrator::Orchestrator;
use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::path::Path;

/// 一次站会的材料：每位参与者的事实，以及全员的对齐检查结果。
pub struct StandupFacts {
    pub participants: Vec<Employee>,
    pub per_member: Vec<(String, String)>,
}

impl Orchestrator {
    async fn active_tasks(&self, project_path: &str) -> Result<Vec<Task>> {
        Ok(self.db.list_tasks(project_path)?.into_iter().filter(|t| t.status == "active").collect())
    }

    /// 收集事实：谁在做什么、改了哪些文件、卡在哪里。参与者 = 有进行中的任务或计划条目的成员。
    pub async fn standup_facts(&self, project_id: &str) -> Result<StandupFacts> {
        let project = self.db.get_project(project_id)?.ok_or_else(|| anyhow::anyhow!("项目不存在"))?;
        let tasks = self.active_tasks(&project.path).await?;
        let plan = self.db.plan_view(project_id)?;
        let mut participants = Vec::new();
        let mut per_member = Vec::new();
        for m in self.db.list_members(project_id)? {
            let my_tasks: Vec<&Task> = tasks.iter().filter(|t| t.employee_id == m.employee_id).collect();
            let my_items: Vec<_> = plan.items.iter().filter(|i| i.assignee_id.as_deref() == Some(m.employee_id.as_str()) && matches!(i.status.as_str(), "in_progress" | "blocked")).collect();
            if my_tasks.is_empty() && my_items.is_empty() {
                continue;
            }
            let Some(emp) = self.db.employee_by_id(&m.employee_id)? else { continue };
            let mut s = String::new();
            for t in &my_tasks {
                let files = self.ws.changed_files(Path::new(&t.worktree_path), &t.base_commit).await.unwrap_or_default();
                let shown: Vec<String> = files.iter().take(12).map(|f| f.path.clone()).collect();
                s.push_str(&format!("  - 任务《{}》（分支 {}）：已改动 {} 个文件{}\n", t.title, t.branch, files.len(),
                    if shown.is_empty() { String::new() } else { format!("：{}{}", shown.join("、"), if files.len() > shown.len() { " 等" } else { "" }) }));
                for a in self.db.actions_for_task(&t.id)?.into_iter().filter(|a| a.status == "open" || a.status == "working") {
                    s.push_str(&format!("      · 评审行动项{}：{}\n", if a.blocking { "（拦合并）" } else { "" }, a.text));
                }
            }
            for i in &my_items {
                s.push_str(&format!("  - 计划条目《{}》[{}]{}{}\n", i.title, crate::plan::status_label(&i.status),
                    if i.waiting_for.is_empty() { String::new() } else { format!("，等待：{}", i.waiting_for.join("、")) },
                    if i.note.is_empty() { String::new() } else { format!("，备注：{}", i.note) }));
            }
            participants.push(emp);
            per_member.push((m.name.clone(), s));
        }
        Ok(StandupFacts { participants, per_member })
    }

    /// 开一场站会：创建会议（类型 standup），写入系统事实与议程。返回会议与需要报告的成员（按固定顺序）。
    pub async fn start_standup(&self, project_id: &str) -> Result<(Meeting, Vec<Employee>)> {
        let host = self.db.align_host(project_id)?;
        self.start_standup_hosted(project_id, host.as_deref()).await
    }

    pub async fn start_standup_hosted(&self, project_id: &str, host_id: Option<&str>) -> Result<(Meeting, Vec<Employee>)> {
        let facts = self.standup_facts(project_id).await?;
        if facts.participants.is_empty() {
            bail!("现在没有人在做事（没有进行中的任务或计划条目），不需要对齐会");
        }
        let title = format!("对齐会 {}", chrono_like_now());
        let m = self.db.create_meeting(project_id, "standup", &title, host_id, None, &[], facts.participants.len() as i64 + 2)?;
        let mut body = String::from("【系统记录的现状】（来自任务与计划数据，不是谁的自述）\n");
        for (name, s) in &facts.per_member {
            body.push_str(&format!("{name}：\n{s}"));
        }
        body.push_str("\n议程：每位成员发言一次，固定四项——① 做完了什么 ② 在做什么 ③ 被什么卡住 ④ 依赖别人的什么，以及你对对接接口、文件归属、先后顺序的假设。对齐会只同步和对齐，不做决策；发现不一致就明确指出，由项目负责人会后决定。不要写“需要人类”：被卡住时写清缺什么、你需要谁做什么（例如“未验证：会议里不能执行命令，需在我的工作区运行 go test”），由负责人安排。");
        self.db.post_message(&m.id, "system", None, &body)?;
        Ok((m, facts.participants))
    }

    /// 发言结束后的对齐检查（确定性，不调用模型）：写入会议并返回文本。
    pub async fn standup_alignment_check(&self, meeting_id: &str) -> Result<String> {
        let meeting = self.db.get_meeting(meeting_id)?.ok_or_else(|| anyhow::anyhow!("会议不存在"))?;
        let project = self.db.get_project(&meeting.project_id)?.ok_or_else(|| anyhow::anyhow!("项目不存在"))?;
        let tasks = self.active_tasks(&project.path).await?;
        let mut by_file: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for t in &tasks {
            let who = self.db.employee_by_id(&t.employee_id)?.map(|e| e.name).unwrap_or_default();
            for f in self.ws.changed_files(Path::new(&t.worktree_path), &t.base_commit).await.unwrap_or_default() {
                let tag = format!("{who}《{}》", t.title);
                let e = by_file.entry(f.path).or_default();
                if !e.contains(&tag) { e.push(tag); }
            }
        }
        let mut out = Vec::new();
        for (file, owners) in by_file.iter().filter(|(_, o)| o.len() > 1) {
            out.push(format!("多人改了同一个文件 {file}：{}。合并时可能冲突，请先约定谁改、怎么拆。", owners.join("、")));
        }
        let plan = self.db.plan_view(&meeting.project_id)?;
        for i in plan.items.iter().filter(|i| i.status == "blocked") {
            out.push(format!("计划条目《{}》被阻塞{}", i.title, if i.note.is_empty() { String::new() } else { format!("：{}", i.note) }));
        }
        for i in plan.items.iter().filter(|i| i.status == "in_progress" && !i.waiting_for.is_empty()) {
            out.push(format!("计划条目《{}》在做，但它依赖的《{}》还没完成。", i.title, i.waiting_for.join("》《")));
        }
        for t in &tasks {
            let n = self.db.blocking_actions_for_task(&t.id)?.len();
            if n > 0 {
                out.push(format!("任务《{}》有 {n} 条拦合并的评审行动项还没处理。", t.title));
            }
        }
        let text = if out.is_empty() {
            "对齐检查：没有发现多人改同一文件、被阻塞或依赖未完成的条目。".to_string()
        } else {
            format!("对齐检查发现 {} 个需要对齐的地方：\n{}", out.len(), out.iter().map(|s| format!("- {s}")).collect::<Vec<_>>().join("\n"))
        };
        self.db.post_message(meeting_id, "system", None, &text)?;
        Ok(text)
    }
}

fn chrono_like_now() -> String {
    // 不引入时间库：用本地时间的 date 命令格式化失败时退回时间戳
    std::process::Command::new("date").arg("+%m-%d %H:%M").output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
        .unwrap_or_else(|| crate::db::now_ms().to_string())
}
