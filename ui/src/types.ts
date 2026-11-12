// 与 Rust 端结构保持一致（后续可改为由 Rust 自动生成）
export interface Employee {
  id: string;
  name: string;
  role: string;
  persona: string;
  archived: boolean;
  provider: "claude" | "copilot";
  model: string;
  endpoint_id: string | null;
  /** 员工编号（显示用，按创建顺序递增、不复用）；顾问为 0 */
  number: number;
  /** 手动设置的能力上限（1–5）；0 = 按模型自动 */
  level_cap: number;
}
export interface RenameReport { from: string; to: string; moved: string[]; warnings: string[]; persona_changed: boolean }
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
export type ProjectRole = "lead" | "developer" | "qa" | "reviewer" | "architect";
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
  author_kind: "human" | "employee" | "system" | "autopilot";
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
  since: number | null; totals: Totals; by_employee: CostGroup[]; by_project: CostGroup[]; by_purpose: CostGroup[]; by_model: CostGroup[]; by_provider: CostGroup[]; by_day: CostDay[]; by_hour: CostDay[]; latency: Latency;
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
  /** 等级 1–5，0 = 未定级 */
  level: number;
}
export interface PlanViewData {
  items: PlanItem[];
  counts: { proposed: number; planned: number; in_progress: number; done: number; blocked: number; cancelled: number };
  percent_done: number;
}
export interface PlanItemInput { title: string; description: string; acceptance: string; kind: string; module: string; assignee_id: string | null; deps: string[]; level: number }

export interface ModelEntry { provider: string; model_id: string; label: string; note: string; enabled: boolean; builtin: boolean }
export interface ProviderStatus { provider: string; label: string; enabled: boolean; installed: boolean; version: string; program: string; employees: number }
export interface ModelsOverview { providers: ProviderStatus[]; models: ModelEntry[]; purposes: [string, string, Record<string, string>][]; recommended: [string, string, string][] }

export interface UsageFilter {
  since?: number | null; until?: number | null; provider?: string | null; employeeId?: string | null; projectId?: string | null;
  model?: string | null; purpose?: string | null; status?: "ok" | "error" | null;
}
export interface SeriesPoint { label: string; ts: number; totals: Totals }
export interface UsageOverview {
  summary: Totals; granularity: "hour" | "day"; series: SeriesPoint[]; heatmap: SeriesPoint[]; latency: Latency;
  providers: CostGroup[]; models: CostGroup[]; employees: CostGroup[]; projects: CostGroup[]; median_speed: number | null;
}
export interface CallRec {
  event_id: number; ts: number; session_id: string | null; employee_id: string; employee: string; project_id: string; project: string;
  purpose: string; purpose_label: string; task_id: string | null; meeting_id: string | null; subject: string; provider: string;
  models: ModelUse[]; model: string | null; cost_usd: number | null; input_tokens: number | null; output_tokens: number | null;
  cache_read_tokens: number | null; cache_creation_tokens: number | null; premium_requests: number | null;
  duration_ms: number | null; ttft_ms: number | null; is_error: boolean; subtype: string;
}
export interface UsagePageData { total: number; rows: CallRec[] }

export interface DutyItem { kind: string; title: string; reason: string; goto: string; count: number; meeting_id: string | null; action_id: string | null }
export interface Duties { items: DutyItem[]; later_actions: number; plan_done: number; plan_total: number; plan_percent: number }

export interface AutopilotRun { id: string; project_id: string; status: string; max_calls: number; max_minutes: number; calls: number; started_at: number; ended_at: number | null; reason: string; uncounted_calls: number; chain: number }
export interface AutopilotConfig { max_calls: number; max_minutes: number; auto_continue: boolean; max_batches: number; standup_blocks: boolean }
export interface AutopilotStatus { run: AutopilotRun | null; live: boolean }

export interface DocFile { path: string; size: number; managed: boolean }
export interface DocsPublish { files: number; commit: string | null; note: string }
export interface Curated { verified: string[]; expired: string[]; merged: string[]; rejected: string[]; escalate: string | null; summary: string }

export interface SessionCtx {
  session_id: string; employee_id: string; employee: string; provider: string; path: string; status: string;
  context_tokens: number | null; updated_at: number; rotations: number;
}
export interface SessionsView { limit: number; sessions: SessionCtx[] }

export interface EndedProject { project: Project; end_kind: "completed" | "archived"; end_reason: string; ended_at: number | null }
export interface DiscardReport { tasks_removed: number; folder_trashed: string | null }

export interface GoalProposal {
  id: string; project_id: string; proposed_by: string | null; proposer_name: string | null;
  old_goal: string; new_goal: string; reason: string; status: string; note: string; created_at: number;
}

export interface AlignConfig {
  enabled: boolean; explicit: boolean; interval_hours: number; host_id: string | null; host_name: string | null; has_ai_lead: boolean;
  last_at: number | null; last_meeting_id: string | null; last_goal_note: string | null; last_check: string | null;
}
export interface AlignOverviewRow { project_id: string; name: string; config: AlignConfig; open_points: number }

export interface Endpoint { id: string; name: string; base_url: string; models: string[]; extra_env: Record<string, string>; max_concurrency: number; context_tokens: number; limit_budget: boolean; has_token: boolean; enabled: boolean; employees: number }
export interface EndpointInput { name: string; base_url: string; models: string[]; extra_env: Record<string, string>; max_concurrency: number; context_tokens: number; limit_budget: boolean; token: string | null }
export interface EndpointTest { ok: boolean; millis: number; detail: string }

export interface Activity { employee: string; what: string; subject: string; project: string | null; started_at: number; last_tool: string | null; last_tool_at: number | null; origin: string; project_id: string | null; tab: string; meeting_id: string | null; queue: QueueInfo | null; turn_finished_at: number | null }
export interface QueueInfo { endpoint: string; max: number; running: number; waiting: number; self_waiting: boolean; waiting_since: number | null; self_running: boolean }

export interface LogTool { ts: number; tool: string; summary: string }
export interface LogRun { started_at: number; ended_at: number | null; what: string; subject: string; prompt: string; project: string | null; origin: string; status: "running" | "done" | "failed" | "interrupted"; error: string | null; duration_ms: number | null; tool_count: number; tools: LogTool[]; last_text: string | null; cost_usd: number | null; tokens: number | null; model: string | null }

export interface DomainEntry { domain: string; warning: string | null }
export interface SandboxDomains { builtin: string[]; custom: DomainEntry[] }
export interface DomainCheck { domain: string; warning: string | null; already: boolean }
export interface StuckItem { employee: string; project_id: string | null; kind: "network" | "bypass" | "silent" | "long"; title: string; detail: string; hosts: string[]; elapsed_min: number; since: number; tab: string; meeting_id: string | null }

export interface BlockedMerge { task_id: string; employee: string; project_id: string | null; repo_path: string; title: string; kind: "blocked" | "conflict"; reason: string; files: string[]; since: number; dirty_files: string[]; can_retry: boolean }

export interface IvAction { kind: "goto" | "models" | "settings" | "whitelist" | "merge" | "log"; label: string; tab: string | null; domain: string | null; task_id: string | null; employee: string | null }
export interface Intervention { error: string | null; actions: IvAction[]; id: number; ts: number; category: string; label: string; title: string; detail: string; project_id: string | null; project_name: string | null; employee: string | null; tab: string }
export interface InterventionReport { items: Intervention[]; counts: [string, string, number][] }
