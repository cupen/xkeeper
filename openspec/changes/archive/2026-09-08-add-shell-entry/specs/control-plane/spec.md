# control-plane Specification — Delta

## ADDED Requirements

### Requirement: shell 与 system 子命令

单二进制 SHALL 另提供两个客户端入口：

- `xkeeper shell`：交互式 REPL（详见 `shell-client` 能力规范），经控制面 API 与根进程
  通信，`-c "<命令>"` 进入单命令模式；
- `xkeeper system webui [url]`：本地辅助命令，探测/拉起 webui 并用系统默认浏览器打开。

两者 SHALL 遵循既有退出码约定：0 成功；1 一般错误；2 配置错误；3 守护进程不可达。
`system webui` 拉起根进程失败 SHALL 归入退出码 1。

#### Scenario: shell 纳入退出码约定

- **WHEN** 守护进程未启动时执行 `xkeeper shell -c "status"`
- **THEN** 输出守护进程不可达提示，退出码为 3

#### Scenario: system webui 拉起失败

- **WHEN** `xkeeper system webui` 尝试拉起根进程但启动失败（如端口被占用）
- **THEN** 输出失败原因，退出码为 1
