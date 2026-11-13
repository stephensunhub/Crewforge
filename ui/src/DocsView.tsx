import { useCallback, useEffect, useState } from "react";
import { api } from "./api";
import type { DocFile, DocVersion } from "./types";
import { Md } from "./common";

/** 项目文档：固定五份（长期背景，Kiro 的 steering 对应物）。路径沿用已有的文件，没有的显示“未建立”。 */
const PROJECT_DOCS: { path: string; title: string; hint: string }[] = [
  { path: "docs/architecture.md", title: "架构", hint: "系统现在是怎么划分的（架构师维护）" },
  { path: "docs/crewforge/decisions.md", title: "决策", hint: "为什么这样选、放弃了什么（由议题裁决自动生成）" },
  { path: "docs/crewforge/known-issues.md", title: "风险", hint: "已知问题与遗留风险（自动生成）" },
  { path: "docs/crewforge/overview.md", title: "项目背景与约定", hint: "项目是什么、怎么运行、关键约束（负责人撰写）" },
  { path: "docs/conventions.md", title: "文档与代码规范", hint: "命名、目录、风格、测试、文档约定（架构师维护）" },
];
const when = (ms: number) => new Date(ms).toLocaleString("zh-CN", { year: "numeric", month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit" });
const specSlug = (p: string) => p.match(/^docs\/specs\/([^/]+)\//)?.[1] ?? null;
const SPEC_FILE: Record<string, string> = { "requirements.md": "业务需求", "bugfix.md": "缺陷规格", "design.md": "技术方案" };

export function DocsView({ projectId, hasLead, onError }: { projectId: string; hasLead: boolean; onError: (m: string) => void }) {
  const [files, setFiles] = useState<DocFile[] | null>(null);
  const [sel, setSel] = useState<string | null>(null);
  const [text, setText] = useState("");
  const [versions, setVersions] = useState<DocVersion[]>([]);
  const [diff, setDiff] = useState<{ v: DocVersion; text: string } | null>(null);
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState("");

  const load = useCallback(async () => {
    try { const f = await api.listDocs(projectId); setFiles(f); setSel((s) => (s && f.some((x) => x.path === s) ? s : (PROJECT_DOCS.find((d) => f.some((x) => x.path === d.path))?.path ?? f[0]?.path ?? null))); } catch (e) { onError(String(e)); setFiles([]); }
  }, [projectId, onError]);
  useEffect(() => { void load(); }, [load]);
  useEffect(() => {
    setDiff(null);
    if (!sel) { setText(""); setVersions([]); return; }
    void api.readDoc(projectId, sel).then(setText).catch((e) => { setText(""); onError(String(e)); });
    void api.docHistory(projectId, sel).then(setVersions).catch(() => setVersions([]));
  }, [projectId, sel, onError]);

  const have = new Set((files ?? []).map((f) => f.path));
  const fixed = new Set(PROJECT_DOCS.map((d) => d.path));
  const specs = (files ?? []).filter((f) => specSlug(f.path));
  const bySlug = specs.reduce<Record<string, DocFile[]>>((m, f) => { (m[specSlug(f.path)!] ??= []).push(f); return m; }, {});
  const others = (files ?? []).filter((f) => !fixed.has(f.path) && !specSlug(f.path));
  const selDoc = PROJECT_DOCS.find((d) => d.path === sel);
  const Item = ({ path, title, sub, missing }: { path: string; title: string; sub: string; missing?: boolean }) => (
    <button className={`mitem ${sel === path ? "on" : ""}`} disabled={missing} onClick={() => setSel(path)}>
      <div className="mitem-title">{title}</div><div className="hint">{sub}</div>
    </button>
  );

  return (
    <div className="split">
      <aside className="split-side" style={{ width: "clamp(220px, 26vw, 280px)" }}>
        <div className="split-head"><b>文档</b>{files && <span className="hint">{files.length} 份</span>}</div>
        <div className="split-list">
          <div className="hint" style={{ padding: "8px 12px 2px" }}><b>项目文档</b>（长期背景）</div>
          {PROJECT_DOCS.map((d) => <Item key={d.path} path={d.path} title={d.title} sub={have.has(d.path) ? d.hint : "未建立"} missing={!have.has(d.path)} />)}
          <div className="hint" style={{ padding: "10px 12px 2px" }}><b>规格文档</b>（每个需求一份：业务 + 技术）</div>
          {Object.keys(bySlug).length === 0 && <div className="hint" style={{ padding: "2px 12px 8px" }}>还没有。在「需求」里采纳一个需求后会生成。</div>}
          {Object.entries(bySlug).map(([slug, fs]) => (
            <div key={slug}>
              <div className="hint" style={{ padding: "4px 12px 0" }}>📁 {slug}</div>
              {fs.map((f) => <Item key={f.path} path={f.path} title={SPEC_FILE[f.path.split("/").pop() ?? ""] ?? f.path.split("/").pop() ?? f.path} sub={`${(f.size / 1024).toFixed(1)} KB`} />)}
            </div>
          ))}
          {others.length > 0 && <div className="hint" style={{ padding: "10px 12px 2px" }}><b>其他文档</b></div>}
          {others.map((f) => <Item key={f.path} path={f.path} title={f.path.replace(/^docs\//, "")} sub={`${f.managed ? "负责人维护" : "项目文档"} · ${(f.size / 1024).toFixed(1)} KB`} />)}
        </div>
      </aside>
      <div className="split-main" style={{ overflow: "auto" }}>
        <div className="page wide">
          <div className="page-head">
            <div><h2>{selDoc?.title ?? (sel ? sel.replace(/^docs\//, "") : "文档")}</h2><div className="hint">读的是主分支上 docs/ 下的 Markdown，只读。{selDoc?.hint ?? "分两类：项目文档是长期背景，规格文档随功能演进。"}每份文档的版本列表以 git 历史为准：每次合并算一个版本。</div></div>
            {hasLead && (
              <button className="btn primary" disabled={busy} title="决策、风险由数据生成；项目背景与约定由负责人读代码撰写（一次模型调用）。内容没变就不产生新提交。" onClick={async () => {
                setBusy(true); setNote("");
                try { const r = await api.leadUpdateDocs(projectId); setNote(r.commit ? `${r.note}，提交 ${r.commit.slice(0, 8)}` : r.note); await load(); } catch (e) { onError(String(e)); } finally { setBusy(false); }
              }}>{busy ? "负责人更新中…" : "让负责人更新文档"}</button>
            )}
          </div>
          {note && <div className="note" style={{ marginBottom: 10 }}>{note}</div>}
          {sel ? (
            <>
              <section className="panel-box"><Md text={text} /></section>
              <section className="panel-box" style={{ marginTop: 12 }}>
                <h4 className="group-title">版本列表 <span className="u-count">{versions.length}</span></h4>
                {versions.length === 0 && <div className="hint">还没有提交历史。</div>}
                <table className="tbl">
                  <tbody>
                    {versions.map((v) => (
                      <tr key={v.commit} className="clickable" onClick={async () => { try { setDiff({ v, text: await api.docVersionDiff(projectId, sel, v.commit) }); } catch (e) { onError(String(e)); } }}>
                        <td>v{v.version}</td><td className="hint">{when(v.at)}</td><td>{v.author}</td><td className="tbl-title" title={v.subject}>{v.subject}</td>
                        <td className="hint">+{v.added} −{v.removed}</td><td className="hint">{v.commit.slice(0, 8)}</td>
                      </tr>))}
                  </tbody>
                </table>
                {diff && <div style={{ marginTop: 8 }}><div className="hint">v{diff.v.version} 的改动：</div><pre className="doc-diff">{diff.text}</pre></div>}
              </section>
            </>
          ) : <div className="hint">选择左边的文档查看。</div>}
        </div>
      </div>
    </div>
  );
}
