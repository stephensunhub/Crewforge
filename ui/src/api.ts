import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { ask, open } from "@tauri-apps/plugin-dialog";
import type { ApprovalReq, Employee, EventRow, MergeOutcome, RepoStatus, Session, TaskView } from "./types";

export const api = {
  employees: () => invoke<Employee[]>("list_employees"),
  addEmployee: (name: string, role: string, persona: string) =>
    invoke<Employee>("add_employee", { name, role, persona }),
  sessions: () => invoke<Session[]>("list_sessions"),
  recentEvents: (limit: number) => invoke<EventRow[]>("recent_events", { limit }),
  running: () => invoke<string[]>("running_employees"),
  dataDir: () => invoke<string>("data_dir"),
  getConcurrency: () => invoke<number>("get_concurrency"),
  setConcurrency: (n: number) => invoke<number>("set_concurrency", { n }),
  runTask: (employee: string, prompt: string, projectPath: string) =>
    invoke<void>("run_task", { employee, prompt, projectPath }),
  respondApproval: (id: string, allow: boolean) => invoke<boolean>("respond_approval", { id, allow }),
  repoStatus: (projectPath: string) => invoke<RepoStatus>("repo_status", { projectPath }),
  initRepo: (projectPath: string) => invoke<void>("init_repo", { projectPath }),
  taskViews: (projectPath: string) => invoke<TaskView[]>("list_task_views", { projectPath }),
  taskDiff: (taskId: string) => invoke<string>("task_diff", { taskId }),
  mergeTask: (taskId: string) => invoke<MergeOutcome>("merge_task", { taskId }),
  abandonTask: (taskId: string) => invoke<void>("abandon_task", { taskId }),
  reveal: (taskId: string) => invoke<void>("reveal_in_finder", { taskId }),
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
