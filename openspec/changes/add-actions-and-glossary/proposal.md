## Why

xkeeper 的核心名词（daemon / app / program）至今没有权威定义，散落在各规范的行文里靠语境消歧；而「对受管对象发出执行指令」这件事（start/stop/restart/reload/shutdown）也一直没有统一概念，导致三类真实运维需求无处安放：升级后重启（先构建再重启自己）、向运行中进程发任意信号、按应用批量启停。本变更先把术语立住，再以「动作（action）」统一指挥词汇，并让动作可以通过配置扩展。

## What Changes

- 新增规范能力 `glossary`：daemon、app、program、action（内置 vs 自定义）的权威定义与用法约束；README 增加精简术语表指向规范源（README 内容属实施阶段）。
- 统一动作概念并收纳现状：daemon 级 shutdown/reload、program 级 start/stop/restart 定义为内置动作；查询类命令（status/pid/log/list/pending）明确不属于动作；apply 保持 apply-workflow 独立概念。
- 新增 app 级扇出动作：`xkeeper start/stop/restart --app <name>` 作用于该 app 全部程序，排序复用既有 priority + depends_on 规则。
- 新增内置动作 signal：向 program 子进程投递白名单信号（TERM/INT/HUP/QUIT/USR1/USR2），仅 unix，Windows 明确报平台不支持。
- 新增自定义动作（custom action）：在 app 配置 `[program.<name>.action.<action-name>]` 声明（字段仅 `command` 必填 + `timeout` 可选缺省 30s），经平台 shell 执行（unix `/bin/sh -c`、windows `cmd /C`），支持 `${program.<n>.pid}` 等内置变量替换，worker 线程执行不阻塞 supervisor 循环，超时杀动作进程树，同 program 同动作互斥。
- 新增面板：CLI `xkeeper action <program> <name>`、`xkeeper signal <program> <SIGNAL>`；API `POST /v1/programs/{name}/actions/{action}`、`POST /v1/programs/{name}/signal`；shell client 同步新增 action/signal/--app。webui 不动，动作执行不进状态投影。
- **BREAKING** 标识符字符集收紧：app 名、程序名、动作名统一只允许 `[A-Za-z0-9_-]+`（英文字母、数字、下划线、连字符），其余字符（含点号、空格、非 ASCII）非法。既有配置中含其它字符的名字升级后将被 validate/reload 拒绝。

## Capabilities

### New Capabilities

- `glossary`: 核心术语（daemon/app/program/action 及复合词）的规范定义、术语不变量（程序名全局唯一、标识符字符集）与正确用法约束。
- `actions`: 动作模型总纲（三层动作矩阵、内置/自定义二分）、custom action 声明与执行契约（shell 执行、变量表、超时、互斥、输出去向、校验时机）、signal 投递语义、app 级扇出动作。

### Modified Capabilities

- `configuration`: 标识符字符集收紧为 `[A-Za-z0-9_-]+`（校验规则 requirement 增加约束，**BREAKING**）；app 配置新增 `[program.<name>.action.<name>]` 表的形态与解析校验（字段集、timeout 范围、变量名已知性、内置动作保留字）。
- `control-plane`: 本地 HTTP API 新增 `POST /v1/programs/{name}/actions/{action}` 与 `POST /v1/programs/{name}/signal`；CLI 新增 `action`、`signal` 命令与 start/stop/restart 的 `--app` 扇出参数，映射到上述端点。
- `shell-client`: shell 内置命令集新增 `action`、`signal` 与 start/stop/restart 的 `--app` 参数，REPL 与单命令模式一致。

## Impact

- **代码**：`src/config.rs`（action 表解析 + 字符集收紧 + 变量名校验）、`src/main.rs`（CLI 子命令）、`src/supervisor.rs`（动作 worker、扇出命令展开、signal 投递）、`src/server.rs`（新端点）、`src/client.rs`（CLI 客户端）、`src/shell.rs`（新动词）、`src/platform.rs`（信号白名单投递，unix/windows 两条路径）。
- **规范**：2 个新能力（glossary、actions）+ 3 个既有能力 delta（configuration、control-plane、shell-client）。
- **兼容性**：字符集收紧为破坏性变更——含非法字符名字的既有 app 配置/注册链接在升级后校验失败，需改名；信号动作在 Windows 上返回明确错误而非静默降级。
- **文档**：README 增补术语表精简版（指向 glossary 规范）与新命令示例。
