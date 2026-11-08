import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { ask, open } from "@tauri-apps/plugin-dialog";
import type {
  ApprovalReq, Company, Employee, EmployeeCard, EventRow, Member, MergeOutcome, Project,
  ProjectRole, ProjectSummary, RepoStatus, TaskView,
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
  createProject: (name: string, description: string, path: string, initGit: boolean) =>
    invoke<Project>("create_project", { name, description, path, initGit }),
  updateProject: (id: string, name: string, description: string) => invoke<void>("update_project", { id, name, description }),
  archiveProject: (id: string) => invoke<void>("archive_project", { id }),
  // 项目成员
  members: (projectId: string) => invoke<Member[]>("list_members", { projectId }),
  addMember: (projectId: string, employeeId: string, role: ProjectRole) => invoke<void>("add_member", { projectId, employeeId, role }),
  setMemberRole: (projectId: string, employeeId: string, role: ProjectRole) => invoke<void>("set_member_role", { projectId, employeeId, role }),
  removeMember: (projectId: string, employeeId: string) => invoke<void>("remove_member", { projectId, employeeId }),
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
  mergeTask: (taskId: string) => invoke<MergeOutcome>("merge_task", { taskId }),
  abandonTask: (taskId: string) => invoke<void>("abandon_task", { taskId }),
  reveal: (taskId: string) => invoke<void>("reveal_in_finder", { taskId }),
  // 设置
  dataDir: () => invoke<string>("data_dir"),
  claudeVersion: () => invoke<string>("claude_version"),
  getConcurrency: () => invoke<number>("get_concurrency"),
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
