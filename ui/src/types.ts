// 与 Rust 端结构保持一致（后续可改为由 Rust 自动生成）
export interface Employee {
  id: string;
  name: string;
  role: string;
  persona: string;
  archived: boolean;
}
export interface Company {
  name: string;
  description: string;
  conventions: string;
}
export interface Project {
  id: string;
  name: string;
  description: string;
  path: string;
  status: string;
  created_at: number;
  updated_at: number;
}
export interface ProjectSummary {
  project: Project;
  member_count: number;
  active_tasks: number;
  lead_name: string | null;
}
export type ProjectRole = "lead" | "developer" | "qa" | "reviewer";
export interface Member {
  employee_id: string;
  name: string;
  title: string;
  role: ProjectRole;
}
export interface EmployeeCard {
  employee: Employee;
  projects: [string, string][];
  active_tasks: number;
}
export interface Session {
  id: string;
  employee_id: string;
  project_path: string;
  status: string;
  started: boolean;
}
export interface EventRow {
  id: number;
  ts: number;
  kind: string;
  project_id: string | null;
  employee_id: string | null;
  session_id: string | null;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  payload: any;
}
export interface ApprovalReq {
  id: string;
  project_id: string | null;
  employee: string;
  tool: string;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  input: any;
  reason: string;
}

export interface RepoStatus {
  is_repo: boolean;
  root: string | null;
  branch: string | null;
  has_commits: boolean;
  dirty: boolean;
}
export interface FileChange {
  path: string;
  insertions: number | null;
  deletions: number | null;
  untracked: boolean;
}
export interface Task {
  id: string;
  repo_path: string;
  employee_id: string;
  title: string;
  branch: string;
  base_branch: string;
  base_commit: string;
  worktree_path: string;
  status: "active" | "merged" | "abandoned";
  merged_commit: string | null;
  created_at: number;
  updated_at: number;
}
export interface TaskView {
  task: Task;
  employee_name: string;
  files: FileChange[];
  merge: { clean: boolean; conflicts: string[] } | null;
  overlaps: { path: string; task_ids: string[] }[];
}
export type MergeOutcome =
  | { outcome: "merged"; commit: string }
  | { outcome: "empty" }
  | { outcome: "conflict"; files: string[] }
  | { outcome: "blocked"; reason: string };
