//! 负责人处理“标给人”的行动项。
//!
//! 评审和会议的纪要里，AI 常把一些事标成“人类”。但按分工，只有需求变更、高风险操作、架构取舍才需要人拍板；
//! 其余的（界面细节、与需求原文一致与否、实现取舍）应该由负责人定。所以自主推进时，负责人先看一遍：
//! - 能判断的，自己定：记下决定和原因（沉淀为项目决策记忆），需要改代码、测试或文档的，追加计划条目；
//! - 不需要做的，驳回并写明原因；
//! - 属于需求变更、业务方向、资金、生产、不可逆、安全、架构取舍或缺少关键信息的，才留给人，并写明为什么必须是人。

use crate::db::Employee;
use crate::memory::NewMemory;
use crate::orchestrator::Orchestrator;
use crate::plan::NewPlanItem;
use crate::runner::Approver;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

pub const MAX_FOLLOW_UPS: usize = 2;

#[derive(Debug, Clone, PartialEq)]
pub struct FollowUp {
    pub title: String,
    pub description: String,
    pub acceptance: String,
    pub assignee: Option<String>,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ActionRuling {
    Decide { decision: String, reason: String, follow_ups: Vec<FollowUp> },
    Dismiss { reason: String },
    Escalate { reason: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ActionOutcome {
    Decided { decision: String, follow_ups: Vec<String> },
    Dismissed { reason: String },
    Escalated { reason: String },
}

pub fn parse_action_ruling(text: &str) -> Result<ActionRuling> {
    let (a, b) = (text.find('{').context("输出里没有 JSON")?, text.rfind('}').context("输出里没有 JSON")?);
    if b <= a {
        bail!("输出里没有完整的 JSON");
    }
    let v: Value = serde_json::from_str(&text[a..=b]).context("JSON 无法解析")?;
    let s = |x: &Value, k: &str| x[k].as_str().unwrap_or("").trim().to_string();
    let reason = s(&v, "reason");
    match v["action"].as_str() {
        Some("escalate") => {
            if reason.is_empty() { bail!("升级时必须写明原因"); }
            Ok(ActionRuling::Escalate { reason })
        }
        Some("dismiss") => {
            if reason.is_empty() { bail!("驳回时必须写明原因"); }
            Ok(ActionRuling::Dismiss { reason })
        }
        Some("decide") => {
            let decision = s(&v, "decision");
            if decision.is_empty() || reason.is_empty() { bail!("决定必须写明内容和原因"); }
            let mut follow_ups = Vec::new();
            for f in v["follow_up"].as_array().into_iter().flatten() {
                let title = s(f, "title");
                if title.is_empty() { bail!("后续条目必须有标题"); }
                let files = f["files"].as_array().into_iter().flatten().filter_map(|x| x.as_str()).map(|x| x.trim().trim_start_matches("./").to_string()).filter(|x| !x.is_empty()).take(8).collect();
                follow_ups.push(FollowUp { title, description: s(f, "description"), acceptance: s(f, "acceptance"), assignee: Some(s(f, "assignee")).filter(|a| !a.is_empty() && a != "null"), files });
            }
            Ok(ActionRuling::Decide { decision, reason, follow_ups })
        }
        _ => bail!("action 必须是 decide / dismiss / escalate"),
    }
}

impl Orchestrator {
    /// 让负责人处理一条标给人的行动项。已经不是“待处理”或不是标给人的，原样返回 None。
    pub async fn lead_handle_action<A: Approver>(&self, action_id: &str, lead: &Employee, approver: &A) -> Result<Option<ActionOutcome>> {
        let item = self.db.get_action(action_id)?.context("行动项不存在")?;
        if !item.for_human || item.status != "open" {
            return Ok(None);
        }
        let project = self.db.get_project(&item.project_id)?.context("项目不存在")?;
        let meeting = self.db.get_meeting(&item.meeting_id)?;
        let members = self.db.list_members(&project.id)?;
        let plan = self.db.plan_view(&project.id)?;
        let mut p = format!("[处理行动项] 你是项目「{}」的负责人。下面这条行动项在会议纪要里被标成“需要人类处理”，但按分工，只有需求变更、高风险操作、架构取舍才需要人拍板，其余的由你来定。\n", project.name);
        if !project.goal.trim().is_empty() {
            p.push_str(&format!("\n项目目标与需求原文（以它为准）：\n{}\n", project.goal.trim()));
        }
        p.push_str(&format!("\n行动项：{}\n", item.text));
        if let Some(m) = &meeting {
            p.push_str(&format!("来自会议《{}》。纪要：\n{}\n", m.title, m.minutes.chars().take(3000).collect::<String>()));
        }
        p.push_str("\n项目成员：\n");
        for m in &members {
            p.push_str(&format!("- {}（{}，项目角色 {}）\n", m.name, m.title, m.role));
        }
        if !plan.items.is_empty() {
            p.push_str(&format!("\n现有计划：\n{}", crate::planning::plan_table(&plan.items)));
        }
        let memory = self.memory_block(Some(&project.id), lead, None, &item.text, "ruling", action_id);
        if !memory.trim().is_empty() {
            p.push_str(&format!("\n{memory}\n"));
        }
        p.push_str("\n你可以只读地查看代码来核实事实。处理原则：\n\
- 能根据需求原文、现有代码和项目记忆判断的，就自己定（decide），写明依据；需求原文已经写明的，照需求办。\n\
- 决定之后如果需要改代码、补测试或改文档，在 follow_up 里列出条目（最多 2 个，每个写清 description 和能用命令验证的 acceptance，assignee 填项目成员名，files 列出预计会改的文件）；不需要改就让 follow_up 为空。\n\
- 这件事其实不必做（已经符合需求、重复、过度设计），就驳回（dismiss），写明原因。\n\
- 只有涉及需求变更、业务方向、资金、生产、不可逆操作、安全、架构取舍，或缺少你无法自己核实的关键信息时，才升级给人（escalate），并写明为什么必须是人。\n");
        p.push_str("只输出一个 JSON 对象，不要任何其他文字：\n{\"action\":\"decide\",\"decision\":\"最终决定，一句话，具体到做法\",\"reason\":\"依据\",\"follow_up\":[{\"title\":\"...\",\"description\":\"...\",\"acceptance\":\"...\",\"assignee\":\"成员名\",\"files\":[\"路径\"]}]}\n或 {\"action\":\"dismiss\",\"reason\":\"...\"}\n或 {\"action\":\"escalate\",\"reason\":\"为什么必须是人\"}");

        let text = self.lead_turn_as("ruling", lead, &project.id, &p, "处理行动项", approver).await?;
        let ruling = match parse_action_ruling(&text) {
            Ok(r) => r,
            Err(e) => {
                self.db.append_event_for(Some(&project.id), "action_ruling_failed", Some(&lead.id), None, &json!({"action_id": action_id, "error": e.to_string()}))?;
                return Ok(Some(self.escalate_action(&item.id, &project.id, lead, &format!("负责人的回复格式不对（{e}），交给你来定"))?));
            }
        };
        match ruling {
            ActionRuling::Escalate { reason } => Ok(Some(self.escalate_action(&item.id, &project.id, lead, &reason)?)),
            ActionRuling::Dismiss { reason } => {
                self.db.set_action_status(action_id, "dismissed", &format!("负责人（{}）判断不必做：{reason}", lead.name))?;
                Ok(Some(ActionOutcome::Dismissed { reason }))
            }
            ActionRuling::Decide { decision, reason, follow_ups } => {
                // 先建后续条目，全部成功再关闭行动项；某个条目被拒绝（比如负责人不是成员）时仍然记录决定，但写明
                let mut created = Vec::new();
                let mut skipped = Vec::new();
                for f in follow_ups.into_iter().take(MAX_FOLLOW_UPS) {
                    let aid = f.assignee.as_deref().and_then(|n| members.iter().find(|m| m.name == n)).map(|m| m.employee_id.clone());
                    match self.db.create_plan_item(&project.id, &NewPlanItem { title: f.title.clone(), description: f.description, acceptance: f.acceptance, kind: "migrate".into(), module: String::new(), assignee_id: aid, deps: vec![] }, None) {
                        Ok(id) => {
                            let _ = self.db.set_plan_item_files(&id, &f.files);
                            let _ = self.db.set_plan_note(&id, &format!("负责人处理行动项追加：{}", item.text.chars().take(80).collect::<String>()));
                            created.push(f.title);
                        }
                        Err(e) => skipped.push(format!("《{}》：{e}", f.title)),
                    }
                }
                let mut note = format!("负责人（{}）决定：{decision}（依据：{reason}）", lead.name);
                if !created.is_empty() { note.push_str(&format!("；已追加计划条目：{}", created.join("、"))); }
                if !skipped.is_empty() { note.push_str(&format!("；没能追加：{}", skipped.join("；"))); }
                self.db.set_action_status(action_id, "done", &note)?;
                self.db.add_memory(NewMemory {
                    layer: "project".into(), project_id: Some(project.id.clone()), kind: "decision".into(),
                    title: item.text.chars().take(60).collect(), content: format!("决定：{decision}\n原因：{reason}"),
                    source_kind: "issue".into(), source_ref: Some(action_id.into()), created_by: lead.name.clone(), status: "verified".into(), ..Default::default()
                })?;
                self.db.append_event_for(Some(&project.id), "lead_decided_action", Some(&lead.id), None, &json!({"action_id": action_id, "decision": decision, "follow_ups": created,
                    "text": format!("{} 处理了需要人决定的事项：{}", lead.name, decision.chars().take(100).collect::<String>())}))?;
                Ok(Some(ActionOutcome::Decided { decision, follow_ups: created }))
            }
        }
    }

    fn escalate_action(&self, action_id: &str, project_id: &str, lead: &Employee, reason: &str) -> Result<ActionOutcome> {
        self.db.set_action_note(action_id, &format!("负责人（{}）认为必须由你决定：{reason}", lead.name))?;
        self.db.append_event_for(Some(project_id), "lead_escalated_action", Some(&lead.id), None, &json!({"action_id": action_id, "reason": reason,
            "text": format!("{} 没有处理一项标给人的事项，交给你：{}", lead.name, reason.chars().take(120).collect::<String>())}))?;
        Ok(ActionOutcome::Escalated { reason: reason.to_string() })
    }

    /// 负责人已经看过并留给人的行动项（不要再反复问）。
    pub fn action_was_escalated(&self, action_id: &str) -> bool {
        self.db.conn.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM events WHERE kind = 'lead_escalated_action' AND json_extract(payload,'$.action_id') = ?1", [action_id], |r| r.get::<_, i64>(0)).unwrap_or(0) > 0
    }

    /// 标给人、还没处理、负责人也没看过的行动项。
    pub fn human_actions_for_lead(&self, project_id: &str) -> Result<Vec<String>> {
        Ok(self.db.open_actions_for_project(project_id)?.into_iter().filter(|a| a.for_human && a.status == "open" && !self.action_was_escalated(&a.id)).map(|a| a.id).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_three_outcomes_and_rejects_incomplete_ones() {
        let d = parse_action_ruling(r#"好的：{"action":"decide","decision":"回车直接开始","reason":"需求原文写了","follow_up":[{"title":"改 restart","description":"d","acceptance":"cargo test","assignee":"bob","files":["./src/game.rs","src/game.rs"]}]}"#).unwrap();
        let ActionRuling::Decide { follow_ups, .. } = d else { panic!() };
        assert_eq!(follow_ups[0].files, vec!["src/game.rs".to_string(), "src/game.rs".to_string()], "去掉 ./ 前缀");
        assert!(matches!(parse_action_ruling(r#"{"action":"dismiss","reason":"已符合需求"}"#).unwrap(), ActionRuling::Dismiss { .. }));
        assert!(matches!(parse_action_ruling(r#"{"action":"escalate","reason":"涉及需求变更"}"#).unwrap(), ActionRuling::Escalate { .. }));
        assert!(parse_action_ruling(r#"{"action":"escalate"}"#).is_err());
        assert!(parse_action_ruling(r#"{"action":"decide","decision":"x"}"#).is_err());
        assert!(parse_action_ruling("没有 JSON").is_err());
    }
}
