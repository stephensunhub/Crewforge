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

/// 小到不值得调用负责人的改动：总改动 ≤ TRIVIAL_MAX_LINES 行，且（全是文档文件，或改动的每一行都是注释/空行）。
/// 负责人带着几万字上下文判断一个 3 行的注释订正，实测要 3.9 分钟，比看一眼 diff 还慢。
pub const TRIVIAL_MAX_LINES: u32 = 10;

fn is_comment_line(t: &str) -> bool {
    let t = t.trim();
    if t.is_empty() { return true; }
    if t.starts_with("//") || t.starts_with("/*") || t.starts_with("*/") || t.starts_with("<!--") { return true; }
    // # 开头：脚本、YAML、Python 的注释；但 Rust 属性 #[..]、shebang、C 预处理指令是代码
    if let Some(rest) = t.strip_prefix('#') {
        return !(rest.starts_with('[') || rest.starts_with('!') || ["include", "define", "if", "else", "elif", "endif", "pragma", "undef", "error"].iter().any(|k| rest.trim_start().starts_with(k)));
    }
    false
}

/// 改动（diff 里以 + 或 - 开头的行）是不是全是注释和空行，且至少有一行。
pub fn comment_only_diff(diff: &str) -> bool {
    let mut n = 0;
    for l in diff.lines() {
        if l.starts_with("+++") || l.starts_with("---") { continue; }
        if let Some(body) = l.strip_prefix('+').or_else(|| l.strip_prefix('-')) {
            if !is_comment_line(body) { return false; }
            n += 1;
        }
    }
    n > 0
}

/// 不必问负责人、直接不评审的小改动；返回原因。
pub fn trivial_skip_reason(files: &[FileChange], diff: &str) -> Option<String> {
    let (lines, _) = change_size(files);
    if files.is_empty() || files.iter().any(|f| f.untracked) || lines > TRIVIAL_MAX_LINES { return None; }
    if files.iter().all(|f| [".md", ".rst", ".adoc"].iter().any(|e| f.path.to_lowercase().ends_with(e))) {
        return Some(format!("只改了文档（{lines} 行），不开评审会"));
    }
    if comment_only_diff(diff) {
        return Some(format!("只改了注释（{lines} 行），不开评审会"));
    }
    None
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
        &self, project_id: &str, lead: Option<&Employee>, developer: &Employee, item: &PlanItem, files: &[FileChange], diff: &str, approver: &A,
    ) -> (bool, String) {
        if let Some(why) = forced_review_reason(files) {
            return (true, why);
        }
        if let Some(why) = trivial_skip_reason(files, diff) {
            let _ = self.db.append_event_for(Some(project_id), "review_decided", None, None, &json!({
                "item": item.title, "review": false, "reason": why, "lines": change_size(files).0, "files": files.len(), "by": "rule",
                "text": format!("《{}》{}", item.title, why)}));
            return (false, why);
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
    fn comment_only_changes_are_recognised_and_code_is_not() {
        let d = |body: &str| format!("diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n ctx\n{body}\n");
        assert!(comment_only_diff(&d("-# 旧注释\n+# 新注释")));
        assert!(comment_only_diff(&d("-  // old\n+  // new\n+")));
        assert!(!comment_only_diff(&d("-# 旧注释\n+x := 1")));
        assert!(!comment_only_diff(&d("+#[derive(Debug)]")), "Rust 属性是代码");
        assert!(!comment_only_diff(&d("+#!/bin/sh")), "shebang 是代码");
        assert!(!comment_only_diff(&d("+#include <x.h>")));
        assert!(!comment_only_diff("diff --git a/x b/x\n--- a/x\n+++ b/x\n"), "没有改动行不算");
    }

    #[test]
    fn trivial_rule_needs_few_lines_tracked_files_and_docs_or_comments() {
        let md = FileChange { path: "docs/usage.md".into(), insertions: Some(3), deletions: Some(1), untracked: false };
        assert!(trivial_skip_reason(&[md.clone()], "").is_some());
        let big_md = FileChange { insertions: Some(20), ..md.clone() };
        assert!(trivial_skip_reason(&[big_md], "").is_none(), "超过 10 行不走快速规则");
        let sh = FileChange { path: "test/smoke.sh".into(), insertions: Some(2), deletions: Some(1), untracked: false };
        assert!(trivial_skip_reason(&[sh.clone()], "+++ b/x\n+# note\n-# old\n").is_some());
        assert!(trivial_skip_reason(&[sh], "+++ b/x\n+echo hi\n").is_none(), "代码改动交给负责人");
        let new = FileChange { path: "NEW.md".into(), insertions: None, deletions: None, untracked: true };
        assert!(trivial_skip_reason(&[new], "").is_none(), "未跟踪的新文件看不到行数，不走快速规则");
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
