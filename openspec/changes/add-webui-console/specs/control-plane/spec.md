## MODIFIED Requirements

### Requirement: 本地 HTTP API

守护进程 SHALL 在 `127.0.0.1` 的可配置端口（`[daemon] port`）上提供 JSON API，MUST NOT 默认绑定非回环地址。API SHALL 提供以下端点：

- `GET /v1/health`：守护进程存活探测
- `GET /v1/status`：守护进程信息与全部程序的状态汇总
- `GET /v1/programs`：程序列表
- `GET /v1/programs/{name}`：单个程序详情（所属应用、状态、pid、存活时长、重启/退出计数、健康状态、等待原因、CPU 利用率与驻留内存（可空，metrics 能力口径）、stdout/stderr 日志输出速率）
- `POST /v1/programs/{name}/start` / `stop` / `restart`
- `GET /v1/programs/{name}/logs`：按 `stream=out|err`、`tail=N` 查询日志
- `POST /v1/reload`：热更新配置
- `POST /v1/shutdown`：停止全部程序并退出守护进程

程序详情与状态汇总中的指标字段为增量扩展：仅新增可空字段，既有字段名与
语义不变，既有消费者（含 CLI 控制命令）行为不受影响。

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

#### Scenario: 详情含指标且向后兼容

- **WHEN** 请求 `GET /v1/programs/{name}` 且该程序运行中
- **THEN** 详情含可空的 cpu_percent、mem_bytes 与日志速率字段；不带新
  字段消费旧 JSON 的既有客户端仍能解析全部既有字段
