import { useCallback, useEffect, useState } from "react";
import { api } from "./api";
import type { DocFile } from "./types";

/** 很小的 Markdown 渲染：标题、列表、代码块、引用、粗体和行内代码；不引入依赖，也不执行任何 HTML。 */
function Md({ text }: { text: string }) {
  const inline = (s: string) => s.split(/(`[^`]+`|\*\*[^*]+\*\*)/g).map((p, i) => p.startsWith("`") && p.endsWith("`") && p.length > 2 ? <code key={i}>{p.slice(1, -1)}</code> : p.startsWith("**") && p.endsWith("**") && p.length > 4 ? <b key={i}>{p.slice(2, -2)}</b> : <span key={i}>{p}</span>);
  const out: React.ReactElement[] = [];
  const lines = text.split("\n");
  for (let i = 0; i < lines.length; i++) {
    const l = lines[i];
    if (l.startsWith("```")) {
      const buf: string[] = [];
      for (i++; i < lines.length && !lines[i].startsWith("```"); i++) buf.push(lines[i]);
      out.push(<pre key={i} className="md-code">{buf.join("\n")}</pre>);
    } else if (/^#{1,4} /.test(l)) {
      const n = l.match(/^#+/)![0].length;
      const Tag = (`h${Math.min(n + 1, 5)}`) as "h2";
      out.push(<Tag key={i} style={{ margin: "14px 0 6px" }}>{inline(l.replace(/^#+ /, ""))}</Tag>);
    } else if (/^\s*[-*] /.test(l)) {
      out.push(<div key={i} style={{ paddingLeft: 16 + (l.match(/^\s*/)![0].length) * 4, textIndent: -12 }}>• {inline(l.replace(/^\s*[-*] /, ""))}</div>);
    } else if (l.startsWith("> ")) {
      out.push(<div key={i} className="hint" style={{ borderLeft: "3px solid var(--line)", paddingLeft: 10, margin: "6px 0" }}>{inline(l.slice(2))}</div>);
    } else if (l.trim() === "") {
      out.push(<div key={i} style={{ height: 6 }} />);
    } else {
      out.push(<div key={i}>{inline(l)}</div>);
    }
  }
  return <div className="md">{out}</div>;
}

export function DocsView({ projectId, hasLead, onError }: { projectId: string; hasLead: boolean; onError: (m: string) => void }) {
  const [files, setFiles] = useState<DocFile[] | null>(null);
  const [sel, setSel] = useState<string | null>(null);
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState("");

  const load = useCallback(async () => {
    try { const f = await api.listDocs(projectId); setFiles(f); setSel((s) => (s && f.some((x) => x.path === s) ? s : f[0]?.path ?? null)); } catch (e) { onError(String(e)); setFiles([]); }
  }, [projectId, onError]);
  useEffect(() => { void load(); }, [load]);
  useEffect(() => { if (!sel) { setText(""); return; } void api.readDoc(projectId, sel).then(setText).catch((e) => { setText(""); onError(String(e)); }); }, [projectId, sel, onError]);

  return (
    <div className="split">
      <aside className="split-side" style={{ width: "clamp(200px, 24vw, 260px)" }}>
        <div className="split-head"><b>项目文档</b>{files && <span className="hint">{files.length} 份</span>}</div>
        <div className="split-list">
          {files && files.length === 0 && <div className="hint" style={{ padding: 12 }}>主分支的 docs/ 里还没有 Markdown 文档。{hasLead ? "可以让负责人生成。" : ""}</div>}
          {files?.map((f) => (
            <button key={f.path} className={`mitem ${sel === f.path ? "on" : ""}`} onClick={() => setSel(f.path)}>
              <div className="mitem-title">{f.path.replace(/^docs\//, "")}</div>
              <div className="hint">{f.managed ? "负责人维护" : "项目文档"} · {(f.size / 1024).toFixed(1)} KB</div>
            </button>
          ))}
        </div>
      </aside>
      <div className="split-main" style={{ overflow: "auto" }}>
        <div className="page wide">
          <div className="page-head">
            <div><h2>文档</h2><div className="hint">读的是主分支上 docs/ 下的 Markdown。负责人维护的放在 docs/crewforge/：决策记录、已知问题、计划进度由数据生成，概览由负责人读代码撰写；更新走和代码一样的分支与合并流程，署名是负责人。</div></div>
            {hasLead && (
              <button className="btn primary" disabled={busy} title="决策记录、已知问题、计划进度由数据生成；概览由负责人读代码撰写（一次模型调用）。内容没变就不产生新提交。" onClick={async () => {
                setBusy(true); setNote("");
                try { const r = await api.leadUpdateDocs(projectId); setNote(r.commit ? `${r.note}，提交 ${r.commit.slice(0, 8)}` : r.note); await load(); } catch (e) { onError(String(e)); } finally { setBusy(false); }
              }}>{busy ? "负责人更新中…" : "让负责人更新文档"}</button>
            )}
          </div>
          {note && <div className="note" style={{ marginBottom: 10 }}>{note}</div>}
          {sel ? <section className="panel-box"><Md text={text} /></section> : <div className="hint">选择左边的文档查看。</div>}
        </div>
      </div>
    </div>
  );
}
