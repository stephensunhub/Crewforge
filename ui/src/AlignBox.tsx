import { useCallback, useEffect, useMemo, useState } from "react";
import { api } from "./api";
import { short } from "./common";
import type { AlignConfig, AlignOverviewRow, EventRow, Member } from "./types";

const INTERVALS: [number, string][] = [[4, "每 4 小时"], [12, "每 12 小时"], [24, "每天"], [72, "每 3 天"]];
export const when = (ts: number | null) => (ts ? new Date(ts).toLocaleString("zh-CN", { month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit" }) : "还没开过");

/** 项目里的对齐会设置：负责人带头对齐目标、计划、任务和行动项。 */
export function AlignBox({ projectId, events, onOpenMeeting, onError }: { projectId: string; events: EventRow[]; onOpenMeeting: (id: string) => void; onError: (m: string) => void }) {
  const [cfg, setCfg] = useState<AlignConfig | null>(null);
  const [members, setMembers] = useState<Member[]>([]);
  const [starting, setStarting] = useState(false);
  const load = useCallback(async () => { try { setCfg(await api.getAlignConfig(projectId)); setMembers(await api.members(projectId)); } catch (e) { onError(String(e)); } }, [projectId, onError]);
  const last = useMemo(() => events.reduce((m, e) => (e.project_id === projectId && e.kind === "alignment_done" ? Math.max(m, e.id) : m), 0), [events, projectId]);
  useEffect(() => { void load(); }, [load, last]);
  if (!cfg) return null;

  async function save(enabled: boolean, interval: number, host: string | null) {
    try { await api.setAlignConfig(projectId, enabled, interval, host); } catch (e) { onError(String(e)); }
    await load();
  }
  const hostValue = cfg.host_id ?? "";
  return (
    <section className="panel-box">
      <h4 className="group-title">对齐会</h4>
      <div className="hint" style={{ marginBottom: 8 }}>
        由负责人带头，定期对齐目标、计划、任务和行动项：系统汇总事实 → 在做事的成员各报告一次 → 对齐检查 → 主持人对照目标检查（目标不对时由负责人提议，你点头才改）→ 纪要，AI 负责人再处理待拍板的事。上次之后没有变化就不开，不花额度。
      </div>
      {!cfg.has_ai_lead && <div className="note" style={{ marginBottom: 8 }}>这个项目由你担任负责人，默认不开对齐会。要开的话指定一名成员代为主持，你只读结果，不用参会。</div>}
      <div className="row" style={{ gap: 10, flexWrap: "wrap", alignItems: "center" }}>
        <label className="tog" style={{ margin: 0 }}><input type="checkbox" checked={cfg.enabled} onChange={(e) => void save(e.target.checked, cfg.interval_hours, cfg.host_id)} />定期开对齐会</label>
        <select style={{ width: "auto" }} value={cfg.interval_hours} disabled={!cfg.enabled} onChange={(e) => void save(cfg.enabled, Number(e.target.value), cfg.host_id)}>
          {INTERVALS.map(([h, l]) => <option key={h} value={h}>{l}</option>)}
        </select>
        <span className="hint">主持：</span>
        <select style={{ width: "auto" }} value={hostValue} disabled={!cfg.enabled} onChange={(e) => void save(cfg.enabled, cfg.interval_hours, e.target.value || null)}>
          <option value="">{cfg.has_ai_lead ? "项目负责人" : "（请选择）"}</option>
          {members.map((m) => <option key={m.employee_id} value={m.employee_id}>{m.name}</option>)}
        </select>
        <button className="btn" disabled={starting} title="立刻开一次：没有人在做事时不会开" onClick={async () => {
          setStarting(true);
          try { const m = await api.startStandup(projectId); onOpenMeeting(m.id); } catch (e) { onError(String(e)); } finally { setStarting(false); }
        }}>{starting ? "准备中…" : "现在开一次"}</button>
      </div>
      {cfg.enabled && !cfg.host_name && <div className="note warn" style={{ marginTop: 8 }}>还没有主持人，请先指定一名成员，否则不会开。</div>}
      <div className="hint" style={{ marginTop: 8 }}>
        上次：{when(cfg.last_at)}
        {cfg.last_goal_note && <> · {short(cfg.last_goal_note, 140)}</>}
        {cfg.last_meeting_id && <> · <button className="link" onClick={() => onOpenMeeting(cfg.last_meeting_id!)}>打开会议</button></>}
      </div>
    </section>
  );
}

/** 公司页：各项目的对齐情况，一屏看完。 */
export function AlignOverview({ refreshKey, onOpenProject, onError }: { refreshKey: number; onOpenProject: (id: string) => void; onError: (m: string) => void }) {
  const [rows, setRows] = useState<AlignOverviewRow[]>([]);
  useEffect(() => { api.alignOverview().then(setRows).catch((e) => onError(String(e))); }, [refreshKey, onError]);
  if (rows.length === 0) return null;
  return (
    <section className="panel-box" style={{ marginTop: 16 }}>
      <h4 className="group-title">各项目对齐情况</h4>
      <div style={{ display: "grid", gap: 6 }}>
        {rows.map((r) => (
          <div className="row" key={r.project_id} style={{ gap: 10, alignItems: "baseline", flexWrap: "wrap" }}>
            <button className="link" onClick={() => onOpenProject(r.project_id)}><b>{r.name}</b></button>
            <span className="chip static">{r.config.enabled ? `${r.config.host_name ?? "无主持人"} 主持 · ${r.config.interval_hours} 小时` : "对齐会已关闭"}</span>
            <span className="hint">上次 {when(r.config.last_at)}{r.config.last_goal_note ? ` · ${short(r.config.last_goal_note, 100)}` : ""}</span>
            {r.open_points > 0 && <span className="hint">· 未处理行动项 {r.open_points}</span>}
          </div>
        ))}
      </div>
    </section>
  );
}
