import { useEffect, useState } from "react";
import { api } from "./api";
import { Placeholder } from "./common";

export function SettingsView({ concurrency, onConcurrency }: { concurrency: number; onConcurrency: (n: number) => void }) {
  const [version, setVersion] = useState("检测中…");
  const [dir, setDir] = useState("");
  useEffect(() => {
    api.claudeVersion().then(setVersion).catch((e) => setVersion(`未检测到 claude：${e}`));
    api.dataDir().then(setDir).catch(() => undefined);
  }, []);
  return (
    <div className="page narrow">
      <div className="page-head"><div><h2>设置</h2></div></div>
      <div className="form">
        <div className="setting">
          <b>并发上限</b>
          <div className="hint">同时运行的 CLI 进程数。员工数量不设上限，但所有员工共享同一份订阅额度，超出的任务会排队。</div>
          <input type="number" min={1} max={50} style={{ width: 90 }} value={concurrency}
            onChange={(e) => onConcurrency(Math.max(1, Math.min(50, Math.round(Number(e.target.value)) || 1)))} />
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


