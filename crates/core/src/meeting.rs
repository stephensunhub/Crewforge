//! 会议室：员工被 @ 时才被唤醒，在独立的只读会话里发言。
//! 听到不等于调用模型：没被点名的成员只在事件表里"旁听"。

use crate::board::{Meeting, MeetingMessage};
use crate::db::Employee;
use crate::orchestrator::Orchestrator;
use crate::runner::{Approver, NoopObserver, TurnConfig, run_turn};
use anyhow::{Context, Result, bail};
use std::path::Path;

/// 一次提示里最多带入的新消息与字符数，防止上下文无限增长。
const MAX_NEW_MESSAGES: usize = 30;
const MAX_CONTEXT_CHARS: usize = 12_000;
const MAX_MSG_CHARS: usize = 1_500;
const MAX_DIFF_CHARS: usize = 30_000;
const MAX_TRANSCRIPT_CHARS: usize = 24_000;

pub fn kind_label(kind: &str) -> &'static str {
    match kind {
        "requirements" => "需求评审",
        "design" => "方案评审",
        "code_review" => "Code Review",
        "qa_case" => "QA Case 评审",
        "retro" => "复盘",
        "planning" => "迁移规划",
        _ => "会议",
    }
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…（已截断）", s.chars().take(n).collect::<String>()) }
}

/// 把消息格式化成对话记录，从最新往回装，超出字符预算就丢弃更早的。
pub fn format_messages(msgs: &[MeetingMessage], name_of: &dyn Fn(Option<&str>) -> String, budget: usize) -> String {
    let mut lines = Vec::new();
    let mut used = 0;
    let mut dropped = 0;
    for m in msgs.iter().rev().take(MAX_NEW_MESSAGES * 4) {
        let who = match m.author_kind.as_str() {
            "human" => "人类".to_string(),
            "system" => "系统".to_string(),
            _ => name_of(m.employee_id.as_deref()),
        };
        let line = format!("[{who}] {}", clip(&m.content, MAX_MSG_CHARS));
        if used + line.chars().count() > budget || lines.len() >= MAX_NEW_MESSAGES {
            dropped += 1;
            continue;
        }
        used += line.chars().count();
        lines.push(line);
    }
    dropped += msgs.len().saturating_sub(MAX_NEW_MESSAGES * 4);
    lines.reverse();
    let mut out = lines.join("\n");
    if dropped > 0 {
        out = format!("（更早的 {dropped} 条消息已省略）\n{out}");
    }
    out
}

impl Orchestrator {
    fn name_of(&self, id: Option<&str>) -> String {
        id.and_then(|i| self.db.employee_by_id(i).ok().flatten()).map(|e| e.name).unwrap_or_else(|| "未知".into())
    }

    pub(crate) fn issues_block(&self, meeting: &Meeting, detailed: bool) -> Result<String> {
        let mut out = String::new();
        for iid in self.db.meeting_issue_ids(&meeting.id)? {
            let Some(i) = self.db.get_issue(&iid)? else { continue };
            out.push_str(&format!("- 《{}》（状态：{}）", i.title, i.status));
            if detailed {
                if !i.summary.is_empty() {
                    out.push_str(&format!("：{}", clip(&i.summary, 600)));
                }
                for (n, o) in self.db.issue_options(&iid)?.iter().enumerate() {
                    out.push_str(&format!("\n    方案{}：{}{}", n + 1, o.title, if o.description.is_empty() { String::new() } else { format!(" — {}", clip(&o.description, 300)) }));
                }
            }
            out.push('\n');
        }
        Ok(out)
    }

    /// 组装发给员工的提示。首次发言带完整背景；之后只带新增消息和议题状态，避免重复传递。
    pub async fn build_meeting_prompt(&self, meeting: &Meeting, employee: &Employee) -> Result<String> {
        let project = self.db.get_project(&meeting.project_id)?.context("项目不存在")?;
        let last_seen = self.db.participant_last_seen(&meeting.id, &employee.id)?;
        let first = last_seen.is_none();
        let new_msgs = self.db.meeting_messages(&meeting.id, last_seen.unwrap_or(0))?;
        let transcript = format_messages(&new_msgs, &|id| self.name_of(id), MAX_CONTEXT_CHARS);

        if let Some(g) = self.db.guest_of_meeting(&meeting.id, &employee.id)? {
            if g.active {
                return self.guest_prompt(meeting, employee, first, &transcript);
            }
        }
        let mut p = String::new();
        if first {
            let role = self.db.list_members(&project.id)?.into_iter().find(|m| m.employee_id == employee.id).map(|m| m.role).unwrap_or_default();
            p.push_str(&format!("[会议] 类型：{}；主题：{}\n项目：{}；你在本项目的角色：{}\n", kind_label(&meeting.kind), meeting.title, project.name, role));
            let conv = self.db.company()?.conventions;
            if !conv.trim().is_empty() {
                p.push_str(&format!("\n公司工程规范：\n{}\n", clip(&conv, 1500)));
            }
            if !project.goal.trim().is_empty() {
                p.push_str(&format!("\n项目目标与硬性约束：\n{}\n", clip(project.goal.trim(), 1500)));
            }
            // 规划会议需要事实依据：扫描结果、导入时的基线、两份评估报告，以及现有计划
            if meeting.kind == "planning" {
                p.push_str("\n这是一场规划会议：你的任务是制定计划。先用 list_plan 看现有计划，再用 propose_plan 一次性提交整份计划（条目写清说明和验收标准，用 depends_on 标明先后依赖）。提交后条目会等人类确认。\n");
                if let Some(scan) = self.db.latest_scan(&project.id)? {
                    p.push_str(&format!("\n项目扫描结果：\n{}", crate::onboarding::summarize_scan(&scan)));
                }
                if let Some(b) = self.db.current_baseline(&project.id)? {
                    p.push_str(&format!("\n测试基线：\n{}", crate::onboarding::summarize_baseline(&b)));
                }
                for r in self.db.latest_reports(&project.id)? {
                    p.push_str(&format!("\n{}评估报告（节选）：\n{}\n", if r.kind == "architect" { "全栈" } else { "QA" }, clip(&r.content, 1800)));
                }
                let plan = self.db.plan_view(&project.id)?;
                if !plan.items.is_empty() {
                    p.push_str(&format!("\n现有计划（{} 条）：\n{}", plan.items.len(), plan.items.iter().filter(|i| i.status != "cancelled").map(|i| format!("- [{}] {}\n", crate::plan::status_label(&i.status), i.title)).collect::<String>()));
                }
            }
            let issues = self.issues_block(meeting, true)?;
            if !issues.is_empty() {
                p.push_str(&format!("\n相关议题：\n{issues}"));
            }
            // 与会议主题、议题、最近发言相关的记忆
            let mut q = format!("{} ", meeting.title);
            for iid in self.db.meeting_issue_ids(&meeting.id)? {
                if let Some(i) = self.db.get_issue(&iid)? {
                    q.push_str(&format!("{} {} ", i.title, i.summary));
                }
            }
            for m in new_msgs.iter().rev().take(3) {
                q.push_str(&format!("{} ", m.content));
            }
            let mem = self.memory_block(Some(&project.id), employee, None, &q, "meeting", &meeting.id);
            if !mem.is_empty() {
                p.push_str(&format!("\n{mem}"));
            }
            if let Some(tid) = &meeting.task_id {
                if let Ok(diff) = self.task_diff(tid).await {
                    p.push_str(&format!("\n待评审的改动：\n```diff\n{}\n```\n", clip(&diff, MAX_DIFF_CHARS)));
                }
            }
        } else {
            p.push_str(&format!("[会议继续] {}\n", meeting.title));
            let issues = self.issues_block(meeting, false)?;
            if !issues.is_empty() {
                p.push_str(&format!("议题当前状态：\n{issues}"));
            }
        }
        p.push_str(&format!("\n{}：\n{}\n", if first { "目前的讨论" } else { "自你上次发言以来的新消息" }, if transcript.is_empty() { "（暂无）".into() } else { transcript }));
        p.push_str("\n要求：你是这场会议的参与者，处于只读模式，不要修改任何文件。请直接回应上面的讨论，");
        p.push_str("控制在 200 字以内：同意或反对都要给出理由；有不确定的地方就提问；不要重复别人已经说过的话。");
        if meeting.kind == "code_review" {
            p.push_str("\n这是代码评审：请先基于改动本身独立判断，指出具体的文件和问题，不要只是附和他人。");
        }
        Ok(p)
    }

    /// 员工在会议里发言一次：只读、独立会话，回复写入会议消息。
    /// 返回回复内容。上限与会议状态检查在这里完成。
    pub async fn meeting_reply<A: Approver>(&self, meeting_id: &str, employee: &Employee, approver: &A) -> Result<String> {
        let meeting = self.db.get_meeting(meeting_id)?.context("会议不存在")?;
        if meeting.status != "open" {
            bail!("会议已结束");
        }
        let guest = self.db.guest_of_meeting(meeting_id, &employee.id)?;
        match &guest {
            Some(g) if !g.active => bail!("顾问 {} 已离场或已到期", employee.name),
            Some(_) => {}
            None if !self.db.is_member(&meeting.project_id, &employee.id)? => bail!("{} 不是本项目成员，不能参会", employee.name),
            None => {}
        }
        if self.db.agent_reply_count(meeting_id)? >= meeting.max_agent_replies {
            bail!("本场会议的 AI 发言已达上限（{}）。请由负责人或你来裁决，或结束会议", meeting.max_agent_replies);
        }
        let project = self.db.get_project(&meeting.project_id)?.context("项目不存在")?;
        let prompt = self.build_meeting_prompt(&meeting, employee).await?;
        let seen_up_to = self.db.meeting_messages(meeting_id, 0)?.last().map(|m| m.id).unwrap_or(0);

        let cfg = TurnConfig {
            project_path: Path::new(&project.path).to_path_buf(),
            task_id: None,
            project_id: None, // 不进入项目群聊的事件流，会议内容只在会议里显示
            read_only: true,
            mcp: if guest.is_some() { None } else { self.mcp_launch(&employee.id, &meeting.project_id, &format!("meeting:{meeting_id}")) },
            purpose: "meeting".into(),
            attr_project: Some(meeting.project_id.clone()),
            meeting_id: Some(meeting_id.into()),
            log_prompt: Some(format!("[会议:{}] {}", kind_label(&meeting.kind), meeting.title)),
            program: self.program.clone(),
        };
        let result = run_turn(&self.db, employee, &cfg, &prompt, approver, &NoopObserver).await?;
        if result.is_error || result.text.trim().is_empty() {
            bail!("{} 没有给出有效回复（{}）", employee.name, result.subtype);
        }
        self.db.set_participant_last_seen(meeting_id, &employee.id, seen_up_to)?;
        self.db.post_message(meeting_id, "employee", Some(&employee.id), result.text.trim())?;
        // 自己的回复也算"已听到"，避免下次把自己的话再发一遍
        let own = self.db.meeting_messages(meeting_id, 0)?.last().map(|m| m.id).unwrap_or(seen_up_to);
        self.db.set_participant_last_seen(meeting_id, &employee.id, own)?;
        Ok(result.text)
    }

    /// 让主持人（没有则负责人，再没有则第一位成员）根据会议记录起草纪要。
    pub async fn draft_minutes<A: Approver>(&self, meeting_id: &str, approver: &A) -> Result<String> {
        let meeting = self.db.get_meeting(meeting_id)?.context("会议不存在")?;
        let members = self.db.list_members(&meeting.project_id)?;
        let scribe_id = meeting.host_id.clone()
            .or_else(|| members.iter().find(|m| m.role == "lead").map(|m| m.employee_id.clone()))
            .or_else(|| members.first().map(|m| m.employee_id.clone()))
            .context("项目里没有成员，无法起草纪要。可以直接手写")?;
        let scribe = self.db.employee_by_id(&scribe_id)?.context("主持人不存在")?;
        let msgs = self.db.meeting_messages(meeting_id, 0)?;
        if msgs.is_empty() {
            bail!("会议里还没有任何发言");
        }
        let transcript = format_messages(&msgs, &|id| self.name_of(id), MAX_TRANSCRIPT_CHARS);
        let project = self.db.get_project(&meeting.project_id)?.context("项目不存在")?;
        let prompt = format!(
            "请为下面这场会议整理纪要。\n会议类型：{}；主题：{}\n{}\n会议记录：\n{}\n\n\
             请用 Markdown，严格包含这四节标题：\n## 决议\n## 被否决的方案及原因\n## 行动项\n## 待验证的经验\n\
             “行动项”一节每条单独一行，格式固定为「- [合并前] 负责人：事项」或「- [之后] 负责人：事项」。\
             [合并前]=不解决就不该合并的缺陷，或必须补上的测试；[之后]=可以另开任务跟进的事。\
             “运行测试确认通过”这类例行验证不要列为行动项；需要人类拍板但不影响这个改动正确性的事写成 [之后]。\
             负责人用项目成员的名字（{}），需要人类处理的写“人类”，没有就不写。\n\
             “待验证的经验”只写以后能复用的做法或教训（一条一句，说明适用条件）；当前的状态、未解决的问题、待办事项不是经验，不要写在这里（待办写进行动项）。\n\
             只写会上实际出现过的内容，没有的小节只写一个“无”字（不要再加解释），不要编造；不要修改任何文件。",
            kind_label(&meeting.kind), meeting.title, self.issues_block(&meeting, false)?, transcript,
            members.iter().map(|m| self.name_of(Some(&m.employee_id))).collect::<Vec<_>>().join("、")
        );
        let cfg = TurnConfig {
            project_path: Path::new(&project.path).to_path_buf(),
            task_id: None,
            project_id: None,
            read_only: true,
            mcp: None,
            purpose: "minutes".into(),
            attr_project: Some(meeting.project_id.clone()),
            meeting_id: Some(meeting_id.into()),
            log_prompt: Some(format!("[纪要起草] {}", meeting.title)),
            program: self.program.clone(),
        };
        let r = run_turn(&self.db, &scribe, &cfg, &prompt, approver, &NoopObserver).await?;
        if r.is_error || r.text.trim().is_empty() {
            bail!("纪要起草失败（{}）", r.subtype);
        }
        self.db.set_minutes(meeting_id, r.text.trim())?;
        Ok(r.text)
    }
}
