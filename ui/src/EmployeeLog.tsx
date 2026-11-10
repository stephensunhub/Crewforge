import { useEffect, useState } from "react";
import { api } from "./api";
import { Modal } from "./common";
import type { LogRun } from "./types";

const STATUS: Record<LogRun["status"], [string, string]> = { running: ["进行中", "run"], done: ["完成", "merged"], failed: ["失败", "bad"], interrupted: ["被中断", ""] };
const day = (ts: number) => new Date(ts).toLocaleDateString("zh-CN", { month: "numeric", day: "numeric" });
const clock = (ts: number) => new Date(ts).toLocaleTimeString("zh-CN", { hour12: false });
const dur = (ms: number | null, start: number) => {
  const s = Math.round((ms ?? Math.max(0, Date.now() - start)) / 1000);
  return s < 60 ? `${s} 秒` : s < 3600 ? `${Math.floor(s / 60)} 分 ${s % 60} 秒` : `${Math.floor(s / 3600)} 小时 ${Math.floor((s % 3600) / 60)} 分`;
};

/** 员工的工作日志：最近每一次执行做了什么、进度、耗时。点开一次执行看工具调用流水和原始提示。 */
export function EmployeeLog({ id, name, running, onClose }: { id: string; name: string; running: boolean; onClose: () => void }) {
  const [runs, setRuns] = useState<LogRun[] | null>(null);
  const [open, setOpen] = useState<number | null>(null);
  const [err, setErr] = useState("");
  const load = () => { void api.employeeLog(id, 50).then(setRuns).catch((e) => setErr(String(e))); };
  useEffect(() => {
    load();
    if (!running) return;
    const t = setInterval(load, 5000);
    return () => clearInterval(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [id, running]);
  return (
    <Modal title={`${name} 的工作日志`} onClose={onClose} width={820}>
      {err && <div className="note bad">{err}</div>}
      {!runs ? <div className="hint">加载中…</div> : runs.length === 0 ? <div className="hint">还没有执行记录。</div> : (
        <div style={{ display: "grid", gap: 8 }}>
          <div className="hint">最近 {runs.length} 次执行，新的在前。点一条展开看做了什么。{running && "（工作中，每 5 秒刷新）"}</div>
          {runs.map((r, i) => {
            const [label, cls] = STATUS[r.status];
            const isOpen = open === i;
            return (
              <div key={r.started_at + ":" + i} className="panel-box" style={{ padding: 10 }}>
                <button className="link" style={{ display: "block", width: "100%", textAlign: "left" }} onClick={() => setOpen(isOpen ? null : i)}>
                  <span className={`badge ${cls}`} style={{ marginRight: 8 }}>{label}</span>
                  <b>{r.what}</b>{r.origin === "autopilot" ? "（自主推进）" : r.origin === "system" ? "（系统）" : ""} · {r.subject || "（无内容）"}
                  <div className="hint">{day(r.started_at)} {clock(r.started_at)}{r.ended_at ? ` → ${clock(r.ended_at)}` : ""} · 用时 {dur(r.duration_ms ?? (r.ended_at ? r.ended_at - r.started_at : null), r.started_at)}
                    {r.project ? ` · ${r.project}` : ""} · {r.tool_count} 次操作{r.tokens ? ` · ${r.tokens.toLocaleString()} token` : ""}{r.model ? ` · ${r.model}` : ""}{r.cost_usd ? ` · $${r.cost_usd.toFixed(3)}` : ""}</div>
                </button>
                {isOpen && (
                  <div style={{ marginTop: 8 }}>
                    {r.error && <div className="note bad" style={{ whiteSpace: "pre-wrap" }}>出错：{r.error}</div>}
                    <div className="hint" style={{ marginTop: 4 }}><b>收到的提示</b></div>
                    <pre className="codeblock" style={{ whiteSpace: "pre-wrap", maxHeight: 160, overflow: "auto" }}>{r.prompt || "（无）"}</pre>
                    {r.tools.length > 0 && (<>
                      <div className="hint" style={{ marginTop: 6 }}><b>操作流水</b>{r.tool_count > r.tools.length ? `（共 ${r.tool_count} 次，只显示最近 ${r.tools.length} 次）` : ""}</div>
                      <div className="ap-timeline" style={{ maxHeight: 240, overflow: "auto" }}>
                        {r.tools.map((t, k) => <div key={k} className="ap-line"><span className="time">{clock(t.ts)}</span><span><b>{t.tool}</b> {t.summary}</span></div>)}
                      </div></>)}
                    {r.last_text && (<><div className="hint" style={{ marginTop: 6 }}><b>最后说的话</b></div><div style={{ whiteSpace: "pre-wrap" }}>{r.last_text}</div></>)}
                  </div>
                )}
              </div>
            );
          })}
        </div>
      )}
    </Modal>
  );
}
