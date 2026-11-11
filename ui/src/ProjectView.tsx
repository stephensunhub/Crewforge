import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api } from "./api";
import { ActivityLine, Avatar, Field, Modal, ROLE_LABEL, ROLE_ORDER } from "./common";
import { Chat } from "./Chat";
import { TasksPanel } from "./TasksPanel";
import { IssuesView } from "./IssuesView";
import { MeetingsView } from "./MeetingsView";
import { MemoryView } from "./MemoryView";
import { OnboardingView } from "./OnboardingView";
import { DocsView } from "./DocsView";
import { DutiesPanel } from "./DutiesPanel";
import { PlanPanel } from "./PlanPanel";
import { ActionsView } from "./ActionsView";
import { EndProjectModal } from "./EndProject";
import type { Activity, ApprovalReq, Employee, EventRow, IssueView, Meeting, Member, ProjectRole, ProjectSummary, RepoStatus, TaskView } from "./types";

export type ProjectTab = "duties" | "onboarding" | "chat" | "plan" | "tasks" | "issues" | "meetings" | "actions" | "members" | "memory" | "docs";
export const PROJECT_TABS: [ProjectTab, string][] = [
  ["duties", "职责"], ["onboarding", "导入"], ["chat", "群聊"], ["plan", "计划"], ["tasks", "任务"], ["issues", "议题"], ["meetings", "会议"], ["actions", "行动项"], ["members", "成员"], ["memory", "记忆"], ["docs", "文档"],
];
const TASK_EVENTS = ["task_created", "checkpoint_committed", "checkpoint_failed", "task_merged", "task_merge_blocked", "task_abandoned", "turn_result", "task_failed", "task_error"];

export function ProjectView({
  tab, summary, employees, events, nameById, running, activities, approvals, concurrency, meetingFocus, onFocusHandled, onOpenMeeting, onGoOnboarding, onGoTab, onDecide, onChanged, onError, onLeave,
}: {
  tab: ProjectTab;
  summary: ProjectSummary;
  employees: Employee[];
  events: EventRow[];
  nameById: Map<string, string>;
  running: string[];
  activities: Activity[];
  approvals: ApprovalReq[];
  concurrency: number;
  meetingFocus: string | null;
  onFocusHandled: () => void;
  onOpenMeeting: (id: string) => void;
  onGoOnboarding: () => void;
  onGoTab: (t: ProjectTab) => void;
  onDecide: (a: ApprovalReq, allow: boolean) => void;
  onChanged: () => void;
  onError: (m: string) => void;
  onLeave: () => void;
}) {
  const project = summary.project;
  const [members, setMembers] = useState<Member[]>([]);
  const [repo, setRepo] = useState<RepoStatus | null>(null);
  const [taskViews, setTaskViews] = useState<TaskView[]>([]);
  const [issues, setIssues] = useState<IssueView[]>([]);
  const [meetings, setMeetings] = useState<Meeting[]>([]);

  const refreshMembers = useCallback(async () => {
    try { setMembers(await api.members(project.id)); } catch (e) { onError(String(e)); }
  }, [project.id, onError]);
  const refreshRepo = useCallback(async () => {
    try {
      const [r, t] = await Promise.all([api.repoStatus(project.path), api.taskViews(project.path)]);
      setRepo(r); setTaskViews(t);
    } catch (e) { onError(String(e)); }
  }, [project.path, onError]);

  const refreshBoard = useCallback(async () => {
    try {
      const [i, m] = await Promise.all([api.issues(project.id), api.meetings(project.id)]);
      setIssues(i); setMeetings(m);
    } catch (e) { onError(String(e)); }
  }, [project.id, onError]);
  const lastBoardEvent = useMemo(
    () => events.reduce((m, e) => (e.project_id === project.id && /^(issue_|meeting_)/.test(e.kind) ? Math.max(m, e.id) : m), 0),
    [events, project.id],
  );
  useEffect(() => { void refreshBoard(); }, [refreshBoard, lastBoardEvent]);

  useEffect(() => { setMembers([]); setRepo(null); setTaskViews([]); void refreshMembers(); void refreshRepo(); }, [refreshMembers, refreshRepo]);

  // 本项目的任务相关事件到来时，合并刷新
  const lastTaskEvent = useMemo(
    () => events.reduce((m, e) => (e.project_id === project.id && TASK_EVENTS.includes(e.kind) ? Math.max(m, e.id) : m), 0),
    [events, project.id],
  );
  const timer = useRef<number | undefined>(undefined);
  useEffect(() => {
    if (!lastTaskEvent) return;
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => void refreshRepo(), 400);
    return () => window.clearTimeout(timer.current);
  }, [lastTaskEvent, refreshRepo]);

  const mine = approvals.filter((a) => a.project_id === project.id);

  async function initGit() {
    try { await api.initRepo(project.path); await refreshRepo(); } catch (e) { onError(String(e)); }
  }

  if (tab === "onboarding")
    return <OnboardingView projectId={project.id} members={members} events={events} running={running} onError={onError} onChanged={onChanged} />;
  if (tab === "duties")
    return <DutiesPanel projectId={project.id} projectName={project.name} events={events} onGoTab={onGoTab} onOpenMeeting={onOpenMeeting} onError={onError} />;
  if (tab === "plan")
    return <PlanPanel project={project} members={members} events={events} onError={onError} onChanged={onChanged} onOpenMeeting={onOpenMeeting} />;
  if (tab === "chat")
    return (
      <Chat project={project} members={members} events={events} nameById={nameById} running={running} approvals={mine}
        concurrency={concurrency} repo={repo} taskViews={taskViews} onboarding={project.onboarding} onGoOnboarding={onGoOnboarding} onGoTab={onGoTab} onOpenMeeting={onOpenMeeting} onDecide={onDecide} onError={onError} onInitGit={initGit} />
    );
  if (tab === "tasks")
    return (
      <div className="tasks-page">
        <TasksPanel views={taskViews} busy={running} hasBaseline={project.onboarding !== "none"} onChanged={() => { void refreshRepo(); onChanged(); }} onError={onError} />
      </div>
    );
  if (tab === "members")
    return (
      <MembersTab project={summary} members={members} employees={employees} running={running} activities={activities} onOpenWork={(a) => { onGoTab(a.tab as ProjectTab); if (a.meeting_id) onOpenMeeting(a.meeting_id); }}
        onChanged={() => { void refreshMembers(); onChanged(); }} onError={onError} onArchived={onLeave} />
    );
  if (tab === "issues")
    return (
      <IssuesView projectId={project.id} views={issues} members={members} onChanged={() => void refreshBoard()} onError={onError}
        onDiscuss={async (issueId) => {
          const i = issues.find((x) => x.issue.id === issueId);
          try {
            const m = await api.createMeeting({ projectId: project.id, kind: "design", title: `讨论：${i?.issue.title ?? ""}`, hostId: null, taskId: null, issueIds: [issueId], maxAgentReplies: 12 });
            await refreshBoard(); onOpenMeeting(m.id);
          } catch (e) { onError(String(e)); }
        }} />
    );
  if (tab === "meetings")
    return (
      <MeetingsView projectId={project.id} meetings={meetings} members={members} issues={issues} tasks={taskViews} events={events} running={running}
        focusId={meetingFocus} onFocused={onFocusHandled} onChanged={() => void refreshBoard()} onError={onError} />
    );
  if (tab === "actions")
    return <ActionsView projectId={project.id} members={members} tasks={taskViews} events={events} running={running} onOpenMeeting={onOpenMeeting} onError={onError} onChanged={onChanged} />;
  if (tab === "memory") return <MemoryView scope={{ layer: "project", projectId: project.id }} events={events} members={members} onError={onError} canCurate={members.some((m) => m.role === "lead")} />;
  return <DocsView projectId={project.id} hasLead={members.some((m) => m.role === "lead")} onError={onError} />;
}

function MembersTab({ project, members, employees, running, activities, onOpenWork, onChanged, onError, onArchived }: {
  project: ProjectSummary;
  members: Member[];
  employees: Employee[];
  running: string[];
  activities: Activity[];
  onOpenWork: (a: Activity) => void;
  onChanged: () => void;
  onError: (m: string) => void;
  onArchived: () => void;
}) {
  const [adding, setAdding] = useState(false);
  const [editing, setEditing] = useState(false);
  const [ending, setEnding] = useState(false);
  const p = project.project;
  const candidates = employees.filter((e) => !members.some((m) => m.employee_id === e.id));

  async function guard(fn: () => Promise<unknown>) {
    try { await fn(); onChanged(); } catch (e) { onError(String(e)); }
  }

  return (
    <div className="page">
      <div className="page-head">
        <div><h2>项目成员</h2><div className="hint">负责人只能有一位；没有 AI 负责人时，由你（人类）担任。</div></div>
        <button className="btn primary" disabled={candidates.length === 0} onClick={() => setAdding(true)}>
          ＋ 添加成员
        </button>
      </div>
      {members.length === 0 && <div className="hint" style={{ padding: "8px 0" }}>还没有成员。{employees.length === 0 ? "请先到「公司 → 员工」创建员工。" : "点右上角添加。"}</div>}
      <div className="cards">
        {members.map((m) => (
          <div className="card" key={m.employee_id}>
            <div className="card-top">
              <Avatar name={m.name} size={38} />
              <div style={{ minWidth: 0 }}><b>{m.name}</b><div className="hint">{m.title}</div></div>
              <span className={`status ${running.includes(m.name) ? "run" : ""}`}>{running.includes(m.name) ? "● 工作中" : "○ 空闲"}</span>
            </div>
            {running.includes(m.name) && <ActivityLine a={activities.find((x) => x.employee === m.name)} onOpen={onOpenWork} />}
            <div className="card-foot">
              <select value={m.role} onChange={(e) => void guard(() => api.setMemberRole(p.id, m.employee_id, e.target.value as ProjectRole))}>
                {ROLE_ORDER.map((r) => <option key={r} value={r}>{ROLE_LABEL[r]}</option>)}
              </select>
              <button className="btn danger" onClick={async () => {
                if (await api.confirm(`把 ${m.name} 移出项目？\n不会删除 TA 的任何历史。`, "确认移出")) await guard(() => api.removeMember(p.id, m.employee_id));
              }}>移出</button>
            </div>
          </div>
        ))}
      </div>

      <div className="page-head" style={{ marginTop: 28 }}><div><h2>项目信息</h2></div>
        <div className="row" style={{ gap: 8 }}>
          <button className="btn" onClick={() => setEditing(true)}>编辑</button>
          <button className="btn danger" onClick={() => setEnding(true)}>结束项目…</button>
        </div></div>
      <div className="kv">
        <span>名称</span><span>{p.name}</span>
        <span>目录</span><code>{p.path}</code>
        <span>描述</span><span>{p.description || "—"}</span>
      </div>
      {ending && <EndProjectModal summary={project} onClose={() => setEnding(false)} onError={onError} onDone={() => { setEnding(false); onChanged(); onArchived(); }} />}

      {adding && <AddMember candidates={candidates} hasLead={members.some((m) => m.role === "lead")} onClose={() => setAdding(false)}
        onAdd={async (eid, role) => { await guard(() => api.addMember(p.id, eid, role)); setAdding(false); }} />}
      {editing && <EditProject name={p.name} desc={p.description} onClose={() => setEditing(false)}
        onSave={async (n, d) => { await guard(() => api.updateProject(p.id, n, d)); setEditing(false); }} />}
    </div>
  );
}

function AddMember({ candidates, hasLead, onAdd, onClose }: {
  candidates: Employee[]; hasLead: boolean; onAdd: (id: string, role: ProjectRole) => Promise<void>; onClose: () => void;
}) {
  const [id, setId] = useState(candidates[0]?.id ?? "");
  const [role, setRole] = useState<ProjectRole>("developer");
  return (
    <Modal title="添加项目成员" onClose={onClose} width={420}>
      <div className="form">
        <Field label="员工"><select value={id} onChange={(e) => setId(e.target.value)}>
          {candidates.map((e) => <option key={e.id} value={e.id}>{e.name}（{e.role}）</option>)}
        </select></Field>
        <Field label="在本项目中的角色" hint={role === "lead" && hasLead ? "已有负责人，原负责人会降为开发。" : undefined}>
          <select value={role} onChange={(e) => setRole(e.target.value as ProjectRole)}>
            {ROLE_ORDER.map((r) => <option key={r} value={r}>{ROLE_LABEL[r]}</option>)}
          </select>
        </Field>
        <div className="form-btns"><button className="btn primary" disabled={!id} onClick={() => void onAdd(id, role)}>添加</button></div>
      </div>
    </Modal>
  );
}

function EditProject({ name, desc, onSave, onClose }: { name: string; desc: string; onSave: (n: string, d: string) => Promise<void>; onClose: () => void }) {
  const [n, setN] = useState(name);
  const [d, setD] = useState(desc);
  return (
    <Modal title="编辑项目" onClose={onClose} width={460}>
      <div className="form">
        <Field label="名称"><input value={n} onChange={(e) => setN(e.target.value)} /></Field>
        <Field label="描述"><textarea rows={4} value={d} onChange={(e) => setD(e.target.value)} /></Field>
        <div className="form-btns"><button className="btn primary" disabled={!n.trim()} onClick={() => void onSave(n, d)}>保存</button></div>
      </div>
    </Modal>
  );
}
