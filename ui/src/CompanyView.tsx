import { EndedProjects } from "./EndProject";
import { AlignOverview } from "./AlignBox";
import { useEffect, useState } from "react";
import { api } from "./api";
import { Avatar, Field, Modal, Placeholder, basename } from "./common";
import { MemoryView } from "./MemoryView";
import type { Company, Endpoint, EmployeeCard, EventRow, ModelsOverview, ProjectSummary } from "./types";

export type CompanyTab = "employees" | "projects" | "memory" | "docs" | "manage";
export const COMPANY_TABS: [CompanyTab, string][] = [
  ["employees", "员工"], ["projects", "项目"], ["memory", "记忆"], ["docs", "文档"], ["manage", "管理"],
];

const ROLES = ["全栈工程师", "QA 测试工程师", "架构师 / CTO", "其他"];

function EmployeeForm({ initial, onSubmit, onClose }: {
  initial?: { name: string; role: string; persona: string; provider?: string; model?: string; endpointId?: string | null };
  onSubmit: (name: string, role: string, persona: string, provider: string, model: string, endpointId: string) => Promise<void>;
  onClose: () => void;
}) {
  const [name, setName] = useState(initial?.name ?? "");
  const [role, setRole] = useState(initial?.role ?? ROLES[0]);
  const [persona, setPersona] = useState(initial?.persona ?? "");
  const [provider, setProvider] = useState(initial?.provider ?? "claude");
  const [model, setModel] = useState(initial?.model ?? "");
  const [busy, setBusy] = useState(false);
  const [cat, setCat] = useState<ModelsOverview | null>(null);
  const [eps, setEps] = useState<Endpoint[]>([]);
  const [epId, setEpId] = useState(initial?.endpointId ?? "");
  useEffect(() => { void api.modelsOverview().then(setCat).catch(() => setCat(null)); void api.listEndpoints().then(setEps).catch(() => setEps([])); }, []);
  const ep = provider === "claude" ? eps.find((x) => x.id === epId) : undefined;
  const enabledProv = (p: string) => !cat || (cat.providers.find((x) => x.provider === p)?.enabled ?? true) || initial?.provider === p;
  const valid = name.trim() !== "" && !/[\s@]/.test(name);
  return (
    <Modal title={initial ? `编辑 ${initial.name}` : "新员工"} onClose={onClose}>
      <div className="form">
        <Field label="名称" hint={initial ? "名称不可修改（它决定工作区目录和分支名）" : "不含空格和 @，创建后不可修改"}>
          <input value={name} disabled={!!initial} onChange={(e) => setName(e.target.value)} autoFocus={!initial} />
        </Field>
        <Field label="岗位">
          <select value={role} onChange={(e) => setRole(e.target.value)}>
            {[...new Set([role, ...ROLES])].map((r) => <option key={r}>{r}</option>)}
          </select>
        </Field>
        <Field label="执行引擎" hint={initial
          ? (provider !== initial.provider ? "⚠ 切换后 TA 之前的对话上下文不会带过去（任务分支、工作区、历史记录和记忆都保留）；TA 正在工作时不能切换。" : "可以随时切换，下次执行时生效。")
          : "Claude：可以边做边向你请求权限，按订阅额度计量。Copilot：权限在启动时一次定好（没有列入的命令会被直接拒绝，不会来问你），按 Copilot 高级请求计量。"}>
          <select value={provider} onChange={(e) => { setProvider(e.target.value); setModel(""); if (e.target.value !== "claude") setEpId(""); }}>
            <option value="claude" disabled={!enabledProv("claude")}>Claude Code{enabledProv("claude") ? "" : "（已停用）"}</option><option value="copilot" disabled={!enabledProv("copilot")}>GitHub Copilot CLI{enabledProv("copilot") ? "" : "（已停用）"}</option>
          </select>
        </Field>
        {provider === "claude" && (
          <Field label="LLM" hint={eps.length === 0 ? "默认用本机 Claude 订阅。要接其他模型（如另一台电脑上的 Ollama），先到“模型 → LLM 端点”添加。" : "默认用本机 Claude 订阅；选端点后，只有 TA 的进程连到那个服务，不影响本机。换 LLM 会清掉 TA 的旧会话。"}>
            <select value={epId} onChange={(e) => { setEpId(e.target.value); setModel(""); }}>
              <option value="">Claude 订阅（本机登录）</option>
              {eps.map((x) => <option key={x.id} value={x.id} disabled={!x.enabled && x.id !== initial?.endpointId}>{x.name}{x.enabled ? "" : "（已停用）"}</option>)}
            </select>
          </Field>
        )}
        {ep ? (
          <Field label="模型" hint="只能选这个端点上配置的模型；留空用第一个。本地模型的工具调用较弱，更适合做范围明确的开发任务，不建议当负责人。">
            <select value={model} onChange={(e) => setModel(e.target.value)}>
              <option value="">{ep.models[0]}（默认）</option>
              {ep.models.slice(1).map((m) => <option key={m} value={m}>{m}</option>)}
            </select>
          </Field>
        ) : (
        <Field label="模型" hint="留空使用引擎默认。可填别名或完整模型名；名称不对时 TA 会在下次执行时报错，改回即可。">
          <input list={`models-${provider}`} value={model} placeholder="默认（可在“模型”页按用途设置默认模型）" onChange={(e) => setModel(e.target.value.trim())} />
          <datalist id={`models-${provider}`}>{(cat?.models ?? []).filter((m) => m.provider === provider && m.enabled).map((m) => <option key={m.model_id} value={m.model_id}>{m.label}</option>)}</datalist>
        </Field>
        )}
        <Field label="身份说明" hint="告诉 TA 是谁、怎么沟通、擅长什么。留空会使用默认说明。">
          <textarea rows={5} value={persona} onChange={(e) => setPersona(e.target.value)} />
        </Field>
        <div className="form-btns">
          <button className="btn primary" disabled={!valid || busy} onClick={async () => {
            setBusy(true);
            try { await onSubmit(name.trim(), role, persona.trim() || `你是 ${name.trim()}，一名${role}。`, provider, model, provider === "claude" ? epId : ""); } finally { setBusy(false); }
          }}>{initial ? "保存" : "创建"}</button>
        </div>
      </div>
    </Modal>
  );
}

function ProjectForm({ onSubmit, onClose }: { onSubmit: (name: string, desc: string, path: string, init: boolean, imp: boolean) => Promise<void>; onClose: () => void }) {
  const [path, setPath] = useState("");
  const [name, setName] = useState("");
  const [desc, setDesc] = useState("");
  const [init, setInit] = useState(true);
  const [imp, setImp] = useState(true);
  const [busy, setBusy] = useState(false);
  async function pick() {
    const p = await api.pickDirectory();
    if (p) { setPath(p); if (!name) setName(basename(p)); }
  }
  return (
    <Modal title="新建 / 导入项目" onClose={onClose}>
      <div className="form">
        <Field label="项目目录" hint="选择已有代码目录即为导入；选择空目录即为新建。">
          <div style={{ display: "flex", gap: 8 }}>
            <input value={path} readOnly placeholder="尚未选择" />
            <button className="btn" onClick={() => void pick()}>选择…</button>
          </div>
        </Field>
        <Field label="项目名称"><input value={name} onChange={(e) => setName(e.target.value)} /></Field>
        <Field label="描述（可选）"><textarea rows={3} value={desc} onChange={(e) => setDesc(e.target.value)} /></Field>
        <label className="tog"><input type="checkbox" checked={imp} onChange={(e) => setImp(e.target.checked)} />
          这是已有的项目：先做只读扫描和测试基线，确认后再让员工开发（推荐）</label>
        <label className="tog"><input type="checkbox" checked={init} onChange={(e) => setInit(e.target.checked)} />
          目录还不是 Git 仓库时，自动初始化（员工需要在 Git 工作区里工作）</label>
        <div className="form-btns">
          <button className="btn primary" disabled={!path || !name.trim() || busy} onClick={async () => {
            setBusy(true);
            try { await onSubmit(name, desc, path, init, imp); } finally { setBusy(false); }
          }}>创建项目</button>
        </div>
      </div>
    </Modal>
  );
}

export function CompanyView({
  tab, cards, projects, company, running, events, onOpenProject, onChanged, onError,
}: {
  tab: CompanyTab;
  cards: EmployeeCard[];
  projects: ProjectSummary[];
  company: Company | null;
  running: string[];
  events: EventRow[];
  onOpenProject: (id: string) => void;
  onChanged: () => void;
  onError: (m: string) => void;
}) {
  const [modal, setModal] = useState<"new-emp" | "new-proj" | { edit: EmployeeCard } | { memory: EmployeeCard } | null>(null);
  const [filter, setFilter] = useState("");
  const visible = filter.trim()
    ? cards.filter((c) => (c.employee.name + c.employee.role).toLowerCase().includes(filter.trim().toLowerCase()))
    : cards;

  async function guard(fn: () => Promise<unknown>) {
    try { await fn(); onChanged(); return true; } catch (e) { onError(String(e)); return false; }
  }

  if (tab === "employees") {
    return (
      <div className="page">
        <div className="page-head">
          <div><h2>AI 员工</h2><div className="hint">员工是公司的长期身份，可以参与多个项目。数量不设上限。</div></div>
          <div style={{ display: "flex", gap: 8 }}>
            {cards.length > 8 && <input style={{ width: 200 }} placeholder={`搜索 ${cards.length} 名员工`} value={filter} onChange={(e) => setFilter(e.target.value)} />}
            <button className="btn primary" onClick={() => setModal("new-emp")}>＋ 新员工</button>
          </div>
        </div>
        {cards.length === 0 && (
          <div className="empty"><img src="/logo.png" alt="" /><h2>先组建你的团队</h2><p>团队规模由你决定，一个人或二十个人都可以。</p>
            <button className="btn primary" onClick={() => setModal("new-emp")}>创建第一名员工</button></div>
        )}
        <div className="cards">
          {visible.map((c) => {
            const busy = running.includes(c.employee.name);
            return (
              <div className="card" key={c.employee.id}>
                <div className="card-top">
                  <Avatar name={c.employee.name} size={38} />
                  <div style={{ minWidth: 0 }}>
                    <b>{c.employee.name}</b>
                    <div className="hint">{c.employee.role}{c.employee.provider === "copilot" && <span className="badge" style={{ marginLeft: 6 }}>Copilot</span>}{c.employee.endpoint_id && <span className="badge" style={{ marginLeft: 6 }} title="连的是自定义 LLM 端点，不是本机 Claude 订阅">本地模型</span>}{c.employee.model && <span className="badge" style={{ marginLeft: 6 }}>{c.employee.model}</span>}</div>
                  </div>
                  <span className={`status ${busy ? "run" : ""}`}>{busy ? "● 工作中" : "○ 空闲"}</span>
                </div>
                <div className="card-persona">{c.employee.persona}</div>
                <div className="card-meta">
                  {c.projects.length === 0 ? <span className="hint">未参与任何项目</span> : c.projects.map(([id, n]) => (
                    <button key={id} className="chip" onClick={() => onOpenProject(id)}>{n}</button>
                  ))}
                </div>
                <div className="card-foot">
                  {c.active_tasks > 0 && <span className="hint">{c.active_tasks} 个进行中的任务</span>}
                  <span style={{ flex: 1 }} />
                  <button className="btn" onClick={() => setModal({ memory: c })}>个人记忆</button>
                  <button className="btn" onClick={() => setModal({ edit: c })}>编辑</button>
                  <button className="btn danger" onClick={async () => {
                    if (await api.confirm(`归档 ${c.employee.name}？\n历史记录和记忆会保留，但不再出现在列表里，也不能分配任务。`, "确认归档"))
                      await guard(() => api.archiveEmployee(c.employee.id));
                  }}>归档</button>
                </div>
              </div>
            );
          })}
        </div>
        {modal === "new-emp" && <EmployeeForm onClose={() => setModal(null)} onSubmit={async (n, r, p, pv, md, ep) => { if (await guard(async () => { const e = await api.addEmployee(n, r, p, pv, ep ? "" : md); if (ep) await api.setEmployeeEndpoint(e.id, ep, md); })) setModal(null); }} />}
        {modal && typeof modal === "object" && "memory" in modal && (
          <Modal title={`${modal.memory.employee.name} 的个人记忆`} onClose={() => setModal(null)} width={760}>
            <MemoryView embedded scope={{ layer: "employee", employeeId: modal.memory.employee.id, name: modal.memory.employee.name }} events={events} onError={onError} />
          </Modal>
        )}
        {modal && typeof modal === "object" && "edit" in modal && (
          <EmployeeForm
            initial={{ name: modal.edit.employee.name, role: modal.edit.employee.role, persona: modal.edit.employee.persona, provider: modal.edit.employee.provider, model: modal.edit.employee.model, endpointId: modal.edit.employee.endpoint_id }}
            onClose={() => setModal(null)}
            onSubmit={async (_n, r, p, pv, md, ep) => {
              const emp = modal.edit.employee;
              if (await guard(async () => {
                await api.updateEmployee(emp.id, r, p, pv, md);
                if (ep || emp.endpoint_id) await api.setEmployeeEndpoint(emp.id, ep || null, md);
              })) setModal(null);
            }}
          />
        )}
      </div>
    );
  }

  if (tab === "projects") {
    return (
      <div className="page">
        <div className="page-head">
          <div><h2>项目</h2><div className="hint">每个项目有自己的群聊、任务、成员和记忆。</div></div>
          <button className="btn primary" onClick={() => setModal("new-proj")}>＋ 新建 / 导入项目</button>
        </div>
        {projects.length === 0 && (
          <div className="empty"><img src="/logo.png" alt="" /><h2>还没有项目</h2><p>选择一个已有代码目录导入，或选择空目录新建。</p>
            <button className="btn primary" onClick={() => setModal("new-proj")}>新建 / 导入项目</button></div>
        )}
        <div className="cards">
          {projects.map((s) => (
            <div className="card clickable" key={s.project.id} onClick={() => onOpenProject(s.project.id)}>
              <div className="card-top"><div style={{ minWidth: 0 }}><b>{s.project.name}</b>
                <div className="hint path-line" title={s.project.path}>{s.project.path}</div></div></div>
              {s.project.description && <div className="card-persona">{s.project.description}</div>}
              <div className="card-meta">
                <span className="chip static">{s.member_count} 名成员</span>
                <span className="chip static">{s.active_tasks} 个进行中任务</span>
                <span className="chip static">负责人：{s.lead_name ?? "人类（你）"}</span>
              </div>
            </div>
          ))}
        </div>
        <AlignOverview refreshKey={projects.length} onOpenProject={(id) => onOpenProject(id)} onError={onError} />
        <EndedProjects refreshKey={projects.length} onRestored={onChanged} onError={onError} />
        {modal === "new-proj" && (
          <ProjectForm onClose={() => setModal(null)} onSubmit={async (n, d, p, i, imp) => {
            const ok = await guard(() => api.createProject(n, d, p, i, imp));
            if (ok) setModal(null);
          }} />
        )}
      </div>
    );
  }

  if (tab === "manage") return <CompanyManage company={company} onSaved={onChanged} onError={onError} />;
  if (tab === "memory") return <MemoryView scope={{ layer: "company" }} events={events} onError={onError} />;
  return <Placeholder title="公司文档">公司级工程规范、技术选型原则、通用流程。存放在数据目录，供所有项目引用。</Placeholder>;
}

function CompanyManage({ company, onSaved, onError }: { company: Company | null; onSaved: () => void; onError: (m: string) => void }) {
  const [name, setName] = useState("");
  const [desc, setDesc] = useState("");
  const [conv, setConv] = useState("");
  const [saved, setSaved] = useState(false);
  useEffect(() => {
    if (company) { setName(company.name); setDesc(company.description); setConv(company.conventions); }
  }, [company]);
  return (
    <div className="page narrow">
      <div className="page-head"><div><h2>公司管理</h2><div className="hint">这些设置对所有项目生效。</div></div></div>
      <div className="form">
        <Field label="公司名称"><input value={name} onChange={(e) => { setName(e.target.value); setSaved(false); }} /></Field>
        <Field label="简介"><textarea rows={3} value={desc} onChange={(e) => { setDesc(e.target.value); setSaved(false); }} /></Field>
        <Field label="公司级工程规范" hint="例如代码风格、测试要求、提交规范。后续会随任务注入给员工（尚未接入）。">
          <textarea rows={8} value={conv} onChange={(e) => { setConv(e.target.value); setSaved(false); }} />
        </Field>
        <div className="form-btns">
          {saved && <span className="hint">已保存</span>}
          <button className="btn primary" disabled={!name.trim()} onClick={async () => {
            try { await api.updateCompany(name, desc, conv); setSaved(true); onSaved(); } catch (e) { onError(String(e)); }
          }}>保存</button>
        </div>
      </div>
    </div>
  );
}
