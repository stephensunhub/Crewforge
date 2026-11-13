import { useCallback, useEffect, useMemo, useState } from "react";
import { api } from "./api";
import { Field, Modal } from "./common";
import { AlignBox } from "./AlignBox";
import type { EventRow, Schedule } from "./types";

const STATUS: Record<string, string> = { proposed: "待确认", planned: "待开始", in_progress: "进行中", done: "已完成", blocked: "已阻塞", cancelled: "已取消" };
const WEEKDAYS = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"];
const KIND_LABEL: Record<string, string> = { other: "其他", characterize: "补测试", cleanup: "清理", migrate: "实现" };
const when = (ms: number | null) => (ms ? new Date(ms).toLocaleString("zh-CN", { month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit" }) : "—");
const cadenceText = (s: Pick<Schedule, "cadence" | "weekday" | "hour">) => `${s.cadence === "daily" ? "每天" : `每${WEEKDAYS[s.weekday]}`} ${String(s.hour).padStart(2, "0")}:00`;

/** 定时器：到点就在计划里建一个未排期的条目（上一次的还没做完就不重复建）。条目不会自己开始花额度，等负责人排期、自主推进来做。 */
export function TimersView({ projectId, events, onOpenMeeting, onError, onChanged }: { projectId: string; events: EventRow[]; onOpenMeeting: (id: string) => void; onError: (m: string) => void; onChanged: () => void }) {
  const [scheds, setScheds] = useState<Schedule[]>([]);
  const [presets, setPresets] = useState<[string, string, string, string][]>([]);
  useEffect(() => { void api.schedulePresets().then(setPresets).catch(() => setPresets([])); }, []);
  const [modal, setModal] = useState(false);
  const load = useCallback(async () => { try { setScheds(await api.listSchedules(projectId)); } catch (e) { onError(String(e)); } }, [projectId, onError]);
  useEffect(() => { void load(); }, [load]);
  const last = useMemo(() => events.reduce((m, e) => (e.project_id === projectId && (e.kind.startsWith("schedule_") || e.kind.startsWith("plan_")) ? Math.max(m, e.id) : m), 0), [events, projectId]);
  useEffect(() => { if (last) void load(); }, [last, load]);
  const act = async (f: () => Promise<unknown>) => { try { await f(); await load(); onChanged(); return true; } catch (e) { onError(String(e)); return false; } };
  return (
    <div className="page">
      <div className="page-head">
        <div><h2>定时器</h2><div className="hint">项目里所有定时运行的东西都在这里：<b>对齐会</b>（防止大家做着做着偏离目标），和下面预置好的<b>检查类定时任务</b>。预置的默认全部关闭，由你决定启用哪些。检查任务到点会在计划里建一个<b>未排期</b>的条目，由负责人排期；应用没开着错过的时间点不补建。</div></div>
        <button className="btn primary" onClick={() => setModal(true)}>＋ 新建定时任务</button>
      </div>
      <section className="maint-sec"><AlignBox projectId={projectId} events={events} onOpenMeeting={onOpenMeeting} onError={onError} /></section>
      <h3 style={{ margin: "6px 0" }}>检查类定时任务</h3>
      {scheds.map((s) => (
        <div className="maint-row" key={s.id}>
          <label className="tog" title={s.enabled ? "点击停用" : "点击启用"}><input type="checkbox" checked={s.enabled} onChange={(e) => void act(() => api.setScheduleEnabled(s.id, e.target.checked))} /></label>
          <span className="maint-title" title={s.description}>{s.title}</span>
          <span className="hint">{cadenceText(s)} · 下次 {s.enabled ? when(s.next_run_at) : "已停用"}{s.last_run_at ? ` · 上次 ${when(s.last_run_at)}${s.last_item_status ? `（${STATUS[s.last_item_status] ?? s.last_item_status}）` : ""}` : ""}</span>
          <button className="btn" onClick={() => void act(() => api.deleteSchedule(s.id))}>删除</button>
        </div>
      ))}
      {presets.filter(([t]) => !scheds.some((s) => s.title === t)).map(([t, k, d, c]) => (
        <div className="maint-row" key={`preset-${t}`}>
          <label className="tog" title="启用：每周一 09:00（每天的是每天 09:00），之后可以删了重建来改时间"><input type="checkbox" checked={false} onChange={() => void act(() => api.addSchedule(projectId, t, d, k, c, 0, 9))} /></label>
          <span className="maint-title" title={d}>{t}</span>
          <span className="hint">预置 · {c === "daily" ? "每天" : "每周一"} 09:00 · 未启用</span>
        </div>
      ))}
      {modal && <ScheduleForm onClose={() => setModal(false)} onSubmit={async (a) => { if (await act(() => api.addSchedule(projectId, a.title, a.description, a.kind, a.cadence, a.weekday, a.hour))) setModal(false); }} />}
    </div>
  );
}

function ScheduleForm({ onClose, onSubmit }: { onClose: () => void; onSubmit: (a: { title: string; description: string; kind: string; cadence: string; weekday: number; hour: number }) => Promise<void> }) {
  const [presets, setPresets] = useState<[string, string, string, string][]>([]);
  useEffect(() => { void api.schedulePresets().then(setPresets).catch(() => setPresets([])); }, []);
  const [a, setA] = useState({ title: "", description: "", kind: "other", cadence: "weekly", weekday: 0, hour: 9 });
  const [busy, setBusy] = useState(false);
  return (
    <Modal title="新建定时任务" onClose={onClose} width={620}>
      <div className="form">
        {presets.length > 0 && <Field label="从模板开始"><div className="maint-btns">{presets.map(([t, k, d, c]) => <button key={t} className="chip" onClick={() => setA((x) => ({ ...x, title: t, kind: k, description: d, cadence: c }))}>{t}</button>)}</div></Field>}
        <Field label="名称"><input value={a.title} onChange={(e) => setA({ ...a, title: e.target.value })} /></Field>
        <Field label="每次要做什么" hint="写给员工的任务说明。"><textarea rows={5} value={a.description} onChange={(e) => setA({ ...a, description: e.target.value })} /></Field>
        <div className="maint-btns">
          <Field label="周期"><select value={a.cadence} onChange={(e) => setA({ ...a, cadence: e.target.value })}><option value="daily">每天</option><option value="weekly">每周</option></select></Field>
          {a.cadence === "weekly" && <Field label="星期"><select value={a.weekday} onChange={(e) => setA({ ...a, weekday: Number(e.target.value) })}>{WEEKDAYS.map((w, i) => <option key={w} value={i}>{w}</option>)}</select></Field>}
          <Field label="几点（本机时间）"><select value={a.hour} onChange={(e) => setA({ ...a, hour: Number(e.target.value) })}>{Array.from({ length: 24 }, (_, h) => <option key={h} value={h}>{String(h).padStart(2, "0")}:00</option>)}</select></Field>
          <Field label="条目类型"><select value={a.kind} onChange={(e) => setA({ ...a, kind: e.target.value })}>{Object.entries(KIND_LABEL).map(([k, v]) => <option key={k} value={k}>{v}</option>)}</select></Field>
        </div>
        <div className="form-btns"><button className="btn primary" disabled={busy || !a.title.trim() || !a.description.trim()} onClick={async () => { setBusy(true); try { await onSubmit(a); } finally { setBusy(false); } }}>创建</button></div>
      </div>
    </Modal>
  );
}
