# Tasks — add-webui-console

## 1. 指标采集后端（metrics 能力）

- [x] 1.1 新建 `src/metrics.rs`：`MetricsTable`（程序 cpu/mem 最新值 + 系统
      概况）与 `RateCounter`（原子总计数 + 300 格滚动桶 + 窗口速率计算），
      单测覆盖窗口求和与重启清零语义；`cargo test metrics` 通过
- [x] 1.2 Linux 采样路径：`/proc/<pid>/stat`（从最后 `)` 解析）、`statm`、
      `/proc/stat`、`/proc/meminfo`，CPU% 按逻辑核归一；单测覆盖含空格
      comm 的 stat 解析
- [x] 1.3 Windows 采样路径：`GetProcessTimes`/`GetProcessMemoryInfo`/
      `GetSystemTimes`/`GlobalMemoryStatusEx`，`windows-sys` 增补 feature
      后 `cargo check` 双 cfg 通过（本机非 Windows 时以 CI/交叉检查或
      `cargo check --target` 验证编译面）
- [x] 1.4 采样线程接入守护进程启动路径（`run` 与 `webui` 均启动，随
      shutdown 退出）：每秒读 pid 列表、维护基线、pid 消失/变化时置空与
      重建；集成测试：守护一个 sleep 程序，2 秒后投影含非空指标，停止后
      指标回 null；另断言采样不持守护状态锁跨系统调用（命令执行不被
      采样周期延迟）
- [x] 1.6 指标线程故障隔离：人为使采样线程 panic/退出，守护循环与受管
      程序照常运行、投影除指标为空外正常；`cargo test` 通过
- [x] 1.5 pump 热路径接 `RateCounter`（每行一次 `fetch_add`），spawn 路径
      清零计数；单测：无订阅者时速率照常累计

## 2. 投影与 API 扩展（三处同源）

- [x] 2.1 扩展 `ProgramInfo`（`cpu_percent`/`mem_bytes`/`log_rate`）与
      `DaemonInfo`（`system`），`status_doc`/`program_info` 合并
      `MetricsTable`；既有 `web.rs`/`server.rs` 测试更新并通过，新增字段
      JSON/MessagePack 同构断言
- [x] 2.2 `/v1/status`、`/api/overview`、WS 快照含指标字段的契约测试
      （运行程序 → 三处均读到同名字段）；`xkeeper status` CLI 输出不回归
- [x] 2.3 WS 状态增量携带指标变化：单测模拟指标更新 → diff 事件包含该
      程序条目

## 3. 高吞吐日志管道（webui-api 批量与背压）

- [x] 3.1 `pump.rs` 攒批：本地缓冲 ≥256KiB 或 ≥50ms flush（一次文件写 +
      一次 `ring.push_batch`），`Ring` 增 `push_batch`；既有轮转/tail
      单测通过，新增"多行一批、tail 语义不变"断言
- [x] 3.2 `Ring::subscribe` 有界化：`sync_channel` + `try_send`，满则
      记 dropped、恢复时先补 Gap 项；单测：慢消费者收到 Gap、ring 内容
      完整、第二个订阅者不受影响
- [x] 3.3 `api.rs` 新帧型 `LOG_GAP`(6)（程序名 + 流字节 + u64 行数），
      编解码单测；`web.rs` 订阅任务改为批量 drain 攒帧（≥64KiB 或
      ≥100ms），Gap 转 `LOG_GAP` 帧；`out_tx` send 带 250ms 超时、超时
      关闭会话
- [x] 3.4 WS 集成测试：高速写入（模拟 ≥1MB/s）下——正常客户端一帧多行
      且内容完整；人为限速消费者触发 `LOG_GAP`；落盘文件行数与写入行数
      相等（丢弃只影响实时流）
- [x] 3.5 10MB/s 压测脚本/用例（`xtask stress`：驱动真实守护进程二进制 +
      限速生成器 → 环形缓冲 + 落盘 + 一个 WS 订阅者）：守护进程无消息丢失落盘、CPU 占用有界、WS 流出现
      批量帧与（客户端限速时）Gap 标记；记录实测数字到变更目录
- [x] 3.6 零反压验证（log-management 零干扰场景）：压测中断言 (a) 子进程
      输出吞吐不因订阅者存在或挂起而下降（对比无订阅者基线），(b) 守护
      进程 RSS 在持续高速输出下稳定不随总量增长，(c) 模拟磁盘写失败
      （只读目录/满盘环境）时子进程继续输出不阻塞、日志仅进内存；结果
      记入变更目录

## 4. 前端基础设施（webui/）

- [x] 4.1 引入 `@msgpack/msgpack`；实现 ws-client 模块：连接、指数退避
      重连、帧解码（类型字节 + zlib via `DecompressionStream` + msgpack）、
      快照/增量合并、日志订阅管理与 Gap 解码；vitest 单测覆盖解码与
      合并逻辑（含构造的压缩帧）
- [x] 4.2 轻量 store + Lit controller：快照/增量 → 树形状态；WS 断开降级
      轮询 `/api/overview`（按所选刷新频率）+ 离线横幅；单测模拟断连/
      恢复状态迁移
- [x] 4.3 偏好模块：刷新频率（1/3/5/10s，默认 3s）、速率窗口（默认 10s）
      localStorage 持久化；单测覆盖持久化与缺省值

## 5. 控制台界面（webui-ui 规范落地）

- [x] 5.1 侧栏两层树（app → 程序，含未启动程序、状态徽章、选中高亮）
      与路由联动；happy-dom 测试：多 app 多程序渲染、未启动占位、选中
      切换主区
- [x] 5.2 守护概况条（版本/monitor_interval/uptime + 系统 CPU/内存）与
      刷新频率选择器；测试覆盖指标显示与频率切换生效
- [x] 5.3 App 概况视图：程序汇总表（名称/状态徽章/重启计数/uptime/
      CPU%/内存/日志速率列，fatal 行警示，速率窗口切换全局生效）；测试
      覆盖占位（未运行 → `--`）与窗口切换
- [x] 5.4 程序详情视图：运行信息（命令行/cwd/PID/uptime/重启数）、
      start/stop/restart 按钮带确认与结果反馈；测试覆盖确认流程与失败
      反馈（409 场景）
- [x] 5.5 日志查看器组件：out/err 切换、tail 初始、跟随开关、翻阅让位、
      有界缓冲（≈5000 行）、rAF 批量渲染、Gap 标记行独立样式；测试覆盖
      跟随/暂停/让位与缓冲上限
- [x] 5.6 离线降级与恢复：后端不可达横幅 + 旧数据标注 + 自动恢复；测试
      覆盖两态迁移
- [x] 5.7 端到端手测清单走查（真实浏览器 + `xkeeper webui`：树/概况/
      详情/控制/日志/指标刷新/断连恢复），`pnpm exec tsc --noEmit &&
      pnpm test && pnpm build` 全绿后 `cargo build --release` 重嵌入并
      复验占位页被真控制台替换

## 6. 收尾

- [x] 6.1 全量验证：`cargo test`（双平台路径单测）、`cargo run -- validate`、
      `openspec validate add-webui-console --strict`；对照 specs 逐条
      scenario 核对（metrics/webui-api/webui-ui/control-plane/
      log-management delta，重点核对零干扰与零反压条款），结果记入变更
      目录
