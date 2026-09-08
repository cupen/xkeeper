# Design: rename-frontend-to-webui

## 现状盘点（已完成调研）

- `build.rs` 现有逻辑：监听 `frontend/{src,index.html,package.json,
  pnpm-lock.yaml,vite.config.ts,tsconfig.json,dist}`；mtime 判断 staleness；
  `pnpm install --frozen-lockfile`（按需）+ `pnpm build`；无 Node 时写占位页；
  `XKEEPER_FRONTEND_BUILD=skip|0|false` 跳过；dist 内容 stamp 经
  `cargo:rustc-env=XKEEPER_FRONTEND_STAMP` 触发重编译。
- `src/assets.rs`：`#[folder = "$CARGO_MANIFEST_DIR/frontend/dist"]`。
- `frontend/vite.config.ts`：dev 端口 5273（strictPort），代理 `/api`、
  `/health` → 127.0.0.1:9877；**未代理 `/ws`**（dev 模式 WS 推送断链，
  本次补上）。
- `frontend/package.json` name 为 `xkeeper-webui-frontend`。
- `.gitignore` 含 `/frontend/node_modules`、`/frontend/dist`。
- CI（`.github/workflows/ci.yml`）仅 `cargo test`，无 Node 设置 —— 依赖
  占位回退保持绿色，本次不需改 CI。
- `webui` 的 dist 目录本身已在 `.gitignore`（dist 不提交），git mv 只迁移
  源文件。

## 方案

### 1. 目录迁移

`git mv frontend webui`（保留历史）。注意 git status 中已有未提交改动的
文件不存在于 frontend/ 下，无冲突。`package.json` name 改为
`xkeeper-webui`。

### 2. build.rs 重构

- 路径：所有 `frontend` → `webui`（含 rerun-if-changed 列表、inputs、
  dist 路径、注释）。
- 模式判定：
  ```
  let profile = std::env::var("PROFILE").unwrap_or_default(); // "debug"/"release"
  let mode = match env XKEEPER_WEBUI_BUILD:
      "skip"|"0"|"false" => Skip,
      "force"            => Force,       // debug 下也构建
      未设置              => if profile == "debug" { Skip } else { Auto },
  ```
  - `Skip`：不调用 pnpm；dist 缺失时写占位页（沿用现有 `skip_requested`
    分支逻辑）。
  - `Auto` / `Force`：现有 staleness 检查 + 构建流程；失败且无 dist 时
    占位回退（不变）。
- 兼容：`XKEEPER_FRONTEND_BUILD` 仍被读取（优先级低于新变量），保证
  既有脚本不立即失效；文档只宣传新变量。
- 占位 marker：`.xkeeper-placeholder` 语义不变——Skip 模式下若磁盘已有
  真实 dist 则直接嵌入真实 dist（与现状一致）。
- stamp：变量名同步改为 `XKEEPER_WEBUI_STAMP`（纯内部，无外部契约）。

关键取舍：debug 默认 Skip 而不是"增量构建"——mtime 检查在大型 monorepo
易误判且 pnpm build 本身秒级但 install 可能分钟级；Rust 调试场景
（改 supervisor.rs 反复 cargo build）完全不需要 UI。需要真 UI 时一次
`XKEEPER_WEBUI_BUILD=force cargo build` 或手动 `pnpm build` 即可
（手动 build 产物会被 Skip 模式直接采用）。

### 3. assets.rs / web.rs

- `#[folder]` 改为 `$CARGO_MANIFEST_DIR/webui/dist`；头注释同步。
- 全仓 `grep -rn "frontend"` 清理剩余引用（src/、README、AGENTS.md、
  .gitignore、build.rs 注释）。

### 4. Vite dev 代理补 /ws

`webui/vite.config.ts`：

```ts
const backend = process.env.XKEEPER_WEBUI_DEV_BACKEND ?? 'http://127.0.0.1:9877'
// proxy: '/api'、'/health'（http），'/ws'（ws: true）
```

`/ws` 代理需 `ws: true`；binary msgpack 帧经 http-proxy 透传无额外配置。

### 5. 文档

- `AGENTS.md` 架构/构建章节路径与命令更新（frontend → webui，debug 说明）。
- `README.md` webui 构建章节同步。

## 风险与对策

| 风险 | 对策 |
|---|---|
| rerun-if-changed 漏改导致 dist 变更不重嵌入 | tasks 中显式核对清单；用"改 dist 内文件→重编译触发"手动验证 |
| debug Skip 误伤"刚手动 pnpm build 完想立刻看 UI" | Skip 模式沿用磁盘 dist（非占位即用）；文档说明 |
| 旧环境变量静默失效 | 保留 `XKEEPER_FRONTEND_BUILD` 兼容读取 |
| git mv 后 pnpm-lock 路径变化致 CI install 判定 | install 判定基于 webui/ 下相对路径，无绝对路径；无需处理 |
| Windows 路径差异 | build.rs 全部经 `Path::join`，无硬编码分隔符；现有代码已如此 |

## 验证方式

1. `cd webui && pnpm exec tsc --noEmit && pnpm test && pnpm build`
2. `cargo build`（debug，无 dist）→ 占位页、不调 pnpm
3. `XKEEPER_WEBUI_BUILD=force cargo build` → 真实 UI 嵌入
4. `cargo build --release` → 真实 UI 嵌入
5. `pnpm dev` + `cargo run -- webui` → HMR 页面 /api 与 /ws 均通
