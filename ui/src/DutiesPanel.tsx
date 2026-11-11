import { useCallback, useEffect, useMemo, useState } from "react";
import { api } from "./api";
import { focusAction } from "./common";
import { AlignBox } from "./AlignBox";
import type { AutopilotConfig, AutopilotStatus, Duties, EventRow } from "./types";
import type { ProjectTab } from "./ProjectView";

const KIND_LABEL: Record<string, string> = { plan: "确认计划", decision: "拍板", proposal: "确认议题", merge: "确认合并", review: "确认评审意见", action: "你的事项", memory: "验证经验", goal: "确认改目标" };

const DECIDE: [string, string][] = [
  ["需求和业务方向", "做什么、不做什么。AI 没有拍板权限，系统在代码里强制拦着。"],
  ["涉及资金、生产、不可逆操作、安全的事", "同样只有你能拍板。"],
  ["确认 AI 起草的计划", "你批准之后，员工才会开始按计划做。"],
  ["里程碑验收", "你只在里程碑看结果，不用逐个任务验收。"],
];
const GUARD: [string, string][] = [
  ["合并到主分支", "评审通过后，由项目负责人最终确认并合并；负责人是你，所以由你来点。"],
  ["关闭评审行动项", "AI 只能处理，不能自己关；员工改完也要你确认。"],
  ["验证 AI 总结的经验", "没验证的经验不会给员工用。"],
  ["越权操作的审批", "工作区内读写自动放行，越界的才来问你。"],
];
const AI_DOES = "写代码、跑测试、互相评审、起草计划与纪要、提议题和经验、处理评审意见。这些不需要你盯着；AI 能提议，但没有拍板的工具。";

const AP_LABEL: Record<string, string> = { running: "推进中", paused: "已暂停", budget: "预算用尽，已暂停", escalated: "等你处理", finished: "已完成", stopped: "已停止", failed: "出错" };
const hms = (ts: number) => { const d = new Date(ts); const p = (n: number) => String(n).padStart(2, "0"); return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`; };

/** 自主推进：讨论定稿后由负责人推进，人旁观和纠偏。批次有预算，用完自动暂停。 */
function AutopilotBox({ projectId, events, onGoTab, onError }: { projectId: string; events: EventRow[]; onGoTab: (t: ProjectTab) => void; onError: (m: string) => void }) {
  const [st, setSt] = useState<AutopilotStatus | null>(null);
  // 每批次的预算和自动续批次的设置按项目保存，不用每次重填
  const [cfg, setCfg] = useState<AutopilotConfig>({ max_calls: 30, max_minutes: 60, auto_continue: false, max_batches: 6, standup_blocks: false });
  useEffect(() => { void api.getAutopilotConfig(projectId).then(setCfg).catch(() => undefined); }, [projectId]);
  const saveCfg = (next: AutopilotConfig) => { setCfg(next); void api.setAutopilotConfig(projectId, next).catch((e) => onError(String(e))); };
  const calls = cfg.max_calls;
  const minutes = cfg.max_minutes;
  const [busy, setBusy] = useState(false);
  const [resume, setResume] = useState(true);
  useEffect(() => { void api.getAutopilotResume(projectId).then(setResume).catch(() => setResume(true)); }, [projectId]);
  const refresh = useCallback(async () => { try { setSt(await api.autopilotStatus(projectId)); } catch { /* 忽略 */ } }, [projectId]);
  const timeline = useMemo(() => events.filter((e) => e.project_id === projectId && e.kind.startsWith("autopilot_")).slice(-40).reverse(), [events, projectId]);
  const lastId = timeline[0]?.id ?? 0;
  useEffect(() => { void refresh(); }, [refresh, lastId]);
  const run = st?.run ?? null;
  const live = st?.live ?? false;
  const act = async (f: () => Promise<unknown>) => { setBusy(true); try { await f(); await refresh(); } catch (e) { onError(String(e)); } finally { setBusy(false); } };
  return (
    <section className="panel-box">
      <div className="page-head" style={{ marginBottom: 8 }}>
        <div><h4 className="group-title" style={{ margin: 0 }}>自主推进 {run && <span className={`badge ${live ? "run" : ""}`} style={{ marginLeft: 6 }}>{AP_LABEL[run.status] ?? run.status}</span>}</h4>
          <div className="hint">讨论定稿后，由负责人按计划派活、验证、评审、合并，你旁观和纠偏。每个批次有预算，用完自动暂停并汇报。</div></div>
        {live ? (
          <div style={{ display: "flex", gap: 8 }}>
            <button className="btn" disabled={busy} onClick={() => void act(() => api.pauseAutopilot(projectId))}>暂停</button>
            <button className="btn danger" disabled={busy} onClick={() => void act(async () => { if (await api.confirm("停止自主推进？手上正在做的会做完当前一步就停。", "停止")) await api.stopAutopilot(projectId); })}>停止</button>
          </div>
        ) : (
          <div style={{ display: "flex", gap: 8, alignItems: "center", flexWrap: "wrap", justifyContent: "flex-end" }}>
            <button className="btn primary" disabled={busy} onClick={() => void act(async () => {
              if (await api.confirm(`开始自主推进？\n\n• 提议中的计划条目会一并批准（这就是你对计划的最后确认）\n• 本批次最多 ${calls} 次调用（只统计计入预算的）、${minutes > 0 ? `${minutes} 分钟` : "不限时间"}，用完自动暂停${cfg.auto_continue ? `；之后自动续批次，最多连续 ${cfg.max_batches} 个` : ""}\n• 遇到必须由你决定的事（改项目目标、危险命令）、反复失败时会停下来等你`, "开始自主推进")) await api.startAutopilot(projectId, calls, minutes);
            })}>{run ? "再开一个批次" : "开始自主推进"}</button>
          </div>
        )}
      </div>
      <div className="panel-box" style={{ margin: "0 0 10px", padding: "10px 12px" }}>
        <div className="hint" style={{ marginBottom: 6 }}><b>批次设置</b>（按项目保存；{live ? "对正在跑的这个批次不生效，下一个批次起生效" : "下次开批次时生效"}）</div>
        <div style={{ display: "flex", gap: 14, alignItems: "center", flexWrap: "wrap" }}>
          <label className="hint" style={{ whiteSpace: "nowrap" }} title="只统计计入预算的调用；连不限制预算的 LLM 端点的员工，调用不计入（在“模型 → LLM 端点”里设置）">每批次最多 <input type="number" min={1} style={{ width: 64 }} value={calls} onChange={(e) => setCfg({ ...cfg, max_calls: Math.max(1, Number(e.target.value) || 1) })} onBlur={() => saveCfg(cfg)} /> 次调用</label>
          <label className="hint" style={{ whiteSpace: "nowrap" }} title="0 表示不限时间">最长 <input type="number" min={0} style={{ width: 64 }} value={minutes} onChange={(e) => setCfg({ ...cfg, max_minutes: Math.max(0, Number(e.target.value) || 0) })} onBlur={() => saveCfg(cfg)} /> 分钟（<b>0 = 不限时间</b>）</label>
          <label className="hint" style={{ display: "flex", gap: 6, alignItems: "center" }} title="只在预算正常用完时续；遇到升级、失败、计划做完、你手动停止都不续。到了连续批次的上限会停下来通知你。">
            <input type="checkbox" checked={cfg.auto_continue} onChange={(e) => saveCfg({ ...cfg, auto_continue: e.target.checked })} />
            预算用完后自动续批次，最多连续
            <input type="number" min={1} max={50} style={{ width: 56 }} value={cfg.max_batches} disabled={!cfg.auto_continue} onChange={(e) => setCfg({ ...cfg, max_batches: Math.max(1, Math.min(50, Number(e.target.value) || 1)) })} onBlur={() => saveCfg(cfg)} />
            个批次
          </label>
          <label className="hint" style={{ display: "flex", gap: 6, alignItems: "center" }} title="关（默认）：对齐会放到后台开，任务不等。开：任务做完第一轮后，要等并行任务的对齐会开完才继续验证、评审、合并（本地模型很慢时对齐会要几十分钟）。改了之后下一次对齐会起生效。">
            <input type="checkbox" checked={cfg.standup_blocks} onChange={(e) => saveCfg({ ...cfg, standup_blocks: e.target.checked })} />
            对齐会阻塞任务（默认关：在后台开）
          </label>
        </div>
      </div>
      {run && (
        <div className="hint" style={{ marginBottom: 8 }}>
          本批次：{run.calls}/{run.max_calls} 次调用{run.uncounted_calls > 0 ? `（另有 ${run.uncounted_calls} 次调用不计入预算）` : ""} · 预算 {run.max_minutes > 0 ? `${run.max_minutes} 分钟` : "不限时间"}{run.chain > 0 ? ` · 自动续批次第 ${run.chain + 1} 个` : ""} · 开始于 {hms(run.started_at)}{run.reason && !live ? ` · ${run.reason}` : ""}
          {run.status === "escalated" && <> · <button className="link" onClick={() => onGoTab("issues")}>去看议题</button></>}
        </div>
      )}
      <label className="hint" style={{ display: "flex", gap: 6, alignItems: "center", marginBottom: 6 }} title="应用被关掉或重启时，正在推进的批次会中断。勾选后，下次打开应用约 5 秒后自动接着推进，预算是剩余的部分，也不会替你批准重启期间 AI 新提议的计划条目。">
        <input type="checkbox" checked={resume} onChange={(e) => { const v = e.target.checked; setResume(v); void api.setAutopilotResume(projectId, v).catch((er) => { setResume(!v); onError(String(er)); }); }} />
        应用重启后自动接着推进（用上次剩下的预算）
      </label>
      <div className="hint" style={{ marginBottom: 6 }}>你可以随时在<button className="link" onClick={() => onGoTab("chat")}>群聊</button>里 @ 员工或负责人纠偏；需要停就点“暂停”。</div>
      {timeline.length === 0 ? <div className="hint">还没有动态。</div> : (
        <div className="ap-timeline">
          {timeline.map((e) => (
            <div key={e.id} className={`ap-line ${e.kind === "autopilot_merged" || e.kind === "autopilot_decided" || e.kind === "autopilot_docs" || e.kind === "autopilot_memory" ? "ok" : e.kind === "autopilot_issue_escalated" ? "warn" : e.kind === "autopilot_finished" ? "end" : ""}`}>
              <span className="time">{hms(e.ts)}</span><span>{(e.payload as { text?: string }).text ?? e.kind}</span>
            </div>
          ))}
        </div>
      )}
    </section>
  );
}

export function DutiesPanel({ projectId, projectName, events, onGoTab, onOpenMeeting, onError }: { projectId: string; projectName: string; events: EventRow[]; onGoTab: (t: ProjectTab) => void; onOpenMeeting: (id: string) => void; onError: (m: string) => void }) {
  const [d, setD] = useState<Duties | null>(null);
  const [handling, setHandling] = useState(false);
  const [triaging, setTriaging] = useState(false);
  const [triaged, setTriaged] = useState<string | null>(null);
  const [handled, setHandled] = useState<string[]>([]);
  const load = useCallback(async () => { try { setD(await api.myDuties(projectId)); } catch { /* 忽略 */ } }, [projectId]);
  const last = useMemo(() => events.reduce((m, e) => (/^(plan_|issue_|action_|memory_|task_|meeting_|agent_proposed)/.test(e.kind) ? Math.max(m, e.id) : m), 0), [events]);
  useEffect(() => { void load(); }, [load, last]);

  return (
    <div className="page wide">
      <div className="page-head">
        <div><h2>我的职责</h2>
          <div className="hint">你在「{projectName}」里的身份是<b>项目负责人（人类 CTO / 架构师）</b>。AI 员工干活，你负责决定方向、把关质量、最终确认。</div></div>
      </div>

      <AutopilotBox projectId={projectId} events={events} onGoTab={onGoTab} onError={onError} />
      <AlignBox projectId={projectId} events={events} onOpenMeeting={onOpenMeeting} onError={onError} />

      <section className="panel-box">
        <h4 className="group-title">现在在等你 {d && <span className="u-count">{d.items.length}</span>}
          {d && d.items.some((i) => i.kind === "action") && (
            <button className="btn" style={{ marginLeft: 12 }} disabled={handling} title="这些是等负责人拍板的事。让 AI 负责人先看一遍：能定的定、不必做的驳回，只有缺少关键事实时才留给你。"
              onClick={async () => { setHandling(true); setHandled([]); try { setHandled(await api.leadHandleActions(projectId)); await load(); } catch (e) { onError(String(e)); } finally { setHandling(false); } }}>
              {handling ? "负责人处理中…" : "让负责人先处理"}</button>
          )}</h4>
        {handled.length > 0 && <div className="note ok" style={{ whiteSpace: "pre-wrap" }}>{handled.map((h) => `• ${h}`).join("\n")}</div>}
        {!d ? <div className="hint">加载中…</div> : d.items.length === 0 ? (
          <div className="hint" style={{ padding: "6px 0" }}>现在没有需要你处理的事，员工可以自己推进。有事会出现在这里，也会出现在左上角的“待你处理”里。</div>
        ) : (
          <div style={{ display: "grid", gap: 8 }}>
            {d.items.map((it, i) => (
              <div className="duty-item" key={i}>
                <div style={{ minWidth: 0 }}>
                  <span className="badge" style={{ marginRight: 8 }}>{KIND_LABEL[it.kind] ?? it.kind}</span><b>{it.title}</b>
                  <div className="hint" style={{ marginTop: 3 }}>为什么是你：{it.reason}</div>
                </div>
                <button className="btn primary" onClick={() => { if (it.meeting_id) { focusAction.id = it.action_id; onOpenMeeting(it.meeting_id); } else onGoTab(it.goto as ProjectTab); }}>去处理</button>
              </div>
            ))}
          </div>
        )}
        {d && d.later_actions > 0 && (
          <div className="hint" style={{ marginTop: 10 }}>另有 {d.later_actions} 条“之后再说”的评审行动项，不拦合并，也不需要你决定，集中在<button className="link" onClick={() => onGoTab("actions")}>行动项页</button>，不会占你的待办。
            {d.later_actions >= 5 && <button className="btn" style={{ marginLeft: 10 }} disabled={triaging} title="负责人逐条看：重复、过时、不必做的驳回，值得做的转成计划条目，拿不准的保留" onClick={async () => { setTriaging(true); setTriaged(null); try { setTriaged((await api.leadTriageActions(projectId)) ?? "数量不多，没有整理"); await load(); } catch (e) { onError(String(e)); } finally { setTriaging(false); } }}>{triaging ? "负责人整理中…" : "让负责人整理"}</button>}
            {triaged && <span className="hint" style={{ marginLeft: 10 }}>结果：{triaged}</span>}</div>
        )}
        {d && d.plan_total > 0 && <div className="hint" style={{ marginTop: 6 }}>计划进度：{d.plan_done}/{d.plan_total}（{d.plan_percent}%）。<button className="link" onClick={() => onGoTab("plan")}>查看计划</button></div>}
      </section>

      <div className="two-col">
        <section className="panel-box">
          <h4 className="group-title">只有你能决定</h4>
          <table className="cost-table"><tbody>{DECIDE.map(([k, v]) => <tr key={k}><td style={{ width: "42%" }}><b>{k}</b></td><td className="hint">{v}</td></tr>)}</tbody></table>
        </section>
        <section className="panel-box">
          <h4 className="group-title">需要你把关</h4>
          <table className="cost-table"><tbody>{GUARD.map(([k, v]) => <tr key={k}><td style={{ width: "42%" }}><b>{k}</b></td><td className="hint">{v}</td></tr>)}</tbody></table>
        </section>
      </div>

      <section className="panel-box">
        <h4 className="group-title">不需要你管</h4>
        <div className="hint">{AI_DOES}</div>
        <div className="hint" style={{ marginTop: 8 }}>这些职责来自需求文档：人类 CTO 负责需求确认、架构决策、重大技术争议裁决；合并由项目负责人确认；拍板权限按议题类型分（需求类和高风险议题只能人类）。想调整你想承担的范围，告诉我，先改定义，再改界面。</div>
      </section>
    </div>
  );
}
