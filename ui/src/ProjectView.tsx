import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api } from "./api";
import { Avatar, Field, Modal, Placeholder, ROLE_LABEL, ROLE_ORDER } from "./common";
import { Chat } from "./Chat";
import { TasksPanel } from "./TasksPanel";
import { IssuesView } from "./IssuesView";
import { MeetingsView } from "./MeetingsView";
import type { ApprovalReq, Employee, EventRow, IssueView, Meeting, Member, ProjectRole, ProjectSummary, RepoStatus, TaskView } from "./types";

export type ProjectTab = "chat" | "tasks" | "issues" | "meetings" | "members" | "memory" | "docs";
export const PROJECT_TABS: [ProjectTab, string][] = [
  ["chat", "群聊"], ["tasks", "任务"], ["issues", "议题"], ["meetings", "会议"], ["members", "成员"], ["memory", "记忆"], ["docs", "文档"],
];
const TASK_EVENTS = ["task_created", "checkpoint_committed", "checkpoint_failed", "task_merged", "task_merge_blocked", "task_abandoned", "turn_result", "task_failed", "task_error"];

export function ProjectView({
  tab, summary, employees, events, nameById, running, approvals, concurrency, meetingFocus, onFocusHandled, onOpenMeeting, onDecide, onChanged, onError, onLeave,
}: {
  tab: ProjectTab;
  summary: ProjectSummary;
  employees: Employee[];
  events: EventRow[];
  nameById: Map<string, string>;
  running: string[];
  approvals: ApprovalReq[];
  concurrency: number;
  meetingFocus: string | null;
  onFocusHandled: () => void;
  onOpenMeeting: (id: string) => void;
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

  if (tab === "chat")
    return (
      <Chat project={project} members={members} events={events} nameById={nameById} running={running} approvals={mine}
        concurrency={concurrency} repo={repo} taskViews={taskViews} onDecide={onDecide} onError={onError} onInitGit={initGit} />
    );
  if (tab === "tasks")
    return (
      <div className="tasks-page">
        <TasksPanel views={taskViews} busy={running} onChanged={() => { void refreshRepo(); onChanged(); }} onError={onError} />
      </div>
    );
  if (tab === "members")
    return (
      <MembersTab project={summary} members={members} employees={employees} running={running}
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
  if (tab === "memory") return <Placeholder title="项目记忆">架构、接口约定、业务规则、已知问题与历史决策。员工被唤醒时只检索与当前任务相关的少量条目。</Placeholder>;
  return <Placeholder title="项目文档">需求、架构说明、决策记录、测试基线。计划存放在项目仓库的 docs/ 目录，由员工通过同一套分支与合并流程维护。</Placeholder>;
}

function MembersTab({ project, members, employees, running, onChanged, onError, onArchived }: {
  project: ProjectSummary;
  members: Member[];
  employees: Employee[];
  running: string[];
  onChanged: () => void;
  onError: (m: string) => void;
  onArchived: () => void;
}) {
  const [adding, setAdding] = useState(false);
  const [editing, setEditing] = useState(false);
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
        <button className="btn" onClick={() => setEditing(true)}>编辑</button></div>
      <div className="kv">
        <span>名称</span><span>{p.name}</span>
        <span>目录</span><code>{p.path}</code>
        <span>描述</span><span>{p.description || "—"}</span>
      </div>
      <div style={{ marginTop: 16 }}>
        <button className="btn danger" onClick={async () => {
          if (await api.confirm(`归档项目「${p.name}」？\n数据和代码都会保留，只是不再出现在列表里。`, "确认归档")) {
            try { await api.archiveProject(p.id); onChanged(); onArchived(); } catch (e) { onError(String(e)); }
          }
        }}>归档项目</button>
      </div>

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
