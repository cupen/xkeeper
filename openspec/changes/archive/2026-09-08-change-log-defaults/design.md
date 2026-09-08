## Context

现状（已验证）：

- `src/config.rs`：`DaemonConfig::default().log_dir = "logs"`，经 `resolve_path` 与 core 配置目录拼接；`log_max_size` 兜底链末端为 `None`（pump 的 `max_size: Option<u64>` 为 None 即完全不轮转）；`log_rotate_keep` 兜底 `unwrap_or(5)`。
- `src/pump.rs`：`rotate(path, keep)` 的 keep 语义为保留 `.1`..`.<keep>` 共 keep 个**轮转文件**，不含当前文件；`keep == 0` 表示轮转即删除。
- `resolve_path` 对绝对路径原样返回——绝对路径默认值无需改任何解析逻辑。
- 四层字段优先级（program > `[app]` > `[app-default]` > 内置默认）与 `parse_size`（"50MB" 等人类可读写法）均已存在。

动机见 proposal.md（Why），行为契约见各 spec delta。

## Goals / Non-Goals

**Goals:**

- 三处内置默认值调整：`log_dir` → `/tmp/xkeeper/logs`；`log_max_size` 兜底 → `50MB`；`log_rotate_keep` 兜底 → `2`。
- 为默认开启的轮转提供显式逃生门：`log_max_size = "0"` 禁用轮转。
- README 示例与默认值说明同步。

**Non-Goals:**

- 不改轮转机制本身（`rotate()`、写入路径轮转、命名规则均不动）。
- 不做旧日志迁移、不清理旧默认目录下遗留的日志文件。
- 不新增配置字段、不改 `/v1`、`/api`、WS 帧结构。

## Decisions

**D1 兜底在 config.rs 解析末端应用，不注入隐式 `[app-default]`。**
`log_max_size` 四层取值完成后做一次映射：未在任何层声明 → `Some(50MB)`；声明 `"0"` → `None`（禁用）；其余 → 解析值。`log_rotate_keep` 兜底由 `unwrap_or(5)` 改为 `unwrap_or(2)`。
备选：把默认值写成 core 空启动时隐式注入的 `[app-default]`——会让"用户显式设置"与"内置默认"在 reload/状态投影中不可区分，且 core 缺省空启动路径也要携带，复杂度更高，放弃。

**D2 禁用语义复用 `max_size = 0 → None`，不新增布尔字段。**
pump 已以 `Option<u64>` 表达"不轮转"，`parse_size("0") = 0` 天然可解析；只需在 D1 的映射中把 `Some(0)` 归一为 `None`。备选：新增 `log_rotate = false` 布尔字段——表达力与 `log_max_size = "0"` 重叠，多一个字段多一处优先级文档，放弃。负数不可达（`parse_size` 无符号），无需新增校验规则。

**D3 默认目录按平台取系统临时目录，且强制绝对路径。**
Unix 固定 `/tmp/xkeeper/logs`（与 spec 字面一致）；Windows 取 `std::env::temp_dir()/xkeeper/logs`（`/tmp` 在 Windows 不存在）。实现为 `DaemonConfig::default()` 内一个平台分支，`resolve_path` 零改动。
备选：统一 `env::temp_dir()`——Unix 上会跟随 `TMPDIR`，实际落点偏离 spec 字面且更难预测，放弃。

**D4 "限制 3 个文件"按盘上文件总数解读，`rotate_keep = 2`。**
当前 + `.1` + `.2` 共 3 个文件，单流上限约 150MB（单程序 out/err 两流独立，最多 6 个文件）。备选：按 logrotate 惯例解读为"保留 3 份轮转备份"（盘上 4 个文件，`rotate_keep = 3`）——若用户本意如此，改一个常量即可。

**D5 `log_dir` 强制绝对路径，校验失败即拒绝启动。**
在 `DaemonConfig` 既有校验（空值拒绝）处追加 `is_absolute()` 检查，错误信息指明字段并给出修复示例（如 `log_dir = "/var/log/xkeeper"`）。缺省值本身为绝对路径，空启动路径不受影响；supervisor 侧 `resolve_path` 调用保留，绝对路径下为直通，行为可预测。
理由：相对路径的解析基准（core 配置目录）是隐式知识，落点漂移正是本次要消除的问题；与其保留一条隐式规则，不如强制显式。备选：保留相对路径并告警——兼容性好但落点仍不可预测，且"警告后继续"容易被忽略，放弃。
另需核查实施面：`service install` 等生成/改写 core 配置的代码路径若写入相对 `log_dir`，必须同步改为绝对路径，否则自产配置过不了自家校验。

## Risks / Trade-offs

- [/tmp 易失：tmpfs 或 systemd-tmpfiles 清理导致重启后日志不留存] → 有意取舍（proposal 已记录）；README 提示需要跨重启留存的部署显式配置绝对 `log_dir`（如 `/var/log/xkeeper`）。
- [升级观感"日志丢了"：未显式配置 `log_dir` 的部署升级后写新目录，旧文件留在原处] → **BREAKING** 已标注；README 迁移说明写明旧文件位置（`<core 配置目录>/logs`）且不自动迁移。
- [存量配置含相对 `log_dir`，升级后守护进程拒绝启动] → **BREAKING** 已标注；错误信息必须可行动（字段名 + 绝对路径示例）；README 迁移说明给出一行修复。拒绝启动是有意的快速失败，好过静默写到漂移位置。
- [同机多守护进程实例（不同 `-c`）默认写同一 `/tmp/xkeeper/logs`，同名程序日志互相覆盖] → 现状单实例假设（一个 systemd unit、默认单端口），低风险；README 建议多实例显式配置 `log_dir`。
- [单条超长行使文件略超 50MB 才触发轮转] → 现有写入路径轮转的既有行为，本变更不处理。

## Migration Plan

升级即生效，无部署步骤。两类存量动作：

1. 未配置 `log_dir` 的部署：无需动作，新日志写入 `/tmp/xkeeper/logs`；若要找回旧日志，去 `<core 配置目录>/logs` 取（不自动迁移）。
2. core 配置显式写了相对 `log_dir`（如 `"logs"`）的部署：启动会失败，按错误提示改为绝对路径（如 `"/var/log/xkeeper"` 或原落点的绝对形式）。

回滚 = 还原三处默认值常量与校验规则。用户侧回退方式：core 配置显式写绝对路径指向旧目录、程序或 `[app-default]` 显式写 `log_max_size`/`log_rotate_keep` 旧值，或 `log_max_size = "0"` 关闭轮转。

## Open Questions

无。
