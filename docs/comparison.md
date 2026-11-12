# 和其他工具的对比

> 信息核对日期：2026-11-13。Claude Code、Codex、Kiro、Copilot Projects 都在快速迭代，下面关于它们的描述来自官方文档和公开报道（部分是第三方博客），**没有逐项实测**，写进对外材料前请以官方页面为准。关于 Crewforge 的描述来自本仓库代码和我们自己的运行记录。

## 先说关系：Crewforge 不是 Claude Code 的替代品

Crewforge 的员工**就是** Claude Code（或 GitHub Copilot CLI）的子进程。所以和「单纯使用 Claude Code」比，问题不是二选一，而是：**在 CLI 之上，再加一层「团队管理」值不值得**。

| 层 | 单用 Claude Code / Codex | 加上 Crewforge |
|---|---|---|
| 执行 | 一个 Agent 在你的终端里干活（也可以开多个会话、子 Agent） | 同样用 CLI 执行，但由系统调度多个员工 |
| 记忆与状态 | 对话、CLAUDE.md / AGENTS.md | 员工、角色、计划、任务、议题、记忆、事件流都落库，可回溯 |
| 谁来排活 | 你自己，或你写的提示词 | 负责人（AI 或你）按依赖关系排计划、派活 |
| 质量关卡 | 你自己看 diff | 验证、评审门、QA、合并全流程 |
| 长时间无人值守 | 靠你盯着或自己写脚本 | 批次预算、自动续批次、重启后自动恢复 |

## 一、对比：只用 Claude Code / Codex

### 它们本身已经很强（别低估）
- **Claude Code：** 有子 Agent（subagent）、实验性的 Agent Teams（多个完整会话协作）、`--worktree` 隔离并行会话、Hooks、MCP。
- **Codex：** 有 CLI 和桌面应用；桌面应用支持用 Git worktree 并行跑多个 Agent，云端任务可以在隔离环境里后台运行。（相关细节来自第三方博客，具体上限和版本请看官方文档。）
- 单个任务、交互式地写代码，它们**更直接、更轻、更快**。

### Crewforge 多出来的能力
1. **持久的团队结构。** 角色（负责人、开发、QA、评审、架构师）、等级（1–5）、能力上限，跨会话、跨批次保留。CLI 的子 Agent 通常是一次任务里临时拉起来的。
2. **带依赖的计划与按能力派活。** 计划条目有前置、验收标准、等级；难的活派给强的模型，简单的活派给便宜的或本地的模型，因能力不足失败会自动升级。
3. **完整的交付流水线。** 每个任务一个 worktree + 分支 → 验证 → 评审门（大改动强制评审，文案小改动直接跳过，其余由负责人判断）→ squash 合并 → 检查点。放弃的任务保留 WIP 提交。
4. **先设计后实现。** 有架构师时，先产出设计文档（只改 `docs/`），再据此起草实现计划，并把设计要点写进项目记忆。
5. **长期无人值守。** 批次有调用数 / 时间预算，用完自动暂停，可自动续批次；应用重启后被打断的批次会按剩余预算接着做。
6. **决策权分层。** 有 AI 负责人时，议题裁决、行动项都由它定；你只保留危险命令、删除项目、预算和批准目标变更。
7. **混合模型与本地模型。** 同一个项目里可以同时有 Claude 订阅员工、Copilot 员工和连局域网 Ollama 的员工，不绑单一厂商。
8. **可观察。** 几乎所有动作都是事件；工作日志、群聊、计划页的等待原因、人工介入中心、卡住检测、用量统计都是从事件汇总出来的。
9. **团队级的上下文管理。** 三层记忆（公司 / 项目 / 个人）、会话超限时自动写交接摘要、角色交接。

### Crewforge 的代价和不足（如实说）
- **额外的开销。** 负责人调度、评审、会议、整理记忆都要调用模型。我们在本地模型环境下的一次实测里，一个批次约 38% 的模型时间花在会议上（之后做了若干优化，但这类开销依然存在）。小任务用 Crewforge 反而更慢、更贵。
- **上手成本。** 要先装好 Claude Code（或 Copilot CLI），创建员工、项目，写清目标。
- **只支持 macOS（Apple 芯片的安装包）。** 当前员工引擎只有 Claude Code 和 GitHub Copilot CLI，**暂不支持把 Codex 当员工**。
- **早期版本。** 界面很多地方没有在所有场景下验证，没有前端测试。
- **效果取决于模型。** 本地模型当员工慢、容易卡；负责人需要稳定输出 JSON 的判断，建议用更强的模型。

### 怎么选
| 场景 | 建议 |
|---|---|
| 改一个 bug、写一个小功能、探索代码 | 直接用 Claude Code / Codex |
| 同时跑两三个互不相干的任务 | CLI 的 worktree / 并行会话就够了 |
| 一个有明确目标的中等以上项目，要拆成很多有依赖的任务，希望长时间少打扰 | Crewforge |
| 想混用多种模型，尤其想让本地模型分担简单活 | Crewforge |
| 需要留下完整的决策、评审、进度记录，方便事后追溯 | Crewforge |

## 二、对比：Kiro 与 Copilot Projects

| 维度 | Crewforge | Kiro（AWS） | Copilot Projects（GitHub） |
|---|---|---|---|
| 形态 | macOS 桌面应用 + 命令行 | 基于 Code OSS 的 IDE（另有 CLI、网页版） | Copilot 应用中的功能，公开预览 |
| 核心思路 | 长期运转的 AI 团队：角色、计划、批次、评审、合并 | 规格驱动：需求 → 设计 → 任务 → 代码 | 给一个目标，由 Copilot 协调多个 Agent，返回 PR |
| 模型 | 自选：Claude 订阅、Copilot、自建 Ollama 等 | 报道称以 Claude 为主 | 文档未写明 |
| 数据位置 | 全部在本机 | 本机 IDE，云端按产品而异 | 文档未写明运行位置 |
| 成本 | 软件免费开源，成本是你自己的模型订阅和硬件 | 有免费档和付费档，各来源价格不一致 | 预览中，文档未给出计费 |
| 擅长 | 少打扰地推进整个项目；混合模型；可追溯 | 需要规范设计文档的个人 / 团队；IDE 一体化体验 | 已在 GitHub 的跨仓库工作；定时自动化；接 Slack / Figma |
| 短板 | 仅 macOS、早期、有调度开销、无 IDE | 定价曾被批评偏贵；偏向「一个人 + Agent」 | 预览期，协调机制和运行细节不透明 |

## 参考来源

- Kiro：[AWS Kiro 介绍报道](https://yourstory.com/ai-story/amazon-kiro-ai-ide-launch-developers)、[Kiro 评测](https://dev.to/pickuma/amazon-kiro-review-awss-spec-driven-agentic-ide-in-2026-4f48)、[定价争议报道](https://www.theregister.com/2025/08/18/aws_updated_kiro_pricing/)
- Copilot Projects：[官方文档：Working with projects](https://docs.github.com/en/copilot/how-tos/github-copilot-app/working-with-projects)
- Claude Code：[Agent Teams 文档](https://code.claude.com/docs/en/agent-teams)
- Codex：[Codex CLI 并行 worktree 开发（第三方）](https://codex.danielvaughan.com/2026/03/26/codex-cli-worktree-parallel-development/)、[Codex 应用介绍（第三方）](https://theneuron.ai/explainer-articles/openai-codex-app-deep-dive-how-it-works)
