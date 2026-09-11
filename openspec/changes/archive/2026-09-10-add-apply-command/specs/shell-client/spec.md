# shell-client Specification Delta — add-apply-command

## MODIFIED Requirements

### Requirement: shell 内置命令集

shell SHALL 支持以下内置命令，语义与对应单发 CLI 子命令一致，全部经控制面 `/v1` API 通信：

- `status`：以对齐的表格输出全部程序的名称、所属 app、状态、pid、重启次数与 unhealthy 标记；
- `start|stop|restart <name>`：执行对应动作，未知程序报错；
- `pid <name>`：输出程序 pid 或 `not running`；
- `log <name> [-f] [--tail N] [--stream out|err]`：查看日志，`-f` 跟随输出；
- `reload`：重扫并检出待应用变更，输出 pending 预览（不应用）；
- `pending`：输出当前待应用变更清单（逐程序 changed、app 级注册变化、检出错误）；
- `apply [<app> [<program>]] [--restart]`：应用待应用变更，输出与 `xkeeper apply` 一致的逐程序结果表格；
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

#### Scenario: 在 shell 中应用变更

- **WHEN** 配置存在 pending 时 shell 中输入 `apply myapp`
- **THEN** 输出与 `xkeeper apply myapp` 一致的逐程序结果，仅作用于该 app

#### Scenario: 在 shell 中查看 pending

- **WHEN** 配置存在 pending 时 shell 中输入 `pending`
- **THEN** 输出变更程序与 app 注册变化清单；无变更时输出「无变更」

#### Scenario: open 打开 webui

- **WHEN** webui 在默认端口可达且输入 `open`
- **THEN** 系统默认浏览器被调起并指向控制台地址，shell 不退出

#### Scenario: open 在 webui 未启动时

- **WHEN** webui 未启动且输入 `open`
- **THEN** 输出"webui 未启动"的提示（含启动方式提示），不启动浏览器，shell 不退出
