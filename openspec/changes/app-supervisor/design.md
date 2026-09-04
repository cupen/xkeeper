# Design: app-supervisor — xkeeper v0.2 架构设计

## Context

当前代码（v0.1）是一个单线程主循环的进程保活器：

```
src/main.rs        CLI（run / validate）+ ctrlc 处理
src/config.rs      v0.1 TOML schema（[daemon] + [[program]]，autorestart 为 bool）
src/program.rs     ManagedProgram：spawn / tick / stop，状态 5 态（running/backoff/exited/stopped/fatal）
src/supervisor.rs  顺序启动 + 每 monitor_interval 巡检 try_wait + 退避重启
src/platform.rs    Windows Job Object（kill-on-close）/ Unix SIGTERM（libc）
```

约束与既有资产：纯 std 线程模型（无 async 运行时）、serde + toml 解析、ctrlc 信号处理、env_logger 自身日志、单二进制、配置相对路径基于配置文件目录解析。动机见 [proposal.md](proposal.md) 的 Why 一节，行为契约见各 `specs/*/spec.md`，本文只回答"怎么实现"。

## Goals / Non-Goals

**Goals:**

- 单二进制双角色：`xkeeper run` = 守护进程，其余子命令 = 控制客户端；`run` 与 `validate` 对 v0.1 用户完全兼容
- 本地控制平面：HTTP JSON API（回环绑定）+ CLI 控制命令，状态可查、进程可控、配置可热更
- 应用层进程语义补齐：startsecs/startretries、重启策略与期望退出码、priority/depends_on 排序、健康检查
- 日志可运维：输出接管 + 按大小轮转 + 环形缓冲 + `log --tail/-f`
- Linux / Windows 行为对齐到同一规格，平台差异全部收敛在 `platform.rs`
- 分层配置模型：core 根配置只做全局（`[daemon]` 含 `app_dir` + `[app-default]` 应用默认值，禁止应用条目）+ 各部署目录内的 app 配置（`[program.*]` map、扁平字段）；`xkeeper add .` 注册即 `app_dir` 建链，全新安装空启动

**Non-Goals:**

- 内置 Web UI（另立变更单独设计；本变更只交付其依赖的 REST API 基础）、事件监听协议（supervisord event listener）、远程/多主机管理
- 自安装为系统服务（继续交给 systemd / nssm / 计划任务托管 `xkeeper run`）
- cgroups / Job 内存 CPU 限额、run-as 用户切换、附加到非子进程（attach by pid）
- 除 TOML 外的配置格式；配置中心或远端配置拉取

## Decisions

### D1. 单二进制双角色，而非 xkeeperd + xkeeperctl 两个可执行文件

**选择**：一个 `xkeeper` 二进制，`run` 子命令进入守护进程模式，`status/start/...` 作为 API 客户端。
**备选**：supervisord 式两个二进制（xkeeperd / xkeeperctl）。
**理由**：发布产物只有一个，跨平台分发与升级最简单；客户端代码量很小，不值得拆分。特权分离需求（本工具不提权）暂不存在。

### D2. 控制平面 = 回环 HTTP JSON API，std::TcpListener 手写极简服务端

**选择**：`[daemon] host=127.0.0.1, port`（默认 7310）上提供 `/v1/*` JSON API；JSON 用 `serde_json`。HTTP 层为手写极简实现：std::TcpListener + 每连接一线程，仅解析请求行与头（上限 32KB），JSON 响应带 Content-Length 并 `Connection: close`；`logs` follow 端点以 chunked 编码逐行写并显式 flush。
**备选**：(a) tiny_http —— 实测其响应 writer 为 1024B BufWriter 且 flush 只发生在响应体 EOF，无限 follow 流在客户端超时前收不到任何字节，被否；(b) Unix domain socket / Windows named pipe —— 需要双平台抽象层，Windows 侧无法用 curl 直接调试，回环 TCP 已满足本机隔离；(c) axum/gRPC + tokio —— 引入 async 运行时，与现有纯线程模型和"轻量"定位冲突。
**理由**：本地单用户控制负载极低，端点集小而固定（一个 GET 路由 + 少量 POST），手写解析约百行且可控；流式 flush 由我们直接掌控，follow 才能做到逐行实时。

### D3. 并发模型：共享状态 + 命令队列，而非纯消息传递

**选择**：核心状态放在 `Arc<Mutex<SupervisorState>>`（含每个程序的运行时状态与全局配置快照）。API 线程读取直接拿锁取快照；控制操作（start/stop/restart/reload/shutdown）把命令连同应答通道（`std::sync::mpsc` 一次性回执）塞进队列并 `Condvar::notify` 主循环，由主循环**执行**（stop 可能阻塞到 `stop_timeout`），完成后回执。主循环每至多 100ms 醒一次（沿用现有分片睡眠），收到 notify 立即醒。
**备选**：(a) 所有状态归主循环私有、纯 channel 通信 —— 读快照也要排队，status 在程序多时变慢且实现繁琐；(b) 全异步 —— 与 D2 冲突。
**理由**：读多写少，Mutex 读快照最简单；命令执行权收敛在主循环一处，避免两处并发操纵同一子进程。日志 follow 走独立的订阅注册表（每客户端一个 mpsc，泵线程广播），不占用状态锁。

### D4. 采用 supervisord 式 startsecs/startretries，与退避参数共存

**选择**：引入两个计数器——`startretries`（连续"未达 startsecs 即退出"的预算，耗尽 → fatal）与稳态重启退避计数（现有 `restart_backoff` 指数退避，`backoff_reset_after` 清零）。进程存活 ≥ `startsecs` 即视为"启动成功"，startretries 预算重置。
**备选**：只保留 v0.1 的 `backoff_reset_after`（用运行时长近似判断"稳定"）。
**理由**：`startsecs` 让"启动即崩"与"稳定运行后崩溃"可区分配置（前者要限次进入 fatal，后者要无限重启），这正是守护进程管理器的核心语义；两个计数器职责不同，不冲突。

### D5. 输出接管：管道 + 每流泵线程，轮转在写路径上完成

**选择**：spawn 时 stdout/stderr 接管道（不再继承文件句柄），每个流一个泵线程：读 → 写当前文件 → 超过 `max_size` 时 close → 重命名轮转链（`.1`、`.2`…，超出 `rotate_keep` 删除）→ 重新打开 → 继续写；同时 push 进环形缓冲（默认 1000 行，`[daemon] log_buffer_lines`）并广播给 follow 订阅者。
**备选**：维持 v0.1 的文件句柄继承 —— 无法轮转、无法 follow，且句柄被子进程持有导致 Windows 上 rename 不可行。
**理由**：只有 xkeeper 自己持有日志文件句柄，Windows 上轮转 rename 才安全（子进程只握管道写端）；子进程经管道天然受泵线程反压，不会丢行；线程成本（每程序 2 个）在百级程序规模内可接受。写盘失败（磁盘满等）只告警、计数并继续，绝不阻塞主流程。

### D6. 健康检查由单一检查器线程统一调度

**选择**：一个检查器线程按每个程序的 `interval` 计算下次检查时间，到点执行：`tcp`（TCP 连接）、`http`（GET，2xx/3xx 为健康）、`exec`（短命子进程，退出码 0 为健康，超时杀）。连续失败达 `retries` 置 unhealthy；`restart_on_unhealthy` 时向主循环投递与崩溃等效的重启命令。`start_period` 内失败不计数。
**备选**：每程序独立定时器线程。
**理由**：检查器是 IO 等待型负载，单线程串行调度在合理 interval（秒级）下足够，避免线程爆炸；exec 检查临时进程与被管进程无耦合，不影响状态机。

### D7. reload 按应用粒度 diff，统一走停止-重建路径

**选择**：reload 重读 core 与全部已注册 app 配置：注册表变化（新增/移除应用）总是应用；对每个 app 以定义哈希 diff——变更的 app 停止其受影响程序、以新定义重建、按 autostart/依赖决定是否拉起。校验与应用按 app 原子：单个 app 文件非法仅该应用保持旧定义并上报错误，不阻塞其他应用；跨应用程序名冲突在注册/校验期拒绝。
**备选**：单一大配置文件整体 diff（本变更初稿方案）——所有应用挤在一个文件里，多应用协作与配置漂移都难管理；只对 `command/args` 做原地重启则覆盖不了 health_check、env 等字段变化。
**理由**：分层后变更天然按 app 隔离，故障域小；停止-重建复用现有状态机，实现面最小。

### D8. 依赖与优先级：校验期成环检测，运行期等待-放弃

**选择**：校验时对 `depends_on` 做拓扑排序检测（成环 → 校验失败）。启动顺序 = priority 升序稳定排序（同值按配置顺序）；依赖未 running 的程序挂起等待，依赖最终 `fatal` / `exited`（且不再重启）时依赖方进入 fatal（原因注明）；关闭按逆序停止。依赖重启不自动级联重启下游（记录到未来事件钩子）。
**理由**：成环在配置期拒绝比运行期死等友好；"放弃"语义防止雪崩式无限等待。

### D9. 平台层扩展：Unix 进程组 + run-as 保持不做

**选择**：`platform.rs` 在现有 Job Object 基础上，为 Unix 增加 spawn 后 `setpgid(0,0)`（pre_exec）与 `killpg` 停止路径，实现"孙进程不残留"。Windows 优雅停止维持 TerminateProcess（规格已按平台对齐）；CTRL_BREAK 进程组通知列为未来增强。run-as（Unix setuid）不在本变更范围。
**理由**：进程组是 Unix 下与 Job Object 对等的树清理原语，改动集中在 platform.rs 与 spawn 参数。

### D10. 模块布局与分层配置 schema

```
src/
├── main.rs        CLI 分发：run（守护进程）/ validate / 控制子命令（客户端）
├── config.rs      core/app 分层 schema、平台默认路径、校验（依赖成环、跨应用重名）、优先级解析
├── registry.rs    应用注册表：add/remove/upsert、core 配置原子写入、legacy 导入
├── program.rs     单程序状态机（7 态 + unhealthy）与 spawn/stop/restart 动作
├── supervisor.rs  注册表、排序启动、命令队列消费主循环、reload 执行、关闭编排
├── pump.rs        输出泵：文件写入、轮转、环形缓冲、follow 广播
├── health.rs      健康检查器线程
├── server.rs      HTTP API：路由、鉴权、JSON 编解码、logs 流式响应
├── client.rs      控制子命令的 HTTP 客户端、log follow 渲染、退出码
└── platform.rs    平台差异：Job Object / 进程组、信号、树清理
```

分层配置 schema——core 只做全局，app 配置为部署目录内的 map 形态；内置默认值保持 v0.1 兼容行为：

```toml
# ── core 根配置（全局唯一，禁止任何应用专属条目）──
# Linux: /etc/xkeeper.toml   Windows: %APPDATA%\xkeeper\xkeeper.toml   （-c 可覆盖）
[daemon]
log_level = "info"          # trace|debug|info|warn|error
log_dir = "logs"            # 相对 core 配置文件目录
monitor_interval = 1.0
host = "127.0.0.1"          # 仅回环默认
port = 7310
auth_token = ""             # 空 = 不鉴权（依赖回环绑定）
log_buffer_lines = 1000
app_dir = "apps"            # 应用注册目录（缺省 core 配置同级的 apps/，相对 core 配置目录解析）

[app-default]               # 可选：全部应用共享的默认值（字段与程序同名）
autostart = true
autorestart = "always"
restart_backoff = 1.0       # 重启间隔（秒）
```

```toml
# ── app 配置（各应用部署目录内，默认名 xkeeper.toml）──
[app]                       # 可选：应用级默认值（`add` 微调 flag 的落点，部署人可手改）
description = "演示应用"
autorestart = "on-failure"
priority = 0

[program.api]               # map 接口：键即程序名，省略 name 字段
command = "python -m http.server 8000"   # 单行写法：按 shell 词法拆分，不经 shell；与 args 互斥
work_dir = "."
env = { FOO = "bar" }
exit_codes = [0]            # on-failure 的"期望退出码"
max_restart_backoff = 30.0
max_restarts = 0            # 0 = 不限（稳态崩溃重启次数上限）
backoff_reset_after = 60.0
startsecs = 1.0             # 存活超过此时长才算启动成功
startretries = 3            # 连续启动失败预算，耗尽 -> fatal
depends_on = []             # 可引用其他应用的程序
restart_on_unhealthy = false
log_max_size = "10MB"       # 日志拍平：支持 KB/MB/GB 后缀或字节数
log_rotate_keep = 5
health_check = "http://127.0.0.1:8000/health"   # 单字符串：http(s):// -> HTTP、tcp:// -> TCP、其余 -> exec 命令行
health_interval = 10        # 可选节奏字段；health_timeout/retries/start_period 同理
```

字段优先级（高→低）：`[program.*]` 显式字段 > app 配置 `[app]` > core `[app-default]` > 内置默认。`autostart`/`priority` 为应用级字段（仅 `[app]`/`[app-default]` 层）。程序名 = 映射键，跨全部应用全局唯一；`command` 含空白时拆分为 argv（不经 shell），与显式 `args` 互斥。注册时在 `app_dir` 建立 `<name>.toml` 链接（symlink → 同卷硬链接 → 告警跳过），配置本体始终以部署目录原件为准。

状态机（含迁移触发条件）：

```
                    spawn                       连续运行 >= startsecs
   [stopped] ───────────────▶ [starting] ─────────────────────────▶ [running]
       ▲  autostart / start      │ startsecs 内退出                      │ 退出(稳态)
       │  命令                    ▼  (startretries 预算-1)               ▼
       │                    [backoff] ◀──── 策略允许重启 ──────────── 退出记录
       │                        │ 退避到期 spawn                    (unhealthy 可触发)
       │                        └──────────▶ [starting]
       │
       │ startretries 耗尽 ──▶ [fatal]（仅显式 start / reload 可清除）
       │
       └── stop/restart/shutdown ◀── [stopping]（SIGTERM|Terminate → 超时强杀）── [stopped]
```

### D11. API 与 CLI 具体契约

| 端点 | 方法 | 说明 | 主要错误 |
|---|---|---|---|
| `/v1/health` | GET | 存活探测（免鉴权） | - |
| `/v1/status` | GET | 守护进程信息 + 全部程序状态 | 401 |
| `/v1/programs` | GET | 程序列表 | 401 |
| `/v1/programs/{name}` | GET | 程序详情（状态/pid/存活时长/计数/unhealthy/等待原因） | 404 |
| `/v1/programs/{name}/start\|stop\|restart` | POST | 同步执行，受 stop_timeout 上限；返回执行后状态 | 404, 409（非法转换如 start@running） |
| `/v1/programs/{name}/logs` | GET | `stream=out\|err`、`tail=N`、`follow=1`（chunked） | 404 |
| `/v1/reload` | POST | 热更新，返回变更摘要 | 400（新配置非法，守护进程不受影响） |
| `/v1/shutdown` | POST | 优雅关闭后进程退出 | - |

CLI 控制命令逐一映射上述端点（日志命令为 `log`，映射 `/logs` 端点）；`log -f` 渲染 chunked 流；`add/remove/list` 离线直接读写 core 配置并维护 `app_dir` 链接，在线时持久化后调用 `/v1/reload` 同步。退出码：0 成功；1 一般错误；2 配置错误；3 守护进程不可达。守护进程启动时端口占用 → 以明确错误退出（非零），提示可能已有实例。

### D12. 测试策略

- **单元测试**：core/app 分层解析（map 接口、扁平字段、单行 command 拆分、health_check 协议分发）、legacy 导入、`app_dir` 链接扫描与降级链、四层优先级解析、校验（依赖成环、跨应用重名、单行/args 互斥）、按 app 定义哈希 diff；状态机迁移表；退避与 startretries 计数；大小字符串解析；环形缓冲与轮转文件名推进（临时目录）。
- **集成测试**（真实子进程，`cmd`/`sh` 双平台分支）：启动-崩溃-重启-fatal 全链路；depends_on 排序与放弃；stop 超时强杀；`xkeeper run` 起守护进程后用内置客户端驱动全部端点（含 401/404/409）；reload 三种 diff；Windows 上强杀守护进程验证 Job Object 清理（沿用 v0.1 冒烟方法）。
- **CI（后续可选任务）**：GitHub Actions 矩阵 ubuntu-latest + windows-latest 跑 `cargo test`。

### D14. 分层配置：core 只做全局 + 部署目录 app 配置 + `app_dir` 链接注册

**选择**：core 配置全局唯一（Linux `/etc/xkeeper.toml`、Windows `%APPDATA%\xkeeper\xkeeper.toml`，`-c` 可覆盖），只做全局：`[daemon]`（含 `app_dir`）与可选 `[app-default]`（全部应用共享的默认值表）；core 中出现 `[[app]]` 等应用专属条目一律被未知字段校验拒绝。应用注册记录 = `app_dir` 中的链接文件（`<name>.toml` → 部署目录配置本体；symlink → 同卷硬链接 → 告警跳过降级链）：`xkeeper add . --name xxx`（缺省名 = 目录名）建链即注册，`remove` 删链即注销，`list` 扫描 `app_dir`。`add` 的微调 flag 幂等写入部署目录 app 配置的 `[app]` 表（应用级默认值，部署人可手改；写入失败则注册回滚）。全新安装空无一物：core 与 `app_dir` 均可不存在。v0.1 单文件是合法的 legacy 形状：`add` 检测到 `[daemon]` 段时以独立 legacy schema 解析并转写为新形注册（`[daemon]` 提示并入 core，原文件不改）；`run -c <旧文件>` 走同一条导入路径。
**备选**：(a) core 内 `[[app]]` 注册表 + 父模板（本变更上一版）——应用专属内容（名称/路径/每应用 flag）进入 core，违背"core 只做全局"；(b) `app_dir` 下用"路径指针 + flag"注册文件替代链接——运维看到的是指针而非配置本体；(c) 单一大配置文件 / 目录扫描自动发现——同前述否决理由。
**理由**：注册即链接，`app_dir` 一眼看清全部受管应用；默认值四级各归其主——管理员改 core `[app-default]`、部署人改自己文件的 `[app]` 或 `[program.*]`、程序级细节进程序表，互不踩踏。注意 Linux `/etc` 写权限（非 root 需 sudo 或 `-c` 用户级）与 Windows 链接权限降级链。

### D15. app 配置写法：map 接口 + 扁平字段 + 单行 command + 协议分发健康检查

**选择**：程序用 `[program.<name>]` 映射声明，键即程序名（省略 `name` 字段，同 priority 平级排序按名称稳定排序）；`command` 支持单行带参（`"python -m http.server 8000"`，按 shell 词法规则——空白拆分、引号包裹——生成 argv，**不经过 shell**，无管道/重定向语义），与显式 `args` 互斥；字段扁平化并改名：`working_dir→work_dir`、`environment→env`、`[program.log]` 子表 → `log_max_size`/`log_rotate_keep` 标量；健康检查收敛为单字符串 `health_check`，按前缀协议分发（`http://`/`https://`→HTTP 探测、`tcp://host:port`→TCP 连通、其余→exec 命令行），节奏字段 `health_interval/health_timeout/health_retries/health_start_period` 可选扁平声明。
**备选**：(a) 保持 `[[program]]` 数组 + `name` 字段 + 子表 section——TOML 下样板重、嵌套深，手写易错；(b) 健康检查保留 `type+target` 结构——URL/地址已自带协议前缀，属冗余；(c) 单行 command 直接交给 `sh -c`/`cmd /c` 执行——引入 shell 注入面与跨平台差异，拆分为 argv 更安全一致。
**理由**：app 配置由部署人手写且要求"其它都默认"，写法少即是多：绝大多数程序一行 `command` + 一行 `health_check` 即可完整描述。

## Risks / Trade-offs

- [泵线程写盘失败或磁盘慢，管道缓冲塞满导致子进程被反压] → 写失败仅告警、计数并丢弃该次写入，泵永不挂死；正常路径反压是期望行为（与 supervisord 一致），文档注明。
- [tiny_http 同步处理 + 长连接 follow 占用工作线程] → follow 限并发数（默认 8，可配置），超出返回 429；其余端点均为短请求。
- [回环 API 被本机其他用户访问] → 默认仅绑定 127.0.0.1；提供 `auth_token`；文档明确多用户主机应启用 token。
- [Windows 无优雅信号，stop 语义弱于 Unix] → 规格按平台对齐（spec 已分列场景）；未来可加 CTRL_BREAK 进程组通知增强。
- [reload 部分成功造成新旧定义混杂] → 逐程序原子 + 错误逐条上报；新配置先整体校验通过才开始应用，杜绝"校验通过但中途失败"的大面积不一致。
- [依赖链在频繁重启时抖动放大] → 依赖方仅在依赖"永久不可用"（fatal/exited-不重启）时放弃，短暂重启只等待不动作。
- [线程数随程序数线性增长（每程序 2 泵线程）] → 百级程序规模内可接受；设计保留把泵合并为单线程多路复用的演进路径（改 pump.rs 内部即可）。
- [手写 HTTP 解析存在边角风险（畸形请求、pipelining）] → 请求面极小（GET/POST、无 body、Connection: close），头上限 32KB，解析失败一律 400 关连接；回环监听 + 可选 token，攻击面可控。
- [Linux 上 /etc/xkeeper.toml 写权限限制 add/remove（非 root 用户）] → 文档注明 sudo 或 `-c` 用户级覆盖；Windows 默认路径在用户目录下天然可写。
- [Windows 无符号链接权限导致 `app_dir` 降级为硬链接，而 `sed -i` 等替换式编辑会断开硬链接使 reload 读到旧内容] → 降级时告警说明影响；`xkeeper add .` 幂等刷新链接（编辑配置后重跑一次 add 即恢复）；symlink 模式不受影响。
- [app 配置分散后"哪些应用受管"不易一眼看清] → `app_dir` 集中存放全部注册链接，`xkeeper list` 扫描输出全量；reload 反馈逐 app 结果。
- [单行 command 拆分规则与真实 shell 不完全一致] → 明确只做词法拆分（空白 + 引号），不支持管道/变量展开；文档注明需要 shell 语义时显式写 `bash -c "..."`。

## Migration Plan

1. v0.1 迁移：`xkeeper add config.toml`（或 `run -c <旧文件>` 触发同一路径）把旧单文件注册为应用；`[daemon]` 段被检测并提示并入 core；原文件不修改、不删除。
2. 部署即替换二进制后重启守护进程（`xkeeper shutdown` → 新二进制 `xkeeper run`）；守护进程本身继续由 systemd/nssm 托管。
3. 回滚：换回 v0.1 二进制并按旧方式启动（直接使用原 v0.1 `config.toml`，导入不修改它）；core 与各 app 配置文件保留，不影响回滚。

## Open Questions

- 日志轮转是否需要按日期（daily）模式？当前仅 size 触发，`rotate = "size"|"daily"` 可在后续变更中以增量 spec 扩展。
- `xkeeper top` 之类的交互式 TUI 是否有价值？倾向不做，控制命令已覆盖。
