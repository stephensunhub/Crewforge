import { useCallback, useEffect, useState } from "react";
import { api } from "./api";
import { EndpointDetail, EndpointForm } from "./EndpointsView";
import type { Employee, Endpoint, ModelsOverview, ProviderStatus } from "./types";

const PROVIDER_HINT: Record<string, string> = {
  claude: "用你本机已登录的 Claude Code 账号，按订阅额度（5 小时 / 7 天窗口）计量，可以边做边向你请求权限。",
  copilot: "用你本机已登录的 GitHub Copilot 账号，按“高级请求”计量；权限在启动时一次定好，没有列入的命令会被直接拒绝。可以用 Copilot 里的各家模型（Claude、GPT、Gemini、Grok、Kimi 等）。",
};

export function ModelsView({ onError }: { onError: (m: string) => void }) {
  const [o, setO] = useState<ModelsOverview | null>(null);
  const [eps, setEps] = useState<Endpoint[]>([]);
  const [emps, setEmps] = useState<Employee[]>([]);
  const [editing, setEditing] = useState<Endpoint | "new" | null>(null);
  const [sel, setSel] = useState<string>(() => { try { const k = localStorage.getItem("models.sel") ?? "claude"; return k === "endpoints" ? "claude" : k; } catch { return "claude"; } });
  const [busy, setBusy] = useState("");
  const [note, setNote] = useState("");
  const [newId, setNewId] = useState("");
  const [newLabel, setNewLabel] = useState("");
  const [path, setPath] = useState("");
  const pick = (k: string) => { setSel(k); try { localStorage.setItem("models.sel", k); } catch { /* 忽略 */ } };

  const load = useCallback(async () => {
    try { const v = await api.modelsOverview(); setEps(await api.listEndpoints()); setEmps((await api.employees()).filter((e) => !e.archived)); setO(v); setPath(v.providers.find((p) => p.provider === "copilot")?.program ?? ""); } catch (e) { onError(String(e)); }
  }, [onError]);
  useEffect(() => { void load(); }, [load]);

  const act = async (key: string, f: () => Promise<unknown>, ok?: string) => {
    setBusy(key);
    try { const r = await f(); setNote(ok ? (typeof r === "number" ? `${ok}：新增 ${r} 个` : typeof r === "string" && r ? `${ok}：${r}` : ok) : ""); await load(); } catch (e) { onError(String(e)); } finally { setBusy(""); }
  };

  if (!o) return <div className="hint" style={{ padding: 20 }}>加载中…</div>;
  const prov = o.providers.find((p) => p.provider === sel);
  const selEp = sel.startsWith("ep:") ? eps.find((x) => x.id === sel.slice(3)) : undefined;
  const modelsOf = (p: string) => o.models.filter((m) => m.provider === p);

  return (
    <div className="split">
      <aside className="split-side" style={{ width: 230 }}>
        <div className="split-head"><b>引擎与模型</b></div>
        <div className="split-list">
          {o.providers.map((p) => (
            <button key={p.provider} className={`mitem ${sel === p.provider ? "on" : ""}`} onClick={() => pick(p.provider)}>
              <div className="mitem-title">{p.label}</div>
              <div className="hint">{!p.enabled ? "已停用" : p.installed ? `已就绪 · ${p.employees} 名员工` : "未检测到"}</div>
            </button>
          ))}
          <div className="mitem-group">自建端点{eps.length ? `（${eps.length}）` : ""}</div>
          {eps.map((x) => (
            <button key={x.id} className={`mitem ${sel === `ep:${x.id}` ? "on" : ""}`} onClick={() => pick(`ep:${x.id}`)}>
              <div className="mitem-title">{x.name}</div>
              <div className="hint">{!x.enabled ? "已停用" : `${x.models.length} 个模型 · ${emps.filter((e) => e.endpoint_id === x.id).length} 名员工`}</div>
            </button>
          ))}
          <button className="mitem" onClick={() => setEditing("new")}>
            <div className="mitem-title">＋ 添加 LLM 端点</div>
            <div className="hint">另一台电脑上的 Ollama 或其他兼容服务</div>
          </button>
          <div className="mitem-group">总览</div>
          <button className={`mitem ${sel === "who" ? "on" : ""}`} onClick={() => pick("who")}>
            <div className="mitem-title">员工与模型对照</div>
            <div className="hint">谁在通过哪个通道用哪个模型</div>
          </button>
          <button className={`mitem ${sel === "purposes" ? "on" : ""}`} onClick={() => pick("purposes")}>
            <div className="mitem-title">按用途的默认模型</div>
            <div className="hint">量大的用强模型，量小的用便宜模型</div>
          </button>
        </div>
      </aside>

      <div className="split-main" style={{ overflow: "auto" }}>
        <div className="page wide">
          {note && <div className="note" style={{ marginBottom: 10 }}>{note}</div>}
          {prov && <ProviderPanel p={prov} users={emps.filter((e) => e.provider === prov.provider && !e.endpoint_id)} models={modelsOf(prov.provider)} busy={busy} path={path} setPath={setPath}
            newId={newId} setNewId={setNewId} newLabel={newLabel} setNewLabel={setNewLabel} act={act} />}

          {selEp && <EndpointDetail key={selEp.id} ep={selEp} users={emps.filter((e) => e.endpoint_id === selEp.id)} reload={load} onError={onError} onEdit={() => setEditing(selEp)} onRemoved={() => pick("claude")} />}
          {sel === "endpoints" && eps.length === 0 && <div className="hint">还没有自建端点。点左侧的“＋ 添加 LLM 端点”。</div>}

          {sel === "who" && (
            <>
              <div className="page-head"><div><h2>员工与模型对照</h2>
                <div className="hint">每位员工通过哪个通道、实际用哪个模型。要改，到“公司 → 员工”里编辑员工（通道和模型在同一个窗口里选）。</div></div></div>
              <table className="cost-table">
                <thead><tr><th>编号</th><th>员工</th><th>岗位</th><th>通道</th><th>模型</th></tr></thead>
                <tbody>{emps.map((e) => {
                  const ep = e.endpoint_id ? eps.find((x) => x.id === e.endpoint_id) : undefined;
                  const provider = ep ? `${ep.name}（自建端点）` : e.provider === "copilot" ? "GitHub Copilot" : "Claude 订阅";
                  const model = e.model || (ep ? `${ep.models[0] ?? ""}（端点默认）` : "引擎默认 / 按用途");
                  return <tr key={e.id}><td className="hint">{e.number > 0 ? `#${e.number}` : ""}</td><td><b>{e.name}</b></td><td className="hint">{e.role}</td><td>{provider}{ep && !ep.enabled && <span className="badge" style={{ marginLeft: 6 }}>已停用</span>}</td><td><code>{model}</code></td></tr>;
                })}</tbody>
              </table>
            </>
          )}

          {sel === "purposes" && (
            <>
              <div className="page-head"><div><h2>按用途的默认模型</h2>
                <div className="hint">选模型的优先级：员工自己指定的模型 &gt; 这里的用途默认 &gt; 引擎默认。例如：开发任务用 Opus，会议发言和纪要用 Haiku。</div></div></div>
              <table className="cost-table">
                <thead><tr><th>用途</th>{o.providers.map((p) => <th key={p.provider}>{p.label}</th>)}</tr></thead>
                <tbody>{o.purposes.map(([k, label, m]) => (
                  <tr key={k}><td><b>{label}</b></td>
                    {o.providers.map((p) => {
                      const cur = m[p.provider] ?? "";
                      const opts = modelsOf(p.provider).filter((x) => x.enabled || x.model_id === cur);
                      return (
                        <td key={p.provider}>
                          <select value={cur} onChange={(e) => void act(`pm-${k}-${p.provider}`, () => api.setPurposeModel(p.provider, k, e.target.value))}>
                            <option value="">引擎默认</option>
                            {opts.map((x) => <option key={x.model_id} value={x.model_id}>{x.label || x.model_id}{x.enabled ? "" : "（已停用）"}</option>)}
                            {cur && !opts.some((x) => x.model_id === cur) && <option value={cur}>{cur}</option>}
                          </select>
                        </td>
                      );
                    })}
                    {(() => {
                      const r = o.recommended.find((x) => x[0] === k);
                      return <td className="hint" title={r?.[2]}>{r ? <><b>{r[1]}</b> · {r[2]}</> : "—"}</td>;
                    })()}
                  </tr>))}</tbody>
              </table>
              <div className="row" style={{ gap: 10, marginTop: 10, alignItems: "center" }}>
                <button className="btn" disabled={busy === "rec"} onClick={() => void act("rec", () => api.applyRecommendedModels(), "已应用推荐")}>应用推荐（只填还没设置的用途）</button>
                <span className="hint">思路：次数少、影响大的判断（裁决、计划）用强模型；量最大的开发和讨论用中档；整理类用小模型。只改 Claude 的用途默认，员工自己指定的模型不受影响。</span>
              </div>
              <div className="hint" style={{ marginTop: 10 }}>实际用了哪个模型，可以在“用量 → 按模型”和“调用明细”里核对。</div>
            </>
          )}
        </div>
      </div>
      {editing && <EndpointForm initial={editing === "new" ? null : editing} onClose={() => setEditing(null)} onSaved={async () => { setEditing(null); await load(); }} onError={onError} />}
    </div>
  );
}

function ProviderPanel({ p, users, models, busy, path, setPath, newId, setNewId, newLabel, setNewLabel, act }: {
  p: ProviderStatus; users: Employee[]; models: ModelsOverview["models"]; busy: string; path: string; setPath: (v: string) => void;
  newId: string; setNewId: (v: string) => void; newLabel: string; setNewLabel: (v: string) => void;
  act: (key: string, f: () => Promise<unknown>, ok?: string) => Promise<void>;
}) {
  const isCopilot = p.provider === "copilot";
  return (
    <>
      <div className="page-head">
        <div><h2>{p.label}</h2><div className="hint">{PROVIDER_HINT[p.provider]}</div></div>
        <label className="hint" style={{ display: "flex", gap: 6, alignItems: "center", whiteSpace: "nowrap", flex: "none" }}>
          <input type="checkbox" checked={p.enabled} onChange={(e) => void act("enable", () => api.setProviderEnabled(p.provider, e.target.checked))} /> 激活
        </label>
      </div>

      <section className="panel-box">
        <h4 className="group-title">状态</h4>
        <table className="cost-table"><tbody>
          <tr><td><b>可执行文件</b></td><td>{p.installed ? <span>已检测到 · {p.version}</span> : <span className="bad">没有检测到可用的 {p.provider === "claude" ? "claude" : "copilot"}</span>}</td></tr>
          <tr><td><b>路径</b></td><td>
            {isCopilot ? (
              <div style={{ display: "flex", gap: 8 }}>
                <input style={{ flex: 1 }} value={path} onChange={(e) => setPath(e.target.value)} />
                <button className="btn" disabled={busy === "path"} onClick={() => void act("path", () => api.setCopilotProgram(path), "已保存路径")}>保存</button>
                <button className="btn" disabled={busy === "install"} title="用 npm 装到应用自己的数据目录，不动系统全局" onClick={() => void act("install", () => api.installCopilot(), "已安装")}>{busy === "install" ? "安装中…" : "安装到应用目录"}</button>
              </div>
            ) : <code>{p.program}</code>}
          </td></tr>
          <tr><td><b>员工</b></td><td>{users.length === 0 ? "还没有员工用它" : users.map((u) => `${u.name}${u.model ? `（${u.model}）` : ""}`).join("、")}</td></tr>
          {!p.enabled && <tr><td colSpan={2}><span className="bad">已停用：不能新建员工或切换到它；用它的员工执行任务时会被拒绝并提示原因。</span></td></tr>}
        </tbody></table>
      </section>

      <section className="panel-box">
        <div className="page-head" style={{ marginBottom: 6 }}>
          <h4 className="group-title" style={{ margin: 0 }}>可选模型（{models.length}）</h4>
          {isCopilot && <button className="btn" disabled={busy === "discover"} title="读取 Copilot CLI 自己列出的模型" onClick={() => void act("discover", () => api.discoverCopilotModels(), "已读取 Copilot 的模型清单")}>{busy === "discover" ? "读取中…" : "发现模型"}</button>}
        </div>
        <table className="cost-table">
          <thead><tr><th>模型</th><th>说明</th><th>启用</th><th></th></tr></thead>
          <tbody>{models.map((m) => (
            <tr key={m.model_id}>
              <td><b>{m.label && m.label !== m.model_id ? m.label : m.model_id}</b>{m.label && m.label !== m.model_id && <div className="hint"><code>{m.model_id}</code></div>}</td>
              <td className="hint">{m.note}{!m.builtin && "自己添加"}</td>
              <td><input type="checkbox" checked={m.enabled} onChange={(e) => void act(`en-${m.model_id}`, () => api.setModelEnabled(m.provider, m.model_id, e.target.checked))} /></td>
              <td>{!m.builtin && <button className="link" onClick={() => void act(`rm-${m.model_id}`, () => api.removeModel(m.provider, m.model_id))}>删除</button>}</td>
            </tr>))}</tbody>
        </table>
        <div style={{ display: "flex", gap: 8, marginTop: 10 }}>
          <input style={{ width: 240 }} value={newId} placeholder="模型名（传给 --model）" onChange={(e) => setNewId(e.target.value.trim())} />
          <input style={{ flex: 1 }} value={newLabel} placeholder="显示名（可选）" onChange={(e) => setNewLabel(e.target.value)} />
          <button className="btn primary" disabled={!newId} onClick={() => void act("add", async () => { await api.addModel(p.provider, newId, newLabel); setNewId(""); setNewLabel(""); })}>添加</button>
        </div>
        <div className="hint" style={{ marginTop: 6 }}>名字写错时，员工下次执行会报错，改回即可。内置的只能停用，自己添加的可以删除。</div>
      </section>
    </>
  );
}
