import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { ask, open } from "@tauri-apps/plugin-dialog";
import type {
  ApprovalReq, Company, Employee, EmployeeCard, EventRow, Member, MergeOutcome, Project,
  ProjectRole, ProjectSummary, RepoStatus, TaskView, IssueView, PendingDecision, Memory, OnboardingState, CostReport, ScanReport, BaselineCommand, CheckReport, StoredCheck, ActionItem, SchedStatus, SchedulerConfig, Meeting, MeetingDetail, MeetingKind,
} from "./types";

export const api = {
  // 公司
  company: () => invoke<Company>("get_company"),
  updateCompany: (name: string, description: string, conventions: string) =>
    invoke<void>("update_company", { name, description, conventions }),
  // 员工（公司级）
  employees: () => invoke<Employee[]>("list_employees"),
  employeeCards: () => invoke<EmployeeCard[]>("employee_cards"),
  addEmployee: (name: string, role: string, persona: string) => invoke<Employee>("add_employee", { name, role, persona }),
  updateEmployee: (id: string, role: string, persona: string) => invoke<void>("update_employee", { id, role, persona }),
  archiveEmployee: (id: string) => invoke<void>("archive_employee", { id }),
  // 项目
  projects: () => invoke<ProjectSummary[]>("list_projects"),
  createProject: (name: string, description: string, path: string, initGit: boolean, importExisting: boolean) =>
    invoke<Project>("create_project", { name, description, path, initGit, importExisting }),
  updateProject: (id: string, name: string, description: string) => invoke<void>("update_project", { id, name, description }),
  archiveProject: (id: string) => invoke<void>("archive_project", { id }),
  // 项目成员
  members: (projectId: string) => invoke<Member[]>("list_members", { projectId }),
  addMember: (projectId: string, employeeId: string, role: ProjectRole) => invoke<void>("add_member", { projectId, employeeId, role }),
  setMemberRole: (projectId: string, employeeId: string, role: ProjectRole) => invoke<void>("set_member_role", { projectId, employeeId, role }),
  removeMember: (projectId: string, employeeId: string) => invoke<void>("remove_member", { projectId, employeeId }),
  // 成本与额度
  costReport: (days: number | null) => invoke<CostReport>("cost_report", { days, tzOffsetMinutes: -new Date().getTimezoneOffset() }),
  // 旧项目导入
  onboardingState: (projectId: string) => invoke<OnboardingState>("onboarding_state", { projectId }),
  runScan: (projectId: string) => invoke<ScanReport>("run_scan", { projectId }),
  runBaseline: (projectId: string, commands: BaselineCommand[]) => invoke<void>("run_baseline", { projectId, commands }),
  evaluateProject: (projectId: string, kind: "architect" | "qa", employeeId: string) => invoke<void>("evaluate_project", { projectId, kind, employeeId }),
  confirmOnboarding: (projectId: string, skipBaseline: boolean) => invoke<void>("confirm_onboarding", { projectId, skipBaseline }),
  checkTask: (taskId: string) => invoke<CheckReport>("check_task", { taskId }),
  latestTaskCheck: (taskId: string) => invoke<StoredCheck | null>("latest_task_check", { taskId }),
  // 议题
  issues: (projectId: string) => invoke<IssueView[]>("list_issues", { projectId }),
  pendingDecisions: () => invoke<PendingDecision[]>("pending_decisions"),
  pendingProposals: () => invoke<PendingDecision[]>("pending_proposals"),
  createIssue: (a: { projectId: string; title: string; summary: string; kind: string; highRisk: boolean; ownerId: string | null; supersedes: string | null }) =>
    invoke<void>("create_issue", a),
  addOption: (issueId: string, title: string, description: string) => invoke<void>("add_issue_option", { issueId, title, description }),
  setIssueStatus: (id: string, status: string) => invoke<void>("set_issue_status", { id, status }),
  decideIssue: (id: string, chosenOption: string | null, decision: string, reason: string, rejectedReasons: Record<string, string>) =>
    invoke<void>("decide_issue", { id, chosenOption, decision, reason, rejectedReasons }),
  // 会议
  meetings: (projectId: string) => invoke<Meeting[]>("list_meetings", { projectId }),
  meetingDetail: (id: string) => invoke<MeetingDetail>("meeting_detail", { id }),
  createMeeting: (a: { projectId: string; kind: MeetingKind; title: string; hostId: string | null; taskId: string | null; issueIds: string[]; maxAgentReplies: number }) =>
    invoke<Meeting>("create_meeting", a),
  linkMeetingIssue: (meetingId: string, issueId: string) => invoke<void>("link_meeting_issue", { meetingId, issueId }),
  postMeetingMessage: (meetingId: string, content: string) => invoke<string[]>("post_meeting_message", { meetingId, content }),
  draftMinutes: (meetingId: string) => invoke<void>("draft_minutes", { meetingId }),
  saveMinutes: (id: string, minutes: string) => invoke<void>("save_minutes", { id, minutes }),
  closeMeeting: (id: string) => invoke<void>("close_meeting", { id }),
  // 记忆
  memories: (f: { layer?: string; projectId?: string; employeeId?: string; status?: string; query?: string }) =>
    invoke<Memory[]>("list_memories", { layer: f.layer ?? null, projectId: f.projectId ?? null, employeeId: f.employeeId ?? null, status: f.status ?? null, query: f.query ?? null }),
  addMemory: (a: { layer: string; projectId: string | null; employeeId: string | null; kind: string; title: string; content: string; scope: string }) =>
    invoke<Memory>("add_memory", a),
  updateMemory: (id: string, title: string, content: string, scope: string) => invoke<void>("update_memory", { id, title, content, scope }),
  verifyMemory: (id: string) => invoke<void>("verify_memory", { id }),
  expireMemory: (id: string, reason: string) => invoke<void>("expire_memory", { id, reason }),
  deleteMemory: (id: string) => invoke<void>("delete_memory", { id }),
  promoteMemory: (id: string, employeeId: string) => invoke<void>("promote_memory", { id, employeeId }),
  saveLessons: (meetingId: string) => invoke<number>("save_meeting_lessons", { meetingId }),
  unverifiedCounts: () => invoke<[string, string, number][]>("unverified_counts"),
  previewRetrieval: (projectId: string | null, employeeId: string, text: string) =>
    invoke<Memory[]>("preview_retrieval", { projectId, employeeId, text }),
  // 评审行动项
  actionItems: (meetingId: string) => invoke<ActionItem[]>("list_action_items", { meetingId }),
  setActionStatus: (id: string, status: "done" | "dismissed" | "open", note = "") => invoke<void>("set_action_status", { id, status, note }),
  dispatchAction: (id: string, employeeId: string | null) => invoke<void>("dispatch_action_item", { id, employeeId }),
  actionCounts: () => invoke<[string, string, number][]>("action_counts"),
  // 事件与执行
  recentEvents: (projectId: string | null, limit: number) => invoke<EventRow[]>("recent_events", { projectId, limit }),
  running: () => invoke<string[]>("running_employees"),
  runTask: (employee: string, prompt: string, projectId: string) => invoke<void>("run_task", { employee, prompt, projectId }),
  respondApproval: (id: string, allow: boolean) => invoke<boolean>("respond_approval", { id, allow }),
  // 工作区与任务
  initRepo: (projectPath: string) => invoke<void>("init_repo", { projectPath }),
  repoStatus: (projectPath: string) => invoke<RepoStatus>("repo_status", { projectPath }),
  taskViews: (projectPath: string) => invoke<TaskView[]>("list_task_views", { projectPath }),
  taskDiff: (taskId: string) => invoke<string>("task_diff", { taskId }),
  mergeTask: (taskId: string, acknowledgeOpenItems = false) => invoke<MergeOutcome>("merge_task", { taskId, acknowledgeOpenItems }),
  abandonTask: (taskId: string) => invoke<void>("abandon_task", { taskId }),
  reveal: (taskId: string) => invoke<void>("reveal_in_finder", { taskId }),
  // 设置
  dataDir: () => invoke<string>("data_dir"),
  claudeVersion: () => invoke<string>("claude_version"),
  getConcurrency: () => invoke<number>("get_concurrency"),
  schedulerStatus: () => invoke<SchedStatus>("scheduler_status"),
  setSchedulerConfig: (config: SchedulerConfig) => invoke<void>("scheduler_set_config", { config }),
  pauseScheduler: (paused: boolean) => invoke<void>("scheduler_pause", { paused }),
  overrideScheduler: (minutes: number) => invoke<void>("scheduler_override", { minutes }),
  setConcurrency: (n: number) => invoke<number>("set_concurrency", { n }),
  // 对话框
  confirm: (message: string, title: string) => ask(message, { title, kind: "warning", okLabel: "确认", cancelLabel: "取消" }),
  pickDirectory: async (): Promise<string | null> => {
    const r = await open({ directory: true, multiple: false });
    return typeof r === "string" ? r : null;
  },
};

export function subscribe(handlers: {
  onEvent: (e: EventRow) => void;
  onApproval: (a: ApprovalReq) => void;
  onRunning: (names: string[]) => void;
}): Promise<UnlistenFn[]> {
  return Promise.all([
    listen<EventRow>("crewforge://event", (e) => handlers.onEvent(e.payload)),
    listen<ApprovalReq>("crewforge://approval", (e) => handlers.onApproval(e.payload)),
    listen<string[]>("crewforge://running", (e) => handlers.onRunning(e.payload)),
  ]);
}
