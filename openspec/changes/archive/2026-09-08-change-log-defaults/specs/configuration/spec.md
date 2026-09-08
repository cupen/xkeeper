## MODIFIED Requirements

### Requirement: core 根配置

core 配置全局唯一，SHALL 只包含全局内容：`[daemon]`（监听地址/端口、鉴权 token、日志、巡检间隔、`app_dir` 应用注册目录）与可选的 `[app-default]`（全部应用共享的默认值表，字段名与程序字段一致）。core MUST NOT 承载任何应用专属内容：应用条目（如 `[[app]]`）、应用名、配置路径等 SHALL 被校验直接拒绝。`app_dir` 缺省为 core 配置文件同级的 `apps` 目录。`daemon.log_dir` 缺省为 `/tmp/xkeeper/logs`（绝对路径，不随 core 配置文件位置漂移）；配置的 `log_dir` MUST 为绝对路径，相对路径 SHALL 被校验拒绝（守护进程拒绝启动）。core 默认路径 SHALL 为 Linux `/etc/xkeeper.toml`、Windows `%APPDATA%\xkeeper\xkeeper.toml`，并 SHALL 可用 `-c/--core-config` 覆盖。core 文件不存在时守护进程 SHALL 以内置默认值空启动，首次写入时创建。

#### Scenario: 全新机器空启动

- **WHEN** 在没有任何 core 配置文件的机器上执行 `xkeeper run`
- **THEN** 守护进程以默认设置启动，受管程序为空，控制平面正常可用

#### Scenario: core 中的应用条目被拒绝

- **WHEN** core 配置中出现 `[[app]]` 等应用专属条目
- **THEN** 校验失败并报未知字段，守护进程拒绝以该配置启动

#### Scenario: 缺省日志目录固定

- **WHEN** core 配置未声明 `log_dir`，守护进程拉起任一程序并产生输出
- **THEN** 程序日志写入 `/tmp/xkeeper/logs`，与 core 配置文件所在目录无关

### Requirement: app 配置

每个应用的配置本体 SHALL 存放在其部署目录，默认文件名 `xkeeper.toml`，注册时可指定任意路径。app 配置 SHALL 由可选的 `[app]` 表（应用级默认值与 `description` 元数据，注册微调 flag 的落点）与一或多个 `[program.<name>]` 映射表组成：映射键即程序名，MUST NOT 再写 `name` 字段。程序字段 SHALL 采用简化写法：`work_dir`（工作目录）、`env`（环境变量表）、`log_max_size`/`log_rotate_keep`（日志轮转，无需独立 section；缺省 `50MB` 保留 `2` 份，即每流盘上最多 3 个文件；`log_max_size = "0"` 显式禁用轮转）、`health_check`（单字符串，按协议前缀分发：`http(s)://` 为 HTTP 探测、`tcp://` 为 TCP 连通、其余视为探测命令行）；`health_interval`/`health_timeout`/`health_retries`/`health_start_period` 为可选节奏字段。`command` SHALL 支持单行带参写法（如 `"python -m http.server 8000"`，按 shell 词法规则拆分为 argv、不经过 shell），与显式 `args` 数组互斥。

#### Scenario: 最小 app 配置可用

- **WHEN** 部署目录 `xkeeper.toml` 只写 `[program.api]` 与其 `command = "python -m http.server 8000"`
- **THEN** `xkeeper add .` 即可注册，程序名为 `api`，参数经拆分后按默认值运行

#### Scenario: 健康检查按协议分发

- **WHEN** 程序声明 `health_check = "tcp://127.0.0.1:5432"`
- **THEN** 该程序的健康检查为 TCP 连通探测，无需再声明类型字段

#### Scenario: 缺省阈值触发轮转

- **WHEN** 程序未声明任何轮转字段，某流日志超过 50MB 后继续输出
- **THEN** 当前文件被轮转为 `.1`，该流盘上最多保留 3 个文件（当前 + 2 份轮转）

#### Scenario: 显式禁用轮转

- **WHEN** 程序声明 `log_max_size = "0"`
- **THEN** 该程序的日志不轮转，文件持续追加不设上限

### Requirement: 校验规则

配置校验 SHALL 拒绝未知字段（拼写错误报错而非忽略）、空的应用（无任何 `[program.*]`）、使用不安全字符（路径分隔符、控制字符等）的应用名与程序名、超出合法范围的数值字段；`command` 非空且单行拆分与显式 `args` 不得同时出现；`health_check` 字符串 SHALL 在加载期按协议解析并校验。`daemon.log_dir` SHALL 为绝对路径：相对路径或空值 SHALL 被校验拒绝，守护进程 MUST 拒绝启动并在错误信息中指明字段与修复方式。程序名 SHALL 在全部已注册应用之间全局唯一：注册与 reload 时检测到跨应用重名 SHALL 拒绝并指明冲突双方。`depends_on` 引用 SHALL 允许跨应用，成环在注册/校验期被拒绝。

#### Scenario: 跨应用程序名冲突被拒绝

- **WHEN** 注册一个其程序名与已注册程序重名的 app 配置
- **THEN** 注册失败，错误指明重名程序与冲突的双方应用

#### Scenario: 单行 command 与 args 冲突

- **WHEN** 程序同时写 `command = "python -m server"` 与 `args = ["-m", "server"]`
- **THEN** 校验失败，提示二者互斥

#### Scenario: 相对 log_dir 拒绝启动

- **WHEN** core 配置声明 `log_dir = "logs"`（相对路径）并执行 `xkeeper run` 或 `xkeeper validate`
- **THEN** 校验失败，错误信息指明 `daemon.log_dir` 必须为绝对路径，守护进程不启动
