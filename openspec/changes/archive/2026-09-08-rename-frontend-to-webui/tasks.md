# Tasks: rename-frontend-to-webui

## 1. 目录迁移

- [x] 1.1 `git mv frontend webui`，确认 `webui/` 下源文件齐全（src、
  index.html、package.json、pnpm-lock.yaml、vite.config.ts、tsconfig.json）；
  验证：`git status` 显示 rename，`ls webui` 与迁移前 `ls frontend` 一致
- [x] 1.2 `webui/package.json` name 改为 `xkeeper-webui`；验证：
  `cd webui && pnpm install --frozen-lockfile && pnpm exec tsc --noEmit && pnpm test`

## 2. 构建脚本（build.rs）

- [x] 2.1 build.rs 所有路径与注释 frontend → webui（rerun-if-changed、
  inputs、dist、文档注释）；验证：`grep -n frontend build.rs` 无结果
- [x] 2.2 引入模式判定：读 `PROFILE` 与 `XKEEPER_WEBUI_BUILD`
  （skip/0/false→Skip；force→Force；未设置→debug Skip、release Auto），
  保留 `XKEEPER_FRONTEND_BUILD` 兼容读取（低优先级）；验证：
  debug 无 dist 构建不调 pnpm 且嵌占位页，`XKEEPER_WEBUI_BUILD=force
  cargo build` 嵌真实 UI
- [x] 2.3 stamp 环境变量改名 `XKEEPER_WEBUI_STAMP`；验证：改 `webui/src`
  下文件后 `cargo build` 触发相关 crate 重编译
- [x] 2.4 `src/assets.rs` embed folder 改为 `$CARGO_MANIFEST_DIR/webui/dist`
  并同步头注释；验证：`cargo build` 后 `cargo run -- webui --listen
  127.0.0.1:19877`，浏览器访问首页加载控制台

## 3. Vite dev 代理

- [x] 3.1 `webui/vite.config.ts`：代理目标支持
  `XKEEPER_WEBUI_DEV_BACKEND` 环境变量（默认 127.0.0.1:9877），新增 `/ws`
  代理（`ws: true`）；验证：`pnpm dev` + `cargo run -- webui`，浏览器
  5273 端口页面 /api/health 探测通过且 WS 心跳可见

## 4. 配套与文档

- [x] 4.1 `.gitignore` 更新为 `/webui/node_modules`、`/webui/dist`；验证：
  `git status` 不再出现 webui/dist 或 webui/node_modules
- [x] 4.2 `AGENTS.md`、`README.md` 中 frontend 路径、命令与 debug 构建说明
  同步；验证：`grep -rn frontend README.md AGENTS.md` 无残留（历史名词
  除外需显式豁免）

## 5. 集成验证

- [x] 5.1 四条构建路径全过：debug 无 dist（占位、无 pnpm）、
  `XKEEPER_WEBUI_BUILD=force` debug（真实 UI）、release（真实 UI）、
  `XKEEPER_WEBUI_BUILD=skip`（沿用磁盘 dist）；验证：逐条记录构建输出
- [x] 5.2 无 Node 模拟（PATH 去掉 pnpm/corepack）`cargo test` 通过且占位
  回退生效；验证：CI 本地模拟或 `env PATH=... cargo test`
- [x] 5.3 新检出演练：`git stash`/clean 副本上 `pnpm install && pnpm
  build && cargo build --release` 产出内嵌真实 UI 的二进制；验证：二进制
  启动后首页可访问
