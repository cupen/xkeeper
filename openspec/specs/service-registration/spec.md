# service-registration Specification

## Purpose

让用户用一条命令把 xkeeper 自身安装为 Linux systemd 服务（开机自启、崩溃自动拉起），并提供对称的卸载命令；在不受支持的平台上给出明确失败。

## Requirements

### Requirement: service install 生成并注册 systemd unit

`xkeeper service install` SHALL 在 `/etc/systemd/system/xkeeper.service` 生成 unit 文件，并执行 `systemctl daemon-reload` 与 `systemctl enable`，使服务开机自启。unit 文件 MUST 满足：

- `ExecStart` 为当前 xkeeper 可执行文件的绝对路径，后接 `run --config <daemon 配置文件绝对路径>`（daemon 配置路径按 `-c/--config` 解析后的路径；未指定时用平台默认路径）。
- `Restart=always` 与 `RestartSec`，崩溃退出后由 systemd 重新拉起。
- 优雅停机走 SIGTERM（与 xkeeper 自身信号处理一致），并配置 `TimeoutStopSec` 不小于配置解析后的最长停止等待。

#### Scenario: 安装成功并开机自启

- **WHEN** 用户以 root 执行 `xkeeper service install`
- **THEN** `/etc/systemd/system/xkeeper.service` 存在且 ExecStart 指向当前可执行文件与解析后的 daemon 配置路径，`systemctl is-enabled xkeeper` 返回 enabled

#### Scenario: --now 立即启动

- **WHEN** 用户执行 `xkeeper service install --now`
- **THEN** 注册完成后执行 `systemctl start xkeeper`，服务进入 running 状态

### Requirement: 安装需要 root 权限

`service install` 与 `service uninstall` MUST 在非 root（有效 uid 非 0）时失败，错误信息说明需要 root，且 MUST NOT 产生任何文件或 systemd 状态变更。

#### Scenario: 非 root 执行安装

- **WHEN** 非 root 用户执行 `xkeeper service install`
- **THEN** 命令以非零退出码失败并提示需要 root，unit 文件不被创建

### Requirement: 同名 unit 已存在时拒绝覆盖

当目标 unit 文件已存在且内容与将生成的不同时，`service install` MUST 默认拒绝并提示使用 `--force`；内容完全相同时 MUST 幂等成功（不重复报错）。

#### Scenario: 重复安装

- **WHEN** unit 文件已存在且再次执行 `xkeeper service install`
- **THEN** 若内容一致则成功且无副作用；若内容不同则失败并提示 `--force`

#### Scenario: --force 覆盖

- **WHEN** 用户执行 `xkeeper service install --force` 且 unit 已存在
- **THEN** unit 文件被覆盖并重新 `daemon-reload` + `enable`

### Requirement: service uninstall 对称注销

`xkeeper service uninstall` SHALL 停止（若在运行）、禁用服务，删除 unit 文件并执行 `daemon-reload`。unit 文件不存在时 MUST 幂等成功并提示未安装。

#### Scenario: 卸载已安装的服务

- **WHEN** 用户以 root 执行 `xkeeper service uninstall`
- **THEN** 服务被 stop + disable，unit 文件被删除，`systemctl is-enabled xkeeper` 失败

#### Scenario: 卸载未安装的服务

- **WHEN** unit 文件不存在时执行 `xkeeper service uninstall`
- **THEN** 命令成功退出并提示服务未安装

### Requirement: Windows 平台明确不支持

在 Windows 上执行 `service install` 或 `service uninstall` MUST 以非零退出码失败，错误信息说明暂不支持 Windows 服务注册。

#### Scenario: Windows 上执行安装

- **WHEN** 在 Windows 上执行 `xkeeper service install`
- **THEN** 命令失败并提示暂不支持，无任何文件写入

### Requirement: 自定义 unit 名与运行用户

`service install` SHALL 支持 `--name <unit>`（默认 `xkeeper`，仅限合法 systemd unit 名字符）与 `--user <name>`（unit 中 `User=` 字段，影响子进程运行用户），卸载按同名 unit 反向清理。

#### Scenario: 自定义 unit 名安装与卸载

- **WHEN** 用户执行 `xkeeper service install --name xk-prod` 随后 `xkeeper service uninstall --name xk-prod`
- **THEN** 安装生成 `/etc/systemd/system/xk-prod.service`，卸载删除该文件且不影响其他 unit

#### Scenario: 非法 unit 名

- **WHEN** `--name` 包含非法字符（如 `/` 或空白）
- **THEN** 安装在写任何文件前失败并提示非法名称
