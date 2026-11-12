# Crewforge

**在你的 Mac 上，用一队 AI 员工长期协作开发软件。**

Crewforge 是一个 macOS 本地桌面应用。你创建项目和 AI 员工，员工是 [Claude Code](https://docs.claude.com/en/docs/claude-code) 或 GitHub Copilot 的命令行子进程；他们各自在独立的 git 工作区里写代码、跑测试、互相评审，由一位负责人（可以是 AI）排计划、派活、合并。你只在关键处介入：批准目标变更、危险命令和预算。

> 状态：早期版本（0.1），仅在 macOS 上使用和验证。界面、日志和文档以中文为主。

## 它能做什么

- **一个团队，多种角色：** 负责人、开发、QA、评审、架构师。每个角色有各自的职责和权限。
- **自主推进：** 负责人按计划的依赖关系派活，员工并行开发，完成后评审、合并、整理记忆、更新文档；失败时负责人调整计划。
- **先设计后实现：** 有架构师时，先产出设计文档，再由负责人按设计起草实现计划。
- **按能力派活：** 任务分 1–5 级，员工有能力上限，难的活派给强的模型，简单的活派给便宜的模型。
- **本地模型也能当员工：** 员工可以连局域网里的 Ollama 等 Anthropic 兼容服务，不占订阅额度。
- **一切可观察：** 工作日志、群聊、会议、议题板、计划页、用量统计都由同一条事件流汇总出来。
- **本地优先：** 数据在本机 SQLite 和 git 仓库里，没有云端服务和账号体系。

## 快速开始

前置条件：macOS、Rust（支持 edition 2024，即 1.85 以上）、较新的 Node.js、已安装并登录的 [Claude Code](https://docs.claude.com/en/docs/claude-code) CLI。

```sh
git clone <本仓库地址> crewforge && cd crewforge
npm install
npm run tauri dev          # 开发运行
# 或者构建并安装到 /Applications：
./scripts/install-mac.sh
```

第一次启动后：创建员工 → 新建项目并选一个 git 仓库 → 在「计划」页写目标 → 开启自主推进。完整步骤见 [快速开始](docs/getting-started.md)。

## 文档

| 文档 | 内容 |
|---|---|
| [快速开始](docs/getting-started.md) | 安装、首次配置、跑通第一个项目 |
| [核心概念](docs/concepts.md) | 员工、角色、项目、任务、计划、自主推进、等级、记忆、端点 |
| [使用指南](docs/user-guide.md) | 每个页面怎么用，常见操作 |
| [连接本地模型（Ollama）](docs/endpoints.md) | 自建端点的配置、并发和排错 |
| [常见问题与排错](docs/troubleshooting.md) | 员工卡住、没权限、数据库版本等 |
| [架构说明](docs/architecture.md) | 给贡献者：代码结构、事件主干、一次执行的流程 |
| [贡献指南](CONTRIBUTING.md) | 开发环境、测试、提交约定 |
| [产品设计与开发记录](docs/target.md) | 需求和按时间顺序的实现记录（每节写明验证了什么、没验证什么） |
| [开发场景流程图](docs/flows.md) | 各类场景的流程 |

## 项目结构

```
crates/core    全部业务逻辑（SQLite、git、调度、自主推进），不依赖 UI
crates/cli     crewforge 命令行；同时提供员工用的 mcp-serve 子命令
src-tauri      Tauri 2 外壳：命令封装 + 后台定时器
ui             React + TypeScript 界面
scripts        安装脚本
docs           文档
```

## 安全说明

员工是会运行命令、改文件的 AI 进程。Crewforge 的默认做法是：在系统沙箱里执行，工作区之外不可写，网络只通白名单域名，危险命令（提权、推送、远程登录等）必须由你批准。详见 [SECURITY.md](SECURITY.md)。

## 许可证

[MIT](LICENSE)。
