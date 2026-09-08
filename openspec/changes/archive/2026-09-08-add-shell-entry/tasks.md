# Tasks: add-shell-entry

## 1. 依赖与脚手架

- [x] 1.1 `Cargo.toml` 增加 `rustyline = "15"`；`cargo build` 确认可解析无冲突
- [x] 1.2 `src/main.rs` 增加 `Shell { cmd: Option<String> }` 与 `System`（子命令 `Webui { url: Option<String> }`）两个 clap 子命令骨架，分发到 `shell::run` / `system_webui` 占位函数；`cargo run -- shell --help` 输出正常

## 2. shell 核心模块 `src/shell.rs`

- [x] 2.1 定义 `ShellCmd` 枚举与行解析器（拆词、`help`/`?`、`exit`/`quit` 别名），单测覆盖：未知命令、空行、别名、`log` 参数解析（`-f`/`--tail N`/`--stream out|err`）
- [x] 2.2 实现各命令执行器，复用 `client::Client`：`status`（手写对齐表格：name/app/state/pid/restarts/unhealthy）、`start|stop|restart`、`pid`、`reload`、`shutdown`；单测用本地 mock HTTP（参考现有 client/server 测试方式）覆盖未知程序报错与表格列对齐
- [x] 2.3 实现 `log` 命令：非 follow 走 `log_tail` 打印；`-f` 走 `log_follow`；Ctrl+C 中断 follow 回到提示符
- [x] 2.4 REPL 主循环：rustyline（提示符 `xkeeper> `）、`Interrupted` 清行继续、`Eof` 退出、历史按会话维护；非 TTY stdin 自动降级逐行读取
- [x] 2.5 `-c` 单命令模式：同一解析/执行路径执行一条后退出，退出码经 `client::exit_code_of`（守护不可达 → 3）；集成测试：守护未启动时 `xkeeper shell -c status` 退出码 3

## 3. `open` 与 webui 探测

- [x] 3.1 `src/client.rs` 增加 `webui_health(base)`（`GET /api/health`，2s 超时）；确定 webui 地址来源（core 配置或默认 127.0.0.1:9877）；单测覆盖可达/不可达
- [x] 3.2 平台浏览器拉起：linux `xdg-open`、macOS `open`、windows `cmd /C start`（URL 用 raw 传参）；失败仅告警；单测验证命令构造（平台分别编译验证）
- [x] 3.3 `open` 命令接线：可达 → 打开浏览器输出地址；不可达 → 提示未启动及启动方式，不开浏览器

## 4. `xkeeper system webui`

- [x] 4.1 实现 `system_webui`：守护在跑时按 webui 可达性分流（打开 or 提示 + 退出码 1）；守护未跑时用 `current_exe()` spawn 分离子进程 `xkeeper webui`（unix `process_group(0)`，输出重定向），轮询 `/api/health` 至多 5s，成功后打开浏览器打印地址
- [x] 4.2 集成验证：本机实测 `xkeeper system webui`（守护离线/在线两种场景），确认浏览器命令被调起（可用 `BROWSER=echo` 类桩或观测子进程 spawn 日志）、失败路径退出码 1

## 5. 收尾

- [x] 5.1 README 增补 `xkeeper shell` / `xkeeper system webui` 用法示例
- [x] 5.2 `cargo build --release` + `cargo test` 全绿；unix 路径实测 REPL 手工冒烟（status/restart/open/exit）
