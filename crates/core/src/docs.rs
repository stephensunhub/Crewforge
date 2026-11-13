//! 项目文档：放在项目仓库的 `docs/` 里，由负责人通过和代码一样的分支、检查点、合并流程维护。
//! 为了不碰用户已有的文档，负责人只写 `docs/crewforge/` 下的文件：
//!   decisions.md（决策记录，ADR 风格，来自已决议的议题）、known-issues.md（已知问题与遗留风险）、
//!   progress.md（计划进度）——这三份由系统从数据确定性地生成，不调用模型，内容不变时不产生提交；
//!   overview.md（项目概览与运行方式）——由负责人读代码后撰写（一次调用）。
//! 界面里的“文档”页读的是主分支上 `docs/` 下的全部 Markdown，只读展示。

use crate::db::Employee;
use crate::cost::days_to_ymd;
use crate::orchestrator::Orchestrator;
use crate::runner::{Approver, NoopObserver, TurnConfig, run_turn};
use crate::workspace::{MergeOutcome, git_output};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::Path;

pub const DOCS_DIR: &str = "docs/crewforge";
const MAX_DOC_BYTES: usize = 200_000;

#[derive(Debug, Clone, Serialize)]
pub struct DocFile {
    pub path: String,
    pub size: usize,
    /// 是否由负责人维护（在 docs/crewforge/ 下）
    pub managed: bool,
}

/// 文档的一个版本：该文件的每一次合并是一个版本（以 git 历史为准，不靠模型写）。
#[derive(Debug, Clone, Serialize)]
pub struct DocVersion {
    /// 版本号，最早的是 1
    pub version: usize,
    pub commit: String,
    pub at: i64,
    pub author: String,
    /// 提交标题（任务合并的提交标题就是条目标题）
    pub subject: String,
    /// 这次改了多少行（新增、删除）
    pub added: u32,
    pub removed: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocsPublish {
    pub files: usize,
    /// 合并后的提交；None 表示没有变化（内容与主分支一致）
    pub commit: Option<String>,
    pub note: String,
}

fn issue_status(s: &str) -> &'static str {
    match s { "open" => "待讨论", "discussing" => "讨论中", "pending_decision" => "待决策", "shelved" => "搁置", _ => "其他" }
}

fn ymd(ms: i64) -> String {
    days_to_ymd(ms.div_euclid(86_400_000))
}

/// 文档路径必须在 docs/ 下、是 .md、不含 `..`。
pub fn valid_doc_path(p: &str) -> bool {
    p.starts_with("docs/") && p.ends_with(".md") && !p.contains("..") && !p.contains('\\') && !p.starts_with('/')
}

/// 去掉模型可能包在外面的 ``` 围栏。
pub fn strip_fence(s: &str) -> String {
    let t = s.trim();
    let Some(rest) = t.strip_prefix("```") else { return t.to_string() };
    let rest = rest.split_once('\n').map(|(_, r)| r).unwrap_or(rest);
    rest.trim_end().strip_suffix("```").unwrap_or(rest).trim().to_string()
}

impl Orchestrator {
    /// 主分支上 docs/ 下的全部 Markdown。
    pub async fn list_docs(&self, project_id: &str) -> Result<Vec<DocFile>> {
        let project = self.db.get_project(project_id)?.context("项目不存在")?;
        let repo = self.ws.repo_info(Path::new(&project.path)).await?;
        let out = git_output(&repo.root, &["ls-tree", "-r", "-l", "HEAD", "docs/"]).await.unwrap_or_default();
        let mut v: Vec<DocFile> = out.lines().filter_map(|l| {
            let (meta, path) = l.split_once('\t')?;
            let size = meta.split_whitespace().nth(3)?.parse().ok()?;
            valid_doc_path(path).then(|| DocFile { path: path.to_string(), size, managed: path.starts_with(&format!("{DOCS_DIR}/")) })
        }).collect();
        v.sort_by(|a, b| b.managed.cmp(&a.managed).then(a.path.cmp(&b.path)));
        Ok(v)
    }

    /// 一份文档的版本列表（新的在前）：版本号、时间、作者、提交标题、改动行数。
    pub async fn doc_history(&self, project_id: &str, path: &str) -> Result<Vec<DocVersion>> {
        if !valid_doc_path(path) { bail!("只能查看 docs/ 下的 Markdown 文档"); }
        let project = self.db.get_project(project_id)?.context("项目不存在")?;
        let repo = self.ws.repo_info(Path::new(&project.path)).await?;
        let out = git_output(&repo.root, &["log", "--follow", "--format=\u{1}%H\u{1}%ct\u{1}%an\u{1}%s", "--numstat", "HEAD", "--", path]).await.unwrap_or_default();
        Ok(parse_doc_history(&out))
    }

    /// 某个版本对这份文档的改动（diff 文本，过长截断）。
    pub async fn doc_version_diff(&self, project_id: &str, path: &str, commit: &str) -> Result<String> {
        if !valid_doc_path(path) { bail!("只能查看 docs/ 下的 Markdown 文档"); }
        if commit.len() < 7 || commit.len() > 40 || !commit.chars().all(|c| c.is_ascii_hexdigit()) { bail!("提交号不对"); }
        let project = self.db.get_project(project_id)?.context("项目不存在")?;
        let repo = self.ws.repo_info(Path::new(&project.path)).await?;
        let d = git_output(&repo.root, &["show", "--format=", "--no-color", commit, "--", path]).await.context("读不到这个版本")?;
        Ok(if d.len() > MAX_DOC_BYTES { format!("{}\n…（改动太长，已截断）", d.chars().take(MAX_DOC_BYTES / 3).collect::<String>()) } else { d })
    }

    pub async fn read_doc(&self, project_id: &str, path: &str) -> Result<String> {
        if !valid_doc_path(path) {
            bail!("只能读取 docs/ 下的 Markdown 文档");
        }
        let project = self.db.get_project(project_id)?.context("项目不存在")?;
        let repo = self.ws.repo_info(Path::new(&project.path)).await?;
        let text = git_output(&repo.root, &["show", &format!("HEAD:{path}")]).await.context("文档不存在")?;
        Ok(if text.len() > MAX_DOC_BYTES { format!("{}\n\n…（文档过长，已截断）", text.chars().take(MAX_DOC_BYTES / 3).collect::<String>()) } else { text })
    }

    /// 由数据确定性生成的三份文档（不含时间戳，数据没变就不产生新提交）。
    pub fn generate_docs(&self, project_id: &str) -> Result<Vec<(String, String)>> {
        let project = self.db.get_project(project_id)?.context("项目不存在")?;
        let name = |id: &Option<String>| id.as_ref().and_then(|i| self.db.employee_by_id(i).ok().flatten()).map(|e| e.name).unwrap_or_else(|| "人类".into());

        // 决策记录
        let issues = self.db.list_issues(project_id)?;
        let mut d = format!("# 决策记录 — {}\n\n> 由 Crewforge 根据已决议的议题自动生成，请勿手工修改；要推翻某个决定，请新建议题取代它。\n\n", project.name);
        let mut decided: Vec<_> = issues.iter().filter(|i| i.decision.is_some()).collect();
        decided.sort_by_key(|i| i.decided_at.unwrap_or(0));
        if decided.is_empty() { d.push_str("还没有已决议的议题。\n"); }
        for i in decided {
            d.push_str(&format!("## {}\n\n- 类型：{}{}\n- 决定：{}\n- 原因：{}\n- 拍板人：{}（{}）\n", i.title, i.kind, if i.high_risk { "（高风险）" } else { "" },
                i.decision.clone().unwrap_or_default(), if i.decision_reason.clone().unwrap_or_default().is_empty() { "—".to_string() } else { i.decision_reason.clone().unwrap_or_default() },
                name(&i.decided_by), ymd(i.decided_at.unwrap_or(0))));
            if let Some(s) = &i.supersedes { if let Some(old) = issues.iter().find(|x| &x.id == s) { d.push_str(&format!("- 取代：《{}》\n", old.title)); } }
            let rejected: Vec<String> = self.db.issue_options(&i.id)?.into_iter().filter(|o| o.status == "rejected").map(|o| format!("  - {}：{}", o.title, o.reject_reason.unwrap_or_default())).collect();
            if !rejected.is_empty() { d.push_str(&format!("- 未采纳的方案：\n{}\n", rejected.join("\n"))); }
            d.push('\n');
        }

        // 已知问题与遗留风险
        let mut k = format!("# 已知问题与遗留风险 — {}\n\n> 由 Crewforge 自动生成，反映生成时的状态。\n\n", project.name);
        let open_issues: Vec<_> = issues.iter().filter(|i| matches!(i.status.as_str(), "open" | "discussing" | "pending_decision" | "shelved")).collect();
        k.push_str("## 未决议题\n\n");
        if open_issues.is_empty() { k.push_str("无。\n"); }
        for i in &open_issues { k.push_str(&format!("- 《{}》（{}，{}）{}\n", i.title, i.kind, issue_status(&i.status), if i.summary.trim().is_empty() { String::new() } else { format!("：{}", i.summary.trim().lines().next().unwrap_or("")) })); }
        k.push_str("\n## 待跟进的评审事项\n\n");
        let acts = self.db.open_actions_for_project(project_id)?;
        if acts.is_empty() { k.push_str("无。\n"); }
        for a in &acts { k.push_str(&format!("- {}{}（来自评审《{}》，{}）\n", if a.blocking { "[拦合并] " } else { "" }, a.text, a.meeting_title, a.assignee_name.clone().unwrap_or_else(|| if a.for_human { "人类".into() } else { "未指派".into() }))); }
        k.push_str("\n## 受阻或取消的计划条目\n\n");
        let plan = self.db.plan_view(project_id)?;
        let stuck: Vec<_> = plan.items.iter().filter(|i| matches!(i.status.as_str(), "blocked" | "cancelled")).collect();
        if stuck.is_empty() { k.push_str("无。\n"); }
        for i in stuck { k.push_str(&format!("- 《{}》（{}）{}\n", i.title, crate::plan::status_label(&i.status), if i.note.is_empty() { String::new() } else { format!("：{}", i.note.lines().next().unwrap_or("")) })); }

        // 计划进度
        let mut p = format!("# 计划进度 — {}\n\n> 由 Crewforge 自动生成。\n\n完成 {}/{}（{}%）\n\n", project.name, plan.counts.done, plan.items.iter().filter(|i| i.status != "cancelled").count(), plan.percent_done);
        for i in plan.items.iter().filter(|i| i.status != "cancelled") {
            p.push_str(&format!("- [{}] {}{}\n", if i.status == "done" { "x" } else { " " }, i.title, i.assignee_name.as_ref().map(|n| format!("（{n}）")).unwrap_or_default()));
        }
        Ok(vec![(format!("{DOCS_DIR}/decisions.md"), d), (format!("{DOCS_DIR}/known-issues.md"), k), (format!("{DOCS_DIR}/progress.md"), p)])
    }

    /// 负责人读代码，写一份“项目概览与运行方式”（只读，输出 Markdown 正文）。
    pub async fn lead_write_overview<A: Approver>(&self, project_id: &str, lead: &Employee, approver: &A) -> Result<String> {
        let project = self.db.get_project(project_id)?.context("项目不存在")?;
        let mut p = format!("[撰写文档] 你是项目「{}」的负责人，请为新加入的人写一份《项目概览与运行方式》。\n", project.name);
        if !project.goal.trim().is_empty() {
            p.push_str(&format!("\n项目目标：\n{}\n", project.goal.trim()));
        }
        p.push_str("\n你可以只读地查看代码来核实每一句话。要求：只写你核实过的事实，不确定就写“待确认”；不要编造命令或目录。包含：项目是什么（两三句）、怎么构建/运行/测试（给出可复制的命令）、目录结构（只列重要的）、关键设计和约束、已知限制。不超过 120 行。\n只输出 Markdown 正文（以一级标题开头），不要任何前言，不要用代码围栏包住全文。");
        let cfg = TurnConfig {
            project_path: Path::new(&project.path).to_path_buf(), task_id: None, project_id: None, read_only: true, mcp: None,
            purpose: "docs".into(), attr_project: Some(project_id.into()), meeting_id: None, log_prompt: Some(format!("[撰写文档] {}", project.name)),
            program: self.program.clone(), copilot_program: self.copilot_program.clone(), origin: "system".into(),
        };
        let r = run_turn(&self.db, lead, &cfg, &p, approver, &NoopObserver).await?;
        let md = strip_fence(&r.text);
        if r.is_error || !md.starts_with('#') || md.lines().count() < 3 {
            bail!("负责人没有写出有效的文档（{}）", r.subtype);
        }
        Ok(md)
    }

    /// 把文档作为一次正常的提交合并进主分支：以负责人名义开任务分支、写文件、保存检查点、合并。
    pub async fn publish_docs(&self, project_id: &str, author: &Employee, files: Vec<(String, String)>, title: &str) -> Result<DocsPublish> {
        let project = self.db.get_project(project_id)?.context("项目不存在")?;
        for (p, _) in &files {
            if !valid_doc_path(p) || !p.starts_with(&format!("{DOCS_DIR}/")) {
                bail!("负责人只能写 {DOCS_DIR}/ 下的文档：{p}");
            }
        }
        let repo = self.ws.repo_info(Path::new(&project.path)).await?;
        let key = repo.root.to_string_lossy().into_owned();
        if self.db.active_task(&author.id, &key)?.is_some() {
            bail!("{} 手上还有进行中的任务，文档等他做完再更新", author.name);
        }
        let task = self.start_task(author, &repo.root, &repo.branch, title).await?;
        for (p, content) in &files {
            let dest = Path::new(&task.worktree_path).join(p);
            if let Some(dir) = dest.parent() { std::fs::create_dir_all(dir)?; }
            std::fs::write(&dest, content)?;
        }
        self.checkpoint_task(&task, author, &format!("[crewforge] {}: {title}\n\nCrewforge-Task: {}\nCrewforge-Employee: {}", author.name, task.id, author.name)).await;
        match self.merge_task(&task.id).await? {
            MergeOutcome::Merged { commit } => Ok(DocsPublish { files: files.len(), commit: Some(commit), note: format!("已合并 {} 个文档", files.len()) }),
            MergeOutcome::Empty => Ok(DocsPublish { files: files.len(), commit: None, note: "文档没有变化".into() }),
            other => {
                let _ = self.abandon_task(&task.id).await;
                bail!("文档合并没有成功：{other:?}")
            }
        }
    }

    /// 更新全部负责人维护的文档：三份生成的 + 一份负责人撰写的概览（`with_overview` 为 true 时调用一次模型）。
    pub async fn update_project_docs<A: Approver>(&self, project_id: &str, lead: &Employee, with_overview: bool, approver: &A) -> Result<DocsPublish> {
        let mut files = self.generate_docs(project_id)?;
        if with_overview {
            match self.lead_write_overview(project_id, lead, approver).await {
                Ok(md) => files.push((format!("{DOCS_DIR}/overview.md"), md)),
                Err(e) => { self.db.append_event_for(Some(project_id), "docs_overview_failed", Some(&lead.id), None, &serde_json::json!({"error": e.to_string()}))?; }
            }
        }
        let r = self.publish_docs(project_id, lead, files, "文档：更新项目文档").await?;
        self.db.append_event_for(Some(project_id), "docs_published", Some(&lead.id), None, &serde_json::json!({"files": r.files, "commit": r.commit, "text": format!("{} 更新了项目文档：{}", lead.name, r.note)}))?;
        Ok(r)
    }
}

/// 解析 `git log --format=\x01%H\x01%ct\x01%an\x01%s --numstat` 的输出（新的在前），版本号从最早的 1 开始。
pub fn parse_doc_history(out: &str) -> Vec<DocVersion> {
    let mut v: Vec<DocVersion> = vec![];
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix('\u{1}') {
            let mut f = rest.splitn(4, '\u{1}');
            let (commit, at, author, subject) = (f.next().unwrap_or(""), f.next().unwrap_or("0"), f.next().unwrap_or(""), f.next().unwrap_or(""));
            v.push(DocVersion { version: 0, commit: commit.into(), at: at.parse::<i64>().unwrap_or(0) * 1000, author: author.into(), subject: subject.into(), added: 0, removed: 0 });
        } else if let Some(cur) = v.last_mut() {
            let mut f = line.split_whitespace();
            if let (Some(a), Some(r)) = (f.next(), f.next()) { cur.added += a.parse().unwrap_or(0); cur.removed += r.parse().unwrap_or(0); }
        }
    }
    let n = v.len();
    for (i, x) in v.iter_mut().enumerate() { x.version = n - i; }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_history_is_parsed_newest_first_with_version_numbers_and_line_counts() {
        let out = "\u{1}bbb222\u{1}1700000200\u{1}alice\u{1}修订需求规格：导出\n3\t1\tdocs/specs/x/requirements.md\n\u{1}aaa111\u{1}1700000100\u{1}bob\u{1}需求规格：导出\n10\t0\tdocs/specs/x/requirements.md\n";
        let v = parse_doc_history(out);
        assert_eq!(v.len(), 2);
        assert_eq!((v[0].version, v[0].commit.as_str(), v[0].author.as_str(), v[0].added, v[0].removed, v[0].at), (2, "bbb222", "alice", 3, 1, 1_700_000_200_000));
        assert_eq!((v[1].version, v[1].subject.as_str(), v[1].added), (1, "需求规格：导出", 10));
        assert!(parse_doc_history("").is_empty());
    }

    #[test]
    fn doc_paths_are_confined_to_docs_markdown() {
        for ok in ["docs/a.md", "docs/crewforge/decisions.md", "docs/x/y/z.md"] { assert!(valid_doc_path(ok), "{ok}"); }
        for bad in ["README.md", "docs/../etc/passwd.md", "docs/a.txt", "/docs/a.md", "docs\\a.md", "src/docs/a.md", "docs/a.md/../../b.md"] { assert!(!valid_doc_path(bad), "{bad}"); }
    }

    #[test]
    fn model_output_fences_are_stripped() {
        assert_eq!(strip_fence("```markdown\n# 标题\n正文\n```"), "# 标题\n正文");
        assert_eq!(strip_fence("```\n# T\n```"), "# T");
        assert_eq!(strip_fence("# 直接是正文\n内容"), "# 直接是正文\n内容");
    }
}
