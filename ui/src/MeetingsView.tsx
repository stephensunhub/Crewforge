import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api } from "./api";
import { Avatar, Field, ISSUE_STATUS_LABEL, MEETING_KIND_LABEL, Modal, hhmm } from "./common";
import type { EventRow, IssueView, Meeting, MeetingDetail, MeetingKind, Member, TaskView } from "./types";

export function MeetingsView({ projectId, meetings, members, issues, tasks, events, running, focusId, onFocused, onChanged, onError }: {
  projectId: string;
  meetings: Meeting[];
  members: Member[];
  issues: IssueView[];
  tasks: TaskView[];
  events: EventRow[];
  running: string[];
  focusId: string | null;
  onFocused: () => void;
  onChanged: () => void;
  onError: (m: string) => void;
}) {
  const [selected, setSelected] = useState<string | null>(null);
  const [creating, setCreating] = useState<{ issueId?: string } | null>(null);

  useEffect(() => { if (focusId) { setSelected(focusId); onFocused(); } }, [focusId, onFocused]);
  useEffect(() => { if (!selected && meetings[0]) setSelected(meetings[0].id); }, [meetings, selected]);

  return (
    <div className="split">
      <aside className="split-side">
        <div className="split-head">
          <b>会议</b>
          <button className="btn primary" onClick={() => setCreating({})}>＋ 开会</button>
        </div>
        <div className="split-list">
          {meetings.length === 0 && <div className="hint" style={{ padding: 12 }}>还没有会议。需求评审、方案评审、Code Review 都可以在这里开。</div>}
          {meetings.map((m) => (
            <button key={m.id} className={`mitem ${selected === m.id ? "on" : ""}`} onClick={() => setSelected(m.id)}>
              <div className="mitem-title">{m.title}</div>
              <div className="hint">{MEETING_KIND_LABEL[m.kind]} · {m.status === "open" ? "进行中" : "已结束"}</div>
            </button>
          ))}
        </div>
      </aside>
      <div className="split-main">
        {selected
          ? <MeetingRoom key={selected} id={selected} members={members} issues={issues} events={events} running={running} onChanged={onChanged} onError={onError} />
          : <div className="empty"><img src="/logo.png" alt="" /><h2>会议室</h2><p>像真实公司一样：有问题就开会，大家一起听、一起讨论。没被 @ 的成员只旁听，不消耗额度。</p></div>}
      </div>
      {creating && (
        <NewMeeting members={members} issues={issues} tasks={tasks} preIssue={creating.issueId} onClose={() => setCreating(null)}
          onCreate={async (a) => {
            try { const m = await api.createMeeting({ projectId, ...a }); onChanged(); setSelected(m.id); setCreating(null); } catch (e) { onError(String(e)); }
          }} />
      )}
    </div>
  );
}

function NewMeeting({ members, issues, tasks, preIssue, onCreate, onClose }: {
  members: Member[]; issues: IssueView[]; tasks: TaskView[]; preIssue?: string; onClose: () => void;
  onCreate: (a: { kind: MeetingKind; title: string; hostId: string | null; taskId: string | null; issueIds: string[]; maxAgentReplies: number }) => Promise<void>;
}) {
  const pre = issues.find((i) => i.issue.id === preIssue);
  const [kind, setKind] = useState<MeetingKind>("design");
  const [title, setTitle] = useState(pre ? `讨论：${pre.issue.title}` : "");
  const [host, setHost] = useState("");
  const [task, setTask] = useState("");
  const [picked, setPicked] = useState<string[]>(preIssue ? [preIssue] : []);
  const [max, setMax] = useState(12);
  const openIssues = issues.filter((i) => ["open", "discussing", "pending_decision"].includes(i.issue.status));
  return (
    <Modal title="开会" onClose={onClose} width={560}>
      <div className="form">
        <Field label="会议类型">
          <select value={kind} onChange={(e) => setKind(e.target.value as MeetingKind)}>
            {Object.entries(MEETING_KIND_LABEL).map(([k, l]) => <option key={k} value={k}>{l}</option>)}
          </select>
        </Field>
        <Field label="主题"><input value={title} autoFocus onChange={(e) => setTitle(e.target.value)} /></Field>
        {kind === "code_review" && (
          <Field label="评审哪个任务" hint="任务的改动会随提示发给被 @ 的评审人。评审人会先独立判断，再看他人意见。">
            <select value={task} onChange={(e) => { setTask(e.target.value); const t = tasks.find((x) => x.task.id === e.target.value); if (t && !title) setTitle(`评审：${t.task.title}`); }}>
              <option value="">不关联任务</option>
              {tasks.map((t) => <option key={t.task.id} value={t.task.id}>{t.employee_name} · {t.task.title}（{t.task.status === "active" ? "进行中" : t.task.status === "merged" ? "已合并" : "已放弃"}）</option>)}
            </select>
          </Field>
        )}
        <Field label="主持人" hint="负责起草纪要。选“人类”时，纪要由项目负责人或第一位成员起草，你来确认。">
          <select value={host} onChange={(e) => setHost(e.target.value)}>
            <option value="">人类（你）</option>
            {members.map((m) => <option key={m.employee_id} value={m.employee_id}>{m.name}</option>)}
          </select>
        </Field>
        {openIssues.length > 0 && (
          <Field label="讨论哪些议题（可多选）">
            {openIssues.map((i) => (
              <label className="tog" key={i.issue.id}>
                <input type="checkbox" checked={picked.includes(i.issue.id)}
                  onChange={(e) => setPicked(e.target.checked ? [...picked, i.issue.id] : picked.filter((x) => x !== i.issue.id))} />
                {i.issue.title}<span className="hint">（{ISSUE_STATUS_LABEL[i.issue.status]}）</span>
              </label>
            ))}
          </Field>
        )}
        <Field label="AI 发言上限" hint="防止无限讨论。达到上限后 AI 不再发言，由你或负责人裁决。">
          <input type="number" min={1} max={200} style={{ width: 90 }} value={max} onChange={(e) => setMax(Math.max(1, Math.min(200, Number(e.target.value) || 1)))} />
        </Field>
        <div className="form-btns">
          <button className="btn primary" disabled={!title.trim()} onClick={() => void onCreate({ kind, title, hostId: host || null, taskId: kind === "code_review" ? task || null : null, issueIds: picked, maxAgentReplies: max })}>开始</button>
        </div>
      </div>
    </Modal>
  );
}

function MeetingRoom({ id, members, issues, events, running, onChanged, onError }: {
  id: string; members: Member[]; issues: IssueView[]; events: EventRow[]; running: string[];
  onChanged: () => void; onError: (m: string) => void;
}) {
  const [d, setD] = useState<MeetingDetail | null>(null);
  const [text, setText] = useState("");
  const [minutes, setMinutes] = useState("");
  const [dirty, setDirty] = useState(false);
  const [waiting, setWaiting] = useState<string[]>([]);
  const feed = useRef<HTMLDivElement>(null);
  const stick = useRef(true);

  const load = useCallback(async () => {
    try { setD(await api.meetingDetail(id)); } catch (e) { onError(String(e)); }
  }, [id, onError]);
  useEffect(() => { void load(); }, [load]);

  // 这场会议的新事件（有人发言、纪要写好、会议结束）到来时刷新
  const last = useMemo(() => events.reduce((m, e) => (["meeting_message", "meeting_minutes", "meeting_closed"].includes(e.kind) && e.payload?.meeting_id === id ? Math.max(m, e.id) : m), 0), [events, id]);
  useEffect(() => { if (last) void load(); }, [last, load]);
  useEffect(() => { if (d && !dirty) setMinutes(d.meeting.minutes); }, [d, dirty]);
  useEffect(() => { if (stick.current) feed.current?.scrollTo({ top: feed.current.scrollHeight }); }, [d?.messages.length]);
  // 被唤醒的成员回复完（不再运行）后，取消"正在回复"提示
  useEffect(() => { setWaiting((w) => w.filter((n) => running.includes(n))); }, [running]);

  if (!d) return <div className="hint" style={{ padding: 20 }}>加载中…</div>;
  const m = d.meeting;
  const nameOf = (eid: string | null) => members.find((x) => x.employee_id === eid)?.name ?? "未知";
  const open = m.status === "open";
  const names = members.map((x) => x.name);
  const mention = /@([^\s@]*)$/.exec(text);
  const sugg = mention ? names.filter((n) => n.toLowerCase().startsWith(mention[1].toLowerCase())).slice(0, 8) : names.slice(0, 8);
  const complete = (n: string) => setText((t) => (mention ? t.replace(/@([^\s@]*)$/, `@${n} `) : `${t}${t && !t.endsWith(" ") ? " " : ""}@${n} `));
  const linked = issues.filter((i) => d.issue_ids.includes(i.issue.id));
  const capped = d.agent_replies >= m.max_agent_replies;

  async function send() {
    const body = text.trim();
    if (!body) return;
    setText("");
    try { setWaiting(await api.postMeetingMessage(id, body)); await load(); } catch (e) { onError(String(e)); }
  }

  return (
    <div className="col">
      <div className="room-head">
        <div style={{ minWidth: 0 }}>
          <b>{m.title}</b>
          <div className="hint">{MEETING_KIND_LABEL[m.kind]} · {open ? "进行中" : "已结束"} · AI 发言 {d.agent_replies}/{m.max_agent_replies}
            {m.host_id ? ` · 主持：${nameOf(m.host_id)}` : " · 主持：人类"}</div>
          {linked.length > 0 && <div className="card-meta" style={{ marginTop: 4 }}>{linked.map((i) => <span key={i.issue.id} className="chip static">议题：{i.issue.title}（{ISSUE_STATUS_LABEL[i.issue.status]}）</span>)}</div>}
        </div>
        {open && (
          <div style={{ display: "flex", gap: 8 }}>
            <button className="btn" onClick={async () => {
              if (d.messages.length === 0) { onError("会议里还没有发言"); return; }
              try { await api.draftMinutes(id); setDirty(false); setWaiting([]); } catch (e) { onError(String(e)); }
            }}>让主持人起草纪要</button>
            <button className="btn danger" onClick={async () => {
              if (await api.confirm("结束会议？结束后不能再发言，纪要和记录会保留。", "结束会议")) {
                try { await api.closeMeeting(id); onChanged(); await load(); } catch (e) { onError(String(e)); }
              }
            }}>结束会议</button>
          </div>
        )}
      </div>

      <div className="feed" ref={feed} onScroll={(e) => { const el = e.currentTarget; stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80; }}>
        {d.messages.length === 0 && <div className="empty"><p>会议开始了。用 <code>@{names[0] ?? "成员"}</code> 点名发言；没被点名的成员只旁听，不会调用模型。</p></div>}
        {d.messages.map((x) => {
          if (x.author_kind === "system") return <div className="line warn" key={x.id} style={{ paddingLeft: 0 }}><span>ⓘ {x.content}</span><span className="time">{hhmm(x.created_at)}</span></div>;
          if (x.author_kind === "human")
            return <div className="msg me" key={x.id}><div><div className="who" style={{ textAlign: "right" }}>你 · {hhmm(x.created_at)}</div><div className="bubble">{x.content}</div></div></div>;
          const n = nameOf(x.employee_id);
          return <div className="msg" key={x.id}><Avatar name={n} /><div><div className="who">{n} · {hhmm(x.created_at)}</div><div className="bubble">{x.content}</div></div></div>;
        })}
        {waiting.filter((n) => running.includes(n)).map((n) => <div className="line" key={n} style={{ paddingLeft: 0 }}><span>● {n} 正在回复…</span></div>)}
      </div>

      <div className="minutes">
        <div className="minutes-head"><b>会议纪要</b>
          <span className="hint">决议、被否方案及原因、行动项、待验证经验。由 AI 起草，你可以修改。{dirty ? " · 有未保存的修改" : ""}</span>
          <span style={{ flex: 1 }} />
          <button className="btn" disabled={!dirty} onClick={async () => { try { await api.saveMinutes(id, minutes); setDirty(false); await load(); } catch (e) { onError(String(e)); } }}>保存纪要</button>
        </div>
        <textarea rows={minutes ? 7 : 2} value={minutes} placeholder="还没有纪要。可以让主持人起草，也可以直接手写。" onChange={(e) => { setMinutes(e.target.value); setDirty(true); }} />
      </div>

      {open ? (
        <footer className="composer">
          <div className="chips">
            <span className="hint">{mention ? "选择成员（Tab 补全）" : "@ 点名发言，未被 @ 的成员只旁听"}</span>
            {sugg.map((n) => <button key={n} className="chip" onClick={() => complete(n)}>{n}{running.includes(n) ? " ●" : ""}</button>)}
            {capped && <span className="hint" style={{ color: "var(--warn)" }}>AI 发言已达上限，被 @ 的成员不会再回复</span>}
          </div>
          <div className="row">
            <textarea value={text} placeholder="说点什么…（Enter 发送，Shift+Enter 换行）" onChange={(e) => setText(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Tab" && mention && sugg.length > 0) { e.preventDefault(); complete(sugg[0]); return; }
                if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) { e.preventDefault(); void send(); }
              }} />
            <button className="btn primary" disabled={!text.trim()} onClick={() => void send()}>发送</button>
          </div>
        </footer>
      ) : <div className="hint" style={{ padding: 14, textAlign: "center" }}>会议已结束</div>}
    </div>
  );
}
