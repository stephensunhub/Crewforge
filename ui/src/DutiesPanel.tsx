import { useCallback, useEffect, useMemo, useState } from "react";
import { api } from "./api";
import type { Duties, EventRow } from "./types";
import type { ProjectTab } from "./ProjectView";

const KIND_LABEL: Record<string, string> = { plan: "确认计划", decision: "拍板", proposal: "确认议题", merge: "确认合并", review: "确认评审意见", action: "你的事项", memory: "验证经验" };

const DECIDE: [string, string][] = [
  ["需求和业务方向", "做什么、不做什么。AI 没有拍板权限，系统在代码里强制拦着。"],
  ["涉及资金、生产、不可逆操作、安全的事", "同样只有你能拍板。"],
  ["确认 AI 起草的计划", "你批准之后，员工才会开始按计划做。"],
  ["里程碑验收", "你只在里程碑看结果，不用逐个任务验收。"],
];
const GUARD: [string, string][] = [
  ["合并到主分支", "评审通过后，由项目负责人最终确认并合并；负责人是你，所以由你来点。"],
  ["关闭评审行动项", "AI 只能处理，不能自己关；员工改完也要你确认。"],
  ["验证 AI 总结的经验", "没验证的经验不会给员工用。"],
  ["越权操作的审批", "工作区内读写自动放行，越界的才来问你。"],
];
const AI_DOES = "写代码、跑测试、互相评审、起草计划与纪要、提议题和经验、处理评审意见。这些不需要你盯着；AI 能提议，但没有拍板的工具。";

export function DutiesPanel({ projectId, projectName, events, onGoTab }: { projectId: string; projectName: string; events: EventRow[]; onGoTab: (t: ProjectTab) => void }) {
  const [d, setD] = useState<Duties | null>(null);
  const load = useCallback(async () => { try { setD(await api.myDuties(projectId)); } catch { /* 忽略 */ } }, [projectId]);
  const last = useMemo(() => events.reduce((m, e) => (/^(plan_|issue_|action_|memory_|task_|meeting_|agent_proposed)/.test(e.kind) ? Math.max(m, e.id) : m), 0), [events]);
  useEffect(() => { void load(); }, [load, last]);

  return (
    <div className="page wide">
      <div className="page-head">
        <div><h2>我的职责</h2>
          <div className="hint">你在「{projectName}」里的身份是<b>项目负责人（人类 CTO / 架构师）</b>。AI 员工干活，你负责决定方向、把关质量、最终确认。</div></div>
      </div>

      <section className="panel-box">
        <h4 className="group-title">现在在等你 {d && <span className="u-count">{d.items.length}</span>}</h4>
        {!d ? <div className="hint">加载中…</div> : d.items.length === 0 ? (
          <div className="hint" style={{ padding: "6px 0" }}>现在没有需要你处理的事，员工可以自己推进。有事会出现在这里，也会出现在左上角的“待你处理”里。</div>
        ) : (
          <div style={{ display: "grid", gap: 8 }}>
            {d.items.map((it, i) => (
              <div className="duty-item" key={i}>
                <div style={{ minWidth: 0 }}>
                  <span className="badge" style={{ marginRight: 8 }}>{KIND_LABEL[it.kind] ?? it.kind}</span><b>{it.title}</b>
                  <div className="hint" style={{ marginTop: 3 }}>为什么是你：{it.reason}</div>
                </div>
                <button className="btn primary" onClick={() => onGoTab(it.goto as ProjectTab)}>去处理</button>
              </div>
            ))}
          </div>
        )}
        {d && d.later_actions > 0 && (
          <div className="hint" style={{ marginTop: 10 }}>另有 {d.later_actions} 条“之后再说”的评审行动项，不拦合并，也不需要你决定，留在<button className="link" onClick={() => onGoTab("meetings")}>会议页</button>，员工或负责人择机处理，不会占你的待办。</div>
        )}
        {d && d.plan_total > 0 && <div className="hint" style={{ marginTop: 6 }}>计划进度：{d.plan_done}/{d.plan_total}（{d.plan_percent}%）。<button className="link" onClick={() => onGoTab("plan")}>查看计划</button></div>}
      </section>

      <div className="two-col">
        <section className="panel-box">
          <h4 className="group-title">只有你能决定</h4>
          <table className="cost-table"><tbody>{DECIDE.map(([k, v]) => <tr key={k}><td style={{ width: "42%" }}><b>{k}</b></td><td className="hint">{v}</td></tr>)}</tbody></table>
        </section>
        <section className="panel-box">
          <h4 className="group-title">需要你把关</h4>
          <table className="cost-table"><tbody>{GUARD.map(([k, v]) => <tr key={k}><td style={{ width: "42%" }}><b>{k}</b></td><td className="hint">{v}</td></tr>)}</tbody></table>
        </section>
      </div>

      <section className="panel-box">
        <h4 className="group-title">不需要你管</h4>
        <div className="hint">{AI_DOES}</div>
        <div className="hint" style={{ marginTop: 8 }}>这些职责来自需求文档：人类 CTO 负责需求确认、架构决策、重大技术争议裁决；合并由项目负责人确认；拍板权限按议题类型分（需求类和高风险议题只能人类）。想调整你想承担的范围，告诉我，先改定义，再改界面。</div>
      </section>
    </div>
  );
}
