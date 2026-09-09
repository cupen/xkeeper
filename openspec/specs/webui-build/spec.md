# webui-build Specification

## Purpose

规定 Web 控制台前端工程的构建与嵌入契约：`webui/dist` 产物在何种条件下
被自动构建并嵌入 rust 二进制、各构建模式下嵌入内容的来源、工具链缺失时
的回退行为，以及 HMR 开发工作流的代理契约。本能力只约束可观察的构建
产物与开发体验，不约束具体构建工具的内部实现。

## Requirements

### Requirement: dist 嵌入二进制

`cargo build` 产出的单个二进制 SHALL 内嵌 Web 控制台的构建产物
（`webui/dist`），并按既有 webui-api 契约伺服。产物未变化时 MUST NOT
触发相关 crate 的重新编译。

#### Scenario: release 构建包含真实控制台

- **WHEN** 在有 Node 工具链的环境执行 `cargo build --release`
- **THEN** 构建产物内嵌 `webui/dist` 的真实 SPA 入口页，`xkeeper webui`
  启动后浏览器可加载控制台

#### Scenario: 前端源码变更触发重新嵌入

- **WHEN** `webui/` 下的前端源码或配置发生变更后重新执行 release 构建
- **THEN** 构建系统检测到产物变化并重新构建、重新嵌入

### Requirement: debug 构建默认跳过前端工具链

debug 构建（`PROFILE=debug`）SHALL 默认不调用前端包管理器：dist 缺失或
为占位产物时直接嵌入占位页。此默认行为 MUST 可被环境变量
`XKEEPER_WEBUI_BUILD=force` 覆盖，覆盖后 debug 构建与 release 行为一致。

#### Scenario: 无 dist 的 debug 构建快速通过

- **WHEN** `webui/dist` 不存在且执行 `cargo build`（debug）
- **THEN** 构建不调用 pnpm，产物内嵌占位页，构建在无 Node 环境下成功

#### Scenario: debug 强制构建

- **WHEN** 设置 `XKEEPER_WEBUI_BUILD=force` 后执行 debug 构建
- **THEN** 前端被正常构建并嵌入真实 UI

#### Scenario: 环境变量重命名

- **WHEN** 设置旧变量 `XKEEPER_FRONTEND_BUILD=skip`
- **THEN** 其语义（跳过前端构建、沿用磁盘已有 dist）与新变量
  `XKEEPER_WEBUI_BUILD=skip` 等价，保证既有脚本（CI、文档）不立即失效

### Requirement: 工具链缺失回退

当 dist 缺失且需要构建（release 模式或 debug 显式 force），但 pnpm /
corepack 不可用或构建失败时，构建 SHALL 回退为嵌入占位入口页并以
编译警告提示，MUST NOT 使 `cargo build` / `cargo test` 失败。

#### Scenario: CI 无 Node 环境测试通过

- **WHEN** 在未安装 Node/pnpm 的 CI 环境执行 `cargo test`
- **THEN** 构建成功，嵌入占位页，测试全部通过

### Requirement: HMR 开发工作流

前端开发 SHALL 支持 `pnpm dev`（Vite dev server，固定端口 5273）热更新
模式：dev server SHALL 把后端伺服的 `/api`、`/health` 与 `/ws` 三类路径
代理到 `xkeeper webui` 后端（默认 `127.0.0.1:9877`），代理目标 MUST 可用
环境变量覆盖。前端 `pnpm exec tsc --noEmit`、`pnpm test`、`pnpm build`
MUST 可在 `webui/` 目录下独立执行。

#### Scenario: dev 模式 WebSocket 可用

- **WHEN** 开发者运行 `pnpm dev` 并在浏览器访问 5273 端口，同时后端
  运行于 9877
- **THEN** 页面经 dev server 代理建立的 `/api` 请求与 `/ws` WebSocket
  推送均正常工作

#### Scenario: 代理目标可覆盖

- **WHEN** 以环境变量把代理目标改为非默认后端地址后运行 `pnpm dev`
- **THEN** 请求被代理到新地址

### Requirement: 目录与命名

Web 控制台前端工程 SHALL 位于仓库 `webui/` 目录（含源码、工具链配置、
lockfile 与构建输出 `dist`）；`dist/` 与 `node_modules/` MUST 被版本控制
忽略。文档与构建脚本中对该工程的引用 SHALL 使用 webui 术语。

#### Scenario: 新检出可构建

- **WHEN** 从版本控制新检出仓库并依次执行前端构建与 `cargo build --release`
- **THEN** 无需手工挪动文件即可产出内嵌真实 UI 的二进制
