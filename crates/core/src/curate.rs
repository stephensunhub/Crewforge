//! 负责人整理项目记忆：AI 提议的经验、纪要里的经验是“待验证”的，不会注入给员工；
//! 自主推进时由负责人替人把关——能核实的验证，过时或矛盾的让它过期，重复的合并。
//!
//! 边界：只动项目层的记忆；**人写的、议题决议沉淀的、导入时确认的记忆一律不碰**；
//! 验证必须有具体依据（读代码或已有决议能核实），拿不准就不动，留给人；每次最多处理 8 条改动。
//! 这是个有影响的权力——已验证的记忆会被注入给所有员工——所以每一条改动都要写原因并记入动态。

use crate::db::Employee;
use crate::memory::{Memory, MemoryFilter};
use crate::orchestrator::Orchestrator;
use crate::runner::{Approver, NoopObserver, TurnConfig, run_turn};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::Path;

pub const MAX_OPS: usize = 8;
pub const MAX_CANDIDATES: usize = 24;

#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Verify { memory: usize, reason: String },
    Expire { memory: usize, reason: String },
    Merge { keep: usize, drop: Vec<usize>, content: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub ops: Vec<Op>,
    pub escalate: Option<String>,
    pub summary: String,
}

fn txt(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or("").trim().to_string()
}

pub fn parse_plan(text: &str) -> Result<Plan> {
    let (a, b) = (text.find('{').context("输出里没有 JSON")?, text.rfind('}').context("输出里没有 JSON")?);
    if b <= a {
        bail!("输出里没有完整的 JSON");
    }
    let v: Value = serde_json::from_str(&text[a..=b]).context("JSON 无法解析")?;
    let mut ops = Vec::new();
    for o in v["ops"].as_array().into_iter().flatten() {
        let n = o["memory"].as_u64().map(|n| n as usize);
        ops.push(match o["op"].as_str() {
            Some("verify") => Op::Verify { memory: n.context("verify 需要 memory")?, reason: Some(txt(o, "reason")).filter(|r| !r.is_empty()).context("验证必须写依据")? },
            Some("expire") => Op::Expire { memory: n.context("expire 需要 memory")?, reason: Some(txt(o, "reason")).filter(|r| !r.is_empty()).context("让过期必须写原因")? },
            Some("merge") => {
                let drop: Vec<usize> = o["drop"].as_array().into_iter().flatten().filter_map(|d| d.as_u64().map(|x| x as usize)).collect();
                let content = txt(o, "content");
                if drop.is_empty() || content.is_empty() {
                    bail!("合并需要 drop 列表和合并后的内容");
                }
                Op::Merge { keep: o["keep"].as_u64().context("merge 需要 keep")? as usize, drop, content }
            }
            other => bail!("未知的改动类型：{other:?}"),
        });
    }
    Ok(Plan { ops, escalate: Some(txt(&v, "escalate")).filter(|e| !e.is_empty() && e != "null"), summary: txt(&v, "summary") })
}

/// 负责人能动的记忆：项目层、不是人写的、不是议题决议和导入确认沉淀的。
pub fn lead_may_touch(m: &Memory) -> bool {
    m.layer == "project" && m.created_by != "human" && !matches!(m.source_kind.as_str(), "human" | "issue" | "onboarding") && m.status != "expired"
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Curated {
    pub verified: Vec<String>,
    pub expired: Vec<String>,
    pub merged: Vec<String>,
    pub rejected: Vec<String>,
    pub escalate: Option<String>,
    pub summary: String,
}

impl Orchestrator {
    /// 项目里负责人可以整理的记忆：待验证的优先，其次是已验证的（用于找重复和过时）。
    pub fn curation_candidates(&self, project_id: &str) -> Result<Vec<Memory>> {
        let mut all = self.db.list_memories(&MemoryFilter { layer: Some("project".into()), project_id: Some(project_id.into()), ..Default::default() })?;
        all.retain(lead_may_touch);
        all.sort_by_key(|m| (m.status != "unverified", m.created_at));
        all.truncate(MAX_CANDIDATES);
        Ok(all)
    }

    pub async fn lead_curate_memory<A: Approver>(&self, project_id: &str, lead: &Employee, approver: &A) -> Result<Curated> {
        let project = self.db.get_project(project_id)?.context("项目不存在")?;
        let cands = self.curation_candidates(project_id)?;
        if !cands.iter().any(|m| m.status == "unverified") {
            return Ok(Curated { summary: "没有待验证的记忆".into(), ..Default::default() });
        }
        let mut p = format!("[整理记忆] 你是项目「{}」的负责人。下面是项目记忆里 AI 提议或从纪要里总结出来的条目，已验证的会被注入给所有员工，所以要把关。\n", project.name);
        if !project.goal.trim().is_empty() {
            p.push_str(&format!("\n项目目标：\n{}\n", project.goal.trim()));
        }
        p.push_str("\n记忆列表：\n");
        for (i, m) in cands.iter().enumerate() {
            p.push_str(&format!("#{} [{}｜{}｜来源 {}] {}：{}\n", i + 1, if m.status == "unverified" { "待验证" } else { "已验证" }, crate::memory::kind_label(&m.kind), m.source_kind, m.title, m.content.chars().take(300).collect::<String>().replace('\n', " ")));
        }
        p.push_str("\n你可以只读地查看代码核实。规则：只在能从代码或已有决议核实时才验证（写明依据）；过时、被推翻或和代码矛盾的让它过期（写明原因）；意思重复的合并成一条（保留一条、写出合并后的内容）；拿不准就不动，留给人。\
空泛的、只适用于一次性情况的、当前状态描述这类不是经验的条目，让它过期。每次最多 8 条改动。涉及你无法判断的矛盾，用 escalate 交给人。\n");
        p.push_str("只输出一个 JSON 对象，不要任何其他文字：\n{\"ops\":[{\"op\":\"verify\",\"memory\":1,\"reason\":\"依据\"},{\"op\":\"expire\",\"memory\":2,\"reason\":\"原因\"},{\"op\":\"merge\",\"keep\":3,\"drop\":[4],\"content\":\"合并后的内容\"}],\"escalate\":null,\"summary\":\"一句话\"}");
        let cfg = TurnConfig {
            project_path: Path::new(&project.path).to_path_buf(), task_id: None, project_id: None, read_only: true, mcp: None,
            purpose: "curation".into(), attr_project: Some(project_id.into()), meeting_id: None, log_prompt: Some(format!("[整理记忆] {}", project.name)),
            program: self.program.clone(), copilot_program: self.copilot_program.clone(), origin: "system".into(),
        };
        let r = run_turn(&self.db, lead, &cfg, &p, approver, &NoopObserver).await?;
        if r.is_error || r.text.trim().is_empty() {
            bail!("{} 没有给出有效回复（{}）", lead.name, r.subtype);
        }
        let plan = parse_plan(&r.text)?;
        let mut out = Curated { escalate: plan.escalate.clone(), summary: plan.summary.clone(), ..Default::default() };
        let get = |n: usize| n.checked_sub(1).and_then(|i| cands.get(i));
        for (done, op) in plan.ops.into_iter().enumerate() {
            if done >= MAX_OPS {
                out.rejected.push("超过每次 8 条改动的上限，其余的留到下次".into());
                break;
            }
            match op {
                Op::Verify { memory, reason } => match get(memory) {
                    Some(m) if m.status == "unverified" => match self.db.verify_memory(&m.id, &lead.id) {
                        Ok(()) => { let _ = self.db.append_event_for(Some(project_id), "memory_curated", Some(&lead.id), None, &json!({"memory_id": m.id, "op": "verify", "reason": reason})); out.verified.push(m.title.clone()) }
                        Err(e) => out.rejected.push(format!("验证《{}》：{e}", m.title)),
                    },
                    Some(m) => out.rejected.push(format!("验证《{}》：它已经是「{}」", m.title, m.status)),
                    None => out.rejected.push(format!("验证 #{memory}：没有这条记忆")),
                },
                Op::Expire { memory, reason } => match get(memory) {
                    Some(m) => match self.db.expire_memory(&m.id, &format!("负责人整理：{reason}")) {
                        Ok(()) => out.expired.push(m.title.clone()),
                        Err(e) => out.rejected.push(format!("让《{}》过期：{e}", m.title)),
                    },
                    None => out.rejected.push(format!("让 #{memory} 过期：没有这条记忆")),
                },
                Op::Merge { keep, drop, content } => {
                    let Some(k) = get(keep) else { out.rejected.push(format!("合并：保留 #{keep} 不存在")); continue };
                    let dropped: Vec<&Memory> = drop.iter().filter_map(|d| get(*d)).filter(|m| m.id != k.id).collect();
                    if dropped.len() != drop.len() { out.rejected.push(format!("合并到《{}》：有要合并的条目不存在或就是保留的那条", k.title)); continue; }
                    if self.db.update_memory(&k.id, &k.title, &content, &k.scope).is_err() { out.rejected.push(format!("合并到《{}》：更新失败", k.title)); continue; }
                    for m in dropped { let _ = self.db.expire_memory(&m.id, &format!("负责人整理：已合并到《{}》", k.title)); }
                    out.merged.push(format!("{} 条合并到《{}》", drop.len(), k.title));
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_requires_evidence_and_known_ops() {
        let ok = r#"好的：{"ops":[{"op":"verify","memory":1,"reason":"README 里有这条命令"},{"op":"expire","memory":2,"reason":"已被决议 X 取代"},{"op":"merge","keep":3,"drop":[4,5],"content":"合并后"}],"escalate":null,"summary":"整理了 3 条"}"#;
        let p = parse_plan(ok).unwrap();
        assert_eq!(p.ops.len(), 3);
        assert!(p.escalate.is_none() && p.summary == "整理了 3 条");
        for bad in [r#"{"ops":[{"op":"verify","memory":1}]}"#, r#"{"ops":[{"op":"expire","memory":1}]}"#, r#"{"ops":[{"op":"merge","keep":1,"drop":[]}]}"#,
                    r#"{"ops":[{"op":"delete","memory":1}]}"#, "没有 json"] {
            assert!(parse_plan(bad).is_err(), "{bad}");
        }
    }
}
