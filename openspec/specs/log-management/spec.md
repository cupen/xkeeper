# log-management Specification

## Purpose

定义 xkeeper 对子进程输出的接管与日志服务契约：输出捕获与落盘、按大小轮转、内存环形缓冲，以及日志查询/跟随行为和守护进程自身日志。

## Requirements

### Requirement: 输出捕获

xkeeper SHALL 捕获每个子进程的 stdout 与 stderr，并分别追加写入 `{log_dir}/{name}.out.log` 与 `{log_dir}/{name}.err.log`；子进程的 stdin SHALL 连接到空输入。程序重启 SHALL 继续追加到同一组文件而不是覆盖。

#### Scenario: 标准输出落盘

- **WHEN** 受管程序向 stdout 写入文本
- **THEN** 该文本出现在该程序的 out.log 中

#### Scenario: 重启后追加不覆盖

- **WHEN** 程序崩溃并被自动重启后再次输出
- **THEN** 新输出追加在同一日志文件末尾，先前内容保留

### Requirement: 按大小轮转

每个日志文件 SHALL 默认启用按大小轮转：阈值缺省 `50MB`、保留份数缺省 `2`（每流盘上最多 3 个文件：当前 + 2 份轮转），out 与 err 两个流 SHALL 各自独立轮转、互不影响。程序 SHALL 可通过 `log_max_size = "0"` 显式禁用轮转。超过阈值时当前文件重命名为 `.1`，更早的轮转文件依次顺延，超出保留份数的最旧文件被删除。轮转 SHALL 发生在写入路径上，不丢失已写入的行。

#### Scenario: 缺省配置下默认轮转

- **WHEN** 程序未声明任何轮转字段，某流日志文件大小超过 50MB 后程序继续输出
- **THEN** 当前文件被移入 `.1`，新输出从当前文件继续，无需任何配置

#### Scenario: 超过阈值触发轮转

- **WHEN** 显式配置 `max_size` 的程序日志文件大小超过阈值后继续输出
- **THEN** 旧内容被移入 `.1` 文件，当前文件从新内容继续

#### Scenario: 保留份数受限

- **WHEN** 轮转文件数量超过 `rotate_keep`
- **THEN** 最旧的轮转文件被删除

#### Scenario: 两流独立轮转

- **WHEN** 程序的 err 流已轮转多次而 out 流从未超过阈值
- **THEN** 仅 err 流发生轮转，out 流保持单一文件继续追加

#### Scenario: 显式禁用轮转

- **WHEN** 程序声明 `log_max_size = "0"` 且日志持续增长
- **THEN** 任何流都不发生轮转，日志持续追加到同一文件

### Requirement: 环形缓冲与日志查询

xkeeper SHALL 为每个输出流在内存中保留有界数量的最近日志行（可配置），供 `log` 查询与跟随使用，不依赖读取磁盘文件；查询 SHALL 支持 `tail=N` 返回最近 N 行，`follow` SHALL 持续推送新增行直到客户端断开或用户中断。

#### Scenario: tail 不受轮转影响

- **WHEN** 日志文件已被轮转多次后查询 `tail=100`
- **THEN** 返回的最近 100 行来自内存缓冲，与磁盘轮转状态无关

#### Scenario: follow 实时推送

- **WHEN** 客户端处于 follow 状态时程序输出新行
- **THEN** 新行在写入磁盘的同时被推送给客户端

### Requirement: 守护进程自身日志

xkeeper 自身的生命周期事件（启动/关闭、程序启停、退出原因、健康状态变化、reload 结果）SHALL 以带时间戳的行输出到 stderr，级别可通过 `RUST_LOG` 或配置的 `log_level` 调整。

#### Scenario: 程序启停事件可见

- **WHEN** 守护进程启动一个程序或该程序退出
- **THEN** 守护进程日志中出现带时间戳、程序名、pid 与原因的记录
