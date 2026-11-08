//! 完整的真实演练（默认忽略，会消耗真实的 Claude 额度，约 8~10 次调用）：
//! 导入旧项目 → 派任务 → 对比基线 → 评审会议 → 纪要与记忆 → 议题拍板 → 合并 → 带记忆的第二个任务 → 成本与调度。
//!
//! 运行：WALK_DIR=/某个目录 cargo test -p crewforge-cli --test walkthrough -- --ignored --nocapture
//! 项目是一个带“历史遗留 bug”的小 Rust 账户库（withdraw 允许透支，对应的测试在导入前就是失败的）。

use crewforge_core::board::Actor;
use crewforge_core::db::Db;
use crewforge_core::memory::{MemoryFilter, NewMemory, is_none_marker};
use crewforge_core::onboarding::baseline::Origin;
use crewforge_core::orchestrator::Orchestrator;
use crewforge_core::provider::claude::{ApprovalRequest, Decision};
use crewforge_core::runner::{Approver, NoopObserver};
use crewforge_core::scheduler::{Level, Permit, Scheduler};
use crewforge_core::workspace::{MergeOutcome, WorkspaceManager};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 记录 Claude 真正请求过哪些需要审批的操作，以便发现策略太严或太松的地方。
/// 为了让演练能继续，非危险的命令放行，其余一律拒绝。
#[derive(Default)]
struct LoggingApprover(Mutex<Vec<String>>);
impl Approver for LoggingApprover {
    async fn ask(&self, employee: &str, req: &ApprovalRequest, reason: &str) -> Decision {
        let cmd = req.input.get("command").and_then(|c| c.as_str()).unwrap_or("");
        let dangerous = ["rm -rf", "sudo", "curl", "wget", "git push", "ssh"].iter().any(|p| cmd.contains(p));
        let allow = req.tool_name == "Bash" && !dangerous;
        self.0.lock().unwrap().push(format!("[{employee}] {} {} → {}（{reason}）", req.tool_name, if cmd.is_empty() { req.input.to_string() } else { cmd.to_string() }, if allow { "放行" } else { "拒绝" }));
        if allow { Decision::Allow } else { Decision::Deny("演练中不允许".into()) }
    }
}

struct Report(Vec<(bool, String)>);
impl Report {
    fn check(&mut self, ok: bool, what: &str, detail: impl std::fmt::Display) {
        println!("  {} {what}{}", if ok { "✓" } else { "✗" }, { let d = detail.to_string(); if d.is_empty() { d } else { format!("  —  {d}") } });
        self.0.push((ok, what.to_string()));
    }
    fn note(&self, s: impl std::fmt::Display) { println!("  · {s}"); }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn clip(s: &str, n: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= n { one } else { format!("{}…", one.chars().take(n).collect::<String>()) }
}

async fn permit(s: &Arc<Scheduler>, interactive: bool, r: &mut Report) -> Option<Permit> {
    let fut = async { if interactive { s.acquire_interactive().await.ok() } else { Some(s.acquire().await) } };
    let p = tokio::time::timeout(Duration::from_secs(20), fut).await.ok().flatten();
    if p.is_none() {
        let st = s.status();
        r.check(false, "调度器放行了这次调用", format!("{:?}：{}", st.level, st.reason));
    }
    p
}

const LIB_RS: &str = r#"//! 极简账户库（旧项目）。

#[derive(Debug, Default)]
pub struct Account {
    pub balance: i64,
}

impl Account {
    pub fn new() -> Self {
        Account { balance: 0 }
    }

    pub fn deposit(&mut self, amount: i64) {
        self.balance += amount;
    }

    pub fn withdraw(&mut self, amount: i64) -> Result<(), String> {
        // 历史遗留：这里没有检查余额，允许透支
        self.balance -= amount;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deposit_adds() {
        let mut a = Account::new();
        a.deposit(100);
        assert_eq!(a.balance, 100);
    }

    #[test]
    fn withdraw_reduces() {
        let mut a = Account::new();
        a.deposit(100);
        a.withdraw(30).unwrap();
        assert_eq!(a.balance, 70);
    }

    #[test]
    fn withdraw_rejects_overdraft() {
        let mut a = Account::new();
        a.deposit(100);
        assert!(a.withdraw(500).is_err());
    }
}
"#;

#[tokio::test]
#[ignore]
async fn full_walkthrough_with_real_claude() {
    let dir = std::env::var("WALK_DIR").map(std::path::PathBuf::from).unwrap_or_else(|_| tempfile::tempdir().unwrap().keep());
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let mut r = Report(vec![]);
    println!("\n演练目录：{}\n", dir.display());

    // ---------- 0. 造一个旧项目 ----------
    let repo = dir.join("ledger");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("Cargo.toml"), "[package]\nname = \"ledger\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
    std::fs::write(repo.join(".gitignore"), "/target\n").unwrap();
    std::fs::write(repo.join("README.md"), "# ledger\n极简账户库。\n").unwrap();
    std::fs::write(repo.join("src/lib.rs"), LIB_RS).unwrap();
    for a in [&["init", "-q", "-b", "main"][..], &["add", "-A"]] { git(&repo, a); }
    git(&repo, &["-c", "user.name=旧同事", "-c", "user.email=old@example.com", "commit", "-q", "-m", "初始版本：账户存取"]);
    std::fs::write(repo.join("README.md"), "# ledger\n极简账户库。\n\n## 已知问题\n- 提现的校验还没做\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["-c", "user.name=旧同事", "-c", "user.email=old@example.com", "commit", "-q", "-m", "文档：记录已知问题"]);

    let data = dir.join("home");
    let db = Arc::new(Db::open(&data).unwrap());
    let ws = Arc::new(WorkspaceManager::new(&data));
    let mut orch = Orchestrator::new(db.clone(), ws.clone());
    orch.mcp_exe = Some(env!("CARGO_BIN_EXE_crewforge").into());
    let sched = Scheduler::new(db.clone(), 2);
    let approver = LoggingApprover::default();

    let alice = db.add_employee("alice", "全栈工程师", "你是 Alice，后端方向的全栈工程师。沟通简洁，先给结论再给理由。").unwrap();
    let bob = db.add_employee("bob", "全栈工程师", "你是 Bob，认真的代码审查者。指出具体的文件和问题，不确定就提问。").unwrap();
    let project = db.create_project("Ledger", "极简账户库（旧项目）", repo.to_str().unwrap()).unwrap();
    let pid = project.id.clone();
    db.add_member(&pid, &alice.id, "lead").unwrap();
    db.add_member(&pid, &bob.id, "reviewer").unwrap();

    // ---------- 1. 导入旧项目 ----------
    println!("【1】导入旧项目");
    db.set_onboarding(&pid, "in_progress").unwrap();
    let blocked = orch.assign(&alice, &repo, "随便做点什么", &approver, &NoopObserver).await;
    r.check(blocked.as_ref().err().is_some_and(|e| e.to_string().contains("导入阶段")), "导入阶段不能派开发任务", "");

    let scan = orch.scan_project(&pid).await.unwrap();
    r.check(scan.languages.first().map(|l| l.0.as_str()) == Some("Rust"), "扫描识别出 Rust", format!("{:?}", scan.languages));
    r.check(scan.commands.iter().any(|c| c.id == "cargo-test"), "扫描建议了 cargo test", "");
    r.check(scan.git.commit_count == Some(2), "读到了 Git 历史", format!("{:?}", scan.git.recent));

    let cmds: Vec<_> = scan.commands.iter().filter(|c| c.default_on).cloned().collect();
    let base = orch.run_baseline(&pid, cmds, Duration::from_secs(600)).await.unwrap();
    for res in &base.results { r.note(format!("基线 {}：{} · {}s · 失败项 {:?}", res.label, res.status, res.duration_ms / 1000, res.failed_tests)); }
    let test = base.results.iter().find(|x| x.id == "cargo-test").unwrap();
    r.check(test.status == "failed" && test.parsed && test.failed_tests == vec!["tests::withdraw_rejects_overdraft".to_string()],
        "真实 cargo 基线：解析出导入前就存在的失败测试", format!("{:?}", test.failed_tests));
    r.check(git(&repo, &["status", "--porcelain"]).is_empty(), "基线没有弄脏用户的目录", "");

    for (kind, emp) in [("architect", &alice), ("qa", &bob)] {
        let Some(_p) = permit(&sched, true, &mut r).await else { return finish(r, &approver) };
        match orch.evaluate_project(&pid, kind, emp, &approver).await {
            Ok(text) => {
                r.check(text.contains("##"), &format!("{kind} 评估给出了结构化报告"), clip(&text, 160));
                r.check(text.contains("withdraw") || text.contains("透支"), &format!("{kind} 评估提到了透支这个历史问题"), "");
            }
            Err(e) => r.check(false, &format!("{kind} 评估"), e),
        }
    }
    orch.confirm_onboarding(&pid, false).unwrap();
    let mems = db.list_memories(&MemoryFilter { project_id: Some(pid.clone()), ..Default::default() }).unwrap();
    r.check(mems.len() == 2 && mems.iter().all(|m| m.status == "verified"), "确认导入后沉淀了 2 条已验证的项目记忆", mems.iter().map(|m| m.title.clone()).collect::<Vec<_>>().join(" / "));

    // ---------- 2. 派开发任务 ----------
    println!("\n【2】alice 开发 transfer");
    let Some(p) = permit(&sched, false, &mut r).await else { return finish(r, &approver) };
    let t1 = orch.assign(&alice, &repo, "给 Account 增加 transfer 方法：把金额从一个账户转到另一个账户，余额不足时返回错误，并且两边余额都保持不变。补充单元测试，并运行 cargo test 确认结果。", &approver, &NoopObserver).await;
    drop(p);
    match &t1 {
        Ok(res) => { r.check(!res.is_error, "任务执行完成", format!("费用 {:?} · {}", res.cost_usd, clip(&res.text, 200))); }
        Err(e) => r.check(false, "任务执行", e),
    }
    let views = orch.task_views(&repo).await.unwrap();
    let Some(v1) = views.first() else { r.check(false, "任务已创建", ""); return finish(r, &approver) };
    let task1 = v1.task.clone();
    r.check(v1.files.iter().any(|f| f.path == "src/lib.rs"), "改动落在员工自己的 worktree 里", format!("{:?}", v1.files.iter().map(|f| &f.path).collect::<Vec<_>>()));
    r.check(git(&repo, &["status", "--porcelain"]).is_empty() && !std::fs::read_to_string(repo.join("src/lib.rs")).unwrap().contains("transfer"), "用户的主工作区没有被动", "");
    let wt = Path::new(&task1.worktree_path);
    r.check(git(wt, &["log", "-1", "--format=%an"]) == "alice", "检查点以 alice 署名", git(wt, &["log", "-1", "--format=%s"]));
    let diff = orch.task_diff(&task1.id).await.unwrap();
    r.check(diff.contains("fn transfer"), "diff 里有 transfer", format!("{} 字节", diff.len()));
    r.note(format!("alice 期间请求过的审批：{}", if approver.0.lock().unwrap().is_empty() { "无".into() } else { format!("\n      {}", approver.0.lock().unwrap().join("\n      ")) }));

    // ---------- 3. 对比基线 ----------
    println!("\n【3】对比基线检查");
    let check = orch.check_task(&task1.id, Duration::from_secs(600)).await.unwrap();
    for f in &check.findings { r.note(format!("{:?} · {} · {}", f.origin, f.item, f.note)); }
    r.check(check.introduced == 0, "alice 的修改没有引入新的失败", format!("引入 {} · 导入前已存在 {} · 已修复 {} · 无法确认 {}", check.introduced, check.pre_existing, check.fixed, check.unknown));
    r.check(check.pre_existing + check.fixed >= 1, "历史遗留的透支测试被正确归类（已存在或被修复），没有算成 AI 引入",
        check.findings.iter().find(|f| f.item.contains("overdraft")).map(|f| format!("{:?}", f.origin)).unwrap_or_default());
    r.check(check.findings.iter().all(|f| f.origin != Origin::Introduced || !f.item.contains("overdraft")), "没有把历史问题归咎于 AI", "");

    // ---------- 4. 评审会议 ----------
    println!("\n【4】代码评审会议");
    let m = db.create_meeting(&pid, "code_review", "评审：transfer", None, Some(&task1.id), &[], 6).unwrap();
    db.post_message(&m.id, "human", None, "@bob 请评审 alice 的改动，重点看 transfer 的边界情况。如果发现需要团队决定的设计问题，请先 list_issues 查重，再用 propose_issue 提议。").unwrap();
    if let Some(_p) = permit(&sched, true, &mut r).await {
        match orch.meeting_reply(&m.id, &bob, &approver).await {
            Ok(text) => r.check(text.trim().chars().count() > 20, "bob 在会议里给出了评审意见", clip(&text, 260)),
            Err(e) => r.check(false, "bob 评审发言", e),
        }
    }
    r.check(git(wt, &["status", "--porcelain"]).is_empty() && git(&repo, &["status", "--porcelain"]).is_empty(), "评审是只读的，没有改动任何文件", "");
    let proposals: Vec<_> = db.recent_events_for_project(&pid, 500).unwrap().into_iter().filter(|e| e.kind == "agent_proposed").collect();
    r.note(format!("bob 提议了 {} 项：{:?}", proposals.len(), proposals.iter().map(|e| format!("{}《{}》", e.payload["type"], e.payload["title"])).collect::<Vec<_>>()));

    db.post_message(&m.id, "human", None, "@alice 你同意 bob 的意见吗？简短回复。").unwrap();
    if let Some(_p) = permit(&sched, true, &mut r).await {
        match orch.meeting_reply(&m.id, &alice, &approver).await {
            Ok(text) => r.check(!text.trim().is_empty(), "alice 回应了（只收到新增消息）", clip(&text, 200)),
            Err(e) => r.check(false, "alice 回应", e),
        }
    }
    if let Some(_p) = permit(&sched, true, &mut r).await {
        match orch.draft_minutes(&m.id, &approver).await {
            Ok(min) => {
                r.check(["## 决议", "## 行动项", "## 待验证的经验"].iter().all(|h| min.contains(h)), "主持人起草了结构化纪要", "");
                println!("      纪要：{}", clip(&min, 500));
            }
            Err(e) => r.check(false, "起草纪要", e),
        }
    }
    // 评审里提出的问题不会自动拦住合并，所以任务卡必须能在合并前把评审和行动项摆出来
    let rv = orch.task_views(&repo).await.unwrap().into_iter().find(|v| v.task.id == task1.id).map(|v| v.reviews).unwrap_or_default();
    r.check(rv.len() == 1 && rv[0].status == "open" && !rv[0].action_items.is_empty(), "合并前任务卡能看到评审会议和它的行动项",
        rv.first().map(|x| format!("{} 条行动项", x.action_items.len())).unwrap_or_default());
    let saved = db.save_meeting_lessons(&m.id);
    r.note(format!("纪要里的待验证经验 → 记忆：{}", match &saved { Ok(n) => format!("存入 {n} 条"), Err(e) => format!("未存入（{e}）") }));
    // 只看“有没有存进去”不够：存进去的必须是真正的经验，而不是“无。本次没有新增”这类空话
    let from_meeting = db.list_memories(&MemoryFilter { project_id: Some(pid.clone()), ..Default::default() }).unwrap().into_iter().filter(|x| x.source_kind == "meeting").collect::<Vec<_>>();
    r.check(from_meeting.iter().all(|x| !is_none_marker(&x.content)), "纪要里“无”的小节没有被当成经验存进记忆",
        format!("{:?}", from_meeting.iter().map(|x| clip(&x.content, 40)).collect::<Vec<_>>()));
    let pending: Vec<_> = db.list_memories(&MemoryFilter { project_id: Some(pid.clone()), status: Some("unverified".into()), ..Default::default() }).unwrap();
    r.check(pending.iter().all(|x| x.task_id.as_deref() == Some(task1.id.as_str()) || x.source_kind == "agent"), "待验证的经验都有来源，且没有直接生效", format!("{} 条待验证", pending.len()));

    // ---------- 5. 议题 ----------
    println!("\n【5】议题与拍板");
    let ai_issue = db.list_issues(&pid).unwrap().into_iter().find(|i| i.raised_by.is_some());
    let issue = match ai_issue {
        Some(i) => { r.note(format!("采用 AI 提议的议题《{}》（状态 {}）", i.title, i.status)); r.check(i.status == "open", "AI 提议的议题进入“待讨论”，等人类确认", ""); i }
        None => { r.note("bob 没有提议议题，由人类创建一个来走完拍板流程"); db.create_issue(&pid, "金额要不要改用 Decimal", "目前用 i64 表示分，是否值得引入 Decimal 类型", "technical", false, None, None, None).unwrap() }
    };
    db.add_option(&issue.id, "保持 i64（分）", "简单，无依赖", None).unwrap();
    db.add_option(&issue.id, "引入 Decimal", "表达更直观，多一个依赖", None).unwrap();
    db.set_issue_status(&issue.id, "discussing").unwrap();
    db.set_issue_status(&issue.id, "pending_decision").unwrap();
    r.check(db.pending_decisions().unwrap().iter().any(|p| p.issue.id == issue.id), "待决策的议题出现在人类待办里", "");
    let opts = db.issue_options(&issue.id).unwrap();
    let chosen = opts.iter().find(|o| o.title.starts_with("保持")).expect("找到“保持 i64”这个选项");
    db.decide_issue(&issue.id, &Actor::Human, Some(&chosen.id), "保持 i64（分）", "目前没有小数需求，避免多余依赖", &Default::default()).unwrap();
    let dm = db.list_memories(&MemoryFilter { project_id: Some(pid.clone()), ..Default::default() }).unwrap();
    r.check(dm.iter().any(|x| x.kind == "decision" && x.source_ref.as_deref() == Some(issue.id.as_str()) && x.status == "verified"), "决议自动沉淀为已验证的项目决策记忆", "");
    r.check(dm.iter().filter(|x| x.kind == "decision").all(|x| !x.content.contains("未采纳「保持 i64（分）」")), "决策记忆没有自相矛盾（选中的方案不会同时出现在“未采纳”里）", "");

    // ---------- 6. 合并 ----------
    println!("\n【6】合并 alice 的任务");
    let outcome = orch.merge_task(&task1.id).await.unwrap();
    let merged_ok = matches!(outcome, MergeOutcome::Merged { .. });
    r.check(merged_ok, "squash 合并成功", format!("{outcome:?}"));
    r.check(std::fs::read_to_string(repo.join("src/lib.rs")).unwrap().contains("fn transfer"), "主分支上有了 transfer", "");
    let after = db.list_memories(&MemoryFilter { project_id: Some(pid.clone()), status: Some("verified".into()), ..Default::default() }).unwrap();
    r.check(pending.iter().filter(|x| x.task_id.is_some()).all(|x| after.iter().any(|a| a.id == x.id && a.verified_by.as_deref() == Some("merge"))), "关联该任务的经验在合并后自动验证", "");

    // ---------- 7. 带记忆的第二个任务 ----------
    println!("\n【7】bob 的第二个任务（带记忆注入）");
    db.add_memory(NewMemory { layer: "project".into(), project_id: Some(pid.clone()), kind: "convention".into(), title: String::new(),
        content: "Account 的金额一律用 i64 表示分，不要用浮点数，也不要引入 Decimal。".into(), source_kind: "human".into(), created_by: "human".into(), ..Default::default() }).unwrap();
    let Some(p) = permit(&sched, false, &mut r).await else { return finish(r, &approver) };
    let t2 = orch.assign(&bob, &repo, "给 Account 增加 apply_interest(rate_bp: i64) 方法：按基点计算利息并加到余额。补测试并运行 cargo test。", &approver, &NoopObserver).await;
    drop(p);
    match &t2 { Ok(res) => r.check(!res.is_error, "bob 完成了任务", clip(&res.text, 160)), Err(e) => r.check(false, "bob 的任务", e) }
    let inj = db.recent_events(2000).unwrap().into_iter().filter(|e| e.kind == "memory_injected" && e.employee_id.as_deref() == Some(bob.id.as_str())).last();
    r.check(inj.as_ref().is_some_and(|e| e.payload["count"].as_u64().unwrap_or(0) >= 1), "新任务开始时注入了相关记忆", inj.map(|e| format!("{} 条 / {} 字", e.payload["count"], e.payload["chars"])).unwrap_or_default());
    let v2 = orch.task_views(&repo).await.unwrap().into_iter().find(|v| v.employee_name == "bob");
    if let Some(v2) = v2 {
        let d2 = orch.task_diff(&v2.task.id).await.unwrap();
        r.check(d2.contains("apply_interest"), "bob 的 diff 里有 apply_interest", "");
        r.check(!d2.contains("f64") && !d2.contains("Decimal"), "遵守了记忆里的约定（没用浮点或 Decimal）", "");
        let c2 = orch.check_task(&v2.task.id, Duration::from_secs(600)).await.unwrap();
        r.check(c2.introduced == 0, "bob 的修改没有引入新的失败", format!("引入 {} · 已存在 {} · 已修复 {} · 无法确认 {}", c2.introduced, c2.pre_existing, c2.fixed, c2.unknown));
        let o2 = orch.merge_task(&v2.task.id).await.unwrap();
        r.check(matches!(o2, MergeOutcome::Merged { .. }), "第二个任务合并成功（基于合并后的新基础，无冲突）", format!("{o2:?}"));
    } else { r.check(false, "bob 的任务已创建", ""); }
    r.check(git(&repo, &["status", "--porcelain"]).is_empty(), "全程用户的主工作区保持干净", "");
    r.check(git(&repo, &["worktree", "list"]).lines().count() == 3, "只剩两个员工 worktree，没有残留的临时检出", git(&repo, &["worktree", "list"]).lines().count().to_string());

    // ---------- 8. 成本与额度 ----------
    println!("\n【8】成本、额度与调度");
    let cost = db.cost_report(None, 480).unwrap();
    let turn_events = db.recent_events(5000).unwrap().into_iter().filter(|e| e.kind == "turn_result").count();
    r.check(cost.totals.turns as usize == turn_events && turn_events >= 6, "成本统计的调用次数与事件一致", format!("{} 次", cost.totals.turns));
    r.check(cost.by_project.iter().all(|g| g.label == "Ledger"), "所有调用都归属到了项目（没有“未归属”）", cost.by_project.iter().map(|g| g.label.clone()).collect::<Vec<_>>().join("、"));
    r.note(format!("估算费用 ${:.3} · 上下文 {} token · 输出 {} · 缓存命中 {:?}", cost.totals.cost_usd, cost.totals.context_tokens(), cost.totals.output_tokens, cost.efficiency.cache_hit_ratio.map(|v| format!("{:.0}%", v * 100.0))));
    for g in &cost.by_purpose { r.note(format!("{}：{} 次 · ${:.3}", g.label, g.totals.turns, g.totals.cost_usd)); }
    for g in &cost.by_employee { r.note(format!("{}：{} 次 · ${:.3}", g.label, g.totals.turns, g.totals.cost_usd)); }
    r.note(format!("记忆注入 {} 次 · 会话复用 {}/{}", cost.efficiency.memory_injections, cost.efficiency.resumed_starts, cost.efficiency.total_starts));
    let st = sched.status();
    r.check(st.quota.is_some(), "读到了真实的额度数据", st.quota.as_ref().map(|q| format!("5h {:?} · 7d {:?} · 状态 {} · 5h 重置 {:?}", q.five_hour, q.seven_day, q.status, q.five_resets_at)).unwrap_or_default());
    r.note(format!("调度器：{:?}（有效并发 {}/{}）— {}", st.level, st.effective_limit, st.user_limit, st.reason));
    r.check(st.level != Level::Paused, "演练结束时调度器没有暂停", "");

    finish(r, &approver);
}

fn finish(r: Report, approver: &LoggingApprover) {
    let bad: Vec<_> = r.0.iter().filter(|(ok, _)| !ok).map(|(_, w)| w.clone()).collect();
    println!("\n====== 演练结果：{} 项通过，{} 项未通过 ======", r.0.len() - bad.len(), bad.len());
    for b in &bad { println!("  ✗ {b}"); }
    let asks = approver.0.lock().unwrap();
    println!("演练期间 Claude 共请求了 {} 次需要审批的操作", asks.len());
    for a in asks.iter() { println!("  - {a}"); }
    assert!(bad.is_empty(), "有 {} 项未通过", bad.len());
}
