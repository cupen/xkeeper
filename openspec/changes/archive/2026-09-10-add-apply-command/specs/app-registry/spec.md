# app-registry Specification Delta — add-apply-command

## MODIFIED Requirements

### Requirement: 离线可用

add / remove / list MUST NOT 依赖守护进程运行：守护进程离线时 SHALL 直接维护 `app_dir` 链接完成操作（离线时 MUST NOT 因无法同步而报错）；守护进程在线时 SHALL 在持久化后通过控制 API 触发同步（重扫检出），使注册变化进入待应用（pending）状态，由 `xkeeper apply` 落地生效。在线同步失败 SHALL 视为本次命令失败：错误信息 MUST 说明注册已持久化但守护进程未同步及补救方式（`xkeeper reload` 重扫后 `xkeeper apply`），并以非零码退出，MUST NOT 仅以警告带过。

#### Scenario: 离线批量注册后启动

- **WHEN** 守护进程停止状态下依次 add 两个应用，随后启动守护进程
- **THEN** 两个应用按 priority 与依赖关系全部自动拉起

#### Scenario: 在线同步失败不静默

- **WHEN** 守护进程在线但其重扫失败时执行 `xkeeper add`
- **THEN** 注册已持久化到 `app_dir`，命令以非零码退出，错误信息说明守护进程未同步并提示重跑 `xkeeper reload` 与 `xkeeper apply`

#### Scenario: 在线注册进入 pending 而非立即生效

- **WHEN** 守护进程在线时执行 `xkeeper add`，随后立即执行 `xkeeper status`
- **THEN** 注册同步成功（命令成功退出），新 app 的程序尚未运行，处于 pending；执行 `xkeeper apply` 后按 autostart 语义拉起
