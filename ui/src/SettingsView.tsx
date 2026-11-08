import { useEffect, useState } from "react";
import { api } from "./api";
import { Placeholder, hhmm } from "./common";
import type { SchedStatus, SchedulerConfig } from "./types";

export function SettingsView({ sched, onConcurrency, onAction }: {
  sched: SchedStatus | null;
  onConcurrency: (n: number) => void;
  onAction: (fn: () => Promise<unknown>) => Promise<void>;
}) {
  const [version, setVersion] = useState("检测中…");
  const [dir, setDir] = useState("");
  const [cfg, setCfg] = useState<SchedulerConfig | null>(null);
  useEffect(() => {
    api.claudeVersion().then(setVersion).catch((e) => setVersion(`未检测到 claude：${e}`));
    api.dataDir().then(setDir).catch(() => undefined);
  }, []);
  useEffect(() => { if (sched && !cfg) setCfg(sched.config); }, [sched, cfg]);
  const dirty = !!(sched && cfg && JSON.stringify(cfg) !== JSON.stringify(sched.config));
  const pctIn = (k: "soft" | "hard" | "stop") => (
    <input type="number" min={1} max={100} style={{ width: 70 }} value={cfg ? Math.round(cfg[k] * 100) : 0}
      onChange={(e) => cfg && setCfg({ ...cfg, [k]: Number(e.target.value) / 100 })} />
  );
  return (
    <div className="page narrow">
      <div className="page-head"><div><h2>设置</h2></div></div>
      <div className="form">
        <div className="setting">
          <b>并发上限</b>
          <div className="hint">同时运行的 CLI 进程数。员工数量不设上限，但所有员工共享同一份订阅额度，超出的任务会排队。</div>
          <input type="number" min={1} max={50} style={{ width: 90 }} value={sched?.user_limit ?? 3}
            onChange={(e) => onConcurrency(Math.max(1, Math.min(50, Math.round(Number(e.target.value)) || 1)))} />
        </div>

        <div className="setting">
          <b>额度感知调度</b>
          <div className="hint">按最紧张的额度窗口（5 小时 / 7 天）的使用率自动调整：达到限速线并发减半，达到严格限速线降为 1，达到暂停线暂停新任务，窗口重置后自动恢复。额度读数来自最近一次调用；不知道何时重置时，30 分钟后会先放行 1 个任务确认当前额度。</div>
          {sched && (
            <div className={`note ${sched.level === "normal" ? "ok" : sched.level === "paused" ? "bad" : "warn"}`}>
              当前：{sched.level === "normal" ? "正常" : sched.level === "throttled" ? "限速" : "已暂停"} · 有效并发 {sched.effective_limit}/{sched.user_limit} · {sched.running} 个运行中 · {sched.waiting} 个排队
              <div className="hint">{sched.reason}{sched.resume_at ? `（预计 ${hhmm(sched.resume_at)} 恢复）` : ""}</div>
            </div>
          )}
          {cfg && (
            <>
              <label className="tog"><input type="checkbox" checked={cfg.enabled} onChange={(e) => setCfg({ ...cfg, enabled: e.target.checked })} />启用额度感知调度</label>
              <div className="row" style={{ alignItems: "center", gap: 16, flexWrap: "wrap" }}>
                <label className="tog">限速线 {pctIn("soft")} %</label>
                <label className="tog">严格限速线 {pctIn("hard")} %</label>
                <label className="tog">暂停线 {pctIn("stop")} %</label>
                <button className="btn primary" disabled={!dirty} onClick={() => void onAction(() => api.setSchedulerConfig(cfg)).then(() => undefined)}>保存</button>
                {dirty && <button className="btn" onClick={() => setCfg(sched?.config ?? null)}>还原</button>}
              </div>
            </>
          )}
          <div className="row" style={{ gap: 10, flexWrap: "wrap" }}>
            <button className="btn" onClick={() => void onAction(() => api.pauseScheduler(!sched?.manual_pause))}>{sched?.manual_pause ? "取消手动暂停" : "手动暂停新任务"}</button>
            {sched?.override_until
              ? <button className="btn" onClick={() => void onAction(() => api.overrideScheduler(0))}>取消“忽略额度限制”</button>
              : <button className="btn" onClick={() => void onAction(() => api.overrideScheduler(30))}>忽略额度限制 30 分钟</button>}
          </div>
          <div className="hint">“忽略额度限制”只是临时的，不会修改阈值；手动暂停优先于它。交互式的操作（会议发言、导入评估）在暂停时会直接提示原因，不会排队。</div>
        </div>

        <div className="setting">
          <b>执行引擎</b>
          <div className="hint">Claude Code：<code>{version}</code>（使用你本机已登录的账号）</div>
          <div className="hint">GitHub Copilot CLI 适配尚未开始。</div>
        </div>
        <div className="setting">
          <b>数据目录</b>
          <code className="hint">{dir}</code>
        </div>
      </div>
      <Placeholder title="账号管理">多账号 Profile 暂未实现；目前所有员工共用本机 Claude Code 的登录账号。</Placeholder>
    </div>
  );
}
