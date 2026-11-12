# 架构说明（给贡献者）

## 总览

```
ui/ (React + TS)  ──invoke──▶  src-tauri (Tauri 2 外壳)  ──▶  crates/core (全部业务逻辑)
                                   │  300ms 推送新事件          │  SQLite（rusqlite, bundled）
                                   ▼                           ├── git（worktree、合并、检查点）
                              后台定时器                        └── 员工 CLI 子进程（claude / copilot）
crates/cli: crewforge 命令行 + `mcp-serve`（员工用的团队协作工具 MCP 服务器）
```

- `crates/core` 不依赖任何 UI 框架，可由命令行或 Tauri 外壳调用。
- 加一个功能通常要同时改四处：core 的函数 → `src-tauri/src/commands.rs` 的命令 → `lib.rs` 里 `generate_handler!` 注册 → `ui/src/api.ts` 与 `types.ts`。

## 事件表是主干

几乎所有动作都 `append_event`（`events` 表，同时写 `events.jsonl`）。`src-tauri/src/lib.rs` 里一个线程每 300ms 读新事件推给界面。工作日志、「正在做什么」、人工介入中心、卡住检测、计划页的等待原因都是**读取时从事件汇总**出来的，不是各自存表。

系统发起的调用（裁决、整理、会议）事件里 `project_id` 为空，只有 `attr_project`（成本归属）。任务开始事件里的 `origin`（空 = 人 / `autopilot` / `system`）决定群聊里显示「你 →」还是「自主推进 →」。

## 一次员工执行

`runner::run_turn`：

1. 按（员工，项目路径）找会话；上下文超限就换新会话（自建端点的员工换前先写交接摘要，换后重新带上项目目标和记忆）。
2. 解析端点并注入环境变量；拿端点并发名额（`endpoint.rs`，带优先级队列）。
3. 启动 CLI 子进程（`provider/claude.rs` / `copilot.rs`，stream-json 双向）。
4. 逐个事件处理；审批走 `policy.rs`（沙箱内自动放行，越界才问人；等人批准期间让出名额）。
5. 看门狗：连续太久无输出就终止。

`orchestrator.rs` 在它之上管任务：每个任务一个 git worktree + 分支，合并回主分支、检查点提交、放弃任务（WIP 提交）。所有 CLI 进程登记在 `proc_registry`，应用退出时统一终止，启动时清理孤儿。

## 主要模块

| 模块 | 职责 |
|---|---|
| `autopilot.rs` | 自主推进批次：选条目、派活、验证、评审门、合并、整理、失败后调整计划 |
| `planning.rs` / `plan.rs` | 计划条目、依赖、负责人起草计划和调整计划 |
| `design.rs` | 架构师与设计阶段 |
| `levels.rs` | 等级、能力上限、失败升级 |
| `review_gate.rs` | 是否评审：强制规则、可跳过规则、负责人判断 |
| `ruling.rs` / `lead_actions.rs` / `lead_triage.rs` | 负责人裁决议题、处理行动项 |
| `meeting.rs` / `standup.rs` | 会议（只读模式）与对齐会 |
| `memory.rs` / `curate.rs` | 三层记忆与整理 |
| `endpoint.rs` / `models.rs` | LLM 端点、模型目录 |
| `handover.rs` / `rename.rs` | 角色交接、员工改名 |
| `mcp.rs` | 员工可用的团队工具（提议题、提记忆、`propose_plan` 等，部分仅负责人） |
| `policy.rs` / `sandbox_net.rs` | 审批策略、沙箱网络白名单 |
| `stuck.rs` / `interventions.rs` | 卡住检测、人工介入中心 |

## 决策权

有 AI 负责人时，行动项、议题裁决（含需求类和高风险）都由它定；人只保留危险命令审批、删除项目、预算、批准目标变更（AI 只能通过 MCP 的 `propose_goal` 提议）。负责人回复的 JSON 格式不对会带着原因重试，最多 3 次，仍失败才升级给人。

## 数据库迁移

各模块导出 `MIGRATION_N` 常量，在 `db.rs` 的 `MIGRATIONS` 数组里按顺序登记。**只追加，不改已有的。** 应用拒绝打开比自己新的数据库。

## 后台定时器

都在 `src-tauri/src/lib.rs`：事件推送、对齐会（10 分钟一次，会调用模型）、卡住检测（每分钟，只读事件）、启动时恢复自主推进。

## 测试

- `cargo test -p crewforge-core`：全部核心测试，用 shell 脚本当假 CLI（输出 stream-json），不花额度；git 相关用真实临时仓库。
- `crates/cli/tests/real_*.rs`：`#[ignore]` 的真实 Claude / Copilot 测试，**会消耗额度**。
- 前端没有测试，用 `cd ui && npx tsc --noEmit` 做类型检查。

更详细的设计动机和每个功能验证了什么、没验证什么，见 [产品设计与开发记录](target.md) 第 20 章。
