# Proposal: add-shell-entry

## Why

xkeeper 目前只有两类客户端入口：单发 CLI 子命令（`status`/`start`/`stop`…）与 Web 控制台
（`xkeeper webui`）。运维时反复敲 `xkeeper status`、`xkeeper restart foo` 效率低，且无一个
终端内的常驻交互界面可以持续观察 app 清单与状态。参照 supervisor 的 `supervisorctl`，
补一个交互式 shell 入口，让终端用户也能在 TUI/行式界面里浏览与操控被守护进程。

## What Changes

- 新增 `xkeeper shell` 子命令：进入一个交互式 REPL，与根进程（`xkeeper run` 守护）通过
  现有本地 HTTP 控制面 `/v1/*` 通信。
  - 内置命令：`status`（表格化 app/程序清单与状态）、`start/stop/restart <name>`、
    `pid <name>`、`log <name> [-f] [--stream out|err] [--tail N]`、`reload`、`shutdown`、
    `open`（在浏览器打开 webui，daemon 未启动 webui 时给出明确提示）、`help`、`exit/quit`。
  - 支持命令行直接执行单条 shell 命令后退出：`xkeeper shell -c "status"`（脚本友好）。
  - 基础行编辑与历史（复用 readline 风格库），输入 `?` 或 `help` 显示可用命令。
- 新增 `xkeeper system` 子命令组，v1 仅含 `xkeeper system webui [url]`：
  检测守护是否在运行，若未运行则以 `xkeeper webui` 模式拉起根进程，然后用系统默认浏览器
  打开控制台页面。其他可修改参数（如运行时改配置项）明确留作后续设计，不在本变更内。
- 守护侧行为零变化：不新增 API 端点，不改动 `/v1`、`/api`、WS 三处共享投影与命令队列。
- webui 的可达性探测沿用 `GET /api/health`（shell 内经 client 增加一个对 webui 端口的
  health 探测辅助即可，不涉及新端点语义）。

## Capabilities

### New Capabilities

- `shell-client`: 交互式 shell 客户端能力 —— `xkeeper shell` 的 REPL 语义、内置命令集、
  单命令模式（`-c`）、与控制面 API 的通信约定、离线时的行为与退出码。

### Modified Capabilities

- `control-plane`: CLI 控制命令的子命令清单扩展：`shell` 加入客户端子命令集合，
  并受同一退出码约定约束；`system webui` 作为本地辅助命令（不经控制面 API）。

## Impact

- 代码：`src/main.rs`（新增 `Shell`、`System` 子命令分发）、新模块 `src/shell.rs`
  （REPL 与内置命令实现）；`src/client.rs` 增加对 webui 端口的 health 探测辅助函数。
- 依赖：引入一个轻量行编辑库（如 `rustyline`，纯 Rust、跨平台），替代裸 stdin 读取。
- API/守护进程：无变化（shell 全部走现有 `/v1` 端点与退出码契约）。
- 文档：README 增补 shell 用法示例。
