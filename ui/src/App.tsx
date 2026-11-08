import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api, subscribe } from "./api";
import type { ApprovalReq, Employee, EventRow, Session } from "./types";

const PROJECT_KEY = "crewforge.project";
const CONCURRENCY_KEY = "crewforge.concurrency";
const MAX_EVENTS = 1500;
const COLORS = ["#ffb347", "#6ec1ff", "#9be37a", "#e58bff", "#ff8a8a", "#7fe3d0"];

// 仅作为可选的快速开始示例；团队规模不设上限，可以是 1 个，也可以是 20 个
const SAMPLE_TEAM = [
  { name: "alice", role: "全栈工程师", persona: "你是 Alice，一名后端方向的全栈工程师。沟通简洁，先给结论再给理由，不确定时主动提问。" },
  { name: "bob", role: "全栈工程师", persona: "你是 Bob，一名前端方向的全栈工程师。沟通简洁，关注用户体验与可维护性，会认真审查同事的改动。" },
];

const colorFor = (name: string) => {
  let h = 0;
  for (const c of name) h = (h * 31 + c.charCodeAt(0)) >>> 0;
  return COLORS[h % COLORS.length];
};
const hhmm = (ts: number) => new Date(ts).toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit" });
const short = (s: string, n = 90) => (s.length > n ? s.slice(0, n) + "…" : s);
const basename = (p: string) => p.split("/").filter(Boolean).pop() ?? p;

function toolSummary(input: Record<string, unknown> | undefined): string {
  if (!input) return "";
  const v = input.file_path ?? input.command ?? input.pattern ?? input.path ?? input.url ?? "";
  return typeof v === "string" ? short(v) : "";
}

function Avatar({ name }: { name: string }) {
  return (
    <div className="avatar" style={{ background: colorFor(name) }}>
      {name.slice(0, 1).toUpperCase()}
    </div>
  );
}

export default function App() {
  const [employees, setEmployees] = useState<Employee[]>([]);
  const [sessions, setSessions] = useState<Session[]>([]);
  const [events, setEvents] = useState<EventRow[]>([]);
  const [running, setRunning] = useState<string[]>([]);
  const [approvals, setApprovals] = useState<ApprovalReq[]>([]);
  const [project, setProject] = useState(() => {
    try { return localStorage.getItem(PROJECT_KEY) ?? ""; } catch { return ""; }
  });
  const [text, setText] = useState("");
  const [error, setError] = useState("");
  const [verbose, setVerbose] = useState(false);
  const [showNew, setShowNew] = useState(false);
  const [concurrency, setConcurrency] = useState(3);
  const [filter, setFilter] = useState("");
  const [dataDir, setDataDir] = useState("");
  const feedRef = useRef<HTMLDivElement>(null);
  const stick = useRef(true);

  const reload = useCallback(async () => {
    try {
      const [emps, sess, evs, run, dir] = await Promise.all([
        api.employees(), api.sessions(), api.recentEvents(400), api.running(), api.dataDir(),
      ]);
      setEmployees(emps); setSessions(sess); setEvents(evs); setRunning(run); setDataDir(dir);
    } catch (e) { setError(String(e)); }
  }, []);

  useEffect(() => {
    void reload();
    // 并发上限由界面持久化，启动时同步给后端
    let saved = 0;
    try { saved = Number(localStorage.getItem(CONCURRENCY_KEY)) || 0; } catch { /* ignore */ }
    void (saved > 0 ? api.setConcurrency(saved) : api.getConcurrency()).then(setConcurrency);
    const un = subscribe({
      onEvent: (e) => {
        setEvents((prev) => [...prev, e].slice(-MAX_EVENTS));
        if (e.kind === "employee_created") void api.employees().then(setEmployees);
        if (["task_started", "turn_result", "task_failed", "task_error"].includes(e.kind))
          void api.sessions().then(setSessions);
      },
      onApproval: (a) => setApprovals((p) => [...p, a]),
      onRunning: setRunning,
    });
    return () => { void un.then((fns) => fns.forEach((f) => f())); };
  }, [reload]);

  useEffect(() => {
    if (stick.current) feedRef.current?.scrollTo({ top: feedRef.current.scrollHeight });
  }, [events, approvals]);

  useEffect(() => {
    try { localStorage.setItem(PROJECT_KEY, project); } catch { /* 无法持久化时忽略 */ }
  }, [project]);

  const nameById = useMemo(() => new Map(employees.map((e) => [e.id, e.name])), [employees]);
  const quota = useMemo(() => {
    for (let i = events.length - 1; i >= 0; i--)
      if (events[i].kind === "rate_limit") return events[i].payload as { status: string; five_hour: number | null; seven_day: number | null };
    return null;
  }, [events]);
  // 排队中：已提交但并发已满，尚未真正开始
  const queued = useMemo(() => {
    const q = new Set<string>();
    for (const e of events) {
      const n = e.employee_id ? nameById.get(e.employee_id) : undefined;
      if (!n) continue;
      if (e.kind === "task_queued") q.add(n);
      else if (["task_started", "task_failed", "task_error"].includes(e.kind)) q.delete(n);
    }
    for (const n of [...q]) if (!running.includes(n)) q.delete(n);
    return q;
  }, [events, nameById, running]);
  const working = running.filter((n) => !queued.has(n)).length;

  async function changeConcurrency(v: number) {
    const n = Math.max(1, Math.min(50, Math.round(v) || 1));
    setConcurrency(n);
    try { localStorage.setItem(CONCURRENCY_KEY, String(n)); } catch { /* ignore */ }
    try { setConcurrency(await api.setConcurrency(n)); } catch (e) { setError(String(e)); }
  }

  const mention = /@([^\s@]*)$/.exec(text);
  const suggestions = mention
    ? employees.filter((e) => e.name.toLowerCase().startsWith(mention[1].toLowerCase())).slice(0, 8)
    : employees.slice(0, 6);
  const completeMention = (n: string) =>
    setText((t) => (mention ? t.replace(/@([^\s@]*)$/, `@${n} `) : `${t}${t && !t.endsWith(" ") ? " " : ""}@${n} `));

  const visibleEmployees = filter.trim()
    ? employees.filter((e) => (e.name + e.role).toLowerCase().includes(filter.trim().toLowerCase()))
    : employees;

  const hasSession = (empId: string) => sessions.some((s) => s.employee_id === empId && s.project_path === project);

  async function pickProject() {
    try { const p = await api.pickDirectory(); if (p) setProject(p); } catch (e) { setError(String(e)); }
  }

  async function createEmployee(name: string, role: string, persona: string) {
    try { await api.addEmployee(name, role, persona); setShowNew(false); setError(""); } catch (e) { setError(String(e)); }
  }
  async function createSampleTeam() {
    for (const m of SAMPLE_TEAM) await createEmployee(m.name, m.role, m.persona);
  }

  // 解析 @提及：支持一次 @ 多人，并发各自执行
  async function send() {
    const body = text.trim();
    if (!body) return;
    if (!project) { setError("请先选择项目目录"); return; }
    const names = [...body.matchAll(/@([^\s@]+)/g)].map((m) => m[1]).filter((n) => employees.some((e) => e.name === n));
    const targets = [...new Set(names)];
    if (targets.length === 0) { setError("请用 @员工名 指定要找谁，例如 @alice"); return; }
    const prompt = body.replace(/@([^\s@]+)\s*/g, (m, n) => (targets.includes(n) ? "" : m)).trim() || body;
    setText(""); setError("");
    for (const t of targets) {
      try { await api.runTask(t, prompt, project); } catch (e) { setError(String(e)); }
    }
  }

  async function decide(a: ApprovalReq, allow: boolean) {
    setApprovals((p) => p.filter((x) => x.id !== a.id));
    try { await api.respondApproval(a.id, allow); } catch (e) { setError(String(e)); }
  }

  const who = (e: EventRow) => (e.employee_id ? nameById.get(e.employee_id) ?? "未知员工" : "系统");

  function renderEvent(e: EventRow) {
    const name = who(e);
    const p = e.payload ?? {};
    switch (e.kind) {
      case "task_started":
        return (
          <div className="msg me" key={e.id}>
            <div>
              <div className="who" style={{ textAlign: "right" }}>你 → @{name}{p.resume ? " · 续接会话" : ""}</div>
              <div className="bubble">{p.prompt}</div>
            </div>
          </div>
        );
      case "task_queued":
        return <div className="line warn" key={e.id}><span>⏸ {name} 排队中：并发已满（上限 {concurrency}），有空位后自动开始</span><span className="time">{hhmm(e.ts)}</span></div>;
      case "agent_text":
        return (
          <div className="msg" key={e.id}>
            <Avatar name={name} />
            <div>
              <div className="who">{name} · {hhmm(e.ts)}</div>
              <div className="bubble">{p.text}</div>
            </div>
          </div>
        );
      case "tool_use":
        return (
          <div className="line" key={e.id}>
            <span>🔧 {name}</span><span className="t">{p.tool}</span>
            <code>{toolSummary(p.input)}</code><span className="time">{hhmm(e.ts)}</span>
          </div>
        );
      case "approval_requested":
        return (
          <div className="line warn" key={e.id}>
            <span>⏳ {name} 请求审批</span><span className="t">{p.reason}</span><span className="time">{hhmm(e.ts)}</span>
          </div>
        );
      case "approval_decided":
        if (p.decided_by === "policy" && !verbose) return null;
        return (
          <div className={`line ${p.decision === "allow" ? "ok" : "bad"}`} key={e.id}>
            <span>{p.decision === "allow" ? "✓ 已允许" : "✗ 已拒绝"}</span>
            <span className="t">{p.tool}</span>
            <span>{p.decided_by === "human" ? "人类决定" : "策略自动放行"}</span>
            <span className="time">{hhmm(e.ts)}</span>
          </div>
        );
      case "turn_result":
        return (
          <div className={`line ${p.is_error ? "bad" : ""}`} key={e.id}>
            <span>{p.is_error ? "⚠ 执行出错" : "✔ 本轮完成"} · {name}</span>
            <span>
              {p.cost_usd != null ? `≈$${Number(p.cost_usd).toFixed(3)} · ` : ""}
              输入 {p.input_tokens ?? "?"} / 输出 {p.output_tokens ?? "?"}
            </span>
            <span className="time">{hhmm(e.ts)}</span>
          </div>
        );
      case "session_recovered":
        return <div className="line warn" key={e.id}><span>↻ {name} 的上次任务被中断，已恢复会话并核对状态</span><span className="time">{hhmm(e.ts)}</span></div>;
      case "task_failed":
      case "task_error":
        return <div className="line bad" key={e.id}><span>✗ {name} 任务失败：{short(String(p.error ?? ""), 200)}</span><span className="time">{hhmm(e.ts)}</span></div>;
      case "employee_created":
        return <div className="line" key={e.id}><span>＋ {p.name} 加入了团队</span><span className="time">{hhmm(e.ts)}</span></div>;
      default:
        return null;
    }
  }

  const insertMention = completeMention;
  const empty = events.filter((e) => ["task_started", "agent_text"].includes(e.kind)).length === 0;

  return (
    <div className="app">
      <aside className="side">
        <div className="brand">
          <img src="/logo.png" alt="" />
          <div><b>Crewforge</b><span>本地 AI 研发团队</span></div>
        </div>
        <h3>团队 <button onClick={() => setShowNew((v) => !v)}>{showNew ? "收起" : "＋ 新员工"}</button></h3>
        {employees.length > 8 && (
          <div style={{ padding: "0 12px 6px" }}>
            <input placeholder={`搜索 ${employees.length} 名员工`} value={filter} onChange={(e) => setFilter(e.target.value)} />
          </div>
        )}
        <div className="emps">
          {visibleEmployees.map((e) => (
            <button className="emp" key={e.id} onClick={() => insertMention(e.name)} title={e.persona}>
              <Avatar name={e.name} />
              <div>
                <div className="n">{e.name}</div>
                <div className="r">{e.role}{hasSession(e.id) ? " · 已有会话" : ""}</div>
              </div>
              <span
                className={`dot ${queued.has(e.name) ? "wait" : running.includes(e.name) ? "run" : ""}`}
                title={queued.has(e.name) ? "排队中" : running.includes(e.name) ? "工作中" : "空闲"}
              />
            </button>
          ))}
          {employees.length === 0 && <div className="hint" style={{ padding: "8px 8px" }}>还没有员工</div>}
          {employees.length > 0 && visibleEmployees.length === 0 && <div className="hint" style={{ padding: "8px 8px" }}>没有匹配的员工</div>}
        </div>
        {showNew && <NewEmployee onCreate={createEmployee} />}
        <div className="side-foot">数据目录<br />{dataDir}</div>
      </aside>

      <section className="main">
        <header className="top">
          <div className="proj">
            <button className="btn" onClick={pickProject}>{project ? "切换项目" : "选择项目目录"}</button>
            <div style={{ minWidth: 0 }}>
              <b>{project ? basename(project) : "未选择项目"}</b>
              <div className="path" title={project}>{project ? `‎${project}` : "员工将在该目录中工作"}</div>
            </div>
          </div>
          {quota && (
            <div className="quota" title={`额度状态：${quota.status}`}>
              {[["5 小时", quota.five_hour], ["7 天", quota.seven_day]].map(([l, v]) => (
                <div key={String(l)}>
                  {l} {v != null ? `${Math.round(Number(v) * 100)}%` : "?"}
                  <div className="meter"><i className={Number(v) > 0.85 ? "hot" : ""} style={{ width: `${Math.min(100, Number(v ?? 0) * 100)}%` }} /></div>
                </div>
              ))}
            </div>
          )}
          <div className="conc" title="员工数量不设上限；这里限制同时运行的 CLI 进程数（共享同一份订阅额度），超出的任务排队">
            <span>工作中 {working}{queued.size > 0 ? ` · 排队 ${queued.size}` : ""}</span>
            <label>并发上限
              <input type="number" min={1} max={50} value={concurrency} onChange={(e) => void changeConcurrency(Number(e.target.value))} />
            </label>
          </div>
          <label className="tog"><input type="checkbox" checked={verbose} onChange={(e) => setVerbose(e.target.checked)} />显示自动放行</label>
        </header>

        {error && <div className="banner"><span>{error}</span><button onClick={() => setError("")}>✕</button></div>}

        <div className="feed" ref={feedRef} onScroll={(e) => {
          const el = e.currentTarget;
          stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
        }}>
          {empty && (
            <div className="empty">
              <img src="/logo.png" alt="" />
              <h2>{employees.length ? "团队已就绪" : "先组建你的团队"}</h2>
              {employees.length ? (
                <p>选好项目目录，在下方用 <code>@{employees[0].name}</code> 给员工分配任务。可以同时 @ 多人，并发数超过上限时自动排队。</p>
              ) : (
                <>
                  <p>团队规模由你决定，一个人或二十个人都可以。先创建第一名员工。</p>
                  <button className="btn primary" onClick={() => setShowNew(true)}>创建第一名员工</button>
                  <p className="hint"><button className="link" onClick={createSampleTeam}>或快速创建示例团队（Alice 和 Bob）</button></p>
                </>
              )}
            </div>
          )}
          {events.map(renderEvent)}
        </div>

        {approvals.length > 0 && (
          <div className="approvals">
            {approvals.map((a) => (
              <div className="apv" key={a.id}>
                <b>⚠ {a.employee} 请求审批 · {a.tool}</b>
                <div className="hint">{a.reason}</div>
                <pre>{JSON.stringify(a.input, null, 2)}</pre>
                <div className="btns">
                  <button className="btn primary" onClick={() => decide(a, true)}>允许</button>
                  <button className="btn danger" onClick={() => decide(a, false)}>拒绝</button>
                </div>
              </div>
            ))}
          </div>
        )}

        <footer className="composer">
          <div className="chips">
            <span className="hint">{mention ? "选择员工（Tab 补全第一个）" : employees.length > 6 ? "输入 @ 搜索员工" : "@"}</span>
            {suggestions.map((e) => <button key={e.id} className="chip" onClick={() => insertMention(e.name)}>{e.name}</button>)}
            {mention && suggestions.length === 0 && <span className="hint">没有匹配的员工</span>}
          </div>
          <div className="row">
            <textarea
              value={text}
              placeholder="例如：@alice 给登录接口补上单元测试    （Enter 发送，Shift+Enter 换行）"
              onChange={(e) => setText(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Tab" && mention && suggestions.length > 0) { e.preventDefault(); completeMention(suggestions[0].name); return; }
                // 输入法选词时的回车不应发送
                if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) { e.preventDefault(); void send(); }
              }}
            />
            <button className="btn primary" disabled={!text.trim()} onClick={() => void send()}>发送</button>
          </div>
        </footer>
      </section>
    </div>
  );
}

function NewEmployee({ onCreate }: { onCreate: (n: string, r: string, p: string) => void }) {
  const [name, setName] = useState("");
  const [role, setRole] = useState("全栈工程师");
  const [persona, setPersona] = useState("");
  return (
    <div className="newemp">
      <input placeholder="名称（不含空格）" value={name} onChange={(e) => setName(e.target.value)} />
      <select value={role} onChange={(e) => setRole(e.target.value)}>
        <option>全栈工程师</option><option>QA 测试工程师</option><option>架构师 / CTO</option>
      </select>
      <textarea placeholder="身份说明（可选）" value={persona} onChange={(e) => setPersona(e.target.value)} />
      <button className="btn primary" disabled={!name.trim()} onClick={() => onCreate(name, role, persona || `你是 ${name}，一名${role}。`)}>创建</button>
    </div>
  );
}
