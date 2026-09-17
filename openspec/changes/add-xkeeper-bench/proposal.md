## Why

xkeeper 目前缺少面向用户的性能基准测量手段：`xtask/src/stress.rs` 只覆盖 firehose 零反压这一条验证路径，且是开发者内部工具（不随发布产物分发、单一场景、输出面向 CI 断言而非人）。容量评估（这台机器能挂多少程序、日志吞吐上限多少）与版本间性能回归对比，都需要一个可按场景驱动真实 daemon、产出可比较指标报告的独立工具。

## What Changes

- 新增独立二进制 `xkeeper-bench`（workspace 新成员 crate `xkeeper-bench/`），用于对真实 xkeeper daemon 做负载基准测量：按 case 拉起负载、采集指标、输出报告。
- 默认自带隔离环境：在临时 workspace 中拉起 daemon 并注册 bench 临时 app；`--connect <addr>` 可改连已运行的 daemon（支持 `--token` Bearer 鉴权）。
- v1 提供四个 case（`--case`）：`firehose`（单程序日志洪峰吞吐）、`rotation`（小阈值高频轮转 + 逐行完整性）、`fanout`（N 程序 × stdout/stderr 双流扩展性）、`drip`（低速率稳态长跑 RSS 有界性）。
- 负载参数：`--log-rows`（每程序总行数，0/缺省 = 不限）、`--log-row-size`（单行载荷字节数）、`--log-total-size`（总字节上限，先到为准）、`--rate`（行/秒，0 = 全速）、`--duration`（墙钟上限）、`--programs`（fanout 程序数）。
- 日志生成器 = bench 二进制自我重入（隐藏 `__generate` 子进程模式），作为 daemon 的普通子进程精确控速产出真实日志。
- 报告：默认人类可读表格 + `--json <path>` 导出机器可读结果（rows/s、bytes/s、峰值/均值 RSS、轮转次数、wall time、行完整性校验）；退出码仅反映运行成败，不内置阈值门禁。
- 资源清理：bench 结束（无论成败）自动卸载临时 app 并删除其日志文件，无残留；`--keep` 逃生门供调试。
- xtask e2e 增加一段短 firehose 冒烟，验证 bench 接线与 JSON 输出；README / AGENTS.md 增补 bench 用法。
- daemon 主二进制与 `/v1`、`/api`、WS 语义零改动：bench 全部从既有控制面状态投影与磁盘日志文件外部取数。

## Capabilities

### New Capabilities

- `benchmark`: xkeeper-bench 负载基准测量能力——case 语义（firehose/rotation/fanout/drip）、运行拓扑（自带隔离环境 / connect）、负载参数、生成器行为、指标报告与清理契约。

### Modified Capabilities

（无——daemon 侧行为与既有规范不变，bench 是 /v1 控制面与日志管线的纯外部消费者。）

## Impact

- 新增 `xkeeper-bench/` workspace 成员 crate（独立 `Cargo.toml`，自带依赖；不触碰根包 `xkeeper` 的任何源码）。
- `Cargo.toml` workspace members 增加 `xkeeper-bench`。
- `xtask/src/e2e.rs` 增加 bench 冒烟段。
- `README.md` / `AGENTS.md` 增补 bench 段落。
- 依赖面：bench crate 需 HTTP 客户端（复用 `ureq`）、`serde_json`、`clap`；无新外部服务依赖。
- 不影响：supervisor/pump/轮转/健康探测、控制面路由、webui、构建产物形态（daemon 二进制体积与依赖不变）。
