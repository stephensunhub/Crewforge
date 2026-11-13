import { useCallback, useEffect, useMemo, useState } from "react";
import { api } from "./api";
import type { EventRow, ProjectOverview, Schedule } from "./types";
import type { ProjectTab } from "./ProjectView";

const when = (ms: number | null) => (ms ? new Date(ms).toLocaleString("zh-CN", { month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit" }) : "—");
const RUN: Record<string, string> = { running: "进行中", paused: "已暂停", budget: "预算用尽", escalated: "等你决定", finished: "已完成", stopped: "已停止", failed: "出错" };

/** 公司总览：每个项目一张卡，哪些在跑、哪些要你关注、花了多少，不用逐个点进去。 */
export function OverviewView({ running, events, onOpenProject, onError }: {
  running: string[]; events: EventRow[]; onOpenProject: (id: string, tab?: ProjectTab) => void; onError: (m: string) => void;
}) {
  const [rows, setRows] = useState<ProjectOverview[] | null>(null);
  const [scheds, setScheds] = useState<Schedule[]>([]);
  const load = useCallback(async () => { try { setRows(await api.companyOverview()); setScheds(await api.listSchedules(null)); } catch (e) { onError(String(e)); } }, [onError]);
  useEffect(() => { void load(); }, [load]);
  // 有新的计划 / 自主推进 / 合并事件，或员工忙闲变化时刷新
  const last = useMemo(() => events.reduce((m, e) => (e.kind.startsWith("plan_") || e.kind.startsWith("autopilot_") || e.kind.startsWith("schedule_") || e.kind === "task_merged" || e.kind === "bug_reported" ? Math.max(m, e.id) : m), 0), [events]);
  useEffect(() => { if (last) void load(); }, [last, load]);
  useEffect(() => { void load(); }, [running.length, load]);

  if (!rows) return <div className="page"><div className="hint">加载中…</div></div>;
  const need = rows.filter((r) => r.attention.length > 0);
  const upcoming = scheds.filter((s) => s.enabled).sort((a, b) => a.next_run_at - b.next_run_at).slice(0, 6);
  const totalCost = rows.reduce((s, r) => s + r.cost_7d, 0);
  const totalCalls = rows.reduce((s, r) => s + r.calls_7d, 0);

  return (
    <div className="page">
      <div className="page-head">
        <div><h2>总览</h2><div className="hint">{rows.length} 个项目 · {need.length} 个需要关注 · 近 7 天 {totalCalls} 次调用{totalCost > 0 ? ` · $${totalCost.toFixed(2)}` : ""}（本地模型和订阅员工费用记 0 或未计）</div></div>
      </div>
      {rows.length === 0 && <div className="hint">还没有项目。</div>}
      <div className="cards">
        {rows.map((r) => (
          <div className="card clickable" key={r.project_id} onClick={() => onOpenProject(r.project_id, "plan")}>
            <div className="card-top"><b>{r.name}</b>
              <span className={`badge ${r.attention.length ? "ps-blocked" : "ps-done"}`}>{r.attention.length ? "需要关注" : "正常"}</span></div>
            <div className="ov-bar" title={`${r.percent_done}%`}><i style={{ width: `${r.percent_done}%` }} /></div>
            <div className="card-meta">
              <span className="chip static">{r.percent_done}% 完成</span>
              <span className="chip static">进行中 {r.in_progress}</span>
              <span className="chip static">待开始 {r.planned}</span>
              {r.bugs_open > 0 && <span className="chip static">bug {r.bugs_open}</span>}
              {r.open_issues > 0 && <span className="chip static">议题 {r.open_issues}</span>}
              {r.specs > 0 && <span className="chip static">规格 {r.specs}</span>}
              {r.schedules > 0 && <span className="chip static">定时 {r.schedules}</span>}
            </div>
            {r.doing.length > 0 && <div className="hint">正在做：{r.doing.slice(0, 2).join("；")}{r.doing.length > 2 ? "…" : ""}</div>}
            {r.attention.map((a) => <div key={a} className="ov-attn">⚠ {a}</div>)}
            <div className="hint">
              负责人：{r.lead_name ?? "人类（你）"} · 最近活动 {when(r.last_event_at)}
              {r.last_run ? ` · 最近一批：${RUN[r.last_run.status] ?? r.last_run.status}（${r.last_run.calls} 次调用）` : " · 还没有跑过自主推进"}
            </div>
            <div className="hint">近 7 天：{r.calls_7d} 次调用{r.cost_7d > 0 ? ` · $${r.cost_7d.toFixed(2)}` : ""}</div>
          </div>
        ))}
      </div>
      {upcoming.length > 0 && (
        <section className="maint-sec">
          <h3>即将触发的定时任务</h3>
          {upcoming.map((s) => (
            <div className="maint-row" key={s.id}>
              <span className="maint-title"><b>{s.project_name}</b> · {s.title}</span>
              <span className="hint">{when(s.next_run_at)}</span>
              <button className="btn" onClick={() => onOpenProject(s.project_id, "maint")}>去看看</button>
            </div>
          ))}
        </section>
      )}
    </div>
  );
}
