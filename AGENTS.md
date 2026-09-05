# xkeeper-webui — Agent Instructions

轻量级进程守护工具（Rust），通过 TOML 配置守护任意多个子进程：退出后指数退避重启、
stdout/stderr 落盘、Ctrl+C / SIGTERM 优雅停机、Windows Job Object 清理进程树。
附带 `xkeeper webui` 子命令伺服内嵌的 Web 控制台。

## Architecture

- `src/main.rs` — CLI 入口：`run`（纯守护）| `webui`（守护 + Web 控制台）|
  `validate`；控制子命令 `status/start/stop/restart/log/pid/reload/shutdown`
  走本地 HTTP API；`add/remove/list` 管理 app 注册表。
- `src/supervisor.rs` — 守护循环（7 态状态机 × 程序），命令队列唯一写者；
  `src/registry.rs` — app 注册表（app_dir 链接）；`src/config.rs` — 分层配置
  （core + app 部署文件）；`src/pump.rs` — 输出接管/轮转/环形缓冲；
  `src/health.rs` — 健康探测；`src/client.rs` — CLI HTTP 客户端。
- `src/server.rs` — 控制平面：std 手写 HTTP（`/v1/*`，可选 Bearer 鉴权），
  状态投影（`StatusDoc`/`ProgramInfo`）由此导出。
- `src/web.rs` + `src/api.rs` — Web 控制台（`xkeeper webui`）：axum 伺服内嵌
  SPA + `/api/*`（复用控制面投影）+ `/ws` 推送（快照/增量/日志/心跳，
  MessagePack 二进制帧）；与控制面共享 `Supervisor` 与环形缓冲。
- `src/assets.rs` — `rust-embed` 内嵌前端 `dist/`。
- `frontend/` — Web 控制台前端（pnpm + Vite + TypeScript + Lit），编译后的
  `dist/` 经 rust-embed 嵌入二进制；dist 不提交，fresh checkout 需先
  `pnpm build`（build.rs 会在 `cargo build` 时自动构建，无 Node 时回退占位页）。

## Build & Run

```bash
cargo build --release                  # 产物 target/release/xkeeper(.exe)

cd frontend
pnpm install
pnpm exec tsc --noEmit && pnpm test && pnpm build   # 类型检查 + 测试 + 产出 dist/，之后 cargo build 重新嵌入
pnpm dev                               # HMR dev server（:5273），代理 /api → 后端（:9877）

cargo run -- validate                  # 校验 core + 全部注册应用
cargo run -- webui --listen 127.0.0.1:9877   # 守护 + Web 控制台
cargo run -- status                    # 控制面 CLI（默认端口 7310）
```

## OpenSpec 规范驱动开发（`.agents`）

本仓库使用 [OpenSpec](https://github.com/Fission-AI/OpenSpec) 管理变更流程，
技能安装于 `.agents/skills/openspec-*`（6 个），规范与变更存放在 `openspec/`
（`openspec/specs/` 存能力规范，`openspec/changes/` 存进行中的变更，config 为
`openspec/config.yaml`，schema: spec-driven）。需要 `openspec` CLI（≥ 1.12）。

技能一览（通过 `Skill` 工具加载，或直接参考 `openspec <cmd>`）：

| Skill | 用途 |
|---|---|
| `openspec-propose` | 一步生成完整变更提案（proposal + specs 增量 + design + tasks） |
| `openspec-apply-change` | 按已批准提案实施，逐任务打钩 |
| `openspec-archive-change` | 变更完成后归档，把 specs 增量合入正式规范 |
| `openspec-sync-specs` | 修补/同步归档后未合并的 spec delta |
| `openspec-update-change` | 实施中途补充/更新变更工件 |
| `openspec-explore` | 动手前的方案探索与头脑风暴 |

### 工作流约定

- **先规划后编码**：非平凡变更先走 `openspec-propose` 产出提案工件；提案只做规划，
  不得在同一次响应里顺手改业务代码。实施须等用户明确批准后调用 `openspec-apply-change`。
- 新建变更：`openspec new change "<name>" --json`；查看状态：`openspec status --change "<name>" --json`；
  校验：`openspec validate --change "<name>"`；完成后归档：`openspec archive <name> --yes`。
- 归档前用 `openspec validate --specs` 确认正式规范无损合入。
- 遵循 `.agents/skills/openspec-*/SKILL.md` 中各自声明的边界（如 propose 的 planning boundary）。

## Constraints

- 前端改动后必须 `pnpm exec tsc --noEmit && pnpm test && pnpm build`，再 `cargo build`，
  否则二进制里嵌的是旧前端。
- 后端进程管理逻辑（重启退避、Job Object、信号处理）涉及平台差异
  （unix `libc` / windows-sys），改动需两条平台路径都过一遍。
- webui 与控制面共享 `server.rs` 的状态投影与 `Supervisor` 命令队列：改状态
  字段/命令语义时 `/v1`、`/api`、WS 帧三处必须同源修改。
