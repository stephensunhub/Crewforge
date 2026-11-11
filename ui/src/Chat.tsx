import { useEffect, useMemo, useRef, useState } from "react";
import { api } from "./api";
import { Avatar, formatApprovalInput, hhmm, short } from "./common";
import type { ApprovalReq, EventRow, Member, Project, RepoStatus, TaskView } from "./types";
import type { ProjectTab } from "./ProjectView";

function toolSummary(input: Record<string, unknown> | undefined): string {
  if (!input) return "";
  const v = input.file_path ?? input.command ?? input.pattern ?? input.path ?? input.url ?? "";
  return typeof v === "string" ? short(v) : "";
}

export function Chat({
  project, members, events, nameById, running, approvals, concurrency, repo, taskViews, onboarding, onGoOnboarding, onGoTab, onOpenMeeting, onDecide, onError, onInitGit,
}: {
  project: Project;
  members: Member[];
  events: EventRow[];
  nameById: Map<string, string>;
  running: string[];
  approvals: ApprovalReq[];
  concurrency: number;
  repo: RepoStatus | null;
  taskViews: TaskView[];
  onboarding: string;
  onGoOnboarding: () => void;
  onGoTab: (t: ProjectTab) => void;
  onOpenMeeting: (id: string) => void;
  onDecide: (a: ApprovalReq, allow: boolean) => void;
  onError: (m: string) => void;
  onInitGit: () => void;
}) {
  const [text, setText] = useState("");
  const [verbose, setVerbose] = useState(false);
  const feedRef = useRef<HTMLDivElement>(null);
  const stick = useRef(true);

  const mine = useMemo(() => events.filter((e) => e.project_id === project.id), [events, project.id]);
  const names = members.map((m) => m.name);

  useEffect(() => {
    if (stick.current) feedRef.current?.scrollTo({ top: feedRef.current.scrollHeight });
  }, [mine.length, approvals.length]);

  const queued = useMemo(() => {
    const q = new Set<string>();
    for (const e of mine) {
      const n = e.employee_id ? nameById.get(e.employee_id) : undefined;
      if (!n) continue;
      if (e.kind === "task_queued") q.add(n);
      else if (["task_started", "task_failed", "task_error"].includes(e.kind)) q.delete(n);
    }
    for (const n of [...q]) if (!running.includes(n)) q.delete(n);
    return q;
  }, [mine, nameById, running]);

  const problem = !repo ? null : !repo.is_repo ? "not_repo" : !repo.has_commits ? "no_commits" : !repo.branch ? "detached" : null;
  const activeTasks = taskViews.filter((v) => v.task.status === "active");

  const mention = /@([^\s@]*)$/.exec(text);
  const suggestions = mention
    ? names.filter((n) => n.toLowerCase().startsWith(mention[1].toLowerCase())).slice(0, 8)
    : names.slice(0, 6);
  const complete = (n: string) =>
    setText((t) => (mention ? t.replace(/@([^\s@]*)$/, `@${n} `) : `${t}${t && !t.endsWith(" ") ? " " : ""}@${n} `));

  async function send() {
    const body = text.trim();
    if (!body) return;
    if (onboarding === "in_progress") { onError("项目还在导入阶段，请先到“导入”页完成扫描、基线和确认，之后才能派任务"); return; }
    if (problem) { onError("这个项目的 Git 仓库还不能用，请先处理上方提示"); return; }
    const targets = [...new Set([...body.matchAll(/@([^\s@]+)/g)].map((m) => m[1]).filter((n) => names.includes(n)))];
    if (targets.length === 0) {
      onError(names.length ? "请用 @员工名 指定要找谁，例如 @" + names[0] : "项目里还没有成员，请先到「成员」页添加");
      return;
    }
    const prompt = body.replace(/@([^\s@]+)\s*/g, (m, n) => (targets.includes(n) ? "" : m)).trim() || body;
    setText("");
    for (const t of targets) {
      try { await api.runTask(t, prompt, project.id); } catch (e) { onError(String(e)); }
    }
  }

  const who = (e: EventRow) => (e.employee_id ? nameById.get(e.employee_id) ?? "未知员工" : "系统");

  // 点击跳到相关页面；会议直接打开那场会议
  const jump = (tab: ProjectTab, meetingId?: unknown) => ({
    onClick: () => { if (typeof meetingId === "string" && meetingId) onOpenMeeting(meetingId); else onGoTab(tab); },
    style: { cursor: "pointer" } as const,
    title: "点击跳转",
  });
  const TAB_NAME: Record<string, string> = { duties: "职责", plan: "计划", tasks: "任务", issues: "议题", meetings: "会议", actions: "行动项", members: "成员", memory: "记忆", onboarding: "导入" };

  /** 项目级事件：[图标, 文字, 色调, 跳转页]。没有列在这里的事件（工具调用细节、限额读数、会议逐条消息、调度状态、记忆注入）太碎，不进群聊。 */
  function projectLine(e: EventRow, name: string): [string, string, string, ProjectTab] | null {
    const p = e.payload ?? {};
    const text = String(p.text ?? "");
    switch (e.kind) {
      case "autopilot_started": return ["▶", text, "ok", "duties"];
      case "autopilot_finished": return [p.status === "finished" ? "🏁" : p.status === "stopped" ? "⏹" : p.status === "paused" ? "⏸" : p.status === "budget" ? "💰" : "✋", text, p.status === "finished" ? "ok" : p.status === "failed" || p.status === "escalated" ? "bad" : "warn", "duties"];
      case "autopilot_step": return ["🧭", text, "", "duties"];
      case "autopilot_merged": return ["🔀", text, "ok", "plan"];
      case "autopilot_decided": return ["⚖", text, "", "issues"];
      case "autopilot_issue_escalated": return ["🙋", text, "warn", "issues"];
      case "lead_triaged_actions": return ["🧹", text, "", "actions"];
      case "lead_decided_action": return ["⚖", `${name === "系统" ? "" : name + " "}处理了需要人决定的事项：${short(String(p.decision ?? ""), 100)}`, "", "actions"];
      case "plan_drafted_by_lead": return ["📋", text, "warn", "plan"];
      case "stuck_detected": return ["🚨", String(p.title ?? "有员工卡住") + (p.detail ? `：${short(String(p.detail), 120)}` : ""), "warn", "duties"];
      case "handoff_started": case "handoff_finished": return ["🔄", text, "", "members"];
      case "session_rotated": return ["🔄", `${name} 换新会话（上下文 ${Math.round(Number(p.context_tokens ?? 0) / 1000)}K，上限 ${Math.round(Number(p.limit ?? 0) / 1000)}K）`, "", "members"];
      case "meeting_started": return ["📣", `会议开始：${p.title ?? ""}`, "", "meetings"];
      case "meeting_closed": return ["📣", `会议结束：${p.title ?? ""}`, "", "meetings"];
      case "alignment_done": return ["🧭", `对齐检查：${short(String(p.check ?? ""), 100)} ${short(String(p.goal_note ?? ""), 100)}`, "", "meetings"];
      case "issue_created": return ["💡", `新议题《${p.title ?? ""}》`, "", "issues"];
      case "issue_status": return ["💡", `议题《${p.title ?? ""}》→ ${p.to}`, "", "issues"];
      case "issue_decided": return ["🗳", `议题《${p.title ?? ""}》已定：${short(String(p.decision ?? ""), 100)}`, "ok", "issues"];
      case "memory_created": return ["🧠", `新增记忆《${p.title ?? ""}》`, "", "memory"];
      case "task_checked": return ["🔎", `${name} 的任务验证完成：修好 ${p.fixed ?? 0}，新引入 ${p.introduced ?? 0}，原有 ${p.pre_existing ?? 0}，未知 ${p.unknown ?? 0}`, Number(p.introduced ?? 0) > 0 ? "warn" : "", "tasks"];
      case "report_flags": return ["📝", `汇报有问题：${((p.flags as string[] | undefined) ?? []).join("；")}`, "warn", "tasks"];
      case "onboarding_scanned": return ["📂", "项目扫描完成", "", "onboarding"];
      case "baseline_started": return ["🧪", "开始跑测试基线", "", "onboarding"];
      case "baseline_finished": return ["🧪", `测试基线完成，失败 ${p.failed ?? 0} 项`, Number(p.failed ?? 0) > 0 ? "warn" : "ok", "onboarding"];
      case "onboarding_confirmed": return ["✅", "导入已确认", "ok", "onboarding"];
      case "issue_ruling_failed": return ["⚠", text || "议题裁决没有完成", "bad", "issues"];
      case "lead_format_retry": return ["↻", text || "负责人回复格式不对，重试中", "warn", "duties"];
      default: return null;
    }
  }

  function render(e: EventRow) {
    const name = who(e);
    const p = e.payload ?? {};
    const t = <span className="time">{hhmm(e.ts)}</span>;
    switch (e.kind) {
      case "task_started": {
        // 谁发起的：空 = 你；autopilot = 自主推进里 AI 负责人派的活；system = 系统或负责人的后台工作
        const origin = String(p.origin ?? "");
        if (origin) {
          const label = origin === "autopilot" ? "自主推进" : "系统";
          return (
            <div className="msg" key={e.id}>
              <div>
                <div className="who">{label} → @{name}{p.resume ? " · 续接会话" : ""}</div>
                <div className="bubble">{p.prompt}</div>
              </div>
            </div>
          );
        }
        return (
          <div className="msg me" key={e.id}>
            <div>
              <div className="who" style={{ textAlign: "right" }}>你 → @{name}{p.resume ? " · 续接会话" : ""}</div>
              <div className="bubble">{p.prompt}</div>
            </div>
          </div>
        );
      }
      case "task_queued":
        return <div className="line warn" key={e.id}><span>⏸ {name} 排队中：{p.reason ?? `并发已满（上限 ${concurrency}）`}，条件允许后自动开始</span>{t}</div>;
      case "task_created":
        return <div className="line" key={e.id} {...jump("tasks")}><span>🌿 {name} 开始新任务</span><span className="t">{p.title}</span><code>{p.branch}</code>{t}</div>;
      case "checkpoint_committed":
        return <div className="line" key={e.id} {...jump("tasks")}><span>💾 {name} 保存检查点</span><span>{p.files} 个文件 <span className="add">+{p.insertions}</span> <span className="del">−{p.deletions}</span></span><code>{String(p.commit).slice(0, 8)}</code>{t}</div>;
      case "checkpoint_failed":
        return <div className="line bad" key={e.id}><span>✗ {name} 检查点保存失败：{short(String(p.error ?? ""), 160)}</span>{t}</div>;
      case "task_merged":
        return <div className="line ok" key={e.id} {...jump("tasks")}><span>🔀 已合并 {name} 的任务</span><span className="t">{p.title}</span>{p.commit && <code>{String(p.commit).slice(0, 8)}</code>}{t}</div>;
      case "task_merge_blocked":
        return <div className="line bad" key={e.id} {...jump("tasks")}><span>⛔ {name} 的任务未能合并</span><span>{p.kind === "conflict" ? `冲突：${(p.files ?? []).join("、")}` : p.kind === "action_items" ? `评审里还有 ${(p.items ?? []).length} 条合并前行动项没处理` : p.reason}</span>{t}</div>;
      case "merged_with_open_items":
        return <div className="line warn" key={e.id} {...jump("tasks")}><span>⚠ {name} 的任务在还有 {(p.items ?? []).length} 条未处理的行动项时被确认合并</span>{t}</div>;
      case "action_item_status":
        return <div className="line" key={e.id} {...jump("actions")}><span>{p.to === "done" ? "✅ 行动项已处理" : p.to === "dismissed" ? "🚫 行动项已驳回" : "↩ 行动项重新打开"}</span><span className="t">{short(String(p.text ?? ""), 80)}</span>{t}</div>;
      case "task_abandoned":
        return <div className="line" key={e.id} {...jump("tasks")}><span>🗑 {name} 的任务已放弃（分支 {p.branch} 保留）</span>{t}</div>;
      case "agent_text":
        return (
          <div className="msg" key={e.id}>
            <Avatar name={name} />
            <div><div className="who">{name} · {hhmm(e.ts)}</div><div className="bubble">{p.text}</div></div>
          </div>
        );
      case "tool_use":
        return <div className="line" key={e.id}><span>🔧 {name}</span><span className="t">{p.tool}</span><code>{toolSummary(p.input)}</code>{t}</div>;
      case "approval_requested":
        return <div className="line warn" key={e.id}><span>⏳ {name} 请求审批</span><span className="t">{p.reason}</span>{t}</div>;
      case "approval_decided":
        if (p.decided_by === "policy" && !verbose) return null;
        return (
          <div className={`line ${p.decision === "allow" ? "ok" : "bad"}`} key={e.id}>
            <span>{p.decision === "allow" ? "✓ 已允许" : "✗ 已拒绝"}</span><span className="t">{p.tool}</span>
            <span>{p.decided_by === "human" ? "人类决定" : "策略自动放行"}</span>{t}
          </div>
        );
      case "turn_result":
        return (
          <div className={`line ${p.is_error ? "bad" : ""}`} key={e.id}>
            <span>{p.is_error ? "⚠ 执行出错" : "✔ 本轮完成"} · {name}</span>
            <span>{p.cost_usd != null ? `≈$${Number(p.cost_usd).toFixed(3)} · ` : ""}输入 {p.input_tokens ?? "?"} / 输出 {p.output_tokens ?? "?"}</span>{t}
          </div>
        );
      case "session_recovered":
        return <div className="line warn" key={e.id}><span>↻ {name} 的上次任务被中断，已恢复会话并核对状态</span>{t}</div>;
      case "task_failed":
      case "task_error":
        return <div className="line bad" key={e.id}><span>✗ {name} 任务失败：{short(String(p.error ?? ""), 200)}</span>{t}</div>;
      case "plan_item_status":
        return <div className="line" key={e.id} {...jump("plan")}><span>{p.to === "done" ? "✅" : p.to === "in_progress" ? "▶" : "📋"} 计划条目《{short(String(p.title ?? ""), 40)}》→ {({ planned: "待开始", in_progress: "进行中", done: "已完成", blocked: "已阻塞", cancelled: "已取消" } as Record<string, string>)[p.to] ?? p.to}</span>{t}</div>;
      case "plan_approved":
        return <div className="line ok" key={e.id} {...jump("plan")}><span>📋 已批准 {p.count} 个计划条目</span>{t}</div>;
      case "agent_proposed": {
        const icons: Record<string, string> = { issue: "💡", option: "➕", request_decision: "🗳", memory: "🧠", plan: "📋" };
        const text: Record<string, string> = {
          issue: `提议了议题《${p.title}》，等你确认`, option: `给《${p.issue_title}》补充了方案《${p.title}》`,
          request_decision: `请求拍板《${p.title}》${p.note ? `：${p.note}` : ""}`, memory: `提议了一条经验《${p.title}》，等你验证`, plan: `起草了 ${p.count} 个计划条目，等你确认`,
        };
        return <div className="line warn" key={e.id}><span>{icons[p.type] ?? "•"} {name} {text[p.type] ?? "提出了建议"}</span>{t}</div>;
      }
      case "project_created":
        return <div className="line" key={e.id}><span>📁 项目「{p.name}」已创建</span>{t}</div>;
      default: {
        const pl = projectLine(e, name);
        if (!pl) return null;
        const [icon, txt, tone, tab] = pl;
        const mid = e.kind.startsWith("meeting_") || e.kind === "alignment_done" ? p.meeting_id : undefined;
        return <div className={`line ${tone}`} key={e.id} {...jump(tab, mid)}><span>{icon} {txt}</span><span className="hint">→ {TAB_NAME[tab] ?? tab}</span>{t}</div>;
      }
    }
  }

  const hasChat = mine.some((e) => ["task_started", "agent_text"].includes(e.kind));
  const cont = !mention ? activeTasks.filter((v) => [...text.matchAll(/@([^\s@]+)/g)].some((m) => m[1] === v.employee_name)) : [];

  return (
    <div className="col">
      {onboarding === "in_progress" && (
        <div className="banner info"><span>项目还在导入阶段：完成扫描、测试基线并确认后，才能给员工派开发任务。</span><button className="btn" onClick={onGoOnboarding}>去导入</button></div>
      )}
      {problem === "not_repo" && (
        <div className="banner info"><span>这个目录还不是 Git 仓库。员工需要在隔离的 Git 工作区里工作。</span><button className="btn" onClick={onInitGit}>初始化 Git 仓库</button></div>
      )}
      {problem === "no_commits" && <div className="banner info"><span>仓库还没有任何提交，无法创建员工工作区。请先在项目里提交一次。</span></div>}
      {problem === "detached" && <div className="banner info"><span>主工作区处于游离 HEAD，请先切换到某个分支。</span></div>}

      <div className="feed" ref={feedRef} onScroll={(e) => {
        const el = e.currentTarget;
        stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
      }}>
        {!hasChat && (
          <div className="empty">
            <img src="/logo.png" alt="" />
            <h2>{members.length ? "项目群已就绪" : "先添加项目成员"}</h2>
            <p>{members.length
              ? <>用 <code>@{members[0].name}</code> 给员工分配任务，可以同时 @ 多人；并发超过上限时自动排队。</>
              : "到「成员」页，把公司里的员工加入这个项目，并指定负责人。"}</p>
          </div>
        )}
        {mine.map(render)}
      </div>

      {approvals.length > 0 && (
        <div className="approvals">
          {approvals.map((a) => (
            <div className="apv" key={a.id}>
              <b>⚠ {a.employee} 请求审批 · {a.tool}</b>
              <div className="hint">{a.reason}</div>
              <pre>{formatApprovalInput(a.tool, a.input)}</pre>
              <div className="btns">
                <button className="btn primary" onClick={() => onDecide(a, true)}>允许</button>
                {a.tool === "SandboxNetworkAccess" && typeof a.input?.host === "string" && (
                  <button className="btn" title="这次允许，并把这个域名加入全局白名单，以后所有员工都不再为它询问你（在设置里可以移除）"
                    onClick={() => { void api.addSandboxDomainChecked(String(a.input.host)).then((ok) => { if (ok) onDecide(a, true); }).catch((e) => onError(String(e))); }}>允许并加入白名单</button>
                )}
                <button className="btn danger" onClick={() => onDecide(a, false)}>拒绝</button>
              </div>
            </div>
          ))}
        </div>
      )}

      <footer className="composer">
        <div className="chips">
          <span className="hint">{mention ? "选择员工（Tab 补全第一个）" : names.length > 6 ? "输入 @ 搜索成员" : "@"}</span>
          {suggestions.map((n) => (
            <button key={n} className="chip" onClick={() => complete(n)}>
              {n}{queued.has(n) ? " ⏸" : running.includes(n) ? " ●" : ""}
            </button>
          ))}
          {cont.length > 0 && <span className="hint">↻ 将继续：{cont.map((v) => `${v.employee_name}《${v.task.title}》`).join("、")}</span>}
          {mention && suggestions.length === 0 && <span className="hint">没有匹配的成员</span>}
          <label className="tog" style={{ marginLeft: "auto" }}><input type="checkbox" checked={verbose} onChange={(e) => setVerbose(e.target.checked)} />显示自动放行</label>
        </div>
        <div className="row">
          <textarea
            value={text}
            placeholder="例如：@alice 给登录接口补上单元测试    （Enter 发送，Shift+Enter 换行）"
            onChange={(e) => setText(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Tab" && mention && suggestions.length > 0) { e.preventDefault(); complete(suggestions[0]); return; }
              if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) { e.preventDefault(); void send(); }
            }}
          />
          <button className="btn primary" disabled={!text.trim()} onClick={() => void send()}>发送</button>
        </div>
      </footer>
    </div>
  );
}
