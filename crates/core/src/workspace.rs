//! Git 工作区隔离：每个员工在每个仓库拥有独立的 worktree，每个任务对应一个分支。
//!
//! 设计要点：
//! - worktree 放在数据目录下，而不是项目仓库内。放进仓库会让 Cargo 等工具向上找到
//!   用户自己的根配置而报错。
//! - 同一仓库的所有 git 写操作通过仓库级互斥锁串行，避免并发 `worktree add` 互相冲突。
//! - 合并使用 squash，要求主工作区在基础分支上且没有未提交改动；合并前用
//!   `git merge-tree` 预检冲突，不触碰任何工作目录。
//! - 任何操作都不会丢弃用户或员工的改动：放弃任务只是把改动提交为 WIP 并保留分支。

use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::process::Command;

const MAX_CHANGED_FILES: usize = 5000;

#[derive(Debug, Clone, Serialize)]
pub struct RepoStatus {
    pub is_repo: bool,
    pub root: Option<String>,
    /// 当前分支；游离 HEAD 时为 None
    pub branch: Option<String>,
    pub has_commits: bool,
    /// 主工作区是否有未提交的改动（不含未跟踪文件）
    pub dirty: bool,
}

#[derive(Debug, Clone)]
pub struct RepoInfo {
    pub root: PathBuf,
    pub branch: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub insertions: Option<u32>,
    pub deletions: Option<u32>,
    pub untracked: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Checkpoint {
    pub commit: String,
    pub files: usize,
    pub insertions: u32,
    pub deletions: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct MergeCheck {
    pub clean: bool,
    pub conflicts: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum MergeOutcome {
    Merged { commit: String },
    /// 任务分支与基础分支相比没有任何改动
    Empty,
    Conflict { files: Vec<String> },
    Blocked { reason: String },
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Overlap {
    pub path: String,
    pub task_ids: Vec<String>,
}

/// 找出被多个任务同时修改的文件，用于提示"两个员工在改同一处"。
pub fn find_overlaps(sets: &[(String, Vec<String>)]) -> Vec<Overlap> {
    let mut by_path: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (task, paths) in sets {
        for p in paths {
            by_path.entry(p).or_default().push(task.clone());
        }
    }
    by_path
        .into_iter()
        .filter(|(_, ids)| ids.len() > 1)
        .map(|(p, task_ids)| Overlap { path: p.into(), task_ids })
        .collect()
}

struct Output {
    ok: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

async fn run_git(dir: &Path, args: &[&str]) -> Result<Output> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .output()
        .await
        .context("无法运行 git，请确认已安装 git")?;
    Ok(Output {
        ok: out.status.success(),
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

/// 运行 git，失败则返回带 stderr 的错误；成功返回去掉尾部换行的 stdout。
async fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let o = run_git(dir, args).await?;
    if !o.ok {
        bail!("git {} 失败: {}", args.join(" "), o.stderr);
    }
    Ok(o.stdout.trim_end().to_string())
}

/// 供提交使用的身份，不依赖用户本机的 git 配置。
fn identity(name: &str) -> [String; 4] {
    ["-c".into(), format!("user.name={name}"), "-c".into(), format!("user.email={name}@crewforge.local")]
}

async fn git_as(dir: &Path, who: &str, args: &[&str]) -> Result<String> {
    let id = identity(who);
    let mut all: Vec<&str> = id.iter().map(String::as_str).collect();
    all.extend_from_slice(args);
    git(dir, &all).await
}

/// 把员工名转成可用于目录和分支名的片段；含特殊字符（如中文名）时退回到 id 前缀。
pub fn safe_slug(name: &str, id: &str) -> String {
    let ok = !name.is_empty()
        && name.len() <= 40
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok { name.to_string() } else { format!("e-{}", id.chars().take(8).collect::<String>()) }
}

fn fnv(s: &str) -> u32 {
    s.bytes().fold(0x811c9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x01000193))
}

pub struct WorkspaceManager {
    root: PathBuf,
    locks: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
}

impl WorkspaceManager {
    /// `data_dir` 下的 `worktrees/` 用于存放所有员工的 worktree。
    pub fn new(data_dir: &Path) -> Self {
        Self { root: data_dir.join("worktrees"), locks: Mutex::new(HashMap::new()) }
    }

    fn lock_for(&self, repo: &Path) -> Arc<tokio::sync::Mutex<()>> {
        self.locks.lock().unwrap().entry(repo.to_path_buf()).or_default().clone()
    }

    pub fn worktree_path(&self, repo_root: &Path, employee_slug: &str) -> PathBuf {
        let name = repo_root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
        let safe: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
        self.root
            .join(format!("{safe}-{:08x}", fnv(&repo_root.to_string_lossy())))
            .join(employee_slug)
    }

    /// 检查目录的 Git 状态；不是仓库时不报错，只返回 `is_repo = false`。
    pub async fn repo_status(&self, path: &Path) -> Result<RepoStatus> {
        let Ok(root) = git(path, &["rev-parse", "--show-toplevel"]).await else {
            return Ok(RepoStatus { is_repo: false, root: None, branch: None, has_commits: false, dirty: false });
        };
        let has_commits = run_git(Path::new(&root), &["rev-parse", "--verify", "-q", "HEAD"]).await?.ok;
        let branch = git(Path::new(&root), &["symbolic-ref", "--short", "-q", "HEAD"]).await.ok();
        let dirty = has_commits
            && !git(Path::new(&root), &["status", "--porcelain", "--untracked-files=no"]).await?.is_empty();
        Ok(RepoStatus { is_repo: true, root: Some(root), branch, has_commits, dirty })
    }

    /// 要求目录是已有提交、位于某个分支上的仓库，返回仓库根。
    pub async fn repo_info(&self, path: &Path) -> Result<RepoInfo> {
        let st = self.repo_status(path).await?;
        let Some(root) = st.root else { bail!("{} 不是 Git 仓库，请先初始化", path.display()) };
        if !st.has_commits {
            bail!("仓库还没有任何提交，无法创建工作区。请先提交一次");
        }
        let Some(branch) = st.branch else { bail!("主工作区处于游离 HEAD，请先切换到某个分支") };
        Ok(RepoInfo { root: PathBuf::from(root), branch })
    }

    /// 初始化仓库并创建首个提交（worktree 需要至少一个提交）。
    pub async fn init_repo(&self, path: &Path) -> Result<()> {
        if self.repo_status(path).await?.is_repo {
            bail!("该目录已经是 Git 仓库");
        }
        git(path, &["init", "-b", "main"]).await?;
        git_as(path, "Crewforge", &["commit", "--allow-empty", "-m", "Initial commit"]).await?;
        Ok(())
    }

    /// 让目录成为可用的项目仓库：`init` 为 true 时，缺少 Git 仓库就初始化，
    /// 有仓库但还没有提交就补一个空的初始提交。返回仓库信息（根目录以 Git 为准）。
    pub async fn ensure_ready(&self, path: &Path, init: bool) -> Result<RepoInfo> {
        let st = self.repo_status(path).await?;
        if init {
            if !st.is_repo {
                self.init_repo(path).await?;
            } else if !st.has_commits {
                let root = st.root.context("无效的仓库")?;
                git_as(Path::new(&root), "Crewforge", &["commit", "--allow-empty", "-m", "Initial commit"]).await?;
            }
        }
        self.repo_info(path).await
    }

    /// 为任务准备工作区：首次为员工创建 worktree，之后在其中切换到新的任务分支。
    /// 返回 (worktree 路径, 基础提交)。
    pub async fn prepare_task(
        &self,
        repo: &Path,
        employee_slug: &str,
        branch: &str,
        base_branch: &str,
    ) -> Result<(PathBuf, String)> {
        let lock = self.lock_for(repo);
        let _g = lock.lock().await;
        let wt = self.worktree_path(repo, employee_slug);
        let base_commit = git(repo, &["rev-parse", "--verify", &format!("{base_branch}^{{commit}}")]).await?;
        let wt_str = wt.to_string_lossy().into_owned();

        // 清理已被手动删除的 worktree 登记
        git(repo, &["worktree", "prune"]).await?;
        // git 列出的是真实路径，而我们的路径可能经过符号链接（如 /var → /private/var），需规范化后比较
        let wt_real = wt.canonicalize().unwrap_or_else(|_| wt.clone());
        let registered = git(repo, &["worktree", "list", "--porcelain"]).await?.lines().any(|l| {
            l.strip_prefix("worktree ").is_some_and(|p| {
                let p = Path::new(p);
                p == wt || p.canonicalize().is_ok_and(|c| c == wt_real)
            })
        });

        if registered && wt.exists() {
            let dirty = git(&wt, &["status", "--porcelain"]).await?;
            if !dirty.is_empty() {
                bail!("{employee_slug} 的工作区还有未提交的改动，不能切换到新任务");
            }
            git(&wt, &["switch", "-c", branch, &base_commit]).await?;
        } else {
            if wt.exists() {
                bail!("{} 已存在但不是有效的 worktree，请手动处理", wt.display());
            }
            std::fs::create_dir_all(wt.parent().context("无效的 worktree 路径")?)?;
            git(repo, &["worktree", "add", "-b", branch, &wt_str, &base_commit]).await?;
        }
        Ok((wt, base_commit))
    }

    /// 把 worktree 中的改动提交为检查点；没有改动则返回 None。
    pub async fn checkpoint(&self, worktree: &Path, employee: &str, message: &str) -> Result<Option<Checkpoint>> {
        let status = git(worktree, &["status", "--porcelain"]).await?;
        if status.is_empty() {
            return Ok(None);
        }
        let n = status.lines().count();
        if n > MAX_CHANGED_FILES {
            bail!("改动文件多达 {n} 个，疑似包含构建产物。请检查项目的 .gitignore 后再提交");
        }
        git(worktree, &["add", "-A"]).await?;
        git_as(worktree, employee, &["commit", "-q", "-m", message]).await?;
        let commit = git(worktree, &["rev-parse", "HEAD"]).await?;
        let stat = git(worktree, &["show", "--numstat", "--format=", "HEAD"]).await?;
        let (mut files, mut ins, mut del) = (0, 0, 0);
        for l in stat.lines() {
            let mut it = l.split('\t');
            files += 1;
            ins += it.next().and_then(|x| x.parse::<u32>().ok()).unwrap_or(0);
            del += it.next().and_then(|x| x.parse::<u32>().ok()).unwrap_or(0);
        }
        Ok(Some(Checkpoint { commit, files, insertions: ins, deletions: del }))
    }

    /// 相对基础提交的改动文件（含已提交、未提交与未跟踪）。
    pub async fn changed_files(&self, worktree: &Path, base_commit: &str) -> Result<Vec<FileChange>> {
        let mut out: Vec<FileChange> = git(worktree, &["diff", "--numstat", "--no-renames", base_commit])
            .await?
            .lines()
            .filter_map(|l| {
                let mut it = l.splitn(3, '\t');
                let (a, d, p) = (it.next()?, it.next()?, it.next()?);
                Some(FileChange { path: p.into(), insertions: a.parse().ok(), deletions: d.parse().ok(), untracked: false })
            })
            .collect();
        for p in git(worktree, &["ls-files", "--others", "--exclude-standard"]).await?.lines() {
            out.push(FileChange { path: p.into(), insertions: None, deletions: None, untracked: true });
        }
        Ok(out)
    }

    /// 相对基础提交的完整 diff，超过 `max_bytes` 时截断。
    pub async fn diff(&self, worktree: &Path, base_commit: &str, max_bytes: usize) -> Result<String> {
        let mut d = git(worktree, &["diff", "--no-color", "--no-renames", base_commit]).await?;
        if d.len() > max_bytes {
            let cut = (0..=max_bytes).rev().find(|i| d.is_char_boundary(*i)).unwrap_or(0);
            d.truncate(cut);
            d.push_str("\n\n… diff 过长，已截断 …\n");
        }
        Ok(d)
    }

    /// 两个提交之间的 diff（用于已结束的任务，此时 worktree 可能已切换到别的分支）。
    pub async fn diff_range(&self, repo: &Path, base_commit: &str, branch: &str, max_bytes: usize) -> Result<String> {
        let mut d = git(repo, &["diff", "--no-color", "--no-renames", base_commit, branch]).await?;
        if d.len() > max_bytes {
            let cut = (0..=max_bytes).rev().find(|i| d.is_char_boundary(*i)).unwrap_or(0);
            d.truncate(cut);
            d.push_str("\n\n… diff 过长，已截断 …\n");
        }
        Ok(d)
    }

    /// 预检把任务分支合入基础分支是否冲突；只读，不会改动任何工作目录。
    pub async fn merge_check(&self, repo: &Path, base_branch: &str, branch: &str) -> Result<MergeCheck> {
        let o = run_git(repo, &["merge-tree", "--write-tree", "--name-only", "--no-messages", base_branch, branch]).await?;
        match o.code {
            Some(0) => Ok(MergeCheck { clean: true, conflicts: vec![] }),
            Some(1) => {
                // 第一行是树对象，其后到空行之前是冲突文件
                let conflicts = o.stdout.lines().skip(1).take_while(|l| !l.is_empty()).map(String::from).collect();
                Ok(MergeCheck { clean: false, conflicts })
            }
            _ => bail!("预检合并失败: {}", o.stderr),
        }
    }

    /// 以 squash 方式把任务分支合入主工作区的基础分支。
    pub async fn merge(&self, repo: &Path, base_branch: &str, branch: &str, author: &str, message: &str) -> Result<MergeOutcome> {
        let lock = self.lock_for(repo);
        let _g = lock.lock().await;
        let st = self.repo_status(repo).await?;
        if st.branch.as_deref() != Some(base_branch) {
            return Ok(MergeOutcome::Blocked {
                reason: format!(
                    "主工作区当前在分支 {}，需要先切回 {base_branch} 才能合并",
                    st.branch.as_deref().unwrap_or("（游离 HEAD）")
                ),
            });
        }
        if st.dirty {
            return Ok(MergeOutcome::Blocked { reason: "主工作区有未提交的改动，请先提交或暂存，以免被合并覆盖".into() });
        }
        let check = self.merge_check(repo, base_branch, branch).await?;
        if !check.clean {
            return Ok(MergeOutcome::Conflict { files: check.conflicts });
        }
        let sq = run_git(repo, &["merge", "--squash", branch]).await?;
        if !sq.ok {
            let _ = run_git(repo, &["reset", "--merge"]).await;
            return Ok(MergeOutcome::Blocked { reason: format!("合并失败: {}", sq.stderr) });
        }
        if run_git(repo, &["diff", "--cached", "--quiet"]).await?.ok {
            return Ok(MergeOutcome::Empty);
        }
        git_as(repo, author, &["commit", "-q", "-m", message]).await?;
        Ok(MergeOutcome::Merged { commit: git(repo, &["rev-parse", "HEAD"]).await? })
    }

    /// 尽力删除已合并的任务分支（仍被某个 worktree 检出时会失败，忽略即可）。
    pub async fn delete_branch_best_effort(&self, repo: &Path, branch: &str) {
        let _ = run_git(repo, &["branch", "-D", branch]).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_handles_special_names() {
        assert_eq!(safe_slug("alice", "1234567890"), "alice");
        assert_eq!(safe_slug("小明", "1234567890"), "e-12345678");
        assert_eq!(safe_slug("a/b", "1234567890"), "e-12345678");
        assert_eq!(safe_slug("..", "1234567890"), "e-12345678");
    }

    #[test]
    fn overlaps_only_for_shared_files() {
        let sets = vec![
            ("t1".to_string(), vec!["a.rs".to_string(), "b.rs".to_string()]),
            ("t2".to_string(), vec!["b.rs".to_string(), "c.rs".to_string()]),
            ("t3".to_string(), vec!["b.rs".to_string()]),
        ];
        let o = find_overlaps(&sets);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].path, "b.rs");
        assert_eq!(o[0].task_ids, vec!["t1", "t2", "t3"]);
        assert!(find_overlaps(&[("t1".into(), vec!["a".into()])]).is_empty());
    }
}
