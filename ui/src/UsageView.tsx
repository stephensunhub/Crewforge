import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api } from "./api";
import type { CallRec, CostGroup, CostReport, EventRow, SeriesPoint, Totals, UsageFilter, UsageOverview, UsagePageData, SessionsView } from "./types";

// ---------- 格式化 ----------
const fmtInt = (n: number) => n.toLocaleString("zh-CN");
const fmtTok = (n: number) => (n >= 1e9 ? `${(n / 1e9).toFixed(2)}B` : n >= 1e6 ? `${(n / 1e6).toFixed(2)}M` : n >= 1e4 ? `${(n / 1e3).toFixed(1)}K` : fmtInt(n));
const fmtCost = (n: number) => `$${n < 1 ? n.toFixed(4) : n.toFixed(2)}`;
const pct = (n: number | null) => (n == null ? "—" : `${(n * 100).toFixed(1)}%`);
const dur = (ms: number) => (ms >= 60_000 ? `${(ms / 60_000).toFixed(1)} 分钟` : `${(ms / 1000).toFixed(1)} 秒`);
const durC = (ms: number) => (ms >= 60_000 ? `${(ms / 60_000).toFixed(1)}分` : `${(ms / 1000).toFixed(0)}秒`);
const hm = (ts: number) => { const d = new Date(ts); const p = (n: number) => String(n).padStart(2, "0"); return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`; };
const ctxOf = (t: Totals) => t.input_tokens + t.cache_read_tokens + t.cache_creation_tokens;
const allTok = (t: Totals) => ctxOf(t) + t.output_tokens;
const hit = (t: Totals) => (ctxOf(t) > 0 ? t.cache_read_tokens / ctxOf(t) : null);
const okRate = (t: Totals) => (t.turns > 0 ? (t.turns - t.errors) / t.turns : null);
const callTok = (r: CallRec) => (r.input_tokens ?? 0) + (r.output_tokens ?? 0) + (r.cache_read_tokens ?? 0) + (r.cache_creation_tokens ?? 0);
const hasTok = (r: CallRec) => [r.input_tokens, r.output_tokens, r.cache_read_tokens, r.cache_creation_tokens].some((x) => x != null);

// ---------- 时间范围 ----------
type Preset = "today" | "1d" | "7d" | "14d" | "30d" | "all" | "custom";
const PRESETS: [Preset, string][] = [["today", "今天"], ["1d", "24 小时"], ["7d", "7 天"], ["14d", "14 天"], ["30d", "30 天"], ["all", "全部"], ["custom", "自定义"]];
const dayStart = (d: Date) => { const x = new Date(d); x.setHours(0, 0, 0, 0); return x.getTime(); };
function resolveRange(p: Preset, from: string, to: string): { since: number | null; until: number | null } {
  const now = Date.now();
  switch (p) {
    case "today": return { since: dayStart(new Date()), until: null };
    case "1d": return { since: now - 86_400_000, until: null };
    case "7d": return { since: now - 7 * 86_400_000, until: null };
    case "14d": return { since: now - 14 * 86_400_000, until: null };
    case "30d": return { since: now - 30 * 86_400_000, until: null };
    case "custom": {
      const s = from ? dayStart(new Date(from + "T00:00:00")) : null;
      const u = to ? dayStart(new Date(to + "T00:00:00")) + 86_400_000 - 1 : null;
      return { since: s, until: u };
    }
    default: return { since: null, until: null };
  }
}
const daysOf = (p: Preset): number | null => (p === "today" ? 1 : p === "1d" ? 1 : p === "7d" ? 7 : p === "14d" ? 14 : p === "30d" ? 30 : null);

// ---------- 记住用户的选择 ----------
function useRemembered<T extends string | number | boolean>(key: string, init: T): [T, (v: T) => void] {
  const [v, setV] = useState<T>(() => { try { const s = localStorage.getItem(key); return s == null ? init : (JSON.parse(s) as T); } catch { return init; } });
  const set = useCallback((x: T) => { setV(x); try { localStorage.setItem(key, JSON.stringify(x)); } catch { /* 忽略 */ } }, [key]);
  return [v, set];
}

// ---------- 指标与图表 ----------
type Metric = "requests" | "cost" | "tokens";
const METRIC_LABEL: Record<Metric, string> = { requests: "请求数", cost: "估算费用", tokens: "Token 总量" };
const metricVal = (t: Totals, m: Metric) => (m === "requests" ? t.turns : m === "cost" ? t.cost_usd : allTok(t));
const TOKEN_PARTS: [keyof Totals, string, string][] = [
  ["cache_read_tokens", "缓存读", "var(--ok)"], ["cache_creation_tokens", "缓存写", "var(--warn)"], ["input_tokens", "新输入", "var(--accent)"], ["output_tokens", "输出", "#9aa0a6"],
];

function MetricCell({ label, value, sub, selected, onSelect, help }: { label: string; value: string; sub?: string; selected?: boolean; onSelect?: () => void; help?: string }) {
  const body = (<><span className="u-cell-label" title={help}>{label}{help && <i className="u-q">?</i>}</span><b className="u-cell-value">{value}</b>{sub && <span className="u-cell-sub">{sub}</span>}</>);
  return onSelect
    ? <button className={`u-cell ${selected ? "on" : ""}`} onClick={onSelect} aria-pressed={selected}>{body}</button>
    : <div className="u-cell static">{body}</div>;
}

function Trend({ series, metric, hourly }: { series: SeriesPoint[]; metric: Metric; hourly: boolean }) {
  const [hover, setHover] = useState<number | null>(null);
  if (series.length === 0 || series.every((p) => p.totals.turns === 0)) return <div className="hint" style={{ padding: "26px 16px" }}>这个范围内还没有调用。</div>;
  const W = 900, H = 170, padL = 6, padB = 22;
  const vals = series.map((p) => metricVal(p.totals, metric));
  const max = Math.max(1e-9, ...vals);
  const step = (W - padL) / series.length;
  const bw = Math.max(2, Math.min(40, step - 3));
  const tickEvery = Math.ceil(series.length / (hourly ? 12 : 10));
  return (
    <div style={{ position: "relative" }}>
      <svg viewBox={`0 0 ${W} ${H + padB}`} className="u-chart" role="img" aria-label={`${METRIC_LABEL[metric]}趋势`} onMouseLeave={() => setHover(null)}>
        {[0.25, 0.5, 0.75, 1].map((g) => <line key={g} x1={padL} x2={W} y1={H - g * (H - 12)} y2={H - g * (H - 12)} stroke="var(--line)" strokeDasharray="3 4" opacity=".6" />)}
        {series.map((p, i) => {
          const x = padL + i * step + (step - bw) / 2;
          const total = vals[i];
          const h = (total / max) * (H - 12);
          let y = H;
          const segs: [number, string][] = metric === "tokens"
            ? TOKEN_PARTS.map(([k, , c]) => [Number(p.totals[k]) / Math.max(1, total) * h, c])
            : [[h, "var(--accent)"]];
          return (
            <g key={p.ts} onMouseEnter={() => setHover(i)}>
              <rect x={padL + i * step} y={0} width={step} height={H} fill="transparent" />
              {total > 0 && segs.map(([sh, c], j) => { y -= sh; return sh > 0 ? <rect key={j} x={x} y={y} width={bw} height={sh} fill={c} opacity={hover === i ? 1 : 0.85} rx="1.5" /> : null; })}
              {i % tickEvery === 0 && <text x={x + bw / 2} y={H + 15} textAnchor="middle" fontSize="10" fill="var(--muted)">{hourly ? p.label.slice(6) : p.label.slice(5)}</text>}
            </g>
          );
        })}
      </svg>
      {hover != null && (() => {
        const p = series[hover], t = p.totals;
        return (
          <div className="u-tip" style={{ left: `${Math.min(80, Math.max(2, ((hover + 0.5) / series.length) * 100 - 8))}%` }}>
            <b>{p.label}</b>
            <div>请求 {fmtInt(t.turns)}{t.errors > 0 && <span className="bad"> · {t.errors} 出错</span>}</div>
            <div>费用 {t.turns_with_cost === 0 ? "—" : fmtCost(t.cost_usd)}</div>
            <div>Token {t.turns_with_tokens === 0 ? "—" : fmtTok(allTok(t))}</div>
            {t.turns_with_tokens > 0 && <div className="hint">新输入 {fmtTok(t.input_tokens)} · 输出 {fmtTok(t.output_tokens)} · 缓存读 {fmtTok(t.cache_read_tokens)} · 缓存写 {fmtTok(t.cache_creation_tokens)}</div>}
          </div>
        );
      })()}
      {metric === "tokens" && <div className="u-legend">{TOKEN_PARTS.map(([, l, c]) => <span key={l}><i style={{ background: c }} />{l}</span>)}</div>}
    </div>
  );
}

function Heatmap({ days, metric }: { days: SeriesPoint[]; metric: Metric }) {
  const map = new Map(days.map((d) => [d.label, d.totals]));
  const today = new Date(); today.setHours(0, 0, 0, 0);
  const weeks = 53;
  const start = new Date(today); start.setDate(start.getDate() - (weeks * 7 - 1) - 0);
  start.setDate(start.getDate() - start.getDay());
  const cells: { date: string; v: number; t?: Totals; future: boolean }[][] = [];
  const iso = (d: Date) => `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
  const d = new Date(start);
  while (d <= today || cells.length === 0 || cells[cells.length - 1].length < 7) {
    if (d.getDay() === 0) cells.push([]);
    const key = iso(d); const t = map.get(key);
    cells[cells.length - 1].push({ date: key, v: t ? metricVal(t, metric) : 0, t, future: d > today });
    d.setDate(d.getDate() + 1);
    if (cells.length > weeks + 1) break;
  }
  const vs = cells.flat().map((c) => c.v).filter((v) => v > 0).sort((a, b) => a - b);
  const level = (v: number) => (v <= 0 ? 0 : vs.length === 0 ? 0 : 1 + Math.min(3, Math.floor((vs.indexOf(v) / vs.length) * 4)));
  const shade = ["var(--line)", "color-mix(in srgb, var(--accent) 30%, var(--panel))", "color-mix(in srgb, var(--accent) 55%, var(--panel))", "color-mix(in srgb, var(--accent) 80%, var(--panel))", "var(--accent)"];
  const size = 12, gap = 3;
  return (
    <div style={{ overflowX: "auto", padding: "12px 16px" }}>
      <svg width={cells.length * (size + gap)} height={7 * (size + gap)} role="img" aria-label={`${METRIC_LABEL[metric]}热力图`}>
        {cells.map((col, x) => col.map((c, y) => (c.future ? null : (
          <rect key={c.date} x={x * (size + gap)} y={y * (size + gap)} width={size} height={size} rx="2.5" fill={shade[level(c.v)]}>
            <title>{`${c.date}：${c.t ? `${fmtInt(c.t.turns)} 次请求 · ${c.t.turns_with_cost ? fmtCost(c.t.cost_usd) : "—"} · ${fmtTok(allTok(c.t))} token` : "没有调用"}`}</title>
          </rect>
        ))))}
      </svg>
      <div className="hint" style={{ marginTop: 6 }}>最近一年每天的{METRIC_LABEL[metric]}，颜色越深越多。</div>
    </div>
  );
}

// ---------- 下拉过滤 ----------
function FilterSelect({ label, value, groups, onChange }: { label: string; value: string | null | undefined; groups: CostGroup[]; onChange: (v: string | null) => void }) {
  return (
    <select className={`u-pill ${value ? "active" : ""}`} value={value ?? ""} onChange={(e) => onChange(e.target.value || null)} aria-label={label}>
      <option value="">{label}：全部</option>
      {groups.map((g) => <option key={g.key} value={g.key}>{g.label}（{g.totals.turns}）</option>)}
      {value && !groups.some((g) => g.key === value) && <option value={value}>{value}</option>}
    </select>
  );
}

// ---------- 请求明细 ----------
type ColId = "time" | "engine" | "employee" | "subject" | "model" | "cost" | "input" | "output" | "cacheRead" | "cacheWrite" | "hit" | "speed" | "ttft" | "dur" | "status";
const COLS: { id: ColId; label: string; on: boolean; locked?: boolean }[] = [
  { id: "time", label: "时间", on: true, locked: true }, { id: "engine", label: "引擎", on: true }, { id: "employee", label: "员工", on: true },
  { id: "subject", label: "用途 / 内容", on: true }, { id: "model", label: "模型", on: true }, { id: "cost", label: "费用", on: true },
  { id: "input", label: "新输入", on: true }, { id: "output", label: "输出", on: true }, { id: "cacheRead", label: "缓存读", on: true },
  { id: "cacheWrite", label: "缓存写", on: false }, { id: "hit", label: "命中率", on: false }, { id: "speed", label: "速度", on: false },
  { id: "ttft", label: "首字", on: true }, { id: "dur", label: "耗时", on: true }, { id: "status", label: "状态", on: false },
];
const modelOf = (r: CallRec) => (r.models.length > 0 ? r.models.map((m) => m.model.replace(/^claude-/, "")).join("、") : r.model ?? "未记录");
const speedOf = (r: CallRec) => (r.duration_ms && r.output_tokens ? r.output_tokens / (r.duration_ms / 1000) : null);
const hitOf = (r: CallRec) => { const c = (r.input_tokens ?? 0) + (r.cache_read_tokens ?? 0) + (r.cache_creation_tokens ?? 0); return c > 0 ? (r.cache_read_tokens ?? 0) / c : null; };
const costCell = (r: CallRec) => (r.cost_usd != null ? fmtCost(r.cost_usd) : r.premium_requests != null ? `${r.premium_requests} 高级请求` : "—");

function LogTable({ filter, refreshKey, onOpen }: { filter: UsageFilter; refreshKey: number; onOpen: (r: CallRec) => void }) {
  const [page, setPage] = useState(0);
  const [size, setSize] = useRemembered<number>("usage.pageSize", 20);
  const [cols, setCols] = useRemembered<string>("usage.cols", COLS.filter((c) => c.on).map((c) => c.id).join(","));
  const [menu, setMenu] = useState(false);
  const [data, setData] = useState<UsagePageData | null>(null);
  const key = JSON.stringify(filter);
  useEffect(() => { setPage(0); }, [key, size]);
  useEffect(() => { void api.usageCalls(filter, page, size).then(setData).catch(() => undefined); }, [key, page, size, refreshKey]); // eslint-disable-line react-hooks/exhaustive-deps
  const visible = COLS.filter((c) => c.locked || cols.split(",").includes(c.id));
  const pages = data ? Math.max(1, Math.ceil(data.total / size)) : 1;
  const cell = (r: CallRec, id: ColId) => {
    switch (id) {
      case "time": return hm(r.ts);
      case "engine": return r.provider === "copilot" ? "Copilot" : "Claude";
      case "employee": return <b>{r.employee}</b>;
      case "subject": return <>{r.purpose_label}{r.subject && <div className="hint" title={r.subject}>{r.subject.length > 26 ? `${r.subject.slice(0, 26)}…` : r.subject}</div>}</>;
      case "model": return <span className="hint">{modelOf(r)}</span>;
      case "cost": return costCell(r);
      case "input": return r.input_tokens == null ? "—" : fmtTok(r.input_tokens);
      case "output": return r.output_tokens == null ? "—" : fmtTok(r.output_tokens);
      case "cacheRead": return r.cache_read_tokens == null ? "—" : fmtTok(r.cache_read_tokens);
      case "cacheWrite": return r.cache_creation_tokens == null ? "—" : fmtTok(r.cache_creation_tokens);
      case "hit": return pct(hitOf(r));
      case "speed": { const s = speedOf(r); return s == null ? "—" : `${s.toFixed(1)} tok/s`; }
      case "ttft": return r.ttft_ms == null ? "—" : dur(r.ttft_ms);
      case "dur": return r.duration_ms == null ? "—" : dur(r.duration_ms);
      case "status": return r.is_error ? <span className="bad">出错 · {r.subtype}</span> : "成功";
    }
  };
  return (
    <div>
      <div className="u-toolbar">
        <div style={{ position: "relative" }}>
          <button className="btn" onClick={() => setMenu((v) => !v)} aria-label="显示的列">列 ▾</button>
          {menu && (
            <div className="u-menu" onMouseLeave={() => setMenu(false)}>
              {COLS.map((c) => (
                <label key={c.id}><input type="checkbox" checked={c.locked || cols.split(",").includes(c.id)} disabled={c.locked}
                  onChange={(e) => { const s = new Set(cols.split(",")); e.target.checked ? s.add(c.id) : s.delete(c.id); setCols(COLS.filter((x) => s.has(x.id)).map((x) => x.id).join(",")); }} /> {c.label}</label>
              ))}
            </div>
          )}
        </div>
      </div>
      {!data ? <div className="hint" style={{ padding: 16 }}>加载中…</div> : data.rows.length === 0 ? <div className="hint" style={{ padding: 16 }}>没有符合条件的调用。</div> : (
        <div style={{ overflowX: "auto" }}>
          <table className="cost-table u-table">
            <thead><tr>{visible.map((c) => <th key={c.id}>{c.label}</th>)}</tr></thead>
            <tbody>{data.rows.map((r) => (
              <tr key={r.event_id} className={`clickable ${r.is_error ? "row-bad" : ""}`} onClick={() => onOpen(r)} tabIndex={0} onKeyDown={(e) => e.key === "Enter" && onOpen(r)}>
                {visible.map((c) => <td key={c.id}>{cell(r, c.id)}</td>)}
              </tr>))}</tbody>
          </table>
        </div>
      )}
      <div className="u-pager">
        <span className="hint">共 {data?.total ?? 0} 条</span>
        <span style={{ flex: 1 }} />
        <select value={size} onChange={(e) => setSize(Number(e.target.value))} aria-label="每页条数">{[10, 20, 50, 100].map((n) => <option key={n} value={n}>{n} 条/页</option>)}</select>
        <button className="btn" disabled={page === 0} onClick={() => setPage(page - 1)}>上一页</button>
        <span className="hint">{page + 1} / {pages}</span>
        <button className="btn" disabled={page + 1 >= pages} onClick={() => setPage(page + 1)}>下一页</button>
      </div>
    </div>
  );
}

// ---------- 请求详情 ----------
function Detail({ r, onClose }: { r: CallRec; onClose: () => void }) {
  const sp = speedOf(r);
  const row = (k: string, v: React.ReactNode) => <tr><td className="hint" style={{ width: 120 }}>{k}</td><td>{v}</td></tr>;
  return (
    <div className="u-drawer-wrap" onClick={onClose}>
      <aside className="u-drawer" onClick={(e) => e.stopPropagation()} role="dialog" aria-label="请求详情">
        <div className="page-head"><h3 style={{ margin: 0 }}>请求详情</h3><button className="btn" onClick={onClose}>关闭</button></div>
        <h4 className="group-title">基本信息</h4>
        <table className="cost-table"><tbody>
          {row("时间", hm(r.ts))}{row("引擎", r.provider === "copilot" ? "GitHub Copilot" : "Claude Code")}{row("员工", r.employee)}
          {row("项目", r.project || "—")}{row("用途", r.purpose_label)}{row("内容", r.subject || "—")}{row("模型", modelOf(r))}
          {row("事件 ID", <code>{r.event_id}</code>)}{r.session_id && row("会话", <code>{r.session_id}</code>)}
        </tbody></table>
        <h4 className="group-title">Token</h4>
        {!hasTok(r) ? <div className="hint">这次调用没有 token 数据{r.provider === "copilot" ? "（Copilot 只按高级请求计量）" : ""}。</div> : (
          <table className="cost-table"><tbody>
            {row("新输入", fmtInt(r.input_tokens ?? 0))}{row("输出", fmtInt(r.output_tokens ?? 0))}{row("缓存读", fmtInt(r.cache_read_tokens ?? 0))}{row("缓存写", fmtInt(r.cache_creation_tokens ?? 0))}
            {row("命中率", pct(hitOf(r)))}{row("真实总量", <b>{fmtInt(callTok(r))}</b>)}
          </tbody></table>
        )}
        <h4 className="group-title">费用</h4>
        <table className="cost-table"><tbody>
          {row("估算费用", r.cost_usd != null ? fmtCost(r.cost_usd) : "—")}
          {r.premium_requests != null && row("高级请求", r.premium_requests)}
          {r.models.length > 1 && r.models.map((m) => row(m.model, `${m.cost_usd != null ? fmtCost(m.cost_usd) : "—"} · 输出 ${fmtTok(m.output)} · 缓存读 ${fmtTok(m.cache_read)}`))}
        </tbody></table>
        <div className="hint">费用是 CLI 报告的等值估算，不是订阅账单上的实际扣费。</div>
        <h4 className="group-title">性能</h4>
        <table className="cost-table"><tbody>
          {row("耗时", r.duration_ms == null ? "—" : dur(r.duration_ms))}{row("首字时间", r.ttft_ms == null ? "没有记录" : dur(r.ttft_ms))}
          {row("输出速度", sp == null ? "—" : `${sp.toFixed(1)} token/秒（输出 token ÷ 整轮耗时，包含工具执行时间）`)}
        </tbody></table>
        {r.is_error && (<><h4 className="group-title">错误</h4><div className="note bad">{r.subtype || "执行出错"}</div></>)}
      </aside>
    </div>
  );
}

// ---------- 统计表 ----------
function StatsTable({ groups, nameLabel }: { groups: CostGroup[]; nameLabel: string }) {
  if (groups.length === 0) return <div className="hint" style={{ padding: 16 }}>这个范围内还没有数据。</div>;
  return (
    <div style={{ overflowX: "auto" }}>
      <table className="cost-table u-table">
        <thead><tr><th>{nameLabel}</th><th>请求</th><th>Token 总量</th><th>新输入</th><th>输出</th><th>缓存读</th><th>缓存写</th><th>命中率</th><th>费用</th><th>平均费用/次</th><th>成功率</th></tr></thead>
        <tbody>{groups.map((g) => {
          const t = g.totals, tk = t.turns_with_tokens > 0, co = t.turns_with_cost > 0;
          return (
            <tr key={g.key}>
              <td><b>{g.label}</b></td><td>{fmtInt(t.turns)}</td>
              <td>{tk ? fmtTok(allTok(t)) : "—"}</td><td>{tk ? fmtTok(t.input_tokens) : "—"}</td><td>{tk ? fmtTok(t.output_tokens) : "—"}</td>
              <td>{tk ? fmtTok(t.cache_read_tokens) : "—"}</td><td>{tk ? fmtTok(t.cache_creation_tokens) : "—"}</td><td>{tk ? pct(hit(t)) : "—"}</td>
              <td>{co ? fmtCost(t.cost_usd) : t.premium_requests > 0 ? `${t.premium_requests} 高级请求` : "—"}</td>
              <td>{co ? fmtCost(t.cost_usd / t.turns_with_cost) : "—"}</td><td>{pct(okRate(t))}</td>
            </tr>
          );
        })}</tbody>
      </table>
    </div>
  );
}

// ---------- 效率与额度（沿用成本页的内容） ----------
function Efficiency({ days }: { days: number | null }) {
  const [r, setR] = useState<CostReport | null>(null);
  useEffect(() => { void api.costReport(days).then(setR).catch(() => undefined); }, [days]);
  if (!r) return <div className="hint" style={{ padding: 16 }}>加载中…</div>;
  const e = r.efficiency, q = r.quota_now;
  return (
    <div style={{ padding: 16, display: "grid", gap: 16 }}>
      <div>
        <h4 className="group-title">Claude 订阅额度（所有 Claude 员工共享同一份）</h4>
        {!q ? <div className="hint">还没有额度数据。</div> : (
          <div className="qgrid">
            {([["5 小时窗口", q.five_hour], ["7 天窗口", q.seven_day]] as const).map(([l, v]) => (
              <div className="qmeter" key={l}><div className="qtop"><b>{l}</b><span className={(v ?? 0) > 0.85 ? "bad" : ""}>{v == null ? "未知" : `${(v * 100).toFixed(0)}%`}</span></div>
                <div className="meter wide"><i className={(v ?? 0) > 0.85 ? "hot" : ""} style={{ width: `${Math.min(100, (v ?? 0) * 100)}%` }} /></div></div>
            ))}
          </div>
        )}
        <div className="hint" style={{ marginTop: 6 }}>GitHub Copilot 没有额度读数，只能按“高级请求”计数。</div>
      </div>
      <div>
        <h4 className="group-title">省 token 效率</h4>
        <table className="cost-table"><tbody>
          <tr><td><b>平均每轮上下文</b></td><td>{e.avg_context_per_turn == null ? "—" : `${fmtTok(Math.round(e.avg_context_per_turn))} token`}</td></tr>
          <tr><td><b>缓存命中率</b></td><td>{pct(e.cache_hit_ratio)}</td></tr>
          <tr><td><b>会话复用率</b></td><td>{e.total_starts === 0 ? "—" : `${pct(e.resumed_starts / e.total_starts)}（${e.resumed_starts}/${e.total_starts}）`}</td></tr>
          <tr><td><b>每个任务平均轮数</b></td><td>{e.avg_turns_per_task == null ? "—" : e.avg_turns_per_task.toFixed(1)}</td></tr>
          <tr><td><b>记忆注入</b></td><td>{e.memory_injections === 0 ? "—" : `${e.memory_injections} 次，平均 ${(e.memory_items / e.memory_injections).toFixed(1)} 条 / ${Math.round(e.memory_chars / e.memory_injections)} 字`}</td></tr>
          <tr><td><b>会议发言</b></td><td>人类 {e.meeting_human_messages} 条，AI {e.meeting_agent_replies} 条</td></tr>
          <tr><td><b>执行失败 / 会话恢复</b></td><td>{e.failed_runs} / {e.recovered_sessions}</td></tr>
        </tbody></table>
      </div>
      <div>
        <h4 className="group-title">花费最多的任务</h4>
        {r.top_tasks.length === 0 ? <div className="hint">暂无任务数据</div> : (
          <table className="cost-table"><thead><tr><th>任务</th><th>员工</th><th>项目</th><th>轮数</th><th>估算费用</th></tr></thead>
            <tbody>{r.top_tasks.map((x) => <tr key={x.task_id}><td><b>{x.title}</b></td><td>{x.employee}</td><td>{x.project}</td><td>{x.totals.turns}</td><td>{x.totals.turns_with_cost === 0 ? "—" : fmtCost(x.totals.cost_usd)}</td></tr>)}</tbody></table>
        )}
      </div>
    </div>
  );
}

// ---------- 页面 ----------
type Tab = "logs" | "sessions" | "model" | "employee" | "project" | "purpose" | "efficiency";
function SessionsTable({ refreshKey, onError }: { refreshKey: number; onError: (m: string) => void }) {
  const [v, setV] = useState<SessionsView | null>(null);
  useEffect(() => { void api.usageSessions().then(setV).catch((e) => onError(String(e))); }, [refreshKey]); // eslint-disable-line react-hooks/exhaustive-deps
  if (!v) return <div className="empty" style={{ padding: 16 }}>加载中…</div>;
  if (v.sessions.length === 0) return <div className="empty" style={{ padding: 16 }}>还没有员工会话。</div>;
  const base = (p: string) => p.split("/").filter(Boolean).slice(-2).join("/");
  return (
    <div style={{ overflowX: "auto" }}>
      <div className="muted" style={{ padding: "8px 12px", fontSize: 12 }}>
        {v.limit > 0 ? `上下文超过 ${fmtTok(v.limit)} 后，该员工的下一次任务会换成新会话（设置项 context_limit，0 为不限）。` : "未设置上下文上限（context_limit = 0）。"}
        上下文越大，每一步越贵。
      </div>
      <table className="cost-table u-table">
        <thead><tr><th>员工</th><th>目录</th><th>状态</th><th>当前上下文</th><th>占上限</th><th>最近活动</th><th>已换新</th></tr></thead>
        <tbody>{v.sessions.map((x) => {
          const c = x.context_tokens, ratio = c != null && v.limit > 0 ? c / v.limit : null;
          const cls = ratio == null ? "" : ratio >= 1 ? "bad" : ratio >= 0.7 ? "warn" : "";
          return (
            <tr key={x.session_id}>
              <td><b>{x.employee}</b> <span className="muted">{x.provider}</span></td>
              <td title={x.path}>{base(x.path)}</td>
              <td>{x.status === "running" ? "运行中" : "空闲"}</td>
              <td className={cls}>{c == null ? (x.provider === "copilot" ? "不提供" : "—") : fmtTok(c)}</td>
              <td className={cls}>{ratio == null ? "—" : `${Math.round(ratio * 100)}%${ratio >= 1 ? "（下次换新）" : ""}`}</td>
              <td>{new Date(x.updated_at).toLocaleString("zh-CN", { hour12: false })}</td>
              <td>{x.rotations > 0 ? `${x.rotations} 次` : "—"}</td>
            </tr>
          );
        })}</tbody>
      </table>
    </div>
  );
}

const TABS: [Tab, string][] = [["logs", "请求明细"], ["sessions", "员工会话"], ["model", "模型"], ["employee", "员工"], ["project", "项目"], ["purpose", "用途"], ["efficiency", "效率与额度"]];
const PURPOSES: [string, string][] = [["task", "开发任务"], ["meeting", "会议发言"], ["minutes", "纪要起草"], ["onboarding", "导入评估"]];

export function UsageView({ events, onError }: { events: EventRow[]; onError: (m: string) => void }) {
  const [preset, setPreset] = useRemembered<Preset>("usage.preset", "7d");
  const [from, setFrom] = useRemembered<string>("usage.from", "");
  const [to, setTo] = useRemembered<string>("usage.to", "");
  const [metric, setMetric] = useRemembered<Metric>("usage.metric", "cost");
  const [tab, setTab] = useRemembered<Tab>("usage.tab", "logs");
  const [refreshMs, setRefreshMs] = useRemembered<number>("usage.refresh", 0);
  const [provider, setProvider] = useState<string | null>(null);
  const [employee, setEmployee] = useState<string | null>(null);
  const [project, setProject] = useState<string | null>(null);
  const [model, setModel] = useState<string | null>(null);
  const [purpose, setPurpose] = useState<string | null>(null);
  const [status, setStatus] = useState<"ok" | "error" | null>(null);
  const [ov, setOv] = useState<UsageOverview | null>(null);
  const [stats, setStats] = useState<CostGroup[]>([]);
  const [detail, setDetail] = useState<CallRec | null>(null);
  const [settings, setSettings] = useState(false);
  const [tick, setTick] = useState(0);
  const range = useMemo(() => resolveRange(preset, from, to), [preset, from, to, tick]); // eslint-disable-line react-hooks/exhaustive-deps
  const filter: UsageFilter = useMemo(() => ({ ...range, provider, employeeId: employee, projectId: project, model, purpose, status }), [range, provider, employee, project, model, purpose, status]);
  const fkey = JSON.stringify(filter);

  const lastEvent = useMemo(() => events.reduce((m, e) => (e.kind === "turn_result" ? Math.max(m, e.id) : m), 0), [events]);
  const load = useCallback(async () => {
    try { setOv(await api.usageOverview(filter)); } catch (e) { onError(String(e)); }
  }, [fkey]); // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => { void load(); }, [load, lastEvent, tick]);
  useEffect(() => {
    if (tab === "logs" || tab === "efficiency" || tab === "sessions") return;
    void api.usageStats(filter, tab).then(setStats).catch((e) => onError(String(e)));
  }, [tab, fkey, lastEvent, tick]); // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => { if (refreshMs <= 0) return; const t = window.setInterval(() => setTick((x) => x + 1), refreshMs); return () => window.clearInterval(t); }, [refreshMs]);
  const lastLoaded = useRef(Date.now());
  useEffect(() => { if (ov) lastLoaded.current = Date.now(); }, [ov]);

  if (!ov) return <div className="hint" style={{ padding: 20 }}>加载中…</div>;
  const t = ov.summary;
  const counts: Record<Tab, number | null> = { logs: t.turns, sessions: null, model: ov.models.length, employee: ov.employees.length, project: ov.projects.length, purpose: null, efficiency: null };
  const clearAll = () => { setProvider(null); setEmployee(null); setProject(null); setModel(null); setPurpose(null); setStatus(null); };
  const anyFilter = provider || employee || project || model || purpose || status;

  return (
    <div className="page wide usage-page">
      <div className="page-head">
        <div><h2>用量</h2>
          <div className="hint">每次调用结束时记录的真实数据。<b>费用是 CLI 报告的等值估算，不是订阅账单上的实际扣费</b>；没有的数据显示“—”，不按 0 计入。</div></div>
        <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
          <span className="hint" title="数据随每次调用结束自动更新">更新于 {hm(lastLoaded.current).slice(6)}</span>
          <button className="btn" onClick={() => setTick((x) => x + 1)}>刷新</button>
          <button className="btn" onClick={() => setSettings((v) => !v)} aria-label="用量设置">⚙</button>
        </div>
      </div>

      {settings && (
        <section className="panel-box">
          <h4 className="group-title">用量设置</h4>
          <label className="hint" style={{ display: "flex", gap: 8, alignItems: "center" }}>自动刷新
            <select value={refreshMs} onChange={(e) => setRefreshMs(Number(e.target.value))}>
              <option value={0}>仅在有新调用时</option><option value={10000}>每 10 秒</option><option value={30000}>每 30 秒</option><option value={60000}>每 60 秒</option>
            </select>
          </label>
          <div className="hint" style={{ marginTop: 8 }}>价格：费用直接采用 Claude CLI 报告的估算，不自己按单价重算，所以这里没有价格表。Copilot 只按“高级请求”计数。</div>
        </section>
      )}

      <div className="u-bar">
        <div className="u-seg" role="tablist" aria-label="时间范围">
          {PRESETS.map(([k, l]) => <button key={k} role="tab" aria-selected={preset === k} className={preset === k ? "on" : ""} onClick={() => setPreset(k)}>{l}</button>)}
        </div>
        {preset === "custom" && (
          <span style={{ display: "flex", gap: 6, alignItems: "center" }}>
            <input type="date" value={from} onChange={(e) => setFrom(e.target.value)} aria-label="开始日期" />
            <span className="hint">至</span>
            <input type="date" value={to} onChange={(e) => setTo(e.target.value)} aria-label="结束日期" />
          </span>
        )}
      </div>

      <div className="u-bar">
        <div className="u-seg" role="group" aria-label="执行引擎">
          <button className={provider == null ? "on" : ""} onClick={() => setProvider(null)}>全部引擎</button>
          {ov.providers.map((g) => <button key={g.key} className={provider === g.key ? "on" : ""} onClick={() => setProvider(g.key)}>{g.label} <span className="hint">{g.totals.turns}</span></button>)}
        </div>
        <FilterSelect label="员工" value={employee} groups={ov.employees} onChange={setEmployee} />
        <FilterSelect label="模型" value={model} groups={ov.models} onChange={setModel} />
        <FilterSelect label="项目" value={project} groups={ov.projects} onChange={setProject} />
        <select className={`u-pill ${purpose ? "active" : ""}`} value={purpose ?? ""} onChange={(e) => setPurpose(e.target.value || null)} aria-label="用途">
          <option value="">用途：全部</option>{PURPOSES.map(([k, l]) => <option key={k} value={k}>{l}</option>)}
        </select>
        <select className={`u-pill ${status ? "active" : ""}`} value={status ?? ""} onChange={(e) => setStatus((e.target.value || null) as "ok" | "error" | null)} aria-label="状态">
          <option value="">状态：全部</option><option value="ok">成功</option><option value="error">出错</option>
        </select>
        {anyFilter && <button className="link" onClick={clearAll}>清除筛选</button>}
      </div>

      <section className="panel-box u-overview">
        <div className="u-cells">
          <MetricCell label="总请求数" value={fmtInt(t.turns)} sub={t.errors > 0 ? `${t.errors} 次出错` : undefined} selected={metric === "requests"} onSelect={() => setMetric("requests")} />
          <MetricCell label="估算费用" value={t.turns_with_cost === 0 ? "—" : fmtCost(t.cost_usd)} sub={t.turns_with_cost > 0 && t.turns_with_cost < t.turns ? `${t.turns - t.turns_with_cost} 次无费用数据` : t.turns_with_cost > 0 ? `平均 ${fmtCost(t.cost_usd / t.turns_with_cost)}/次` : undefined} selected={metric === "cost"} onSelect={() => setMetric("cost")} />
          <MetricCell label="真实总 Token" value={t.turns_with_tokens === 0 ? "—" : fmtTok(allTok(t))} sub="新输入 + 输出 + 缓存读写" help="包含缓存读和缓存写，是真正处理的 token 总量" selected={metric === "tokens"} onSelect={() => setMetric("tokens")} />
          <MetricCell label="新输入" value={t.turns_with_tokens === 0 ? "—" : fmtTok(t.input_tokens)} sub="未命中缓存的输入" />
          <MetricCell label="输出" value={t.turns_with_tokens === 0 ? "—" : fmtTok(t.output_tokens)} />
          <MetricCell label="缓存读" value={t.turns_with_tokens === 0 ? "—" : fmtTok(t.cache_read_tokens)} sub={`缓存写 ${t.turns_with_tokens === 0 ? "—" : fmtTok(t.cache_creation_tokens)}`} />
          <MetricCell label="缓存命中率" value={pct(hit(t))} help="缓存读 ÷ （新输入 + 缓存读 + 缓存写）" />
          <MetricCell label="成功率" value={pct(okRate(t))} />
          <MetricCell label="耗时 中位 / P95" value={ov.latency.p50_ms == null ? "—" : `${durC(ov.latency.p50_ms)} / ${durC(ov.latency.p95_ms ?? 0)}`} sub={ov.latency.avg_ttft_ms != null ? `首字平均 ${dur(ov.latency.avg_ttft_ms)}` : undefined} />
          <MetricCell label="输出速度" value={ov.median_speed == null ? "—" : `${ov.median_speed.toFixed(1)} tok/s`} sub="中位数" help="输出 token ÷ 整轮耗时，包含工具执行时间" />
          {t.premium_requests > 0 && <MetricCell label="Copilot 高级请求" value={String(t.premium_requests)} sub="按请求数计量" />}
        </div>
        <div className="u-chart-head"><b>{preset === "all" ? `${METRIC_LABEL[metric]}（最近一年）` : `${METRIC_LABEL[metric]}趋势（按${ov.granularity === "hour" ? "小时" : "天"}）`}</b></div>
        {preset === "all" ? <Heatmap days={ov.heatmap} metric={metric} /> : <Trend series={ov.series} metric={metric} hourly={ov.granularity === "hour"} />}
      </section>

      <section className="panel-box" style={{ padding: 0 }}>
        <div className="u-tabs" role="tablist">
          {TABS.map(([k, l]) => (
            <button key={k} role="tab" aria-selected={tab === k} className={tab === k ? "on" : ""} onClick={() => setTab(k)}>
              {l}{counts[k] != null && <span className="u-count">{fmtInt(counts[k]!)}</span>}
            </button>
          ))}
        </div>
        {tab === "logs" && <LogTable filter={filter} refreshKey={lastEvent + tick} onOpen={setDetail} />}
        {tab === "sessions" && <SessionsTable refreshKey={lastEvent + tick} onError={onError} />}
        {tab === "model" && <StatsTable groups={stats} nameLabel="模型" />}
        {tab === "employee" && <StatsTable groups={stats} nameLabel="员工" />}
        {tab === "project" && <StatsTable groups={stats} nameLabel="项目" />}
        {tab === "purpose" && <StatsTable groups={stats} nameLabel="用途" />}
        {tab === "efficiency" && <Efficiency days={daysOf(preset)} />}
      </section>

      {detail && <Detail r={detail} onClose={() => setDetail(null)} />}
    </div>
  );
}
