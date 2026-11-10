//! 负责人整理“之后再说”的行动项。
//!
//! 评审里不拦合并的意见会降级成“之后再说”：不进人的待办，也没有环节去处理，项目做长了会无限积累。
//! 收尾时负责人把它们过一遍：重复的、过时的、不必做的驳回（必须写原因，重复的写明和哪一条重复）；
//! 值得做的变成计划条目（最多 3 个，写明验收标准和预计改的文件），对应的行动项标为已处理；拿不准的保留。
//! 没提到的行动项原样保留，编号不对的指令被拒绝，所有改动都写进事件。

use crate::actions::ActionItem;
use crate::db::Employee;
use crate::orchestrator::Orchestrator;
use crate::plan::NewPlanItem;
use crate::runner::Approver;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// 整理的触发条件：至少积累了这么多条才值得花一次调用。
pub const MIN_ITEMS_TO_TRIAGE: usize = 5;
pub const MAX_ITEMS_PER_TRIAGE: usize = 60;
pub const MAX_PLAN_ITEMS: usize = 3;
/// 一次整理最多转成几个计划条目：积压多时放宽（每 3 条积压多转 1 个），3 到 8 个。
pub fn max_plan_items(backlog: usize) -> usize { (backlog / 3).clamp(MAX_PLAN_ITEMS, 8) }

#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Dismiss { n: usize, reason: String },
    Plan { n: usize, title: String, description: String, acceptance: String, assignee: Option<String>, files: Vec<String> },
    Keep { n: usize },
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Triaged {
    pub dismissed: usize,
    pub planned: Vec<String>,
    pub kept: usize,
    pub rejected: Vec<String>,
}

pub fn parse_triage(text: &str) -> Result<Vec<Op>> {
    let (a, b) = (text.find('{').context("输出里没有 JSON")?, text.rfind('}').context("输出里没有 JSON")?);
    if b <= a {
        bail!("输出里没有完整的 JSON");
    }
    let v: Value = serde_json::from_str(&text[a..=b]).context("JSON 无法解析")?;
    let s = |x: &Value, k: &str| x[k].as_str().unwrap_or("").trim().to_string();
    let mut ops = Vec::new();
    for it in v["items"].as_array().context("缺少 items")? {
        let n = it["n"].as_u64().context("每项要有编号 n")? as usize;
        ops.push(match it["op"].as_str() {
            Some("dismiss") => {
                let reason = s(it, "reason");
                if reason.is_empty() { bail!("第 {n} 条驳回没有写原因"); }
                Op::Dismiss { n, reason }
            }
            Some("plan") => {
                let title = s(it, "title");
                if title.is_empty() { bail!("第 {n} 条转计划没有标题"); }
                let files = it["files"].as_array().into_iter().flatten().filter_map(|x| x.as_str()).map(|x| x.trim().trim_start_matches("./").to_string()).filter(|x| !x.is_empty()).take(8).collect();
                Op::Plan { n, title, description: s(it, "description"), acceptance: s(it, "acceptance"), assignee: Some(s(it, "assignee")).filter(|a| !a.is_empty() && a != "null"), files }
            }
            Some("keep") => Op::Keep { n },
            other => bail!("第 {n} 条的 op 不对：{other:?}"),
        });
    }
    Ok(ops)
}

impl Orchestrator {
    /// 这个项目里“之后再说”的行动项：待处理、不是给人的、不拦进行中任务的合并。
    pub fn later_actions(&self, project_id: &str) -> Result<Vec<ActionItem>> {
        let mut out = Vec::new();
        for a in self.db.open_actions_for_project(project_id)? {
            if a.for_human || a.status != "open" { continue; }
            let blocks_active = a.blocking && a.task_id.as_deref().map(|t| self.db.get_task(t).ok().flatten().map(|t| t.status == "active").unwrap_or(false)).unwrap_or(false);
            if !blocks_active { out.push(a); }
        }
        Ok(out)
    }

    /// 让负责人整理“之后再说”的行动项。不够 `MIN_ITEMS_TO_TRIAGE` 条时不调用模型，返回 None。
    pub async fn lead_triage_later<A: Approver>(&self, project_id: &str, lead: &Employee, approver: &A) -> Result<Option<Triaged>> {
        let mut items = self.later_actions(project_id)?;
        if items.len() < MIN_ITEMS_TO_TRIAGE {
            return Ok(None);
        }
        items.truncate(MAX_ITEMS_PER_TRIAGE);
        let plan_cap = max_plan_items(items.len());
        let project = self.db.get_project(project_id)?.context("项目不存在")?;
        let members = self.db.list_members(project_id)?;
        let plan = self.db.plan_view(project_id)?;
        let mut p = format!("[整理行动项] 你是项目「{}」的负责人。下面是评审和会议里积累的“之后再说”的行动项（不拦合并，也没人跟进），请整理一遍。\n", project.name);
        if !project.goal.trim().is_empty() {
            p.push_str(&format!("\n项目目标与需求原文：\n{}\n", project.goal.trim()));
        }
        p.push_str("\n行动项（编号 n）：\n");
        for (i, a) in items.iter().enumerate() {
            p.push_str(&format!("{}. {}（来自《{}》）\n", i + 1, a.text.chars().take(200).collect::<String>(), a.meeting_title.chars().take(40).collect::<String>()));
        }
        p.push_str("\n项目成员：\n");
        for m in &members {
            p.push_str(&format!("- {}（{}，项目角色 {}）\n", m.name, m.title, m.role));
        }
        if !plan.items.is_empty() {
            p.push_str(&format!("\n现有计划（已经在做或做完的事不要重复）：\n{}", crate::planning::plan_table(&plan.items)));
        }
        // 已经裁决的议题：行动项只是在重复这些决定的，不必再留着
        let decided: Vec<_> = self.db.list_issues(project_id)?.into_iter().filter(|i| i.status == "decided" || i.status == "implementing").take(20).collect();
        if !decided.is_empty() {
            p.push_str("\n已经裁决的议题（行动项如果只是在重复这些决定，dismiss 并写明“已由议题《…》裁决”）：\n");
            for i in &decided {
                p.push_str(&format!("- 《{}》→ {}\n", i.title.chars().take(60).collect::<String>(), i.decision.as_deref().unwrap_or("").chars().take(100).collect::<String>()));
            }
        }
        p.push_str(&format!("\n你可以只读地查看代码来核实现状。逐条处理，原则：\n\
- 重复的（和另一条意思相同，或已被上面的议题裁决覆盖）、已经被代码或现有计划解决的、事实已经变化而过期的、过度设计的、对目标没有价值的：dismiss，必须写原因（重复的写明和第几条重复）。\n\
- 确实值得做、会提高质量的：最多 {plan_cap} 条转成计划条目（plan），写清 title、description、能用命令验证的 acceptance、assignee（项目成员名）、files（预计改的文件）；相近的几条合成一个条目。\n\
- 拿不准的：keep（保留）。没提到的也会保留。不要为了清空而清空，也不要把每条都转成计划。\n"));
        p.push_str("只输出一个 JSON 对象，不要任何其他文字：\n{\"items\":[{\"n\":1,\"op\":\"dismiss\",\"reason\":\"...\"},{\"n\":2,\"op\":\"plan\",\"title\":\"...\",\"description\":\"...\",\"acceptance\":\"...\",\"assignee\":\"成员名\",\"files\":[\"路径\"]},{\"n\":3,\"op\":\"keep\"}]}");

        let ops = self.lead_turn_parsed("curation", lead, project_id, &p, "整理行动项", approver, parse_triage).await?
            .map_err(|why| anyhow::anyhow!("负责人的整理结果{why}"))?;
        let mut out = Triaged::default();
        let mut seen = std::collections::HashSet::new();
        for op in ops {
            let n = match &op { Op::Dismiss { n, .. } | Op::Plan { n, .. } | Op::Keep { n } => *n };
            let Some(item) = n.checked_sub(1).and_then(|i| items.get(i)) else { out.rejected.push(format!("编号 {n} 不存在")); continue };
            if !seen.insert(n) { out.rejected.push(format!("编号 {n} 重复出现")); continue; }
            match op {
                Op::Keep { .. } => out.kept += 1,
                Op::Dismiss { reason, .. } => {
                    self.db.set_action_status(&item.id, "dismissed", &format!("负责人（{}）整理：{reason}", lead.name))?;
                    out.dismissed += 1;
                }
                Op::Plan { title, description, acceptance, assignee, files, .. } => {
                    if out.planned.len() >= plan_cap { out.rejected.push(format!("第 {n} 条转计划：超过 {plan_cap} 个的上限，保留"));  continue; }
                    let aid = assignee.as_deref().and_then(|nm| members.iter().find(|m| m.name == nm)).map(|m| m.employee_id.clone());
                    match self.db.create_plan_item(project_id, &NewPlanItem { title: title.clone(), description, acceptance, kind: "migrate".into(), module: String::new(), assignee_id: aid, deps: vec![] }, None) {
                        Ok(id) => {
                            let _ = self.db.set_plan_item_files(&id, &files);
                            let _ = self.db.set_plan_note(&id, &format!("负责人整理行动项追加：{}", item.text.chars().take(80).collect::<String>()));
                            self.db.set_action_status(&item.id, "done", &format!("负责人（{}）整理：已转为计划条目《{title}》", lead.name))?;
                            out.planned.push(title);
                        }
                        Err(e) => out.rejected.push(format!("第 {n} 条转计划《{title}》：{e}")),
                    }
                }
            }
        }
        self.db.append_event_for(Some(project_id), "lead_triaged_actions", Some(&lead.id), None, &json!({
            "dismissed": out.dismissed, "planned": out.planned, "kept": out.kept, "rejected": out.rejected,
            "text": format!("{} 整理了“之后再说”的行动项：驳回 {} 条，转为计划条目 {} 个，保留 {} 条", lead.name, out.dismissed, out.planned.len(), items.len().saturating_sub(out.dismissed + out.planned.len())),
        }))?;
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ops_and_rejects_missing_reasons() {
        let ops = parse_triage(r#"好的 {"items":[{"n":1,"op":"dismiss","reason":"和第 2 条重复"},{"n":2,"op":"plan","title":"补边界测试","files":["./tests/t.py"]},{"n":3,"op":"keep"}]}"#).unwrap();
        assert_eq!(ops.len(), 3);
        assert!(matches!(&ops[1], Op::Plan { files, .. } if files == &vec!["tests/t.py".to_string()]));
        assert!(parse_triage(r#"{"items":[{"n":1,"op":"dismiss"}]}"#).is_err());
        assert!(parse_triage(r#"{"items":[{"n":1,"op":"plan"}]}"#).is_err());
        assert!(parse_triage(r#"{"items":[{"n":1,"op":"explode"}]}"#).is_err());
        assert!(parse_triage("没有 JSON").is_err());
    }
}
