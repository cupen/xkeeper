## ADDED Requirements

### Requirement: 配置驱动的控制台生命周期

控制台 SHALL 由 daemon 配置的 `[webui]` 段驱动启停：段存在即伺服，`webui.listen` 缺省 `127.0.0.1:9877`；段不存在 SHALL NOT 伺服。`xkeeper run` 启动时 SHALL 按该配置决定是否伺服。运行中的守护进程执行 `xkeeper reload` 时 SHALL 重读 `[webui]` 段并立即热生效（MUST NOT 等待 apply、MUST NOT 进入 pending 预览）：由关到开 SHALL 起伺服；由开到关 SHALL 停止伺服并将既有 WebSocket 连接 graceful 关闭（先发关闭帧再断开，不丢已在途推送帧）；`listen` 变更 SHALL 停旧起新完成重绑。webui 监听地址被占用时 SHALL 降级为记录错误日志并继续守护运行（无控制台），MUST NOT 致使守护进程退出。reload 时 `[webui]` 段非法 SHALL 视为本轮 reload 失败（保持既有伺服状态不变并上报错误）。

#### Scenario: 启动时按配置伺服

- **WHEN** daemon 配置含 `[webui]`（未写 `listen`）且执行 `xkeeper run`
- **THEN** 控制台在 `127.0.0.1:9877` 可访问；不含 `[webui]` 段时不监听任何 webui 端口

#### Scenario: reload 动态开启

- **WHEN** 守护进程以无 `[webui]` 段的配置运行，执行 `xkeeper config --set webui.listen=127.0.0.1:9877` 后执行 `xkeeper reload`
- **THEN** 控制台立即在该地址可访问，无需重启守护进程

#### Scenario: reload 动态关闭

- **WHEN** 控制台伺服中存在 WebSocket 订阅，执行 `xkeeper config --delete webui` 后执行 `xkeeper reload`
- **THEN** 控制台停止伺服，WebSocket 连接以关闭帧 graceful 断开，守护进程与其余受管程序不受影响

#### Scenario: listen 变更重绑

- **WHEN** 控制台伺服于 `127.0.0.1:9877`，将配置改为 `webui.listen=127.0.0.1:9898` 后执行 `xkeeper reload`
- **THEN** 旧地址停止伺服、新地址开始伺服，守护进程不重启

#### Scenario: webui 端口占用降级

- **WHEN** `[webui]` 启用且目标端口已被其他进程占用，执行 `xkeeper run` 或 `xkeeper reload`
- **THEN** 记录控制台不可用的错误日志，守护进程与受管程序照常运行，后续修正配置并 reload 可恢复伺服

## MODIFIED Requirements

### Requirement: 与守护进程同进程共存

控制台服务器 MUST 与守护循环运行在同一进程并共享同一 `Supervisor`：状态读取
SHALL 来自共享状态（与控制面相同的锁），控制命令 SHALL 经命令队列在守护循环
中执行，日志 SHALL 来自 pump 环形缓冲。控制台服务器 SHALL 随守护进程关闭而
退出（Ctrl+C 或 `POST /v1/shutdown`）。`xkeeper run` SHALL 是唯一的守护入口
子命令；控制台是否伺服 SHALL 由 daemon 配置决定（见「配置驱动的控制台生命周期」），
MUST NOT 存在独立的启动模式子命令。

#### Scenario: webui 反映真实状态

- **WHEN** 控制台伺服中且某程序进入 fatal
- **THEN** `GET /api/programs/{name}` 与 WebSocket 推送均反映 fatal

#### Scenario: 随守护进程退出

- **WHEN** 发送 `POST /v1/shutdown` 或 Ctrl+C
- **THEN** 控制台服务器随之退出，进程正常终止
