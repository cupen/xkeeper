## Purpose

定义面向 daemon、app、program 三层目标的动作模型：现状命令统一收纳为内置动作，新增 app 级扇出与 signal，并为自定义动作（配置声明映射为 shell 命令）提供执行契约。

## ADDED Requirements

### Requirement: 动作层级矩阵

动作 SHALL 按目标层级组织：daemon 级内置动作为 shutdown、reload（语义不变，见既有规范）；program 级内置动作为 start、stop、restart、signal；app 级动作为 start、stop、restart 的扇出形式（作用于该 app 全部程序）；自定义动作 SHALL 仅支持 program 级声明，daemon 级与 app 级自定义动作为预留扩展点，本规范版本不提供。app 扇出 start/stop/restart 的排序 SHALL 复用 process-management「启动与停止排序」规则：启动按 priority 与 depends_on 拓扑序，停止按逆序。

#### Scenario: app 扇出启动按既有排序

- **WHEN** 执行 `xkeeper start --app <name>` 且该 app 含多个程序
- **THEN** 各程序按 priority 与 depends_on 排序规则依次启动，返回逐程序结果

#### Scenario: 扇出 start 覆盖 user_stopped

- **WHEN** 对包含因显式 stop 而 user_stopped 程序的 app 执行扇出 start
- **THEN** 该程序同样被启动，user_stopped 标记清除，后续依赖评估不再视其为用户停止

#### Scenario: 扇出停止按逆序

- **WHEN** 执行 `xkeeper stop --app <name>`
- **THEN** 各程序按启动排序的逆序依次停止

### Requirement: signal 动作

signal 动作 SHALL 向 program 的当前子进程（pid 本身）投递信号，MUST NOT 投递其进程组（进程组清理是 stop 的语义）。允许的信号 SHALL 为白名单：TERM、INT、HUP、QUIT、USR1、USR2（名称大小写不敏感，内部规范化为大写）。白名单之外（含 KILL、STOP、CONT）SHALL 被拒绝并返回解释性错误。signal 成功投递 MUST NOT 改变程序状态机（程序保持原状态）。Windows 平台 SHALL 返回明确的「平台不支持」错误，MUST NOT 静默降级。对未运行（无存活子进程）的程序执行 signal SHALL 报错。

#### Scenario: 投递白名单信号

- **WHEN** unix 上对 running 程序执行 `xkeeper signal web USR1`
- **THEN** SIGUSR1 投递给该程序的子进程 pid，命令成功，程序状态保持 running

#### Scenario: 非白名单信号被拒绝

- **WHEN** 执行 `xkeeper signal web KILL`
- **THEN** 报错说明 KILL 不在白名单（需要停止请使用 stop），程序不受影响

#### Scenario: Windows 平台不支持

- **WHEN** Windows 上执行任意 signal 请求
- **THEN** 返回平台不支持的明确错误，CLI 退出码 1

#### Scenario: 对未运行程序发信号报错

- **WHEN** 对 stopped 状态的程序执行 signal
- **THEN** 报错（API 409 / CLI 退出码 1），指明程序当前没有可投递的子进程

### Requirement: 自定义动作执行契约

自定义动作 SHALL 以 program 名义执行：命令字符串经平台 shell（unix `/bin/sh -c`，Windows `cmd /C`）执行，多行命令整体作为 shell 脚本文本；执行前 xkeeper SHALL 完成 `${...}` 内置变量替换，shell MUST NOT 接触到 `${program.…}` 语法的原文。动作子进程的工作目录 SHALL 缺省为所属程序的 work_dir。动作 SHALL 在独立于 supervisor 巡检循环的工作线程执行，巡检循环 MUST NOT 被动作阻塞。stdout/stderr SHALL 被捕获：完整输出写入 daemon 日志（附动作名、程序名与退出码），调用响应携带截断的输出尾部。调用方同步等待至动作结束或超时：正常结束返回退出码与耗时；超时 SHALL 终止动作进程树并返回超时错误。同一 program 的同一动作 SHALL 互斥：执行中再次调用返回冲突错误，不排队。对处于停止状态的程序 SHALL 允许执行动作（此时 `pid` 变量替换为空串）。动作执行 MUST NOT 进入状态投影（StatusDoc）；动作结果仅通过调用响应与 daemon 日志呈现。

#### Scenario: 变量替换后经 shell 执行

- **WHEN** 执行声明为 `command = "curl -fsS http://127.0.0.1:8080/flush?pid=${program.api.pid}"` 的动作
- **THEN** 变量在 spawn 前替换为实际值，命令经平台 shell 执行，退出码与输出尾部返回给调用方

#### Scenario: 超时终止动作进程树

- **WHEN** 动作命令运行超过其 timeout
- **THEN** 动作进程树被终止，调用方收到超时错误，supervisor 巡检循环全程不受阻塞

#### Scenario: 同动作互斥

- **WHEN** 同一程序的同名动作已在执行中再次发起调用
- **THEN** 返回冲突错误（API 409 / CLI 退出码 1），原执行不受影响，不产生排队

#### Scenario: 动作中链式调用控制命令

- **WHEN** 动作命令包含 `xkeeper restart <程序名>`（如 `command = "cargo install --path . && xkeeper restart api"`）
- **THEN** 该调用经本地控制面 API 生效（CLI 自行从 daemon 配置读取连接与鉴权信息），动作以自身命令的退出码结束，重启结果记录于 daemon 日志

#### Scenario: 对已停程序执行动作

- **WHEN** 对 stopped 程序执行引用 `${program.<n>.pid}` 的动作
- **THEN** 动作正常执行，pid 变量替换为空串，可从输出观测替换结果
