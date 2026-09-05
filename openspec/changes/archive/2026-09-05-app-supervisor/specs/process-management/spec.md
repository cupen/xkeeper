# Delta Spec: process-management

## Purpose

定义 xkeeper 对受管子进程生命周期的行为契约：状态机、启动与重启策略、启动/停止排序、优雅停止与进程树清理、健康监控。所有行为在 Linux 与 Windows 上对齐到同一规格，平台差异仅体现在信号与进程树机制上。

## ADDED Requirements

### Requirement: 程序生命周期状态机

每个受管程序的状态 SHALL 是以下之一，且可从控制平面观察到：`stopped`、`starting`、`running`、`stopping`、`exited`、`backoff`、`fatal`。程序另 SHALL 携带 `unhealthy` 标记（默认 false）。守护进程启动时 SHALL 按 `autostart` 配置启动程序；程序进入 `fatal` 后守护进程 MUST NOT 再自动拉起它，直到收到显式的 start 请求或 reload 改变其定义。

#### Scenario: 守护进程启动后自动拉起

- **WHEN** 守护进程以包含 `autostart = true` 程序的配置启动
- **THEN** 该程序被启动并进入 `running`，其 pid 与运行时长可从状态查询中获得

#### Scenario: 稳定运行前退出计入启动失败

- **WHEN** 进程在 `startsecs` 秒内退出
- **THEN** 该次记录为一次失败的启动（消耗 `startretries` 预算），而不是一次普通退出，程序在退避后重新启动

#### Scenario: 稳定运行后退出按重启策略处理

- **WHEN** 进程连续运行超过 `startsecs` 后退出
- **THEN** 启动重试计数清零，退出被记录（退出码、存活时长、累计退出次数），并按重启策略决定后续状态

### Requirement: 重启策略

每个程序 SHALL 支持 `never`、`on-failure`、`always` 三种重启策略，并支持 `exit_codes` 期望退出码列表：`on-failure` 在退出码不在期望列表（默认仅 0）时重启；`always` 无论退出码如何都重启；`never` 不重启。稳态崩溃重启 SHALL 使用指数退避（`restart_backoff` 起步、`max_restart_backoff` 封顶），进程连续运行超过 `backoff_reset_after` 秒后重置退避。

#### Scenario: always 对干净退出也重启

- **WHEN** 策略为 `always` 的程序以退出码 0 退出
- **THEN** 程序按退避计划重启

#### Scenario: on-failure 对期望退出码不重启

- **WHEN** 策略为 `on-failure`、`exit_codes` 为 `[0]` 的程序以退出码 0 退出
- **THEN** 程序进入 `exited`，不再重启

#### Scenario: on-failure 对非期望退出码重启

- **WHEN** 策略为 `on-failure` 的程序以退出码 3 退出
- **THEN** 程序按退避计划重启

#### Scenario: 启动重试耗尽进入 fatal

- **WHEN** 程序连续失败的启动次数超过 `startretries`（例如命令不存在，每次都在 startsecs 内退出）
- **THEN** 程序进入 `fatal` 并在状态中给出原因，守护进程不再自动拉起它

### Requirement: 启动与停止排序

程序 SHALL 支持 `priority`（数值越小越先启动）与 `depends_on`（依赖的程序名列表）。守护进程启动与 reload 引入新程序时，SHALL 按 priority 排序启动；被依赖程序未进入 `running` 前，依赖它的程序 MUST NOT 被启动；守护进程关闭时 SHALL 按与启动相反的顺序停止程序。

#### Scenario: 依赖未就绪时等待

- **WHEN** 程序 B `depends_on` 程序 A，且 A 尚未进入 `running`
- **THEN** B 保持 `stopped` 并显示等待原因，直到 A 运行后才被启动

#### Scenario: 依赖失败时放弃启动

- **WHEN** 程序 A 进入 `fatal` 或 `exited`（且不再重启），而 B 依赖 A
- **THEN** B 进入 `fatal`，原因标明依赖不可用

#### Scenario: 关闭时逆序停止

- **WHEN** 守护进程收到关闭信号，A 的优先级先于 B 启动
- **THEN** B 先被停止，A 后被停止

### Requirement: 优雅停止

对运行中的程序发出停止请求时，xkeeper SHALL 先请求优雅终止（Unix 上发送 SIGTERM），等待至多 `stop_timeout` 秒；超时后 SHALL 强制终止（Unix 上 SIGKILL，Windows 上 TerminateProcess）。停止完成后程序 SHALL 进入 `stopped`。

#### Scenario: 进程在超时内自行退出

- **WHEN** 对运行中的程序发出 stop，进程在 `stop_timeout` 内响应 SIGTERM 退出
- **THEN** 程序进入 `stopped`，未使用强制终止

#### Scenario: 进程无视优雅信号被强杀

- **WHEN** 进程在 `stop_timeout` 内未退出
- **THEN** xkeeper 强制终止该进程，程序仍进入 `stopped`，且记录使用了强制终止

### Requirement: 进程树清理

守护进程退出时 MUST NOT 遗留存活的子进程。在 Windows 上，该保证 SHALL 由作业对象（kill-on-close）兜底：即使守护进程被强制杀死，子进程树也被内核回收。在 Unix 上，xkeeper SHALL 将子进程放入独立进程组，停止程序时终止整个进程组。

#### Scenario: 守护进程被强制杀死后无孤儿（Windows）

- **WHEN** 在 Windows 上守护进程被外部强杀
- **THEN** 其子进程树在守护进程死亡后被内核一并终止

#### Scenario: 孙进程不残留

- **WHEN** 被守护的程序又派生了子进程，随后该程序被 stop
- **THEN** 该程序派生的孙进程不继续存活（按平台机制尽力保证）

### Requirement: 健康监控

每个程序 SHALL 支持可选健康检查，类型为 `none`（默认）、`tcp`（连通目标地址端口）、`http`（请求 URL 期待 2xx/3xx）、`exec`（执行命令，退出码 0 为健康），并可配置 `interval`、`timeout`、`retries`、`start_period`。连续失败达到 `retries` 次后程序 SHALL 被标记为 `unhealthy`；配置了 `restart_on_unhealthy = true` 时，unhealthy SHALL 触发与崩溃一致的重启流程。`start_period` 内的失败不计入连续失败次数。

#### Scenario: 健康检查通过

- **WHEN** 配置 http 健康检查的程序其健康端点在 interval 周期内返回 2xx
- **THEN** 程序保持 `running` 且 unhealthy 标记为 false

#### Scenario: 连续失败触发 unhealthy

- **WHEN** 健康检查连续失败达到 retries 次
- **THEN** 程序被标记为 unhealthy；若启用了 restart_on_unhealthy，则按重启策略重启并清除 unhealthy

#### Scenario: 预热期失败不计入

- **WHEN** 程序刚启动仍在 `start_period` 内且健康检查失败
- **THEN** 这些失败不计入连续失败计数
