# shell-client Specification

## Purpose

定义 `xkeeper shell` 交互式客户端能力：作为根进程（`xkeeper run` 守护）的终端入口，
在 REPL 中浏览被守护 app/程序的清单与状态、执行控制动作，并通过 `xkeeper system webui`
辅助用户打开 Web 控制台。shell 不改变守护进程行为，全部经现有控制面 API 通信。

## ADDED Requirements

### Requirement: shell 入口与 REPL

`xkeeper shell` SHALL 启动一个交互式行式 REPL：显示提示符，读取一行作为一条 shell 命令，
执行后继续读取，直至用户输入 `exit`、`quit` 或发送 EOF（Ctrl+D）。REPL SHALL 提供基础
行编辑（光标移动、退格、历史上下翻）并按会话维护命令历史。Ctrl+C SHALL 仅中断当前输入行，
不退出 shell。

#### Scenario: 进入并退出 shell

- **WHEN** 执行 `xkeeper shell`，输入 `status` 后再输入 `exit`
- **THEN** shell 执行一次状态查询后退出，进程退出码为 0

#### Scenario: Ctrl+C 不退出

- **WHEN** 在提示符处按下 Ctrl+C
- **THEN** 当前输入行被放弃并显示新提示符，shell 进程不退出

#### Scenario: EOF 退出

- **WHEN** 在提示符处按下 Ctrl+D
- **THEN** shell 退出，退出码为 0

### Requirement: shell 内置命令集

shell SHALL 支持以下内置命令，语义与对应单发 CLI 子命令一致，全部经控制面 `/v1` API 通信：

- `status`：以对齐的表格输出全部程序的名称、所属 app、状态、pid、重启次数与 unhealthy 标记；
- `start|stop|restart <name>`：执行对应动作，未知程序报错；
- `pid <name>`：输出程序 pid 或 `not running`；
- `log <name> [-f] [--tail N] [--stream out|err]`：查看日志，`-f` 跟随输出；
- `reload`：热更新配置；
- `shutdown`：请求守护进程优雅退出，随后 shell SHOULD 因守护不可达提示并可用 `exit` 离开；
- `open`：探测 webui 可达性（`GET /api/health`），可达则调用系统默认浏览器打开控制台，
  不可达时输出明确提示且不启动浏览器；
- `help`（或 `?`）：列出内置命令与简短用法；
- `exit` / `quit`：离开 shell。

空行 SHALL 忽略；未知命令 SHALL 输出 `unknown command` 提示并列出相近命令（若有），不退出。

#### Scenario: status 表格输出

- **WHEN** shell 中输入 `status` 且守护进程有两个程序（一 running 一 stopped）
- **THEN** 输出包含两个程序行的对齐表格，含名称、状态、pid 与重启次数列

#### Scenario: 未知命令容错

- **WHEN** shell 中输入 `sttaus`
- **THEN** 输出 unknown command 提示并建议相近命令，shell 继续接受下一条输入

#### Scenario: 在 shell 中重启程序

- **WHEN** shell 中输入 `restart web`
- **THEN** 请求经控制面 API 下发，程序状态迁移与 `xkeeper restart web` 一致

#### Scenario: open 打开 webui

- **WHEN** webui 在默认端口可达且输入 `open`
- **THEN** 系统默认浏览器被调起并指向控制台地址，shell 不退出

#### Scenario: open 在 webui 未启动时

- **WHEN** webui 未启动且输入 `open`
- **THEN** 输出"webui 未启动"的提示（含启动方式提示），不启动浏览器，shell 不退出

### Requirement: shell 单命令模式

`xkeeper shell -c "<命令>"` SHALL 执行该单条 shell 命令后立即退出，退出码遵循控制面
CLI 的同一约定（0 成功；1 一般错误；3 守护进程不可达）。该模式 SHALL 不进入交互界面。

#### Scenario: 脚本化调用

- **WHEN** 执行 `xkeeper shell -c "status"` 且守护进程运行中
- **THEN** 输出状态表格后立即退出，退出码 0

#### Scenario: 单命令模式守护不可达

- **WHEN** 守护进程未启动时执行 `xkeeper shell -c "status"`
- **THEN** 输出守护进程不可达提示，退出码为 3

### Requirement: system webui 辅助命令

`xkeeper system webui [url]` SHALL：探测守护进程与 webui 的可达性；若守护进程未运行，
以 `xkeeper webui` 模式在后台拉起根进程；随后调用系统默认浏览器打开控制台地址。
`url` 缺省时使用 webui 默认地址。该命令为本地辅助命令，MUST NOT 依赖控制面新增端点。

#### Scenario: 守护未运行时一键打开

- **WHEN** 守护进程未运行且执行 `xkeeper system webui`
- **THEN** 根进程以 webui 模式被拉起，浏览器打开控制台页面，命令输出访问地址后返回

#### Scenario: 守护已运行时打开

- **WHEN** 守护进程运行中（无论 webui 是否启动）且执行 `xkeeper system webui`
- **THEN** webui 可达时直接打开浏览器；webui 不可达时输出提示（如需以 webui 模式启动请
  使用 `xkeeper webui`），命令不强行改动运行中的守护
