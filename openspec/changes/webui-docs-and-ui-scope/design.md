# Design: webui-docs-and-ui-scope

## Context

`frontend/` 骨架与 API client 全部来自 sebas-webui 模板（会话工作台域），
`src/web.rs` 只有 `GET /api/health`。README 已有 Web UI 雏形章节（构建嵌入、
双进程开发、`XKEEPER_FRONTEND_BUILD=skip`），但技术栈、发布流程与界面元素
定义均为空白。状态词汇表以 README「配置参考」注释为准：
`running / backoff / exited / fatal`。

## Goals / Non-Goals

**Goals**

- 把技术栈、`pnpm dev` 双进程调试、发布方式写成 README 里可照做的操作文档。
- 产出 `webui-ui` 能力规格，枚举控制台必备界面元素与可验证场景。

**Non-Goals**

- 不实现任何 API 端点或前端视图（后续变更按规格实施）。
- 不在本变更中删除/改造模板遗留视图（sessions / transcript-view 等）。
- 不改 build.rs / rust-embed 的现有机制，只如实记录。

## Decisions

1. **文档、规格与模板清理同一变更，领域实现拆到后续。**
   用户明确要求把不匹配元素全部删除；先删净会话域代码、把界面元素固化为可验收
   的规格，实现（webui API、视图重构）作为 follow-up 变更引用该规格。备选方案是
   在本变更里直接实现 MVP——被否，因为那会让一个变更同时背文档、API、UI 三摊事，
   评审与回滚都困难。
2. **删除范围以"域"划线，不按"能不能跑"划线。**
   会话工作台域的视图/API client/组件/预览原型全删；域中立设施（history router、
   主题切换、design tokens、图标、通用状态徽章渲染器）保留复用。status-badge 的
   slug 联合类型属于会话域词汇，放宽为后端下发的字符串，避免保留错误词汇表。
   保留清单经 grep 依赖核查（谁 import 谁），删除后 `pnpm test` / `pnpm build`
   必须仍然通过。
3. **占位 dashboard 只做两件事：证明前后端链路通、诚实标注建设中。**
   展示 `GET /api/health` 结果 + 指向 `webui-ui` 规格的"按规格实施中"说明；
   不放任何假数据或假控件。
4. **侧栏采用两层树：app 节点 = 注册的应用（app-registry），子节点 = 该应用的程序。**
   用户方向性反馈"左侧两层菜单选择 app 和 app 下的进程"。修订：app-supervisor
   落地了 app-registry（`xkeeper add/remove/list`，应用即部署目录注册记录），
   第一层直接采用注册应用——与配置模型天然吻合，无需原设想的"实例/分组"解释。
   选中驱动主区（App 概况 / 进程详情 + 日志），守护进程概况在页面级覆盖全部
   应用。
5. **状态词汇表沿用守护循环既有状态，不新增。**
   `webui-ui` 规格只消费 `running/backoff/exited/fatal`，避免规格先行发明
   后端尚不存在的状态（如 starting 过渡态）导致实现期被迫改规格。
6. **实时刷新规格为"WebSocket 优先 + 轮询兜底"。**
   模板的 ws 基建随会话域一并删除；规格允许实现期按成本选择纯轮询起步——
   规格只约束"实时可见 + 断线降级"这一可观察行为。
7. **文档落在 README 单文件，不另起 docs/。**
   项目体量小、README 已是唯一文档入口；拆目录只会增加失同步面。

## Risks / Trade-offs

- [规格先行，实现滞后导致规格与真实 API 演化脱节] → 实现变更中若必须调整
  需求，用 `openspec-update-change` / 新 delta 同步改规格，禁止"代码对了
  规格不改"。
- [占位骨架期，控制台无可用功能] → 占位页如实展示后端健康与建设中说明；
  实现变更按 `webui-ui` 规格逐条落地，避免长期停留在占位态。

## Open Questions

（无。）
