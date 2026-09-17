## REMOVED Requirements

### Requirement: system webui 辅助命令

**Reason**: 控制台生命周期改为 daemon 配置驱动（`[webui]` 段 + `xkeeper reload`
热生效，见 webui-api 能力规范），「探测/拉起/开浏览器」的本地辅助命令失去存在
基础：`webui` 模式子命令已删除，后台拉起无从谈起；守护已在跑而控制台未开时，
正确动作是 `config --set webui.listen=...` + `xkeeper reload` 而非重启进程。

**Migration**: 原一键打开场景改为三步——`xkeeper config --set
webui.listen=127.0.0.1:9877`（一次性，之后常驻配置）→ `xkeeper reload`（守护
在线时热开启）→ 浏览器访问 `http://127.0.0.1:9877`；离线时下次 `xkeeper run`
自动伺服。
