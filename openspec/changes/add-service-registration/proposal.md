## Why

xkeeper 目前是前台进程，长期驻留需要用户手写 systemd unit 并手动 enable（README 给的是手工抄写步骤）。这一步容易抄错（路径、Restart 策略、KillSignal、开机自启），也让"装好即用"的体验断在一环。让 xkeeper 自己注册/注销为系统服务，可以一条命令完成守护接入。

## What Changes

- 新增 `xkeeper service` 子命令组（Linux systemd 实现；Windows 暂不支持，命令报清晰错误退出）：
  - `service install [-c <config>] [--now] [--user <name>] [--name <unit>]`：生成 `xkeeper.service` unit 文件到 `/etc/systemd/system/`，执行 `systemctl daemon-reload` 并 `enable`；`--now` 追加立即 `start`。
  - `service uninstall [--name <unit>]`：`stop`（若在运行）、`disable`、删除 unit 文件、`daemon-reload`。
- unit 文件内容按当前核心配置生成：`ExecStart` 指向当前可执行文件绝对路径 + `run -c <核心配置绝对路径>`，`Restart=always`，优雅停机走 SIGTERM（与 xkeeper 自身信号处理一致）。
- 安装/卸载需要 root 权限，无 root 时给出明确错误提示。
- 已存在同名 unit 时默认拒绝覆盖（`--force` 覆盖）。
- README 的"作为系统服务运行"章节更新为使用新命令。

## Capabilities

### New Capabilities
- `service-registration`: xkeeper 自身注册为系统服务（Linux systemd）的安装、卸载与幂等性要求；包括 unit 文件生成规则、权限要求、平台限制行为。

### Modified Capabilities

## Impact

- `src/main.rs`：新增 `service` 子命令组与分发。
- 新增 `src/service.rs`（unit 模板渲染、systemctl 调用、平台判定）。
- 无新增第三方依赖（调用系统 `systemctl`，不使用 systemd crate）。
- 文档：README 安装/服务章节。
- 影响范围限定为 Linux；Windows 分支编译通过但命令运行时报"不支持"。
