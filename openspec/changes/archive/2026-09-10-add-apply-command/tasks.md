# Tasks — add-apply-command

## 1. 后端检出/应用拆分（supervisor 核心）

- [x] 1.1 重构 `cmd_reload`：抽出 `detect()`（重扫 + per-app 隔离 +
      `validate_all` + 产出 `PendingDoc`，零进程动作）与
      `apply_pending(scope, restart)`（scope 过滤 + 程序动作 + 注册变化
      + daemon 字段合并）；`Command` 增 `Apply { scope, restart, reply }`；
      `Reload` 改为 detect + 返回 pending 预览。单测：修改配置后 reload
      返回预览且程序状态/pid 不变；坏 app 隔离；跨应用校验失败整体放弃
- [x] 1.2 守护循环 tick 接周期 detect：mtime+size 快照短路（全部未变
      跳过解析），结果写 `SupervisorState.pending`。单测：直接改磁盘配置
      不执行任何命令，一个 tick 后 pending 出现；未变更时 pending 为空
      且无重复解析（用计数器断言短路生效）
- [x] 1.3 apply 动作判定表落地（design D3 全表）：单测逐行覆盖——
      changed×{running,backoff,stopped,exited,fatal}、unchanged×同五态、
      `--restart` 列、新增程序 autostart on/off、已移除程序的 stop+drop；
      断言手动停止（stopped/exited/fatal）永不因 apply 被拉起
- [x] 1.4 `ApplyResult`/`ProgramAction`/`PendingDoc` 结构（serde 派生）
      + CLI 三段式人读输出渲染函数；单测覆盖渲染分组与「无变更」路径

## 2. 控制面与 CLI

- [x] 2.1 `server.rs`：`GET /v1/pending`、`POST /v1/apply`（body：
      `{"app": ..., "program": ..., "restart": bool}`，范围校验——未知
      app/program 返回 404）；`/v1/reload` 响应改为 pending 预览结构。
      端到端测试：修改配置 → `/v1/pending` 见 changed → `/v1/apply` →
      程序重启 → pending 清空
- [x] 2.2 `client.rs` + `main.rs`：`xkeeper apply [<app> [<program>]]
      [--restart]`（无变更输出「无变更」退出 0；守护不可达退出 3）；
      reload 输出改为 pending 预览摘要。集成测试：两 app 各有变更，
      `apply A` 后 B 的 pending 保留且 B 程序 uptime 不重置
- [x] 2.3 `sync_if_online` 切换：add/remove 后提示「已进入待应用，执行
      `xkeeper apply` 生效」；`xkeeper add` 在线路径集成测试：注册
      pending → apply → autostart 程序拉起
- [x] 2.4 `shell.rs`：`pending` 与 `apply [<app> [<program>]] [--restart]`
      内置命令（解析 + help 文案 + 单测，含 sttaus 式纠错不回归）

## 3. webui（投影 + 控制台）

- [x] 3.1 `StatusDoc` 增 `pending` 字段（/v1、/api、WS 快照三处同源）；
      pending 变化触发 WS 增量事件（detect 更新与 apply 清空两路径）。
      单测：快照含 pending、增量事件载荷正确
- [x] 3.2 `/api/pending`、`/api/apply` 端点（与 /v1 同源）；契约测试：
      同一时刻两处 pending 一致、apply 参数与结果同构
- [x] 3.3 前端：pending 徽章（全局提示 + 程序行变更标记）、app 级与全局
      apply 按钮（含确认）、`--restart` 开关、结果通知逐程序呈现三类
      动作。vitest：徽章出现/消失、apply 后结果分组、单 app 范围不波及
      另一 app、失败反馈
- [x] 3.4 `pnpm exec tsc --noEmit && pnpm test && pnpm build` 全绿后
      `cargo build --release` 重嵌入，浏览器手测：改配置 → 徽章出现 →
      apply → 结果通知 → 徽章消失

## 4. 收尾

- [x] 4.1 全量验证：`cargo test`、`cargo run -- validate`、`openspec
      validate add-apply-command --strict`；README 快速上手增补「编辑 →
      apply」与 reload 语义变更迁移说明（`reload && apply` 兼容路径）
- [x] 4.2 对照 specs 逐条 scenario 核对（apply-workflow /
      configuration / control-plane / shell-client / app-registry /
      webui-api / webui-ui 七个 delta，重点：手动停止保持停止、范围
      隔离、无变更幂等、reload 不再触碰进程），结果记入变更目录
- [x] 4.3 验收 e2e 固化（测试验收补充）：`cargo run -p xtask -- e2e`
      —— CLI 段（幂等/检出/范围隔离/--restart 保停/add 进 pending/shell）
      + playwright 浏览器段（徽章出现与清空、app 级 apply 确认与结果通知）；
      webui 组件级断言补 `apply-ui.test.ts`；e2e 暴露并修复 WS 快照
      compact/named 编码缺陷（详见 verification.md）
