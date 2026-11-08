import { useState } from "react";
import { api } from "./api";
import { Field, ISSUE_KIND_LABEL, ISSUE_STATUS_LABEL, Modal, hhmm } from "./common";
import type { IssueStatus, IssueView, Member } from "./types";

const NEXT: Record<string, [IssueStatus, string][]> = {
  open: [["discussing", "开始讨论"], ["cancelled", "取消"]],
  discussing: [["pending_decision", "提交决策"], ["shelved", "搁置"], ["cancelled", "取消"]],
  pending_decision: [["discussing", "退回讨论"]],
  decided: [["implementing", "开始实施"], ["closed", "关闭"]],
  implementing: [["closed", "关闭"]],
  shelved: [["discussing", "重新打开"]],
};
const GROUPS: [string, IssueStatus[]][] = [
  ["需要处理", ["pending_decision", "open", "discussing"]],
  ["已决议 / 实施中", ["decided", "implementing"]],
  ["搁置", ["shelved"]],
  ["已结束", ["closed", "cancelled"]],
];

export function IssuesView({ projectId, views, members, onChanged, onError, onDiscuss }: {
  projectId: string;
  views: IssueView[];
  members: Member[];
  onChanged: () => void;
  onError: (m: string) => void;
  onDiscuss: (issueId: string) => void;
}) {
  const [creating, setCreating] = useState<{ supersedes?: IssueView } | null>(null);
  const [openId, setOpenId] = useState<string | null>(null);
  const open = views.find((v) => v.issue.id === openId) ?? null;

  async function guard(fn: () => Promise<unknown>) {
    try { await fn(); onChanged(); return true; } catch (e) { onError(String(e)); return false; }
  }

  return (
    <div className="page">
      <div className="page-head">
        <div><h2>议题板</h2><div className="hint">需要讨论并决定的事项。解决后沉淀为决策记录；被推翻的决议不会被修改，而是新建议题取代它。</div></div>
        <button className="btn primary" onClick={() => setCreating({})}>＋ 新议题</button>
      </div>
      {views.length === 0 && <div className="hint">还没有议题。遇到需要团队讨论、需要有人拍板的事，就在这里记下来。</div>}
      {GROUPS.map(([label, sts]) => {
        const list = views.filter((v) => sts.includes(v.issue.status));
        if (list.length === 0) return null;
        return (
          <section key={label} style={{ marginBottom: 22 }}>
            <h4 className="group-title">{label} · {list.length}</h4>
            <div className="cards">
              {list.map((v) => (
                <div className="card clickable" key={v.issue.id} onClick={() => setOpenId(v.issue.id)}>
                  <div className="card-top">
                    <div style={{ minWidth: 0 }}><b>{v.issue.title}</b>
                      <div className="hint">{ISSUE_KIND_LABEL[v.issue.kind]}{v.issue.high_risk ? " · 高风险" : ""}</div></div>
                    <span className={`badge st-${v.issue.status}`}>{ISSUE_STATUS_LABEL[v.issue.status]}</span>
                  </div>
                  {v.issue.summary && <div className="card-persona">{v.issue.summary}</div>}
                  <div className="card-meta">
                    <span className="chip static">{v.options.length} 个方案</span>
                    <span className="chip static">拍板：{v.decider}</span>
                    {v.issue.status === "pending_decision" && <span className="chip static" style={{ color: "var(--warn)" }}>等待裁决</span>}
                  </div>
                  {v.issue.decision && <div className="note ok">决议：{v.issue.decision}</div>}
                  {v.superseded_by && <div className="note warn">已被《{v.superseded_by[1]}》取代</div>}
                </div>
              ))}
            </div>
          </section>
        );
      })}

      {creating && (
        <NewIssue members={members} supersedes={creating.supersedes} onClose={() => setCreating(null)}
          onCreate={async (a) => { if (await guard(() => api.createIssue({ projectId, ...a }))) setCreating(null); }} />
      )}
      {open && (
        <IssueDetail v={open} members={members} onClose={() => setOpenId(null)} guard={guard} onDiscuss={() => { onDiscuss(open.issue.id); setOpenId(null); }}
          onSupersede={() => { setCreating({ supersedes: open }); setOpenId(null); }} />
      )}
    </div>
  );
}

function NewIssue({ members, supersedes, onCreate, onClose }: {
  members: Member[]; supersedes?: IssueView; onClose: () => void;
  onCreate: (a: { title: string; summary: string; kind: string; highRisk: boolean; ownerId: string | null; supersedes: string | null }) => Promise<void>;
}) {
  const [title, setTitle] = useState(supersedes ? `重新讨论：${supersedes.issue.title}` : "");
  const [summary, setSummary] = useState("");
  const [kind, setKind] = useState(supersedes?.issue.kind ?? "technical");
  const [risk, setRisk] = useState(false);
  const [owner, setOwner] = useState("");
  const humanOnly = risk || kind === "requirement";
  return (
    <Modal title={supersedes ? "新建议题，取代已有决议" : "新议题"} onClose={onClose}>
      <div className="form">
        {supersedes && <div className="note warn">将取代《{supersedes.issue.title}》（当前决议：{supersedes.issue.decision}）。原记录会完整保留。</div>}
        <Field label="标题"><input value={title} autoFocus onChange={(e) => setTitle(e.target.value)} /></Field>
        <Field label="问题摘要" hint="一两句话说清楚要决定什么、为什么要决定。"><textarea rows={3} value={summary} onChange={(e) => setSummary(e.target.value)} /></Field>
        <Field label="类型" hint={`谁来拍板：${humanOnly ? "仅人类" : kind === "implementation" ? "议题负责人或人类" : "项目负责人或人类"}`}>
          <select value={kind} onChange={(e) => setKind(e.target.value as typeof kind)}>
            {Object.entries(ISSUE_KIND_LABEL).map(([k, l]) => <option key={k} value={k}>{l}</option>)}
          </select>
        </Field>
        <label className="tog"><input type="checkbox" checked={risk} onChange={(e) => setRisk(e.target.checked)} />
          涉及资金、生产环境、不可逆操作或安全（只能由人类拍板）</label>
        <Field label="议题负责人（推动解决的人，可选）">
          <select value={owner} onChange={(e) => setOwner(e.target.value)}>
            <option value="">未指定</option>
            {members.map((m) => <option key={m.employee_id} value={m.employee_id}>{m.name}</option>)}
          </select>
        </Field>
        <div className="form-btns">
          <button className="btn primary" disabled={!title.trim()} onClick={() => void onCreate({ title, summary, kind, highRisk: risk, ownerId: owner || null, supersedes: supersedes?.issue.id ?? null })}>创建</button>
        </div>
      </div>
    </Modal>
  );
}

function IssueDetail({ v, members, onClose, guard, onDiscuss, onSupersede }: {
  v: IssueView; members: Member[]; onClose: () => void; onDiscuss: () => void; onSupersede: () => void;
  guard: (fn: () => Promise<unknown>) => Promise<boolean>;
}) {
  const i = v.issue;
  const [optTitle, setOptTitle] = useState("");
  const [optDesc, setOptDesc] = useState("");
  const [deciding, setDeciding] = useState(false);
  const nameOf = (id: string | null) => (id ? members.find((m) => m.employee_id === id)?.name ?? "（已离开项目）" : "人类");
  const editable = ["open", "discussing", "pending_decision"].includes(i.status);

  return (
    <Modal title={i.title} onClose={onClose} width={680}>
      <div className="form">
        <div className="card-meta">
          <span className={`badge st-${i.status}`}>{ISSUE_STATUS_LABEL[i.status]}</span>
          <span className="chip static">{ISSUE_KIND_LABEL[i.kind]}{i.high_risk ? " · 高风险" : ""}</span>
          <span className="chip static">提出：{v.raised_by_name ?? "人类"}</span>
          <span className="chip static">负责人：{v.owner_name ?? "未指定"}</span>
          <span className="chip static">拍板：{v.decider}</span>
        </div>
        {v.supersedes_title && <div className="note warn">取代了：《{v.supersedes_title}》</div>}
        {v.superseded_by && <div className="note warn">已被《{v.superseded_by[1]}》取代</div>}
        {i.summary && <div style={{ whiteSpace: "pre-wrap" }}>{i.summary}</div>}

        <div>
          <h4 className="group-title">候选方案</h4>
          {v.options.length === 0 && <div className="hint">还没有方案。</div>}
          {v.options.map((o) => (
            <div className={`option ${o.status}`} key={o.id}>
              <div><b>{o.title}</b>{o.status === "chosen" && <span className="badge merged" style={{ marginLeft: 8 }}>已采纳</span>}
                {o.status === "rejected" && <span className="badge abandoned" style={{ marginLeft: 8 }}>未采纳</span>}</div>
              {o.description && <div className="hint">{o.description}</div>}
              <div className="hint">提出：{nameOf(o.proposed_by)}</div>
              {o.reject_reason && <div className="hint">未采纳原因：{o.reject_reason}</div>}
            </div>
          ))}
          {editable && (
            <div className="option-add">
              <input placeholder="方案标题" value={optTitle} onChange={(e) => setOptTitle(e.target.value)} />
              <input placeholder="说明（可选）" value={optDesc} onChange={(e) => setOptDesc(e.target.value)} />
              <button className="btn" disabled={!optTitle.trim()} onClick={async () => {
                if (await guard(() => api.addOption(i.id, optTitle, optDesc))) { setOptTitle(""); setOptDesc(""); }
              }}>添加方案</button>
            </div>
          )}
        </div>

        {i.decision && (
          <div className="note ok" style={{ whiteSpace: "pre-wrap" }}>
            <b>决议：</b>{i.decision}{i.decision_reason ? `\n原因：${i.decision_reason}` : ""}
            {`\n拍板：${i.decided_by === "human" ? "人类" : nameOf(i.decided_by)} · ${i.decided_at ? hhmm(i.decided_at) : ""}`}
          </div>
        )}

        <div className="form-btns" style={{ justifyContent: "flex-start", flexWrap: "wrap" }}>
          {i.status === "pending_decision" && <button className="btn primary" onClick={() => setDeciding(true)}>拍板</button>}
          {(NEXT[i.status] ?? []).map(([to, label]) => (
            <button key={to} className="btn" onClick={async () => { if (await guard(() => api.setIssueStatus(i.id, to))) onClose(); }}>{label}</button>
          ))}
          {editable && <button className="btn" onClick={onDiscuss}>发起会议讨论</button>}
          {["decided", "implementing", "closed"].includes(i.status) && !v.superseded_by && <button className="btn" onClick={onSupersede}>推翻并重新讨论</button>}
        </div>
      </div>
      {deciding && <DecideModal v={v} onClose={() => setDeciding(false)}
        onDecide={async (c, d, r, rej) => { if (await guard(() => api.decideIssue(i.id, c, d, r, rej))) { setDeciding(false); onClose(); } }} />}
    </Modal>
  );
}

function DecideModal({ v, onClose, onDecide }: {
  v: IssueView; onClose: () => void;
  onDecide: (chosen: string | null, decision: string, reason: string, rejected: Record<string, string>) => Promise<void>;
}) {
  const [chosen, setChosen] = useState<string | null>(v.options[0]?.id ?? null);
  const [decision, setDecision] = useState(v.options[0] ? `采用「${v.options[0].title}」` : "");
  const [reason, setReason] = useState("");
  const [rej, setRej] = useState<Record<string, string>>({});
  return (
    <Modal title={`拍板：${v.issue.title}`} onClose={onClose} width={560}>
      <div className="form">
        {v.options.length > 0 && (
          <Field label="采纳哪个方案">
            {v.options.map((o) => (
              <label className="tog" key={o.id}>
                <input type="radio" checked={chosen === o.id} onChange={() => { setChosen(o.id); setDecision(`采用「${o.title}」`); }} />{o.title}
              </label>
            ))}
            <label className="tog"><input type="radio" checked={chosen === null} onChange={() => setChosen(null)} />都不采纳（自行填写决定）</label>
          </Field>
        )}
        <Field label="最终决定"><textarea rows={2} value={decision} onChange={(e) => setDecision(e.target.value)} /></Field>
        <Field label="决议原因" hint="这条会保留为决策记录，写清楚为什么。"><textarea rows={3} value={reason} onChange={(e) => setReason(e.target.value)} /></Field>
        {v.options.filter((o) => o.id !== chosen).map((o) => (
          <Field key={o.id} label={`「${o.title}」为什么没被采纳`}>
            <input value={rej[o.id] ?? ""} placeholder="未被采纳" onChange={(e) => setRej({ ...rej, [o.id]: e.target.value })} />
          </Field>
        ))}
        <div className="form-btns"><button className="btn primary" disabled={!decision.trim()} onClick={() => void onDecide(chosen, decision, reason, rej)}>确认决议</button></div>
      </div>
    </Modal>
  );
}
