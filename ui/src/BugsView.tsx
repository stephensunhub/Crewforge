import { Fragment, useCallback, useEffect, useMemo, useState } from "react";
import { api } from "./api";
import { Field, Modal } from "./common";
import type { Bug, EventRow, Member, PlanItem } from "./types";

const SEVERITY: [string, string][] = [["blocker", "阻断：线上不可用 / 数据错误，要马上修"], ["major", "严重：主要功能受影响"], ["minor", "一般：有影响但有绕过办法"]];
const SEV_LABEL: Record<string, string> = { blocker: "阻断", major: "严重", minor: "一般" };
const STATUS: Record<string, string> = { open: "待修", fixing: "修复中", verify: "待复测", closed: "已关闭", cannot_repro: "无法复现", duplicate: "重复" };
const ITEM_STATUS: Record<string, string> = { proposed: "待确认", planned: "待开始", in_progress: "进行中", done: "已完成", blocked: "已阻塞", cancelled: "已取消" };
const SOURCE: Record<string, string> = { human: "人", qa: "QA", employee: "员工" };
const when = (ms: number) => new Date(ms).toLocaleString("zh-CN", { month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit" });

/** Bug 栏目：QA 在测试里记、人也可以加；谁开发的谁修，修完复测，复测通过才关闭。 */
export function BugsView({ projectId, members, events, onGoTab, onError, onChanged }: {
  projectId: string; members: Member[]; events: EventRow[]; onGoTab: (t: "timeline" | "plan") => void; onError: (m: string) => void; onChanged: () => void;
}) {
  const [bugs, setBugs] = useState<Bug[]>([]);
  const [items, setItems] = useState<PlanItem[]>([]);
  const [filter, setFilter] = useState<"active" | "all">("active");
  const [modal, setModal] = useState<"new" | { resolve: Bug } | { reopen: Bug } | null>(null);
  const [open, setOpen] = useState<string | null>(null);
  const load = useCallback(async () => {
    try { setBugs(await api.listBugs(projectId)); setItems((await api.planView(projectId)).items); } catch (e) { onError(String(e)); }
  }, [projectId, onError]);
  useEffect(() => { void load(); }, [load]);
  const last = useMemo(() => events.reduce((m, e) => (e.project_id === projectId && (e.kind.startsWith("bug_") || e.kind.startsWith("plan_") || e.kind === "task_merged") ? Math.max(m, e.id) : m), 0), [events, projectId]);
  useEffect(() => { if (last) void load(); }, [last, load]);
  const act = async (f: () => Promise<unknown>) => { try { await f(); await load(); onChanged(); return true; } catch (e) { onError(String(e)); return false; } };
  const shown = bugs.filter((b) => filter === "all" || ["open", "fixing", "verify"].includes(b.status));
  const retester = members.some((m) => m.role === "qa") ? "QA" : "负责人";

  return (
    <div className="page">
      <div className="page-head">
        <div><h2>Bug</h2>
          <div className="hint">QA 在测试时记，你也可以手动添加。默认由开发了出问题那个条目的人修；修好后由{retester}复测，复测通过才关闭。优先级默认按严重程度（阻断 P0、严重 P1、一般 P2），由负责人在排期时调整。</div></div>
        <button className="btn primary" onClick={() => setModal("new")}>＋ 添加 bug</button>
      </div>
      <div className="maint-btns" style={{ marginBottom: 10 }}>
        <button className={`chip ${filter === "active" ? "on" : ""}`} onClick={() => setFilter("active")}>未结束（{bugs.filter((b) => ["open", "fixing", "verify"].includes(b.status)).length}）</button>
        <button className={`chip ${filter === "all" ? "on" : ""}`} onClick={() => setFilter("all")}>全部（{bugs.length}）</button>
        <a className="link" onClick={() => onGoTab("timeline")}>去排期表看它们排在哪 →</a>
      </div>
      {shown.length === 0 && <div className="hint maint-empty">{bugs.length === 0 ? "还没有 bug。QA 测试发现问题时会自己记到这里。" : "没有未结束的 bug。"}</div>}
      <table className="tbl">
        <thead><tr><th>编号</th><th>优先级</th><th>标题</th><th>严重</th><th>状态</th><th>责任开发</th><th>修复 / 复测</th><th>来源</th><th></th></tr></thead>
        <tbody>
          {shown.map((b) => (
            <Fragment key={b.id}>
              <tr className="clickable" onClick={() => setOpen(open === b.id ? null : b.id)}>
                <td>BUG-{b.seq}</td>
                <td onClick={(e) => e.stopPropagation()}>
                  <select value={b.priority} disabled={!["open", "fixing"].includes(b.status)} onChange={(e) => void act(() => api.setBugPriority(b.id, Number(e.target.value)))}>
                    {[0, 1, 2, 3].map((p) => <option key={p} value={p}>P{p}</option>)}
                  </select>
                </td>
                <td className="tbl-title" title={b.title}>{b.title}</td>
                <td>{SEV_LABEL[b.severity] ?? b.severity}</td>
                <td><span className={`badge bs-${b.status}`}>{STATUS[b.status]}</span></td>
                <td onClick={(e) => e.stopPropagation()}>
                  <select value={b.owner_id ?? ""} disabled={!["open"].includes(b.status)} onChange={(e) => e.target.value && void act(() => api.setBugOwner(b.id, e.target.value))}>
                    <option value="">{b.owner_id ? "" : "未指定（排期时由负责人指派）"}</option>
                    {members.filter((m) => m.role !== "architect").map((m) => <option key={m.employee_id} value={m.employee_id}>{m.name}</option>)}
                  </select>
                </td>
                <td className="hint">{b.fix_status ? ITEM_STATUS[b.fix_status] : "—"} / {b.retest_status ? ITEM_STATUS[b.retest_status] : "—"}</td>
                <td className="hint">{SOURCE[b.source] ?? b.source}{b.reporter_name ? `（${b.reporter_name}）` : ""}</td>
                <td onClick={(e) => e.stopPropagation()}>
                  <div className="maint-btns">
                    {["verify", "closed"].includes(b.status) && <button className="btn" onClick={() => setModal({ reopen: b })}>没修好，重开</button>}
                    {["open", "fixing"].includes(b.status) && <button className="btn" onClick={() => setModal({ resolve: b })}>无法复现 / 重复</button>}
                  </div>
                </td>
              </tr>
              {open === b.id && (
                <tr key={`${b.id}-d`}><td colSpan={9}>
                  <div className="bug-detail">
                    <div className="hint">{when(b.created_at)} 记录{b.origin_title ? ` · 出自条目《${b.origin_title}》` : " · 没写出处"}{b.resolution ? ` · 结论：${b.resolution}` : ""}</div>
                    {([["复现步骤", b.steps], ["期望行为", b.expected], ["实际行为", b.actual], ["不应改变的行为", b.unchanged], ["补充信息", b.extra]] as [string, string][]).filter(([, v]) => v.trim()).map(([k, v]) => <div key={k}><b>{k}：</b>{v}</div>)}
                  </div>
                </td></tr>
              )}
            </Fragment>
          ))}
        </tbody>
      </table>
      {modal === "new" && <BugForm items={items} members={members} onClose={() => setModal(null)} onSubmit={async (b) => { if (await act(() => api.reportBug(projectId, b))) setModal(null); }} />}
      {modal && typeof modal === "object" && "resolve" in modal && <ResolveForm bug={modal.resolve} onClose={() => setModal(null)} onSubmit={async (st, r) => { if (await act(() => api.resolveBug(modal.resolve.id, st, r))) setModal(null); }} />}
      {modal && typeof modal === "object" && "reopen" in modal && <ReasonForm title={`重开 BUG-${modal.reopen.seq}`} label="复测发现了什么" hint="步骤和现象。会再安排一次修复和复测。" onClose={() => setModal(null)} onSubmit={async (r) => { if (await act(() => api.reopenBug(modal.reopen.id, r))) setModal(null); }} />}
    </div>
  );
}

function BugForm({ items, members, onClose, onSubmit }: { items: PlanItem[]; members: Member[]; onClose: () => void; onSubmit: (b: { title: string; steps: string; expected: string; actual: string; unchanged: string; severity: string; extra: string; origin_item_id: string | null; owner_id: string | null }) => Promise<void> }) {
  const [b, setB] = useState({ title: "", steps: "", expected: "", actual: "", unchanged: "", severity: "major", extra: "", origin: "", owner: "" });
  const [busy, setBusy] = useState(false);
  const set = (k: keyof typeof b) => (e: { target: { value: string } }) => setB((x) => ({ ...x, [k]: e.target.value }));
  const origin = items.find((i) => i.id === b.origin);
  return (
    <Modal title="添加 bug" onClose={onClose} width={640}>
      <div className="form">
        <Field label="一句话说明问题"><input value={b.title} onChange={set("title")} placeholder="例如：导出报表时遇到空列表会闪退" /></Field>
        <Field label="严重程度"><select value={b.severity} onChange={set("severity")}>{SEVERITY.map(([k, v]) => <option key={k} value={k}>{v}</option>)}</select></Field>
        <Field label="复现步骤" hint="越具体越好：点哪里、输入什么。不知道怎么复现就写下你看到的现象。"><textarea rows={3} value={b.steps} onChange={set("steps")} /></Field>
        <Field label="期望的行为"><textarea rows={2} value={b.expected} onChange={set("expected")} /></Field>
        <Field label="实际的行为"><textarea rows={2} value={b.actual} onChange={set("actual")} /></Field>
        <Field label="不应改变的行为（可选）" hint="修这个 bug 时哪些功能不能被影响。会成为复测的检查范围。"><textarea rows={2} value={b.unchanged} onChange={set("unchanged")} /></Field>
        <Field label="出自哪个条目（可选）" hint={origin ? "默认由做这个条目的人修。" : "选了才能自动指派给原开发；不选就由负责人在排期时指派。"}>
          <select value={b.origin} onChange={set("origin")}><option value="">不确定</option>
            {items.filter((i) => i.status !== "cancelled" && i.kind !== "bugfix" && i.kind !== "retest").map((i) => <option key={i.id} value={i.id}>{i.title}{i.assignee_name ? `（${i.assignee_name}）` : ""}</option>)}</select>
        </Field>
        <Field label="指定责任开发（可选）"><select value={b.owner} onChange={set("owner")}><option value="">按出处条目的开发</option>{members.filter((m) => m.role !== "architect").map((m) => <option key={m.employee_id} value={m.employee_id}>{m.name}</option>)}</select></Field>
        <Field label="补充信息（可选）" hint="版本、环境、报错日志。"><textarea rows={2} value={b.extra} onChange={set("extra")} /></Field>
        <div className="form-btns"><button className="btn primary" disabled={busy || !b.title.trim()} onClick={async () => { setBusy(true); try { await onSubmit({ ...b, origin_item_id: b.origin || null, owner_id: b.owner || null }); } finally { setBusy(false); } }}>提交</button></div>
      </div>
    </Modal>
  );
}

function ResolveForm({ bug, onClose, onSubmit }: { bug: Bug; onClose: () => void; onSubmit: (status: string, reason: string) => Promise<void> }) {
  const [st, setSt] = useState("cannot_repro");
  const [r, setR] = useState("");
  return (
    <Modal title={`结束 BUG-${bug.seq}（不修）`} onClose={onClose}>
      <div className="form">
        <Field label="结论"><select value={st} onChange={(e) => setSt(e.target.value)}><option value="cannot_repro">无法复现</option><option value="duplicate">重复（和另一个 bug 是同一个问题）</option></select></Field>
        <Field label="原因（必填）" hint="重复的写明和哪个 bug 重复。还没做的修复、复测条目会一并取消。"><textarea rows={3} value={r} onChange={(e) => setR(e.target.value)} /></Field>
        <div className="form-btns"><button className="btn primary" disabled={!r.trim()} onClick={() => void onSubmit(st, r)}>确定</button></div>
      </div>
    </Modal>
  );
}

function ReasonForm({ title, label, hint, onClose, onSubmit }: { title: string; label: string; hint: string; onClose: () => void; onSubmit: (r: string) => Promise<void> }) {
  const [r, setR] = useState("");
  return (
    <Modal title={title} onClose={onClose}>
      <div className="form">
        <Field label={label} hint={hint}><textarea rows={4} value={r} onChange={(e) => setR(e.target.value)} /></Field>
        <div className="form-btns"><button className="btn primary" disabled={!r.trim()} onClick={() => void onSubmit(r)}>确定</button></div>
      </div>
    </Modal>
  );
}
