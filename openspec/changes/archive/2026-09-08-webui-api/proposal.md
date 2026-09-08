# Proposal: webui-api

> 2026-09-05 修订：rebase 到 main 的 app-supervisor 架构后，本变更的范围
> 收敛——REST 查询/控制能力已由 main 的 **control-plane** 能力（`/v1/*`，
> `src/server.rs`）承载，本变更不再平行定义一套查询 API，而是复用之。

## Why

`webui-ui` 规格已定义控制台需要的界面元素。main 的控制平面已提供守护进程的
REST 查询/控制/日志接口，但它是为 CLI 设计的（`Connection: close`、无推送）；
浏览器控制台还需要：内嵌 SPA 的伺服、跨请求的实时状态推送与二进制日志流。
数据格式需满足用户约束：允许二进制，要求高效率、数据 size 小。

## What Changes

- **新增 `webui-api` 能力**（收敛后）：定义 webui 控制台的伺服与推送契约——
  - 控制台服务器与守护进程同进程，复用 `Supervisor`：`/api/*` 查询端点使用与
    控制面 `/v1/*` **完全相同的状态投影**（`server.rs` 的 `StatusDoc`/`ProgramInfo`），
    控制命令经同一命令队列由守护循环执行；
  - 日志数据取自内存环形缓冲（log-management 能力），不经磁盘文件；
  - `/ws` 推送通道：连接即全量快照 + 状态增量 + 日志分块 + 心跳；
  - 数据格式：REST JSON；WS 二进制帧（1 字节类型 + MessagePack 载荷，日志帧为
    原始 UTF-8）——实证基准（同构负载）：MessagePack 为 JSON 的 82–87%，
    日志文本本身占绝对大头，真正的体积收益来自压缩（HTTP gzip / WS zlib 标记位，
    重复日志实测压缩后约为原始 10%）。Protobuf 因需要 schema 代码生成管线、
    收益边际被否。
- **CLI**：新增 `xkeeper webui [--listen <addr>]` 子命令 = 守护循环 + Web 控制
  台一体（控制台默认 127.0.0.1:9877；控制面端口 `[daemon] port` 不变）。
- **依赖**：`tokio` + `axum`（WS 伺服）、`rmp-serde`、`flate2`、`tower-http`、
  `arc-swap`（前端 `@msgpack/msgpack` 留待界面变更使用）。

## Capabilities

### New Capabilities

- `webui-api`: web 控制台的伺服与推送契约 —— 控制台端点复用控制面投影、命令
  经队列执行、日志取自环形缓冲；WS 协议（快照、状态增量、日志分块、心跳）、
  数据格式与效率约束（REST JSON、WS 二进制 + MessagePack、日志原始 UTF-8 帧）、
  生命周期（随守护进程启停）。

### Modified Capabilities

（无 —— control-plane / log-management / process-management 的需求不变；
本能力是它们的消费方与浏览器侧补充。）

## Impact

- `src/web.rs`：控制台服务器（axum：`/api/*` + `/ws` + 内嵌 SPA）。
- `src/api.rs`：WS 帧编解码。
- `src/server.rs`：状态投影类型改为 `pub(crate)` 共享（行为不变）。
- `src/main.rs`：`webui` 子命令接线。
- `Cargo.toml` / `frontend/package.json`：上述新依赖。
- 前端控制台界面（后续变更）以 `webui-ui` + 本能力为契约。
