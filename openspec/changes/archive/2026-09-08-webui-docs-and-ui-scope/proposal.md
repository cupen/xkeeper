# Proposal: webui-docs-and-ui-scope

## Why

`frontend/` 目前的代码骨架（project-rail / sessions / transcript-view / workbench-composer）
整体来自 sebas-webui 模板，描述的是「AI 会话工作台」域，与 xkeeper 的进程守护域完全不匹配；
后端 `/api` 也只有 `GET /api/health` 一个存根。同时 README 对 Web UI 的介绍只有寥寥数行：
技术栈、双进程开发调试（`pnpm dev`）、发布方式（dist 嵌入二进制）都没有说清楚，
界面元素应该长什么样也从未被定义过。本变更先补齐文档与规格，为后续 webui 实现铺路。

## What Changes

- **README 重写 Web UI 章节**，讲清四件事：
  - 技术栈：Vite + TypeScript + Lit（Web Components / Shadow DOM）+ Web Awesome 组件 +
    vitest（happy-dom）+ pnpm；`build.rs` 在 `cargo build` 时自动 `pnpm install` + `pnpm build`，
    产物经 rust-embed 编译进单个二进制。
  - 开发调试：双进程工作流 —— 后端 `cargo run -- webui`（127.0.0.1:9877）+ 前端
    `pnpm dev`（127.0.0.1:5273，Vite 代理 `/api`、`/ws`、`/health` 到后端），热更新下测试；
    无 Node 工具链时的占位页行为与 `XKEEPER_FRONTEND_BUILD=skip`。
  - 发布：`cargo build --release` 产出自带完整 UI 的单文件二进制；dist 不入库，全新
    checkout 由 build.rs 自动构建；可选跳过前端构建的场景。
  - 文档与代码现状对齐：核对端口（后端 9877 / Vite 5273）、代理路径与实际配置一致。
- **定义 xkeeper 域的 Web UI 界面元素清单**（新能力规格 `webui-ui`）：左侧两层
  菜单树（app = xkeeper 实例 → 进程 = `[[program]]`）承载导航，主区按选中项展示
  App 概况（守护进程信息 + 程序汇总）或进程详情（运行信息 + stdout/stderr 日志）、
  状态徽章、单程序控制、实时刷新与断线提示等。
- **删除全部与守护域不匹配的模板遗留元素**（用户明确要求）：
  - 前端：`views/`（project-rail、sessions、session-detail、transcript-view、
    workbench-composer、settings-modal 及测试）、`api/`（client、ws、shared-ws）、
    `components/`（markdown、review-card、folder-picker）、`preview/` 原型目录与
    `preview.html`；
  - 配置：`vite.config.ts` 中指向不存在后端面的 `/gateway`、`/ws` 代理与 9797
    过时端口注释；
  - 后端 `src/` 经核查无会话域残留，无需删除。
  - 保留域中立的骨架设施：router、theme、styles、icons、status-badge（通用
    渲染器），并用一个诚实展示后端健康状态的占位 dashboard 替换原工作台。

## Capabilities

### New Capabilities

- `webui-ui`: xkeeper Web 控制台必须呈现的界面元素与行为需求 —— 左侧两层菜单树
  （app 节点 = 注册的应用（app-registry），子节点 = 该应用的各程序）、选中驱动主区：
  App 概况（配置来源、巡检间隔、uptime、全部程序状态汇总）或进程详情（命令行、
  PID、状态、uptime、重启计数 + stdout/stderr 日志查看器）、running/backoff/
  exited/fatal 状态徽章、单程序控制（start/stop/restart 及确认）、状态实时刷新
  （WebSocket 推送 + 轮询兜底）与后端不可达时的降级展示。这是后续实现变更的
  验收依据。

### Modified Capabilities

（无 —— 仓库 `openspec/specs/` 目前为空，本变更是首个能力规格。）

## Impact

- `README.md`：Web UI 章节重写（技术栈、开发调试、发布、现状标注）。
- `openspec/specs/webui-ui/spec.md`：新增能力规格（通过本变更的 delta 归档产生）。
- `frontend/`：删除上述模板遗留文件；`app-shell.ts`、`main.ts`、`views/dashboard.ts`
  重写为最小占位骨架；`vite.config.ts`、`components/status-badge.ts`、`index.html`
  注释做配套清理。占位骨架不实现 `webui-ui` 规格的界面元素——那是后续变更的工作。
- 后续实现变更将受此规格约束：webui REST/WS API（`src/web.rs`）、前端视图重构
  （`frontend/src/`）。
