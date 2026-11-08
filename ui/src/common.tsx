import type { ReactNode } from "react";
import type { ProjectRole } from "./types";

const COLORS = ["#ffb347", "#6ec1ff", "#9be37a", "#e58bff", "#ff8a8a", "#7fe3d0"];

export const ROLE_LABEL: Record<ProjectRole, string> = { lead: "负责人", developer: "开发", qa: "QA", reviewer: "审查" };
export const ROLE_ORDER: ProjectRole[] = ["lead", "developer", "qa", "reviewer"];

export const colorFor = (name: string) => {
  let h = 0;
  for (const c of name) h = (h * 31 + c.charCodeAt(0)) >>> 0;
  return COLORS[h % COLORS.length];
};
export const hhmm = (ts: number) => new Date(ts).toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit" });
export const short = (s: string, n = 90) => (s.length > n ? s.slice(0, n) + "…" : s);
export const basename = (p: string) => p.split("/").filter(Boolean).pop() ?? p;

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
