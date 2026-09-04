# xkeeper

跨平台、应用层的轻量级进程守护工具，Rust 单二进制，TOML 分层配置。
Linux 与 Windows 上行为一致：崩溃自动拉起、启动顺序与依赖、健康检查、
日志轮转与跟随、本地控制平面（HTTP API + CLI），内置 Web UI 另立变更。

```text
         ┌────────────────────────── xkeeper run ──────────────────────────┐
         │  supervisor loop（7 态状态机 × N 程序）                          │
         │    spawn/stop/restart · startsecs/startretries · 退避重启        │
         │    priority/depends_on 启动编排 · reload · shutdown              │
         │  ├─ pump×2/程序   输出接管 → 轮转落盘 + 环形缓冲 + follow 广播     │
         │  ├─ health checker tcp/http/exec 探测 → unhealthy/自动重启        │
         │  └─ control API  127.0.0.1:7310 /v1/*（可选 Bearer 鉴权）         │
         └───────────────△─────────────────────────────────────────────────┘
                         │ HTTP JSON
   xkeeper status/start/stop/restart/log/pid/reload/shutdown
   xkeeper add/remove/list        ← 离线可用，在线时自动同步
```

## 模型：core 全局 + app 分布 + app_dir 注册

- **core 根配置**（全局唯一）：Linux `/etc/xkeeper.toml`，Windows
  `%APPDATA%\xkeeper\xkeeper.toml`，`-c` 可覆盖；**文件可以不存在**——
  xkeeper 以内置默认值空启动。只放全局内容：`[daemon]`（含 `app_dir` 注册
  目录）与可选 `[app-default]`（所有应用共享的默认值）。任何应用专属条目
  都会被未知字段校验拒绝。
- **app 配置**（每个应用一份）：放在应用自己的部署目录，默认名
  `xkeeper.toml`，由部署人维护。`[program.<name>]` map 接口，键即程序名，
  绝大多数字段可省略。
- **注册即链接**：在部署目录执行 `xkeeper add .`（名字默认取目录名），
  `app_dir` 里出现 `<name>.toml` 链接指向配置本体——这就是注册记录；
  `xkeeper list` 扫描它，`xkeeper remove` 删除它（部署文件永不删除）。
  Windows 无符号链接权限时降级为硬链接（见下方"注意"）。

字段优先级（高 → 低）：`[program.*]` 显式字段 > app 配置 `[app]` 表 >
core `[app-default]` > 内置默认。`autostart`/`priority` 是应用级字段。

## 快速上手

```bash
cargo build --release          # 产物: target/release/xkeeper(.exe)

# 1. 在你的应用部署目录写一个 xkeeper.toml（见 examples/demo-app）
# 2. 注册（空配置也能先跑起来）
cd /opt/myapp && xkeeper add . --name myapp --autostart
xkeeper list

# 3. 启动守护进程（默认空无一物，只拉起已注册应用）
xkeeper run

# 4. 控制
xkeeper status
xkeeper stop myapp-程序名
xkeeper log <程序名> --tail 50 -f
xkeeper reload                 # 重读 core + 全部 app 配置（按应用隔离失败）
xkeeper shutdown
```

## 配置示例

core（`examples/core.toml`）：

```toml
[daemon]
log_level = "info"
log_dir = "logs"
monitor_interval = 1.0
host = "127.0.0.1"          # 控制平面仅回环
port = 7310
auth_token = ""             # 非空则要求 Bearer 鉴权
log_buffer_lines = 1000
app_dir = "apps"            # 注册目录，缺省 core 同级 apps/

[app-default]               # 可选：全体应用默认值
autostart = true
autorestart = "always"
restart_backoff = 1.0
```

app（部署目录 `xkeeper.toml`，`examples/demo-app/xkeeper.toml`）：

```toml
[app]                       # 可选；add 的微调 flag 写在这里，部署人可手改
description = "演示应用"
autorestart = "on-failure"

[program.api]               # 键即程序名
command = "python -m http.server 8000"   # 单行写法（词法拆分，不经 shell）；或 command + args
work_dir = "."
env = { FOO = "bar" }
startsecs = 1.0             # 存活超过此时长才算启动成功（预算 startretries 次失败）
stop_timeout = 10
exit_codes = [0]            # on-failure 的"期望退出码"
depends_on = ["db.main"]    # 可跨应用引用；成环在注册期拒绝
log_max_size = "10MB"       # 日志按大小轮转，保留 log_rotate_keep 份
log_rotate_keep = 5
health_check = "http://127.0.0.1:8000/health"   # http(s):// | tcp://host:port | exec 命令行
health_interval = 10        # 健康连续失败 health_retries 次 -> unhealthy
restart_on_unhealthy = true # unhealthy 触发与崩溃一致的重启
```

## 状态机与重启语义

```
[stopped] ──spawn──▶ [starting] ──存活≥startsecs──▶ [running]
                        │ startsecs 内退出（消耗 startretries 预算）
                        ▼
                     [backoff] ──退避到期──▶ [starting]
     startretries 耗尽 ──▶ [fatal]（仅显式 start / reload 清除）
[running] ──退出──按 autorestart 策略──▶ [backoff] 或 [exited]
任意 ──stop──▶ [stopping]（SIGTERM→超时强杀 / TerminateProcess）──▶ [stopped]
```

- 重启策略：`always`（含干净退出）、`on-failure`（退出码 ∉ `exit_codes` 时重启）、`never`
- 稳态崩溃退避：`restart_backoff` 指数翻倍至 `max_restart_backoff`；
  连续运行 `backoff_reset_after` 秒后计数清零
- 依赖编排：priority 小者先启动；依赖未 running 则等待；依赖 fatal/exited
  则依赖方进入 fatal（原因注明）；关闭按逆序停止

## 控制平面

守护进程在 `host:port`（默认回环 7310）提供 JSON API：
`GET /v1/health|status|programs|programs/{name}|programs/{name}/logs`，
`POST /v1/programs/{name}/start|stop|restart`、`/v1/reload`、`/v1/shutdown`。
配置 `auth_token` 后除 `/v1/health` 外都要求 `Authorization: Bearer <token>`。
`GET /v1/programs/{name}/logs?stream=out|err&tail=N&follow=1` 支持流式跟随。

CLI 退出码：`0` 成功、`1` 一般错误、`2` 配置错误、`3` 守护进程不可达。

## 从 v0.1 迁移

v0.1 单文件配置（`[daemon]` + `[[program]]`）在 `xkeeper add <旧文件>` 时被
自动识别并转换：程序转写为 `[program.*]` 注册为一个应用，`[daemon]` 段提示
并入 core，原文件不修改。`xkeeper run -c <旧文件>` 走同一条导入路径。

## 作为系统服务运行（守护 xkeeper 本身）

Linux (systemd) `/etc/systemd/system/xkeeper.service`：

```ini
[Unit]
Description=xkeeper process keeper
After=network.target

[Service]
ExecStart=/usr/local/bin/xkeeper run
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
```

Windows (nssm)：

```bat
nssm install xkeeper D:\tools\xkeeper\xkeeper.exe
nssm set xkeeper AppParameters run
nssm start xkeeper
```

xkeeper 被强杀时，Windows 上子进程树由 Job Object（kill-on-close）由内核
兜底清理；Unix 上子进程位于独立进程组，正常路径按进程组终止。

## 平台差异与注意

- Windows 优雅停止无 SIGTERM，stop 等价 TerminateProcess（子进程树由 Job
  Object 兜底）；Unix 先 SIGTERM 等待 `stop_timeout` 再 kill 整个进程组。
- Windows 符号链接需要管理员/开发者模式：无权限时 `app_dir` 降级为硬链接。
  硬链接会被 `sed -i` 等"替换文件式"编辑断开——编辑配置后重跑一次
  `xkeeper add .` 刷新链接即可（幂等）；symlink 模式不受影响。
- Linux 上 `/etc/xkeeper.toml` 需 root 写权限：非 root 用户执行 add/remove
  请用 sudo，或 `-c` 指向用户级 core 配置。
- 单行 `command` 只做词法拆分（空白 + 引号），不经过 shell；需要 shell 语义
  显式写 `bash -c "..."`。

## 测试

```bash
cargo test
```

覆盖：分层配置解析与四层优先级、单行 command 拆分、健康检查协议分发、
跨应用重名/依赖成环校验、legacy 导入、日志轮转与环形缓冲、7 态状态机
（真实子进程：重启/启动预算/策略/停止/落盘）、tcp/http/exec 探测。
