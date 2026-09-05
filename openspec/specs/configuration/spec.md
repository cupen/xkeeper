# configuration Specification

## Purpose

定义 xkeeper 的分层配置契约：唯一的 core 根配置（只做全局：守护进程设置、`app_dir`、`[app-default]` 应用默认值）、随应用部署目录分布的 app 配置（map 接口、扁平字段）、四层字段优先级、校验规则、v0.1 迁移路径与按应用粒度的热更新语义。

## Requirements

### Requirement: core 根配置

core 配置全局唯一，SHALL 只包含全局内容：`[daemon]`（监听地址/端口、鉴权 token、日志、巡检间隔、`app_dir` 应用注册目录）与可选的 `[app-default]`（全部应用共享的默认值表，字段名与程序字段一致）。core MUST NOT 承载任何应用专属内容：应用条目（如 `[[app]]`）、应用名、配置路径等 SHALL 被校验直接拒绝。`app_dir` 缺省为 core 配置文件同级的 `apps` 目录。core 默认路径 SHALL 为 Linux `/etc/xkeeper.toml`、Windows `%APPDATA%\xkeeper\xkeeper.toml`，并 SHALL 可用 `-c/--core-config` 覆盖。core 文件不存在时守护进程 SHALL 以内置默认值空启动，首次写入时创建。

#### Scenario: 全新机器空启动

- **WHEN** 在没有任何 core 配置文件的机器上执行 `xkeeper run`
- **THEN** 守护进程以默认设置启动，受管程序为空，控制平面正常可用

#### Scenario: core 中的应用条目被拒绝

- **WHEN** core 配置中出现 `[[app]]` 等应用专属条目
- **THEN** 校验失败并报未知字段，守护进程拒绝以该配置启动

### Requirement: app 配置

每个应用的配置本体 SHALL 存放在其部署目录，默认文件名 `xkeeper.toml`，注册时可指定任意路径。app 配置 SHALL 由可选的 `[app]` 表（应用级默认值与 `description` 元数据，注册微调 flag 的落点）与一或多个 `[program.<name>]` 映射表组成：映射键即程序名，MUST NOT 再写 `name` 字段。程序字段 SHALL 采用简化写法：`work_dir`（工作目录）、`env`（环境变量表）、`log_max_size`/`log_rotate_keep`（日志轮转，无需独立 section）、`health_check`（单字符串，按协议前缀分发：`http(s)://` 为 HTTP 探测、`tcp://` 为 TCP 连通、其余视为探测命令行）；`health_interval`/`health_timeout`/`health_retries`/`health_start_period` 为可选节奏字段。`command` SHALL 支持单行带参写法（如 `"python -m http.server 8000"`，按 shell 词法规则拆分为 argv、不经过 shell），与显式 `args` 数组互斥。

#### Scenario: 最小 app 配置可用

- **WHEN** 部署目录 `xkeeper.toml` 只写 `[program.api]` 与其 `command = "python -m http.server 8000"`
- **THEN** `xkeeper add .` 即可注册，程序名为 `api`，参数经拆分后按默认值运行

#### Scenario: 健康检查按协议分发

- **WHEN** 程序声明 `health_check = "tcp://127.0.0.1:5432"`
- **THEN** 该程序的健康检查为 TCP 连通探测，无需再声明类型字段

### Requirement: 字段优先级

同一字段 SHALL 按以下优先级取值（从高到低）：`[program.*]` 显式字段 > app 配置 `[app]` 表（注册微调 flag 落点，部署人可手改）> core `[app-default]` > 内置默认。`autostart` 与 `priority` 为应用级字段，仅存在于 `[app]` 与 `[app-default]` 层。

#### Scenario: app 层默认生效

- **WHEN** `[app]` 设置 `autorestart = "on-failure"`，程序未显式声明
- **THEN** 该应用内程序按 on-failure 运行

#### Scenario: core 默认兜底

- **WHEN** core `[app-default]` 设 `restart_backoff = 2.0`，app 配置未在任何层级声明
- **THEN** 程序按 2 秒重启间隔运行

#### Scenario: 程序显式字段最高

- **WHEN** 某程序显式声明 `autorestart = "never"`，而 `[app]` 与 `[app-default]` 均为 always
- **THEN** 该程序按 never 运行，同应用其他程序不受影响

### Requirement: app_dir 注册目录

core 指定的 `app_dir` SHALL 集中存放全部已注册应用的链接：注册成功时自动以应用名创建 `<name>.toml` 链接指向 app 配置本体（优先符号链接，平台不允许时回退硬链接；仍不可行则跳过链接、注册照常生效并告警）。app 配置本体 MUST 始终以部署目录中的原件为准，链接仅为集中查看的投影；add/remove SHALL 同步维护链接，发现链接缺失或悬空时 SHALL 告警。

#### Scenario: 注册后自动链接

- **WHEN** 在部署目录执行 `xkeeper add . --name web` 成功且平台支持符号链接
- **THEN** `app_dir/web.toml` 出现并指向部署目录中的配置本体，本体内容变化即时可见

#### Scenario: 链接创建被平台拒绝

- **WHEN** 平台不允许创建链接（如 Windows 未开启符号链接权限且非同卷）
- **THEN** 注册仍然成功，输出告警说明链接未建立及其影响

### Requirement: 校验规则

配置校验 SHALL 拒绝未知字段（拼写错误报错而非忽略）、空的应用（无任何 `[program.*]`）、使用不安全字符（路径分隔符、控制字符等）的应用名与程序名、超出合法范围的数值字段；`command` 非空且单行拆分与显式 `args` 不得同时出现；`health_check` 字符串 SHALL 在加载期按协议解析并校验。程序名 SHALL 在全部已注册应用之间全局唯一：注册与 reload 时检测到跨应用重名 SHALL 拒绝并指明冲突双方。`depends_on` 引用 SHALL 允许跨应用，成环在注册/校验期被拒绝。

#### Scenario: 跨应用程序名冲突被拒绝

- **WHEN** 注册一个其程序名与已注册程序重名的 app 配置
- **THEN** 注册失败，错误指明重名程序与冲突的双方应用

#### Scenario: 单行 command 与 args 冲突

- **WHEN** 程序同时写 `command = "python -m server"` 与 `args = ["-m", "server"]`
- **THEN** 校验失败，提示二者互斥

### Requirement: v0.1 旧配置迁移

v0.1 的单文件配置（`[daemon]` + `[[program]]` 数组）SHALL 可通过 `xkeeper add <旧文件>` 直接导入：导入器 SHALL 以独立的旧版 schema 解析，将 `[[program]]` 转写为 `[program.*]` 映射并注册为一个应用，`[daemon]` 段被检测并提示需要并入 core 配置；导入 MUST NOT 修改原文件。通过 `-c` 把 v0.1 文件直接交给 `run` 时 SHALL 触发同一条导入路径。

#### Scenario: 一键迁移旧配置

- **WHEN** 对 v0.1 的 `config.toml` 执行 `xkeeper add config.toml`
- **THEN** 其中全部程序注册为一个应用并可被守护进程拉起，输出提示 `[daemon]` 段需并入 core 配置，原文件内容未变

### Requirement: 按应用粒度的热更新

reload SHALL 重扫 `app_dir` 注册并重读全部 app 配置本体与 core：注册变化（新增/移除链接）总是应用；单个 app 文件变化仅影响该应用的程序（停止受影响程序并以新定义重建）；某个 app 文件非法时 SHALL 隔离失败——该应用保持旧定义继续运行并上报错误，其余应用正常应用。core 中守护进程自身字段变更 SHALL 尽量热应用（如日志级别），无法热更的字段（如监听端口）SHALL 明确提示需要重启守护进程。

#### Scenario: 单个 app 文件损坏不影响其他

- **WHEN** reload 时 app B 的配置存在语法错误，app A 的配置正常且已修改
- **THEN** A 的变更被应用，B 保持旧定义继续运行，错误信息被上报

#### Scenario: 移除注册立即生效

- **WHEN** 守护进程运行中执行 `xkeeper remove <app>`
- **THEN** 该应用的程序被停止并从状态中消失，`app_dir` 链接被清理

### Requirement: validate 子命令

`xkeeper validate` SHALL 校验 core 配置及 `app_dir` 中全部已注册 app 配置本体；`xkeeper validate <path>` SHALL 单独校验一个 app 配置文件。通过时输出应用与程序清单并以 0 退出；失败时输出全部错误并以非零码退出；两种情况均 MUST NOT 启动任何进程。

#### Scenario: 校验通过

- **WHEN** 对合法的 core 与全部 app 配置执行 validate
- **THEN** 输出应用/程序清单，退出码 0，无进程被启动

#### Scenario: 校验失败

- **WHEN** 任一文件存在错误时执行 validate
- **THEN** 逐条列出错误（含文件与应用归属），退出码非 0
