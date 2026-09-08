# webui-api Specification

## Purpose
定义 xkeeper Web 控制台的伺服与推送契约：控制台服务器与守护进程同进程、
复用控制面的状态投影与命令队列；WebSocket 通道向浏览器推送状态增量与日志
分块；规定各通道的数据格式与效率约束。

## Requirements

### Requirement: 控制台端点复用控制面投影
控制台服务器 SHALL 在自己的回环端口（`xkeeper webui --listen`，默认
`127.0.0.1:9877`）提供 `/api/*` 查询端点，其状态数据 SHALL 与控制面 `/v1/*`
使用同一投影（同构类型、同源数据）：
- `GET /api/health` —— 控制台存活探针；
- `GET /api/overview` —— 守护进程概况 + 全部程序状态（与 `GET /v1/status` 同构）；
- `GET /api/programs`、`GET /api/programs/{name}`；
- `GET /api/programs/{name}/logs?stream=out|err&tail=<n>` —— 取自内存环形缓冲；
- `POST /api/programs/{name}/start|stop|restart` —— 经命令队列由守护循环执行。

未知程序 MUST 返回 404 与错误对象；非法状态迁移 MUST 返回 409 与原因。

#### Scenario: 投影与控制面一致
- **WHEN** 控制面 `/v1/status` 与控制台 `/api/overview` 在同一时刻被请求
- **THEN** 两者的程序状态字段语义一致（同源投影）

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
- 客户端 SHALL 可按程序与流方向订阅日志；订阅后先补发环形缓冲的 tail 再
  跟随新行；
- 服务端 MUST 周期发送心跳；连接断开后客户端重连时重新获得全量快照（无跨
  连接续传要求）。

#### Scenario: 连接即快照
- **WHEN** 客户端建立 WebSocket 连接
- **THEN** 首条消息为全量快照，其后才是增量事件

#### Scenario: 状态变化推送
- **WHEN** 某程序在守护循环中由 running 进入 backoff
- **THEN** 已连接客户端在一个 monitor_interval 量级内收到该程序的增量状态事件

#### Scenario: 日志订阅先补发后跟随
- **WHEN** 客户端订阅某程序的 stdout 且订阅时缓冲已有内容
- **THEN** 先收到缓冲 tail，再持续收到新输出行

### Requirement: 数据格式
- REST 端点 SHALL 返回 JSON（`application/json`）。
- WebSocket 的结构化消息（快照、状态增量）SHALL 使用二进制帧：1 字节消息
  类型 + MessagePack 载荷；日志分块 SHALL 以二进制帧承载原始 UTF-8 文本
  （不做 JSON/MessagePack 字符串转义）。
- WS 结构化载荷与 REST JSON MUST 来自同一组 serde 结构体（编码同构），
  避免两份字段定义。
- HTTP 响应与 WS 结构化帧 SHOULD 启用压缩（HTTP gzip；WS 帧头压缩标记位 +
  zlib 或等价机制）。

#### Scenario: 编码同构
- **WHEN** 同一快照分别以 JSON 与 MessagePack 编码
- **THEN** 解码后字段与语义完全一致（同一结构体源）

#### Scenario: MessagePack 体积收益
- **WHEN** 对 10 个程序的状态快照分别编码
- **THEN** MessagePack 体积 ≤ 同构 JSON 的 90%

#### Scenario: 日志帧无转义放大
- **WHEN** 一段 100 行的日志文本经日志帧推送
- **THEN** 帧体为原始 UTF-8 字节（加固定头部），不含引号/反斜杠转义放大

### Requirement: 与守护进程同进程共存
控制台服务器 MUST 与守护循环运行在同一进程并共享同一 `Supervisor`：状态读取
SHALL 来自共享状态（与控制面相同的锁），控制命令 SHALL 经命令队列在守护循环
中执行，日志 SHALL 来自 pump 环形缓冲。控制台服务器 SHALL 随守护进程关闭而
退出（Ctrl+C 或 `POST /v1/shutdown`）。`xkeeper run` 保持纯守护、行为不变。

#### Scenario: webui 反映真实状态
- **WHEN** `xkeeper webui` 运行且某程序进入 fatal
- **THEN** `GET /api/programs/{name}` 与 WebSocket 推送均反映 fatal

#### Scenario: 随守护进程退出
- **WHEN** 发送 `POST /v1/shutdown` 或 Ctrl+C
- **THEN** 控制台服务器随之退出，进程正常终止
