# Tasks: webui-docs-and-ui-scope

## 1. 删除域不匹配的模板遗留元素

- [x] 1.1 删除 `frontend/src/views/` 全部会话工作台视图及其测试
  （dashboard、project-rail、sessions、session-detail、transcript-view、
  workbench-composer、settings-modal）。验证：`pnpm test` 通过（先删配套
  引用再跑）。
- [x] 1.2 删除 `frontend/src/api/`（client、ws、shared-ws 及测试）——API
  client 全部是 SessionRow/会话域形状。验证：全仓 grep 无 `api/client.js`
  等残留引用。
- [x] 1.3 删除 `frontend/src/components/` 中会话域组件：markdown、
  review-card、folder-picker（含测试）。验证：同上，无残留引用。
- [x] 1.4 删除 `frontend/src/preview/` 原型目录与 `frontend/preview.html`。
  验证：`pnpm build` 不再产出 preview 入口。
- [x] 1.5 清理 `frontend/vite.config.ts`：删除指向不存在后端面的 `/gateway`
  与 `/ws` 代理，修正注释中 9797 → 9877；保留 `/api`、`/health` 代理。
  验证：`grep -rn "9797\|gateway" frontend/ src/`（node_modules 除外）无残留。
- [x] 1.6 放宽 `frontend/src/components/status-badge.ts` 的 slug 联合类型为
  后端下发的字符串（会话域词汇表不留存），组件渲染逻辑不动。验证：a11y
  测试仍通过。
- [x] 1.7 重写 `frontend/src/app-shell.ts`、`main.ts`、`views/dashboard.ts`：
  保留 router/theme/icons 的最小外壳，单路由占位 dashboard 展示
  `GET /api/health` 结果与"按 webui-ui 规格建设中"说明；app-shell 测试同步
  重写。验证：`pnpm test` 全绿；启动 webui 后占位页显示后端健康状态。

## 2. README Web UI 章节重写

- [x] 2.1 重写「技术栈」小节：Vite + TypeScript + Lit（Web Components /
  Shadow DOM）+ Web Awesome + vitest（happy-dom）+ pnpm，以及 build.rs 自动
  构建、rust-embed 嵌入单二进制的链路。验证：不写代码的读者能从这一节答出
  "前端用什么写的、怎么进到二进制里"。
- [x] 2.2 写「开发调试」小节：双进程流程（`cargo run -- webui` 于 127.0.0.1:9877；
  `cd frontend && pnpm install && pnpm dev` 于 127.0.0.1:5273，Vite 代理
  `/api` `/health`），`pnpm test`、`pnpm build` + `cargo build` 联动，无 Node
  工具链时的占位页行为与 `XKEEPER_FRONTEND_BUILD=skip`。验证：照文档逐步执行
  `pnpm dev` 能打开 5273 并代理到 9877 的 `/api/health`。
- [x] 2.3 写「发布」小节：`cargo build --release` 产出内嵌完整 UI 的单文件
  二进制；dist 不入库、全新 checkout 由 build.rs 自动构建；跳过前端构建的
  场景与代价。验证：发布命令可直接复制执行且与 build.rs 实际行为一致。
- [x] 2.4 现状标注：`frontend/` 为最小占位骨架（模板会话域代码已删除），
  界面元素按 `webui-ui` 规格在后续变更中实现。验证：文档读者不会把占位页
  误认为已交付能力。

## 3. 一致性核对与收尾

- [x] 3.1 核对 README 中所有端口/路径与代码一致：后端 9877（`src/main.rs`）、
  Vite 5273（`frontend/vite.config.ts`）；发现不一致以代码为准修正文档。
  验证：README 中每个端口、路径都能在代码里找到出处。
- [x] 3.2 `openspec validate webui-docs-and-ui-scope --strict` 通过；通读
  README 四块信息互不矛盾；`pnpm test`、`pnpm build`、`cargo build` 全绿。
