# Design — add-apply-command

## Context

当前 `cmd_reload`（src/supervisor.rs）在一次命令里完成「重扫注册 + 重读配置
+ 比对 hash + 停止/重建/拉起」全链路；比对基础设施（`ResolvedProgram.hash`、
per-app 隔离失败、跨应用 `validate_all`）都已存在。本设计把这条链路切为
两段：**检出（detect）** 与 **应用（apply）**，中间隔一个显式的用户动作。

守护循环（`Supervisor::run`）是命令队列的唯一消费者，每个
`monitor_interval` tick 一次；`sync_if_online`（src/main.rs）在 add/remove
后调用 `/v1/reload` 完成在线同步。

## Goals / Non-Goals

**Goals**
- 检出与应用分离，且检出由守护循环周期自动驱动（用户不需要记得 reload）。
- apply 的范围选择（全局 / app / 程序）与 `--restart` 语义精确、可预测。
- apply 结果结构化且三处消费面（CLI / shell / webui）同源。
- 手动停止的程序在 apply（含 `--restart`）下保持停止。

**Non-Goals**
- 不做配置回滚 / 签出旧版本（reload 前快照）。
- 不做单字段 diff 展示（哪个字段变了）；只报告程序级 changed。
- 不改程序状态机本身（7 态不变）。
- daemon 不可热更字段（host/port/auth）仍只是提示，不自动重启守护进程。

## Decisions

### D1. pending 的载体：磁盘即真相，内存只存检出结果

守护进程**不持久化** pending 状态。pending = 「磁盘配置 vs 运行中定义」
的差异，随时可由重扫重新计算。`SupervisorState` 增加一个
`pending: PendingDoc` 字段（检出结果的结构化投影：逐程序 changed 清单、
app 级 added/removed、daemon 不可热更字段提示、检出错误），由重扫步骤写，
由 `/v1/pending`、`/v1/reload` 响应与 WS 快照/增量读。

- 备选：把新配置本体存进内存等 apply 时取用——被否。存两份配置（内存
  resolved + 运行 resolved）会引入一致性问题（apply 时磁盘又变了怎么办）；
  「重扫时算 diff、apply 时再重扫再算」天然以 apply 那一刻的磁盘为准，
  语义即「应用磁盘上的当前状态」。
- 代价：apply 时会重扫一次（检出与 apply 各扫一遍），IO 成本可忽略
  （重扫 = 读 N 个小 TOML 文件）。

```
守护循环 tick（每 monitor_interval）
  ├─ pop_command()        ← reload/apply 命令优先
  ├─ rescan_and_detect()  ← 更新 state.pending（纯内存，零进程动作）
  ├─ programs.tick()
  └─ start_eligible()

apply 命令（Command::Apply { scope, restart, reply }）
  └─ rescan_and_detect()（拿到 apply 时刻的最新磁盘状态）
      → 按 scope 过滤
      → 逐程序执行动作（复用 reload 现有的 stop→重建→按需拉起路径）
      → 汇总 ApplyResult → reply
```

### D2. 检出步骤复用 `cmd_reload` 的前半段，抽出共享函数

把 `cmd_reload` 重构为三块，供 reload（检出）与 apply（检出+应用）复用：

1. `fn detect(&self) -> DetectOutcome` —— 重扫注册、逐 app 重读、
   隔离失败、`validate_all`、产出 pending diff 与错误清单。语义与现状
   `cmd_reload` 的前半完全一致（包括跨应用校验失败整体放弃、坏 app 保持
   旧定义），只是**不落地任何程序变更**。
2. `fn apply_pending(&self, scope: &ApplyScope, restart: bool) -> ApplyResult`
   —— 消费 detect 结果：scope 过滤 → 程序级动作（见 D4）→ app 注册变化
   动作（added：insert+按 autostart/start 状态拉起；removed：stop+drop）
   → 不可热更 daemon 字段仅入结果提示 → 更新 `st.config` / `st.apps`。
3. reload 命令退化为 `detect()` + 输出 pending 预览。

`Command` 枚举增 `Apply { scope, restart, reply }`；`Reload` 保留（触发一次
立即 detect 而非等 tick）。**命令队列仍是唯一写者**，重扫 detect 也在守护
循环线程内执行——检出与 apply 都不改这条铁律，只是重扫从「命令驱动」
变为「tick + 命令双驱动」。

### D3. apply 的动作判定表（核心语义）

对范围内每个程序，按「磁盘 vs 运行」与「--restart」查表：

| 磁盘配置 | 当前状态 | 默认（无 --restart） | --restart |
|---|---|---|---|
| changed | running/starting | 重建 + 重新拉起（update-and-restart） | 同左 |
| changed | backoff | 重建 + 拉起 | 同左 |
| changed | stopped（手动停） | 重建，保持停止（update-only） | 同左 |
| changed | exited | 重建，保持停止 | 同左 |
| changed | fatal | 重建，清 fatal 保持停止 | 同左 |
| unchanged | running/starting | 不动（none） | 重启（restart） |
| unchanged | backoff | 不动（等退避） | 立即拉起（restart） |
| unchanged | stopped/exited/fatal | 不动（none） | 保持停止（keep-stopped） |
| 新增 | — | 按 autostart；autostart=false 则停着（start/skip） | 同左 |
| 已移除 | 任意 | stop + 移除（remove） | 同左 |

「手动停止保持停止」按**状态**判定而非记录停止原因：现状 `ProgramState`
没有「谁停的」概念，exited（正常退出不再拉起）与 stopped（用户停）在
apply 语境下同等待遇——只有 running/starting/backoff 算「在跑」，其余都
保持停止。这与用户确认的语义一致且实现零侵入。

### D4. ApplyResult 结构（三处同源的单一定义）

```rust
struct ApplyResult {
    programs: Vec<ProgramAction>,   // 范围内每个程序一条
    apps_added: Vec<String>,
    apps_removed: Vec<String>,
    daemon_hints: Vec<String>,      // 不可热更字段提示
    errors: Vec<String>,            // 隔离失败的 app
    summary: String,                // 人读一句话（无变更时为 "no changes"）
}
struct ProgramAction {
    app: String,
    program: String,
    changed: bool,        // 配置是否有变化（含新增）
    action: String,      // update-and-restart | update-only | restart |
                         // start | keep-stopped | none | remove
    result: String,       // "ok" 或错误说明
}
```

serde 派生后同一结构体直供 `/v1/apply` JSON、`/api/apply`、shell 表格与
webui 通知；CLI 打印为分组三段式（变更已应用 / 重启 / 未动），实现
「一眼看出哪些重启了、哪些变了、哪些没动」。

### D5. 周期重扫的节奏与开销控制

重扫 detect 挂在守护循环 tick（默认 1s）。为控制开销：

- 检出有**短路**：先对每个 app 的配置文件做 `mtime + size` 预检，与上次
  检出快照比对，全部未变则跳过解析（注册目录列表仍每 tick 全扫，开销为
  一次 readdir）。
- 每次成功检出后保存 mtime/size 快照到 `SupervisorState`。
- 备选「监听文件系统事件（notify crate）」被否：新依赖 + 平台差异大，
  轮询 + mtime 预检在配置文件量级（几十个）下绰绰有余，且与既有
  monitor_interval 节奏一致。

### D6. 在线同步通道切换（add/remove）

`sync_if_online` 从调用 `c.reload()` 改为调用新的 `c.rescan()`（POST
`/v1/reload` 本身已重定义为检出，因此**通道不变、语义已换**——CLI 侧把
「同步成功」的输出从 `daemon synced: reloaded` 改为提示「注册已进入待应用，
执行 `xkeeper apply` 生效」）。app-registry 规范的「同步 = 进入 pending」
条款随之改写（见 specs/app-registry delta）。

### D7. WS 推送的 pending 扩展

`StatusDoc`（src/server.rs）增补 `pending: PendingDoc` 字段（向后兼容
增量字段，模式与 metrics 字段一致）：快照携带全量 pending；pending 变化
时（detect 更新后/apply 清空后）由守护循环置脏标记，WS 增量事件以
`pending` 载荷推送。`/v1/pending` 与 `/api/pending` 从 `state.pending`
直读——三处同源。

## Risks / Trade-offs

- [reload 语义 BREAKING：依赖「reload 即生效」的脚本] → 文档与
  README 显著标注迁移路径 `xkeeper reload && xkeeper apply`（或直接
  `xkeeper apply`）；`/v1/reload` 响应结构变化在 proposal 已标明。CI /
  集成测试同步更新。
- [tick 内重扫与命令执行同线程，慢磁盘上 detect 拖长 tick] → mtime 预检
  短路使稳态开销为一次 readdir + N 次 stat；NFS 等环境下 stat 慢的风险
  记录在案，不为本变更引入异步线程。
- [apply 与检出之间磁盘又被改] → apply 内先重扫（D1），按 apply 时刻的
  磁盘为准，永远不应用陈旧 diff。
- [程序很多时 --restart 全量重启造成服务真空] → 范围参数本就允许分批
  apply；不做滚动/串行间隔（Non-Goal），文档提示分批执行。
- [pending 投影三处一致性] → 单一 `PendingDoc` 源（D7），遵循仓库
  「/v1、/api、WS 三处同源」既有约束。

## Migration Plan

1. 后端先落地 detect/apply 拆分与端点；`reload` 行为切换为检出（BREAKING
   点在此生效）。
2. CLI/shell 跟进 `apply` / `pending` 命令；webui 徽章与 apply UI 随后。
3. 无状态回滚：revert 提交即恢复旧 reload 语义；pending 不落盘，回滚无
   残留（运行态没有新字段需要清理，`ProgramAction` 只是一次响应）。
4. 文档：README 快速上手增加「编辑 → apply」一节；conf/ 模板注释无需变。

## Open Questions

（无——范围、重启语义、输出形态均已与用户确认。）
