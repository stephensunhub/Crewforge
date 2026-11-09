// 与 Rust 端结构保持一致（后续可改为由 Rust 自动生成）
export interface Employee {
  id: string;
  name: string;
  role: string;
  persona: string;
  archived: boolean;
  provider: "claude" | "copilot";
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
  onboarding: "none" | "in_progress" | "confirmed";
  goal: string;
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
export type ActionStatus = "open" | "working" | "done" | "dismissed" | "dropped";
export interface ActionItem {
  id: string; project_id: string; meeting_id: string; meeting_title: string; task_id: string | null; text: string;
  assignee_id: string | null; assignee_name: string | null; for_human: boolean; blocking: boolean; status: ActionStatus;
  work_task_id: string | null; note: string; created_at: number; resolved_at: number | null;
}
export interface ReviewInfo { meeting_id: string; title: string; status: "open" | "closed"; items: ActionItem[]; agent_replies: number }
export interface TaskView {
  task: Task;
  employee_name: string;
  files: FileChange[];
  merge: { clean: boolean; conflicts: string[] } | null;
  overlaps: { path: string; task_ids: string[] }[];
  reviews: ReviewInfo[];
}
export type MergeOutcome =
  | { outcome: "merged"; commit: string }
  | { outcome: "empty" }
  | { outcome: "conflict"; files: string[] }
  | { outcome: "blocked"; reason: string }
  | { outcome: "open_action_items"; items: string[] };

export type IssueKind = "technical" | "requirement" | "bug" | "risk" | "process" | "implementation";
export type IssueStatus = "open" | "discussing" | "pending_decision" | "decided" | "implementing" | "closed" | "shelved" | "cancelled";
export interface Issue {
  id: string;
  project_id: string;
  title: string;
  summary: string;
  kind: IssueKind;
  high_risk: boolean;
  status: IssueStatus;
  raised_by: string | null;
  owner_id: string | null;
  decision: string | null;
  decision_reason: string | null;
  decided_by: string | null;
  decided_at: number | null;
  supersedes: string | null;
  created_at: number;
  updated_at: number;
}
export interface IssueOption {
  id: string;
  issue_id: string;
  title: string;
  description: string;
  proposed_by: string | null;
  status: "candidate" | "chosen" | "rejected";
  reject_reason: string | null;
}
export interface IssueView {
  issue: Issue;
  options: IssueOption[];
  decider: string;
  raised_by_name: string | null;
  owner_name: string | null;
  supersedes_title: string | null;
  superseded_by: [string, string] | null;
}
export interface PendingDecision {
  issue: Issue;
  project_name: string;
}
export type MeetingKind = "requirements" | "design" | "code_review" | "qa_case" | "retro" | "planning" | "standup";
export interface Meeting {
  id: string;
  project_id: string;
  kind: MeetingKind;
  title: string;
  host_id: string | null;
  task_id: string | null;
  status: "open" | "closed";
  minutes: string;
  max_agent_replies: number;
  created_at: number;
  closed_at: number | null;
}
export interface MeetingMessage {
  id: number;
  meeting_id: string;
  author_kind: "human" | "employee" | "system";
  employee_id: string | null;
  content: string;
  created_at: number;
}
export interface MeetingDetail {
  meeting: Meeting;
  messages: MeetingMessage[];
  issue_ids: string[];
  agent_replies: number;
  guests: Guest[];
}
export interface Guest { employee_id: string; name: string; brief: string; invited_by: string; created_at: number; expires_at: number; active: boolean }

export type MemoryLayer = "company" | "project" | "employee" | "task";
export type MemoryKind = "lesson" | "decision" | "fact" | "convention" | "pitfall";
export interface Memory {
  id: string;
  layer: MemoryLayer;
  project_id: string | null;
  employee_id: string | null;
  task_id: string | null;
  kind: MemoryKind;
  title: string;
  content: string;
  status: "unverified" | "verified" | "expired";
  source_kind: "human" | "meeting" | "issue" | "promoted";
  source_ref: string | null;
  scope: string;
  related_commit: string | null;
  promoted_from: string | null;
  created_by: string;
  created_at: number;
  verified_at: number | null;
  verified_by: string | null;
  expired_at: number | null;
  expired_reason: string | null;
  updated_at: number;
  uses: number;
}

export interface BaselineCommand { id: string; label: string; kind: string; command: string; reason: string; default_on: boolean; candidate?: string | null }
export interface ScanModule { name: string; path: string; files: number; kind: string }
export interface GitInfo {
  is_repo: boolean; branch: string | null; head: string | null; commit_count: number | null; branch_count: number | null;
  tag_count: number | null; first_commit_date: string | null; last_commit: string | null; recent: string[];
  authors: [string, number][]; remotes: string[]; dirty: boolean;
}
export interface ScanReport {
  root: string; scanned_at: number; total_files: number; truncated: boolean; languages: [string, number][];
  package_managers: string[]; build_tools: string[]; frameworks: string[]; databases: string[]; test_frameworks: string[];
  ci: string[]; api_definitions: string[]; db_schemas: string[]; docs: string[]; top_dirs: [string, number][];
  modules: ScanModule[]; git: GitInfo; commands: BaselineCommand[]; notes: string[];
}
export interface CommandResult {
  id: string; label: string; kind: string; command: string; status: "passed" | "failed" | "timeout" | "skipped" | "error";
  exit_code: number | null; duration_ms: number; output_tail: string; failed_tests: string[]; parsed: boolean;
}
export interface BaselineRun {
  id: string; project_id: string; commit_sha: string; status: "running" | "done" | "failed";
  started_at: number; finished_at: number | null; commands: BaselineCommand[]; results: CommandResult[];
}
export interface OnboardingReport { id: string; kind: "architect" | "qa"; employee_id: string | null; content: string; commit_sha: string | null; created_at: number }
export interface OnboardingState { status: "none" | "in_progress" | "confirmed"; scan: ScanReport | null; baseline: BaselineRun | null; reports: OnboardingReport[] }
export type Origin = "pre_existing" | "introduced" | "fixed" | "unknown";
export interface Finding { command_id: string; command_label: string; item: string; origin: Origin; note: string }
export interface CheckReport { findings: Finding[]; results: CommandResult[]; pre_existing: number; introduced: number; fixed: number; unknown: number }
export interface StoredCheck { created_at: number; report: CheckReport }

export interface Totals {
  turns: number; errors: number; cost_usd: number; turns_with_cost: number; input_tokens: number; output_tokens: number;
  cache_read_tokens: number; cache_creation_tokens: number; turns_with_tokens: number; duration_ms: number; turns_with_duration: number;
  premium_requests: number;
}
export interface CostGroup { key: string; label: string; totals: Totals }
export interface CostDay { day: string; totals: Totals }
export interface CostTask { task_id: string; title: string; employee: string; project: string; totals: Totals }
export interface Efficiency {
  avg_context_per_turn: number | null; cache_hit_ratio: number | null; resumed_starts: number; total_starts: number; failed_runs: number;
  recovered_sessions: number; memory_injections: number; memory_items: number; memory_chars: number; avg_turns_per_task: number | null;
  meeting_human_messages: number; meeting_agent_replies: number;
}
export interface QuotaPoint { ts: number; five_hour: number | null; seven_day: number | null }
export interface QuotaNow { ts: number; status: string; five_hour: number | null; seven_day: number | null; five_resets_at: number | null; seven_resets_at: number | null }
export interface Latency { p50_ms: number | null; p95_ms: number | null; avg_ttft_ms: number | null; ttft_samples: number }
export interface ModelUse { model: string; input: number; output: number; cache_read: number; cache_creation: number; cost_usd: number | null }
export interface CallRow {
  event_id: number; ts: number; employee: string; project: string; purpose: string; purpose_label: string; subject: string; models: ModelUse[];
  cost_usd: number | null; input_tokens: number | null; output_tokens: number | null; cache_read_tokens: number | null; cache_creation_tokens: number | null;
  duration_ms: number | null; ttft_ms: number | null; is_error: boolean; subtype: string;
  provider: string; model: string | null; premium_requests: number | null;
}
export interface CallPage { total: number; rows: CallRow[] }
export interface CostReport {
  since: number | null; totals: Totals; by_employee: CostGroup[]; by_project: CostGroup[]; by_purpose: CostGroup[]; by_model: CostGroup[]; by_day: CostDay[]; by_hour: CostDay[]; latency: Latency;
  top_tasks: CostTask[]; efficiency: Efficiency; quota_now: QuotaNow | null; quota_series: QuotaPoint[];
}

export interface SchedulerConfig { enabled: boolean; soft: number; hard: number; stop: number }
export type SchedLevel = "normal" | "throttled" | "paused";
export interface SchedStatus {
  config: SchedulerConfig; user_limit: number; effective_limit: number; running: number; waiting: number;
  level: SchedLevel; reason: string; resume_at: number | null; manual_pause: boolean; override_until: number | null;
  quota: QuotaNow | null;
}

export type PlanStatus = "proposed" | "planned" | "in_progress" | "done" | "blocked" | "cancelled";
export interface PlanItem {
  id: string; project_id: string; title: string; description: string; acceptance: string; kind: string; module: string; position: number;
  assignee_id: string | null; assignee_name: string | null; status: PlanStatus; task_id: string | null; proposed_by: string | null;
  proposed_by_name: string | null; note: string; deps: string[]; waiting_for: string[]; ready: boolean;
}
export interface PlanViewData {
  items: PlanItem[];
  counts: { proposed: number; planned: number; in_progress: number; done: number; blocked: number; cancelled: number };
  percent_done: number;
}
export interface PlanItemInput { title: string; description: string; acceptance: string; kind: string; module: string; assignee_id: string | null; deps: string[] }
