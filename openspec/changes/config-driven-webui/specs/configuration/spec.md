## ADDED Requirements

### Requirement: webui 配置段

daemon 配置的 `[webui]` 段 SHALL 由 config 子命令按统一键表机制管理：`config --set webui.listen=<addr>` 写入监听地址（写入即开启控制台，reload 后生效或热生效）、`config --get webui.listen` 打印生效值（段未配置时 SHALL 回退内置默认 `127.0.0.1:9877`）、`config --delete webui` 删除整段（即关闭控制台）。键表机制、类型校验、读改写与原子落盘 SHALL 与 `[daemon]` 键一致（见 config 子命令相关 requirement）。`webui.listen` 值 SHALL 为非空的 `host:port` 形式：无法解析或为空 SHALL 被校验拒绝（写入期与加载期一致拒绝）；`[webui]` 段内未知键 SHALL 被校验拒绝。`[webui]` 段不新增鉴权键：控制台访问控制沿用既有机制，不在本段范围。

#### Scenario: config 开启控制台

- **WHEN** 执行 `xkeeper config --set webui.listen=127.0.0.1:9877`（daemon 离线）
- **THEN** daemon 配置文件新增 `[webui]` 段且整体校验通过，输出说明离线时下次启动生效

#### Scenario: config 关闭控制台

- **WHEN** 配置含 `[webui]` 段，执行 `xkeeper config --delete webui`
- **THEN** 该段整体从文件移除（其余内容含注释保持原样），守护在线时输出需 reload 提示

#### Scenario: 非法 listen 拒绝写入

- **WHEN** 执行 `xkeeper config --set webui.listen=9877`（缺 host）或 `xkeeper config --set webui.listen=`（空值）
- **THEN** 非零码退出并说明值必须是 `host:port`，配置文件内容不变

#### Scenario: 段内未知键校验拒绝

- **WHEN** daemon 配置的 `[webui]` 段内出现未知键（如 `auth = true`），执行 `xkeeper validate` 或 `xkeeper run`
- **THEN** 校验失败并指明该未知字段，守护进程拒绝以该配置启动

## MODIFIED Requirements

### Requirement: daemon 根配置

daemon 配置全局唯一，SHALL 只包含全局内容：`[daemon]`（监听地址/端口、鉴权 token、日志、巡检间隔、`app_dir` 应用注册目录）、可选的 `[app-default]`（全部应用共享的默认值表，字段名与程序字段一致）与可选的 `[webui]`（内嵌控制台段：`listen` 监听地址；段存在即启用控制台，`listen` 缺省 `127.0.0.1:9877`）。daemon 配置 MUST NOT 承载任何应用专属内容：应用条目（如 `[[app]]`）、应用名、配置路径等 SHALL 被校验直接拒绝。`app_dir` 缺省为 daemon 配置文件同级的 `apps` 目录。`daemon.log_dir` 缺省为 `/tmp/xkeeper/logs`（绝对路径，不随 daemon 配置文件位置漂移）；配置的 `log_dir` MUST 为绝对路径，相对路径 SHALL 被校验拒绝（守护进程拒绝启动）。daemon 配置默认路径 SHALL 为 Linux `/etc/xkeeper/daemon.toml`、Windows `%APPDATA%\xkeeper\daemon.toml`，并 SHALL 可用 `-c/--config` 覆盖。daemon 配置文件不存在时守护进程 SHALL 以内置默认值空启动，首次写入时创建。

#### Scenario: 全新机器空启动

- **WHEN** 在没有任何 daemon 配置文件的机器上执行 `xkeeper run`
- **THEN** 守护进程以默认设置启动，受管程序为空，控制平面正常可用，控制台不伺服

#### Scenario: daemon 配置中的应用条目被拒绝

- **WHEN** daemon 配置中出现 `[[app]]` 等应用专属条目
- **THEN** 校验失败并报未知字段，守护进程拒绝以该配置启动

#### Scenario: 缺省日志目录固定

- **WHEN** daemon 配置未声明 `log_dir`，守护进程拉起任一程序并产生输出
- **THEN** 程序日志写入 `/tmp/xkeeper/logs`，与 daemon 配置文件所在目录无关

### Requirement: 校验规则

配置校验 SHALL 拒绝未知字段（拼写错误报错而非忽略）、空的应用（无任何 `[program.*]`）、使用不安全字符（路径分隔符、控制字符等）的应用名与程序名、超出合法范围的数值字段；`command` 非空且单行拆分与显式 `args` 不得同时出现；`health_check` 字符串 SHALL 在加载期按协议解析并校验。`daemon.log_dir` SHALL 为绝对路径：相对路径或空值 SHALL 被校验拒绝，守护进程 MUST 拒绝启动并在错误信息中指明字段与修复方式。`webui.listen` SHALL 为非空且可解析的 `host:port`（端口 1–65535），非法值 SHALL 被校验拒绝。程序名 SHALL 在全部已注册应用之间全局唯一：注册与 reload 时检测到跨应用重名 SHALL 拒绝并指明冲突双方。`depends_on` 引用 SHALL 允许跨应用，成环在注册/校验期被拒绝。

#### Scenario: 跨应用程序名冲突被拒绝

- **WHEN** 注册一个其程序名与已注册程序重名的 app 配置
- **THEN** 注册失败，错误指明重名程序与冲突的双方应用

#### Scenario: 单行 command 与 args 冲突

- **WHEN** 程序同时写 `command = "python -m server"` 与 `args = ["-m", "server"]`
- **THEN** 校验失败，提示二者互斥

#### Scenario: 相对 log_dir 拒绝启动

- **WHEN** daemon 配置声明 `log_dir = "logs"`（相对路径）并执行 `xkeeper run` 或 `xkeeper validate`
- **THEN** 校验失败，错误信息指明 `daemon.log_dir` 必须为绝对路径，守护进程不启动

### Requirement: 按应用粒度的热更新

reload SHALL 重扫 `app_dir` 注册并重读全部 app 配置本体与 daemon 配置，但
仅将差异记为**待应用（pending）变更**并输出预览，MUST NOT 停止、重启或拉起
任何程序（应用由 apply 触发，见 apply-workflow 能力）。**唯一例外**：
`[webui]` 段的启用/关闭/`listen` 变更 SHALL 由 reload 立即热应用（见 webui-api
「配置驱动的控制台生命周期」），MUST NOT 进入 pending。守护进程 SHALL 周期性
自动执行该重扫，`xkeeper reload` 触发一次立即重扫。单个 app 文件非法时
SHALL 隔离失败——该 app 不进入 pending、保持旧定义继续运行并上报错误，其余
app 照常检出。跨应用校验失败时本轮 SHALL 整体不形成 pending。daemon 配置中
守护进程自身字段变更（`[webui]` 段之外）SHALL 尽量热应用（如日志级别），无法
热更的字段（如监听端口）SHALL 在 pending 预览中明确提示需要重启守护进程。

#### Scenario: 单个 app 文件损坏不影响其他

- **WHEN** 重扫时 app B 的配置存在语法错误，app A 的配置正常且已修改
- **THEN** A 的变更进入 pending，B 保持旧定义继续运行，错误信息被上报

#### Scenario: reload 只检出不应用

- **WHEN** app A 的程序 `web` 配置已修改且正在运行，执行 `xkeeper reload`
- **THEN** 输出包含 `web` 的 pending 预览，进程未被停止或重启

#### Scenario: 不可热更字段提示

- **WHEN** daemon 配置的 `port` 被修改，执行 `xkeeper reload`
- **THEN** pending 预览提示该字段需重启守护进程方可生效，控制面仍监听旧端口

#### Scenario: 移除注册立即生效

- **WHEN** 守护进程运行中执行 `xkeeper remove <app>`
- **THEN** `app_dir` 链接即时清理，该 app 的移除进入 pending，其程序在
  apply 前继续运行
