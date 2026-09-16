## MODIFIED Requirements

### Requirement: 本地 HTTP API

守护进程 SHALL 在 `127.0.0.1` 的可配置端口（`[daemon] port`）上提供 JSON API，MUST NOT 默认绑定非回环地址。API SHALL 提供以下端点：

- `GET /v1/health`：守护进程存活探测
- `GET /v1/status`：守护进程信息与全部程序的状态汇总
- `GET /v1/programs`：程序列表
- `GET /v1/programs/{name}`：单个程序详情（所属应用、状态、pid、存活时长、重启/退出计数、健康状态、等待原因）
- `POST /v1/programs/{name}/start` / `stop` / `restart`
- `POST /v1/programs/{name}/signal`：向该程序的子进程投递白名单信号，请求体指定信号名
- `POST /v1/programs/{name}/actions/{action}`：执行该程序声明的自定义动作，同步等待至完成或超时，返回退出码、耗时与截断输出
- `POST /v1/apps/{name}/start` / `stop` / `restart`：app 级扇出动作，按启动排序（停止为逆序）作用于该 app 全部程序，返回逐程序结果
- `GET /v1/programs/{name}/logs`：按 `stream=out|err`、`tail=N` 查询日志
- `POST /v1/reload`：重扫配置并检出待应用变更，返回 pending 预览（不应用）
- `GET /v1/pending`：查询当前待应用变更（逐程序 changed 清单、app 级注册变化、不可热更字段提示、检出错误）
- `POST /v1/apply`：应用待应用变更，请求体可指定范围（app、program）与 `restart` 布尔参数，返回逐程序结构化结果
- `POST /v1/shutdown`：停止全部程序并退出守护进程

#### Scenario: 状态汇总可查询

- **WHEN** 向运行中的守护进程请求 `GET /v1/status`
- **THEN** 返回 JSON，包含守护进程信息与每个程序的名称、状态、pid 与 unhealthy 标记

#### Scenario: reload 返回 pending 预览

- **WHEN** app 配置修改后向运行中的守护进程请求 `POST /v1/reload`
- **THEN** 返回 pending 预览 JSON（含变更程序清单），无任何程序被停止或重启

#### Scenario: pending 可独立查询

- **WHEN** 配置存在未应用的变更时请求 `GET /v1/pending`
- **THEN** 返回逐程序 changed 清单与 app 级注册变化；无变更时返回空清单

#### Scenario: apply 带范围与 restart 参数

- **WHEN** 请求 `POST /v1/apply`（body 含 `{"app": "A", "restart": false}`）
- **THEN** 仅应用 app A 的 pending，返回逐程序 changed/action/result 的 JSON

#### Scenario: 非法状态转换被拒绝

- **WHEN** 对已经 `running` 的程序发送 `POST /v1/programs/{name}/start`
- **THEN** 返回 409 与解释性错误，程序状态不受影响

#### Scenario: 未知程序返回 404

- **WHEN** 请求不存在的程序名
- **THEN** 返回 404 与错误信息

#### Scenario: shutdown 优雅退出

- **WHEN** 发送 `POST /v1/shutdown`
- **THEN** 守护进程按关闭流程停止全部程序后进程退出

#### Scenario: app 扇出动作

- **WHEN** 请求 `POST /v1/apps/gateway/start`
- **THEN** gateway 的全部程序按排序规则启动，返回逐程序结果；未知 app 返回 404

#### Scenario: 执行自定义动作

- **WHEN** 请求 `POST /v1/programs/api/actions/flush`
- **THEN** 动作同步执行，返回含退出码、耗时、是否超时与输出尾部的 JSON；未知动作名返回 404

#### Scenario: 动作执行中冲突

- **WHEN** 同一程序的同名动作执行中再次请求
- **THEN** 返回 409 与冲突说明，原执行不受影响

#### Scenario: signal 投递

- **WHEN** 请求 `POST /v1/programs/web/signal`（body `{"signal": "USR1"}`）
- **THEN** 信号投递给该程序子进程并返回成功；非白名单信号返回 400 与白名单说明

### Requirement: CLI 控制命令

单二进制 SHALL 同时承担守护进程与客户端两种角色：`xkeeper run` 为守护进程；`xkeeper status|start|stop|restart|reload|apply|log|pid|shutdown` 为控制子命令（通过本地 API 操作守护进程），`xkeeper add|remove|list` 为应用注册子命令（离线直接读写 daemon 配置，在线时经控制 API 同步）。`xkeeper apply` SHALL 支持范围参数（缺省全部 / `<app>` / `<app> <program>`）与 `--restart` 开关，输出逐程序的变更与动作结果。`log` SHALL 支持 `-f/--follow` 跟随输出与 `--tail N`。`xkeeper action <program> <action>` SHALL 执行自定义动作并输出退出码与输出尾部，CLI 退出码 SHALL 透传动作自身的退出码（调用失败为 1，守护不可达为 3）；`xkeeper signal <program> <SIGNAL>` SHALL 投递白名单信号；`xkeeper start|stop|restart --app <name>` SHALL 执行 app 级扇出（服务端按序展开），裸 `<name>` 参数维持既有程序名语义不变。控制命令 SHALL 使用如下退出码约定：0 成功（含 apply 无变更的幂等情形）；1 一般错误（如非法状态转换、未知程序、注册校验失败）；2 配置错误；3 守护进程不可达。

#### Scenario: 守护进程未运行

- **WHEN** 守护进程未启动时执行 `xkeeper status`
- **THEN** 输出守护进程不可达的提示，退出码为 3

#### Scenario: CLI 停止程序

- **WHEN** 执行 `xkeeper stop <name>` 且该程序正在运行
- **THEN** 命令返回成功，程序进入 `stopped`

#### Scenario: CLI 跟随日志

- **WHEN** 执行 `xkeeper log <name> -f`
- **THEN** 先输出最近的日志，随后持续输出新增行，直到用户中断

#### Scenario: apply 无变更幂等退出

- **WHEN** 配置无 pending 时执行 `xkeeper apply`
- **THEN** 输出「无变更」提示，退出码 0，无程序受影响

#### Scenario: CLI 执行动作并透传退出码

- **WHEN** 执行 `xkeeper action api upgrade` 且动作命令以退出码 3 结束
- **THEN** stdout 展示动作的输出尾部，CLI 以退出码 3 退出

#### Scenario: CLI 发送信号

- **WHEN** 执行 `xkeeper signal web USR1`，随后执行 `xkeeper signal web KILL`
- **THEN** 前者投递成功退出码 0；后者报错（不在白名单）退出码 1

#### Scenario: CLI 扇出启动

- **WHEN** 执行 `xkeeper start --app gateway`
- **THEN** 服务端按排序规则逐程序启动，输出逐程序结果；未知 app 报错退出码 1
