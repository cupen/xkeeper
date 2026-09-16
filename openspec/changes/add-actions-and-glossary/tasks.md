## 1. 配置层：字符集收紧与动作定义表

- [x] 1.1 `is_valid_name` 收紧为 `[A-Za-z0-9_-]+` 全匹配（应用名/程序名单一闸口），更新 `config.rs` 单测覆盖：点号名、空格、非 ASCII、空串被拒；字母数字下划线连字符通过。验证：`cargo test config::`
- [x] 1.2 动作定义表解析：`ProgramRaw` 增加 `action: BTreeMap<String, ActionRaw>`（字段仅 `command` 必填、`timeout` 可选缺省 30 且必须 > 0），resolve 期校验动作名字符集、内置动作保留字冲突、command 非空；扫描 `${...}` 中已知域前缀（`program.*`/`app.*`/`daemon.*`）的变量名并拒绝未知字段，非已知域前缀的 `${...}` 原样放行。单测覆盖全部校验分支与多行 command。验证：`cargo test config::`
- [x] 1.3 变量求值器：spawn 期将已知域变量替换为运行时值（`pid` 在程序未运行时为空串；`state`/`work_dir`/`log_dir`/`app.path`/`daemon.*` 按实际值），跨程序引用按全局唯一程序名解析。单测覆盖替换与空串规则。验证：`cargo test config::`

## 2. 动作执行核心

- [x] 2.1 新建动作执行模块：worker 线程经平台 shell 执行（unix `/bin/sh -c`、Windows `cmd /C`），cwd 取程序 work_dir，stdout/stderr 捕获，子进程按平台挂进程组 / Job Object（动作孙进程可被清理）。单测：执行 echo 类命令验证退出码与输出捕获。验证：`cargo test action`
- [x] 2.2 输出落盘与响应：完整输出按行写入 daemon 日志（行前缀 `[program.<n>.action.<name>]`），结果携带退出码、耗时、输出尾部（截断常量）。单测覆盖日志前缀与尾部截断。验证：`cargo test action`
- [x] 2.3 超时终止：到期终止动作进程树（复用 `platform` 杀树原语），结果标记 `timed_out` 并返回超时错误。单测：sleep 类命令超时返回。验证：`cargo test action`
- [x] 2.4 (program, action) 互斥登记：执行中再调用返回冲突错误，不排队；同一 program 不同动作允许并行。单测覆盖互斥与并行放行。验证：`cargo test action`
- [x] 2.5 结构性集成确认：动作路径不经过 supervisor 巡检循环与命令队列（巡检循环测试回归通过）。验证：`cargo test supervisor::`

## 3. signal 内置动作

- [x] 3.1 `platform` 信号投递原语与白名单枚举（TERM/INT/HUP/QUIT/USR1/USR2，大小写规范化）：unix `kill(pid, sig)`，Windows 返回平台不支持。单测：unix 对 sleep 子进程投递 TERM 并观察到退出。验证：`cargo test platform::`
- [x] 3.2 投递路径：从 `ManagedProgram` 取当前子进程 pid 投递，不改状态机；未运行报错；KILL/STOP/CONT 拒绝并说明替代方式。单测覆盖白名单内外与未运行分支。验证：`cargo test supervisor::`

## 4. app 级扇出

- [x] 4.1 supervisor 新命令 `AppStart/AppStop/AppRestart`：按已解析程序 priority/depends_on 排序展开（停止为逆序），逐程序复用既有 per-program 逻辑（spawn 清 `user_stopped`）。单测：三程序依赖链验证扇出顺序与 user_stopped 覆盖。验证：`cargo test supervisor::`
- [x] 4.2 未知 app 与空 app 的错误路径。单测覆盖。验证：`cargo test supervisor::`

## 5. 控制平面端点

- [x] 5.1 `server.rs` 新路由：`POST /v1/programs/{name}/signal`、`POST /v1/programs/{name}/actions/{action}`、`POST /v1/apps/{name}/start|stop|restart`；错误映射（未知程序/动作 404、执行中冲突 409、非白名单信号 400）；纳入既有 Bearer 鉴权覆盖。验证：本地起 daemon 的集成测试打全部新端点
- [x] 5.2 动作结果 JSON 形状（exit_code / duration / timed_out / 输出尾部）与扇出逐程序结果。集成测试断言字段。验证：集成测试

## 6. CLI 与 shell 面板

- [x] 6.1 `client.rs` 新增动作/信号/扇出方法；`main.rs` 新子命令 `action <program> <action>`、`signal <program> <SIGNAL>`、`start|stop|restart --app <name>`；CLI 退出码透传动作退出码（调用失败 1、不可达 3）。冒烟：对运行中 daemon 执行成功/失败动作核对退出码。验证：`cargo run -- action/signal` 冒烟
- [x] 6.2 `shell.rs` 新动词 `action`、`signal` 与 `--app` 参数，`help` 更新，相近命令提示覆盖新词；REPL 与 `-e` 单命令模式一致。单测沿用既有解析测试模式。验证：`cargo test shell::`

## 7. 文档

- [x] 7.1 README：精简术语表（daemon/app/program/action + 一句话定义，指向 `openspec/specs/glossary/`）；动作章节（配置示例含多行命令与显式脚本调用、变量表、链式 `xkeeper restart` 模式、经 shell 执行与 no-shell 分叉说明）。验证：人工核对示例与链接
- [x] 7.2 升级迁移说明：字符集 breaking（改名 → validate → reload）、回滚前需移除 `[program.<n>.action.*]` 表。验证：人工核对

## 8. e2e 验收与收口

- [x] 8.1 `xtask e2e` 新场景：动作全链路（声明 → validate → 执行 → 变量替换断言 → 超时 → 互斥 → 链式 restart）、signal（unix 白名单内外）、`--app` 扇出顺序、字符集拒绝用例。验证：`cargo run -p xtask -- e2e`
- [x] 8.2 双平台核对：Windows 路径（signal 平台不支持错误、`cmd /C` 动作执行、Job Object 清理动作树）、unix 路径全部通过。验证：记录两平台 e2e 输出
- [x] 8.3 全量收口：`cargo test`、`cargo run -p xtask -- e2e`、`openspec validate --specs`（归档前）。验证：三条命令全绿
