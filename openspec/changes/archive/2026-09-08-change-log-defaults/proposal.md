## Why

xkeeper 当前默认**不轮转**子进程日志（`log_max_size` 内置兜底为空），长期运行的受管程序会把磁盘写满；且 `daemon.log_dir` 默认值 `logs` 是相对路径，落点随 core 配置文件位置漂移，不可预测。需要一个开箱即用的有界日志默认：固定目录、每文件 50MB、每个流最多 3 个文件——全部仍可在配置文件按四层优先级覆盖（配置通道已存在，本变更是纯默认值语义调整）。

## What Changes

- `daemon.log_dir` 内置默认值 `"logs"`（相对 core 配置目录解析）→ `/tmp/xkeeper/logs`（绝对路径）。**BREAKING**：未显式配置 `log_dir` 的部署升级后，新日志写入新目录，旧文件不迁移。
- **BREAKING**：`daemon.log_dir` SHALL 强制为绝对路径——core 配置声明相对路径时校验失败、守护进程拒绝启动（错误信息指明字段与修复方式）。此前相对路径合法（相对 core 配置目录解析），存量含相对 `log_dir` 的配置升级后必须手工改为绝对路径。
- `log_max_size` 内置兜底 空（不轮转）→ `50MB`（默认开启按大小轮转）。
- `log_rotate_keep` 内置兜底 `5` → `2`，即每个流盘上最多 3 个文件（当前 + `.1` + `.2`）。假设记录：用户口中的"限制 3 个文件"按字面理解为盘上文件总数上限（含当前文件）；若本意是"保留 3 个轮转备份"（盘上 4 个文件），只需把兜底改为 3。
- 新增显式禁用轮转写法：程序字段 `log_max_size = "0"` SHALL 禁用该程序日志轮转（默认开启轮转后的逃生门；`0` 此前为病态语义——`size >= 0` 恒真导致逐行轮转）。
- 主仓 README 的日志配置示例与默认值说明同步更新（含相对路径迁移说明）。
- 已知取舍（记录备查）：`/tmp` 在多数发行版是 tmpfs 或会被 systemd-tmpfiles 清理，重启后日志不保证留存——默认优先"绝不写满磁盘"，需要跨重启留存日志的部署应显式配置绝对 `log_dir`（如 `/var/log/xkeeper`）。

## Capabilities

### New Capabilities

### Modified Capabilities

- `configuration`: 内置默认值变化——`daemon.log_dir` 默认 `/tmp/xkeeper/logs`；程序字段 `log_max_size` 兜底 `50MB`、`log_rotate_keep` 兜底 `2`；新增 `log_max_size = "0"` 禁用轮转的语义与校验规则；新增 `daemon.log_dir` 强制绝对路径的校验规则（相对路径拒绝启动）。
- `log-management`: "按大小轮转"需求从"可选"改为"默认启用、可显式禁用"，并写明默认阈值（50MB）、默认保留份数（2，每流盘上最多 3 个文件）与 out/err 各自独立轮转的边界。

## Impact

- `src/config.rs`：`DaemonConfig::default()` 的 `log_dir`（平台分支，绝对路径）；`DaemonConfig` 校验新增绝对路径规则；program 字段兜底链（`log_max_size`/`log_rotate_keep` 的 `unwrap_or`）；`parse_size` 结果为 0 时的禁用语义与校验。
- `src/pump.rs` 轮转机制不变（`max_size: Option<u64>` 已支持 None = 不轮转）；supervisor 的 `resolve_path` 调用保留（绝对路径下为直通）。
- `README.md` 日志配置示例与默认值表、相对路径迁移说明。
- 无 `/v1`、`/api`、WS 帧结构变化；用户可见影响为升级后日志位置变化、默认开始轮转、含相对 `log_dir` 的存量配置拒绝启动。
