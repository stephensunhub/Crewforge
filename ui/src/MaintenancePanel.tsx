import { useCallback, useEffect, useMemo, useState } from "react";
import { api } from "./api";
import { Field, Modal } from "./common";
import type { EventRow, PlanItem, Schedule, SpecView } from "./types";

const SEVERITY: [string, string][] = [["blocker", "阻断：线上不可用 / 数据错误，要马上修"], ["major", "严重：主要功能受影响"], ["minor", "一般：有影响但有绕过办法"]];
const STATUS: Record<string, string> = { proposed: "待确认", planned: "待开始", in_progress: "进行中", done: "已完成", blocked: "已阻塞", cancelled: "已取消" };
const STAGE: Record<string, [string, string]> = {
  writing: ["撰写中", "架构师（没有就由开发）正在写需求和设计文档"],
  to_split: ["待拆分", "文档已定稿，等负责人拆实现条目（开始自主推进后自动拆；没有 AI 负责人请手动在计划里加条目）"],
  building: ["实现中", "实现条目正在推进"],
  delivered: ["已交付", "规格对应的实现条目都完成了；需求变了就「修订规格」"],
};
const WEEKDAYS = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"];
const KIND_LABEL: Record<string, string> = { other: "其他", characterize: "补测试", cleanup: "清理", migrate: "实现" };

const when = (ms: number | null) => (ms ? new Date(ms).toLocaleString("zh-CN", { month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit" }) : "—");
const cadenceText = (s: Pick<Schedule, "cadence" | "weekday" | "hour">) => `${s.cadence === "daily" ? "每天" : `每${WEEKDAYS[s.weekday]}`} ${String(s.hour).padStart(2, "0")}:00`;

/** 项目的“维护”页：上线之后的 bug、需求迭代（功能规格）、周期性检查。 */
export function MaintenancePanel({ projectId, events, onGoTab, onError, onChanged }: {
  projectId: string; events: EventRow[]; onGoTab: (t: "plan" | "docs") => void; onError: (m: string) => void; onChanged: () => void;
}) {
  const [bugs, setBugs] = useState<PlanItem[]>([]);
  const [specs, setSpecs] = useState<SpecView[]>([]);
  const [scheds, setScheds] = useState<Schedule[]>([]);
  const [modal, setModal] = useState<"bug" | "spec" | "sched" | { revise: SpecView } | null>(null);
  const load = useCallback(async () => {
    try {
      const v = await api.planView(projectId);
      setBugs(v.items.filter((i) => i.kind === "bugfix").reverse());
      setSpecs(await api.listSpecs(projectId));
      setScheds(await api.listSchedules(projectId));
    } catch (e) { onError(String(e)); }
  }, [projectId, onError]);
  useEffect(() => { void load(); }, [load]);
  const last = useMemo(() => events.reduce((m, e) => (e.project_id === projectId && (e.kind.startsWith("plan_") || e.kind.startsWith("autopilot_") || e.kind.startsWith("spec_") || e.kind.startsWith("schedule_") || e.kind === "bug_reported" || e.kind === "task_merged") ? Math.max(m, e.id) : m), 0), [events, projectId]);
  useEffect(() => { if (last) void load(); }, [last, load]);
  const act = async (f: () => Promise<unknown>) => { try { await f(); await load(); onChanged(); return true; } catch (e) { onError(String(e)); return false; } };
  const openBugs = bugs.filter((b) => !["done", "cancelled"].includes(b.status));

  return (
    <div className="page">
      <div className="page-head">
        <div><h2>维护</h2><div className="hint">项目上线之后的事：bug 修复、需求迭代、周期性检查。这里建的内容都会变成「计划」里的条目，由自主推进来做。</div></div>
      </div>

      <section className="maint-sec">
        <div className="maint-head"><h3>Bug</h3><button className="btn primary" onClick={() => setModal("bug")}>＋ 报告 bug</button></div>
        <div className="hint">bug 条目不受设计阶段限制、排在其他条目前面；要求先复现、找根因、最小修复、补回归测试。改动里没有测试文件的会被强制评审。</div>
        {bugs.length === 0 && <div className="hint maint-empty">还没有报告过 bug。</div>}
        {bugs.slice(0, 12).map((b) => (
          <div className="maint-row" key={b.id}>
            <span className={`badge ps-${b.status}`}>{STATUS[b.status]}</span>
            <span className="maint-title" title={b.description}>{b.title}</span>
            <span className="hint">{b.assignee_name ? `由 ${b.assignee_name} 修` : "未指派"}</span>
          </div>
        ))}
        {openBugs.length > 0 && <div className="hint">有 {openBugs.length} 个未完成。<a className="link" onClick={() => onGoTab("plan")}>去计划页看详情 →</a></div>}
      </section>

      <section className="maint-sec">
        <div className="maint-head"><h3>功能规格</h3><button className="btn primary" onClick={() => setModal("spec")}>＋ 新建功能规格</button></div>
        <div className="hint">每个功能一份活文档：<code>docs/specs/&lt;名&gt;/requirements.md</code>（需求与可检查的验收标准）和 <code>design.md</code>（方案）。需求变了不要重讲一遍，点「修订规格」——在原文档上改并追加变更记录，再按变化拆出新的实现条目。实现条目的任务会指向规格，验收以规格为准。</div>
        {specs.length === 0 && <div className="hint maint-empty">还没有功能规格。</div>}
        {specs.map((v) => (
          <div className="card maint-card" key={v.spec.id}>
            <div className="maint-head">
              <div style={{ minWidth: 0 }}><b>{v.spec.title}</b> <span className="hint">docs/specs/{v.spec.slug}/ · 第 {v.spec.rounds} 版</span></div>
              <span className="chip static" title={STAGE[v.stage][1]}>{STAGE[v.stage][0]}</span>
            </div>
            <div className="hint">{v.spec.brief}</div>
            <div className="maint-items">
              {v.items.map((i) => <span key={i.id} className={`badge ps-${i.status}`} title={i.title}>{i.kind === "spec" ? "📄 " : ""}{i.title.length > 22 ? `${i.title.slice(0, 22)}…` : i.title} · {STATUS[i.status]}</span>)}
            </div>
            <div className="maint-btns">
              <button className="btn" onClick={() => onGoTab("docs")}>在「文档」里看</button>
              <button className="btn" disabled={v.stage === "writing"} title={v.stage === "writing" ? "文档还在撰写中" : ""} onClick={() => setModal({ revise: v })}>修订规格（需求变了）</button>
            </div>
          </div>
        ))}
      </section>

      <section className="maint-sec">
        <div className="maint-head"><h3>定时任务</h3><button className="btn primary" onClick={() => setModal("sched")}>＋ 新建定时任务</button></div>
        <div className="hint">到点就在计划里建一个条目（上一次的还没做完就不再重复建）。条目要等自主推进来做——它不会自己开始花额度。</div>
        {scheds.length === 0 && <div className="hint maint-empty">还没有定时任务。常见的：每周依赖与安全检查、每周测试健康检查。</div>}
        {scheds.map((s) => (
          <div className="maint-row" key={s.id}>
            <label className="tog" title={s.enabled ? "点击停用" : "点击启用"}><input type="checkbox" checked={s.enabled} onChange={(e) => void act(() => api.setScheduleEnabled(s.id, e.target.checked))} /></label>
            <span className="maint-title" title={s.description}>{s.title}</span>
            <span className="hint">{cadenceText(s)} · 下次 {s.enabled ? when(s.next_run_at) : "已停用"}{s.last_run_at ? ` · 上次 ${when(s.last_run_at)}${s.last_item_status ? `（${STATUS[s.last_item_status] ?? s.last_item_status}）` : ""}` : ""}</span>
            <button className="btn" onClick={() => void act(() => api.deleteSchedule(s.id))}>删除</button>
          </div>
        ))}
      </section>

      {modal === "bug" && <BugForm onClose={() => setModal(null)} onSubmit={async (b) => { if (await act(() => api.reportBug(projectId, b))) setModal(null); }} />}
      {modal === "spec" && <SpecForm onClose={() => setModal(null)} onSubmit={async (t, b) => { if (await act(() => api.createSpec(projectId, t, b))) setModal(null); }} />}
      {modal === "sched" && <ScheduleForm onClose={() => setModal(null)} onSubmit={async (a) => { if (await act(() => api.addSchedule(projectId, a.title, a.description, a.kind, a.cadence, a.weekday, a.hour))) setModal(null); }} />}
      {modal && typeof modal === "object" && "revise" in modal && (
        <ReviseForm spec={modal.revise} onClose={() => setModal(null)} onSubmit={async (c) => { if (await act(() => api.reviseSpec(modal.revise.spec.id, c))) setModal(null); }} />
      )}
    </div>
  );
}

function BugForm({ onClose, onSubmit }: { onClose: () => void; onSubmit: (b: { title: string; steps: string; expected: string; actual: string; severity: string; extra: string }) => Promise<void> }) {
  const [b, setB] = useState({ title: "", steps: "", expected: "", actual: "", severity: "major", extra: "" });
  const [busy, setBusy] = useState(false);
  const set = (k: keyof typeof b) => (e: { target: { value: string } }) => setB((x) => ({ ...x, [k]: e.target.value }));
  return (
    <Modal title="报告 bug" onClose={onClose} width={620}>
      <div className="form">
        <Field label="一句话说明问题"><input value={b.title} onChange={set("title")} placeholder="例如：导出报表时遇到空列表会闪退" /></Field>
        <Field label="严重程度"><select value={b.severity} onChange={set("severity")}>{SEVERITY.map(([k, v]) => <option key={k} value={k}>{v}</option>)}</select></Field>
        <Field label="复现步骤" hint="越具体越好：点哪里、输入什么。不知道怎么复现就写下你看到的现象。"><textarea rows={3} value={b.steps} onChange={set("steps")} /></Field>
        <Field label="期望的行为"><textarea rows={2} value={b.expected} onChange={set("expected")} /></Field>
        <Field label="实际的行为"><textarea rows={2} value={b.actual} onChange={set("actual")} /></Field>
        <Field label="补充信息（可选）" hint="版本、环境、报错日志。"><textarea rows={3} value={b.extra} onChange={set("extra")} /></Field>
        <div className="form-btns"><button className="btn primary" disabled={busy || !b.title.trim()} onClick={async () => { setBusy(true); try { await onSubmit(b); } finally { setBusy(false); } }}>提交</button></div>
      </div>
    </Modal>
  );
}

function SpecForm({ onClose, onSubmit }: { onClose: () => void; onSubmit: (title: string, brief: string) => Promise<void> }) {
  const [title, setTitle] = useState("");
  const [brief, setBrief] = useState("");
  const [busy, setBusy] = useState(false);
  return (
    <Modal title="新建功能规格" onClose={onClose} width={620}>
      <div className="form">
        <Field label="功能名称" hint="用英文 / 拼音能得到更好读的目录名（docs/specs/<名>/）；纯中文会按编号命名。"><input value={title} onChange={(e) => setTitle(e.target.value)} placeholder="例如：导出 CSV" /></Field>
        <Field label="这个功能要解决什么问题" hint="写需求和背景就行，不用写方案。会先由架构师写成带验收标准的需求文档，再写设计，最后由负责人拆成实现条目。"><textarea rows={5} value={brief} onChange={(e) => setBrief(e.target.value)} /></Field>
        <div className="form-btns"><button className="btn primary" disabled={busy || !title.trim() || !brief.trim()} onClick={async () => { setBusy(true); try { await onSubmit(title, brief); } finally { setBusy(false); } }}>创建</button></div>
      </div>
    </Modal>
  );
}

function ReviseForm({ spec, onClose, onSubmit }: { spec: SpecView; onClose: () => void; onSubmit: (change: string) => Promise<void> }) {
  const [change, setChange] = useState("");
  const [busy, setBusy] = useState(false);
  return (
    <Modal title={`修订规格：${spec.spec.title}`} onClose={onClose} width={620}>
      <div className="form">
        <Field label="需求有什么变化" hint="写变化的部分就行。会在现有的 requirements.md / design.md 上修订并追加变更记录，然后由负责人只为变化的部分拆新条目。"><textarea rows={5} value={change} onChange={(e) => setChange(e.target.value)} /></Field>
        <div className="form-btns"><button className="btn primary" disabled={busy || !change.trim()} onClick={async () => { setBusy(true); try { await onSubmit(change); } finally { setBusy(false); } }}>开始修订</button></div>
      </div>
    </Modal>
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
        {presets.length > 0 && (
          <Field label="从模板开始">
            <div className="maint-btns">{presets.map(([t, k, d, c]) => <button key={t} className="chip" onClick={() => setA((x) => ({ ...x, title: t, kind: k, description: d, cadence: c }))}>{t}</button>)}</div>
          </Field>
        )}
        <Field label="名称"><input value={a.title} onChange={(e) => setA({ ...a, title: e.target.value })} /></Field>
        <Field label="每次要做什么" hint="写给员工的任务说明。"><textarea rows={5} value={a.description} onChange={(e) => setA({ ...a, description: e.target.value })} /></Field>
        <div className="maint-btns">
          <Field label="周期"><select value={a.cadence} onChange={(e) => setA({ ...a, cadence: e.target.value })}><option value="daily">每天</option><option value="weekly">每周</option></select></Field>
          {a.cadence === "weekly" && <Field label="星期"><select value={a.weekday} onChange={(e) => setA({ ...a, weekday: Number(e.target.value) })}>{WEEKDAYS.map((w, i) => <option key={w} value={i}>{w}</option>)}</select></Field>}
          <Field label="几点（本机时间）"><select value={a.hour} onChange={(e) => setA({ ...a, hour: Number(e.target.value) })}>{Array.from({ length: 24 }, (_, h) => <option key={h} value={h}>{String(h).padStart(2, "0")}:00</option>)}</select></Field>
          <Field label="条目类型"><select value={a.kind} onChange={(e) => setA({ ...a, kind: e.target.value })}>{Object.entries(KIND_LABEL).map(([k, v]) => <option key={k} value={k}>{v}</option>)}</select></Field>
        </div>
        <div className="hint">应用没开着时错过的时间点不会补建；下次打开后只按下一个时间点算。</div>
        <div className="form-btns"><button className="btn primary" disabled={busy || !a.title.trim() || !a.description.trim()} onClick={async () => { setBusy(true); try { await onSubmit(a); } finally { setBusy(false); } }}>创建</button></div>
      </div>
    </Modal>
  );
}
