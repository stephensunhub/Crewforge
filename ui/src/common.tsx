import { useState, type ReactNode } from "react";
import type { Activity, ProjectRole } from "./types";

const COLORS = ["#ffb347", "#6ec1ff", "#9be37a", "#e58bff", "#ff8a8a", "#7fe3d0"];

export const ROLE_LABEL: Record<ProjectRole, string> = { lead: "负责人", architect: "架构师", developer: "开发", qa: "QA", reviewer: "审查" };
export const ROLE_ORDER: ProjectRole[] = ["lead", "architect", "developer", "qa", "reviewer"];

export const colorFor = (name: string) => {
  let h = 0;
  for (const c of name) h = (h * 31 + c.charCodeAt(0)) >>> 0;
  return COLORS[h % COLORS.length];
};
export const hhmm = (ts: number) => new Date(ts).toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit" });
export const short = (s: string, n = 90) => (s.length > n ? s.slice(0, n) + "…" : s);
export const basename = (p: string) => p.split("/").filter(Boolean).pop() ?? p;

/** 审批卡片里展示被请求的操作。命令按原样显示（保留换行），不要当成一行 JSON 字符串让人去读转义。 */
export function formatApprovalInput(tool: string, input: Record<string, unknown> | undefined): string {
  if (!input) return "";
  if (tool === "Bash" && typeof input.command === "string") {
    const d = typeof input.description === "string" && input.description ? `# ${input.description}\n` : "";
    return `${d}${input.command}`;
  }
  if (typeof input.file_path === "string") {
    const body = typeof input.content === "string" ? `\n---\n${input.content}` : typeof input.new_string === "string" ? `\n--- 替换为 ---\n${input.new_string}` : "";
    return `${input.file_path}${body}`;
  }
  return JSON.stringify(input, null, 2);
}

export function Avatar({ name, size = 30 }: { name: string; size?: number }) {
  return (
    <div className="avatar" style={{ background: colorFor(name), width: size, height: size, fontSize: size * 0.45 }}>
      {name.slice(0, 1).toUpperCase()}
    </div>
  );
}

/** 尚未实现的页面：说明它将承载什么，避免空白。 */
export function Placeholder({ title, children }: { title: string; children: ReactNode }) {
  return (
    <div className="placeholder">
      <img src="/logo.png" alt="" />
      <h2>{title}</h2>
      <div className="hint">{children}</div>
      <span className="badge">即将推出</span>
    </div>
  );
}

export function Modal({ title, onClose, children, width = 520 }: { title: string; onClose: () => void; children: ReactNode; width?: number }) {
  return (
    <div className="modal-back" onClick={onClose}>
      <div className="modal" style={{ width: `min(${width}px, 92vw)` }} onClick={(e) => e.stopPropagation()}>
        <div className="modal-head">
          <b>{title}</b>
          <button className="btn" onClick={onClose}>关闭</button>
        </div>
        <div className="modal-body">{children}</div>
      </div>
    </div>
  );
}

export function Field({ label, hint, children }: { label: string; hint?: string; children: ReactNode }) {
  return (
    <label className="field">
      <span>{label}</span>
      {children}
      {hint && <span className="hint">{hint}</span>}
    </label>
  );
}

export const ISSUE_KIND_LABEL: Record<string, string> = {
  technical: "技术方案", requirement: "需求澄清", bug: "Bug 归因", risk: "风险", process: "流程", implementation: "代码细节",
};
export const ISSUE_STATUS_LABEL: Record<string, string> = {
  open: "待讨论", discussing: "讨论中", pending_decision: "待决策", decided: "已决议",
  implementing: "实施中", closed: "已关闭", shelved: "搁置", cancelled: "已取消",
};
export const MEETING_KIND_LABEL: Record<string, string> = {
  requirements: "需求评审", design: "方案评审", code_review: "Code Review", qa_case: "QA Case 评审", retro: "复盘", planning: "制定计划", standup: "对齐会",
};

export const MEMORY_KIND_LABEL: Record<string, string> = { lesson: "经验", decision: "决策", fact: "事实", convention: "规范", pitfall: "踩过的坑" };
export const MEMORY_STATUS_LABEL: Record<string, string> = { unverified: "待验证", verified: "已验证", expired: "已过期" };
export const MEMORY_SOURCE_LABEL: Record<string, string> = { human: "人工写入", meeting: "会议纪要", issue: "议题决议", promoted: "学习自项目", onboarding: "导入时确认", agent: "员工提议" };

/** 从“待你处理”点进会议时，要定位到哪一条行动项（打开会议是异步的，用模块级变量传递，行动项加载后取走）。 */
export const focusAction: { id: string | null } = { id: null };

/** 需要填写原因的按钮。WebView 里没有 window.prompt（调用会直接返回 null、什么都不发生），所以就地展开一个输入框。 */
export function ReasonButton({ label, placeholder, required = false, className = "btn", onSubmit }: {
  label: string; placeholder: string; required?: boolean; className?: string; onSubmit: (reason: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const [text, setText] = useState("");
  if (!open) return <button className={className} onClick={() => setOpen(true)}>{label}</button>;
  const ok = !required || text.trim() !== "";
  const submit = () => { if (!ok) return; onSubmit(text.trim()); setOpen(false); setText(""); };
  return (
    <span className="reason-box">
      <input autoFocus value={text} placeholder={placeholder} onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => { if (e.key === "Enter") submit(); if (e.key === "Escape") setOpen(false); }} />
      <button className="btn primary" disabled={!ok} onClick={submit}>确认{label}</button>
      <button className="btn" onClick={() => { setOpen(false); setText(""); }}>取消</button>
    </span>
  );
}


const ago = (ts: number) => { const m = Math.max(0, Math.round((Date.now() - ts) / 60000)); return m < 1 ? "刚刚" : m < 60 ? `${m} 分钟` : `${Math.floor(m / 60)} 小时 ${m % 60} 分钟`; };

/** 员工此刻在做什么：类型、内容、已经多久、最近一个动作。 */
export function ActivityLine({ a, showProject, onOpen }: { a: Activity | undefined; showProject?: boolean; onOpen?: (a: Activity) => void }) {
  if (!a) return <div className="hint">正在工作（详情加载中…）</div>;
  const quiet = a.last_tool_at ? Math.round((Date.now() - a.last_tool_at) / 60000) : null;
  return (
    <div className={`activity${onOpen ? " clickable" : ""}`} title={onOpen ? `点击跳到：${a.subject}` : a.subject} role={onOpen ? "button" : undefined} onClick={onOpen ? (e) => { e.stopPropagation(); onOpen(a); } : undefined}>
      <b>{a.what}</b>{a.origin === "autopilot" ? "（自主推进派的）" : ""}{showProject && a.project ? ` · ${a.project}` : ""} · {short(a.subject, 40)}
      <div className="hint">{a.turn_finished_at && !a.queue?.self_waiting ? `模型这一轮 ${ago(a.turn_finished_at)}前已结束，任务在走后面的步骤（等评审、合并、验证）` : a.queue?.self_waiting ? `已等 ${ago(a.started_at)} · 前面的请求还没做完（${a.queue.endpoint} 同时最多 ${a.queue.max} 个，${a.queue.running} 个在用、${a.queue.waiting} 个在等）` : <>已 {ago(a.started_at)}{a.last_tool ? ` · 最近动作 ${a.last_tool}（${quiet === 0 ? "刚刚" : `${quiet} 分钟前`}）` : " · 还没有动作（可能在等模型首轮响应）"}{quiet !== null && quiet >= 10 ? " · ⚠ 已经 10 分钟没有新动作" : ""}{a.queue && a.queue.running > 1 ? ` · ${a.queue.endpoint} 上有 ${a.queue.running} 个请求同时在跑，本地模型会排队处理，慢不一定是卡住` : ""}</>}{onOpen ? " · 点击查看 ›" : ""}</div>
    </div>
  );
}
