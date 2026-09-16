## Purpose

为 xkeeper 的全部规范、文档与面板提供权威术语：daemon、app、program、action 的定义、边界与不变量，保证跨工件用词一致，杜绝「应用/程序」混称与寻址歧义。

## ADDED Requirements

### Requirement: 核心术语定义

系统文档与规范 SHALL 以下列定义为准：**daemon（守护进程）**——全局唯一的 xkeeper 监督进程本体，持有 daemon 配置与 `app_dir`，唯一执行监督循环的实体；**app（应用）**——注册的部署单元，配置本体为部署目录中的 `xkeeper.toml`，注册记录为 `app_dir` 中的链接（脚手架生成的为真实文件）；app 自身不是进程，是程序的容器；**program（程序）**——app 配置 `[program.<name>]` 定义的被守护子进程，拥有独立状态机与日志流，其名称跨全部已注册应用全局唯一；**action（动作）**——面向 daemon、app 或 program 发出的执行指令，分为内置动作（实现于 xkeeper 自身）与自定义动作（用户在配置中声明、映射为 shell 命令执行）。复合词：**daemon 配置**（全局唯一根配置）、**app 配置**（应用部署目录中的配置文件）、**app_dir**（应用注册目录）、**注册**（建立 `app_dir` 链接或生成脚手架文件的动作）、**控制平面**（daemon 暴露的本地 HTTP API 及其 CLI/Shell 映射）。

#### Scenario: 状态输出中的术语指称

- **WHEN** `xkeeper status` 输出程序列表
- **THEN** 每行的主名称为全局唯一的程序名，所属 app 仅作归属列展示

#### Scenario: 文档用词一致

- **WHEN** 规范、CLI 帮助或错误信息指称被守护的子进程
- **THEN** 使用「程序/program」，不以「应用/app」混称（app 特指部署单元）

### Requirement: 动作词表边界

「动作」SHALL 仅指改变运行状态的执行指令。观察类命令（status、pid、log、list、pending）MUST NOT 纳入动作词表，自定义动作名无需回避它们。`apply` SHALL 保持 apply-workflow 规范定义的配置应用工作流概念，不属于运行时动作。内置动作名（start、stop、restart、signal、reload、shutdown）SHALL 作为自定义动作的保留字。

#### Scenario: 查询不属于动作

- **WHEN** 用户通过 status/pid/log/list/pending 获取信息
- **THEN** 这些命令作为观察面板提供，不占用动作命名空间，自定义动作可使用同名（如动作名 `status`）而不冲突

#### Scenario: apply 独立于动作

- **WHEN** 文档或帮助提及 apply
- **THEN** 其语义为 apply-workflow 规范定义的配置应用工作流，不作为 daemon/app/program 的动作呈现

### Requirement: 标识符不变量

app 名、程序名、自定义动作名 SHALL 遵循同一字符集（仅英文字母、数字、下划线与连字符，校验行为见 configuration 规范）；程序名 SHALL 跨全部已注册应用全局唯一（由 configuration 校验强制）。任一面板寻址单个 program 时 SHALL 使用其全局唯一名即可无歧义定位；`app.program` 复合寻址语法 SHALL 保留语法空间，本规范版本不定义其语义。

#### Scenario: 单程序无歧义寻址

- **WHEN** 任一面板（CLI/API/shell）以程序名寻址操作对象
- **THEN** 该名称全局唯一、无歧义，无需 app 前缀限定

#### Scenario: 复合名不被解析为寻址语法

- **WHEN** 配置或命令中出现含点号的复合名（如 `gateway.api`）
- **THEN** 因程序名不允许点号，该名字只会被校验拒绝，MUST NOT 被解释为 app.program 寻址
