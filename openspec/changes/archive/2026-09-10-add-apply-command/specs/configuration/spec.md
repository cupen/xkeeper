# configuration Specification Delta — add-apply-command

## MODIFIED Requirements

### Requirement: 按应用粒度的热更新

reload SHALL 重扫 `app_dir` 注册并重读全部 app 配置本体与 daemon 配置，但
仅将差异记为**待应用（pending）变更**并输出预览，MUST NOT 停止、重启或拉起
任何程序（应用由 apply 触发，见 apply-workflow 能力）。守护进程 SHALL 周期性
自动执行该重扫，`xkeeper reload` 触发一次立即重扫。单个 app 文件非法时
SHALL 隔离失败——该 app 不进入 pending、保持旧定义继续运行并上报错误，其余
app 照常检出。跨应用校验失败时本轮 SHALL 整体不形成 pending。daemon 配置中
守护进程自身字段变更 SHALL 尽量热应用（如日志级别），无法热更的字段（如监听
端口）SHALL 在 pending 预览中明确提示需要重启守护进程。

#### Scenario: 单个 app 文件损坏不影响其他

- **WHEN** 重扫时 app B 的配置存在语法错误，app A 的配置正常且已修改
- **THEN** A 的变更进入 pending，B 保持旧定义继续运行，错误信息被上报

#### Scenario: reload 只检出不应用

- **WHEN** app A 的程序 `web` 配置已修改且正在运行，执行 `xkeeper reload`
- **THEN** 输出包含 `web` 的 pending 预览，进程未被停止或重启

#### Scenario: 不可热更字段提示

- **WHEN** daemon 配置的 `port` 被修改，执行 `xkeeper reload`
- **THEN** pending 预览提示该字段需重启守护进程方可生效，控制面仍监听旧端口

#### Scenario: 移除注册立即生效

- **WHEN** 守护进程运行中执行 `xkeeper remove <app>`
- **THEN** `app_dir` 链接即时清理，该 app 的移除进入 pending，其程序在
  apply 前继续运行
