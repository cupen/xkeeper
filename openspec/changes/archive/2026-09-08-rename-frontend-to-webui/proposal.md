# Proposal: rename-frontend-to-webui

## Why

`xkeeper webui` 子命令伺服的 Web 控制台已成为产品的正式组成部分，但前端工程
仍叫 `frontend/`，与产品术语（webui）、规范能力名（`webui-ui`/`webui-api`）和
子命令名不一致；同时构建工程把"debug 构建也自动跑 pnpm build"作为唯一路径，
日常 Rust 调试被前端工具链拖慢。需要借重命名机会把构建工程重新梳理：
dist 只在需要时构建并嵌入二进制，前端开发走 HMR。

## What Changes

- **BREAKING（仓库内路径）**：目录 `frontend/` 重命名为 `webui/`（源码、
  配置、lockfile、构建输出全部迁移）。
- 构建工程重新梳理（`build.rs` + `src/assets.rs`）：
  - `webui/dist` 仍经 rust-embed 嵌入 rust 二进制，产物保持"单个二进制
    包含完整控制台"不变；
  - **debug 构建（`PROFILE=debug`）默认跳过前端构建**：不调用 pnpm，
    dist 缺失时嵌入占位页；提供 `XKEEPER_WEBUI_BUILD=force` 供 debug 下
    强制构建真实 UI；
  - release 构建行为不变：自动 install + build + 嵌入，bundle 变化触发
    重新编译；
  - Node 工具链不可用时的占位页回退保留（CI 无 Node 环境 `cargo test`
    保持绿色）。
- HMR 开发工作流补齐：`pnpm dev`（Vite，:5273）代理补上 `/ws`
  WebSocket 代理（当前仅代理 `/api` 与 `/health`），使 WS 推送在 dev 模式
  可用；`XKEEPER_WEBUI_DEV` 说明文档化（dev 时代理目标可用环境变量覆盖）。
- 环境变量与文档同步：`XKEEPER_FRONTEND_BUILD` 更名/别名化为
  `XKEEPER_WEBUI_BUILD`；`.gitignore`、`AGENTS.md`、`README.md`、
  `build.rs` 头注释、`src/assets.rs` 头注释中的 frontend 路径与术语全部
  更新。
- package `name` 更名为 `xkeeper-webui`。

## Capabilities

### New Capabilities

- `webui-build`: Web 控制台前端工程的构建与嵌入契约——dist 何时构建
  （release 自动、debug 默认跳过、显式覆盖）、嵌入二进制的方式、工具链
  缺失时的占位回退、HMR dev 工作流的代理契约。

### Modified Capabilities

（无——`webui-api` 与 `webui-ui` 的运行时行为不变；占位页/真实 UI 的
伺服路径契约由新能力 `webui-build` 表达。）

## Impact

- 代码：`build.rs`（监听路径、PROFILE 分支、环境变量）、
  `src/assets.rs`（embed folder 路径）、`src/web.rs`（如注释引用路径）。
- 前端工程：`frontend/` → `webui/` 整体迁移；`package.json` name；
  `vite.config.ts`（/ws 代理、代理目标环境变量）。
- 配套：`.gitignore`、`AGENTS.md`、`README.md`、CI（无需改：无 Node 时
  占位回退已覆盖 `cargo test`）。
- 风险：`build.rs` 的 rerun-if-changed 路径遗漏会导致 dist 变更不触发
  重新嵌入；debug 跳过逻辑不得影响 release 产物正确性——以任务清单
  逐项核对两条路径。
