## Why

`webui`/`system webui` 把「是否要控制台」绑死在进程启动那一刻：`run` 起的守护永远没有控制台（webui 不是附着客户端，第二次启动只会撞端口退出），想要控制台只能停掉重启；`system webui` 为补救而生的拉起/探测逻辑又叠加了一条隐式启动路径。用户已拍板：控制台是 daemon 的一种可配置运行形态，不该是独立子命令——改为配置驱动（`[webui]` 段开关），由 `xkeeper reload` 动态生效，`run` 成为唯一守护入口。

## What Changes

- **BREAKING** 删除 `xkeeper webui` 子命令（`--listen` 参数随之消失）；删除 `xkeeper system webui` 与整个 `system` 子命令组及 `shell::system_webui` 拉起/探测逻辑。无兼容别名。
- daemon 配置新增可选 `[webui]` 段：**表存在即启用控制台**，`webui.listen` 缺省 `127.0.0.1:9877`；表不存在即关闭。`config --set webui.listen=<addr>` 写入即开启、`config --delete webui` 删整表即关闭（依赖 add-config-subcommand 先行落地 `--delete` 机制，本 change 将 `[webui]` 键纳入其键表）。
- `xkeeper run` 启动时读 `[webui]` 决定是否伺服控制台；`xkeeper reload` 重读该段并**立即热生效**：关闭→停伺服（WS 连接 graceful 关闭）、开启→起伺服、`listen` 变更→停旧起新重绑；`[daemon]` 其他字段变更仍按既有语义仅提示需重启。
- webui 端口绑定失败保持非致命：记录错误日志，守护继续运行（无控制台）。
- systemd 服务方式天然获益：unit 的 `ExecStart=xkeeper run` 不变，`config --set webui.listen=...` + `xkeeper reload` 即可在服务模式下获得控制台。
- e2e webui 段启动方式改为「daemon.toml 写 `[webui]` + `run`」，并新增 reload 动态开关场景；README/AGENTS 同步删除两个子命令引用。

## Capabilities

### New Capabilities

（无。）

### Modified Capabilities

- `webui-api`: **修改**「与守护进程同进程共存」（`run` 成为唯一守护入口、控制台随配置启停）；**新增**「配置驱动的控制台生命周期」（`[webui]` 启停/重绑/reload 热生效/端口占用降级）。
- `configuration`: **修改**「daemon 根配置」（新增可选 `[webui]` 段）、**修改**「校验规则」（`webui.listen` 合法性）、**修改**「按应用粒度的热更新」（`[webui]` 段 reload 立即热生效的例外）、**新增**「webui 配置段」（config 子命令对 `[webui]` 的 set/get/delete 管理语义）。
- `control-plane`: **移除**「shell 与 system 子命令」并以「shell 子命令」替代（system 组消失）；CLI 控制命令集合不变。
- `shell-client`: **移除**「system webui 辅助命令」。

## Impact

- `src/main.rs`：删除 `Cmd::Webui`、`SystemCmd`/`Cmd::System` 分发分支；`run_daemon` 装配改为按配置（而非 CLI 参数）传递 webui 意图。
- `src/shell.rs`：删除 `system_webui()`、`listen_of()`、`webui_health()` 等辅助。
- `src/web.rs`：`serve()` 增加外部控制信号——守护退出之外，支持「被关闭」「换址重绑」触发的 graceful 停机；WS 连接随停机 graceful 关闭。
- `src/supervisor.rs`：`cmd_reload` 扩展——重读 daemon.toml 的 `[webui]` 段并与运行态收敛（开/关/重绑）；其余 `[daemon]` 字段差异维持「需重启」提示。
- `src/config.rs`：新增 `WebuiSettings`（`listen`，serde deny_unknown_fields）与校验；config 子命令静态键表扩入 `webui.listen`。
- `xtask/src/e2e.rs`：两处 `webui --listen` 启动改为配置驱动；新增 reload 开关场景。
- `openspec/specs/webui-api|configuration|control-plane|shell-client` 归档时合入上述增删改。
- 依赖顺序：**add-config-subcommand 必须先于本 change 实施**（`--delete` 机制；且 `deny_unknown_fields` 下先写 `[webui]` 而守护不认识该段会让整份配置非法）。
- 不影响：控制面 `/v1` 路由与鉴权、WS 帧格式、SPA 资产与构建（webui-build/webui-ui 零改动）、service-registration（unit 模板不变）。
