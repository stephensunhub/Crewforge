import { useCallback, useEffect, useMemo, useState } from "react";
import { api } from "./api";
import type { CallRow, CostGroup, CostReport, EventRow, Totals } from "./types";

const RANGES: [string, number | null][] = [["今天", 1], ["近 7 天", 7], ["近 30 天", 30], ["全部", null]];

const fmtNum = (n: number) => (n >= 1e9 ? `${(n / 1e9).toFixed(2)}B` : n >= 1e6 ? `${(n / 1e6).toFixed(2)}M` : n >= 1e4 ? `${(n / 1e3).toFixed(1)}k` : n.toLocaleString("zh-CN"));
const fmtCost = (n: number) => `$${n < 1 ? n.toFixed(3) : n.toFixed(2)}`;
const pct = (n: number | null) => (n == null ? "未知" : `${(n * 100).toFixed(0)}%`);
const secs = (ms: number) => (ms >= 60_000 ? `${(ms / 60_000).toFixed(1)} 分钟` : `${(ms / 1000).toFixed(1)} 秒`);

function resetText(at: number | null): string {
  if (at == null) return "重置时间未知";
  const ms = at * 1000 - Date.now();
  if (ms <= 0) return "已重置";
  const h = Math.floor(ms / 3_600_000);
  const m = Math.floor((ms % 3_600_000) / 60_000);
  return h >= 24 ? `约 ${Math.floor(h / 24)} 天 ${h % 24} 小时后重置` : `约 ${h} 小时 ${m} 分后重置`;
}

function Spark({ points, label }: { points: (number | null)[]; label: string }) {
  const v = points.filter((x): x is number => x != null);
  if (v.length < 2) return <span className="hint">数据不足</span>;
  const W = 220, H = 38;
  const path = v.map((y, i) => `${i === 0 ? "M" : "L"}${((i / (v.length - 1)) * W).toFixed(1)},${(H - Math.min(1, y) * (H - 4) - 2).toFixed(1)}`).join(" ");
  return (
    <svg width={W} height={H} role="img" aria-label={label} className="spark">
      <title>{label}</title>
      <line x1="0" x2={W} y1={H - 0.85 * (H - 4) - 2} y2={H - 0.85 * (H - 4) - 2} stroke="var(--bad)" strokeDasharray="3 3" opacity=".5" />
      <path d={path} fill="none" stroke="var(--accent)" strokeWidth="1.6" />
    </svg>
  );
}

function Meter({ label, v, reset, series }: { label: string; v: number | null; reset: number | null; series: (number | null)[] }) {
  return (
    <div className="qmeter">
      <div className="qtop"><b>{label}</b><span className={(v ?? 0) > 0.85 ? "bad" : ""}>{pct(v)}</span></div>
      <div className="meter wide"><i className={(v ?? 0) > 0.85 ? "hot" : ""} style={{ width: `${Math.min(100, (v ?? 0) * 100)}%` }} /></div>
      <div className="hint">{resetText(reset)}</div>
      <Spark points={series} label={`${label}使用率走势`} />
    </div>
  );
}

function Bars({ days, metric }: { days: CostReport["by_day"]; metric: "cost" | "tokens" }) {
  if (days.length === 0) return <div className="hint">这个时间范围内还没有调用。</div>;
  const val = (t: Totals) => (metric === "cost" ? t.cost_usd : t.input_tokens + t.cache_read_tokens + t.cache_creation_tokens + t.output_tokens);
  const max = Math.max(1e-9, ...days.map((d) => val(d.totals)));
  const W = 700, H = 150, bw = Math.min(46, (W - 20) / days.length - 4);
  const step = (W - 20) / days.length;
  return (
    <svg viewBox={`0 0 ${W} ${H + 22}`} className="bars" role="img" aria-label={metric === "cost" ? "每日估算费用" : "每日 token 总量"}>
      {days.map((d, i) => {
        const h = (val(d.totals) / max) * (H - 14);
        const x = 10 + i * step + (step - bw) / 2;
        const label = `${d.day}：${metric === "cost" ? fmtCost(d.totals.cost_usd) : fmtNum(val(d.totals)) + " token"}（${d.totals.turns} 次调用）`;
        return (
          <g key={d.day}>
            <title>{label}</title>
            <rect x={x} y={H - h} width={bw} height={Math.max(h, 1)} rx="3" fill="var(--accent)" opacity=".85" />
            {(days.length <= 10 || i % Math.ceil(days.length / 8) === 0 || i === days.length - 1) && (
              <text x={x + bw / 2} y={H + 14} textAnchor="middle" fontSize="10" fill="var(--muted)">{d.day.slice(5)}</text>
            )}
          </g>
        );
      })}
    </svg>
  );
}

function GroupTable({ title, groups }: { title: string; groups: CostGroup[] }) {
  return (
    <div>
      <h4 className="group-title">{title}</h4>
      {groups.length === 0 ? <div className="hint">暂无数据</div> : (
        <table className="cost-table">
          <thead><tr><th></th><th>调用</th><th>估算费用</th><th>上下文 token</th><th>输出</th><th>缓存命中</th></tr></thead>
          <tbody>{groups.map((g) => {
            const t = g.totals, ctx = t.input_tokens + t.cache_read_tokens + t.cache_creation_tokens;
            return (
              <tr key={g.key}>
                <td><b>{g.label}</b></td><td>{t.turns}{t.errors > 0 && <span className="bad"> · {t.errors} 出错</span>}</td>
                <td>{t.turns_with_cost === 0 ? <span className="hint">未知</span> : fmtCost(t.cost_usd)}{t.turns_with_cost > 0 && t.turns_with_cost < t.turns && <span className="hint" title="其余轮数 CLI 没有给出费用"> *</span>}</td>
                <td>{t.turns_with_tokens === 0 ? <span className="hint">未知</span> : fmtNum(ctx)}</td>
                <td>{t.turns_with_tokens === 0 ? "" : fmtNum(t.output_tokens)}</td>
                <td>{ctx > 0 ? pct(t.cache_read_tokens / ctx) : ""}</td>
              </tr>
            );
          })}</tbody>
        </table>
      )}
    </div>
  );
}

/** token 构成：未命中缓存的输入、缓存读、缓存写、输出各占多少。 */
function TokenMix({ t }: { t: Totals }) {
  const parts: [string, number, string][] = [
    ["缓存读", t.cache_read_tokens, "var(--ok)"], ["缓存写", t.cache_creation_tokens, "var(--warn)"],
    ["未缓存输入", t.input_tokens, "var(--accent)"], ["输出", t.output_tokens, "var(--muted)"],
  ];
  const sum = parts.reduce((a, p) => a + p[1], 0);
  if (sum === 0) return <div className="hint">暂无 token 数据</div>;
  return (
    <div>
      <div style={{ display: "flex", height: 14, borderRadius: 7, overflow: "hidden", background: "var(--line)" }} role="img" aria-label="token 构成">
        {parts.filter((p) => p[1] > 0).map(([l, v, c]) => <i key={l} title={`${l}：${fmtNum(v)}（${((v / sum) * 100).toFixed(1)}%）`} style={{ width: `${(v / sum) * 100}%`, background: c }} />)}
      </div>
      <div className="hint" style={{ marginTop: 6, display: "flex", gap: 14, flexWrap: "wrap" }}>
        {parts.map(([l, v, c]) => <span key={l}><i style={{ display: "inline-block", width: 8, height: 8, borderRadius: 4, background: c, marginRight: 4 }} />{l} {fmtNum(v)}（{((v / sum) * 100).toFixed(1)}%）</span>)}
      </div>
    </div>
  );
}

const hm = (ts: number) => { const d = new Date(ts); const p = (n: number) => String(n).padStart(2, "0"); return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`; };

/** 调用明细：每次模型调用一行，可按员工、用途、是否出错过滤，分页加载。 */
function CallLog({ days, employees, onError, refreshKey }: { days: number | null; employees: CostGroup[]; onError: (m: string) => void; refreshKey: number }) {
  const [rows, setRows] = useState<CallRow[]>([]);
  const [total, setTotal] = useState(0);
  const [emp, setEmp] = useState("");
  const [purpose, setPurpose] = useState("");
  const [errs, setErrs] = useState(false);
  const PAGE = 30;
  const fetchPage = useCallback(async (offset: number) => {
    try {
      const p = await api.costCalls({ days, employeeId: emp || null, projectId: null, purpose: purpose || null, onlyErrors: errs, limit: PAGE, offset });
      setTotal(p.total);
      setRows((prev) => (offset === 0 ? p.rows : [...prev, ...p.rows]));
    } catch (e) { onError(String(e)); }
  }, [days, emp, purpose, errs, onError]);
  useEffect(() => { void fetchPage(0); }, [fetchPage, refreshKey]);
  return (
    <section className="panel-box" style={{ marginTop: 18 }}>
      <div className="page-head" style={{ marginBottom: 6 }}>
        <h4 className="group-title" style={{ margin: 0 }}>调用明细 <span className="hint">共 {total} 次</span></h4>
        <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
          <select style={{ width: "auto" }} value={emp} onChange={(e) => setEmp(e.target.value)}>
            <option value="">全部员工</option>{employees.map((g) => <option key={g.key} value={g.key}>{g.label}</option>)}
          </select>
          <select style={{ width: "auto" }} value={purpose} onChange={(e) => setPurpose(e.target.value)}>
            <option value="">全部用途</option><option value="task">开发任务</option><option value="meeting">会议发言</option><option value="minutes">纪要起草</option><option value="onboarding">导入评估</option>
          </select>
          <label className="hint"><input type="checkbox" checked={errs} onChange={(e) => setErrs(e.target.checked)} /> 只看出错</label>
        </div>
      </div>
      {rows.length === 0 ? <div className="hint">没有符合条件的调用。</div> : (
        <table className="cost-table">
          <thead><tr><th>时间</th><th>员工</th><th>用途 / 内容</th><th>模型</th><th>输入</th><th>输出</th><th>缓存读/写</th><th>费用</th><th>耗时</th></tr></thead>
          <tbody>{rows.map((r) => (
            <tr key={r.event_id} className={r.is_error ? "row-bad" : ""}>
              <td className="hint">{hm(r.ts)}</td><td><b>{r.employee}</b></td>
              <td>{r.purpose_label}{r.subject && <div className="hint" title={r.subject}>{r.subject.length > 28 ? `${r.subject.slice(0, 28)}…` : r.subject}</div>}{r.is_error && <span className="bad"> · 出错 {r.subtype}</span>}</td>
              <td className="hint">{r.models.length === 0 ? "未记录" : r.models.map((m) => m.model.replace(/^claude-/, "")).join("、")}</td>
              <td>{r.input_tokens == null ? "" : fmtNum(r.input_tokens)}</td><td>{r.output_tokens == null ? "" : fmtNum(r.output_tokens)}</td>
              <td>{r.cache_read_tokens == null ? "" : `${fmtNum(r.cache_read_tokens)} / ${fmtNum(r.cache_creation_tokens ?? 0)}`}</td>
              <td>{r.cost_usd == null ? <span className="hint">未知</span> : fmtCost(r.cost_usd)}</td>
              <td>{r.duration_ms == null ? "" : secs(r.duration_ms)}{r.ttft_ms != null && <div className="hint">首字 {secs(r.ttft_ms)}</div>}</td>
            </tr>))}</tbody>
        </table>
      )}
      {rows.length < total && <div style={{ marginTop: 8 }}><button className="btn" onClick={() => void fetchPage(rows.length)}>加载更多（已显示 {rows.length}/{total}）</button></div>}
    </section>
  );
}

export function CostView({ events, onError }: { events: EventRow[]; onError: (m: string) => void }) {
  const [days, setDays] = useState<number | null>(7);
  const [r, setR] = useState<CostReport | null>(null);
  const [metric, setMetric] = useState<"cost" | "tokens">("cost");

  const load = useCallback(async () => {
    try { setR(await api.costReport(days)); } catch (e) { onError(String(e)); }
  }, [days, onError]);
  useEffect(() => { void load(); }, [load]);
  // 有新的调用结果或额度事件时刷新
  const last = useMemo(() => events.reduce((m, e) => (e.kind === "turn_result" || e.kind === "rate_limit" ? Math.max(m, e.id) : m), 0), [events]);
  useEffect(() => { if (last) void load(); }, [last, load]);

  if (!r) return <div className="hint" style={{ padding: 20 }}>加载中…</div>;
  const t = r.totals;
  const ctx = t.input_tokens + t.cache_read_tokens + t.cache_creation_tokens;
  const e = r.efficiency;
  const q = r.quota_now;

  return (
    <div className="page wide">
      <div className="page-head">
        <div><h2>成本与额度</h2>
          <div className="hint">数据来自每次调用的真实记录。<b>费用是 CLI 报告的等值估算，不是订阅账单上的实际扣费</b>；CLI 没给出的数据标为“未知”，不会按 0 计入。</div></div>
        <select style={{ width: "auto" }} value={days ?? ""} onChange={(ev) => setDays(ev.target.value === "" ? null : Number(ev.target.value))}>
          {RANGES.map(([l, d]) => <option key={l} value={d ?? ""}>{l}</option>)}
        </select>
      </div>

      <section className="panel-box">
        <h4 className="group-title">订阅额度（所有员工共享同一份）</h4>
        {!q ? <div className="hint">还没有额度数据，员工执行过一次任务后就会出现。</div> : (
          <>
            <div className="qgrid">
              <Meter label="5 小时窗口" v={q.five_hour} reset={q.five_resets_at} series={r.quota_series.map((p) => p.five_hour)} />
              <Meter label="7 天窗口" v={q.seven_day} reset={q.seven_resets_at} series={r.quota_series.map((p) => p.seven_day)} />
            </div>
            <div className="hint">状态：{q.status === "allowed" ? "正常" : q.status === "allowed_warning" ? "接近上限（仍可使用）" : q.status}。虚线为 85% 警戒线。窗口数据来自最近一次调用，不是实时的。</div>
          </>
        )}
      </section>

      <div className="cards4">
        <div className="stat"><span>调用次数</span><b>{t.turns}</b>{t.errors > 0 && <i className="bad">{t.errors} 次出错</i>}</div>
        <div className="stat"><span>估算费用</span><b>{t.turns_with_cost === 0 ? "未知" : fmtCost(t.cost_usd)}</b>{t.turns_with_cost < t.turns && t.turns > 0 && <i>{t.turns - t.turns_with_cost} 轮无费用数据</i>}</div>
        <div className="stat"><span>处理的上下文 token</span><b>{t.turns_with_tokens === 0 ? "未知" : fmtNum(ctx)}</b><i>含缓存读写</i></div>
        <div className="stat"><span>输出 token</span><b>{t.turns_with_tokens === 0 ? "未知" : fmtNum(t.output_tokens)}</b></div>
        <div className="stat"><span>耗时（中位 / P95）</span><b>{r.latency.p50_ms == null ? "未知" : `${secs(r.latency.p50_ms)} / ${secs(r.latency.p95_ms ?? 0)}`}</b>
          {r.latency.avg_ttft_ms != null && <i>首字平均 {secs(r.latency.avg_ttft_ms)}（{r.latency.ttft_samples} 次）</i>}</div>
      </div>

      <section className="panel-box">
        <h4 className="group-title">token 构成</h4>
        <TokenMix t={t} />
      </section>

      <section className="panel-box">
        <div className="page-head" style={{ marginBottom: 6 }}>
          <h4 className="group-title" style={{ margin: 0 }}>每日用量</h4>
          <select style={{ width: "auto" }} value={metric} onChange={(ev) => setMetric(ev.target.value as "cost" | "tokens")}>
            <option value="cost">估算费用</option><option value="tokens">token 总量</option>
          </select>
        </div>
        <Bars days={days === 1 && r.by_hour.length > 0 ? r.by_hour : r.by_day} metric={metric} />
        {days === 1 && r.by_hour.length > 0 && <div className="hint">今天按小时显示。</div>}
      </section>

      <div className="two-col">
        <GroupTable title="按员工" groups={r.by_employee} />
        <GroupTable title="按项目" groups={r.by_project} />
      </div>
      <div className="two-col">
        <GroupTable title="按用途（会议和评估也会消耗额度）" groups={r.by_purpose} />
        <GroupTable title="按模型（一次调用用了多个模型时各算一次）" groups={r.by_model} />
      </div>

      <CallLog days={days} employees={r.by_employee} onError={onError} refreshKey={last} />

      <section style={{ marginTop: 18 }}>
        <h4 className="group-title">花费最多的任务</h4>
        {r.top_tasks.length === 0 ? <div className="hint">暂无任务数据</div> : (
          <table className="cost-table"><thead><tr><th>任务</th><th>员工</th><th>项目</th><th>轮数</th><th>估算费用</th><th>上下文 token</th></tr></thead>
            <tbody>{r.top_tasks.map((x) => (
              <tr key={x.task_id}><td><b>{x.title}</b></td><td>{x.employee}</td><td>{x.project}</td><td>{x.totals.turns}</td>
                <td>{x.totals.turns_with_cost === 0 ? "未知" : fmtCost(x.totals.cost_usd)}</td>
                <td>{x.totals.turns_with_tokens === 0 ? "未知" : fmtNum(x.totals.input_tokens + x.totals.cache_read_tokens + x.totals.cache_creation_tokens)}</td></tr>))}</tbody></table>
        )}
      </section>

      <section className="panel-box" style={{ marginTop: 18 }}>
        <h4 className="group-title">省 token 效率指标</h4>
        <div className="hint" style={{ marginBottom: 8 }}>用来验证“按需唤醒、会话复用、只检索相关记忆”这些设计是否真的省了。</div>
        <table className="cost-table"><tbody>
          <tr><td><b>平均每轮上下文</b><div className="hint">越小越省；包含缓存读写</div></td><td>{e.avg_context_per_turn == null ? "未知" : `${fmtNum(Math.round(e.avg_context_per_turn))} token`}</td></tr>
          <tr><td><b>缓存命中率</b><div className="hint">越高越省；会话复用得好，缓存才用得上</div></td><td>{pct(e.cache_hit_ratio)}</td></tr>
          <tr><td><b>会话复用率</b><div className="hint">开始执行时接续已有会话的比例</div></td><td>{e.total_starts === 0 ? "暂无" : `${pct(e.resumed_starts / e.total_starts)}（${e.resumed_starts}/${e.total_starts}）`}</td></tr>
          <tr><td><b>每个任务平均轮数</b><div className="hint">反复追问会让它变大</div></td><td>{e.avg_turns_per_task == null ? "暂无" : e.avg_turns_per_task.toFixed(1)}</td></tr>
          <tr><td><b>记忆注入</b><div className="hint">每次只注入少量相关记忆</div></td>
            <td>{e.memory_injections === 0 ? "暂无" : `${e.memory_injections} 次，平均每次 ${(e.memory_items / e.memory_injections).toFixed(1)} 条 / ${Math.round(e.memory_chars / e.memory_injections)} 字`}</td></tr>
          <tr><td><b>会议发言</b><div className="hint">只有被 @ 的 AI 发言才调用模型</div></td><td>人类 {e.meeting_human_messages} 条，AI 回复 {e.meeting_agent_replies} 条</td></tr>
          <tr><td><b>执行失败 / 会话恢复</b><div className="hint">进程崩溃或中途退出的次数</div></td><td>{e.failed_runs} 次失败，{e.recovered_sessions} 次恢复</td></tr>
        </tbody></table>
      </section>
      <div className="hint" style={{ marginTop: 10 }}>目前只有一个执行引擎（Claude Code，使用本机已登录账号）。重试次数尚未单独统计。</div>
    </div>
  );
}
