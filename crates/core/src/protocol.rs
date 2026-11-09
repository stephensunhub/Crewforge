//! 沟通规范：AI 之间（以及 AI 对人）的输出要具体、可量化、有依据、可执行、有边界，
//! 这样接收方一次就能读懂，不必反复追问确认——追问同样耗费时间和 token。
//! 只在下游最容易误读的几个节点加：任务完成汇报、站会发言、评审意见、会议行动项。
//! 设置项 `protocol` 设为 `off` 可整体关闭（用于对比效果）。

use crate::db::Db;

pub const COMMON: &str = "[沟通规范] 你的输出会被其他 AI 和人直接使用，没有人会追问你，所以：\
结论先行；具体——点名文件、函数、命令、字段，不说“相关代码”“一些问题”；可量化——给数字（例如“通过 55/55”“改动 3 个文件”）；\
有依据——结论后面附上运行的命令和结果；待办写明谁来做、做什么、做到什么算完成；不确定就直接说“不确定，缺 X”。不要客套，不要复述任务。";

pub const TASK_REPORT: &str = "任务结束时，最后一条消息只按这四行汇报，每行一句话以内：\n\
做了什么：（改动的文件或模块）\n验证：（运行的命令 + 结果数字）\n没做 / 已知问题：（没有写“无”）\n需要别人配合：（谁、什么、何时需要；没有写“无”）";

pub const STANDUP_FORMAT: &str = "按固定四项报告，每项都要具体、带数字：① 做完了：文件/功能 + 验证结果（如“cargo test 55/55”）；② 在做：对象 + 大约还差多少；\
③ 被卡住：卡在哪个文件/接口/决定上，需要谁给什么；④ 依赖别人：对方的什么产物（文件、字段名、接口），以及你假设它是什么样的。";

pub const REVIEW_FORMAT: &str = "先给总体结论（通过 / 有条件通过 / 不通过）。之后每条意见单独一行：「【拦合并】或【建议】文件:位置 — 问题 — 期望的改法」。\
没有位置和改法的意见不要写。最多 8 条，按严重程度排序。";

pub const ACTION_FORMAT: &str = "每条行动项写成「负责人：做什么（对象，完成标准）」，例如“alice：补 top 的 N 非法用例（cases.txt，golden 比对通过）”。";

/// 是否启用（默认启用）。
pub fn enabled(db: &Db) -> bool {
    db.get_setting("protocol").ok().flatten().as_deref() != Some("off")
}

/// 按用途给出追加到系统提示里的规范。
pub fn style_for(purpose: &str) -> String {
    match purpose {
        "task" => format!("{COMMON}\n\n{TASK_REPORT}"),
        "meeting" | "minutes" | "ruling" | "planning" => COMMON.to_string(),
        _ => String::new(),
    }
}

/// 对任务完成汇报做不调用模型的检查：返回缺了什么。只提醒，不拦流程。
pub fn lint_report(text: &str) -> Vec<String> {
    let mut flags = Vec::new();
    let t = text.trim();
    if t.is_empty() {
        return flags;
    }
    if !t.chars().any(|c| c.is_ascii_digit()) {
        flags.push("汇报里没有任何数字，别人无法判断验证到了什么程度".to_string());
    }
    const VERIFY: &[&str] = &["cargo", "npm", "pytest", "unittest", "go test", "mvn", "gradle", "jest", "vitest", "测试", "验证", "运行", "对照"];
    if !VERIFY.iter().any(|k| t.contains(k)) {
        flags.push("没有说明怎么验证的（没提到命令或测试）".to_string());
    }
    if t.chars().count() > 1500 {
        flags.push(format!("汇报过长（{} 字），接收方要花更多 token 读", t.chars().count()));
    }
    flags
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lint_flags_missing_numbers_and_verification() {
        assert!(lint_report("已完成，都没问题").len() >= 2);
        let good = "做了什么：rust/src/main.rs\n验证：cargo test 55/55 通过\n没做：无\n需要别人配合：无";
        assert!(lint_report(good).is_empty());
        assert!(lint_report("").is_empty());
        assert!(lint_report(&"字".repeat(1600)).iter().any(|f| f.contains("过长")));
    }

    #[test]
    fn style_is_chosen_by_purpose() {
        assert!(style_for("task").contains("做了什么") && style_for("task").contains("沟通规范"));
        assert!(style_for("meeting").contains("沟通规范") && !style_for("meeting").contains("做了什么"));
        assert!(style_for("onboarding").is_empty());
    }
}
