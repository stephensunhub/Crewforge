import { useCallback, useEffect, useMemo, useState } from "react";
import { api, subscribe } from "./api";
import { COMPANY_TABS, CompanyView, type CompanyTab } from "./CompanyView";
import { PROJECT_TABS, ProjectView, type ProjectTab } from "./ProjectView";
import { SettingsView } from "./SettingsView";
import { ModelsView } from "./ModelsView";
import { UsageView } from "./UsageView";
import { formatApprovalInput, hhmm, short } from "./common";
import type { ApprovalReq, Company, EmployeeCard, EventRow, PendingDecision, ProjectSummary, SchedStatus } from "./types";

const MAX_EVENTS = 2000;
const SCOPE_KEY = "crewforge.scope";

type Scope = { kind: "company" } | { kind: "project"; id: string } | { kind: "cost" } | { kind: "models" } | { kind: "settings" };

function loadScope(): Scope {
  try { const s = JSON.parse(localStorage.getItem(SCOPE_KEY) ?? ""); if (s?.kind) return s; } catch { /* 忽略 */ }
  return { kind: "company" };
}

function SchedChip({ s, onSettings, onAction }: { s: SchedStatus; onSettings: () => void; onAction: (fn: () => Promise<unknown>) => Promise<void> }) {
  if (s.level === "normal" && !s.override_until && s.waiting === 0) return null;
  const when = s.resume_at ? `，预计 ${hhmm(s.resume_at)} 恢复` : "";
  return (
    <div className={`sched-chip ${s.level}`} title={s.reason}>
      <button className="link-plain" onClick={onSettings}>
        {s.level === "paused" ? `⏸ 已暂停${when}` : s.level === "throttled" ? `⚠ 限速 ${s.effective_limit}/${s.user_limit}` : s.override_until ? `已临时忽略额度限制，至 ${hhmm(s.override_until)}` : "额度正常"}
        {s.waiting > 0 && ` · ${s.waiting} 个排队`}
      </button>
      {s.manual_pause && <button className="btn" onClick={() => void onAction(() => api.pauseScheduler(false))}>取消暂停</button>}
      {!s.manual_pause && s.level !== "normal" && <button className="btn" onClick={() => void onAction(() => api.overrideScheduler(30))}>仍然继续 30 分钟</button>}
    </div>
  );
}

export default function App() {
  const [company, setCompany] = useState<Company | null>(null);
  const [cards, setCards] = useState<EmployeeCard[]>([]);
  const [projects, setProjects] = useState<ProjectSummary[]>([]);
  const [events, setEvents] = useState<EventRow[]>([]);
  const [running, setRunning] = useState<string[]>([]);
  const [approvals, setApprovals] = useState<ApprovalReq[]>([]);
  const [sched, setSched] = useState<SchedStatus | null>(null);
  const concurrency = sched?.user_limit ?? 3;
  const [scope, setScope] = useState<Scope>(loadScope);
  const [companyTab, setCompanyTab] = useState<CompanyTab>("employees");
  const [projectTab, setProjectTab] = useState<ProjectTab>("chat");
  const [error, setError] = useState("");
  const [inbox, setInbox] = useState(false);
  const [ready, setReady] = useState(false);
  const [pending, setPending] = useState<PendingDecision[]>([]);
  const [unverified, setUnverified] = useState<[string, string, number][]>([]);
  const [proposals, setProposals] = useState<PendingDecision[]>([]);
  const [actionCounts, setActionCounts] = useState<[string, string, number][]>([]);
  const [planCounts, setPlanCounts] = useState<[string, string, number][]>([]);
  const [meetingFocus, setMeetingFocus] = useState<string | null>(null);

  const employees = useMemo(() => cards.map((c) => c.employee), [cards]);
  const nameById = useMemo(() => new Map(employees.map((e) => [e.id, e.name])), [employees]);

  const reloadStatic = useCallback(async () => {
    try {
      const [c, cs, ps] = await Promise.all([api.company(), api.employeeCards(), api.projects()]);
      setCompany(c); setCards(cs); setProjects(ps);
    } catch (e) { setError(String(e)); }
  }, []);

  useEffect(() => {
    void (async () => {
      await reloadStatic();
      try {
        const [evs, run] = await Promise.all([api.recentEvents(null, 800), api.running()]);
        setEvents(evs); setRunning(run);
        setPending(await api.pendingDecisions());
        setProposals(await api.pendingProposals());
        setActionCounts(await api.actionCounts());
        setPlanCounts(await api.planProposedCounts());
        setUnverified(await api.unverifiedCounts());
        setSched(await api.schedulerStatus());
      } catch (e) { setError(String(e)); }
      setReady(true);
    })();
    const un = subscribe({
      onEvent: (e) => {
        // 事件由后端按 id 推送；启动时的初始加载与推送可能有重叠，按 id 去重
        setEvents((p) => (p.length > 0 && p[p.length - 1].id >= e.id ? p : [...p, e].slice(-MAX_EVENTS)));
        if (["employee_created", "project_created"].includes(e.kind)) void reloadStatic();
        if (e.kind === "scheduler_state" || e.kind === "task_queued" || e.kind === "task_started" || e.kind === "turn_result" || e.kind === "rate_limit") {
          void api.schedulerStatus().then(setSched).catch(() => undefined);
        }
        if (e.kind.startsWith("issue_") || e.kind === "agent_proposed") {
          void api.pendingDecisions().then(setPending).catch(() => undefined);
          void api.pendingProposals().then(setProposals).catch(() => undefined);
        }
        if (e.kind.startsWith("plan_") || e.kind === "agent_proposed") void api.planProposedCounts().then(setPlanCounts).catch(() => undefined);
        if (e.kind.startsWith("action_") || ["meeting_minutes", "task_merged", "task_abandoned", "turn_result"].includes(e.kind)) void api.actionCounts().then(setActionCounts).catch(() => undefined);
        if (e.kind.startsWith("memory_") && e.kind !== "memory_injected") void api.unverifiedCounts().then(setUnverified).catch(() => undefined);
        if (["task_created", "task_merged", "task_abandoned", "task_started", "turn_result", "task_failed"].includes(e.kind)) void api.projects().then(setProjects).catch(() => undefined);
      },
      onApproval: (a) => setApprovals((p) => [...p, a]),
      onRunning: setRunning,
    });
    return () => { void un.then((fns) => fns.forEach((f) => f())); };
  }, [reloadStatic]);

  useEffect(() => { try { localStorage.setItem(SCOPE_KEY, JSON.stringify(scope)); } catch { /* 忽略 */ } }, [scope]);

  // 当前项目被归档或不存在时，回到公司页
  const currentProject = scope.kind === "project" ? projects.find((p) => p.project.id === scope.id) : undefined;
  useEffect(() => {
    if (ready && scope.kind === "project" && !currentProject) setScope({ kind: "company" });
  }, [ready, scope, currentProject]);

  const quota = useMemo(() => {
    for (let i = events.length - 1; i >= 0; i--)
      if (events[i].kind === "rate_limit") return events[i].payload as { status: string; five_hour: number | null; seven_day: number | null };
    return null;
  }, [events]);

  async function decide(a: ApprovalReq, allow: boolean) {
    setApprovals((p) => p.filter((x) => x.id !== a.id));
    try { await api.respondApproval(a.id, allow); } catch (e) { setError(String(e)); }
  }
  async function changeConcurrency(n: number) {
    try { await api.setConcurrency(n); setSched(await api.schedulerStatus()); } catch (e) { setError(String(e)); }
  }
  // 待办来源很多（事件之外，命令行或别的进程也会改库）：定时刷新并在窗口重新获得焦点时刷新，保证角标不过期
  const refreshInbox = useCallback(() => {
    void api.pendingDecisions().then(setPending).catch(() => undefined);
    void api.pendingProposals().then(setProposals).catch(() => undefined);
    void api.actionCounts().then(setActionCounts).catch(() => undefined);
    void api.planProposedCounts().then(setPlanCounts).catch(() => undefined);
    void api.unverifiedCounts().then(setUnverified).catch(() => undefined);
    void api.projects().then(setProjects).catch(() => undefined);
  }, []);
  useEffect(() => {
    const t = window.setInterval(refreshInbox, 20000);
    window.addEventListener("focus", refreshInbox);
    return () => { window.clearInterval(t); window.removeEventListener("focus", refreshInbox); };
  }, [refreshInbox]);
  // 额度窗口到期会自动恢复，没有事件可等；定时刷新让倒计时和状态保持准确
  useEffect(() => {
    const t = window.setInterval(() => void api.schedulerStatus().then(setSched).catch(() => undefined), 15000);
    return () => window.clearInterval(t);
  }, []);
  async function schedAction(fn: () => Promise<unknown>) {
    try { await fn(); setSched(await api.schedulerStatus()); } catch (e) { setError(String(e)); }
  }
  const openProject = (id: string, tab?: ProjectTab) => {
    const importing = projects.find((p) => p.project.id === id)?.project.onboarding === "in_progress";
    setProjectTab(tab ?? (importing ? "onboarding" : "chat"));
    setScope({ kind: "project", id });
  };
  const projectName = (id: string | null) => projects.find((p) => p.project.id === id)?.project.name ?? "";

  const tabs = scope.kind === "company" ? COMPANY_TABS : scope.kind === "project" ? PROJECT_TABS.filter(([k]) => k !== "onboarding" || (currentProject?.project.onboarding ?? "none") !== "none") : null;
  const activeTab = scope.kind === "company" ? companyTab : projectTab;
  // 角标是待处理事项的总数（不是“有待办的项目数”）：行动项、待验证经验、待确认的计划条目都按条数算
  const sum = (a: [string, string, number][]) => a.reduce((n, x) => n + x[2], 0);
  const inboxTotal = approvals.length + pending.length + proposals.length + sum(unverified) + sum(actionCounts) + sum(planCounts);
  const setTab = (t: string) => (scope.kind === "company" ? setCompanyTab(t as CompanyTab) : setProjectTab(t as ProjectTab));

  return (
    <div className="app">
      <header className="topbar">
        <div className="brand"><img src="/logo.png" alt="" /><b>Crewforge</b></div>
        <nav className="scopes">
          <button className={`scope ${scope.kind === "company" ? "on" : ""}`} onClick={() => setScope({ kind: "company" })}>{company?.name ?? "公司"}</button>
          {projects.map((p) => (
            <button key={p.project.id} className={`scope ${scope.kind === "project" && scope.id === p.project.id ? "on" : ""}`} onClick={() => openProject(p.project.id)}>
              {p.project.name}{p.active_tasks > 0 && <i className="pip" title={`${p.active_tasks} 个进行中任务`} />}
            </button>
          ))}
        </nav>
        <div className="top-right">
          {sched && <SchedChip s={sched} onSettings={() => setScope({ kind: "settings" })} onAction={schedAction} />}
          {quota && (
            <div className="quota" title={`额度状态：${quota.status}`}>
              {([["5h", quota.five_hour], ["7d", quota.seven_day]] as const).map(([l, v]) => (
                <div key={l}>{l} {v != null ? `${Math.round(v * 100)}%` : "?"}
                  <div className="meter"><i className={(v ?? 0) > 0.85 ? "hot" : ""} style={{ width: `${Math.min(100, (v ?? 0) * 100)}%` }} /></div></div>
              ))}
            </div>
          )}
          <button className={`icon-btn ${inbox ? "on" : ""}`} onClick={() => setInbox((v) => !v)} title="待你处理">
            🔔{inboxTotal > 0 && <span className="count">{inboxTotal}</span>}
          </button>
          <button className={`icon-btn text ${scope.kind === "models" ? "on" : ""}`} onClick={() => setScope({ kind: "models" })}>模型</button>
          <button className={`icon-btn text ${scope.kind === "cost" ? "on" : ""}`} onClick={() => setScope({ kind: "cost" })}>用量</button>
          <button className={`icon-btn ${scope.kind === "settings" ? "on" : ""}`} onClick={() => setScope({ kind: "settings" })} title="设置">⚙</button>
        </div>
        {inbox && (
          <div className="inbox">
            <b>待你处理</b>
            {inboxTotal === 0 && <div className="hint" style={{ padding: "10px 0" }}>现在没有需要你处理的事项。</div>}
            {planCounts.map(([pid, pname, n]) => (
              <div className="inbox-item" key={`plan-${pid}`}>
                <div><span className="badge">确认计划</span> AI 起草了 <b>{n}</b> 个计划条目<span className="hint"> · {pname}</span></div>
                <div className="hint">为什么是你：计划要你批准后才会派活，批准前员工不会开始做。</div>
                <div className="btns"><button className="btn primary" onClick={() => { openProject(pid, "plan"); setInbox(false); }}>去确认</button></div>
              </div>
            ))}
            {actionCounts.map(([pid, pname, n]) => (
              <div className="inbox-item" key={`act-${pid}`}>
                <div><span className="badge">需要你确认</span> <b>{n}</b> 件事拦着合并或要你来决定<span className="hint"> · {pname}</span></div>
                <div className="hint">为什么是你：拦合并的评审意见，员工改完后要你确认已解决；或者评审里指明要由人决定的事。“之后再说”的小事不在这里。</div>
                <div className="btns"><button className="btn primary" onClick={() => { openProject(pid, "duties"); setInbox(false); }}>看详情</button></div>
              </div>
            ))}
            {proposals.map((p) => (
              <div className="inbox-item" key={p.issue.id}>
                <div><span className="badge">确认议题</span> 员工提议了议题：<b>{p.issue.title}</b><span className="hint"> · {p.project_name}</span></div>
                {p.issue.summary && <div className="hint">{short(p.issue.summary, 120)}</div>}
                <div className="hint">为什么是你：AI 只能提议；是否值得讨论由你确认，确认前不占用大家的额度。你也可以驳回。</div>
                <div className="btns"><button className="btn primary" onClick={() => { openProject(p.issue.project_id, "issues"); setInbox(false); }}>去处理</button></div>
              </div>
            ))}
            {unverified.map(([pid, pname, n]) => (
              <div className="inbox-item" key={pid}>
                <div><span className="badge">验证经验</span> <b>{n}</b> 条经验等待验证<span className="hint"> · {pname}</span></div>
                <div className="hint">为什么是你：未验证的经验不会给员工用，你确认它真的有用才会生效。</div>
                <div className="btns"><button className="btn primary" onClick={() => { openProject(pid, "memory"); setInbox(false); }}>去验证</button></div>
              </div>
            ))}
            {pending.map((p) => (
              <div className="inbox-item" key={p.issue.id}>
                <div><span className="badge">拍板</span> 议题待你裁决：<b>{p.issue.title}</b><span className="hint"> · {p.project_name}</span></div>
                {p.issue.summary && <div className="hint">{short(p.issue.summary, 120)}</div>}
                <div className="hint">为什么是你：{p.issue.high_risk || p.issue.kind === "requirement" ? "需求类或高风险议题，只有你能拍板，AI 没有这个权限。" : "讨论已结束，等有权限者拍板；你始终可以拍板。"}</div>
                <div className="btns"><button className="btn primary" onClick={() => { openProject(p.issue.project_id, "issues"); setInbox(false); }}>去处理</button></div>
              </div>
            ))}
            {approvals.map((a) => (
              <div className="inbox-item" key={a.id}>
                <div><span className="badge">越权审批</span> <b>{a.employee}</b> 请求 <b>{a.tool}</b>{a.project_id && <span className="hint"> · {projectName(a.project_id)}</span>}</div>
                <div className="hint">为什么是你：工作区内的读写自动放行，这次操作超出了范围（{a.reason}）。</div>
                <code className="hint" style={{ whiteSpace: "pre-wrap" }}>{short(formatApprovalInput(a.tool, a.input), 240)}</code>
                <div className="btns">
                  <button className="btn primary" onClick={() => void decide(a, true)}>允许</button>
                  <button className="btn danger" onClick={() => void decide(a, false)}>拒绝</button>
                  {a.project_id && <button className="btn" onClick={() => { openProject(a.project_id!); setInbox(false); }}>去项目</button>}
                </div>
              </div>
            ))}
          </div>
        )}
      </header>

      {tabs && (
        <div className="tabbar">
          {tabs.map(([k, label]) => (
            <button key={k} className={`tab ${activeTab === k ? "on" : ""}`} onClick={() => setTab(k)}>{label}</button>
          ))}
          {scope.kind === "project" && currentProject && <span className="hint tabbar-path" title={currentProject.project.path}>{currentProject.project.path}</span>}
        </div>
      )}

      {error && <div className="banner"><span>{error}</span><button onClick={() => setError("")}>✕</button></div>}

      <main className="content">
        {scope.kind === "company" && (
          <CompanyView tab={companyTab} cards={cards} projects={projects} company={company} running={running} events={events}
            onOpenProject={openProject} onChanged={() => void reloadStatic()} onError={setError} />
        )}
        {scope.kind === "project" && currentProject && (
          <ProjectView key={currentProject.project.id} tab={projectTab} summary={currentProject} employees={employees} events={events}
            nameById={nameById} running={running} approvals={approvals} concurrency={concurrency}
            meetingFocus={meetingFocus} onFocusHandled={() => setMeetingFocus(null)} onOpenMeeting={(id) => { setProjectTab("meetings"); setMeetingFocus(id); }} onGoOnboarding={() => setProjectTab("onboarding")} onGoTab={(t) => setProjectTab(t)}
            onDecide={(a, allow) => void decide(a, allow)} onChanged={() => void reloadStatic()} onError={setError}
            onLeave={() => setScope({ kind: "company" })} />
        )}
        {scope.kind === "cost" && <UsageView events={events} onError={setError} />}
        {scope.kind === "models" && <ModelsView onError={setError} />}
        {scope.kind === "settings" && <SettingsView sched={sched} onConcurrency={(n) => void changeConcurrency(n)} onAction={schedAction} />}
      </main>
    </div>
  );
}
