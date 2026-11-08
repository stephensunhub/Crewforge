import { useState } from "react";
import { api } from "./api";
import type { TaskView } from "./types";

const STATUS_LABEL: Record<string, string> = { active: "进行中", merged: "已合并", abandoned: "已放弃" };

function DiffModal({ title, diff, onClose }: { title: string; diff: string; onClose: () => void }) {
  return (
    <div className="modal-back" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-head">
          <b>{title}</b>
          <button className="btn" onClick={onClose}>关闭</button>
        </div>
        <pre className="diff">
          {diff.trim() === "" ? "（没有改动）" : diff.split("\n").map((l, i) => {
            const cls = l.startsWith("+++") || l.startsWith("---") || l.startsWith("diff ") || l.startsWith("index ")
              ? "meta" : l.startsWith("@@") ? "hunk" : l.startsWith("+") ? "add" : l.startsWith("-") ? "del" : "";
            return <div key={i} className={cls}>{l || " "}</div>;
          })}
        </pre>
      </div>
    </div>
  );
}

export function TasksPanel({
  views, busy, onChanged, onError,
}: {
  views: TaskView[];
  busy: string[];
  onChanged: () => void;
  onError: (m: string) => void;
}) {
  const [diff, setDiff] = useState<{ title: string; text: string } | null>(null);
  const [working, setWorking] = useState<string | null>(null);
  const nameOf = new Map(views.map((v) => [v.task.id, v.employee_name]));
  const active = views.filter((v) => v.task.status === "active");
  const done = views.filter((v) => v.task.status !== "active");

  async function showDiff(v: TaskView) {
    try { setDiff({ title: `${v.employee_name} · ${v.task.title}`, text: await api.taskDiff(v.task.id) }); }
    catch (e) { onError(String(e)); }
  }
  async function merge(v: TaskView) {
    const warn = v.overlaps.length ? `\n\n注意：与其他进行中的任务同时修改了 ${v.overlaps.map((o) => o.path).join("、")}。` : "";
    if (!(await api.confirm(`将「${v.task.title}」合并到 ${v.task.base_branch}？\n改动会压缩成一个提交。${warn}`, "确认合并"))) return;
    setWorking(v.task.id);
    try {
      const r = await api.mergeTask(v.task.id);
      if (r.outcome === "conflict") onError(`合并冲突，未做任何改动：${r.files.join("、")}。请让员工先处理冲突。`);
      else if (r.outcome === "blocked") onError(`无法合并：${r.reason}`);
      onChanged();
    } catch (e) { onError(String(e)); } finally { setWorking(null); }
  }
  async function abandon(v: TaskView) {
    if (!(await api.confirm(`放弃「${v.task.title}」？\n不会删除任何内容：改动会保存为 WIP 提交，分支 ${v.task.branch} 会保留。`, "确认放弃"))) return;
    setWorking(v.task.id);
    try { await api.abandonTask(v.task.id); onChanged(); } catch (e) { onError(String(e)); } finally { setWorking(null); }
  }

  return (
    <aside className="tasks">
      <div className="tasks-head"><b>任务与工作区</b><span className="hint">每名员工在独立的 Git 工作区里工作</span></div>
      <div className="tasks-body">
        {views.length === 0 && <div className="hint" style={{ padding: 14 }}>还没有任务。给员工分配工作后，这里会显示他们的分支、改动和合并状态。</div>}
        {active.map((v) => {
          const isBusy = busy.includes(v.employee_name);
          const others = [...new Set(v.overlaps.flatMap((o) => o.task_ids).filter((id) => id !== v.task.id).map((id) => nameOf.get(id) ?? "?"))];
          return (
            <div className="task" key={v.task.id}>
              <div className="task-title">{v.task.title}</div>
              <div className="task-meta">
                <span>{v.employee_name}</span>
                <code title={v.task.branch}>{v.task.branch}</code>
                {isBusy && <span className="badge run">执行中</span>}
              </div>
              <div className="task-files">
                {v.files.length === 0 ? <span className="hint">暂无改动</span> : (
                  <>
                    <span>{v.files.length} 个文件</span>
                    <span className="add">+{v.files.reduce((a, f) => a + (f.insertions ?? 0), 0)}</span>
                    <span className="del">−{v.files.reduce((a, f) => a + (f.deletions ?? 0), 0)}</span>
                  </>
                )}
              </div>
              {v.files.length > 0 && (
                <ul className="flist">
                  {v.files.slice(0, 6).map((f) => <li key={f.path}><code>{f.path}</code>{f.untracked && <span className="hint"> 新文件</span>}</li>)}
                  {v.files.length > 6 && <li className="hint">… 另有 {v.files.length - 6} 个</li>}
                </ul>
              )}
              {v.overlaps.length > 0 && (
                <div className="note warn">⚠ 与 {others.join("、")} 同时修改：{v.overlaps.map((o) => o.path).join("、")}</div>
              )}
              {v.merge && (v.merge.clean
                ? <div className="note ok">✓ 与 {v.task.base_branch} 无冲突，可合并</div>
                : <div className="note bad">✗ 合并会冲突：{v.merge.conflicts.join("、")}</div>)}
              <div className="task-btns">
                <button className="btn" onClick={() => showDiff(v)}>查看改动</button>
                <button className="btn primary" disabled={isBusy || working === v.task.id || (v.merge != null && !v.merge.clean)} onClick={() => merge(v)}>合并</button>
                <button className="btn" disabled={isBusy || working === v.task.id} onClick={() => abandon(v)}>放弃</button>
                <button className="btn" onClick={() => api.reveal(v.task.id).catch((e) => onError(String(e)))}>在访达显示</button>
              </div>
            </div>
          );
        })}
        {done.length > 0 && <h4>历史</h4>}
        {done.map((v) => (
          <div className="task done" key={v.task.id}>
            <div className="task-title">{v.task.title}</div>
            <div className="task-meta">
              <span>{v.employee_name}</span>
              <span className={`badge ${v.task.status}`}>{STATUS_LABEL[v.task.status]}</span>
              {v.task.merged_commit && <code>{v.task.merged_commit.slice(0, 8)}</code>}
              <button className="link" onClick={() => showDiff(v)}>查看改动</button>
            </div>
          </div>
        ))}
      </div>
      {diff && <DiffModal title={diff.title} diff={diff.text} onClose={() => setDiff(null)} />}
    </aside>
  );
}
