## MODIFIED Requirements

### Requirement: apply 范围与幂等

`xkeeper apply` SHALL 应用 pending 变更，范围 SHALL 可指定：缺省为全部；字面量 `all` SHALL 为全量关键字，`apply all` 与缺省全量严格等价（`all` 为保留字，MUST NOT 被解析为 app 名，帮助与文档 MUST 注明 `all` 不可用作 app 名）；`apply all` 后 MUST NOT 再接受 program 参数——`apply all <program>` 形状 SHALL 报错，MUST NOT 静默忽略 program 部分而按全量执行；`apply <app>` 仅作用于该 app 的程序与该 app 的注册变化，MUST NOT 影响其他 app（其他 app 的 pending 保持不变）；`apply <app> <program>` 进一步收窄到单程序。范围内无 pending 时 SHALL 什么都不做，输出明确的「无变更」结论并以 0 退出（幂等）。新注册 app 的程序进入 apply 范围时 SHALL 直接启动（沿用 autostart 语义）。

#### Scenario: 无变更时 apply 幂等

- **WHEN** 配置无任何 pending 时执行 `xkeeper apply`
- **THEN** 无程序被停止/重启/启动，输出「无变更」类提示，退出码 0

#### Scenario: apply all 等价全量

- **WHEN** app A 与 app B 均有 pending 变更，执行 `xkeeper apply all`
- **THEN** A 与 B 的全部 pending 被应用，结果与裸 `xkeeper apply` 一致

#### Scenario: 单 app 范围不波及其他

- **WHEN** app A 与 app B 均有 pending 变更，执行 `xkeeper apply A`
- **THEN** 仅 A 的变更被应用；B 的 pending 保留，B 的进程不受影响

#### Scenario: 新注册 app 随 apply 启动

- **WHEN** `xkeeper add` 注册了 autostart 的新 app 后执行 `xkeeper apply`
- **THEN** 该 app 的程序被启动，出现在 apply 结果中

#### Scenario: apply 后 pending 清空

- **WHEN** 全量 `xkeeper apply` 成功后立即查询 pending
- **THEN** pending 为空（应用失败被隔离的 app 除外，其 pending 保留）
