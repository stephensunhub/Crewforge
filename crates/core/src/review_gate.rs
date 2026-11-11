//! 要不要为一个条目开评审会：由项目负责人（AI）决定。
//!
//! 评审会 + 纪要在本地模型上要花很多时间（实测占模型时间的将近一半），小改动不值得。
//! 规则：
//! - 改动很大（行数或文件数超过下限）一律评审，不问负责人，这是兜底；
//! - 其余的把改动统计和条目说明交给负责人，由它决定评审还是跳过，并写明原因；
//! - 负责人回复的格式不对、调用出错时，按“评审”处理（宁可多评审，不漏）。
//! 没有 AI 负责人的项目保持原来的行为：每个条目都评审。

use crate::db::Employee;
use crate::plan::PlanItem;
use crate::orchestrator::Orchestrator;
use crate::runner::Approver;
use crate::workspace::FileChange;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// 改动行数（新增 + 删除）达到这个数一律评审。
pub const FORCE_REVIEW_LINES: u32 = 200;
/// 改动文件数达到这个数一律评审。
pub const FORCE_REVIEW_FILES: usize = 8;

#[derive(Debug, Clone, PartialEq)]
pub struct ReviewDecision {
    pub review: bool,
    pub reason: String,
}

/// 改动的行数（未跟踪的新文件没有统计，按 0 行计，但仍计入文件数）和文件数。
pub fn change_size(files: &[FileChange]) -> (u32, usize) {
    let lines = files.iter().map(|f| f.insertions.unwrap_or(0) + f.deletions.unwrap_or(0)).sum();
    (lines, files.len())
}

/// 兜底：必须评审的原因；None 表示可以交给负责人判断。
pub fn forced_review_reason(files: &[FileChange]) -> Option<String> {
    let (lines, n) = change_size(files);
    if lines >= FORCE_REVIEW_LINES {
        Some(format!("改动 {lines} 行，达到 {FORCE_REVIEW_LINES} 行的下限，必须评审"))
    } else if n >= FORCE_REVIEW_FILES {
        Some(format!("改动 {n} 个文件，达到 {FORCE_REVIEW_FILES} 个的下限，必须评审"))
    } else {
        None
    }
}

pub fn parse_review_decision(text: &str) -> Result<ReviewDecision> {
    let (a, b) = (text.find('{').context("输出里没有 JSON")?, text.rfind('}').context("输出里没有 JSON")?);
    if b <= a {
        bail!("输出里没有完整的 JSON");
    }
    let v: Value = serde_json::from_str(&text[a..=b]).context("JSON 无法解析")?;
    let reason = v["reason"].as_str().unwrap_or("").trim().to_string();
    if reason.is_empty() {
        bail!("必须写明原因");
    }
    match v["review"].as_bool() {
        Some(review) => Ok(ReviewDecision { review, reason }),
        None => bail!("review 必须是 true 或 false"),
    }
}

fn stat_table(files: &[FileChange]) -> String {
    files.iter().take(30).map(|f| match (f.insertions, f.deletions) {
        (Some(i), Some(d)) => format!("- {}（+{i} -{d}）", f.path),
        _ => format!("- {}（新文件）", f.path),
    }).collect::<Vec<_>>().join("\n")
}

impl Orchestrator {
    /// 决定这个条目要不要评审。返回（是否评审，原因）。从不失败：出错一律按评审处理。
    pub async fn decide_review<A: Approver>(
        &self, project_id: &str, lead: Option<&Employee>, developer: &Employee, item: &PlanItem, files: &[FileChange], approver: &A,
    ) -> (bool, String) {
        if let Some(why) = forced_review_reason(files) {
            return (true, why);
        }
        let Some(lead) = lead else {
            return (true, "项目没有 AI 负责人，按默认流程评审".into());
        };
        let (lines, n) = change_size(files);
        let mut p = format!("[是否需要评审] 你是项目负责人。{} 做完了计划条目《{}》，验证已经通过。评审会（QA 发言 + 起草纪要）在本地模型上要花十几分钟，请判断这次改动值不值得。\n", developer.name, item.title);
        if !item.description.trim().is_empty() { p.push_str(&format!("\n条目说明：{}\n", item.description.trim().chars().take(800).collect::<String>())); }
        if !item.acceptance.trim().is_empty() { p.push_str(&format!("验收标准：{}\n", item.acceptance.trim().chars().take(400).collect::<String>())); }
        p.push_str(&format!("\n改动统计：{n} 个文件，约 {lines} 行：\n{}\n", stat_table(files)));
        p.push_str("\n判断原则：\n\
- 需要评审（review=true）：改了并发、数据结构、对外接口、错误处理、安全相关的逻辑，或者改动让你拿不准。\n\
- 可以跳过（review=false）：文档、配置、忽略文件、CI 缓存、只补测试用例、改文案、机械的小改动——验证已经通过，评审发现不了更多问题。\n\
- 你可以只读地查看代码来核实，但不要为此花很长时间。\n\
只输出一个 JSON 对象，不要任何其他文字：{\"review\":true或false,\"reason\":\"一句话，具体到这次改动\"}");
        match self.lead_turn_parsed("review_gate", lead, project_id, &p, "判断是否评审", approver, parse_review_decision).await {
            Ok(Ok(d)) => {
                let _ = self.db.append_event_for(Some(project_id), "review_decided", Some(&lead.id), None, &json!({
                    "item": item.title, "review": d.review, "reason": d.reason, "lines": lines, "files": n,
                    "text": format!("{} 判断《{}》{}：{}", lead.name, item.title, if d.review { "需要评审" } else { "不开评审会" }, d.reason)}));
                (d.review, d.reason)
            }
            Ok(Err(why)) => (true, format!("负责人{why}，按评审处理")),
            Err(e) => (true, format!("负责人没能给出判断（{e}），按评审处理")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fc(path: &str, i: u32, d: u32) -> FileChange { FileChange { path: path.into(), insertions: Some(i), deletions: Some(d), untracked: false } }

    #[test]
    fn force_by_lines_and_files() {
        assert!(forced_review_reason(&[fc("a.go", 120, 80)]).is_some());
        assert!(forced_review_reason(&[fc("a.go", 120, 79)]).is_none());
        let many: Vec<_> = (0..8).map(|i| fc(&format!("f{i}"), 1, 0)).collect();
        assert!(forced_review_reason(&many).is_some());
        assert!(forced_review_reason(&many[..7]).is_none());
    }

    #[test]
    fn untracked_counts_as_file_not_lines() {
        let f = FileChange { path: "new.go".into(), insertions: None, deletions: None, untracked: true };
        assert_eq!(change_size(&[f]), (0, 1));
    }

    #[test]
    fn parse_ok_and_errors() {
        let d = parse_review_decision("好的 {\"review\":false,\"reason\":\"只改 .gitignore\"} 完").unwrap();
        assert_eq!(d, ReviewDecision { review: false, reason: "只改 .gitignore".into() });
        assert!(parse_review_decision("{\"review\":true}").is_err());
        assert!(parse_review_decision("{\"review\":\"yes\",\"reason\":\"x\"}").is_err());
        assert!(parse_review_decision("没有 json").is_err());
    }
}
