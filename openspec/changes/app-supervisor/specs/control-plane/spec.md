# Delta Spec: control-plane

## Purpose

定义 xkeeper 的本地控制平面：守护进程暴露的 HTTP JSON API（端点、鉴权、错误码）与映射到该 API 的 CLI 控制命令（含退出码约定），使用户可以查询和操控运行中的守护进程。

## ADDED Requirements

### Requirement: 本地 HTTP API

守护进程 SHALL 在 `127.0.0.1` 的可配置端口（`[daemon] port`）上提供 JSON API，MUST NOT 默认绑定非回环地址。API SHALL 提供以下端点：

- `GET /v1/health`：守护进程存活探测
- `GET /v1/status`：守护进程信息与全部程序的状态汇总
- `GET /v1/programs`：程序列表
- `GET /v1/programs/{name}`：单个程序详情（所属应用、状态、pid、存活时长、重启/退出计数、健康状态、等待原因）
- `POST /v1/programs/{name}/start` / `stop` / `restart`
- `GET /v1/programs/{name}/logs`：按 `stream=out|err`、`tail=N` 查询日志
- `POST /v1/reload`：热更新配置
- `POST /v1/shutdown`：停止全部程序并退出守护进程

#### Scenario: 状态汇总可查询

- **WHEN** 向运行中的守护进程请求 `GET /v1/status`
- **THEN** 返回 JSON，包含守护进程信息与每个程序的名称、状态、pid 与 unhealthy 标记

#### Scenario: 非法状态转换被拒绝

- **WHEN** 对已经 `running` 的程序发送 `POST /v1/programs/{name}/start`
- **THEN** 返回 409 与解释性错误，程序状态不受影响

#### Scenario: 未知程序返回 404

- **WHEN** 请求不存在的程序名
- **THEN** 返回 404 与错误信息

#### Scenario: shutdown 优雅退出

- **WHEN** 发送 `POST /v1/shutdown`
- **THEN** 守护进程按关闭流程停止全部程序后进程退出

### Requirement: API 鉴权

未配置鉴权时，API 仅依赖回环绑定保护。配置了 `[daemon] auth_token` 后，除健康探测外，所有请求 SHALL 要求 `Authorization: Bearer <token>`；缺失或不匹配 SHALL 返回 401。

#### Scenario: 配置 token 后拒绝匿名请求

- **WHEN** 配置了 `auth_token` 且请求未携带正确的 Authorization 头
- **THEN** 返回 401，程序状态不被泄露

#### Scenario: 配置 token 后放行正确请求

- **WHEN** 请求携带正确的 Bearer token
- **THEN** 正常返回 200 响应

### Requirement: CLI 控制命令

单二进制 SHALL 同时承担守护进程与客户端两种角色：`xkeeper run` 为守护进程；`xkeeper status|start|stop|restart|reload|log|pid|shutdown` 为控制子命令（通过本地 API 操作守护进程），`xkeeper add|remove|list` 为应用注册子命令（离线直接读写 core 配置，在线时经控制 API 同步）。`log` SHALL 支持 `-f/--follow` 跟随输出与 `--tail N`。控制命令 SHALL 使用如下退出码约定：0 成功；1 一般错误（如非法状态转换、未知程序、注册校验失败）；2 配置错误；3 守护进程不可达。

#### Scenario: 守护进程未运行

- **WHEN** 守护进程未启动时执行 `xkeeper status`
- **THEN** 输出守护进程不可达的提示，退出码为 3

#### Scenario: CLI 停止程序

- **WHEN** 执行 `xkeeper stop <name>` 且该程序正在运行
- **THEN** 命令返回成功，程序进入 `stopped`

#### Scenario: CLI 跟随日志

- **WHEN** 执行 `xkeeper log <name> -f`
- **THEN** 先输出最近的日志，随后持续输出新增行，直到用户中断

### Requirement: 启动冲突处理

守护进程启动时若控制端口已被占用，SHALL 以明确的错误信息退出（非零码），提示可能已有实例在运行；同机重复启动 SHALL NOT 静默成功。

#### Scenario: 端口被占用时拒绝启动

- **WHEN** 第二个守护进程实例以相同配置启动
- **THEN** 启动失败，错误信息指出端口占用与已运行实例的可能
