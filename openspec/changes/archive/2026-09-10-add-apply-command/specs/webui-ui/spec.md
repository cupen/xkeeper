# webui-ui Specification Delta — add-apply-command

## ADDED Requirements

### Requirement: pending 变更提示与 apply 操作

控制台 SHALL 呈现待应用（pending）配置变更：存在 pending 时界面 SHALL 有
全局可见的提示（徽章/横幅），并在 App 概况与进程详情中标明哪些程序配置有
变化。SHALL 提供 apply 操作：全局 apply（应用全部）、App 级 apply（仅该
app，附范围说明）、`--restart` 开关（对无变更程序也重启，语义与 CLI 一致）。
apply 为变更生效动作，MUST 经过确认步骤。apply 结果 SHALL 以简洁清单/通知
呈现，逐程序标明配置变化与执行动作（重启 / 仅更新定义 / 保持停止 / 未动），
使用户无需比对配置即可分辨发生了什么。apply 完成后 pending 提示 SHALL
消失（应用失败被隔离的 app 除外）。

#### Scenario: pending 徽章出现

- **WHEN** 用户编辑了某 app 的配置使磁盘与运行状态出现差异
- **THEN** 界面出现全局 pending 提示，相关程序的行上有变更标记

#### Scenario: App 级 apply 不波及其他

- **WHEN** 两个 app 均有 pending，用户在 App A 概况执行 apply（不带 restart）
- **THEN** 仅 A 的变更被应用；B 的 pending 提示保留，B 的程序不受影响

#### Scenario: apply 结果可读

- **WHEN** 全局 apply 完成，范围内含「配置有变且在跑」「配置有变但停止」
  「无变化」三种程序
- **THEN** 结果呈现逐程序区分三类动作，pending 提示消失

#### Scenario: apply 失败有反馈

- **WHEN** apply 请求被后端拒绝或部分 app 应用失败
- **THEN** 界面展示失败原因与受影响范围，成功的部分不被回滚显示
