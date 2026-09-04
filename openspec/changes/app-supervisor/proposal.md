# Proposal: app-supervisor — 把 xkeeper 升级为跨平台应用层守护进程管理器

## Why

xkeeper v0.1 目前只是一个最小化的进程保活器（spawn + 指数退避重启 + 日志落盘），缺少一个真正"守护进程管理器"的核心能力：无法查询与操控运行状态、无法表达启动顺序与依赖、没有健康检查与日志轮转、改配置必须重启整个守护进程。同类工具各有短板：systemd 深入系统层（units/cgroups）且仅限 Linux；supervisord 面向应用层但不支持 Windows；nssm/WinSW 只能包装单个服务。市场缺少一个像 supervisord 一样贴近应用层（工作目录、环境变量、应用健康端点、应用日志），却能在 Linux 与 Windows 上以单一静态二进制一致运行的轻量级通用守护进程。

## What Changes

- **新增本地控制平面**：守护进程在 `127.0.0.1` 上暴露 HTTP JSON API（默认端口可配置，支持可选 token 鉴权）；新增 `xkeeper status / start / stop / restart / reload / log / pid / shutdown` 等 CLI 控制子命令，作为 API 的薄客户端。`xkeeper run`（守护进程）语义保持不变。
- **进程运行时升级**：引入 `startsecs` / `startretries`（启动后必须稳定运行才算 started，否则计入启动重试）、重启策略 `never | on-failure | always` 与期望退出码 `exit_codes`、`priority` 与 `depends_on` 驱动的启动/停止排序、健康检查（`tcp | http | exec`），状态机规范化为 `stopped → starting → running → stopping → exited / backoff / fatal`，并增加 `unhealthy` 标记。
- **日志管理重构**：子进程 stdout/stderr 改为由 xkeeper 通过管道接管（泵线程写盘），支持按大小轮转（保留 N 份）、内存环形缓冲（供 `log --tail/-f` 快速查看与跟随）。
- **配置热更新**：`xkeeper reload` 重读 core 与全部已注册 app 配置并按应用粒度增量应用；单个 app 文件非法时隔离失败、不影响其他应用。`xkeeper add/remove` 在守护进程运行时即时生效。
- **分层配置模型**：配置拆分为 core 与 app——core 根配置全局唯一（Linux `/etc/xkeeper.toml`、Windows `%APPDATA%\xkeeper\`）且只做全局：`[daemon]` 设置（含 `app_dir` 应用注册目录）+ 可选 `[app-default]` 应用默认值表，禁止任何应用专属条目；app 配置本体随各应用部署目录存放（默认名 `xkeeper.toml`），采用 `[program.<name>]` map 接口与扁平字段（`work_dir`/`env`/`log_max_size`、单行 `command`、单字符串 `health_check` 按协议分发）。全新安装默认空无一物，部署人执行 `xkeeper add . --name xxx` 注册：`app_dir` 建立链接即注册记录，微调 flag（开机自启、崩溃自动拉起、重启间隔等）写入 app 配置 `[app]` 表。
- **跨平台行为矩阵**：Unix 采用 setsid + 进程组（优雅停止 SIGTERM→超时 SIGKILL），Windows 维持 Job Object 兜底 + TerminateProcess，两侧行为对齐到同一规格。

## Capabilities

### New Capabilities

- `process-management`: 受管进程的生命周期规格——状态机、启动/重启策略（startsecs、startretries、exit_codes）、priority/depends_on 排序、优雅停止与强杀、进程树清理、健康检查与 unhealthy 标记。
- `configuration`: 分层配置——core 根配置（`[daemon]` + `[app-default]`，禁止应用条目）与 app 配置（`[program.*]` map、扁平字段、单行 command、协议分发健康检查）的字段全集、四层优先级、校验规则、v0.1 旧配置迁移、按应用粒度的热更新语义。
- `app-registry`: 应用注册生命周期——`xkeeper add/remove/list`（`add .`、`--name` 缺省目录名）、以 `app_dir` 链接为注册记录、微调 flag 写入 app 配置 `[app]` 表、注册时校验、离线可用与在线同步。
- `control-plane`: 本地控制平面——HTTP JSON API 端点集、token 鉴权、状态转换错误码，以及映射到 API 的 CLI 控制命令与退出码约定。
- `log-management`: 子进程输出接管——泵线程写盘、按大小轮转（保留份数）、环形缓冲、`log --tail/--follow` 的查看与跟随行为。

### Modified Capabilities

（无——`openspec/specs/` 目前为空，本变更是首批主规范的来源，后续变更才会出现修改类条目。）

## Impact

- **代码**：`src/config.rs`（core/app 分层 schema、校验、优先级解析）、`src/registry.rs`（应用注册表、`app_dir` 链接与 core 配置原子写入、legacy 导入）、`src/program.rs`（状态机与重启/健康状态重写）、`src/supervisor.rs`（排序启动、依赖等待、reload、命令处理循环）、新增 `src/server.rs`（HTTP API）、`src/client.rs`（CLI 控制客户端）、`src/pump.rs`（日志泵与轮转）、`src/health.rs`（健康检查器）。
- **依赖**：新增 `tiny_http`（同步 HTTP，维持纯线程模型，不引入 tokio）与 `serde_json`；其余保持不变。
- **CLI/兼容性**：单二进制双角色（`run` = 守护进程，其余子命令 = 客户端/注册管理）；守护进程未运行时控制命令以退出码 3 报错；v0.1 单文件配置通过 `xkeeper add` 一键迁移为应用注册，原文件不修改。
- **平台**：Linux x86_64/aarch64 与 Windows x86_64；行为差异集中收敛在 `src/platform.rs`，规格层面对齐。
- **文档/测试**：README 重写（分层配置与注册模型、控制平面与 CLI 用法）、core/app 示例配置更新；单元测试 + 真实子进程集成测试 + API 集成测试同步扩充。
