import { useCallback, useEffect, useMemo, useState } from "react";
import { api } from "./api";
import { Field, MEMORY_KIND_LABEL, MEMORY_SOURCE_LABEL, MEMORY_STATUS_LABEL, Modal, hhmm } from "./common";
import type { EventRow, Member, Memory, MemoryLayer } from "./types";

export type MemoryScope = { layer: "company" } | { layer: "project"; projectId: string } | { layer: "employee"; employeeId: string; name: string };

const LAYER_HINT: Record<MemoryLayer, string> = {
  company: "跨项目通用的经验与习惯，所有项目的员工开始任务时都可能检索到。",
  project: "这个项目的架构、约定、决策和踩过的坑，项目成员共享。会议纪要里的经验先进入“待验证”。",
  employee: "只属于这名员工的长期经验，别的员工检索不到。员工可以学习已验证的项目经验。",
  task: "",
};

export function MemoryView({ scope, events, members, onError, onChanged, embedded, canCurate }: {
  canCurate?: boolean;
  scope: MemoryScope;
  events: EventRow[];
  members?: Member[];
  onError: (m: string) => void;
  onChanged?: () => void;
  embedded?: boolean;
}) {
  const [items, setItems] = useState<Memory[]>([]);
  const [status, setStatus] = useState<string>("");
  const [query, setQuery] = useState("");
  const [editing, setEditing] = useState<Memory | "new" | null>(null);
  const [curating, setCurating] = useState(false);
  const [curNote, setCurNote] = useState("");
  const [learning, setLearning] = useState<Memory | null>(null);
  const [preview, setPreview] = useState<{ emp: string; text: string; out: Memory[] | null }>({ emp: members?.[0]?.employee_id ?? "", text: "", out: null });

  const load = useCallback(async () => {
    try {
      setItems(await api.memories({
        layer: scope.layer,
        projectId: scope.layer === "project" ? scope.projectId : undefined,
        employeeId: scope.layer === "employee" ? scope.employeeId : undefined,
        status: status || undefined, query: query || undefined,
      }));
    } catch (e) { onError(String(e)); }
  }, [scope, status, query, onError]);
  useEffect(() => { void load(); }, [load]);

  // 记忆有变化（含合并后自动验证）时刷新
  const lastMem = useMemo(() => events.reduce((m, e) => (e.kind.startsWith("memory_") && e.kind !== "memory_injected" ? Math.max(m, e.id) : m), 0), [events]);
  useEffect(() => { if (lastMem) void load(); }, [lastMem, load]);

  async function act(fn: () => Promise<unknown>) {
    try { await fn(); await load(); onChanged?.(); return true; } catch (e) { onError(String(e)); return false; }
  }
  const pending = items.filter((m) => m.status === "unverified");

  return (
    <div className={embedded ? "" : "page"}>
      <div className="page-head">
        <div>
          <h2>{scope.layer === "company" ? "公司记忆" : scope.layer === "project" ? "项目记忆" : `${scope.name} 的个人记忆`}</h2>
          <div className="hint">{LAYER_HINT[scope.layer]}</div>
        </div>
        <div style={{ display: "flex", gap: 8 }}>
          {canCurate && scope.layer === "project" && (
            <button className="btn" disabled={curating} title="负责人逐条核实待验证的经验：能核实的验证，过时的让它过期，重复的合并；人写的和议题决议沉淀的不碰。一次模型调用。" onClick={async () => {
              setCurating(true); setCurNote("");
              try {
                const c = await api.leadCurateMemory(scope.projectId!);
                const parts = [c.verified.length && `验证 ${c.verified.length} 条`, c.expired.length && `让 ${c.expired.length} 条过期`, ...c.merged].filter(Boolean);
                setCurNote(`${parts.length ? parts.join("；") : c.summary || "没有改动（拿不准的留给你）"}${c.rejected.length ? `；${c.rejected.length} 项被拒绝` : ""}${c.escalate ? `；需要你判断：${c.escalate}` : ""}`);
                onChanged?.();
              } catch (e) { onError(String(e)); } finally { setCurating(false); }
            }}>{curating ? "负责人整理中…" : "让负责人整理记忆"}</button>
          )}
          <button className="btn primary" onClick={() => setEditing("new")}>＋ 写入记忆</button>
        </div>
      </div>
      {curNote && <div className="note" style={{ marginBottom: 10 }}>负责人整理：{curNote}</div>}

      <div className="mem-filters">
        <input placeholder="搜索记忆" value={query} onChange={(e) => setQuery(e.target.value)} />
        <select value={status} onChange={(e) => setStatus(e.target.value)}>
          <option value="">全部状态</option><option value="unverified">待验证</option><option value="verified">已验证</option><option value="expired">已过期</option>
        </select>
      </div>

      {pending.length > 0 && !status && (
        <div className="note warn" style={{ marginBottom: 12 }}>
          有 {pending.length} 条经验等待验证。<b>只有已验证的记忆才会被注入给员工</b>；关联到任务的经验会在该任务合并后自动验证。
        </div>
      )}
      {items.length === 0 && <div className="hint">还没有记忆。你可以手写一条，或在会议纪要里把“待验证的经验”存进来。</div>}
      <div className="mem-list">
        {items.map((m) => (
          <div className={`mem ${m.status}`} key={m.id}>
            <div className="mem-top">
              <b>{m.title}</b>
              <span className="chip static">{MEMORY_KIND_LABEL[m.kind]}</span>
              <span className={`badge mst-${m.status}`}>{MEMORY_STATUS_LABEL[m.status]}</span>
            </div>
            <div className="mem-body">{m.content}</div>
            <div className="mem-meta hint">
              来源：{MEMORY_SOURCE_LABEL[m.source_kind]}
              {m.scope && ` · 适用：${m.scope}`}
              {m.related_commit && <> · 提交 <code>{m.related_commit.slice(0, 8)}</code></>}
              {m.status === "verified" && m.verified_by && ` · 由${m.verified_by === "human" ? "人类" : m.verified_by === "merge" ? "任务合并" : m.verified_by}验证 ${m.verified_at ? hhmm(m.verified_at) : ""}`}
              {m.status === "expired" && ` · 过期原因：${m.expired_reason || "未注明"}`}
              {` · 已被注入 ${m.uses} 次`}
            </div>
            <div className="mem-btns">
              {m.status === "unverified" && <button className="btn primary" onClick={() => void act(() => api.verifyMemory(m.id))}>验证</button>}
              {m.status === "unverified" && (
                <button className="btn danger" onClick={async () => { if (await api.confirm(`删除候选「${m.title}」？`, "确认删除")) await act(() => api.deleteMemory(m.id)); }}>否决</button>
              )}
              {m.status !== "expired" && <button className="btn" onClick={() => setEditing(m)}>编辑</button>}
              {m.status === "verified" && (
                <button className="btn" onClick={() => {
                  const reason = window.prompt("为什么过期？（例如：方案已被推翻）", "");
                  if (reason !== null) void act(() => api.expireMemory(m.id, reason));
                }}>标记过期</button>
              )}
              {m.status === "verified" && m.layer === "project" && members && members.length > 0 && <button className="btn" onClick={() => setLearning(m)}>让员工学习</button>}
            </div>
          </div>
        ))}
      </div>

      {scope.layer === "project" && members && members.length > 0 && (
        <div className="preview">
          <h4 className="group-title">检索预览</h4>
          <div className="hint">输入一段任务描述，看这名员工开始任务时会被注入哪些记忆（不会记录使用次数）。</div>
          <div className="row" style={{ marginTop: 8 }}>
            <select style={{ width: 140 }} value={preview.emp} onChange={(e) => setPreview({ ...preview, emp: e.target.value, out: null })}>
              {members.map((m) => <option key={m.employee_id} value={m.employee_id}>{m.name}</option>)}
            </select>
            <input value={preview.text} placeholder="例如：给订单接口补集成测试" onChange={(e) => setPreview({ ...preview, text: e.target.value, out: null })} />
            <button className="btn" disabled={!preview.text.trim() || !preview.emp} onClick={async () => {
              try { setPreview({ ...preview, out: await api.previewRetrieval(scope.projectId, preview.emp, preview.text) }); } catch (e) { onError(String(e)); }
            }}>预览</button>
          </div>
          {preview.out && (preview.out.length === 0
            ? <div className="hint" style={{ marginTop: 8 }}>没有相关记忆会被注入。</div>
            : preview.out.map((m) => <div key={m.id} className="note ok" style={{ marginTop: 6 }}>[{m.layer === "company" ? "公司" : m.layer === "project" ? "项目" : "个人"}] {m.title}：{m.content.slice(0, 80)}</div>))}
        </div>
      )}

      {editing && (
        <MemoryForm scope={scope} initial={editing === "new" ? undefined : editing} onClose={() => setEditing(null)}
          onSave={async (a) => {
            const ok = await act(() => editing === "new"
              ? api.addMemory({ layer: scope.layer, projectId: scope.layer === "project" ? scope.projectId : null, employeeId: scope.layer === "employee" ? scope.employeeId : null, ...a })
              : api.updateMemory(editing.id, a.title, a.content, a.scope));
            if (ok) setEditing(null);
          }} />
      )}
      {learning && members && (
        <Modal title={`让员工学习：${learning.title}`} onClose={() => setLearning(null)} width={420}>
          <div className="form">
            <div className="hint">学习后会复制为 TA 的个人记忆（保留来源），之后只有 TA 自己检索得到。</div>
            {members.map((m) => (
              <button key={m.employee_id} className="btn" onClick={async () => { if (await act(() => api.promoteMemory(learning.id, m.employee_id))) setLearning(null); }}>{m.name}</button>
            ))}
          </div>
        </Modal>
      )}
    </div>
  );
}

function MemoryForm({ scope, initial, onSave, onClose }: {
  scope: MemoryScope; initial?: Memory; onClose: () => void;
  onSave: (a: { kind: string; title: string; content: string; scope: string }) => Promise<void>;
}) {
  const [kind, setKind] = useState(initial?.kind ?? "lesson");
  const [title, setTitle] = useState(initial?.title ?? "");
  const [content, setContent] = useState(initial?.content ?? "");
  const [sc, setSc] = useState(initial?.scope ?? "");
  return (
    <Modal title={initial ? "编辑记忆" : "写入记忆"} onClose={onClose} width={560}>
      <div className="form">
        {!initial && <div className="hint">你手写的记忆会直接标为“已验证”，之后相关任务开始时会被注入。{scope.layer === "employee" ? "" : ""}</div>}
        {!initial && (
          <Field label="类型">
            <select value={kind} onChange={(e) => setKind(e.target.value as typeof kind)}>
              {Object.entries(MEMORY_KIND_LABEL).map(([k, l]) => <option key={k} value={k}>{l}</option>)}
            </select>
          </Field>
        )}
        <Field label="标题" hint="留空会取内容的第一行。"><input value={title} onChange={(e) => setTitle(e.target.value)} /></Field>
        <Field label="内容" hint="写成可以直接照做的一两句话；越具体，检索越准。"><textarea rows={5} value={content} onChange={(e) => setContent(e.target.value)} /></Field>
        <Field label="适用范围（可选）" hint="例如：仅限后端服务、仅限 Rust 项目。"><input value={sc} onChange={(e) => setSc(e.target.value)} /></Field>
        <div className="form-btns"><button className="btn primary" disabled={!content.trim() || (!!initial && !title.trim())} onClick={() => void onSave({ kind, title: title.trim() || content.trim().split("\n")[0].slice(0, 40), content, scope: sc })}>保存</button></div>
      </div>
    </Modal>
  );
}
