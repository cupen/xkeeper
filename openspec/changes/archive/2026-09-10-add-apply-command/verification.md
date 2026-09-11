# Scenario 核对记录 — add-apply-command（任务 4.2）

日期：2026-09-09 · 验证方式：`cargo test`（93 单测/集成）+ `cargo test -- --test-threads=1`
（稳定性 4/4）+ webui `pnpm exec tsc --noEmit && pnpm test`（56）+ 真实二进制端到端
走查（`xkeeper run` / `xkeeper webui`，详见下文「端到端」）。

## apply-workflow（新能力）

| Scenario | 验证 | 结果 |
|---|---|---|
| 配置修改只进入 pending | `reload_detects_without_touching_programs`（pid/状态不变）+ 端到端 sed 改配置后 `/v1/pending` 出现 | ✓ |
| 周期检出免手工 reload | `periodic_detect_publishes_pending`（无命令、直接改盘）+ 端到端 | ✓ |
| 单个 app 文件损坏不影响其他检出 | `detect_isolates_broken_app_file` + `registry::list` broken 标记 | ✓ |
| 跨应用校验失败不形成 pending | `detect_refuses_cross_app_conflict`（pending 保持空） | ✓ |
| 无变更时 apply 幂等 | `apply_with_no_changes_is_idempotent` + 端到端 rc=0 | ✓ |
| 单 app 范围不波及其他 | `scoped_apply_does_not_touch_other_apps`（B 的 pid 不变、pending 保留） | ✓ |
| 新注册 app 随 apply 启动 | `apply_starts_newly_registered_app` + 端到端 add→apply→q 拉起 | ✓ |
| apply 后 pending 清空 | 同上两个测试断言 pending 为空 | ✓ |
| 变更程序原先在跑则重启 | `apply_updates_and_restarts_running_program`（pid 变化） | ✓ |
| 变更程序原先停止则保持停止 | `apply_redefines_but_keeps_user_stopped`（update-only，定义换、进程不拉） | ✓ |
| --restart 跳过手动停止的程序 | `apply_restart_flag_respects_user_stops`（up 重启 / down 保持 stopped / keep-stopped 条目）+ 端到端 | ✓ |
| --restart 无 pending 也重启 | 同上测试（配置无变更、纯 --restart 路径） | ✓ |
| --restart 拉起 backoff | `apply_restart_flag_pulls_up_backoff_program` | ✓ |
| 结果区分三类程序 | `render_apply_groups_by_action`（changed/restarted/untouched 分组）+ 端到端输出 | ✓ |
| API 结果结构化 | `/v1/apply` JSON result（端到端）；`ProgramAction{changed,action,result}` serde 同构 | ✓ |

## configuration（修改）

| Scenario | 验证 | 结果 |
|---|---|---|
| 单个 app 文件损坏不影响其他 | 同 detect_isolates（keep old config 断言） | ✓ |
| reload 只检出不应用 | `reload_detects_without_touching_programs` + 端到端 reload 预览 | ✓ |
| 不可热更字段提示 | detect/apply 的 daemon_hints（port/host/auth 比对分支，单测覆盖 detect 路径） | ✓ |
| 移除注册立即生效 | 端到端 add/apply 流程；remove → pending.apps_removed → apply 停止并移除（apply_removes_dropped_program 覆盖程序级移除） | ✓ |

## control-plane（修改）

| Scenario | 验证 | 结果 |
|---|---|---|
| reload 返回 pending 预览 | 端到端 `/v1/reload` 响应含 result+pending | ✓ |
| pending 可独立查询 | 端到端 `GET /v1/pending` | ✓ |
| apply 带范围与 restart 参数 | 端到端 `/api/apply {"app":"demo"}`；404 范围校验在 server 代码路径 | ✓ |
| apply 无变更幂等退出 | 端到端 CLI apply rc=0 | ✓ |
| 状态汇总/非法转换/404/shutdown/守护未运行/停止/跟随日志 | 既有行为未动，既有测试覆盖 | ✓（未回归） |

## shell-client（修改）

| Scenario | 验证 | 结果 |
|---|---|---|
| 在 shell 中应用变更 / 查看 pending | `aliases_and_basic_commands` 解析测试 + shell -e 端到端（pending/apply demo） | ✓ |
| 其余 shell 场景 | 未触碰，既有测试 | ✓（未回归） |

## app-registry（修改）

| Scenario | 验证 | 结果 |
|---|---|---|
| 在线同步失败不静默 | `reload_failure_while_online_is_an_error`（断言更新为 "failed to rescan"） | ✓ |
| 在线注册进入 pending 而非立即生效 | 端到端 add → 「registration is pending」→ apply 后拉起 | ✓ |
| 离线批量注册后启动 | 未触碰（bootstrap 行为不变） | ✓（未回归） |

## webui-api（修改）

| Scenario | 验证 | 结果 |
|---|---|---|
| pending 与控制面一致 | `ws_status_delta_carries_pending_changes`（/api/pending 与 overview.pending 同源） | ✓ |
| apply 结果与控制面一致 | 同一 `Command::Apply` 队列路径；store.apply 测试断言请求体 | ✓ |
| pending 变化推送 | WS STATUS 帧带 `pending` 字段（web.rs 改动 + store 测试） | ✓ |
| 快照含 pending | StatusDoc.pending 三处同源（/v1/status、/api/overview、WS snapshot） | ✓ |
| 其余场景（日志/压缩/404 等） | 未触碰，既有测试 | ✓（未回归） |

## webui-ui（新需求）

| Scenario | 验证 | 结果 |
|---|---|---|
| pending 徽章出现 | overview-bar pending 条（count 徽章 + 一行摘要）；嵌入 dist 含「待应用变更/Apply 全部」（curl 验证 6 处命中） | ✓ |
| App 级 apply 不波及其他 | app-overview「Apply 此应用」按 app 传 scope；store.apply 测试断言 body | ✓ |
| apply 结果可读 | 结果通知逐行（apply-result / feedback 区域） | ✓（代码路径；人工浏览器走查留待归档前手测） |
| apply 失败有反馈 | applyError 分支 data-error=true | ✓（代码路径） |

## 端到端走查（真实二进制）

1. 空 apply → `no changes`，rc=0。
2. 改盘 sleep 300→600 → 一个周期内 `/v1/pending` 出现 `demo.p [running]`。
3. `xkeeper reload` → 预览三行，pid 不变。
4. `xkeeper apply` → `changed: demo.p -> update-and-restart`，pid 变化，pending 清空。
5. `stop p` 后 `apply --restart` → `untouched: demo.p (keep-stopped)`，状态保持 stopped。
6. `shell -e pending` / `shell -e apply demo` 正常。
7. `add app2`（在线）→ 「registration is pending」→ `apply` → `app2.q -> start`，q 获得.pid。
8. webui 模式：`/api/pending`、`/api/overview{pending}`、`POST /api/apply {"app":"demo"}`
   三处同源一致；嵌入 JS 含新 UI 文案。

## 验收 e2e（2026-09-10 增补）

`cargo run -p xtask -- e2e`（xtask 新子命令）驱动**真实守护进程二进制**把上面的手工
验收清单固化成可重复执行的用例，~6s：

- **CLI 段（A–E）**：幂等 apply → 改盘周期检出（进程不动）→ reload 仅预览 →
  `apply alpha` 范围隔离（beta pending 保留、job pid 不变）→ `stop` 后
  `apply --restart`（web 保持 stopped、job 重启）→ 在线 `add gamma` 进 pending →
  apply 拉起 → `shell -e pending/apply`。
- **浏览器段（F）**：playwright（chromium）打开内嵌控制台——徽章出现（磁盘编辑后）、
  alpha 视图 apply strip 与 changed 行标记、对话框确认后结果通知含
  `update-and-restart`、scoped apply 后徽章清空。`--no-browser` 可跳过（无 node/
  playwright 环境）；`--keep` 保留工作区现场。

配套的 webui 组件级断言补在 `webui/src/components/apply-ui.test.ts`
（4 测试：徽章出现/清空、拒绝确认不发起请求、范围隔离、失败反馈）。

### e2e 过程中发现并修复的缺陷

1. **WS 快照编码 bug（上一变更遗留）**：`web.rs` 快照帧用 `rmp_serde::to_vec`
   （compact tuple）编码 `StatusDoc`，浏览器端 msgpack 解出数组，字段访问抛错且被
   解码链静默吞掉——WS transport 永远停在 connecting，控制台实际全靠轮询降级在跑。
   修复为 `to_vec_named`（map 编码，与 JSON 语义同构，符合 webui-api 数据格式契约）。
2. **apply 结果条遮蔽徽章**（webui-ui）：apply 结果存在时 pending 容器不消失，
   已清空的徽章跟着残留。修复为 pending 条与结果通知分离渲染。
3. `app-overview` 视图 store 硬编码单例，无法注入测试——改为可注入（与
   program-detail/overview-bar 同模式）。

## 已知限制 / 备注

- `--restart` 对 fatal 程序的处理：按「手动停止同等待遇」保持停止（fatal 亦不
  自动拉起），与 spec「手动停止的程序（stopped/exited/fatal）保持停止」一致。
- WS STATUS 帧的 pending 载荷使用 JSON 文本（{programs, pending}），与
  MessagePack 快照并存；ws-client 解码兼容旧版纯数组。
- edit 测试在并行高负载下偶发 flake（与本次变更无关），已用模块级锁串行化，
  之后 8 轮全量稳定通过。
- WS STATUS 增量帧载荷为 JSON 文本（{programs, pending?}），快照帧为 named
  MessagePack；浏览器 ws-client 对旧式纯数组 STATUS 载荷保留解码兼容。
- e2e 依赖本机已有 playwright（npx 缓存目录），xtask 不引入 node 依赖；无浏览器
  环境时 `--no-browser` 仍可跑 CLI 段。
