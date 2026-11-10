import { useCallback, useEffect, useMemo, useState } from "react";
import { api } from "./api";
import { ReasonButton, focusAction } from "./common";
import type { ActionItem, EventRow, Member, TaskView } from "./types";

const STATUS: Record<string, string> = { open: "待处理", working: "处理中", done: "已处理", dismissed: "已驳回" };
type StatusFilter = "live" | "done" | "dismissed" | "all";
type KindFilter = "all" | "human" | "blocking" | "later";

/** 项目里全部行动项的总览：不用一场一场点开会议去找。可以在这里处理，也可以跳到所在的会议。 */
export function ActionsView({ projectId, members, tasks, events, running, onOpenMeeting, onError, onChanged }: {
  projectId: string; members: Member[]; tasks: TaskView[]; events: EventRow[]; running: string[];
  onOpenMeeting: (meetingId: string) => void; onError: (m: string) => void; onChanged: () => void;
}) {
  const [items, setItems] = useState<ActionItem[]>([]);
  const [status, setStatus] = useState<StatusFilter>("live");
  const [kind, setKind] = useState<KindFilter>("all");
  const [pick, setPick] = useState<Record<string, string>>({});
  const [notice, setNotice] = useState<Record<string, { text: string; bad: boolean }>>({});
  const load = useCallback(async () => {
    try { setItems(await api.projectActions(projectId)); } catch (e) { onError(String(e)); }
  }, [projectId, onError]);
  const last = useMemo(() => events.reduce((m, e) => (["action_item_status", "meeting_minutes", "task_merged", "turn_result", "lead_decided_action", "lead_triaged_actions"].includes(e.kind) ? Math.max(m, e.id) : m), 0), [events]);
  useEffect(() => { void load(); }, [load, last]);

  const activeTasks = useMemo(() => new Map(tasks.filter((t) => t.task.status === "active").map((t) => [t.task.id, t.task.employee_id])), [tasks]);
  const isBlocking = (a: ActionItem) => a.blocking && !!a.task_id && activeTasks.has(a.task_id);
  const live = (a: ActionItem) => a.status === "open" || a.status === "working";
  const matchStatus = (a: ActionItem) => status === "all" || (status === "live" ? live(a) : a.status === status);
  const matchKind = (a: ActionItem) => kind === "all" || (kind === "human" ? a.for_human : kind === "blocking" ? isBlocking(a) : !a.for_human && !isBlocking(a));
  const shown = items.filter((a) => matchStatus(a) && matchKind(a));
  const liveItems = items.filter(live);
  const cnt = {
    live: liveItems.length, done: items.filter((a) => a.status === "done").length, dismissed: items.filter((a) => a.status === "dismissed").length, all: items.length,
    human: liveItems.filter((a) => a.for_human).length, blocking: liveItems.filter(isBlocking).length, later: liveItems.filter((a) => !a.for_human && !isBlocking(a)).length,
  };
  // 类型筛选只针对未处理的：切到已处理、已驳回、全部时清掉，免得看到空列表却不知道为什么
  const pickStatus = (v: StatusFilter) => { setStatus(v); if (v !== "live") setKind("all"); };
  const nameBusy = (employeeId: string) => { const n = members.find((x) => x.employee_id === employeeId)?.name; return n && running.includes(n) ? n : null; };

  async function act(fn: () => Promise<unknown>) { try { await fn(); await load(); onChanged(); } catch (e) { onError(String(e)); } }
  async function dispatch(id: string, employeeId: string) {
    const who = members.find((x) => x.employee_id === employeeId)?.name ?? "TA";
    setNotice((n) => ({ ...n, [id]: { text: "正在派活…", bad: false } }));
    try {
      await api.dispatchAction(id, employeeId);
      setNotice((n) => ({ ...n, [id]: { text: `已派给 ${who}。开始处理后这里会变成“处理中”；并发已满或额度紧张时会先排队。`, bad: false } }));
      await load(); onChanged();
    } catch (e) { setNotice((n) => ({ ...n, [id]: { text: String(e).replace(/^Error:\s*/, ""), bad: true } })); }
  }
  const open = (a: ActionItem) => { focusAction.id = a.id; onOpenMeeting(a.meeting_id); };

  const chip = <T extends string>(cur: T, set: (v: T) => void, v: T, label: string, n: number) => (
    <button key={v} className={`chip ${cur === v ? "on" : ""}`} onClick={() => set(v)}>{label}<span className="u-count">{n}</span></button>
  );

  return (
    <div className="page wide">
      <div className="page-head"><div><h2>行动项</h2>
        <div className="hint">评审和会议纪要里产生的待办，集中在这里看。<b>待决定</b>是等负责人拍板的事（负责人是 AI 时由它处理，只有它留给你的才会出现在你的待办里）；<b>拦合并</b>的不解决就不能合并；<b>之后再说</b>不拦合并，负责人会在收尾时整理。</div></div></div>
      <div className="row" style={{ gap: 6, flexWrap: "wrap", marginBottom: 6 }}>
        <span className="hint">状态</span>
        {chip(status, pickStatus, "live", "未处理", cnt.live)}{chip(status, pickStatus, "done", "已处理", cnt.done)}{chip(status, pickStatus, "dismissed", "已驳回", cnt.dismissed)}{chip(status, pickStatus, "all", "全部", cnt.all)}
      </div>
      <div className="row" style={{ gap: 6, flexWrap: "wrap", marginBottom: 12 }}>
        <span className="hint">类型（只统计未处理的）</span>
        {chip(kind, setKind, "all", "全部", cnt.live)}{chip(kind, setKind, "human", "待决定", cnt.human)}{chip(kind, setKind, "blocking", "拦合并", cnt.blocking)}{chip(kind, setKind, "later", "之后再说", cnt.later)}
      </div>
      {shown.length === 0 && <div className="hint" style={{ padding: "12px 0" }}>{items.length === 0 ? "还没有行动项。会议或评审的纪要里写了行动项就会出现在这里。" : "没有符合筛选条件的行动项。"}</div>}
      <div style={{ display: "grid", gap: 8 }}>
        {shown.map((a) => {
          const owner = a.task_id ? activeTasks.get(a.task_id) ?? null : null;
          const target = pick[a.id] ?? a.assignee_id ?? owner ?? "";
          const busy = target ? nameBusy(target) : null;
          return (
            <div className={`action ${a.status}`} key={a.id}>
              <div className="action-top">
                {a.for_human ? <span className="badge ab-block">待决定</span> : isBlocking(a) ? <span className="badge ab-block">拦合并</span> : <span className="badge">之后再说</span>}
                <span className={`badge as-${a.status}`}>{STATUS[a.status] ?? a.status}</span>
                <span className="hint">{a.assignee_name ? `负责人 ${a.assignee_name}` : a.for_human ? "需要你处理" : "未指定负责人"} · 来自《{a.meeting_title}》</span>
                <span style={{ flex: 1 }} />
                <button className="link" onClick={() => open(a)}>打开会议</button>
              </div>
              <div>{a.text}</div>
              {a.note && <div className="hint">备注：{a.note}</div>}
              {live(a) && (
                <div className="action-btns">
                  {!a.for_human && (
                    <>
                      <select value={target} onChange={(e) => setPick({ ...pick, [a.id]: e.target.value })}>
                        <option value="">选择负责人…</option>
                        {members.map((m) => <option key={m.employee_id} value={m.employee_id}>{m.name}</option>)}
                      </select>
                      <button className="btn" disabled={!target || busy !== null} title={busy ? `${busy} 正在执行任务，等他做完再派` : "让 TA 处理这一项。对进行中任务的评审意见，只能交给该任务的员工"} onClick={() => void dispatch(a.id, target)}>{a.status === "working" ? "再派一次" : "交给 TA 处理"}</button>
                    </>
                  )}
                  <button className="btn primary" onClick={() => void act(() => api.setActionStatus(a.id, "done"))}>已处理</button>
                  <ReasonButton label="驳回" required placeholder="为什么不处理这一项？（会记录下来）" onSubmit={(why) => void act(() => api.setActionStatus(a.id, "dismissed", why))} />
                </div>
              )}
              {live(a) && !a.for_human && busy && <div className="hint">{busy} 正在执行任务，做完后才能派给 TA（或换一个负责人）。</div>}
              {notice[a.id] && a.status !== "working" && live(a) && <div className={`hint ${notice[a.id].bad ? "bad" : ""}`}>{notice[a.id].text}</div>}
              {!live(a) && <button className="link" onClick={() => void act(() => api.setActionStatus(a.id, "open"))}>重新打开</button>}
            </div>
          );
        })}
      </div>
    </div>
  );
}
