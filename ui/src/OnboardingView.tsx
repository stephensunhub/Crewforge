import { useCallback, useEffect, useMemo, useState } from "react";
import { api } from "./api";
import { Field, Modal, hhmm, short } from "./common";
import type { BaselineCommand, CommandResult, EventRow, Member, OnboardingState, ScanReport } from "./types";

const STATUS_LABEL: Record<string, string> = { passed: "通过", failed: "失败", timeout: "超时", skipped: "跳过", error: "出错", running: "运行中" };
const REPORT_LABEL = { architect: "全栈工程师评估", qa: "QA 评估" } as const;

const KIND_OPTIONS: [string, string][] = [["setup", "准备"], ["build", "构建"], ["test", "测试"], ["lint", "检查"], ["parity", "对照"]];

function Chips({ label, items }: { label: string; items: string[] }) {
  return (
    <div className="scan-row"><span className="scan-k">{label}</span>
      <span className="card-meta">{items.length === 0 ? <span className="hint">未识别</span> : items.map((x) => <span className="chip static" key={x}>{x}</span>)}</span></div>
  );
}

function ScanView({ s }: { s: ScanReport }) {
  const max = Math.max(1, ...s.languages.map((l) => l[1]));
  return (
    <div className="scan">
      {s.notes.map((n) => <div className="note warn" key={n}>⚠ {n}</div>)}
      <div className="scan-row"><span className="scan-k">语言</span>
        <div className="langs">{s.languages.slice(0, 6).map(([l, n]) => (
          <div key={l} className="lang"><span>{l}</span><i style={{ width: `${(n / max) * 100}%` }} /><b>{n}</b></div>))}
          {s.languages.length === 0 && <span className="hint">未识别</span>}</div></div>
      <Chips label="包管理" items={s.package_managers} /><Chips label="构建工具" items={s.build_tools} />
      <Chips label="框架" items={s.frameworks} /><Chips label="数据库/中间件" items={s.databases} />
      <Chips label="测试框架" items={s.test_frameworks} /><Chips label="CI/CD" items={s.ci} />
      <Chips label="API 定义" items={s.api_definitions} /><Chips label="数据库 Schema" items={s.db_schemas} />
      <Chips label="文档" items={s.docs} />
      {s.modules.length > 0 && (
        <div className="scan-row"><span className="scan-k">模块</span>
          <table className="mod-table"><tbody>{s.modules.slice(0, 30).map((m) => (
            <tr key={m.path}><td><b>{m.name}</b></td><td className="hint">{m.path}</td><td className="hint">{m.files} 个文件</td><td className="hint">{m.kind}</td></tr>))}</tbody></table></div>
      )}
      {s.git.is_repo && (
        <div className="scan-row"><span className="scan-k">Git</span>
          <div className="hint">
            分支 {s.git.branch ?? "（游离 HEAD）"} · {s.git.commit_count ?? "?"} 次提交 · {s.git.branch_count ?? "?"} 个分支 · {s.git.tag_count ?? "?"} 个标签
            {s.git.first_commit_date && ` · 始于 ${s.git.first_commit_date}`}
            {s.git.authors.length > 0 && <div>主要贡献者：{s.git.authors.map(([n, c]) => `${n}（${c}）`).join("、")}</div>}
            {s.git.remotes.length > 0 && <div>远程：{s.git.remotes.join("；")}</div>}
            {s.git.recent.slice(0, 5).map((r) => <div key={r}><code>{r}</code></div>)}
          </div></div>
      )}
    </div>
  );
}

function Step({ n, title, done, children }: { n: number; title: string; done: boolean; children: React.ReactNode }) {
  return (
    <section className="step">
      <div className="step-head"><span className={`step-n ${done ? "done" : ""}`}>{done ? "✓" : n}</span><b>{title}</b></div>
      <div className="step-body">{children}</div>
    </section>
  );
}

export function OnboardingView({ projectId, members, events, running, onError, onChanged }: {
  projectId: string; members: Member[]; events: EventRow[]; running: string[]; onError: (m: string) => void; onChanged: () => void;
}) {
  const [st, setSt] = useState<OnboardingState | null>(null);
  const [picked, setPicked] = useState<Record<string, boolean>>({});
  const [cmds, setCmds] = useState<BaselineCommand[]>([]);
  const [busy, setBusy] = useState<string>("");
  const [emp, setEmp] = useState<{ architect: string; qa: string }>({ architect: "", qa: "" });
  const [skip, setSkip] = useState(false);
  const [detail, setDetail] = useState<CommandResult | null>(null);

  const load = useCallback(async () => {
    try { setSt(await api.onboardingState(projectId)); } catch (e) { onError(String(e)); }
  }, [projectId, onError]);
  useEffect(() => { void load(); }, [load]);

  // 把扫描建议的命令装进可编辑列表（仅在首次拿到扫描结果、且用户还没改过时）
  useEffect(() => {
    if (st?.scan && cmds.length === 0) {
      setCmds(st.scan.commands);
      setPicked(Object.fromEntries(st.scan.commands.map((c) => [c.id, c.default_on])));
    }
  }, [st?.scan, cmds.length]);
  useEffect(() => {
    setEmp((p) => ({
      architect: p.architect || members.find((m) => m.role === "architect")?.employee_id || members.find((m) => m.role === "lead")?.employee_id || members[0]?.employee_id || "",
      qa: p.qa || members.find((m) => m.role === "qa")?.employee_id || members.find((m) => m.role === "reviewer")?.employee_id || members[0]?.employee_id || "",
    }));
  }, [members]);

  // 导入相关事件到来时刷新（基线进度、报告写好）
  const last = useMemo(() => events.reduce((m, e) => (e.project_id === projectId && /^(baseline_|onboarding_)/.test(e.kind) ? Math.max(m, e.id) : m), 0), [events, projectId]);
  useEffect(() => { if (last) void load(); }, [last, load]);

  const live = useMemo(() => {
    const m = new Map<string, string>();
    let startedAt = 0;
    for (const e of events) {
      if (e.project_id !== projectId) continue;
      if (e.kind === "baseline_started") { m.clear(); startedAt = e.id; }
      if (e.id > startedAt && (e.kind === "baseline_command_started" || e.kind === "baseline_command_finished")) m.set(e.payload.command_id, e.payload.status);
    }
    return m;
  }, [events, projectId]);

  if (!st) return <div className="hint" style={{ padding: 20 }}>加载中…</div>;
  const scan = st.scan;
  const base = st.baseline;
  const baseRunning = base?.status === "running";
  const selected = cmds.filter((c) => picked[c.id]);
  const reports = Object.fromEntries(st.reports.map((r) => [r.kind, r]));
  const confirmed = st.status === "confirmed";
  const hasDoneBaseline = base?.status === "done" || (base && base.results.length > 0 && base.status !== "running");

  async function run<T>(key: string, fn: () => Promise<T>) {
    setBusy(key);
    try { await fn(); await load(); onChanged(); } catch (e) { onError(String(e)); await load(); } finally { setBusy(""); }
  }

  async function startBaseline() {
    if (selected.length === 0) { onError("请至少勾选一条命令"); return; }
    const ok = await api.confirm(`将在已提交 HEAD 的干净检出里运行这些命令（不会改动你的工作目录）：\n\n${selected.map((c) => (c.kind === "parity" ? `• [对照] 旧：${c.command}${c.candidate?.trim() ? `\n    新：${c.candidate}` : ""}` : `• ${c.command}`)).join("\n")}\n\n这会执行项目里的构建脚本和测试代码。确认运行？`, "运行测试基线");
    if (ok) await run("baseline", () => api.runBaseline(projectId, selected.map((c) => ({ ...c, candidate: c.kind === "parity" && c.candidate?.trim() ? c.candidate.trim() : null }))));
  }

  return (
    <div className="page narrow wide">
      <div className="page-head">
        <div><h2>项目导入</h2>
          <div className="hint">{confirmed ? "已确认导入，项目已进入 AI 团队维护阶段。" : "先只读地了解项目、建立测试基线，确认后才会让员工开始开发。基线能区分“导入前就有的问题”和“AI 引入的问题”。"}</div></div>
        <span className={`badge ${confirmed ? "merged" : "run"}`}>{confirmed ? "已确认" : "导入中"}</span>
      </div>

      <Step n={1} title="只读扫描" done={!!scan}>
        <div className="hint">识别语言、框架、数据库、构建与测试工具、CI、API 定义和 Git 历史。不调用 AI，不执行项目代码，速度很快，随时可以重新扫描。</div>
        <div><button className="btn primary" disabled={busy === "scan"} onClick={() => void run("scan", () => api.runScan(projectId))}>{busy === "scan" ? "扫描中…" : scan ? "重新扫描" : "开始扫描"}</button>
          {scan && <span className="hint" style={{ marginLeft: 10 }}>上次扫描 {hhmm(scan.scanned_at)} · {scan.total_files} 个文件</span>}</div>
        {scan && <ScanView s={scan} />}
      </Step>

      <Step n={2} title="测试基线" done={!!hasDoneBaseline}>
        {!scan ? <div className="hint">先完成扫描。</div> : (
          <>
            <div className="hint">基线记录项目<b>导入时</b>的原始构建和测试结果。命令在已提交 HEAD 的干净检出里运行，不会改动你的目录；未提交的改动不包含在内。运行前请确认命令，它们会执行项目里的代码。</div>
            {scan.git.dirty && <div className="note warn">⚠ 工作区有未提交的改动，基线不包含它们。</div>}
            <div className="cmds">
              {cmds.map((c, i) => {
                const upd = (patch: Partial<BaselineCommand>) => setCmds(cmds.map((x, j) => (j === i ? { ...x, ...patch } : x)));
                const parity = c.kind === "parity";
                return (
                  <div className="cmd-row" key={c.id} style={parity ? { flexWrap: "wrap" } : undefined}>
                    <input type="checkbox" checked={!!picked[c.id]} onChange={(e) => setPicked({ ...picked, [c.id]: e.target.checked })} />
                    <b style={{ width: 130 }}>{c.label}</b>
                    <select value={c.kind} onChange={(e) => upd({ kind: e.target.value, candidate: e.target.value === "parity" ? (c.candidate ?? "") : null })}>
                      {KIND_OPTIONS.map(([v, l]) => <option key={v} value={v}>{l}</option>)}
                    </select>
                    <input value={c.command} placeholder={parity ? "旧实现的命令（输出作为标准答案）" : "命令"} onChange={(e) => upd({ command: e.target.value })} />
                    <span className="hint" style={{ width: 150 }}>{short(c.reason, 24)}</span>
                    {parity && (
                      <div style={{ flexBasis: "100%", display: "flex", gap: 8, paddingLeft: 26, alignItems: "center" }}>
                        <span className="hint" style={{ width: 130 }}>新实现的命令</span>
                        <input style={{ flex: 1 }} value={c.candidate ?? ""} placeholder="留空则两边都运行上面的命令；检查任务时运行这条，输出必须与标准答案逐字一致"
                          onChange={(e) => upd({ candidate: e.target.value })} />
                      </div>
                    )}
                  </div>
                );
              })}
              <button className="btn" onClick={() => {
                const id = `custom-${cmds.length}-${Date.now() % 100000}`;
                setCmds([...cmds, { id, label: "自定义", kind: "test", command: "", reason: "手动添加", default_on: true }]);
                setPicked({ ...picked, [id]: true });
              }}>＋ 添加命令</button>
            </div>
            <div><button className="btn primary" disabled={baseRunning || busy === "baseline" || selected.some((c) => !c.command.trim())} onClick={() => void startBaseline()}>
              {baseRunning || busy === "baseline" ? "基线运行中…" : base ? "重新建立基线" : "运行基线"}</button></div>
            {(baseRunning || busy === "baseline") && (
              <div className="progress">{selected.map((c) => <div key={c.id} className="hint">{live.get(c.id) === "running" ? "▶" : live.has(c.id) ? "✓" : "·"} {c.label}：{live.get(c.id) === "running" ? "运行中" : live.has(c.id) ? STATUS_LABEL[live.get(c.id)!] : "等待"}</div>)}</div>
            )}
            {base && base.results.length > 0 && (
              <div>
                <div className="hint">基线提交 <code>{base.commit_sha.slice(0, 8)}</code> · {base.finished_at ? hhmm(base.finished_at) : ""}</div>
                <table className="res-table"><tbody>{base.results.map((r) => (
                  <tr key={r.id}>
                    <td><b>{r.label}</b><div className="hint"><code>{r.command}</code></div></td>
                    <td><span className={`badge rs-${r.status}`}>{STATUS_LABEL[r.status]}</span></td>
                    <td className="hint">{(r.duration_ms / 1000).toFixed(1)}s{r.exit_code != null && r.status === "failed" ? ` · 退出码 ${r.exit_code}` : ""}</td>
                    <td>{r.failed_tests.length > 0 ? <span style={{ color: "var(--bad)" }}>{r.failed_tests.length} 个测试失败</span>
                      : r.status === "failed" ? <span className="hint">未解析出逐项失败</span> : ""}</td>
                    <td><button className="link" onClick={() => setDetail(r)}>查看输出</button></td>
                  </tr>))}</tbody></table>
                {base.results.some((r) => r.status === "failed" && !r.parsed) && <div className="note warn">⚠ 有命令失败但无法解析出逐项失败，之后对这类命令只能做整体对比，无法精确区分来源。</div>}
              </div>
            )}
            {base?.status === "failed" && <div className="note bad">上一次基线没有完成（出错或被中断）。</div>}
          </>
        )}
      </Step>

      <Step n={3} title="AI 联合评估" done={!!(reports.architect && reports.qa)}>
        {!scan ? <div className="hint">先完成扫描。</div> : members.length === 0 ? <div className="hint">项目里还没有成员，请先到“成员”页添加员工。</div> : (
          <>
            <div className="hint">让员工以只读方式查看代码并写评估。基线里已有的失败会明确告知他们是历史问题，不会归咎于后续修改。每份评估是一次模型调用。</div>
            {(["architect", "qa"] as const).map((k) => {
              const r = reports[k];
              const who = emp[k];
              const name = members.find((m) => m.employee_id === who)?.name ?? "";
              return (
                <div className="report" key={k}>
                  <div className="report-head">
                    <b>{REPORT_LABEL[k]}</b>
                    <select value={who} onChange={(e) => setEmp({ ...emp, [k]: e.target.value })}>{members.map((m) => <option key={m.employee_id} value={m.employee_id}>{m.name}</option>)}</select>
                    <button className="btn" disabled={!who || busy === `eval-${k}` || running.includes(name)} onClick={() => void run(`eval-${k}`, () => api.evaluateProject(projectId, k, who))}>
                      {busy === `eval-${k}` ? "评估中…" : r ? "重新评估" : "请求评估"}</button>
                    {r && <span className="hint">{hhmm(r.created_at)}</span>}
                  </div>
                  {r && <pre className="report-body">{r.content}</pre>}
                </div>
              );
            })}
          </>
        )}
      </Step>

      <Step n={4} title="人类确认" done={confirmed}>
        {confirmed ? <div className="hint">已确认。技术栈和测试基线已作为已验证的项目记忆保存。之后可以用任务面板里的“对比基线检查”来判断失败的来源。</div> : (
          <>
            <div className="hint">确认后项目进入维护阶段，员工才能接开发任务；同时会把技术栈和基线结果存为已验证的项目记忆。</div>
            {!hasDoneBaseline && <label className="tog"><input type="checkbox" checked={skip} onChange={(e) => setSkip(e.target.checked)} />没有基线，我了解之后无法区分历史问题与 AI 引入的问题，仍然跳过</label>}
            <div><button className="btn primary" disabled={!scan || (!hasDoneBaseline && !skip) || baseRunning} onClick={async () => {
              if (await api.confirm("确认导入？之后员工就可以开始在这个项目里开发了。", "确认导入")) await run("confirm", () => api.confirmOnboarding(projectId, !hasDoneBaseline && skip));
            }}>确认导入，进入维护阶段</button></div>
          </>
        )}
      </Step>

      {detail && (
        <Modal title={`${detail.label} · ${STATUS_LABEL[detail.status]}`} onClose={() => setDetail(null)} width={860}>
          <div className="hint"><code>{detail.command}</code></div>
          {detail.failed_tests.length > 0 && <Field label="失败的测试"><div className="mono">{detail.failed_tests.join("\n")}</div></Field>}
          <pre className="diff" style={{ maxHeight: "50vh" }}>{detail.output_tail || "（没有输出）"}</pre>
        </Modal>
      )}
    </div>
  );
}
