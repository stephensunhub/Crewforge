//! 架构师与设计阶段。
//!
//! 项目角色「架构师」（`architect`）不写代码，只做设计类条目（`kind = design`）：
//! - 新项目：写 `docs/architecture.md`（模块划分、技术选型、数据与并发、测试策略、风险）和 `docs/conventions.md`（目录命名、代码风格、
//!   错误处理、测试约定、文档约定）；
//! - 导入的二次开发项目：只读已有代码，总结原项目的架构和风格（现状，不是理想），同样写这两份文档。
//!
//! 规则：
//! - 项目里有架构师时，设计条目**必须先做完**，其他条目才会开始（`design_phase`）；
//! - 设计条目只能改文档（Markdown 等），改了别的文件算验证没过，让架构师撤销（`non_doc_changes`）；
//! - 架构师只做设计条目，设计条目只给架构师做（项目里没有架构师时按普通条目处理）；
//! - 两份文档合并后，各自的「给团队的要点」一节写进项目记忆（规范类、已验证），自动注入给每位员工，
//!   这样低等级的员工不用读整份文档也能遵守架构和风格；
//! - 设计做完、计划里还没有实现条目时，负责人按设计文档起草实现计划（提示词里带上文档节选）。

use crate::db::{Db, Member};
use crate::memory::{MemoryFilter, NewMemory};
use crate::orchestrator::Orchestrator;
use crate::plan::NewPlanItem;
use crate::workspace::FileChange;
use anyhow::Result;

pub const DESIGN_KIND: &str = "design";
pub const POINTS_HEADING: &str = "## 给团队的要点";
/// 写进记忆的来源标记，重新发布时据此替换旧的。
pub const MEMORY_SOURCE: &str = "architect";
const DOCS: [(&str, &str); 2] = [("docs/architecture.md", "架构要点"), ("docs/conventions.md", "规范要点")];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesignPhase {
    /// 项目里没有架构师：没有设计阶段
    None,
    /// 有架构师，但还没有任何设计条目
    NotStarted,
    /// 还有 n 个设计条目没做完
    Pending(usize),
    /// 设计条目都做完了
    Done,
}

/// 设计条目只能改文档：.md / .rst / .adoc / .txt，或者 docs/ 目录下的任何文件。
pub fn is_doc_path(path: &str) -> bool {
    let p = path.to_lowercase();
    p.starts_with("docs/") || [".md", ".rst", ".adoc", ".txt"].iter().any(|e| p.ends_with(e))
}

/// 改动里不是文档的文件。
pub fn non_doc_changes(files: &[FileChange]) -> Vec<String> {
    files.iter().filter(|f| !is_doc_path(&f.path)).map(|f| f.path.clone()).collect()
}

/// 取出文档里「## 给团队的要点」一节（到下一个二级标题或文末），去掉首尾空白；没有或为空返回 None。
pub fn team_points(md: &str) -> Option<String> {
    let start = md.lines().position(|l| l.trim() == POINTS_HEADING)?;
    let body: Vec<&str> = md.lines().skip(start + 1).take_while(|l| !l.trim_start().starts_with("## ")).collect();
    let t = body.join("\n").trim().to_string();
    if t.is_empty() { None } else { Some(t) }
}

pub struct DesignTemplate { pub title: &'static str, pub description: String, pub acceptance: &'static str, pub file: &'static str, pub level: i64 }

/// 设计条目模板：`imported` 为 true 时是导入的二次开发项目（总结现状），否则是新项目（做设计）。
pub fn design_templates(imported: bool) -> Vec<DesignTemplate> {
    let rules = "你是这个项目的架构师：只做设计，不写任何代码，只改 docs/ 下的 Markdown 文件。先读项目目标（见项目目标原文）和仓库现状，再动笔；没有查证的写“未确认”，不要编造。";
    let points = format!("最后一节必须是「{POINTS_HEADING}」：不超过 12 条，每条一句话，写开发者必须遵守的约定（这一节会被写进项目记忆，自动给每位员工看）。");
    if imported {
        vec![
            DesignTemplate {
                title: "阅读现有项目：总结架构",
                description: format!("{rules}\n这是一个导入的二次开发项目，请只读地阅读已有代码，写 docs/architecture.md，描述现状而不是理想：## 项目概览、## 模块地图（每个模块的职责、对外接口、依赖方向，指出具体文件）、## 技术栈与构建 / 测试方式、## 数据与状态（数据模型、存储、并发要点）、## 已知的坑与技术债。{points}"),
                acceptance: "docs/architecture.md 存在且含上述各节，最后一节是「给团队的要点」；没有改动任何非文档文件",
                file: "docs/architecture.md", level: 5,
            },
            DesignTemplate {
                title: "阅读现有项目：总结代码风格与约定",
                description: format!("{rules}\n这是一个导入的二次开发项目：写 docs/conventions.md，从现有代码里归纳出实际在用的风格（以代码为准，不要写理想的规范），每条给出具体例子和文件：## 目录与命名、## 代码风格、## 错误处理与日志、## 测试写法、## 注释与文档习惯。{points}"),
                acceptance: "docs/conventions.md 存在且含上述各节，最后一节是「给团队的要点」；没有改动任何非文档文件",
                file: "docs/conventions.md", level: 4,
            },
        ]
    } else {
        vec![
            DesignTemplate {
                title: "架构设计：模块划分、技术选型、接口约定",
                description: format!("{rules}\n写 docs/architecture.md：## 目标与约束、## 模块划分（每个模块的职责、对外接口、依赖方向）、## 技术选型与理由（含放弃的备选）、## 数据与状态（数据模型、存储、并发 / 一致性要点）、## 测试策略、## 风险与待定事项。{points}"),
                acceptance: "docs/architecture.md 存在且含上述各节，最后一节是「给团队的要点」；没有改动任何非文档文件",
                file: "docs/architecture.md", level: 5,
            },
            DesignTemplate {
                title: "文档与代码规范：命名、目录、错误处理、测试约定",
                description: format!("{rules}\n依据已有的架构设计，写 docs/conventions.md：## 目录与命名、## 代码风格、## 错误处理与日志、## 测试约定、## 文档约定、## 提交与评审约定。{points}"),
                acceptance: "docs/conventions.md 存在且含上述各节，最后一节是「给团队的要点」；没有改动任何非文档文件",
                file: "docs/conventions.md", level: 4,
            },
        ]
    }
}

impl Db {
    pub fn architects(&self, project_id: &str) -> Result<Vec<Member>> {
        Ok(self.list_members(project_id)?.into_iter().filter(|m| m.role == "architect").collect())
    }

    /// 设计阶段的状态（取消的设计条目不算）。
    pub fn design_phase(&self, project_id: &str) -> Result<DesignPhase> {
        if self.architects(project_id)?.is_empty() { return Ok(DesignPhase::None); }
        let items: Vec<_> = self.plan_view(project_id)?.items.into_iter().filter(|i| i.kind == DESIGN_KIND && i.status != "cancelled").collect();
        if items.is_empty() { return Ok(DesignPhase::NotStarted); }
        let open = items.iter().filter(|i| i.status != "done").count();
        Ok(if open > 0 { DesignPhase::Pending(open) } else { DesignPhase::Done })
    }

    /// 建设计条目（第二个依赖第一个）。已有设计条目（含取消的）就不再建。返回新建的条目 id。
    pub fn create_design_items(&self, project_id: &str, imported: bool) -> Result<Vec<String>> {
        if self.plan_view(project_id)?.items.iter().any(|i| i.kind == DESIGN_KIND) { return Ok(vec![]); }
        let architect = self.architects(project_id)?.into_iter().next().map(|m| m.employee_id);
        let mut ids: Vec<String> = vec![];
        for t in design_templates(imported) {
            let deps = ids.last().cloned().into_iter().collect();
            let id = self.create_plan_item(project_id, &NewPlanItem { title: t.title.into(), description: t.description, acceptance: t.acceptance.into(), kind: DESIGN_KIND.into(), module: "docs".into(), assignee_id: architect.clone(), deps }, None)?;
            self.set_plan_item_files(&id, &[t.file.to_string()])?;
            self.set_plan_item_level(&id, t.level)?;
            self.set_plan_note(&id, "设计阶段：架构师先把设计做完，其他条目才会开始")?;
            ids.push(id);
        }
        // 自动建的设计条目不需要人再确认（它们来自「项目里有架构师」这条规则，不是 AI 的提议）
        self.approve_proposed_plan(project_id)?;
        Ok(ids)
    }

    /// 计划里有没有实现条目（不算设计、bug 修复和规格文档条目：它们不是“按设计拆出来的计划”）。
    pub fn has_implementation_items(&self, project_id: &str) -> Result<bool> {
        Ok(self.plan_view(project_id)?.items.iter().any(|i| !matches!(i.kind.as_str(), DESIGN_KIND | "bugfix" | "spec") && i.status != "cancelled"))
    }
}

impl Orchestrator {
    /// 设计文档合并之后：把各自「给团队的要点」写进项目记忆（规范类、已验证），替换上一次发布的同名记忆。返回写了几条。
    pub async fn publish_design_memories(&self, project_id: &str, author: &str) -> Result<usize> {
        let mut n = 0;
        for (path, title) in DOCS {
            let Ok(md) = self.read_doc(project_id, path).await else { continue };
            let Some(points) = team_points(&md) else { continue };
            let content: String = points.chars().take(3500).collect();
            // 重新发布（比如规范文档合并后再发一次）：已有的同名记忆原地更新，不删除（已验证的记忆不能删，只能过期）
            let existing = self.db.list_memories(&MemoryFilter { layer: Some("project".into()), project_id: Some(project_id.into()), ..Default::default() })?
                .into_iter().find(|m| m.source_kind == MEMORY_SOURCE && m.title == title && m.status != "expired");
            match existing {
                Some(old) => self.db.update_memory(&old.id, title, &content, path)?,
                None => { self.db.add_memory(NewMemory {
                    layer: "project".into(), project_id: Some(project_id.into()), kind: "convention".into(), title: title.into(), content,
                    scope: path.into(), source_kind: MEMORY_SOURCE.into(), created_by: author.into(), status: "verified".into(), ..Default::default()
                })?; }
            }
            n += 1;
        }
        Ok(n)
    }

    /// 写进负责人起草 / 调整计划提示词的设计文档节选；没有就是空串。
    pub async fn design_docs_block(&self, project_id: &str) -> String {
        let mut out = String::new();
        for (path, _) in DOCS {
            if let Ok(md) = self.read_doc(project_id, path).await {
                out.push_str(&format!("\n【{path}（节选）】\n{}\n", md.chars().take(3500).collect::<String>()));
            }
        }
        if out.is_empty() { out } else { format!("\n架构师的设计文档（实现计划要以它为准，条目不要和它矛盾）：{out}") }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fc(p: &str) -> FileChange { FileChange { path: p.into(), insertions: Some(1), deletions: Some(0), untracked: false } }

    #[test]
    fn doc_paths_and_non_doc_changes() {
        for ok in ["docs/architecture.md", "README.md", "docs/img/a.png", "NOTES.TXT", "a/b/design.rst"] { assert!(is_doc_path(ok), "{ok}"); }
        for bad in ["src/main.go", "go.mod", "web/app.js", "Makefile", "docs2/x.go"] { assert!(!is_doc_path(bad), "{bad}"); }
        assert_eq!(non_doc_changes(&[fc("docs/a.md"), fc("src/x.go"), fc("go.mod")]), vec!["src/x.go".to_string(), "go.mod".to_string()]);
    }

    #[test]
    fn team_points_section_is_extracted() {
        let md = "# T\n## 模块\nx\n## 给团队的要点\n- 一\n- 二\n\n## 附录\ny\n";
        assert_eq!(team_points(md).unwrap(), "- 一\n- 二");
        assert_eq!(team_points("## 给团队的要点\n- 末尾一节\n").unwrap(), "- 末尾一节");
        assert!(team_points("## 给团队的要点\n\n## 下一节\n").is_none(), "空的不算");
        assert!(team_points("没有这一节").is_none());
    }

    #[test]
    fn templates_differ_for_new_and_imported_projects_and_ask_for_the_points_section() {
        let (new, imp) = (design_templates(false), design_templates(true));
        assert_eq!((new.len(), imp.len()), (2, 2));
        assert!(new[0].title.contains("架构设计") && imp[0].title.contains("阅读现有项目"));
        for t in new.iter().chain(imp.iter()) { assert!(t.description.contains(POINTS_HEADING) && t.description.contains("只做设计"), "{}", t.title); }
        assert_eq!((new[0].file, new[1].file), ("docs/architecture.md", "docs/conventions.md"));
    }

    #[test]
    fn design_phase_follows_the_architect_and_the_design_items() {
        let db = Db::open_memory().unwrap();
        let p = db.create_project("p", "", "/tmp/p-design").unwrap().id;
        let arch = db.add_employee("arch", "架构师", "x").unwrap();
        let dev = db.add_employee("dev", "全栈", "x").unwrap();
        db.add_member(&p, &dev.id, "developer").unwrap();
        assert_eq!(db.design_phase(&p).unwrap(), DesignPhase::None, "没有架构师");
        db.add_member(&p, &arch.id, "architect").unwrap();
        assert_eq!(db.design_phase(&p).unwrap(), DesignPhase::NotStarted);
        let ids = db.create_design_items(&p, false).unwrap();
        assert_eq!(ids.len(), 2);
        assert!(db.create_design_items(&p, false).unwrap().is_empty(), "不重复建");
        let v = db.plan_view(&p).unwrap();
        assert!(v.items.iter().all(|i| i.kind == DESIGN_KIND && i.status == "planned" && i.assignee_id.as_deref() == Some(arch.id.as_str()) && i.level > 0));
        assert_eq!(v.items[1].deps, vec![ids[0].clone()], "规范依赖架构");
        assert_eq!(db.design_phase(&p).unwrap(), DesignPhase::Pending(2));
        assert!(!db.has_implementation_items(&p).unwrap());
        db.set_plan_status(&ids[0], "done", "").unwrap();
        assert_eq!(db.design_phase(&p).unwrap(), DesignPhase::Pending(1));
        db.set_plan_status(&ids[1], "done", "").unwrap();
        assert_eq!(db.design_phase(&p).unwrap(), DesignPhase::Done);
    }
}
