# Design: webui-api

> 2026-09-05 修订：rebase 到 main 的 app-supervisor 架构后按"保持设计一致"
> 原则收敛——能复用的不复刻。原设计与修订版的差异见决策 1。

## Context

main 的守护进程已有：std 手写控制面 HTTP（`server.rs`，`/v1/*` + 分块流式
log follow）、pump 环形缓冲（`pump.rs`）、7 态状态机（`program.rs`）、命令
队列（`Supervisor::enqueue` + Condvar）。控制面选用 std 而非框架的原因是
log follow 必须逐行刷新（D2）。webui 需要在此之上伺服内嵌 SPA 并向浏览器
推送实时状态——WebSocket 是硬需求，std 手写 RFC6455 不现实。

## Goals / Non-Goals

**Goals**

- 控制台服务器与守护进程同进程、零重复投影：`/api/*` 与 `/v1/*` 同源。
- WS 推送（快照/增量/日志/心跳）+ MessagePack 二进制格式，满足高效与体积约束。
- 生命周期随守护进程：`xkeeper webui` = 守护 + 控制台；Ctrl+C/shutdown 全体退出。

**Non-Goals**

- 不复刻控制面的 `/v1` REST（CLI 直接继续使用控制面）。
- 不实现前端界面（`webui-ui` 规格的落地是后续变更）。
- 不做控制台侧鉴权（默认回环绑定；控制面的 auth_token 语义保持不变）。

## Decisions

1. **（修订）REST 能力收敛到控制面，控制台只做增量。**
   原设计自定义 `/api` 投影 + arc-swap 快照 + watch 版本号 + 独立日志文件
   tail（`logs.rs`）。rebase 后控制面已提供等价查询与控制语义——本变更删除
   自有投影与文件 tail（`logs.rs` 移除），改为：`server.rs` 的
   `StatusDoc`/`ProgramInfo` 改为 `pub(crate)` 共享；命令直接 `enqueue`（复用
   Command 自带的 reply 通道）；日志从 `pump::Ring` 读取（环形缓冲对轮转免疫，
   与 log-management 规格一致）。备选"把控制面迁入 axum 统一端口"被否——
   破坏控制面的 D2 设计且引入 tokio 到守护主路径。
2. **WS 伺服用 axum，独立回环端口，std/async 桥接最小化。**
   axum（tokio）在专属线程上运行，通过 `Arc<Supervisor>` 的状态锁与命令队列
   与守护循环交互——与控制面完全相同的并发模型；关停用轮询
   `SupervisorState.shutdown` 标志实现优雅退出。备选"std 手写 RFC6455"被否
   （帧解析/掩码/分片易错且无生态收益）。
3. **数据格式：REST JSON，WS 用「1 字节类型 + MessagePack 载荷」二进制帧。**
   实证基准（Python msgpack/cbor2，同构负载）：

   | 负载 | JSON | MessagePack | CBOR |
   |---|---|---|---|
   | 快照（10 程序） | 1571 B | 1281 B（82%） | 1291 B（82%） |
   | 状态增量事件 | 91 B | 79 B（87%） | 79 B（87%） |
   | 日志块（100 行） | 6158 B | 6044 B（98%） | 6044 B（98%） |
   | 同日志块 gzip 后 | **603 B（10%）** | — | — |

   - MessagePack 与 CBOR 体积持平；选 **MessagePack**：`rmp-serde` 对 serde
     结构体透明（编码同构零成本），前端 `@msgpack/msgpack` 成熟。
   - **Protobuf 否决**：体积与 MP 同量级，却要 `.proto` schema + 代码生成
     管线，小项目摩擦远大于 10–15% 边际收益，且失去自描述性。
   - **日志帧**：文本占绝对大头（98%），格式选择无关紧要——直接以二进制帧
     承载原始 UTF-8（固定小头部）；体积收益来自压缩：HTTP gzip（tower-http）
     与 WS 结构化帧的 zlib 标记位（帧头 bit7，> 512B 触发）。
   - REST 保持 JSON：低频、可 curl 调试；WS 是持续高频通道，才是二进制的
     用武之地。
4. **WS 增量 = 会话内 diff，不改守护循环。**
   每会话以 `WS_POLL`（500ms）读共享状态锁、与上次快照 diff 出变更条目——
   不引入 watch/版本号机制（避免改动 supervisor 热路径）。monitor_interval
   ≥ 0.05s，推送延迟要求（一个 interval 量级）满足；日志走 ring 的订阅通道
   （std mpsc，会话内 try_recv + 短睡，不阻塞运行时）。备选"在守护循环里
   发布版本号"被否——侵入共享状态写路径，收益仅是省一次锁。
5. **WS 消息信封**：`[type:u8][payload]`，bit7=zlib。类型：1=快照、2=状态
   增量、3=日志块、4=心跳、5=错误；日志块载荷 = u16 名长 + 程序名 + 流向
   字节 + 原始 UTF-8。客户端消息为 `{"action":"subscribe"|"unsubscribe",
   "program":…,"stream":"out"|"err"}`（JSON 或 MessagePack 均可）。

## Risks / Trade-offs

- [两个 HTTP 服务器并存（控制面 + 控制台）] → 端口分离清晰（7310 / 9877），
  投影同源保证语义一致；README 已标注关系。
- [会话 diff 轮询每 500ms 锁一次状态] → 锁持有时间为投影构建（微秒级），
  会话数个位数，不构成热点；与控制面每请求加锁同量级。
- [WS 结构化帧压缩（zlib 标记位）是应用层实现，非 permessage-deflate 协商]
  → 规格为 SHOULD；对小载荷不压缩（阈值 512B），退化不影响正确性。
- [rmp-serde 与 serde_json 双编码漂移] → 同一结构体派生（编码同构规格 +
  测试）。

## Open Questions

（无。）
