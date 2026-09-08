# Design: add-shell-entry

## 总体思路

shell 是**纯客户端**：新增 `src/shell.rs` 模块，复用 `client::Client`（`/v1` API、Bearer
鉴权、退出码契约），守护进程零改动。入口分发在 `src/main.rs` 增加两个 clap 子命令
`Shell`（含 `-c <CMD>`）与 `System`（v1 仅 `webui [url]`）。

```
                 +---------------------+
 xkeeper shell   |      shell.rs       |  REPL / -c 单命令
 (REPL 或 -c) -->|  内置命令解析/执行   |----------+
                 +---------------------+          |
                         |  (system webui 走浏览器)|
                         v                        v
                 +----------------+      +------------------+
                 | client::Client |      | /api/health 探测 |
                 |  (/v1 API)     |      | (webui 可达性)   |
                 +-------+--------+      +------------------+
                         v
                 +----------------+
                 | xkeeper run    |  守护进程（零改动）
                 | /v1/* + /api/* |
                 +----------------+
```

## 关键决策

### 1. REPL 行编辑库：`rustyline`

- 引入 `rustyline = "15"`（纯 Rust，跨平台，无 c-bindings）。仓库当前无任何行编辑库；
  用裸 stdin 也能做，但历史与光标编辑要手写终端转义，跨平台成本高，不值得。
- `Ctrl+C`：rustyline 默认把 Ctrl+C 返回 `Interrupted`，与"中断当前行不退出"语义天然匹配；
  EOF（`Eof`）即退出。
- 不引入异步：shell 是阻塞式同步代码，与 `client.rs` 的 `ureq` 同风格，保持单线程简单。

### 2. 内置命令表：结构化枚举而非字符串 if-chain

定义 `enum ShellCmd`（Status/Start/Stop/Restart/Pid/Log/Reload/Shutdown/Open/Help/Exit），
用小型 hand-rolled 解析器把一行拆词后映射（`log` 有参数、`help` 有别名，用一个 ~100 行的
解析函数比引 clap-in-REPL 更轻）。执行统一返回 `Result<ShellOutcome>`，
`ShellOutcome::{Continue, Exit}` 控制 REPL 循环。

### 3. status 表格：不引表格式库

程序状态字段来自 `/v1/status`（`ProgramInfo` 投影：name/app/state/pid/restarts/unhealthy）。
手写对齐输出（两遍求最大列宽），无需 `comfy-table` 之类的依赖。

### 4. `open` 与 webui 探测

- webui 可达性：`client.rs` 增加 `webui_health(base) -> Result<()>`（`GET /api/health`，
  短超时）。webui 地址默认 `127.0.0.1:9877`，来自 core 配置的 webui listen 段（若配置
  中存在；否则用默认值）。注意 webui 的鉴权与 `/v1` 独立，health 端点公开，无需 token。
- 打开浏览器：`open::that` crate？——不引入。浏览器拉起就一个系统调用级别的事：
  linux `xdg-open <url>`、macOS `open <url>`、Windows `cmd /C start <url>`，用
  `std::process::Command` 分平台实现 ~15 行，避免新增依赖面。
  - 失败（命令不存在等）输出警告但不影响 shell 继续。

### 5. `system webui` 的拉起策略

- 先探测守护（`/v1/health`）：
  - 守护在跑：仅探测 webui；可达则直接打开浏览器，不可达则提示
    "`webui 未启动 —— 请用 xkeeper webui 启动守护的控制台`"，退出码 1（不做热拉起，
    避免动正在运行的守护的端口/生命周期语义）。
  - 守护不在：以 detached 方式拉起 `xkeeper webui --listen <默认或配置地址>`（当前可执行
    文件自身，`std::env::current_exe()`；unix 用 `process::Command` + `setsid` 风格分离
    （`libc::setsid` 在 fork 后；简化版：直接 spawn detach，unix 下
    `CommandExt::process_group(0)`），stdout/stderr 重定向到日志文件或 `/dev/null`），
    轮询 `/api/health` 至多 ~5 秒，成功后打开浏览器并打印地址后退出（守护留驻后台）。
- 本命令自身不守护化：守护由 spawn 的子进程承担；命令失败（端口占用等）退出码 1。

### 6. `-c` 单命令模式

与 REPL 共用同一 `ShellCmd` 解析/执行路径，仅跳过循环。退出码直接复用
`client::exit_code_of`。

## Non-goals（明确留作后续）

- 运行时修改守护参数（`set`/`config` 类命令）：需先设计哪些参数可热改、投影与持久化
  如何同步 —— 另立变更。
- TUI（全屏 curses 风格界面）、shell 自动补全（rustyline Completer 可后续加）。
- 远程（非回环）连接支持。

## 风险

- rustyline 与非交互管道（`xkeeper shell < script`）的兼容：rustyline 在非 TTY 下自动
  降级为逐行读取，已覆盖；tasks 中安排一条验证。
- Windows 下 `cmd /C start` 的引号转义：用 `raw_arg` 传 URL，避免额外引号被吃掉。
- `system webui` 拉起后轮询超时：5 秒内 health 不通则报错退出，并提示手工前台启动方式。
