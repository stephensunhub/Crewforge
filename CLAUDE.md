# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Crewforge（"AI Development Studio"）是 macOS 本地桌面应用：用户创建项目和 AI 员工，员工是 Claude Code / GitHub Copilot 的 CLI 子进程，长期协作开发。Rust 核心 + Tauri 2 + React/TS。界面、日志、提示文案和文档都是中文，新增的用户可见文字也用中文。

产品需求在 `docs/target.md`（第 20 章是按时间顺序的实现进展，§20.x 记录每个功能的设计、验证了什么、**没验证什么**），流程在 `docs/flows.md`。做完功能后在 `docs/target.md` 末尾追加一节。

## 命令

```sh
cargo test -p crewforge-core                         # 全部核心测试（单元 + 集成，用假 CLI，不花额度）
cargo test -p crewforge-core --lib stuck             # 单个模块的单元测试
cargo test -p crewforge-core --test autopilot items_left   # 单个集成测试文件里的某个测试
cargo check -p crewforge-app                         # 检查 Tauri 端（改了 commands.rs / lib.rs 之后）
cd ui && npx tsc --noEmit                            # 前端类型检查（没有 lint 和前端测试）
npm run tauri dev                                    # 开发运行（beforeDevCommand 会启动 vite）
./scripts/install-mac.sh                             # 构建 → 退出旧应用 → 装到 /Applications → 重新签名 → 打开
```

- `cargo test` 遇到第一个失败的测试二进制就会停下，后面的不会跑；汇总用 `cargo test -p crewforge-core 2>&1 | grep "^test result"`。
- `crates/cli/tests/real_*.rs` 是 `#[ignore]` 的真实 Claude/Copilot 测试，**会消耗额度**，只在需要时跑：`cargo test -p crewforge-cli --test real_ruling -- --ignored --nocapture`。
- macOS 的 `sed -i` 必须带后缀参数（`sed -i.bak ...`），否则表达式会被当成后缀。
- 装到本机的应用拒绝打开比它自己新的数据库。改了数据库结构或界面后，要用 `install-mac.sh` 重装才能在应用里看到。脚本在构建完、退出旧应用之前会等没有员工进程（`claude -p`）才继续（最多 1 小时，`CREWFORGE_FORCE=1` 跳过），因为重启会打断员工；重装必须走脚本：Tauri 打出的包只有链接器的临时签名，Info.plist 没绑定，macOS 的"本地网络"授权（访问局域网里的 Ollama 需要）不会生效。

## 架构

三个 crate + 前端：`crates/core`（全部业务逻辑，SQLite 用 rusqlite bundled）、`crates/cli`（`crewforge` 命令行，同时提供员工用的 `mcp-serve` 子命令）、`src-tauri`（Tauri 命令薄封装 + 后台定时器）、`ui/`（React，`api.ts` 是 `invoke` 封装，`types.ts` 是后端结构的镜像）。加一个功能通常要同时改四处：core 的函数、`src-tauri/src/commands.rs` 的命令、`lib.rs` 里的 `generate_handler!` 注册、`ui/src/api.ts` 与 `types.ts`。

**事件表是主干。** 几乎所有动作都 `append_event`（`events` 表，同时写 `events.jsonl`）。`src-tauri/src/lib.rs` 里一个线程每 300ms 读新事件推给界面。工作日志、"正在做什么"、人工介入中心、卡住检测、计划页的等待原因都是从事件**读取时汇总**出来的，不是各自存表。系统发起的调用（裁决、整理、会议）事件里 `project_id` 为空、只有 `attr_project`（成本归属），任务开始事件里的 `origin`（空=人 / `autopilot` / `system`）决定群聊里显示"你 →"还是"自主推进 →"。

**一次员工执行** = `runner::run_turn`：按（员工, 项目路径）找会话 → 上下文超限就换新会话（`rotation_limit`；自建端点的员工换前先写交接摘要，换后重新带上项目目标和记忆）→ 解析端点并注入环境变量 → 拿端点并发名额 → 启动 CLI 子进程（`provider/claude.rs` / `copilot.rs`，stream-json 双向）→ 逐个事件处理，审批走 `policy.rs`（沙箱内自动放行，越界才问人）→ 看门狗：连续太久无输出就终止。`orchestrator.rs` 在它之上管任务：每个任务一个 git worktree + 分支，合并回主分支、检查点提交。所有 CLI 进程登记在 `proc_registry`，应用退出时统一终止，启动时清理孤儿。

**LLM 端点**（`endpoint.rs`）：员工可选连自建的 Anthropic 兼容服务（如另一台电脑上的 Ollama），只对该员工的进程注入 `ANTHROPIC_*`（三个档位别名都指向端点上的模型），不碰本机登录；启动参数里有 `--setting-sources ""`，用户级 settings.json 影响不到。令牌存钥匙串。费用记 0。沙箱网络白名单在 `sandbox_net.rs`（内置 + 用户全局设置）。

**决策权跟着项目负责人走。** 有 AI 负责人时，行动项、议题裁决（含需求类和高风险）都由它定（`ruling.rs`、`lead_actions.rs`、`lead_triage.rs`）；人只保留：危险命令审批、删除项目、预算、**批准目标变更**（AI 只能通过 MCP 的 `propose_goal` 提议）。没有 AI 负责人才把待决定的事交给人（`for_human`）。负责人回复的 JSON 格式不对时带着原因重试，最多 `MAX_FORMAT_ATTEMPTS`=3 次，仍失败才升级给人。员工通过 `mcp.rs` 里的团队工具（提议题、提记忆、`propose_plan` 等，部分只有负责人能调）和系统交互。

**自主推进**（`autopilot.rs`）：负责人按计划依赖派活。批次开始先把没人在做的"进行中"条目放回待开始并整理积压的行动项；循环里先裁决议题，再派"就绪"条目——前置完成、不和进行中的条目改同一批文件、指派的人空闲、并发和预算允许；评审、合并、整理记忆、更新文档、失败后负责人调整计划。应用重启时被打断的批次按剩余预算自动接着做（不替人批准新计划）。预算只统计“限制”的调用：员工连的 LLM 端点上有“限制批次预算”开关（默认限制，关掉只统计不限制），订阅员工一定计入；项目级设置（`autopilot_config`）保存每批次预算、是否自动续批次和连续上限，时间预算 0 表示不限。员工"工作中"的标记在 `busy` 集合里，负责人的后台工作也要用 `busy_scope` 标记，否则界面显示空闲。

**后台定时器**都在 `src-tauri/src/lib.rs`：事件推送、对齐会（`align_tick`，10 分钟一次，会调用模型）、卡住检测（`stuck_tick`，每分钟，只读事件，发现新的弹系统通知）、启动时恢复自主推进。人工介入中心（`interventions.rs`）本身不定时，只在打开页面时汇总事件。

**对齐会**在代码里的会议类型键仍是 `standup`（兼容旧数据），界面叫"对齐会"。

## 约定

- 数据库迁移：各模块导出 `MIGRATION_N` 常量，在 `db.rs` 的 `MIGRATIONS` 数组里按顺序登记（现在到 27）。只追加，不改已有的。
- 测试用 shell 脚本当假 CLI（输出 stream-json），见 `crates/core/tests/` 里的 `fake_cli` 等；审批用 `Deny` 之类的假 `Approver`。检查点、合并等用真实 git 仓库（`tempfile`）。靠时间判断的测试要给足余量，机器忙时会偶发失败。
- 默认数据目录 `~/Library/Application Support/Crewforge`，环境变量 `CREWFORGE_HOME` 可改。调试时先复制 `crewforge.db`（连同 `-wal`、`-shm`）再用 `sqlite3 -readonly` 查。
- 提交信息用中文，结尾加 `Co-Authored-By` 署名行；一个提交一件事。
