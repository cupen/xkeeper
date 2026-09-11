# webui-api Specification Delta — add-apply-command

## MODIFIED Requirements

### Requirement: 控制台端点复用控制面投影
控制台服务器 SHALL 在自己的回环端口（`xkeeper webui --listen`，默认
`127.0.0.1:9877`）提供 `/api/*` 查询端点，其状态数据 SHALL 与控制面 `/v1/*`
使用同一投影（同构类型、同源数据）：
- `GET /api/health` —— 控制台存活探针；
- `GET /api/overview` —— 守护进程概况 + 全部程序状态（与 `GET /v1/status` 同构）；
- `GET /api/programs`、`GET /api/programs/{name}`；
- `GET /api/programs/{name}/logs?stream=out|err&tail=<n>` —— 取自内存环形缓冲；
- `POST /api/programs/{name}/start|stop|restart` —— 经命令队列由守护循环执行；
- `GET /api/pending` —— 待应用变更清单（与 `GET /v1/pending` 同源）；
- `POST /api/apply` —— 应用待应用变更（与 `POST /v1/apply` 同源：范围参数、
  `restart` 布尔、逐程序结构化结果），经命令队列由守护循环执行。

未知程序 MUST 返回 404 与错误对象；非法状态迁移 MUST 返回 409 与原因。

#### Scenario: 投影与控制面一致
- **WHEN** 控制面 `/v1/status` 与控制台 `/api/overview` 在同一时刻被请求
- **THEN** 两者的程序状态字段语义一致（同源投影）

#### Scenario: pending 与控制面一致
- **WHEN** 控制面 `GET /v1/pending` 与控制台 `GET /api/pending` 在同一时刻被请求
- **THEN** 返回的待应用变更清单一致（同源投影）

#### Scenario: apply 结果与控制面一致
- **WHEN** 分别经 `/api/apply` 与 `/v1/apply` 对同一 pending 状态发起 apply
- **THEN** 两者接受相同的请求参数并返回同构的逐程序结果

#### Scenario: 非法迁移被拒绝
- **WHEN** 对 running 的程序 `POST /api/programs/{name}/start`
- **THEN** 返回 409 与原因，程序状态不受影响

#### Scenario: 日志来自环形缓冲
- **WHEN** 程序日志文件已被轮转后请求 `tail=N`
- **THEN** 返回的最近 N 行来自内存缓冲，与磁盘轮转状态无关

### Requirement: WebSocket 推送通道
控制台 SHALL 在 `/ws` 提供 WebSocket 推送通道：
- 连接建立后服务端 MUST 立即下发一次全量快照（与 `/api/overview` 同构）；
- 其后状态变化 MUST 以增量事件推送（仅变化的程序条目），推送延迟不高于
  一个 monitor_interval 量级；
- 待应用（pending）变更 SHALL 随快照携带，且 pending 变化（检出更新、apply
  清空）MUST 以增量事件推送，推送延迟不高于一个重扫周期量级；
- 客户端 SHALL 可按程序与流方向订阅日志；订阅后先补发环形缓冲的 tail 再
  跟随新行；
- 服务端 MUST 周期发送心跳；连接断开后客户端重连时重新获得全量快照（无跨
  连接续传要求）。

#### Scenario: 连接即快照
- **WHEN** 客户端建立 WebSocket 连接
- **THEN** 首条消息为全量快照（含 pending 状态），其后才是增量事件

#### Scenario: 状态变化推送
- **WHEN** 某程序在守护循环中由 running 进入 backoff
- **THEN** 已连接客户端在一个 monitor_interval 量级内收到该程序的增量状态事件

#### Scenario: pending 变化推送
- **WHEN** 配置被编辑使某程序进入 pending，随后 `xkeeper apply` 清空 pending
- **THEN** 已连接客户端分别在一个重扫周期量级内收到 pending 增加与清空的事件

#### Scenario: 日志订阅先补发后跟随
- **WHEN** 客户端订阅某程序的 stdout 且订阅时缓冲已有内容
- **THEN** 先收到缓冲 tail，再持续收到新输出行
