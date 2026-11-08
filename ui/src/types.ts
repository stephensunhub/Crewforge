// 与 Rust 端结构保持一致（后续可改为由 Rust 自动生成）
export interface Employee {
  id: string;
  name: string;
  role: string;
  persona: string;
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
  employee_id: string | null;
  session_id: string | null;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  payload: any;
}
export interface ApprovalReq {
  id: string;
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
