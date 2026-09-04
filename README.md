# xkeeper

轻量级进程守护工具，使用 Rust 编写，通过 TOML 文件配置。

xkeeper 读取一个 TOML 配置文件，把其中声明的程序作为子进程启动并持续守护：
进程退出后按指数退避策略自动重启，stdout/stderr 分别落盘到日志文件，收到
Ctrl+C / SIGTERM 时优雅停机。可以把它看成一个单机版的迷你 supervisor。

## 功能

- TOML 配置，支持任意多个 `[[program]]`
- 退出后自动重启，指数退避（`restart_backoff` 逐次翻倍，封顶 `max_restart_backoff`），
  进程稳定运行 `backoff_reset_after` 秒后计数自动清零
- 连续重启超过 `max_restarts` 次进入 fatal 状态，不再重启
- 每个程序独立的 stdout / stderr 日志文件（追加写入）
- 优雅停机：Unix 先发 SIGTERM，等待 `stop_timeout` 秒后再 SIGKILL；Windows 直接 TerminateProcess
- Windows 下每个子进程绑定到 Job Object（kill-on-close），xkeeper 无论正常退出还是被强杀，
  整个子进程树都会被内核清理，不留孤儿进程
- `xkeeper validate` 子命令校验配置文件（字段名、取值范围、重名检查等）

## 构建

需要 Rust 1.75+：

```bash
cargo build --release
# 产物: target/release/xkeeper(.exe)
```

## 快速开始

仓库根目录自带一个演示配置（持续 ping，被杀后自动重启）：

```bash
cargo run -- validate -c config.toml   # 先校验
cargo run -- run -c config.toml        # 前台运行
# 日志输出在 ./logs/demo-ping.out.log / .err.log
```

## 命令行用法

```text
xkeeper run    [-c <config>]    前台运行守护进程（缺省命令）
xkeeper validate [-c <config>]  校验配置文件后退出
```

- `-c/--config`：配置文件路径，默认 `./config.toml`
- 日志级别可用环境变量 `RUST_LOG` 覆盖（如 `RUST_LOG=debug`），否则取配置中的 `daemon.log_level`
- 收到 Ctrl+C（Windows/Unix）或 SIGTERM（Unix）后，xkeeper 会依次停止所有子进程再退出

## 配置参考

```toml
[daemon]
log_level = "info"       # xkeeper 自身日志级别: trace/debug/info/warn/error
log_dir = "logs"         # 子进程 stdout/stderr 日志目录
monitor_interval = 1.0   # 巡检间隔（秒），退出检测与重启的最小粒度

[[program]]
name = "my-service"              # 必填，全局唯一，同时用作日志文件名（禁止 / \ : * ? " < > |）
command = "python"               # 必填，可执行文件（相对路径或走 PATH 查找）
args = ["-m", "http.server"]     # 参数列表
working_dir = "."                # 子进程工作目录，默认为配置文件所在目录
autorestart = true               # 退出后是否自动重启
restart_backoff = 1.0            # 首次重启延迟（秒），之后 ×2 ×4 ×8 ...
max_restart_backoff = 30.0       # 退避上限（秒）
max_restarts = 0                 # 连续重启上限，超过进入 fatal；0 = 不限制
stop_timeout = 10.0              # Unix: SIGTERM 后等待秒数，超时 SIGKILL
environment = { FOO = "bar" }    # 附加环境变量（在继承的环境之上追加）
backoff_reset_after = 60.0       # 连续运行超过该秒数，重启计数与退避清零
```

说明：

- `log_dir`、`working_dir` 若为相对路径，基于**配置文件所在目录**解析（与 xkeeper 进程的当前目录无关）。
- 每个 `[[program]]` 的输出写入 `{log_dir}/{name}.out.log` 与 `{log_dir}/{name}.err.log`（追加模式，无轮转）。
- 一个程序从 `running → backoff → running` 循环；`exited` 表示正常退出且未开启自动重启；
  `fatal` 表示连续重启次数耗尽，需要人工介入。
- 跨平台差异：Windows 没有 SIGTERM，`stop` 等价于 TerminateProcess（子进程树由 Job Object 兜底清理）；
  Unix 上先 SIGTERM 再按 `stop_timeout` 强杀。

## 作为系统服务运行

xkeeper 本身前台运行，长期驻留建议交给系统服务管理器：

**Linux (systemd)** — `/etc/systemd/system/xkeeper.service`：

```ini
[Unit]
Description=xkeeper process keeper
After=network.target

[Service]
ExecStart=/usr/local/bin/xkeeper run -c /etc/xkeeper/config.toml
Restart=always
RestartSec=3
User=app

[Install]
WantedBy=multi-user.target
```

**Windows (nssm)**：

```bat
nssm install xkeeper D:\tools\xkeeper\xkeeper.exe
nssm set xkeeper AppParameters "run -c D:\tools\xkeeper\config.toml"
nssm start xkeeper
```

Windows 服务方式下没有控制台，无法接收 Ctrl+C；nssm 停止服务时强杀 xkeeper，
子进程树由 Job Object（kill-on-close）保证一并清理。

## 测试

```bash
cargo test
```

测试覆盖：配置解析与校验（默认值、重名、非法字段、非法名称）、
失败进程自动重启、`max_restarts` 耗尽进入 fatal、运行中进程停止并产生日志文件。

## 设计与限制

- 退出检测粒度等于 `daemon.monitor_interval`，重启延迟误差不超过一个巡检周期。
- 子进程日志为原始追加写入，不做轮转；需要轮转可配合 logrotate（Unix）或外部工具。
- Unix 上 xkeeper 被强杀时子进程会被 init 收养（不随主进程退出）；Windows 上由 Job Object 保证清理。
- 停止子进程只处理直接子进程；Windows 上孙进程由 Job Object 覆盖，Unix 上暂未处理进程组。
