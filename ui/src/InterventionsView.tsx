import { useCallback, useEffect, useState } from "react";
import { api } from "./api";
import { Field, Modal } from "./common";
import { EmployeeLog } from "./EmployeeLog";
import type { Employee, Intervention, InterventionReport, IvAction, ProjectSummary } from "./types";

const DAYS: [number, string][] = [[1, "今天"], [7, "近 7 天"], [30, "近 30 天"], [365, "全部"]];
const stamp = (ts: number) => { const d = new Date(ts); const p = (n: number) => String(n).padStart(2, "0"); return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`; };

/** 人工介入中心：每一次需要你出手的事汇总在这里；哪类最多，就是最该先自动化的。 */
export function InterventionsView({ projects, employees, running, events, onOpenWork, onGoScope, onError }: {
  projects: ProjectSummary[]; employees: Employee[]; running: string[]; events: { id: number; kind: string }[];
  onOpenWork: (projectId: string, tab: string) => void; onGoScope: (k: "models" | "settings") => void; onError: (m: string) => void;
}) {
  const [days, setDays] = useState(7);
  const [cat, setCat] = useState<string | null>(null);
  const [rep, setRep] = useState<InterventionReport | null>(null);
  const [noting, setNoting] = useState(false);
  const [logOf, setLogOf] = useState<Employee | null>(null);
  const load = useCallback(() => { void api.interventions(days, cat).then(setRep).catch((e) => onError(String(e))); }, [days, cat, onError]);
  useEffect(() => { load(); }, [load]);
  // 有新的相关事件时刷新
  const last = events.length ? events[events.length - 1].id : 0;
  useEffect(() => { if (last) load(); }, [last, load]);
  // 点操作按钮：都是在应用里直接完成的
  async function run(i: Intervention, a: IvAction) {
    try {
      if (a.kind === "goto" && i.project_id) onOpenWork(i.project_id, a.tab ?? i.tab);
      else if (a.kind === "models" || a.kind === "settings") onGoScope(a.kind);
      else if (a.kind === "whitelist" && a.domain) { if (await api.addSandboxDomainChecked(a.domain)) load(); }
      else if (a.kind === "log") { const e = employees.find((x) => x.name === a.employee); if (e) setLogOf(e); else onError("找不到这位员工（可能已归档）"); }
      else if (a.kind === "merge" && a.task_id) {
        const r = await api.mergeTask(a.task_id);
        if (r.outcome === "conflict") onError(`合并冲突：${r.files.join("、")}`);
        else if (r.outcome === "blocked") onError(`仍然无法合并：${r.reason}`);
        else if (r.outcome === "open_action_items") onError(`评审里还有 ${r.items.length} 条“合并前”的行动项没处理，请先到会议页处理`);
        load();
      }
    } catch (e) { onError(String(e)); }
  }
  const total = rep?.counts.reduce((n, c) => n + c[2], 0) ?? 0;

  return (
    <div className="page wide">
      <div className="page-head">
        <div><h2>人工介入</h2>
          <div className="hint">每一次需要你出手的事：你批准的审批、加白名单、拍板议题、批准计划、合并被拦、卡住了、升级给你的事，以及你在应用之外手动做的事。哪一类最多，就是最值得先自动化的。</div></div>
        <button className="btn primary" onClick={() => setNoting(true)}>＋ 手动记一笔</button>
      </div>

      <div style={{ display: "flex", gap: 8, flexWrap: "wrap", alignItems: "center", marginBottom: 10 }}>
        {DAYS.map(([d, l]) => <button key={d} className={`chip ${days === d ? "on" : ""}`} onClick={() => setDays(d)}>{l}</button>)}
        <span className="hint" style={{ marginLeft: 8 }}>共 {total} 次</span>
      </div>
      <div style={{ display: "flex", gap: 8, flexWrap: "wrap", marginBottom: 12 }}>
        <button className={`chip ${cat === null ? "on" : ""}`} onClick={() => setCat(null)}>全部</button>
        {rep?.counts.map(([k, label, n]) => <button key={k} className={`chip ${cat === k ? "on" : ""}`} onClick={() => setCat(cat === k ? null : k)}>{label} · {n}</button>)}
      </div>

      {!rep ? <div className="hint">加载中…</div> : rep.items.length === 0 ? (
        <div className="hint">这段时间没有需要你介入的事。</div>
      ) : (
        <div style={{ display: "grid", gap: 6 }}>
          {rep.items.map((i) => (
            <div className="duty-item" key={i.id}>
              <div style={{ minWidth: 0 }}>
                <span className="badge" style={{ marginRight: 8 }}>{i.label}</span><b>{i.title}</b>
                {i.error
                  ? <pre className="codeblock" style={{ whiteSpace: "pre-wrap", margin: "4px 0", maxHeight: 120, overflow: "auto" }}>{i.error}</pre>
                  : i.detail && <div className="hint" style={{ marginTop: 3 }}>{i.detail}</div>}
                <div className="hint" style={{ marginTop: 3 }}>{stamp(i.ts)}{i.project_name ? ` · ${i.project_name}` : ""}{i.employee && i.category !== "approval" ? ` · ${i.employee}` : ""}</div>
              </div>
              <div style={{ display: "flex", gap: 6, flexWrap: "wrap", justifyContent: "flex-end", flex: "none", maxWidth: 260 }}>
                {i.actions.filter((a) => a.kind !== "goto" || i.project_id).map((a, k) => (
                  <button key={k} className={`btn ${k === 0 && a.kind !== "goto" ? "primary" : ""}`} onClick={() => void run(i, a)}>{a.label}</button>
                ))}
              </div>
            </div>
          ))}
        </div>
      )}
      {logOf && <EmployeeLog id={logOf.id} name={logOf.name} running={running.includes(logOf.name)} onClose={() => setLogOf(null)} />}
      {noting && <NoteModal projects={projects} onClose={() => setNoting(false)} onSaved={() => { setNoting(false); load(); }} onError={onError} />}
    </div>
  );
}

function NoteModal({ projects, onClose, onSaved, onError }: { projects: ProjectSummary[]; onClose: () => void; onSaved: () => void; onError: (m: string) => void }) {
  const [text, setText] = useState("");
  const [pid, setPid] = useState("");
  const [busy, setBusy] = useState(false);
  return (
    <Modal title="手动记一笔" onClose={onClose} width={520}>
      <div className="form">
        <Field label="你做了什么" hint="记下你在应用之外做的事，比如“在终端里手动执行了 go get，之后还原了 go.mod”。以后回头看，才知道哪些事该让系统自己做。">
          <textarea rows={4} value={text} onChange={(e) => setText(e.target.value)} autoFocus />
        </Field>
        <Field label="相关项目（可选）">
          <select value={pid} onChange={(e) => setPid(e.target.value)}>
            <option value="">不指定</option>
            {projects.map((p) => <option key={p.project.id} value={p.project.id}>{p.project.name}</option>)}
          </select>
        </Field>
        <div className="form-btns">
          <button className="btn primary" disabled={!text.trim() || busy} onClick={async () => { setBusy(true); try { await api.addInterventionNote(pid || null, text); onSaved(); } catch (e) { onError(String(e)); } finally { setBusy(false); } }}>保存</button>
        </div>
      </div>
    </Modal>
  );
}
