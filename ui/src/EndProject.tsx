import { useEffect, useState } from "react";
import { api } from "./api";
import { Modal } from "./common";
import type { EndedProject, ProjectSummary } from "./types";

type Choice = "completed" | "archived" | "discard";

const CHOICES: { key: Choice; title: string; desc: string }[] = [
  { key: "completed", title: "已完成", desc: "项目做完了。要求所有任务都已合并或放弃。只做标记，什么都不删，之后可以恢复。" },
  { key: "archived", title: "归档", desc: "标记项目结束，从列表里收起来。进行中的任务会被放弃（改动保存为 WIP，分支和工作区都保留），不删除任何目录，之后可以恢复。" },
  { key: "discard", title: "不做了（彻底删除）", desc: "删除员工在这个项目里的 worktree 和分支，以及项目的全部记录（群聊、任务、会议、议题、计划、记忆等）。可以同时把项目文件夹移到废纸篓。不能恢复；用量记录会保留。" },
];

/** 结束项目：已完成 / 归档 / 不做了。 */
export function EndProjectModal({ summary, onClose, onDone, onError }: {
  summary: ProjectSummary; onClose: () => void; onDone: () => void; onError: (m: string) => void;
}) {
  const p = summary.project;
  const imported = p.onboarding !== "none";
  const [choice, setChoice] = useState<Choice>("archived");
  const [reason, setReason] = useState("");
  const [name, setName] = useState("");
  const [folder, setFolder] = useState(!imported);
  const [dirName, setDirName] = useState("");
  const base = p.path.split("/").filter(Boolean).pop() ?? "";
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState("");
  const active = summary.active_tasks;
  const blocked = choice === "completed" && active > 0;
  const ready = choice !== "discard" || (name.trim() === p.name && (!(imported && folder) || dirName.trim() === base));

  async function submit() {
    setBusy(true); setNotice("");
    try {
      if (choice === "discard") {
        const r = await api.discardProject(p.id, name.trim(), folder, dirName.trim());
        if (r.folder_trashed) onError(`项目已删除，文件夹已移到废纸篓：${r.folder_trashed}`);
      } else {
        await api.endProject(p.id, choice, reason.trim());
      }
      onDone();
    } catch (e) { setNotice(String(e).replace(/^Error:\s*/, "")); } finally { setBusy(false); }
  }

  return (
    <Modal title={`结束项目「${p.name}」`} onClose={onClose} width={600}>
      <div className="end-choices">
        {CHOICES.map((c) => (
          <label key={c.key} className={`end-choice ${choice === c.key ? "on" : ""} ${c.key === "discard" ? "danger" : ""}`}>
            <input type="radio" name="end" checked={choice === c.key} onChange={() => setChoice(c.key)} />
            <div><b>{c.title}</b><div className="hint">{c.desc}</div></div>
          </label>
        ))}
      </div>
      {active > 0 && choice !== "discard" && (
        <div className={`note ${blocked ? "bad" : "warn"}`}>
          现在有 {active} 个进行中的任务。{blocked ? "“已完成”要求它们先合并或放弃；可以先去任务页处理，或改选“归档”。" : "归档会自动放弃它们（改动保存为 WIP）。"}
        </div>
      )}
      {choice !== "discard" && (
        <label className="field"><span>原因（可选）</span>
          <input value={reason} placeholder={choice === "completed" ? "例如：已上线" : "例如：需求取消"} onChange={(e) => setReason(e.target.value)} /></label>
      )}
      {choice === "discard" && (
        <>
          <label className="tog">
            <input type="checkbox" checked={folder} onChange={(e) => setFolder(e.target.checked)} />
            同时把项目文件夹移到废纸篓
          </label>
          <div className="hint">
            文件夹：<code>{p.path}</code>。目录必须是干净的 Git 仓库（没有未提交改动）；移到废纸篓，不是永久删除。
          </div>
          {imported && folder && (
            <>
              <div className="note warn">这是<b>导入的已有项目</b>，这个目录可能是你自己的代码仓库。要删除它，请再输入一次目录名确认。</div>
              <label className="field"><span>输入目录名「{base}」确认</span>
                <input value={dirName} placeholder={base} onChange={(e) => setDirName(e.target.value)} /></label>
            </>
          )}
          <label className="field"><span>输入项目名称「{p.name}」确认</span>
            <input value={name} placeholder={p.name} onChange={(e) => setName(e.target.value)} /></label>
        </>
      )}
      {notice && <div className="note bad" style={{ whiteSpace: "pre-wrap" }}>{notice}</div>}
      <div className="row" style={{ gap: 10, marginTop: 14, justifyContent: "flex-end" }}>
        <button className="btn" onClick={onClose}>取消</button>
        <button className={`btn ${choice === "discard" ? "danger" : "primary"}`} disabled={busy || blocked || !ready} onClick={() => void submit()}>
          {busy ? "处理中…" : choice === "discard" ? "彻底删除" : choice === "completed" ? "标记已完成" : "归档"}
        </button>
      </div>
    </Modal>
  );
}

const KIND_LABEL: Record<string, string> = { completed: "已完成", archived: "已归档" };

/** 已结束的项目：可以恢复。 */
export function EndedProjects({ refreshKey, onRestored, onError }: { refreshKey: number; onRestored: () => void; onError: (m: string) => void }) {
  const [list, setList] = useState<EndedProject[]>([]);
  const [open, setOpen] = useState(false);
  useEffect(() => { void api.endedProjects().then(setList).catch((e) => onError(String(e))); }, [refreshKey]); // eslint-disable-line react-hooks/exhaustive-deps
  if (list.length === 0) return null;
  return (
    <div style={{ marginTop: 24 }}>
      <button className="link" onClick={() => setOpen(!open)}>{open ? "▾" : "▸"} 已结束的项目（{list.length}）</button>
      {open && (
        <table className="cost-table" style={{ marginTop: 8 }}>
          <thead><tr><th>项目</th><th>状态</th><th>原因</th><th>时间</th><th /></tr></thead>
          <tbody>{list.map((x) => (
            <tr key={x.project.id}>
              <td><b>{x.project.name}</b><div className="hint path-line" title={x.project.path}>{x.project.path}</div></td>
              <td>{KIND_LABEL[x.end_kind] ?? x.end_kind}</td>
              <td>{x.end_reason || "—"}</td>
              <td>{x.ended_at ? new Date(x.ended_at).toLocaleString("zh-CN", { hour12: false }) : "—"}</td>
              <td><button className="btn" onClick={() => void api.restoreProject(x.project.id).then(onRestored).catch((e) => onError(String(e)))}>恢复</button></td>
            </tr>
          ))}</tbody>
        </table>
      )}
    </div>
  );
}
