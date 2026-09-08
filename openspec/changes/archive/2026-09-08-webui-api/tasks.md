# Tasks: webui-api

> 2026-09-05 修订：按 rebase 后的收敛设计实施（详见 design.md 决策 1）。
> 全部任务已完成。

## 1. 依赖

- [x] 1.1 `Cargo.toml` 新增 `tokio`、`axum`(ws)、`rmp-serde`、`flate2`、
  `tower-http`(compression-gzip)、`futures-util`；前端 `@msgpack/msgpack`。
  验证：`cargo build` 与 `corepack pnpm install` 通过。
- [x] 1.2 WS 帧协议模块 `src/api.rs`：类型常量、编解码（zlib 标记位）、
  日志帧（u16 名长 + 名 + 流向 + 原始 UTF-8）。验证：帧编解码单测通过。

## 2. 控制台服务器（复用控制面）

- [x] 2.1 `server.rs` 状态投影共享化：`StatusDoc`/`ProgramInfo`/`DaemonInfo`
  改 `pub(crate)` + `Deserialize`（`version` 转 String），新增 `status_doc()`
  统一入口。验证：控制面测试不回归（cargo test 全绿）。
- [x] 2.2 `web.rs` REST：`/api/health|overview|programs|programs/{name}`、
  `/api/programs/{name}/logs`（pump 环形缓冲 tail）、
  `POST /api/programs/{name}/{action}`（`Supervisor::enqueue` + reply）；
  未知程序 404、非法迁移 409；gzip 压缩层；内嵌 SPA（`/assets/*` + SPA
  fallback）。验证：REST 集成测试（含 404 错误对象、空环日志）。
- [x] 2.3 `/ws` 会话：连接即快照 → 500ms diff 增量（`status_delta`）→ 30s
  心跳 → 日志订阅（先补发 ring tail 再跟随，std mpsc 非阻塞转发）→ 错误帧
  （未知程序/坏流/坏消息）。验证：真实 TCP 的 WS 集成测试（快照→增量→日志帧）。
- [x] 2.4 `main.rs`：`xkeeper webui [--listen]` = `run_daemon` + webui 线程
  （专属 tokio runtime，轮询 shutdown 标志优雅退出）；`xkeeper run` 不变。
  验证：e2e——注册 app 后 `webui` 启动，控制面 `/v1/status` 与控制台
  `/api/overview` 数据一致，restart 生效，`/v1/shutdown` 后进程退出。

## 3. 验证与收尾

- [x] 3.1 体积基准：10 程序快照 MessagePack ≤ JSON 90%（单测断言）。
- [x] 3.2 e2e 冒烟：`xkeeper webui` 下 /v1 与 /api 双端口共存、日志 tail 来自
  环形缓冲、restart 真实重启进程、SPA 200；`cargo test`、`pnpm test` 全绿。
- [x] 3.3 `openspec validate webui-api --strict` 通过；README Web UI 章节
  更新（API 概览 + 与控制面同源说明）。
