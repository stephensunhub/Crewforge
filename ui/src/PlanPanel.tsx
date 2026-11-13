import { useCallback, useEffect, useMemo, useState } from "react";
import { api } from "./api";
import { Field, Modal, ReasonButton } from "./common";
import type { EventRow, GoalProposal, Member, PlanItem, PlanItemInput, PlanViewData, Project } from "./types";

const STATUS: Record<string, string> = { proposed: "待确认", planned: "待开始", in_progress: "进行中", done: "已完成", blocked: "已阻塞", cancelled: "已取消" };
const KIND: Record<string, string> = { characterize: "补特征测试", migrate: "迁移/实现", cleanup: "清理", other: "其他", design: "设计（架构师）", bugfix: "Bug 修复", spec: "功能规格" };
const GOAL_TEMPLATES: [string, string][] = [
  ["技术栈升级（业务不变）", "把现有系统迁移到新的技术栈。业务行为必须保持不变：对外接口、数据格式和可观察的输出都不能改变。\n每一步都要有测试或输出对照来证明行为没变；先补特征测试，再迁移；小步提交，每次只动一个模块。"],
  ["新项目", "从零实现这个项目。技术栈：（写在这里）。要求：一条命令就能运行；核心逻辑有自动化测试；README 写明如何运行和测试。"],
];

export const LEVEL_HINT = ["未定级", "1 机械改动", "2 简单", "3 中等", "4 较难", "5 最难"];

function ItemForm({ initial, items, members, onSave, onClose }: {
  initial?: PlanItem; items: PlanItem[]; members: Member[]; onClose: () => void; onSave: (i: PlanItemInput) => Promise<void>;
}) {
  const [f, setF] = useState<PlanItemInput>({
    title: initial?.title ?? "", description: initial?.description ?? "", acceptance: initial?.acceptance ?? "", kind: initial?.kind ?? "migrate",
    module: initial?.module ?? "", assignee_id: initial?.assignee_id ?? null, deps: initial?.deps ?? [], level: initial?.level ?? 0,
  });
  const others = items.filter((i) => i.id !== initial?.id && i.status !== "cancelled");
  return (
    <Modal title={initial ? "编辑条目" : "添加条目"} onClose={onClose} width={600}>
      <div className="form">
        <Field label="标题"><input value={f.title} autoFocus onChange={(e) => setF({ ...f, title: e.target.value })} /></Field>
        <Field label="说明" hint="做什么、范围、注意事项"><textarea rows={3} value={f.description} onChange={(e) => setF({ ...f, description: e.target.value })} /></Field>
        <Field label="验收标准" hint="做到什么算完成，最好能用命令验证"><textarea rows={2} value={f.acceptance} onChange={(e) => setF({ ...f, acceptance: e.target.value })} /></Field>
        <div className="row" style={{ gap: 12 }}>
          <Field label="类型"><select value={f.kind} onChange={(e) => setF({ ...f, kind: e.target.value })}>{Object.entries(KIND).map(([k, l]) => <option key={k} value={k}>{l}</option>)}</select></Field>
          <Field label="模块"><input value={f.module} onChange={(e) => setF({ ...f, module: e.target.value })} /></Field>
          <Field label="等级" hint="按需要的推理、联想和记忆定，不是工作量。没指定负责人时，自主推进会选「能力上限够用的最低档」的人"><select value={f.level} onChange={(e) => setF({ ...f, level: Number(e.target.value) })}>
            {LEVEL_HINT.map((l, n) => <option key={n} value={n}>{l}</option>)}</select></Field>
          <Field label="负责人"><select value={f.assignee_id ?? ""} onChange={(e) => setF({ ...f, assignee_id: e.target.value || null })}>
            <option value="">未指定</option>{members.map((m) => <option key={m.employee_id} value={m.employee_id}>{m.name}</option>)}</select></Field>
        </div>
        {others.length > 0 && (
          <Field label="依赖（这些条目完成后才能开始）">
            <div className="deps-pick">{others.map((o) => (
              <label className="tog" key={o.id}><input type="checkbox" checked={f.deps.includes(o.id)}
                onChange={(e) => setF({ ...f, deps: e.target.checked ? [...f.deps, o.id] : f.deps.filter((d) => d !== o.id) })} />{o.title}</label>))}</div>
          </Field>
        )}
        <div className="form-btns"><button className="btn primary" disabled={!f.title.trim()} onClick={() => void onSave(f)}>保存</button></div>
      </div>
    </Modal>
  );
}

export function PlanPanel({ project, members, events, onError, onChanged, onOpenMeeting }: {
  project: Project; members: Member[]; events: EventRow[]; onError: (m: string) => void; onChanged: () => void; onOpenMeeting: (id: string) => void;
}) {
  const [view, setView] = useState<PlanViewData | null>(null);
  const [goal, setGoal] = useState(project.goal);
  useEffect(() => { setGoal(project.goal); }, [project.goal]);
  const [gp, setGp] = useState<GoalProposal | null>(null);
  const [waiting, setWaiting] = useState<Record<string, string[]>>({});
  const [gpGoal, setGpGoal] = useState("");
  const [editing, setEditing] = useState<PlanItem | "new" | null>(null);
  const [pick, setPick] = useState<Record<string, string>>({});
  const [open, setOpen] = useState<string | null>(null);
  const [planner, setPlanner] = useState(members.find((m) => m.role === "lead")?.employee_id ?? members[0]?.employee_id ?? "");
  const [busy, setBusy] = useState("");
  const [note, setNote] = useState("");

  const load = useCallback(async () => { try { setView(await api.planView(project.id)); setWaiting(await api.planWaiting(project.id)); const g = await api.pendingGoalProposal(project.id); setGp(g); if (g) setGpGoal((cur) => (gp?.id === g.id && cur ? cur : g.new_goal)); } catch (e) { onError(String(e)); } }, [project.id, onError]);
  useEffect(() => { void load(); }, [load]);
  const last = useMemo(() => events.reduce((m, e) => (e.project_id === project.id && (e.kind.startsWith("plan_") || e.kind.startsWith("autopilot_") || e.kind === "task_started" || e.kind === "agent_proposed" || e.kind === "task_merged" || e.kind === "task_abandoned") ? Math.max(m, e.id) : m), 0), [events, project.id]);
  useEffect(() => { if (last) void load(); }, [last, load]);
  useEffect(() => { if (!planner && members[0]) setPlanner(members.find((m) => m.role === "lead")?.employee_id ?? members[0].employee_id); }, [members, planner]);

  async function act(fn: () => Promise<unknown>) { try { await fn(); await load(); onChanged(); } catch (e) { onError(String(e)); } }
  if (!view) return <div className="hint" style={{ padding: 20 }}>加载中…</div>;
  const proposed = view.items.filter((i) => i.status === "proposed");
  const real = view.items.filter((i) => i.status !== "proposed" && i.status !== "cancelled");
  const goalDirty = goal.trim() !== project.goal.trim();

  return (
    <div className="page wide">
      <div className="page-head">
        <div><h2>计划</h2><div className="hint">把大目标拆成有顺序、有依赖的小步骤，逐条派给员工。条目的任务合并后自动完成。</div></div>
        <div style={{ display: "flex", gap: 8 }}>
          {members.some((m) => m.role === "lead") && (
            <button className="btn" disabled={busy === "lead-plan"} title="负责人读项目目标、扫描、基线和现有计划，起草一份有依赖的计划；条目要你确认（开始自主推进时一并批准）" onClick={async () => {
              if (!project.goal.trim()) { onError("请先写下项目目标并保存，负责人需要它来制定计划"); return; }
              setBusy("lead-plan");
              try { const n = await api.leadDraftPlan(project.id); setNote(`负责人起草了 ${n} 个条目，在下面等你确认；确认后（或直接开始自主推进）才会派活。`); await load(); } catch (e) { onError(String(e)); } finally { setBusy(""); }
            }}>{busy === "lead-plan" ? "负责人起草中…" : "让负责人起草计划"}</button>
          )}
          <button className="btn primary" onClick={() => setEditing("new")}>＋ 添加条目</button>
        </div>
      </div>

      {note && <div className="note" style={{ marginBottom: 10 }}>{note}</div>}
      {gp && (
        <section className="panel-box" style={{ borderColor: "var(--warn)" }}>
          <b>{gp.proposer_name ?? "负责人"} 提议修改项目目标，等你点头</b>
          <div className="hint" style={{ margin: "4px 0 8px" }}>理由：{gp.reason}。批准前目标不变，员工仍按现有目标工作。</div>
          <div className="hint">现有目标</div>
          <div className="note" style={{ whiteSpace: "pre-wrap", marginBottom: 8 }}>{gp.old_goal.trim() || "（空）"}</div>
          <div className="hint">提议的新目标（可以先改再批准）</div>
          <textarea rows={4} value={gpGoal} onChange={(e) => setGpGoal(e.target.value)} />
          <div className="row" style={{ marginTop: 8, gap: 8 }}>
            <button className="btn primary" disabled={!gpGoal.trim()} onClick={() => void act(() => api.decideGoalProposal(gp.id, true, gpGoal.trim() === gp.new_goal.trim() ? null : gpGoal, ""))}>批准并改目标</button>
            <ReasonButton label="驳回" placeholder="为什么不改？负责人以后会看到" required onSubmit={(why) => void act(() => api.decideGoalProposal(gp.id, false, null, why))} />
          </div>
        </section>
      )}
      <section className="panel-box">
        <h4 className="group-title">项目目标与硬性约束</h4>
        <div className="hint" style={{ marginBottom: 6 }}>每个新任务开始时都会带上这段话，例如“技术栈升级，业务行为不变”。</div>
        <textarea rows={Math.min(16, Math.max(4, goal.split("\n").length + 1))} value={goal} placeholder="写下这个项目要达到什么、不能违反什么（支持 Markdown 写法，纯文本保存）" onChange={(e) => setGoal(e.target.value)} />
        <div className={`hint${[...goal].length > 6000 ? " bad" : ""}`} style={{ textAlign: "right" }}>{[...goal].length} / 6000 字</div>
        <div className="row" style={{ marginTop: 8, gap: 8, flexWrap: "wrap" }}>
          <button className="btn primary" disabled={!goalDirty} onClick={() => void act(() => api.setProjectGoal(project.id, goal))}>保存</button>
          {GOAL_TEMPLATES.map(([l, t]) => <button key={l} className="btn" onClick={() => setGoal(t)}>用模板：{l}</button>)}
        </div>
      </section>

      <section className="panel-box">
        <div className="plan-progress">
          <div className="meter wide"><i style={{ width: `${view.percent_done}%` }} /></div>
          <b>{view.percent_done}%</b>
          <span className="hint">完成 {view.counts.done} · 进行中 {view.counts.in_progress} · 待开始 {view.counts.planned} · 阻塞 {view.counts.blocked}{view.counts.proposed ? ` · 待确认 ${view.counts.proposed}` : ""}</span>
        </div>
        {view.items.length === 0 && members.length > 0 && (
          <div style={{ marginTop: 12, display: "grid", gap: 8 }}>
            <div className="hint">还没有计划。可以自己添加条目，也可以让一位员工根据项目目标和已有的扫描、评估结果起草，起草的条目要你批准后才生效。</div>
            <div className="row" style={{ gap: 8, alignItems: "center" }}>
              <select style={{ width: "auto" }} value={planner} onChange={(e) => setPlanner(e.target.value)}>{members.map((m) => <option key={m.employee_id} value={m.employee_id}>{m.name}</option>)}</select>
              <button className="btn primary" disabled={!planner || busy === "plan"} onClick={async () => {
                if (!project.goal.trim()) { onError("请先写下项目目标并保存，员工需要它来制定计划"); return; }
                setBusy("plan");
                try { const id = await api.startPlanning(project.id, planner, project.goal); onOpenMeeting(id); } catch (e) { onError(String(e)); } finally { setBusy(""); }
              }}>让 TA 制定计划</button>
            </div>
          </div>
        )}
      </section>

      {proposed.length > 0 && (
        <section className="panel-box" style={{ borderColor: "var(--warn)" }}>
          <div className="page-head" style={{ marginBottom: 6 }}>
            <div><b>AI 起草了 {proposed.length} 个条目，等你确认</b><div className="hint">批准后进入“待开始”；不合适的先编辑或取消。</div></div>
            <button className="btn primary" onClick={() => void act(() => api.approvePlan(project.id))}>批准全部</button>
          </div>
          {proposed.map((i) => (
            <div className="plan-row" key={i.id}>
              <div style={{ flex: 1 }}><b>{i.title}</b>{i.level > 0 && <span className="badge" style={{ marginLeft: 6 }} title={`等级：${LEVEL_HINT[i.level]}`}>L{i.level}</span>} <span className="hint">{KIND[i.kind]}{i.module ? ` · ${i.module}` : ""} · {i.proposed_by_name} 起草{i.assignee_name ? ` · 建议 ${i.assignee_name}` : ""}</span>
                {i.deps.length > 0 && <div className="hint">依赖 {i.deps.length} 项</div>}</div>
              <button className="btn" onClick={() => setEditing(i)}>编辑</button>
              <button className="btn danger" onClick={() => void act(() => api.setPlanStatus(i.id, "cancelled", "不采纳"))}>取消</button>
            </div>
          ))}
        </section>
      )}

      <div className="plan-list">
        {real.map((i, idx) => {
          const target = pick[i.id] ?? i.assignee_id ?? "";
          return (
            <div className={`plan-item st-${i.status}`} key={i.id}>
              <div className="plan-top">
                <span className="plan-n">{idx + 1}</span>
                <b style={{ flex: 1, cursor: "pointer" }} onClick={() => setOpen(open === i.id ? null : i.id)}>{i.title}</b>
                {i.level > 0 && <span className="badge" title={`等级：${LEVEL_HINT[i.level]}`}>L{i.level}</span>}
                <span className="chip static">{KIND[i.kind]}{i.module ? ` · ${i.module}` : ""}</span>
                <span className={`badge ps-${i.status}`}>{STATUS[i.status]}</span>
              </div>
              {(waiting[i.id] ?? []).length > 0 && (
                <div className={`note ${(waiting[i.id] ?? []).some((w) => w.startsWith("可以开始")) ? "ok" : "warn"}`}>
                  {(waiting[i.id] ?? []).map((w, k) => <div key={k}>{w.startsWith("可以开始") ? "✓ " : "⏸ "}{w}</div>)}
                </div>
              )}
              {i.note && <div className="hint">{i.note}</div>}
              {open === i.id && (
                <div className="plan-detail">
                  {i.description && <div style={{ whiteSpace: "pre-wrap" }}>{i.description}</div>}
                  {i.acceptance && <div className="hint">验收标准：{i.acceptance}</div>}
                </div>
              )}
              <div className="action-btns">
                {i.status === "planned" && (
                  <>
                    <select value={target} onChange={(e) => setPick({ ...pick, [i.id]: e.target.value })}>
                      <option value="">选择负责人…</option>{members.map((m) => <option key={m.employee_id} value={m.employee_id}>{m.name}</option>)}
                    </select>
                    <button className="btn primary" disabled={!i.ready || !target} title={i.ready ? "为它新开一个任务，交给 TA 做" : "前置条目还没完成"}
                      onClick={() => void act(() => api.dispatchPlanItem(i.id, target))}>开始</button>
                  </>
                )}
                {i.status === "in_progress" && <span className="hint">{i.assignee_name ? `${i.assignee_name} 正在做` : "进行中"}，合并后自动完成</span>}
                {i.status === "in_progress" && <button className="btn" onClick={() => void act(() => api.setPlanStatus(i.id, "done"))}>手动标记完成</button>}
                {["planned", "blocked", "in_progress"].includes(i.status) && <button className="btn" onClick={() => setEditing(i)}>编辑</button>}
                <button className="btn" onClick={() => void act(() => api.movePlanItem(i.id, -1))}>↑</button>
                <button className="btn" onClick={() => void act(() => api.movePlanItem(i.id, 1))}>↓</button>
                {i.status === "planned" && <ReasonButton label="阻塞" placeholder="为什么阻塞？" onSubmit={(why) => void act(() => api.setPlanStatus(i.id, "blocked", why))} />}
                {i.status === "blocked" && <button className="btn" onClick={() => void act(() => api.setPlanStatus(i.id, "planned"))}>解除阻塞</button>}
                {i.status === "done" && <button className="link" onClick={() => void act(() => api.setPlanStatus(i.id, "planned"))}>重新打开</button>}
                {["planned", "blocked"].includes(i.status) && <button className="btn danger" onClick={() => void act(() => api.setPlanStatus(i.id, "cancelled", "取消"))}>取消</button>}
              </div>
            </div>
          );
        })}
      </div>

      {editing && (
        <ItemForm initial={editing === "new" ? undefined : editing} items={view.items} members={members} onClose={() => setEditing(null)}
          onSave={async (f) => { try { if (editing === "new") await api.createPlanItem(project.id, f); else await api.updatePlanItem(editing.id, f); setEditing(null); await load(); onChanged(); } catch (e) { onError(String(e)); } }} />
      )}
    </div>
  );
}
