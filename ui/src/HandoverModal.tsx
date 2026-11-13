import { useEffect, useState } from "react";
import { api } from "./api";
import { Field, Modal, ROLE_LABEL } from "./common";
import type { Employee, Endpoint, HandoverPrep, Member, ProjectRole } from "./types";

/** 角色交接：先准备（系统整理状态快照 + 让原员工自己写摘要），看过、改过，再确认执行。 */
export function HandoverModal({ projectId, from, members, employees, onClose, onDone, onError }: {
  projectId: string; from: Member; members: Member[]; employees: Employee[];
  onClose: () => void; onDone: () => void; onError: (m: string) => void;
}) {
  const role = from.role as ProjectRole;
  const [eps, setEps] = useState<Endpoint[]>([]);
  const [caps, setCaps] = useState<Record<string, number>>({});
  const [to, setTo] = useState("");
  const [after, setAfter] = useState<"keep" | "remove">("keep");
  // 原员工名下还没开始的条目和行动项：keep 不动 · move 交给新人 · unassign 改回未指定，由自主推进按等级和能力重新分配
  const [work, setWork] = useState<"keep" | "move" | "unassign">("keep");
  const [ask, setAsk] = useState(true);
  const [busy, setBusy] = useState<"" | "prep" | "run">("");
  const [prep, setPrep] = useState<HandoverPrep | null>(null);
  const [summary, setSummary] = useState("");
  useEffect(() => {
    void api.listEndpoints().then(setEps).catch(() => setEps([]));
    void api.employeeLevels().then((l) => setCaps(Object.fromEntries(l))).catch(() => setCaps({}));
  }, []);

  // 只能交给同一类岗位的人（后端判断，同样的规则在执行交接时再检查一次）
  const [candidates, setCandidates] = useState<Employee[]>([]);
  const [loaded, setLoaded] = useState(false);
  useEffect(() => { void api.handoverCandidates(from.employee_id).then((l) => { setCandidates(l); setLoaded(true); }).catch((e) => { onError(String(e)); setLoaded(true); }); }, [from.employee_id, onError]);
  const label = (e: Employee) => {
    const ep = e.endpoint_id ? eps.find((x) => x.id === e.endpoint_id) : undefined;
    const via = ep ? `${ep.name}（本地）` : e.provider === "copilot" ? "Copilot" : "Claude 订阅";
    const inProject = members.find((m) => m.employee_id === e.id);
    return `${e.name} · ${via}${e.model ? ` · ${e.model}` : ""} · 上限 ${caps[e.id] ?? "?"} 级${inProject ? `（现在是${ROLE_LABEL[inProject.role as ProjectRole]}）` : "（不在项目里，会加入）"}`;
  };

  async function prepare() {
    setBusy("prep");
    try {
      const r = await api.handoverPrepare(projectId, from.employee_id, to, role, ask);
      setPrep(r);
      setSummary(r.model_summary ? `${r.snapshot}\n【${from.name} 的交接摘要】\n${r.model_summary}` : r.snapshot);
    } catch (e) { onError(String(e)); } finally { setBusy(""); }
  }
  async function execute() {
    setBusy("run");
    try {
      const rep = await api.handoverExecute(projectId, from.employee_id, to, role, summary, after, work);
      if (rep.warnings.length) onError(`已交接，但有提示：${rep.warnings.join("；")}`);
      onDone();
    } catch (e) { onError(String(e)); } finally { setBusy(""); }
  }

  return (
    <Modal title={`交接「${ROLE_LABEL[role]}」：${from.name} →`} onClose={busy ? () => {} : onClose} width={700}>
      <div className="form">
        {!prep && (
          <>
            <Field label="交给谁" hint={`只能交给和 ${from.name} 同一类岗位的人（TA 的岗位是「${employees.find((x) => x.id === from.employee_id)?.role ?? from.title}」）。可以是项目里的其他成员，也可以是还不在项目里的员工（会加入并接任）。`}>
              <select value={to} onChange={(e) => setTo(e.target.value)}>
                <option value="">{loaded && candidates.length === 0 ? "没有同岗位的人可以接手" : "请选择"}</option>
                {candidates.map((e) => <option key={e.id} value={e.id}>{label(e)}</option>)}
              </select>
            </Field>
            <Field label={`${from.name} 之后`}>
              <label className="tog"><input type="radio" checked={after === "keep"} onChange={() => { setAfter("keep"); setWork("keep"); }} /> 保留在项目里，改为「开发」</label>
              <label className="tog"><input type="radio" checked={after === "remove"} onChange={() => { setAfter("remove"); setWork(role === "lead" ? "unassign" : "move"); }} /> 移出项目</label>
            </Field>
            <Field label={`${from.name} 名下还没开始的条目和行动项`} hint="TA 手上进行中的任务不受这个选项影响：交接时会自动放弃（改动保存为 WIP 提交、分支保留，条目回到待开始），并写进交接说明。">
              <label className="tog"><input type="radio" checked={work === "keep"} onChange={() => setWork("keep")} /> 不动（保留为开发时继续做；移出项目时会改回「未指定」）</label>
              <label className="tog"><input type="radio" checked={work === "unassign"} onChange={() => setWork("unassign")} /> 改回「未指定」，由自主推进按等级和能力重新分配{role === "lead" ? "（推荐：负责人只做计划和高等级的任务，不用扛一堆开发条目）" : ""}</label>
              <label className="tog"><input type="radio" checked={work === "move"} onChange={() => setWork("move")} /> 全部交给新人</label>
            </Field>
            {role === "lead" && <div className="hint">交接负责人后，新负责人在下一个批次开始时会做一次「接手复盘」：给没有等级的条目补等级、按各人的能力上限重新分配还没开始的条目。只做这一次。</div>}
            <label className="tog" style={{ alignItems: "flex-start" }}>
              <input type="checkbox" checked={ask} onChange={(e) => setAsk(e.target.checked)} />
              <span>让 {from.name} 自己写一份交接摘要<span className="hint" style={{ display: "block" }}>续着 TA 在这个项目里的会话写，本地模型上要几分钟（最长等 5 分钟，超时就只用系统整理的状态快照）。不勾选就只有快照。</span></span>
            </label>
            <div className="form-btns">
              <button className="btn primary" disabled={!to || busy !== ""} onClick={() => void prepare()}>{busy === "prep" ? (ask ? "正在整理，等 TA 写摘要…" : "整理中…") : "下一步：生成交接摘要"}</button>
            </div>
          </>
        )}
        {prep && (
          <>
            {prep.warnings.map((w, i) => <div className="note bad" key={i}>⚠ {w}</div>)}
            {prep.summary_note && <div className="note">{prep.summary_note}</div>}
            <Field label="交接说明（可以修改）" hint="确认后会作为新人下一次被调用时的提示词开头（只给一次），同时记一条项目记忆和一条事件。">
              <textarea rows={16} value={summary} onChange={(e) => setSummary(e.target.value)} />
            </Field>
            <div className="form-btns">
              <button className="btn" disabled={busy !== ""} onClick={() => setPrep(null)}>上一步</button>
              <button className="btn primary" disabled={busy !== "" || !summary.trim()} onClick={() => void execute()}>{busy === "run" ? "交接中…" : `确认交接给 ${candidates.find((e) => e.id === to)?.name ?? ""}`}</button>
            </div>
          </>
        )}
      </div>
    </Modal>
  );
}
