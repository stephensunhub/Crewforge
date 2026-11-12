# 贡献指南

感谢你愿意参与 Crewforge。下面是开发环境、测试和提交约定。

## 开发环境

- macOS、Rust（edition 2024，1.85 以上）、较新的 Node.js、git。
- 想跑端到端流程需要装好并登录 [Claude Code](https://docs.claude.com/en/docs/claude-code) CLI。

```sh
git clone <仓库地址> crewforge && cd crewforge
npm install
npm run tauri dev        # 开发运行（会启动 vite）
```

## 常用命令

```sh
cargo test -p crewforge-core                       # 全部核心测试（用假 CLI，不花额度）
cargo test -p crewforge-core --lib stuck           # 单个模块的单元测试
cargo test -p crewforge-core --test autopilot items_left   # 单个集成测试
cargo check -p crewforge-app                       # 改了 commands.rs / lib.rs 之后检查 Tauri 端
cd ui && npx tsc --noEmit                          # 前端类型检查（没有 lint 和前端测试）
cargo deny check                                   # 依赖的许可证、漏洞和来源（需 cargo install cargo-deny）
python3 scripts/gen-third-party.py                 # 依赖有变化后重新生成 THIRD_PARTY_LICENSES.md
./scripts/install-mac.sh                           # 构建并装到 /Applications
```

小提示：

- `cargo test` 遇到第一个失败的测试二进制就会停；汇总用 `cargo test -p crewforge-core 2>&1 | grep "^test result"`。
- `crates/cli/tests/real_*.rs` 是 `#[ignore]` 的真实 Claude / Copilot 测试，**会消耗额度**，只在需要时运行。
- macOS 的 `sed -i` 必须带后缀参数（`sed -i.bak ...`）。
- 靠时间判断的测试要给足余量，机器忙时会偶发失败。

## 代码约定

- 界面、日志、提示文案和文档用**中文**；新增的用户可见文字也用中文。
- 数据库迁移只追加：新建 `MIGRATION_N` 常量并在 `db.rs` 的 `MIGRATIONS` 里按顺序登记，**不要改已有的**。
- 几乎所有动作都应写事件（`append_event`）；工作日志、等待原因这类视图是从事件汇总的，不要另开表。
- 加一个功能通常要同时改四处：core 函数、`src-tauri/src/commands.rs`、`lib.rs` 的 `generate_handler!`、`ui/src/api.ts` 与 `types.ts`。
- 测试里用 shell 脚本当假 CLI（见 `crates/core/tests/`），不要依赖真实模型。

## 提交与合并请求

- 一个提交只做一件事；提交信息写清「做了什么、为什么」。
- 提交前确认：`cargo test -p crewforge-core` 通过，`cd ui && npx tsc --noEmit` 通过，改了 Tauri 端的话 `cargo check -p crewforge-app` 通过。
- 做完功能后，在 `docs/target.md` 末尾追加一节：设计、**验证了什么、没验证什么**。没验证的要如实写。
- 改了界面请在合并请求里附截图或说明（项目目前没有前端测试）。

## 报告问题

请用 Issue 模板，附上：版本、macOS 版本、复现步骤、相关事件（工作日志或 `events.jsonl` 片段，注意去掉路径、令牌等隐私）。安全问题请看 [SECURITY.md](SECURITY.md)，不要公开提 Issue。
