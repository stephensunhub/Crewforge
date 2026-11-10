//! 负责人裁决技术议题：自主推进时，AI 负责人对一般技术类议题自己拍板，不再等人。
//!
//! 权限沿用议题的拍板规则（`board::check_authority`）：需求类、高风险议题只有人类能拍板；
//! 一般技术方案、流程、风险、Bug 归因由项目负责人拍板；代码细节归议题负责人。
//! 负责人拿到议题、候选方案、相关讨论和项目记忆，只能输出一个结构化的裁决——选哪个方案或自己的方案、
//! 原因、没选的方案为什么没选；缺依据、涉及需求变更/资金/生产/不可逆/安全时必须升级给人。
//! 裁决会沉淀为项目决策记忆（已验证），之后的任务会自动带上它。

use crate::board::{Actor, Issue, IssueOption};
use crate::db::Employee;
use crate::meeting::format_messages;
use crate::orchestrator::Orchestrator;
use crate::runner::{Approver, NoopObserver, TurnConfig, run_turn};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;

/// 负责人的裁决（解析自模型输出）。
#[derive(Debug, Clone, PartialEq)]
pub enum Ruling {
    Decide {
        /// 选了第几个候选方案（从 1 开始）；None 表示采用负责人自己的方案
        option: Option<usize>,
        decision: String,
        reason: String,
        /// 候选方案编号 → 没采纳的原因
        rejected: HashMap<usize, String>,
    },
    Escalate { reason: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum RulingOutcome {
    Decided { decision: String, reason: String },
    Escalated { reason: String },
    /// 负责人无权拍板或不适合由负责人处理，保持原样交给人
    NotForLead { why: String },
}

/// 从模型输出里取出 JSON 并解析。格式不对就报错（调用方按“升级给人”处理）。
pub fn parse_ruling(text: &str, option_count: usize) -> Result<Ruling> {
    let (a, b) = (text.find('{').context("输出里没有 JSON")?, text.rfind('}').context("输出里没有 JSON")?);
    if b <= a {
        bail!("输出里没有完整的 JSON");
    }
    let v: Value = serde_json::from_str(&text[a..=b]).context("JSON 无法解析")?;
    let s = |k: &str| v[k].as_str().unwrap_or("").trim().to_string();
    match v["action"].as_str() {
        Some("escalate") => {
            let reason = s("reason");
            if reason.is_empty() {
                bail!("升级时必须写明原因");
            }
            Ok(Ruling::Escalate { reason })
        }
        Some("decide") => {
            let option = match v["option"].as_u64() {
                Some(0) | None => None,
                Some(n) if (n as usize) <= option_count => Some(n as usize),
                Some(n) => bail!("方案编号 {n} 超出范围（共 {option_count} 个）"),
            };
            let (decision, reason) = (s("decision"), s("reason"));
            if decision.is_empty() || reason.is_empty() {
                bail!("裁决必须写明最终决定和原因");
            }
            let mut rejected = HashMap::new();
            if let Some(m) = v["rejected"].as_object() {
                for (k, why) in m {
                    if let (Ok(n), Some(w)) = (k.parse::<usize>(), why.as_str()) {
                        rejected.insert(n, w.trim().to_string());
                    }
                }
            }
            Ok(Ruling::Decide { option, decision, reason, rejected })
        }
        _ => bail!("action 必须是 decide 或 escalate"),
    }
}

fn prompt_for(issue: &Issue, options: &[IssueOption], discussion: &str, goal: &str, memory: &str) -> String {
    let mut p = String::from("[负责人裁决] 你是这个项目的负责人，现在要对下面的技术议题做出裁决。\n");
    if !goal.trim().is_empty() {
        p.push_str(&format!("\n项目目标与硬性约束：\n{}\n", goal.trim()));
    }
    p.push_str(&format!("\n议题：《{}》（类型：{}）\n{}\n", issue.title, issue.kind, if issue.summary.trim().is_empty() { "（没有补充说明）" } else { issue.summary.trim() }));
    if options.is_empty() {
        p.push_str("\n候选方案：（还没有人提过方案——请你自己给出方案）\n");
    } else {
        p.push_str("\n候选方案：\n");
        for (i, o) in options.iter().enumerate() {
            p.push_str(&format!("{}. {}{}\n", i + 1, o.title, if o.description.trim().is_empty() { String::new() } else { format!(" — {}", o.description.trim()) }));
        }
    }
    if !discussion.trim().is_empty() {
        p.push_str(&format!("\n相关讨论：\n{discussion}\n"));
    }
    if !memory.trim().is_empty() {
        p.push_str(&format!("\n{memory}\n"));
    }
    p.push_str("\n你可以只读地查看代码来核实事实。你是这个项目的最终决策人：议题都由你来定，包括需求取舍和高风险事项（高风险的操作本身仍会在执行时经过审批），人只在事后观察并可以纠正。以需求原文为准，给出具体依据；只有缺少你无法核实、也无法合理假设的关键事实时，才升级给人，并写明缺什么。\n");
    p.push_str("只输出一个 JSON 对象，不要任何其他文字：\n");
    p.push_str("{\"action\":\"decide\",\"option\":<采用的方案编号；用你自己的方案写 0>,\"decision\":\"最终决定，一句话，具体到文件/接口/做法\",\"reason\":\"原因，写出依据\",\"rejected\":{\"<没采纳的方案编号>\":\"没采纳的原因\"}}\n");
    p.push_str("或 {\"action\":\"escalate\",\"reason\":\"为什么需要人来决定\"}");
    p
}

impl Orchestrator {
    /// 议题能不能由负责人在自主推进里裁决：除代码细节类（归议题负责人）外都可以，包括需求类和高风险议题。
    pub fn lead_can_rule(issue: &Issue) -> Result<(), String> {
        if issue.kind == "implementation" {
            return Err("代码细节归议题负责人".into());
        }
        Ok(())
    }

    /// 让负责人裁决一个议题。`open` 状态的议题（员工提议的）先推进到“待决策”再裁决；
    /// 负责人选择升级时，议题进入“待决策”交给人（出现在人的待办里）。
    pub async fn lead_rule_issue<A: Approver>(&self, issue_id: &str, lead: &Employee, approver: &A) -> Result<RulingOutcome> {
        let issue = self.db.get_issue(issue_id)?.context("议题不存在")?;
        if let Err(why) = Self::lead_can_rule(&issue) {
            return Ok(RulingOutcome::NotForLead { why });
        }
        if !matches!(issue.status.as_str(), "open" | "discussing" | "pending_decision") {
            return Ok(RulingOutcome::NotForLead { why: format!("议题已是「{}」", issue.status) });
        }
        let project = self.db.get_project(&issue.project_id)?.context("项目不存在")?;
        let options = self.db.issue_options(issue_id)?;
        // 相关讨论：挂在这个议题上的会议发言
        let mut discussion = String::new();
        let meeting_ids: Vec<String> = {
            let conn = self.db.conn.lock().unwrap();
            let mut s = conn.prepare("SELECT meeting_id FROM meeting_issues WHERE issue_id = ?1")?;
            s.query_map([issue_id], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?
        };
        for mid in meeting_ids {
            let msgs = self.db.meeting_messages(&mid, 0)?;
            discussion.push_str(&format_messages(&msgs, &|id| id.and_then(|i| self.db.employee_by_id(i).ok().flatten()).map(|e| e.name).unwrap_or_else(|| "未知".into()), 6000));
        }
        let query = format!("{} {}", issue.title, issue.summary);
        let memory = self.memory_block(Some(&project.id), lead, None, &query, "ruling", issue_id);
        let prompt = prompt_for(&issue, &options, &discussion, &project.goal, &memory);

        let cfg = TurnConfig {
            project_path: Path::new(&project.path).to_path_buf(),
            task_id: None,
            project_id: None,
            read_only: true,
            mcp: None,
            purpose: "ruling".into(),
            attr_project: Some(project.id.clone()),
            meeting_id: None,
            log_prompt: Some(format!("[负责人裁决] {}", issue.title)),
            program: self.program.clone(),
            copilot_program: self.copilot_program.clone(),
        };
        let result = run_turn(&self.db, lead, &cfg, &prompt, approver, &NoopObserver).await?;
        if result.is_error || result.text.trim().is_empty() {
            bail!("{} 没有给出有效裁决（{}）", lead.name, result.subtype);
        }

        // 议题要先走到“待决策”才能拍板或交给人
        if issue.status == "open" {
            self.db.set_issue_status(issue_id, "discussing")?;
        }
        if self.db.get_issue(issue_id)?.map(|i| i.status).as_deref() == Some("discussing") {
            self.db.set_issue_status(issue_id, "pending_decision")?;
        }
        let ruling = match parse_ruling(&result.text, options.len()) {
            Ok(r) => r,
            Err(e) => {
                let reason = format!("负责人的裁决格式不对（{e}），交给人来定");
                self.db.append_event_for(Some(&project.id), "issue_ruling_failed", Some(&lead.id), None, &json!({"issue_id": issue_id, "title": issue.title, "error": e.to_string()}))?;
                return Ok(RulingOutcome::Escalated { reason });
            }
        };
        match ruling {
            Ruling::Escalate { reason } => Ok(RulingOutcome::Escalated { reason }),
            Ruling::Decide { option, decision, reason, rejected } => {
                let chosen = option.map(|n| options[n - 1].id.clone());
                let rej: HashMap<String, String> = rejected.into_iter().filter(|(n, _)| *n >= 1 && *n <= options.len()).map(|(n, w)| (options[n - 1].id.clone(), w)).collect();
                self.db.decide_issue(issue_id, &Actor::Employee(lead.id.clone()), chosen.as_deref(), &decision, &reason, &rej)?;
                Ok(RulingOutcome::Decided { decision, reason })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_decisions_with_surrounding_text_and_validates_them() {
        let t = "好的，我的裁决如下：\n{\"action\":\"decide\",\"option\":2,\"decision\":\"用 SSE\",\"reason\":\"单向推送足够，实现更简单\",\"rejected\":{\"1\":\"WebSocket 需要额外心跳\"}}\n以上。";
        let Ruling::Decide { option, decision, reason, rejected } = parse_ruling(t, 2).unwrap() else { panic!() };
        assert_eq!((option, decision.as_str(), reason.is_empty()), (Some(2), "用 SSE", false));
        assert_eq!(rejected.get(&1).map(String::as_str), Some("WebSocket 需要额外心跳"));
        // 0 = 自己的方案
        let Ruling::Decide { option, .. } = parse_ruling("{\"action\":\"decide\",\"option\":0,\"decision\":\"x\",\"reason\":\"y\"}", 2).unwrap() else { panic!() };
        assert_eq!(option, None);
        assert!(matches!(parse_ruling("{\"action\":\"escalate\",\"reason\":\"涉及生产数据\"}", 2).unwrap(), Ruling::Escalate { .. }));
        // 不合格的输出一律报错（上层会升级给人，而不是瞎猜）
        for bad in ["没有 json", "{\"action\":\"decide\",\"option\":5,\"decision\":\"x\",\"reason\":\"y\"}", "{\"action\":\"decide\",\"option\":1,\"decision\":\"\",\"reason\":\"y\"}",
                    "{\"action\":\"decide\",\"option\":1,\"decision\":\"x\"}", "{\"action\":\"escalate\"}", "{\"action\":\"随便\"}", "{ 坏的"] {
            assert!(parse_ruling(bad, 2).is_err(), "{bad}");
        }
    }

    #[test]
    fn lead_rules_everything_except_code_details() {
        let mk = |kind: &str, high_risk: bool| Issue { id: "i".into(), project_id: "p".into(), title: "t".into(), summary: String::new(), kind: kind.into(), high_risk, status: "open".into(),
            raised_by: None, owner_id: None, decision: None, decision_reason: None, decided_by: None, decided_at: None, supersedes: None, created_at: 0, updated_at: 0 };
        for ok in ["technical", "process", "risk", "bug"] { assert!(Orchestrator::lead_can_rule(&mk(ok, false)).is_ok(), "{ok}"); }
        assert!(Orchestrator::lead_can_rule(&mk("requirement", false)).is_ok());
        assert!(Orchestrator::lead_can_rule(&mk("technical", true)).is_ok());
        assert!(Orchestrator::lead_can_rule(&mk("implementation", false)).is_err());
    }
}
