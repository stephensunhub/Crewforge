import { EndedProjects } from "./EndProject";
import { AlignOverview } from "./AlignBox";
import { useEffect, useState } from "react";
import { api } from "./api";
import { ActivityLine, Avatar, Field, Modal, Placeholder, basename } from "./common";
import { MemoryView } from "./MemoryView";
import { endpointOfSrc, endpointToInput, providerOfSrc, replaceName, srcOf } from "./llm";
import { EmployeeLog } from "./EmployeeLog";
import { InterventionsView } from "./InterventionsView";
import type { ProjectTab } from "./ProjectView";
import type { Activity, Company, Endpoint, EmployeeCard, EventRow, ModelsOverview, ProjectSummary } from "./types";

export type CompanyTab = "employees" | "projects" | "interventions" | "memory" | "docs" | "manage";
export const COMPANY_TABS: [CompanyTab, string][] = [
  ["employees", "员工"], ["projects", "项目"], ["interventions", "人工介入"], ["memory", "记忆"], ["docs", "文档"], ["manage", "管理"],
];

const ROLES = ["全栈工程师", "QA 测试工程师", "架构师 / CTO", "其他"];

function EmployeeForm({ initial, onSubmit, onClose, onError }: {
  initial?: { name: string; number?: number; levelCap?: number; effectiveCap?: number; role: string; persona: string; provider?: string; model?: string; endpointId?: string | null };
  onSubmit: (name: string, role: string, persona: string, provider: string, model: string, endpointId: string, levelCap: number) => Promise<void>;
  onClose: () => void;
  onError: (m: string) => void;
}) {
  const [name, setName] = useState(initial?.name ?? "");
  const [role, setRole] = useState(initial?.role ?? ROLES[0]);
  const [persona, setPersona] = useState(initial?.persona ?? "");
  const [src, setSrc] = useState(srcOf(initial?.provider ?? "claude", initial?.endpointId));
  const provider = providerOfSrc(src);
  const epId = endpointOfSrc(src);
  const [model, setModel] = useState(initial?.model ?? "");
  const [fetched, setFetched] = useState<string[]>([]);
  const [fetching, setFetching] = useState(false);
  const [fetchNote, setFetchNote] = useState("");
  const [other, setOther] = useState(false);
  const [levelCap, setLevelCap] = useState(initial?.levelCap ?? 0);
  const [busy, setBusy] = useState(false);
  const [cat, setCat] = useState<ModelsOverview | null>(null);
  const [eps, setEps] = useState<Endpoint[]>([]);
  useEffect(() => { void api.modelsOverview().then(setCat).catch(() => setCat(null)); void api.listEndpoints().then(setEps).catch(() => setEps([])); }, []);
  const ep = epId ? eps.find((x) => x.id === epId) : undefined;
  const channelLabel = ep ? ep.name : provider === "copilot" ? "GitHub Copilot" : "Claude 订阅";
  // 当前通道里的模型：端点用它已启用的，订阅用模型目录里启用的
  const channelModels: { id: string; label: string }[] = ep
    ? ep.models.map((m) => ({ id: m, label: m }))
    : (cat?.models ?? []).filter((m) => m.provider === provider && m.enabled).map((m) => ({ id: m.model_id, label: m.label && m.label !== m.model_id ? `${m.label}（${m.model_id}）` : m.model_id }));
  const extraModels = ep ? fetched.filter((m) => !ep.models.includes(m)) : [];
  const enabledProv = (p: string) => !cat || (cat.providers.find((x) => x.provider === p)?.enabled ?? true) || initial?.provider === p;
  const channels = [
    { src: "claude", title: "Claude 订阅", desc: "本机登录的 Claude Code 账号，按订阅额度计量，可以边做边向你请求权限", disabled: !enabledProv("claude") },
    { src: "copilot", title: "GitHub Copilot", desc: "本机登录的 Copilot 账号，按高级请求计量，权限在启动时一次定好", disabled: !enabledProv("copilot") },
    ...eps.map((x) => ({ src: `ep:${x.id}`, title: x.name, desc: `自建端点 · ${x.base_url.replace(/^https?:\/\//, "")} · ${x.models.length} 个模型`, disabled: !x.enabled && x.id !== initial?.endpointId })),
  ];
  const valid = name.trim() !== "" && !/[\s@]/.test(name);
  return (
    <Modal title={initial ? `编辑 ${initial.name}${initial.number ? `（#${initial.number}）` : ""}` : "新员工"} onClose={onClose}>
      <div className="form">
        <Field label="名称" hint={initial
          ? (name.trim() !== initial.name ? "改名要求 TA 空闲、所在项目的自主推进没在运行。会把 TA 的工作区目录一起搬到新名字下、身份说明里的旧名字换成新名字；历史记录（聊天、纪要、记忆）里的旧名字不改，旧消息里的 @旧名 以后不再指向 TA。" : `编号 #${initial.number}（按创建顺序，不会变）。名字可以修改，改名时会有提示。`)
          : "不含空格和 @。创建后可以改名，但要求 TA 空闲。"}>
          <input value={name} onChange={(e) => setName(e.target.value)} autoFocus={!initial} />
        </Field>
        <Field label="岗位">
          <select value={role} onChange={(e) => setRole(e.target.value)}>
            {[...new Set([role, ...ROLES])].map((r) => <option key={r}>{r}</option>)}
          </select>
        </Field>
        <Field label="LLM 通道" hint={initial
          ? (provider !== initial.provider ? "⚠ 切换引擎后 TA 之前的对话上下文不会带过去（任务分支、工作区、历史记录和记忆都保留）；TA 正在工作时不能切换。" : "可以随时切换，下次执行时生效；换通道会清掉 TA 的旧会话。")
          : "先选通道，再在通道里选模型。要添加自建端点，到“模型”页。"}>
          <div className="channels">
            {channels.map((c) => (
              <label key={c.src} className={`channel ${src === c.src ? "on" : ""} ${c.disabled ? "off" : ""}`}>
                <input type="radio" name="llm-channel" checked={src === c.src} disabled={c.disabled}
                  onChange={() => { setSrc(c.src); setModel(""); setOther(false); setFetched([]); setFetchNote(""); }} />
                <span><b>{c.title}</b><span className="hint" style={{ display: "block" }}>{c.desc}{c.disabled ? "（已停用）" : ""}</span></span>
              </label>
            ))}
          </div>
        </Field>
        <Field label={`模型（${channelLabel}里的）`} hint={ep
          ? "只列这个端点已启用的模型；不在列表里的，先“从服务器读取”再选，或选“其他”手动输入（保存时会自动加进端点的模型清单）。留空用清单里的第一个。本地模型的工具调用较弱，更适合做范围明确的开发任务，不建议当负责人。"
          : "只列这个通道里已启用的模型；留空用引擎默认（可在“模型”页按用途设置）。不在列表里的选“其他”手动输入，名称不对时 TA 会在下次执行时报错，改回即可。"}>
          <select value={other ? "__other__" : model} onChange={(e) => { if (e.target.value === "__other__") { setOther(true); setModel(""); } else { setOther(false); setModel(e.target.value); } }}>
            <option value="">{ep ? `${ep.models[0] ?? ""}（端点默认）` : "引擎默认"}</option>
            {channelModels.map((m) => <option key={m.id} value={m.id}>{m.label}</option>)}
            {extraModels.length > 0 && <optgroup label="服务器上有，未启用（选它会自动启用）">{extraModels.map((m) => <option key={m} value={m}>{m}</option>)}</optgroup>}
            {model && !other && !channelModels.some((m) => m.id === model) && !extraModels.includes(model) && <option value={model}>{model}（当前，不在列表里）</option>}
            <option value="__other__">其他（手动输入）…</option>
          </select>
          {other && <input style={{ marginTop: 6 }} autoFocus value={model} placeholder="模型名" onChange={(e) => setModel(e.target.value.trim())} />}
          {ep && (
            <div style={{ display: "flex", gap: 8, alignItems: "center", marginTop: 6 }}>
              <button className="btn" disabled={fetching} onClick={async () => {
                setFetching(true); setFetchNote("");
                try { const m = await api.discoverEndpointModels(ep.base_url, null, ep.id); setFetched(m); setFetchNote(`服务器上共 ${m.length} 个模型，其中 ${m.filter((x) => !ep.models.includes(x)).length} 个还没启用`); }
                catch (e) { setFetchNote(String(e)); } finally { setFetching(false); }
              }}>{fetching ? "读取中…" : "从服务器读取更多模型"}</button>
              <span className="hint">{fetchNote}</span>
            </div>
          )}
        </Field>
        <div className="hint" style={{ marginTop: -4 }}>TA 将通过 <b>{channelLabel}</b> 使用 <b>{model || (ep ? ep.models[0] : "") || "引擎默认模型"}</b>。</div>
        <Field label="能力上限（任务等级）" hint="能做到几级的任务（1–5，见计划页的等级）。自动：按上面选的模型给默认值（本地模型 2、Haiku 3、Sonnet 4、Opus / Fable 5）。没明确指派时，自主推进把活派给「上限够用的最低档」的人；明确指派的不受限制，但会提示。">
          <select value={levelCap} onChange={(e) => setLevelCap(Number(e.target.value))}>
            <option value={0}>自动{initial?.effectiveCap ? `（现在是 ${initial.effectiveCap} 级）` : ""}</option>
            {[1, 2, 3, 4, 5].map((n) => <option key={n} value={n}>{n} 级</option>)}
          </select>
        </Field>
        <Field label="身份说明" hint="告诉 TA 是谁、怎么沟通、擅长什么。留空会使用默认说明。">
          <textarea rows={5} value={persona} onChange={(e) => setPersona(e.target.value)} />
        </Field>
        <div className="form-btns">
          <button className="btn primary" disabled={!valid || busy} onClick={async () => {
            setBusy(true);
            try {
              // 选了清单之外的模型（手输或从服务器读到的）：先加进端点的模型清单，再保存员工
              if (ep && model && !ep.models.includes(model)) await api.updateEndpoint(ep.id, endpointToInput(ep, [...ep.models, model]));
              await onSubmit(name.trim(), role, persona.trim() || `你是 ${name.trim()}，一名${role}。`, provider, model, epId, levelCap);
            } catch (e) { onError(String(e)); } finally { setBusy(false); }
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
  const [imp, setImp] = useState(false);
  const [busy, setBusy] = useState(false);
  async function pick() {
    const p = await api.pickDirectory();
    // 目录里已有内容才按“导入已有项目”处理；空目录就是新建，不需要走导入流程
    if (p) { setPath(p); if (!name) setName(basename(p)); setImp(await api.dirHasContent(p).catch(() => false)); }
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
          这是已有的项目：先做只读扫描和测试基线，确认后再让员工开发（选了有代码的目录会自动勾上；新建项目不要勾）</label>
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
  tab, cards, projects, company, running, activities, onOpenWork, events, onOpenProject, onGoScope, onChanged, onError,
}: {
  tab: CompanyTab;
  cards: EmployeeCard[];
  projects: ProjectSummary[];
  company: Company | null;
  running: string[];
  activities: Activity[];
  onOpenWork: (a: Activity) => void;
  events: EventRow[];
  onOpenProject: (id: string, tab?: ProjectTab) => void;
  onGoScope: (k: "models" | "settings") => void;
  onChanged: () => void;
  onError: (m: string) => void;
}) {
  const [caps, setCaps] = useState<Record<string, number>>({});
  const [modal, setModal] = useState<"new-emp" | "new-proj" | { edit: EmployeeCard } | { memory: EmployeeCard } | { log: EmployeeCard } | null>(null);
  useEffect(() => { void api.employeeLevels().then((l) => setCaps(Object.fromEntries(l))).catch(() => setCaps({})); }, [modal, cards]);
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
                    <b>{c.employee.name}</b>{c.employee.number > 0 && <span className="hint" style={{ marginLeft: 6 }} title="员工编号：按创建顺序，不会变">#{c.employee.number}</span>}
                    <div className="hint">{c.employee.role}{c.employee.provider === "copilot" && <span className="badge" style={{ marginLeft: 6 }}>Copilot</span>}{c.employee.endpoint_id && <span className="badge" style={{ marginLeft: 6 }} title="连的是自定义 LLM 端点，不是本机 Claude 订阅">本地模型</span>}{c.employee.model && <span className="badge" style={{ marginLeft: 6 }}>{c.employee.model}</span>}</div>
                  </div>
                  <span className={`status ${busy ? "run" : ""}`}>{busy ? "● 工作中" : "○ 空闲"}</span>
                </div>
                {busy && <ActivityLine a={activities.find((x) => x.employee === c.employee.name)} showProject onOpen={onOpenWork} />}
                <div className="card-persona">{c.employee.persona}</div>
                <div className="card-meta">
                  {c.projects.length === 0 ? <span className="hint">未参与任何项目</span> : c.projects.map(([id, n]) => (
                    <button key={id} className="chip" onClick={() => onOpenProject(id)}>{n}</button>
                  ))}
                </div>
                <div className="card-foot">
                  {c.active_tasks > 0 && <span className="hint">{c.active_tasks} 个进行中的任务</span>}
                  <span style={{ flex: 1 }} />
                  <button className="btn" onClick={() => setModal({ log: c })}>工作日志</button>
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
        {modal === "new-emp" && <EmployeeForm onError={onError} onClose={() => setModal(null)} onSubmit={async (n, r, p, pv, md, ep, lc) => { if (await guard(async () => { const e = await api.addEmployee(n, r, p, pv, ep ? "" : md); if (ep) await api.setEmployeeEndpoint(e.id, ep, md); if (lc) await api.setEmployeeLevelCap(e.id, lc); })) setModal(null); }} />}
        {modal && typeof modal === "object" && "log" in modal && (
          <EmployeeLog id={modal.log.employee.id} name={modal.log.employee.name} running={running.includes(modal.log.employee.name)} onClose={() => setModal(null)} />
        )}
        {modal && typeof modal === "object" && "memory" in modal && (
          <Modal title={`${modal.memory.employee.name} 的个人记忆`} onClose={() => setModal(null)} width={760}>
            <MemoryView embedded scope={{ layer: "employee", employeeId: modal.memory.employee.id, name: modal.memory.employee.name }} events={events} onError={onError} />
          </Modal>
        )}
        {modal && typeof modal === "object" && "edit" in modal && (
          <EmployeeForm onError={onError}
            initial={{ name: modal.edit.employee.name, number: modal.edit.employee.number, levelCap: modal.edit.employee.level_cap, effectiveCap: caps[modal.edit.employee.id], role: modal.edit.employee.role, persona: modal.edit.employee.persona, provider: modal.edit.employee.provider, model: modal.edit.employee.model, endpointId: modal.edit.employee.endpoint_id }}
            onClose={() => setModal(null)}
            onSubmit={async (n, r, p, pv, md, ep, lc) => {
              const emp = modal.edit.employee;
              if (await guard(async () => {
                let persona = p;
                if (n !== emp.name) {
                  // 先改名（后端会搬工作区、换身份说明里的名字）；表单里的说明也同步换，免得保存时把旧名字写回去
                  const rep = await api.renameEmployee(emp.id, n);
                  persona = replaceName(p, emp.name, n);
                  if (rep.warnings.length) onError(`已改名为 ${n}，但有提示：${rep.warnings.join("；")}`);
                }
                await api.updateEmployee(emp.id, r, persona, pv, md);
                if (lc !== emp.level_cap) await api.setEmployeeLevelCap(emp.id, lc);
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

  if (tab === "interventions") return <InterventionsView projects={projects} employees={cards.map((c) => c.employee)} running={running} events={events} onOpenWork={(pid, t) => onOpenProject(pid, t as ProjectTab)} onGoScope={onGoScope} onError={onError} />;
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
