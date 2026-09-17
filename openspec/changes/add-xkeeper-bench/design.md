## Context

仓库现状：workspace = 根包 `xkeeper`（纯 bin，无 lib target，含 daemon/控制面/webui）+ 成员 `xtask`（e2e + firehose 零反压验证 `stress.rs`）。`metrics.rs` 已实现 per-program CPU/RSS 采样与 `RateCounter` 速率统计，且已进入共享状态投影（`/v1`、`/api`、WS 三处同源）；日志轮转（`log_max_size`/`log_rotate_keep`，缺省 50MB×2）已由 pump 在写路径实现。控制面 `/v1` 提供 add/remove/apply/stop/status 等完整注册表与进程操作（CLI 控制子命令同源消费）。约定见 AGENTS.md：改状态字段/命令语义时 `/v1`、`/api`、WS 必须同源修改——bench 的设计目标是根本不触发这条约束。

## Goals / Non-Goals

**Goals:**

- daemon 侧零改动：bench 是 `/v1` 投影与磁盘产物的纯外部消费者，不新增路由、状态字段或帧类型。
- 测量可信：隔离环境默认化、负载量界确定性强（先到为准）、指标口径在 JSON 中自描述，两次运行可直接对比。
- 跨平台生成器：自我重入模式不依赖 shell，unix/Windows 行为一致。

**Non-Goals:**

- 不做阈值门禁 / 基线库 / 多次运行统计分析（CI 自己比较 JSON）。
- 不做 TUI、实时进度条；进度走 stderr 单行刷新即可。
- 不覆盖 WS 推送路径压测（stress.rs 已有专项，且用户已定 bench 与其完全独立）。
- 不支持 Windows 上的外部 RSS 采样（指标报告 null，见决策 D5）。

## Decisions

- **D1 独立 workspace 成员 crate `xkeeper-bench/`**（自备 `Cargo.toml`，依赖 `clap`/`anyhow`/`serde`/`serde_json`/`ureq`）。备选：根包 `src/bin/xkeeper-bench.rs` —— 否，根包无 lib target，共享代码需先把 daemon crate 重构出 lib，侵入面大且 bench 代码进 daemon 构图；并入 xtask —— 否，不随发布产物分发，且 xtask 会背上 bench 的运行时依赖（tokio 等）。
- **D2 生成器 = 自我重入隐藏子命令**。bench 以 `xkeeper-bench __generate --rate .. --row-size .. --stream both ..` 形式把自己挂成 daemon 子进程（app 配置 `command` 指向 `std::env::current_exe()`），按令牌桶控速逐行写 stdout/stderr（双流按固定 10:1 分流：每 10 行取 1 行写 stderr），行内嵌 `序号 时间戳` 前缀 + 填充文本到 row-size。备选：`sh -c 'yes ... | awk'` 拼装 —— 否，速率不可控、Windows 不可用、行序号无法嵌入；独立生成器二进制 —— 否，多一个发布产物，而自我重入零成本自包含。速率实现用简单令牌桶（sleep 补偿漂移），不引入调度库。
- **D3 量界与终止**：生成器侧维护已产出行数/字节，先到 `--log-rows` / `--log-total-size` / `--duration` 任一界即优雅退出；父侧 bench 以「负载程序全部退出」为测量完成信号，再等待一小段排空窗口（pump 落盘滞后）后开始读盘对账。排空窗口时长（约 2s，轮询日志文件 mtime/大小静止）记为实现调优项。
- **D4 spawn 拓扑**：临时 workspace 布局沿用 daemon 约定（daemon.toml + app_dir + logs），`--daemon` 指定或按 `CARGO_MANIFEST_DIR/../target/{debug,release}` 顺序发现 xkeeper 二进制；以独立控制端口/监听地址拉起（绑定 127.0.0.1 随机空闲端口，避免与用户 daemon 冲突）；bench 经其 `/v1` 操作，与 connect 模式同一条客户端代码路径。测量完 shutdown daemon 后删 workspace。
- **D5 指标采集**：吞吐 = 生成器自计数（行数/字节精确）+ 墙钟；RSS = 父 bench 按 daemon PID 外部采样（unix 读 `/proc/<pid>/status` 低频轮询 1s；Windows 无轻量跨进程方案，报告 null —— metrics.rs 的采样在 daemon 进程内部，bench 不要求 daemon 暴露新字段）；轮转次数 = 盘上 `.1/.2/...` 轮转文件计数（rotation 用例）。备选：给 `/v1` 加 metrics 快照端点 —— 否，违背零改动边界。
- **D6 完整性对账**：行格式 `[k] <seq> <ts> <fill>`，`<seq>` 为每流全局单调序号；对账 = 顺序扫描当前 + 轮转文件（按轮转序倒序拼接），校验 seq 严格递增且恰为 `0..N-1`。rotation 用例通过「keep 足够大」保证无文件被删，从而全集可对账；其他用例总量不超缺省 keep。丢行/乱序 → integrity=fail → 非零退出。
- **D7 connect 模式安全性**：app 名固定前缀 `xkeeper-bench-`，注册前先 list 检查同名存在即拒跑；清理用 remove + 删 `{log_dir}/xkeeper-bench-*` 日志文件；`--token` 透传为 `Authorization: Bearer`。清理逻辑放在退出路径上无论成败都执行（含 ctrl-C handler）。
- **D8 JSON 报告 schema（核心字段，前向可扩展）**：`{ case, params{...}, programs[{name, rows, bytes, out_rows, err_rows}], aggregate{rows, bytes, rows_per_sec, bytes_per_sec}, wall_time_secs, rotation_count, daemon_rss{peak, avg}|null, integrity{pass, expected_rows, found_rows}, started_at, bin_versions{xkeeper, bench} }`。表格输出为同一数据的投影。
- **D9 case 参数矩阵**：firehose/rotation/drip 单程序（--programs 忽略），fanout 消费 --programs；rotation 由 bench 自动设定 `log_max_size = max(64KB, 4×row_size)`、`log_rotate_keep = ceil(rows×row_size / max_size) + 2`（保证无删除），用户无法在 v1 覆盖这两个内部值（防误配破坏对账）。

## Risks / Trade-offs

- [全速产出瞬时速率超出对账前的排空估计，pump 缓冲未落盘导致完整性误报] → 排空窗口按「文件大小静止」自适应而非固定 sleep；误报时报告 found/expected 行数便于定位。
- [/proc 采样粒度 1s 可能错过 RSS 尖峰] → 报告口径注明为采样峰值（sampled peak），drip 用例关心的是有界性趋势，可接受。
- [connect 模式在用户 daemon 上产生真实磁盘写压] → 文档与 --help 明示；量界参数让用户可控总量；清理路径保证无残留。
- [bench 二进制自我重入路径在 current_exe 被移动/删除后失效] → spawn 前解析 current_exe 绝对路径写入 app 配置；失败即报错退出。
- [与 stress.rs 场景重叠（firehose）但实现独立] → 已由用户拍板完全独立；两者口径不同（验证 vs 测量），文档互相引用避免误解。

## Migration Plan

纯新增：workspace members 追加 `xkeeper-bench`，无存量行为变更。回滚 = 从 members 移除并删目录。文档（README/AGENTS.md）随实现同批提交。

## Open Questions

（无——影响 spec/方案/任务拆分的未知点已在拷问轮收束；排空窗口与采样频率为实现期调优项，不影响契约。）
