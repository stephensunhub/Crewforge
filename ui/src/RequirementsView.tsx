import { useCallback, useEffect, useMemo, useState } from "react";
import { api } from "./api";
import { Field, Modal } from "./common";
import type { EventRow, SpecView } from "./types";

const STATUS: Record<string, string> = { proposed: "待确认", planned: "待开始", in_progress: "进行中", done: "已完成", blocked: "已阻塞", cancelled: "已取消" };
const STAGE: Record<string, [string, string]> = {
  pending: ["待评估", "在需求池里：负责人排期时会评估（采纳 / 搁置 / 拒绝），你也可以直接处理"],
  held: ["已搁置", "暂时不做"],
  rejected: ["已拒绝", "不做"],
  writing: ["写规格中", "架构师（没有就由开发）正在写业务需求和技术方案文档"],
  to_split: ["待拆分", "规格已定稿，等负责人拆实现条目（开始自主推进后自动拆）"],
  building: ["实现中", "实现条目正在推进"],
  delivered: ["已交付", "对应的实现条目都完成了；需求变了就「修订」"],
};

/** 需求：需求池（待评估）+ 已采纳需求的规格进度。业务和技术写在同一个需求里。 */
export function RequirementsView({ projectId, events, onGoTab, onError, onChanged }: {
  projectId: string; events: EventRow[]; onGoTab: (t: "plan" | "docs" | "timeline") => void; onError: (m: string) => void; onChanged: () => void;
}) {
  const [specs, setSpecs] = useState<SpecView[]>([]);
  const [modal, setModal] = useState<"new" | { revise: SpecView } | { decide: SpecView; decision: "held" | "rejected" } | null>(null);
  const load = useCallback(async () => { try { setSpecs(await api.listSpecs(projectId)); } catch (e) { onError(String(e)); } }, [projectId, onError]);
  useEffect(() => { void load(); }, [load]);
  const last = useMemo(() => events.reduce((m, e) => (e.project_id === projectId && (e.kind.startsWith("requirement_") || e.kind.startsWith("spec_") || e.kind.startsWith("plan_") || e.kind === "task_merged") ? Math.max(m, e.id) : m), 0), [events, projectId]);
  useEffect(() => { if (last) void load(); }, [last, load]);
  const act = async (f: () => Promise<unknown>) => { try { await f(); await load(); onChanged(); return true; } catch (e) { onError(String(e)); return false; } };
  const pool = specs.filter((v) => v.stage === "pending");
  const parked = specs.filter((v) => v.stage === "held" || v.stage === "rejected");
  const live = specs.filter((v) => !["pending", "held", "rejected"].includes(v.stage));

  return (
    <div className="page">
      <div className="page-head">
        <div><h2>需求</h2>
          <div className="hint">一个需求同时包含业务面（做什么、为什么、验收标准）和技术面（怎么做、用什么、约束），写在同一份规格里：<code>docs/specs/&lt;名&gt;/requirements.md</code>（业务）和 <code>design.md</code>（技术）。采纳后先写规格，规格定稿后拆成计划条目，进排期。</div></div>
        <button className="btn primary" onClick={() => setModal("new")}>＋ 提交需求</button>
      </div>

      <section className="maint-sec">
        <h3>需求池（待评估 {pool.length}）</h3>
        {pool.length === 0 && <div className="hint maint-empty">没有待评估的需求。</div>}
        {pool.map((v) => (
          <div className="card maint-card" key={v.spec.id}>
            <div className="maint-head"><div style={{ minWidth: 0 }}><b>{v.spec.title}</b> <span className="hint">{v.spec.rtype === "tech" ? "技术改进" : "功能"} · 来源 {v.spec.source === "human" ? "人" : v.spec.source}</span></div>
              <span className="chip static" title={STAGE.pending[1]}>待评估</span></div>
            <div className="hint">{v.spec.brief}</div>
            {v.spec.human_tech && <div className="req-tech">人指定的技术要求（硬约束）：{v.spec.human_tech}</div>}
            <div className="maint-btns">
              <button className="btn primary" onClick={() => void act(() => api.adoptRequirement(v.spec.id))}>采纳，开始写规格</button>
              <button className="btn" onClick={() => setModal({ decide: v, decision: "held" })}>搁置</button>
              <button className="btn" onClick={() => setModal({ decide: v, decision: "rejected" })}>拒绝</button>
            </div>
          </div>
        ))}
      </section>

      <section className="maint-sec">
        <h3>已采纳的需求（{live.length}）</h3>
        {live.length === 0 && <div className="hint maint-empty">还没有。</div>}
        {live.map((v) => (
          <div className="card maint-card" key={v.spec.id}>
            <div className="maint-head">
              <div style={{ minWidth: 0 }}><b>{v.spec.title}</b> <span className="hint">docs/specs/{v.spec.slug}/ · 第 {v.spec.rounds} 版 · {v.spec.rtype === "tech" ? "技术改进" : "功能"}</span></div>
              <span className="chip static" title={STAGE[v.stage][1]}>{STAGE[v.stage][0]}</span>
            </div>
            <div className="hint">{v.spec.brief}</div>
            {v.spec.human_tech && <div className="req-tech">人指定的技术要求（硬约束）：{v.spec.human_tech}</div>}
            <div className="maint-items">
              {v.items.map((i) => <span key={i.id} className={`badge ps-${i.status}`} title={i.title}>{i.kind === "spec" ? "📄 " : ""}{i.title.length > 22 ? `${i.title.slice(0, 22)}…` : i.title} · {STATUS[i.status]}</span>)}
            </div>
            <div className="maint-btns">
              <button className="btn" onClick={() => onGoTab("docs")}>在「文档」里看</button>
              <button className="btn" onClick={() => onGoTab("timeline")}>在排期表里看</button>
              <button className="btn" disabled={v.stage === "writing"} title={v.stage === "writing" ? "规格还在撰写中" : ""} onClick={() => setModal({ revise: v })}>修订（需求变了）</button>
            </div>
          </div>
        ))}
      </section>

      {parked.length > 0 && (
        <section className="maint-sec">
          <h3>搁置 / 拒绝（{parked.length}）</h3>
          {parked.map((v) => <div className="maint-row" key={v.spec.id}><span className="badge ps-blocked">{STAGE[v.stage][0]}</span><span className="maint-title">{v.spec.title}</span><span className="hint">{v.spec.decision_note}</span></div>)}
        </section>
      )}

      {modal === "new" && <RequirementForm onClose={() => setModal(null)} onSubmit={async (a) => { if (await act(() => api.submitRequirement(projectId, a.title, a.brief, a.humanTech, a.rtype, a.adoptNow))) setModal(null); }} />}
      {modal && typeof modal === "object" && "revise" in modal && <ReasonForm title={`修订需求：${modal.revise.spec.title}`} label="需求有什么变化" hint="写变化的部分。会在现有的 requirements.md / design.md 上修订并追加变更记录，再只为变化的部分拆新条目。" cta="开始修订" onClose={() => setModal(null)} onSubmit={async (c) => { if (await act(() => api.reviseSpec(modal.revise.spec.id, c))) setModal(null); }} />}
      {modal && typeof modal === "object" && "decide" in modal && <ReasonForm title={`${modal.decision === "held" ? "搁置" : "拒绝"}：${modal.decide.spec.title}`} label="原因（必填）" hint="负责人和以后的你都能看到。" cta="确定" onClose={() => setModal(null)} onSubmit={async (r) => { if (await act(() => api.decideRequirement(modal.decide.spec.id, modal.decision, r))) setModal(null); }} />}
    </div>
  );
}

function RequirementForm({ onClose, onSubmit }: { onClose: () => void; onSubmit: (a: { title: string; brief: string; humanTech: string; rtype: string; adoptNow: boolean }) => Promise<void> }) {
  const [a, setA] = useState({ title: "", brief: "", humanTech: "", rtype: "feature", adoptNow: true });
  const [busy, setBusy] = useState(false);
  return (
    <Modal title="提交需求" onClose={onClose} width={640}>
      <div className="form">
        <Field label="类型"><select value={a.rtype} onChange={(e) => setA({ ...a, rtype: e.target.value })}><option value="feature">功能（有业务面和技术面）</option><option value="tech">技术改进（升级框架、清理技术债，只有技术面）</option></select></Field>
        <Field label="需求名称" hint="英文 / 拼音能得到更好读的目录名；纯中文按编号命名。"><input value={a.title} onChange={(e) => setA({ ...a, title: e.target.value })} placeholder="例如：多端登录" /></Field>
        <Field label="要解决什么问题" hint="写需求和背景就行，业务和技术混着写也可以，规格里会分开。"><textarea rows={5} value={a.brief} onChange={(e) => setA({ ...a, brief: e.target.value })} /></Field>
        <Field label="人指定的技术要求（可选）" hint="例如“必须用 sa-token 做登录鉴权”。这是硬约束：架构师和员工不能推翻，发现不可行只会升级给你。"><textarea rows={2} value={a.humanTech} onChange={(e) => setA({ ...a, humanTech: e.target.value })} /></Field>
        <label className="tog"><input type="checkbox" checked={a.adoptNow} onChange={(e) => setA({ ...a, adoptNow: e.target.checked })} />我来评估，直接采纳并开始写规格（不勾就进需求池，由负责人在排期时评估）</label>
        <div className="form-btns"><button className="btn primary" disabled={busy || !a.title.trim() || !a.brief.trim()} onClick={async () => { setBusy(true); try { await onSubmit(a); } finally { setBusy(false); } }}>提交</button></div>
      </div>
    </Modal>
  );
}

function ReasonForm({ title, label, hint, cta, onClose, onSubmit }: { title: string; label: string; hint: string; cta: string; onClose: () => void; onSubmit: (r: string) => Promise<void> }) {
  const [r, setR] = useState("");
  return (
    <Modal title={title} onClose={onClose} width={620}>
      <div className="form">
        <Field label={label} hint={hint}><textarea rows={5} value={r} onChange={(e) => setR(e.target.value)} /></Field>
        <div className="form-btns"><button className="btn primary" disabled={!r.trim()} onClick={() => void onSubmit(r)}>{cta}</button></div>
      </div>
    </Modal>
  );
}
