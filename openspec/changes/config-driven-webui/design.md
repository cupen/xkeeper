## Context

现状：`xkeeper webui --listen` 与 `xkeeper run` 是两个并列守护入口（`run_daemon(config, Option<listen>)`），webui 线程仅在启动时按 CLI 参数 spawn 一次；`web::serve` 的 graceful shutdown 只轮询守护的 shutdown 标志（Ctrl+C / `POST /v1/shutdown`），无外部开关。`xkeeper reload` = `cmd_reload()` = app 注册表重扫进 pending，不碰 daemon 配置运行态。`DaemonConfig` 为 `deny_unknown_fields`，`[webui]` 段目前不存在——先写该段而守护不认识会让整份配置非法，故本 change 必须在 add-config-subcommand（提供 `config --delete` 与键表机制）落地之后实施。控制面（:7310，`server::bind` 先于 bootstrap 的 fail-fast 顺序）是事实上的单实例保护，webui 端口绑定失败目前即为非致命（线程内记日志）。

## Goals / Non-Goals

**Goals:**

- 控制台启停完全由 daemon.toml `[webui]` 段驱动：`run` 启动时读段伺服，`reload` 热生效（开/关/换址重绑）。
- `run` 成为唯一守护入口；删除 `webui` 与 `system webui`（含 `system` 组）。
- `[webui]` 段纳入 config 子命令键表：`--set webui.listen` / `--get webui.listen` / `--delete webui` 与 `[daemon]` 键同一套机制。

**Non-Goals:**

- 不给控制面 `/v1` 新增 webui 管理端点（启停只走 config + reload）。
- 不做 `[daemon]` 其余字段的热更（维持既有 spec 语义：尽量热更 + 需重启提示；本次只新增 `[webui]` 例外）。
- 不为控制台新增鉴权键或访问控制（沿用现状：缺省 loopback）。
- 不改 WS 帧格式、SPA 资产、service unit 模板。

## Decisions

- **D1 配置形态 = 表存在即启用（presence-based）**：`[webui]` 段存在即伺服，`listen` 缺省 `127.0.0.1:9877`；`--delete webui` 删整表即关闭。用户拍板；否掉 `enabled` 布尔键（多一个键、删除语义不干脆）。`WebuiSettings { listen: String }` 走 serde default，段内 `deny_unknown_fields` 与 `[daemon]` 一致。
- **D2 reload 热更边界 = 只动 `[webui]` 段**：`cmd_reload` 在重扫注册表的同时重读 daemon.toml，计算 webui 意图（enabled? listen?）并与运行态收敛；`[webui]` 变更立即执行、MUST NOT 进 pending；其余 `[daemon]` 字段差异维持「尽量热更 + 需重启提示」既有语义。`[webui]` 段非法时本轮 reload 整体失败（保持既有伺服状态），与「跨应用校验失败整体不形成 pending」的隔离哲学一致。
- **D3 webui 运行时控制 = Supervisor 上的意图句柄 + 管理线程收敛**：`Supervisor` 挂一个 webui 控制句柄（期望状态：enabled + listen；`arc_swap`/`Mutex` + notify），`run_daemon` 常驻一个 webui 管理线程：对比期望与实际状态，需要时 spawn `web::serve`（tokio runtime 线程）或通知其退出。`web::serve` 的 graceful shutdown 条件扩展为「守护 shutdown 标志置位 ∥ 收到停止/重绑信号」；WS 推送循环在退出前发关闭帧。重绑 = 停旧（graceful）→ 起新；旧实例退出超时（秒级）则记日志放弃等待，避免 reload 卡死。否掉「axum 内嵌 watch channel 直接驱动」——管理线程把收敛逻辑收在守循环一侧，reload 路径无需理解 tokio。
- **D4 CLI 删除 = 无别名直接移除**：删 `Cmd::Webui`、`SystemCmd`/`Cmd::System`、`shell::system_webui()`（连带 `listen_of`/`webui_health`/`open_browser` 仅剩的调用方）。用户拍板 BREAKING；仓库尚处 v0.1，README/AGENTS 同步即可，不做 deprecation 过渡。
- **D5 端口占用降级保持现状**：webui bind 失败 → 错误日志 + 守护继续（启动与 reload 一致）；管理线程在下次意图变化时自然重试，不做常驻重试定时器。
- **D6 e2e 启动方式替换**：webui pass（playwright 段）改为临时 daemon.toml 写 `[webui]` + `xkeeper run`；新增非浏览器场景「config --set → reload → /api/health 可达」「--delete webui → reload → 连接拒绝」。浏览器段场景本身不变（webui-ui spec 零改动）。
- **D7 systemd 路径零改动**：unit `ExecStart=xkeeper run` 不变，服务模式经 `config --set` + `xkeeper reload` 获得控制台；service-registration spec 无 delta。

## Risks / Trade-offs

- [reload 路径新增失败面（[webui] 段非法拖垮整轮 reload）] → 与「配置错误在加载期暴露」哲学一致；错误信息指明 `[webui]` 字段，守护保持旧伺服状态不抖动。
- [WS 连接在停旧/重绑窗口的体验] → graceful 关闭帧 + 秒级超时上限；前端已有断线重连，重绑后可自动恢复。
- [管理线程与守护 shutdown 的竞态] → 收敛判断统一读共享 shutdown 标志与意图句柄；web::serve 退出不再守护阻塞主循环（现状即为后台线程）。
- [--get webui.listen 在关闭态回退默认值] → 与 --get 对缺失文件回退默认的语义一致；「enabled」状态可用 `--get` 之外的方式（查看文件）判断，不在键值语义里混入。

## Migration Plan

BREAKING：`xkeeper webui`/`xkeeper system webui` 用户改用 `config --set webui.listen=...`（+`reload`）或配置 `[webui]` 段。回滚 = revert 提交。无数据迁移；既有无 `[webui]` 段的配置行为不变（不伺服控制台）。

## Open Questions

（无——影响 spec/方案/任务拆分的点已在拷问轮收束；用户拍板记录于 D1/D2/D4。）
