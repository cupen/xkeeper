## 1. 配置层：`[webui]` 段

- [x] 1.1 config.rs 新增 `WebuiSettings`（`listen: String`，serde default `127.0.0.1:9877`，段内 `deny_unknown_fields`）挂到 `DaemonConfig`；校验规则：`webui.listen` 非空且可解析为 `host:port`（端口 1–65535），非法逐条报错；单测覆盖缺省回退、合法/非法 listen、段内未知键拒绝
- [x] 1.2 config 子命令静态键表扩入 `webui.listen`（类型解析器 = host:port 字符串、默认值同上；`--get webui.listen` 段未配置回退默认；`--delete webui` 命中整表删除——D8 机制已就绪）；单测覆盖 set/get/delete 三动作与既有注释保持
- [x] 1.3 validate 子命令与 `config --init` 不受影响确认：init 模板不渲染 `[webui]` 段（保持可选段缺省关闭）；单测断言 init 产物 load 通过且无 webui 段

## 2. webui 运行时：意图句柄与动态伺服

- [x] 2.1 supervisor 增加webui 控制句柄（期望状态 enabled+listen，`Mutex`/`arc_swap` + notify）与 `run_daemon` 常驻管理线程：对比期望与实际，spawn/停 `web::serve`；单测覆盖「无段不伺服」「有段伺服」（实现：`WebuiControl` Mutex+Condvar 世代计数，`webui_manager_loop` 常驻线程；未引入 arc_swap，Mutex 足够且不绑定 tokio runtime，见 design D3）
- [x] 2.2 `web::serve` graceful shutdown 扩展：守护 shutdown 标志 ∥ 外部停止/重绑信号；WS 推送循环退出前发关闭帧；单测模拟停机信号下 WS 连接 graceful 断开（实现为 `WebuiLifecycle`：stop 标志 + 存活 WS 会话计数，停伺服时排空等待至多 3s 再关闭，保证 Close 帧先于 socket 销毁发出）
- [x] 2.3 换址重绑与降级路径：停旧（秒级超时放弃等待）→ 起新；bind 失败记错误日志继续运行，意图下次变化时重试；单测覆盖重绑成功与端口占用降级

## 3. reload 热生效

- [x] 3.1 `cmd_reload` 扩展：重读 daemon.toml `[webui]` 段，意图变化写入控制句柄并等待管理线程收敛结果（成功/失败信息进 reload 输出）；`[webui]` 段非法时本轮 reload 失败、伺服状态不变；单测覆盖开→关、关→开、listen 变更、非法段失败
- [x] 3.2 `[daemon]` 其余字段差异维持既有提示语义（需重启/尽量热更），reload 输出不回归；单测回归 `port` 变更提示

## 4. CLI 删除（BREAKING）

- [x] 4.1 main.rs 删除 `Cmd::Webui`、`SystemCmd`/`Cmd::System` 与分发分支；`run_daemon` 改为从启动配置读 webui 意图（不再接收 CLI listen 参数）；全库 grep 无 `Cmd::Webui`/`system webui` 残留；`cargo build` 过
- [x] 4.2 shell.rs 删除 `system_webui()`/`listen_of()`/`webui_health()` 及仅剩调用方的辅助；单测清理并保持 shell 相关覆盖不变（偏差：`webui_health` 保留——已归档 shell-client 规范的 `open` 动词场景要求先 `GET /api/health` 探活再启动浏览器；`open_browser` 并回 `open_webui` 删除）

## 5. e2e 与文档

- [x] 5.1 xtask e2e：webui pass 启动改为临时 daemon.toml `[webui]` + `xkeeper run`；新增场景「config --set webui.listen → reload → /api/health 可达」「config --delete webui → reload → 连接拒绝」；`cargo run -p xtask -- e2e` 全绿（playwright 浏览器段由整体验收执行，本环境以 `--no-browser` 验证其余全绿；另新增 W1 删段关停 / W2 设新址热开 / W3 还原重绑三段）
- [x] 5.2 README/AGENTS.md：删除 `xkeeper webui`/`xkeeper system webui` 引用，新增 `[webui]` 配置段说明与「config --set + reload 开关控制台」用法；架构行更新（CLI 入口清单）
- [x] 5.3 双平台检查：`cargo check`（unix）与 Windows 目标交叉编译检查（`cargo check --target x86_64-pc-windows-msvc` 或 CI 等价）；确认 service unit 生成（`render_unit`）不受影响（Windows 目标 std 未装于本机且无 rustup 可加装，以 unix check 通过 + 本次改动零新增 `#[cfg(windows)]` 路径走查代替；`render_unit` 已是 `run --config`，单测回归通过）

## 6. 前置依赖确认

- [x] 6.1 确认 add-config-subcommand 已实施并归档（`--delete` 机制与键表就绪）；未归档则本 change 停在 1.2 之前不得继续（实施已合入主线提交 e130c23 并按上游指示继续；openspec 归档动作尚未执行，归档时由主流程处理）
