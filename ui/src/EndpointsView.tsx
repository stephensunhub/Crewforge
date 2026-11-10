import { useState } from "react";
import { api } from "./api";
import { Field, Modal } from "./common";
import type { Endpoint, EndpointInput, EndpointTest } from "./types";

const envToText = (e: Record<string, string>) => Object.entries(e).map(([k, v]) => `${k}=${v}`).join("\n");
const textToEnv = (t: string) => {
  const o: Record<string, string> = {};
  for (const line of t.split("\n")) { const i = line.indexOf("="); if (i > 0) o[line.slice(0, i).trim()] = line.slice(i + 1).trim(); }
  return o;
};
const splitModels = (t: string) => t.split(/[\n,]/).map((m) => m.trim()).filter(Boolean);

/** 系统级 LLM 端点：员工背后连哪个大模型服务。默认（不选端点）就是本机 Claude 订阅。 */
export function EndpointsPanel({ endpoints, reload, onError }: { endpoints: Endpoint[]; reload: () => Promise<void>; onError: (m: string) => void }) {
  const [editing, setEditing] = useState<Endpoint | "new" | null>(null);
  const run = async (f: () => Promise<unknown>) => { try { await f(); await reload(); } catch (e) { onError(String(e)); } };
  return (
    <>
      <div className="page-head">
        <div><h2>LLM 端点</h2>
          <div className="hint">员工背后连哪个大模型服务。不选端点就是本机 Claude 订阅（负责人建议用这个）；选了端点（例如另一台电脑上的 Ollama），只有该员工的进程会收到连接设置，不影响本机的 Claude 登录和其他项目。</div></div>
        <button className="btn primary" onClick={() => setEditing("new")}>添加端点</button>
      </div>
      {endpoints.length === 0 && <div className="hint">还没有端点。添加后，在员工的“编辑”里选择它。</div>}
      {endpoints.map((ep) => (
        <section key={ep.id} className="panel-box">
          <div className="page-head" style={{ marginBottom: 6 }}>
            <div><h4 className="group-title" style={{ margin: 0 }}>{ep.name} {!ep.enabled && <span className="badge">已停用</span>}</h4>
              <div className="hint"><code>{ep.base_url}</code> · 模型 {ep.models.join("、")} · 同时最多 {ep.max_concurrency} 位员工用{ep.context_tokens > 0 ? ` · 上下文 ${Math.round(ep.context_tokens / 1000)}K` : ""} · {ep.employees} 位员工在用</div></div>
            <div style={{ display: "flex", gap: 8, flex: "none" }}>
              <label className="hint" style={{ display: "flex", gap: 6, alignItems: "center" }}><input type="checkbox" checked={ep.enabled} onChange={(e) => void run(() => api.setEndpointEnabled(ep.id, e.target.checked))} /> 启用</label>
              <button className="btn" onClick={() => setEditing(ep)}>编辑</button>
              <button className="btn danger" onClick={() => void run(async () => { if (await api.confirm(`删除端点 ${ep.name}？`, "删除")) await api.removeEndpoint(ep.id); })}>删除</button>
            </div>
          </div>
        </section>
      ))}
      <div className="hint" style={{ marginTop: 8 }}>本地模型的工具调用能力通常比 Claude 弱，建议只给它做范围明确的开发任务；一台 Ollama 同时只能处理少数请求，所以默认同时只让一位员工用。</div>
      {editing && <EndpointForm initial={editing === "new" ? null : editing} onClose={() => setEditing(null)} onSaved={async () => { setEditing(null); await reload(); }} onError={onError} />}
    </>
  );
}

function EndpointForm({ initial, onClose, onSaved, onError }: { initial: Endpoint | null; onClose: () => void; onSaved: () => Promise<void>; onError: (m: string) => void }) {
  const [name, setName] = useState(initial?.name ?? "");
  const [url, setUrl] = useState(initial?.base_url ?? "http://");
  const [models, setModels] = useState((initial?.models ?? []).join("\n"));
  const [conc, setConc] = useState(initial?.max_concurrency ?? 1);
  const [ctx, setCtx] = useState(initial?.context_tokens ?? 32768);
  const [token, setToken] = useState("");
  const [clearToken, setClearToken] = useState(false);
  const [env, setEnv] = useState(envToText(initial?.extra_env ?? {}));
  const [test, setTest] = useState<EndpointTest | null>(null);
  const [busy, setBusy] = useState("");
  const list = splitModels(models);
  const input = (): EndpointInput => ({ name, base_url: url, models: list, extra_env: textToEnv(env), max_concurrency: conc, context_tokens: ctx, token: clearToken ? "" : token ? token : null });
  const act = async (k: string, f: () => Promise<void>) => { setBusy(k); try { await f(); } catch (e) { onError(String(e)); } finally { setBusy(""); } };
  return (
    <Modal title={initial ? `编辑 ${initial.name}` : "添加 LLM 端点"} onClose={onClose} width={560}>
        <div className="form">
          <Field label="名称"><input value={name} onChange={(e) => setName(e.target.value)} placeholder="例如：书房 Ollama" autoFocus /></Field>
          <Field label="地址" hint="对方电脑上的 Ollama 要设 OLLAMA_HOST=0.0.0.0 并放行端口；只在局域网或 Tailscale 里用，Ollama 本身没有认证。"><input value={url} onChange={(e) => setUrl(e.target.value)} placeholder="http://192.168.1.20:11434" /></Field>
          <Field label="模型（每行一个，第一个是默认）">
            <textarea rows={3} value={models} onChange={(e) => setModels(e.target.value)} placeholder="用 ollama list 看实际标签" />
            <div><button className="btn" disabled={busy === "disc" || url.length < 9} onClick={() => void act("disc", async () => { const m = await api.discoverEndpointModels(url); if (m.length) setModels(m.join("\n")); else onError("服务器上没有模型"); })}>{busy === "disc" ? "读取中…" : "从 Ollama 读取模型"}</button></div></Field>
          <div style={{ display: "flex", gap: 12 }}>
            <div style={{ flex: 1 }}><Field label="同时最多几位员工用"><input type="number" min={1} max={16} value={conc} onChange={(e) => setConc(Number(e.target.value))} /></Field></div>
            <div style={{ flex: 1 }}><Field label="上下文窗口（token，0 不限）"><input type="number" min={0} step={1024} value={ctx} onChange={(e) => setCtx(Number(e.target.value))} /></Field></div>
          </div>
          <div className="hint" style={{ marginTop: -4 }}>上下文窗口要和 Ollama 里实际设置一致（OLLAMA_CONTEXT_LENGTH，Claude Code 至少要 32K）。会话用到它的七成就会换新会话，避免被截断。</div>
          <Field label="令牌（可选）">
            <input type="password" value={token} onChange={(e) => { setToken(e.target.value); setClearToken(false); }} placeholder={initial?.has_token && !clearToken ? "已保存在钥匙串，留空不改" : "没有认证就留空"} />
            {initial?.has_token && <label className="hint"><input type="checkbox" checked={clearToken} onChange={(e) => setClearToken(e.target.checked)} /> 清除已保存的令牌</label>}</Field>
          <Field label="额外环境变量（可选，每行 KEY=值）"><textarea rows={2} value={env} onChange={(e) => setEnv(e.target.value)} placeholder="API_TIMEOUT_MS=600000" /></Field>
          {test && <div className={`note ${test.ok ? "ok" : "bad"}`} style={{ whiteSpace: "pre-wrap" }}>{test.ok ? "连接正常" : "连接失败"}（{(test.millis / 1000).toFixed(1)} 秒）：{test.detail}</div>}
          <div className="form-btns">
            <button className="btn" disabled={busy === "test" || !list.length} title="发一个最小请求，检查网络、模型名和接口是否兼容。第一次要把模型载入显存，可能要等一会儿" onClick={() => void act("test", async () => setTest(await api.testEndpoint(url, list[0], token || null, initial?.id ?? null)))}>{busy === "test" ? "测试中…" : `测试连接（${list[0] ?? "先填模型"}）`}</button>
            <button className="btn primary" disabled={busy === "save"} onClick={() => void act("save", async () => { if (initial) await api.updateEndpoint(initial.id, input()); else await api.addEndpoint(input()); await onSaved(); })}>保存</button>
          </div>
        </div>
    </Modal>
  );
}
