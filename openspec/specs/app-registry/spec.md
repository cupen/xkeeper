# app-registry Specification

## Purpose

定义应用注册生命周期：`xkeeper add / remove / list` 命令（支持从部署目录直接注册、微调 flag 写入 app 配置 `[app]` 表）、以 `app_dir` 链接为注册记录的持久化、注册时校验、离线可用与在线同步。

## Requirements

### Requirement: 注册应用（add）

`xkeeper add [path] [flags]` SHALL 注册一个应用：`path` 可为部署目录（取其中默认名 `xkeeper.toml`，如 `xkeeper add .`）或直接指向配置文件。注册 SHALL 依次完成：校验（文件存在、格式合法、程序名全局唯一、依赖不成环）；在 `app_dir` 创建以应用名命名的 `<name>.toml` 链接指向配置本体（链接即注册记录）；将微调 flag 幂等写入该 app 配置的 `[app]` 表（`--autostart/--no-autostart`、`--autorestart`、`--restart-backoff`、`--priority` 等，作为应用级默认；flag 写入失败 SHALL 回滚整个注册）。应用名 SHALL 取 `--name`，缺省取部署目录名。注册 SHALL 幂等（upsert）：同名重复 add SHALL 更新链接与 flag，不产生重复。SHALL 至少支持：`--name`、`--description`、`--autostart/--no-autostart`（开机自启）、`--autorestart always|on-failure|never`（崩溃自动拉起策略）、`--restart-backoff <秒>`（重启间隔）、`--priority <n>`。

#### Scenario: 从部署目录注册并指定名字

- **WHEN** 在应用部署目录内执行 `xkeeper add . --name gateway --autorestart on-failure`
- **THEN** `app_dir` 出现以 gateway 命名的链接，部署目录 app 配置的 `[app]` 表写入 `autorestart = "on-failure"`，命令成功

#### Scenario: 未指定 --name 时取目录名

- **WHEN** 在 `/opt/myapp` 目录内执行 `xkeeper add .`（未给 `--name`）
- **THEN** 应用名为 `myapp`

#### Scenario: 守护进程运行中注册即时生效

- **WHEN** 守护进程运行中执行 add
- **THEN** 注册持久化，且该应用立即按配置与默认值拉起（等效于触发一次 reload）

#### Scenario: 注册被校验拒绝

- **WHEN** add 指向不存在的文件或校验失败的配置
- **THEN** 注册失败并指明原因，`app_dir` 与 app 配置均保持不变

#### Scenario: 重复注册更新而非重复条目

- **WHEN** 对同名应用再次 add 并修改 flag
- **THEN** 链接与 `[app]` 表字段被更新，不产生重复注册

### Requirement: 移除与查看

`xkeeper remove <app>` SHALL 删除 `app_dir` 中该应用的链接（即注销）；守护进程运行中时其程序 SHALL 被停止并清理；部署目录中的配置本体 MUST NOT 被删除。`xkeeper list` SHALL 扫描 `app_dir` 输出全部已注册应用（名称、配置本体路径、生效的默认值、程序概要）。

#### Scenario: 移除运行中的应用

- **WHEN** 守护进程运行中执行 `xkeeper remove web`
- **THEN** web 的程序被停止并从状态中消失，`app_dir/web.toml` 链接被清理，部署目录中的 `xkeeper.toml` 仍存在

#### Scenario: 列出注册表

- **WHEN** 执行 `xkeeper list`
- **THEN** 输出每个已注册应用的名称、配置本体路径与生效的默认值

### Requirement: 离线可用

add / remove / list MUST NOT 依赖守护进程运行：守护进程离线时 SHALL 直接维护 `app_dir` 链接完成操作（离线时 MUST NOT 因无法同步而报错）；守护进程在线时 SHALL 在持久化后通过控制 API 触发同步，使命令执行结果与守护进程实际状态一致。在线同步失败 SHALL 视为本次命令失败：错误信息 MUST 说明注册已持久化但守护进程未同步及补救方式（`xkeeper reload`），并以非零码退出，MUST NOT 仅以警告带过。

#### Scenario: 离线批量注册后启动

- **WHEN** 守护进程停止状态下依次 add 两个应用，随后启动守护进程
- **THEN** 两个应用按 priority 与依赖关系全部自动拉起

#### Scenario: 在线同步失败不静默

- **WHEN** 守护进程在线但其 reload 失败时执行 `xkeeper add`
- **THEN** 注册已持久化到 `app_dir`，命令以非零码退出，错误信息说明守护进程未同步并提示重跑 `xkeeper reload`
