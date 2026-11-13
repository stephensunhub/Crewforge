import { useCallback, useEffect, useMemo, useState } from "react";
import { api } from "./api";
import type { EventRow, Member, TimelineRow, TimelineView as TL } from "./types";

const KIND: Record<string, string> = { design: "架构", spec: "规格文档", migrate: "开发", cleanup: "清理", characterize: "测试", bugfix: "修复", retest: "复测", other: "其他" };
const STATUS: Record<string, string> = { proposed: "待确认", planned: "待开始", in_progress: "进行中", done: "已完成", blocked: "已阻塞", cancelled: "已取消" };
const ROLE: Record<string, string> = { lead: "负责人", developer: "开发", qa: "QA", reviewer: "评审", architect: "架构师" };
const clock = (ms: number | null) => (ms ? new Date(ms).toLocaleString("zh-CN", { month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit" }) : "—");

/** 排期表：全部条目按先后顺序排成一张表；分「已排期」和「未排期」。顺序由负责人排（有未排期的、有人空闲时自动排），你也可以调整。 */
export function TimelineView({ projectId, members, events, onGoTab, onError, onChanged }: {
  projectId: string; members: Member[]; events: EventRow[]; onGoTab: (t: "bugs" | "requirements" | "plan") => void; onError: (m: string) => void; onChanged: () => void;
}) {
  const [tl, setTl] = useState<TL | null>(null);
  const [waiting, setWaiting] = useState<Record<string, string[]>>({});
  const [show, setShow] = useState<"todo" | "all" | "done">("todo");
  const [role, setRole] = useState("");
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState(false);
  const load = useCallback(async () => { try { setTl(await api.timelineView(projectId)); setWaiting(await api.planWaiting(projectId)); } catch (e) { onError(String(e)); } }, [projectId, onError]);
  useEffect(() => { void load(); }, [load]);
  const last = useMemo(() => events.reduce((m, e) => (e.project_id === projectId && (e.kind.startsWith("plan_") || e.kind.startsWith("autopilot_") || e.kind.startsWith("bug_") || e.kind.startsWith("requirement_") || e.kind === "task_merged" || e.kind === "schedule_fired") ? Math.max(m, e.id) : m), 0), [events, projectId]);
  useEffect(() => { if (last) void load(); }, [last, load]);
  const act = async (f: () => Promise<unknown>) => { try { await f(); await load(); onChanged(); } catch (e) { onError(String(e)); } };
  const roleOf = (id: string | null) => members.find((m) => m.employee_id === id)?.role ?? "";
  if (!tl) return <div className="page"><div className="hint">加载中…</div></div>;

  const rows = tl.scheduled.map((r, i) => ({ r, seq: i + 1 }));
  const passes = ({ r }: { r: TimelineRow }) => (show === "all" || (show === "done" ? r.item.status === "done" : r.item.status !== "done")) && (!role || roleOf(r.item.assignee_id) === role);
  const shown = rows.filter(passes);
  const hidden = rows.length - shown.length;
  const flagged = rows.filter(({ r }) => r.deps_below.length > 0 && r.item.status !== "done").length;
  const movable = rows.filter(({ r }) => r.item.status === "planned");
  const swap = (id: string, dir: -1 | 1) => {
    const i = movable.findIndex(({ r }) => r.item.id === id); const j = i + dir;
    if (i < 0 || j < 0 || j >= movable.length) return;
    void act(() => api.swapPlanPositions(id, movable[j].r.item.id));
  };
  const need = tl.unscheduled.length + tl.pending_requirements;

  const cell = (r: TimelineRow) => {
    const w = waiting[r.item.id] ?? [];
    const st = r.item.status === "done" ? `完成 ${clock(r.finished_at)}` : STATUS[r.item.status];
    return { w, st };
  };
  return (
    <div className="page">
      <div className="page-head">
        <div><h2>排期</h2>
          <div className="hint">全部条目按先后顺序排成一张表：谁在什么时候因为什么被排进来、前置是什么、有没有阻碍。顺序由负责人排；<b>有未排期的条目、且有人空闲时自动排一次</b>（一次处理全部），没有 AI 负责人按规则排（P0 bug 在最前）。自主推进只会派「已排期」的条目。</div></div>
        <button className="btn primary" disabled={busy || need === 0} title={need === 0 ? "没有未排期的条目，也没有待评估的需求" : "现在就让负责人排一次（会调用模型）"}
          onClick={async () => { setBusy(true); setNote(""); try { setNote(await api.leadScheduleNow(projectId)); await load(); onChanged(); } catch (e) { onError(String(e)); } finally { setBusy(false); } }}>
          {busy ? "排期中…" : `请负责人排期（${need}）`}</button>
      </div>
      {note && <div className="hint" style={{ marginBottom: 8 }}>{note}</div>}

      <section className="maint-sec">
        <div className="maint-head"><h3>已排期</h3>
          <div className="maint-btns">
            {([["todo", "未完成"], ["all", "全部"], ["done", "已完成"]] as ["todo" | "all" | "done", string][]).map(([k, v]) => <button key={k} className={`chip ${show === k ? "on" : ""}`} onClick={() => setShow(k)}>{v}</button>)}
            <select value={role} onChange={(e) => setRole(e.target.value)}><option value="">所有角色</option>{Object.entries(ROLE).map(([k, v]) => <option key={k} value={k}>{v}</option>)}</select>
          </div></div>
        {shown.length === 0 && <div className="hint maint-empty">没有符合条件的条目。</div>}
        {(hidden > 0 || flagged > 0) && <div className="hint" style={{ marginBottom: 4 }}>{show === "todo" && hidden > 0 ? `已隐藏 ${hidden} 个已完成的条目（序号按当前显示的行重新数）。` : hidden > 0 ? `按筛选隐藏了 ${hidden} 个条目。` : ""}{flagged > 0 ? ` ⚠ 有 ${flagged} 行的前置排在它下面，下次负责人排期时会自动纠正。` : ""}</div>}
        <table className="tbl">
          <thead><tr><th>#</th><th>排入时间</th><th>事项</th><th>类型</th><th>负责人</th><th>前置</th><th>状态</th><th>阻碍</th><th>触发 / 原因</th><th></th></tr></thead>
          <tbody>
            {shown.map(({ r }, idx) => { const seq = idx + 1; const { w, st } = cell(r); return (
              <tr key={r.item.id} className={r.item.status === "done" ? "tbl-done" : ""}>
                <td>{seq}</td>
                <td className="hint">{clock(r.scheduled_at)}</td>
                <td className="tbl-title" title={r.item.description}>
                  {r.bug_label && <span className={`badge bp-${r.bug_priority}`}>{r.bug_label} · P{r.bug_priority}</span>} {r.item.title}
                  {r.requirement_title && <span className="hint">（需求：{r.requirement_title}）</span>}</td>
                <td>{KIND[r.item.kind] ?? r.item.kind}</td>
                <td>{r.item.assignee_name ?? "—"}<span className="hint">{roleOf(r.item.assignee_id) ? `（${ROLE[roleOf(r.item.assignee_id)] ?? ""}）` : ""}</span></td>
                <td className="hint">{r.item.deps.length === 0 ? "—" : r.item.waiting_for.length ? `等：${r.item.waiting_for.join("、")}` : "已满足"}
                  {r.deps_below.length > 0 && <div className="tl-block" title="前置条目排在这一行的下面：自主推进不受影响（前置没做完就不会开始），但表的顺序不对。下次负责人排期时会自动纠正，也可以把前置往上移。">⚠ 前置排在下面：{r.deps_below.join("、")}</div>}</td>
                <td><span className={`badge ps-${r.item.status}`}>{st}</span></td>
                <td className={w.length ? "tl-block" : "hint"}>{r.item.status === "done" ? "—" : w.length ? w.join("；") : "无"}</td>
                <td className="hint" title={r.reason ?? ""}>{r.trigger ?? "原有条目"}{r.reason ? `：${r.reason.slice(0, 30)}` : ""}</td>
                <td>{r.item.status === "planned" && (
                  <div className="maint-btns"><button className="btn" onClick={() => swap(r.item.id, -1)} title="提前一位">↑</button><button className="btn" onClick={() => swap(r.item.id, 1)} title="推后一位">↓</button>
                    <button className="btn" onClick={() => void act(() => api.unscheduleItem(r.item.id))}>退回未排期</button></div>)}</td>
              </tr>); })}
          </tbody>
        </table>
      </section>

      <section className="maint-sec">
        <div className="maint-head"><h3>未排期（{tl.unscheduled.length}）</h3>
          <div className="hint">{tl.pending_requirements > 0 && <>另有 {tl.pending_requirements} 个待评估的需求 <a className="link" onClick={() => onGoTab("requirements")}>去需求池 →</a></>}</div></div>
        {tl.unscheduled.length === 0 && <div className="hint maint-empty">没有未排期的条目。bug 的修复和复测、定时检查、评审里“之后再说”的事，都先进这里，等负责人排期。</div>}
        <table className="tbl">
          <thead><tr><th>事项</th><th>类型</th><th>来源</th><th>负责人</th><th>说明</th><th></th></tr></thead>
          <tbody>
            {tl.unscheduled.map((r) => (
              <tr key={r.item.id}>
                <td className="tbl-title">{r.bug_label && <span className={`badge bp-${r.bug_priority}`}>{r.bug_label} · P{r.bug_priority}</span>} {r.item.title}</td>
                <td>{KIND[r.item.kind] ?? r.item.kind}</td>
                <td className="hint">{({ bug: "Bug", schedule: "定时器", review: "评审意见", requirement: "需求" } as Record<string, string>)[r.item.source] ?? "计划"}</td>
                <td>{r.item.assignee_name ?? "—"}</td>
                <td className="hint tbl-title" title={r.item.description}>{r.item.note || r.item.description.slice(0, 60)}</td>
                <td><div className="maint-btns"><button className="btn" onClick={() => void act(() => api.scheduleItemByHuman(r.item.id))}>排入（追加到最后）</button>
                  <button className="btn" onClick={() => void act(() => api.setPlanStatus(r.item.id, "cancelled", "未排期时取消"))}>取消</button></div></td>
              </tr>))}
          </tbody>
        </table>
        <div className="hint">还可以在 <a className="link" onClick={() => onGoTab("bugs")}>Bug</a> 栏目看 bug 的修复和复测进度，在 <a className="link" onClick={() => onGoTab("plan")}>计划</a> 页编辑条目的说明和依赖。</div>
      </section>
    </div>
  );
}
