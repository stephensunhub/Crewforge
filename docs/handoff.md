# 交接文档

快照时间：2026-11-10 23:41。给下一个会话（换 Claude 账号后重新开）看的：先读 `CLAUDE.md`（命令与架构），再读这份（现在是什么状态、还差什么），细节在 `docs/target.md` 第 20 章（§20.50 到 §20.65，每节写了验证了什么、没验证什么）。

## 一句话

Crewforge 是 macOS 本地桌面应用，员工是 Claude Code / Copilot 的 CLI 子进程。当前正在用它开发一个叫 **Todo** 的 Go 小项目（`/Users/stephen/Downloads/Todo`），三位员工全部连另一台电脑上的 Ollama（`qwen3.8:27b`），由 AI 负责人带着自主推进。用户的目标：AI 当负责人时，除硬性安全限制和改项目目标外，**一切由 AI 决定，人只观察和纠偏**。

## 环境事实

- Ollama：`http://192.168.8.102:11434`，模型 `qwen3.8:27b`（另有 `qwen3.6:35b-a3b`），实际加载上下文 65536。端点配置里的上下文窗口已填 65536，**限制批次预算已关**。
- 应用装在 `/Applications/Crewforge.app`，数据在 `~/Library/Application Support/Crewforge`（`crewforge.db`、`events.jsonl`、`worktrees/`、`children.pids`）。
- macOS“本地网络”权限：访问局域网 Ollama 需要，授权绑定应用签名，**每次重装都可能失效**。`scripts/install-mac.sh` 会重新做完整签名；装完后到“模型 → LLM 端点 → 测试连接”确认。从 RustRover 里启动的 Claude 会话没有这个权限，**在那里直接连 Ollama 会 EHOSTUNREACH**（`curl` 不受限制所以能通，容易误判）。
- 员工与登录的关系：`alice`、`bob`、`carol`（贪吃蛇等演示项目）是订阅员工，用本机 Claude 登录；`dave` 是 Copilot；`Ollama-1/2/3-QA` 连端点，只对它们的进程注入 `ANTHROPIC_*`，**不受本机 Claude 账号影响**。切换 Claude 账号只会影响前三位。额度读数（`rate_limit` 事件）会在下一次 Claude 调用后变成新账号的。旧账号建立的 Claude 员工会话换账号后能否 `--resume` 没验证过，如果报错，重置这几位的会话（改一下员工的 LLM 再改回会清会话）。

## 版本状态

- 仓库 HEAD：`1752f04`，工作区干净。
- **已装版本**对应 `6fa0b10`（23:13:55 安装）。
- **还没装**：`e66edf3`（应用退出清理开始后新起的员工进程直接终止，修复重装时留下孤儿进程）和 `1752f04`（批次开始时行动项积压 ≥12 条才先整理，不让负责人慢速整理挡住其他人）。装之前必须确认没有员工进程在跑（`pgrep -f 'claude -p.*qwen3.8'`），重装会打断它们；批次剩余预算够时重启后会自动接着做。
- 数据库版本 20。装好的应用拒绝打开更新的数据库，也不能回退。

## 现在在跑的东西（快照）

- Todo 项目的自主推进批次：`23:14:06` 恢复，预算 30 次 / 20 分钟，但**用户已把项目设置改成最长 0 分钟（不限）、预算用完后自动续批次、最多连续 6 个**，所以这个批次到点后会续成不限时间的新批次。已用调用 0，不计入预算的 4 次。
- 计划：完成 18，进行中 3，待开始 11，取消 1。进行中：SQLite 存储实现（Ollama-1）、仓库卫生（Ollama-2）、补 `handlers_test.go`（Ollama-3-QA）。已合并进 Todo 主分支：脚手架与契约、HTTP API 处理器。待开始里还有：Service 业务逻辑、Dashboard 前端、装配 main、端到端测试、使用文档、CI、QA 验收等。
- 没有需要人决定的议题（上一个乐观锁议题用户已拍板：引入 version 字段，冲突返回 409）。
- **Todo 的主工作区必须保持干净**：手动在里面改文件或 `go get` 会让员工的合并被拦（上次就是）。要手动下依赖，用完 `git checkout -- go.mod && rm go.sum` 还原。
- 全局设置里员工会话上下文上限被用户改成了 46K（`context_limit=46000`），这会让 **Claude 订阅员工**比默认的 150K 更频繁换会话；对 Ollama 员工没影响（它们取端点窗口的七成）。可以问用户要不要改回去。

## 这一阶段做了什么（按主题，节号见 `docs/target.md`）

决策权跟负责人走、负责人可提议改目标由人批准、对齐会（原站会）与定时检查（§20.50–20.53）→ LLM 端点（§20.54）→ 重启后自动恢复自主推进（§20.55）→ 沙箱网络白名单与卡住待办（§20.57）→ 合并被拦待办（§20.58）→ 人工介入中心（§20.59）→ 看门狗与后台卡住检测（§20.60）→ 员工进程随应用退出（§20.61）→ 批次预算、端点“限制批次预算”开关、项目级设置、自动续批次（§20.63）→ 被打断的自动评审会（§20.64）→ 会话已在 CLI 里建好但没记下（§20.65）。其余（员工在做什么、工作日志、计划页等待原因、会话交接摘要、行动项持续整理、点击跳转等）补记在 §20.62。

## 下一步（按优先级）

1. 等批次走到安全的点（没有员工进程）装 `e66edf3` 和 `1752f04`，装完测试连接。
2. 盯 Todo 的推进：SQLite 存储重新评审并合并（旧的半截评审会已被新版启动时清理）；之后 Service、Dashboard 并行开工。出问题先看“人工介入”页，再看员工卡片“工作日志”。
3. 用户**尚未亲眼验证**的界面：员工“正在做什么”和点击跳转、工作日志、计划页等待原因、人工介入页（含出错操作按钮）、设置页白名单、审批里的“允许并加入白名单”、端点“限制批次预算”、“批次设置”框、议题方案可点。他们反馈什么，就按反馈改。
4. 已知缺口：没有“停止单个员工”的能力（只能停整个批次）；卡住检测阈值（`stuck_long_minutes`、`stuck_silent_minutes`）、看门狗阈值（`turn_silence_minutes`）只有设置项没有界面；人工介入里没覆盖到的报错类型只给“查看工作日志”和“去项目看”。
5. 国际化（界面语言 + 员工输出语言）用户明确暂缓，做的时候一起做，见 `docs/target.md` §20.56；不要主动给员工加“输出简体中文”的约束，也不要规定思考语言。

## 本地模型的已知特点（调参和排查用）

- 慢：一次执行中位约 9 分钟，导入评估 40 到 50 分钟，每两次工具调用之间可隔 4 到 18 分钟。并发上限设了 3，但一台 Ollama 实际是排队处理。
- 输出语言会漂移（中文、英文、日文），日文常成串出现。工具调用阶段偏英文。
- 偶尔输出不合法的 JSON：负责人回复会带着错误原因重试，最多 3 次，仍失败才升级给人。
- 会绕过限制：曾为下载依赖自己起转发代理、`GOINSECURE`、`curl -k`。卡住检测会识别这类命令。
- 重启后 Claude Code 会话要用 `--resume`；如果应用恰好在会话刚建好时被关，数据库里记成“没建立”，下次会报 `Session ID … is already in use`，现已自动修复（改用续接重试一次）。

## 排查常用命令

```sh
# 复制数据库再只读查询（直接读会因为 WAL 报 locked）
D="$HOME/Library/Application Support/Crewforge"; cp "$D/crewforge.db" /tmp/cf.db; cp "$D/crewforge.db-wal" /tmp/cf.db-wal 2>/dev/null; cp "$D/crewforge.db-shm" /tmp/cf.db-shm 2>/dev/null
sqlite3 -readonly /tmp/cf.db "select datetime(started_at/1000,'unixepoch','localtime'), status, calls, uncounted_calls, max_calls||'/'||max_minutes from autopilot_runs order by started_at desc limit 3;"
sqlite3 -readonly /tmp/cf.db "select datetime(ts/1000,'unixepoch','localtime'), substr(json_extract(payload,'\$.text'),1,150) from events where kind like 'autopilot_%' order by id desc limit 10;"
pgrep -fl 'claude -p.*qwen3.8'      # 员工进程；父进程应是应用，父进程为 1 的是孤儿
curl -sS http://192.168.8.102:11434/api/ps   # Ollama 里加载了什么（从应用外的终端执行才有本地网络权限的问题）
```

## 工作约定（用户的偏好和被拦截过的动作）

- 用中文沟通。回答先给结论；每次改完要说清**验证了什么、没验证什么**。界面没在真实窗口里看过时，要明说。
- 装应用之前先确认没有员工在干活，并且**先问用户**；用户说“安装”才装。每装一次都会打断员工、可能让本地网络授权失效。
- 做完功能在 `docs/target.md` 末尾追加一节，提交信息写中文并带 `Co-Authored-By` 署名。
- 用户不想被反复叫去手动操作：凡是需要人介入的异常，都要在应用里有提示和能直接点的操作（人工介入中心就是为此做的）。
- 这些动作自动模式的权限检查会拦，**不要绕**，拦了就停下来向用户说明并让用户决定：把共享托管域名（如 `storage.googleapis.com`）加进沙箱内置白名单（放宽安全限制，应该让用户自己在设置里加）；终止进程（即使看起来是孤儿，先把证据给用户）；用本机转发之类的办法绕过 macOS 的“本地网络”权限。
- 设计取舍的记忆放在 `~/.claude/projects/-Users-stephen-Documents-workspace-Crewforge/memory/`（目前只有“国际化暂缓”一条）。
