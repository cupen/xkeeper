## Context

关键现状约束（动机见 proposal）：

- `src/supervisor.rs` 是单线程监督循环 + 命令队列唯一写者，任何长耗时操作进循环都会冻结巡检（健康检查、退避重启）。
- `src/platform.rs` 已有进程树清理原语：unix `terminate_gracefully` / `kill_group`，Windows `JobHandle`（Job Object）。
- `src/config.rs` 的 `is_valid_name` 是应用名/程序名的唯一校验闸口（当前禁路径分隔符与控制字符，允许点号）；`resolve_program` 已实现四层字段解析。
- `src/client.rs` 的 CLI 客户端从 daemon 配置自取 host/port/token，动作命令里链式调 `xkeeper` 子命令因此天然可用。
- `src/shell.rs` 自带词法解析与相近命令提示，新增动词成本低。
- 控制面三面板（`/v1`、`/api`、WS 帧）共享状态投影；本次新增的行为若进投影会强制 webui 三处同源修改。

## Goals / Non-Goals

**Goals:**

- 动作系统落进现有骨架：复用命令队列、平台进程原语、CLI 客户端与 shell client，不引入新依赖。
- 动作执行对 supervisor 循环零阻塞，对状态投影零侵入。
- 标识符字符集在单一闸口收紧，三条平台路径（validate/add/reload）行为一致。

**Non-Goals:**

- 不做 daemon/app 级 custom action（配置结构预留词汇，不实现执行）。
- 不做动作排队、动作状态投影、webui 动作面板。
- 不重构既有端点与命令（薄概念决策，见 proposal）。

## Decisions

1. **动作执行走 worker 线程，不进状态机**。动作子进程由 server 层派生的 worker 线程 spawn，HTTP 请求线程同步等待（有界：timeout）；supervisor 循环与命令队列全程不接触动作。备选「把动作做成 program 状态机的新状态」被否：状态机 7 态描述的是受管子进程生命周期，动作进程不是受管子进程，混入会污染核心模型；备选「异步 run id 轮询」被否：控制面是本地回环 API，同步等待足够，且省掉一套 run 生命周期管理。

2. **经 shell 执行与 no-shell 原则的有意分叉**。program `command` 维持 `split_command` 直 spawn（确定性、跨平台一致）；custom action 命令整体交给平台 shell（unix `/bin/sh -c`，Windows `cmd /C`）。理由：动作是运维者编写的维护逻辑，管道/重定向/`&&` 是核心价值；PowerShell 不保证存在于目标机，`/bin/sh` 与 `cmd` 是唯一可假设的基线。此分叉写入规范与 README，避免被当成疏忽「修掉」。

3. **变量替换：resolve 期校验、spawn 期求值、前缀限定**。替换器只处理以已知域前缀开头的 `${program.*}` / `${app.*}` / `${daemon.*}`：resolve 期扫描名字并拒绝未知字段（fail fast，拼错在 validate/reload 即报）；spawn 期以运行时值求值（pid、state 是动态的）。**不匹配已知域前缀的 `${...}` 原样保留**交给 shell（用户的 `${HOME}` 等 shell 语义不受影响）。备选「全量替换后交给 shell」被否：会把 shell 变量误伤。

4. **互斥粒度为 (program, action)**：进行中登记表（Mutex 保护），冲突返回 409。备选「排队」被否：升级类动作重复排队会在旧动作结束后连环触发，语义危险；「同 program 全部动作互斥」被否：flush-cache 与 upgrade 并行是合理场景。同一 program 不同动作允许并行，风险见下。

5. **app 扇出在 server 端展开**：新命令 `AppStart/AppStop/AppRestart` 入既有命令队列，由 supervisor 按已解析程序的 priority/depends_on 顺序逐程序执行既有 per-program 逻辑（含逆序停止、spawn 清 `user_stopped`），排序逻辑与守护启动路径同源，不写第二份。端点为 `POST /v1/apps/{name}/start|stop|restart`。

6. **signal 走 platform 新原语**：unix `libc::kill(pid, sig)` + 白名单枚举（TERM/INT/HUP/QUIT/USR1/USR2）；Windows 在 server 层直接返回平台不支持错误（不进入 platform 层）。投递目标取 `ManagedProgram` 当前 child pid，不动状态机。KILL/STOP/CONT 显式排除并在错误信息中说明（KILL→stop，STOP/CONT→状态失真）。

7. **字符集收紧在 `is_valid_name` 单点完成**：改为 `[A-Za-z0-9_-]+` 全匹配。add/validate/reload/scaffold 全部经过该函数，行为自动一致；升级后旧名报错信息给出修复指引。动作名、程序名、应用名共用同一条规则（glossary 标识符不变量）。

8. **动作不进状态投影**：结果只在 HTTP 响应（退出码、耗时、是否超时、输出尾部）与 daemon 日志（完整输出，行前缀 `[program.api.action.upgrade]`）。这避免了 `/v1`、`/api`、WS 帧三处同源修改与 webui 变更。

9. **glossary 以独立 spec 能力承载**，README 精简表格指向它；不把定义塞进 configuration（定义是词法约束不是校验行为），也不做构建期文档生成（过度工程）。

## Risks / Trade-offs

- [动作命令 = 配置作者在本机的任意代码执行能力] → 与 health exec 同级信任模型；调用面受 Bearer 鉴权覆盖；README 明示。
- [同一 program 不同动作并行可能互相踩踏（如两个动作同时重启）] → v1 接受，文档写明互斥粒度；链式 `xkeeper restart` 自身有命令队列串行化兜底。
- [动作数量无全局并发上限，线程按需派生] → 单机运维工具的调用量级下可接受；互斥已抑制同动作堆积；后续需要时加信号量，不预设。
- [Windows `cmd /C` 输出编码非 UTF-8（如 GBK 控制台）时日志有损] → 捕获按字节到 UTF-8 宽松转换，容忍替换符；文档记录。
- [字符集收紧破坏既有部署（含点号名）] → 升级文档写明迁移步骤：改名 → `validate` → `reload`；错误信息含修复指引。
- [shell 命令多行文本在 TOML 与 shell 两层都有转义语义] → README 给出 `"""` 与续行符的组合示例；e2e 覆盖多行用例。

## Migration Plan

无数据迁移。升级步骤：升级二进制 → 对既有部署跑 `xkeeper validate`（暴露字符集违规名）→ 改名 → `xkeeper reload` + `apply`。回滚即回退二进制（新字段 `[program.<n>.action.*]` 会被旧版 deny_unknown_fields 拒绝，回滚前需移除动作表——在升级文档中标注）。

## Open Questions

- 动作响应体 JSON 字段名（`exit_code` / `duration_ms` / `timed_out` / `output`）与输出尾部截断常量，实施时定，不影响规格。
