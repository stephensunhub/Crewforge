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
