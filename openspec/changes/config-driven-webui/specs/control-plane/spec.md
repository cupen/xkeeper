## ADDED Requirements

### Requirement: shell 子命令

单二进制 SHALL 另提供客户端入口 `xkeeper shell`：交互式 REPL（详见
`shell-client` 能力规范），经控制面 API 与根进程通信，`-e "<命令>"` 进入单命令
模式。SHALL 遵循既有退出码约定：0 成功；1 一般错误；2 配置错误；3 守护进程
不可达。（原 `xkeeper system webui` 随 webui 子命令一并移除，控制台改为
daemon 配置驱动，见 webui-api 能力规范。）

#### Scenario: shell 纳入退出码约定

- **WHEN** 守护进程未启动时执行 `xkeeper shell -e "status"`
- **THEN** 输出守护进程不可达提示，退出码为 3

## REMOVED Requirements

### Requirement: shell 与 system 子命令

**Reason**: `system` 子命令组唯一动作 `system webui` 已随「配置驱动的控制台
生命周期」删除——控制台的开启/关闭不再是 CLI 拉起问题，而是 daemon 配置 +
`xkeeper reload` 的问题；保留只剩 shell 一个入口的原 requirement 名不再准确。

**Migration**: 打开控制台改用 `xkeeper config --set webui.listen=127.0.0.1:9877`
后执行 `xkeeper reload`（或以含 `[webui]` 段的配置启动 `xkeeper run`），
再用浏览器访问控制台地址；shell 用法不变，见新增的「shell 子命令」requirement。
