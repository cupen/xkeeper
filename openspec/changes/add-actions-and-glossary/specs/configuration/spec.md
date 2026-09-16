## MODIFIED Requirements

### Requirement: 校验规则

配置校验 SHALL 拒绝未知字段（拼写错误报错而非忽略）、空的应用（无任何 `[program.*]`）、超出合法范围的数值字段。应用名、程序名与自定义动作名 SHALL 仅由英文字母、数字、下划线与连字符组成（`[A-Za-z0-9_-]+`），含其它字符的名字（含点号、空格、非 ASCII 字符）SHALL 被校验拒绝（破坏性收紧：此前合法的含点号等名字在升级后校验失败，需改名）。`command` 非空且单行拆分与显式 `args` 不得同时出现；`health_check` 字符串 SHALL 在加载期按协议解析并校验。`daemon.log_dir` SHALL 为绝对路径：相对路径或空值 SHALL 被校验拒绝，守护进程 MUST 拒绝启动并在错误信息中指明字段与修复方式。程序名 SHALL 在全部已注册应用之间全局唯一：注册与 reload 时检测到跨应用重名 SHALL 拒绝并指明冲突双方。`depends_on` 引用 SHALL 允许跨应用，成环在注册/校验期被拒绝。自定义动作定义 SHALL 随配置解析一并校验：`command` 非空、`timeout` 大于 0、引用的变量名全部已知、动作名不与内置动作保留字（start、stop、restart、signal、reload、shutdown）冲突。

#### Scenario: 跨应用程序名冲突被拒绝

- **WHEN** 注册一个其程序名与已注册程序重名的 app 配置
- **THEN** 注册失败，错误指明重名程序与冲突的双方应用

#### Scenario: 单行 command 与 args 冲突

- **WHEN** 程序同时写 `command = "python -m server"` 与 `args = ["-m", "server"]`
- **THEN** 校验失败，提示二者互斥

#### Scenario: 相对 log_dir 拒绝启动

- **WHEN** daemon 配置声明 `log_dir = "logs"`（相对路径）并执行 `xkeeper run` 或 `xkeeper validate`
- **THEN** 校验失败，错误信息指明 `daemon.log_dir` 必须为绝对路径，守护进程不启动

#### Scenario: 非法字符名被拒绝

- **WHEN** app 配置声明 `[program."my.web"]` 或应用名含空格等 `[A-Za-z0-9_-]` 之外的字符
- **THEN** 校验失败，错误指明名字仅允许英文字母、数字、下划线与连字符

#### Scenario: 未知动作变量被拒绝

- **WHEN** 动作 command 引用 `${program.api.hello}`（`hello` 非内置变量字段）
- **THEN** validate 与 reload 校验失败，错误指明未知变量名

#### Scenario: 动作占用保留字被拒绝

- **WHEN** app 配置声明 `[program.api.action.stop]`
- **THEN** 校验失败，错误指明动作名与内置动作保留字冲突

## ADDED Requirements

### Requirement: 动作定义表

app 配置 SHALL 支持在 `[program.<name>]` 下声明自定义动作表 `[program.<name>.action.<action-name>]`：map 键即动作名。字段 SHALL 仅有 `command`（字符串，必填；支持 TOML 多行字符串书写长命令，或显式调用外部脚本并自选解释器）与 `timeout`（秒数，可省略，缺省 30，必须大于 0）。`command` 的执行行为契约见 actions 规范。内置变量 SHALL 以 `${域.名.字段}` 形式引用，可用变量 SHALL 为：`program.<name>.pid`、`program.<name>.state`、`program.<name>.app`、`program.<name>.work_dir`、`program.<name>.log_dir`、`app.<name>.path`、`daemon.pid`、`daemon.host`、`daemon.port`、`daemon.log_dir`、`daemon.app_dir`；跨程序引用 SHALL 允许（程序名全局唯一）。程序未运行时其 `pid` 变量 SHALL 替换为空串。

#### Scenario: 最小动作定义可用

- **WHEN** 程序 api 声明 `[program.api.action.flush]` 且 `command = "curl -fsS http://127.0.0.1:8080/flush"`
- **THEN** validate 通过，动作可经 `xkeeper action api flush` 执行

#### Scenario: 多行长命令与显式脚本调用

- **WHEN** 动作 command 使用 TOML 多行字符串书写多行命令，或书写 `bash scripts/upgrade.sh` 调用外部脚本
- **THEN** 校验通过，执行时命令文本整体交给平台 shell，脚本相对路径按程序 work_dir 解析

#### Scenario: timeout 缺省与非法值

- **WHEN** 动作未写 timeout，或写 `timeout = 0` / 负数
- **THEN** 未写时以 30 秒缺省生效；0 与负数校验失败并指明字段
