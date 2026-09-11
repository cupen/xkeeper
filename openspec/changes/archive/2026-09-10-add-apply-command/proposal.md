# add-apply-command

## Why

配置编辑后 `xkeeper reload` 立即生效，用户无法先看清「哪些程序会被重启」再决定
何时应用——改错一个字段就意味着正在服务的进程被静默重启。需要一个类似
systemd「reload 后需 restart」的两阶段机制：编辑只产生待应用（pending）状态，
由用户显式 `xkeeper apply` 触发生效，并清晰呈现变更面与重启面。

## What Changes

- **两阶段化配置生效**（**BREAKING**：变更 `reload` 语义）：`xkeeper reload`
  重扫注册、重读配置、校验，但只把结果记为**待应用（pending）**并输出变更
  预览，不停止/重启任何程序；守护进程周期性重扫磁盘，使注册变化与配置
  修改自动进入 pending，无需手工 reload。
- **新增 `xkeeper apply`**：应用 pending 变更。可指定范围——`apply`（全部）、
  `apply <app>`（单应用，不影响其他 app）、`apply <app> <program>`（单程序）；
  无 pending 时什么都不做并明确输出「无变更」。支持 `--restart`：无论有无
  配置变更都重启范围内的程序。
- **重启语义**：配置变更程序按 reload 现行规则重建（原先在跑则重新拉起）；
  `--restart` 对无变更程序——手动停止的（stopped/exited/fatal）保持停止，
  因崩溃退出的（backoff 等待中）重新拉起。
- **结构化 apply 结果**：`/v1/apply` 返回 JSON（每程序 changed / action /
  result 三元组）；CLI 以简洁表格呈现「哪些配置变了、哪些重启了、哪些
  未动」，shell 与 webui 复用同一结果。
- **webui 展示**：控制台显示 pending 变更徽章与 app/程序级 apply 按钮，
  结果以通知呈现。
- **在线同步通道切换**：`add`/`remove` 在线同步从「触发 reload 并立即
  生效」改为「触发重扫进入 pending，由 apply 应用」（app-registry 规范的
  同步条款同步修订）。

## Capabilities

### New Capabilities

- `apply-workflow`: pending 检出与 apply 的完整生命周期——重扫检出、范围
  选择、`--restart` 语义、手动停止保持停止、无变更幂等、结构化结果与
  清晰呈现要求。

### Modified Capabilities

- `configuration`: 「按应用粒度的热更新」需求重写为两阶段——reload 只检出
  pending 并输出预览；app 文件非法时的隔离语义移入 pending 检出阶段
  （坏 app 保持旧定义，其余照常 pending）。
- `control-plane`: 新增 `POST /v1/pending`（查询待应用变更）与 `POST
  /v1/apply`（带范围与 `--restart` 参数）端点；`POST /v1/reload` 语义改为
  「重扫 + 检出 pending + 返回预览」；CLI 控制命令集增补 `apply`。
- `shell-client`: 内置命令集增补 `apply [<app> [<program>]] [--restart]`
  与 `pending`，输出与单发 CLI 一致。
- `app-registry`: 在线同步条款改为「同步 = 进入 pending」，同步失败提示
  的补救方式从 `xkeeper reload` 改为 `xkeeper apply`。
- `webui-api`: 投影增补 pending 变更信息（WS 快照/增量携带），新增 apply
  控制端点（复用 `/v1/apply` 语义，同源）。
- `webui-ui`: 新增需求——pending 变更徽章、app/程序级 apply 操作、apply
  结果反馈。

## Impact

- **后端（src/）**：`supervisor.rs`（重扫逻辑抽出为「检出 diff」步骤，
  Command 增 `Pending`/`Apply`；守护循环周期重扫）；`config.rs`（diff 结构）；
  `server.rs`（新端点）；`client.rs`（新 API 封装）；`main.rs`（apply CLI）；
  `shell.rs`（apply/pending 命令）；`web.rs`/`api.rs`（投影与控制面）。
- **前端（webui/）**：pending 徽章、apply 按钮、结果通知，走既有 WS/
  `/api` 通道，遵循 tsc/test/build 后重嵌入的构建链。
- **兼容性**：依赖「reload 即生效」的脚本需改为 `reload && apply`（或
  直接 `apply`，它会先检出再应用）；`/v1/reload` 响应从「结果字符串」变为
  「预览结构」，属行为变更。
- **无破坏的部分**：程序定义 hash 比对、per-app 隔离失败、跨应用校验
  全部沿用现状机制，本变更只改变「何时应用」。
