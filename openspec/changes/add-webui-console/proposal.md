# add-webui-console

## Why

`xkeeper webui` 的后端管道（REST 投影、WS 推送、环形缓冲、控制命令）已就绪并有
`webui-ui` 规范背书，但前端仍是占位页——控制台不可用；同时守护进程不采集任何
资源指标（CPU/内存），日志 WS 通道逐行发帧，在高速输出（约 10MB/s）下整条
推送路径会成为瓶颈。本变更一次性落地完整控制台，并补齐指标采集与高吞吐日志
推送策略。

## What Changes

- **实现完整 Web 控制台**（兑现既有 `webui-ui` 规范，非新增需求）：左侧
  app→程序两层树、App 概况汇总、进程详情、start/stop/restart 控制按钮、
  stdout/stderr 日志跟随查看器、实时刷新与离线降级。
- **新增指标采集**：每个受管子进程的 CPU% 与 RSS 内存，采样周期 1s；守护
  进程概况附带系统级 CPU/内存总量。Linux 走 `/proc`，Windows 走
  `windows-sys`，不引入 sysinfo 依赖。
- **新增日志速率指标**：pump 线程按流（out/err）统计每秒行数，维护最近
  300 秒滚动计数，投影时计算 1s/10s/1min/5min 窗口速率，UI 可切换展示窗口。
- **指标进入共享投影**：`ProgramInfo` 增补 `cpu_percent`/`mem_bytes`/日志
  速率字段、`DaemonInfo` 增补系统资源概况——`/v1`、`/api`、WS 三处同源
  自动同步（向后兼容的增量字段）。
- **UI 刷新频率控制**：列表/指标显示节流可选 1s/3s/5s/10s（默认 3s），
  持久化到 localStorage；后端采样节奏不受影响。
- **高吞吐日志推送策略**：pump→WS 全链路批量化（按时间/字节数攒批），
  订阅通道有界化；客户端跟不上时丢弃批次并推送「跳过 N 行」显式标记，
  环形缓冲与落盘文件始终完整无损。
- **零干扰原则（先决约束）**：一切新增路径 MUST NOT 影响受管 app 进程
  自身——指标采集只读、不发信号不挂起；日志管道任何环节（落盘、轮转、
  订阅投递、查看者慢消费）不得反压子进程的输出排空；守护进程自身开销
  （CPU/内存）在任意日志速率下有界；新增线程/任务的故障被隔离，不波及
  守护循环与泵线程。

## Capabilities

### New Capabilities

- `metrics`: 守护进程内指标采集与暴露——受管程序 CPU/内存采样、系统资源
  概况、按流日志行速率（滚动窗口），以及采样精度与平台路径要求。

### Modified Capabilities

- `webui-ui`: 新增需求——列表/详情的资源利用率与日志速率展示、刷新频率
  控制（1/3/5/10s 默认 3s）、高速日志下的丢行标记与限流渲染交互。
- `webui-api`: 新增需求——投影携带指标字段（编码同构）、WS 日志帧批量
  化与「跳过 N 行」标记帧、订阅通道有界与背压语义（订阅投递非阻塞，
  慢消费不反压泵与子进程）。
- `control-plane`: `/v1/programs` 系列端点的投影字段增补（增量、向后
  兼容），与 webui 投影同源。
- `log-management`: 新增「输出排空零反压」需求——落盘（含轮转）、环形
  缓冲、订阅投递中的任何阻塞不得传导为子进程输出写的阻塞；磁盘失败
  降级与守护进程内存有界。

## Impact

- **后端（src/）**：新增 `metrics.rs`（采样线程 + 滚动速率计算）；
  `pump.rs`（批量推送 API、行速率计数、有界订阅）；`program.rs`（挂载
  速率计数器）；`server.rs`（`ProgramInfo`/`DaemonInfo` 扩展）；
  `web.rs`（WS 日志批量 flush、丢行标记）；`api.rs`（新帧型）。
- **平台**：Linux `/proc/<pid>/stat|statm`、`/proc/stat`、`/proc/meminfo`；
  Windows `GetProcessTimes`/`GetProcessMemoryInfo`/`GetSystemTimes`/
  `GlobalMemoryStatusEx`（`windows-sys` 需增补 feature）。两条路径都需
  测试覆盖。
- **前端（webui/）**：从占位页实现完整控制台——树导航、概况、详情、控制、
  日志查看器、WS 客户端（含批量日志/标记解码）、刷新节流与偏好持久化、
  离线降级。遵循 `pnpm exec tsc --noEmit && pnpm test && pnpm build` 后
  `cargo build` 重嵌入的构建链。
- **依赖**：仅 `windows-sys` feature 增补，无新 crate。
- **故障隔离**：指标采样线程、WS/HTTP 任务均为守护循环之外的旁路，其
  panic/退出只导致对应能力降级（指标置空、控制台不可用），守护循环与
  受管程序不受影响。
