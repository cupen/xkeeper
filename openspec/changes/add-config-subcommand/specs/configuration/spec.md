## ADDED Requirements

### Requirement: config 子命令

`xkeeper config` SHALL 作为 daemon 配置文件的统一本地管理入口，动作由 flag 指定：`--set`（写入，可重复）、`--get`（读取）、`--delete`（删除，可重复）、`--edit`（编辑器编辑）、`--init`（初始化）；一次调用 SHALL 只执行一类动作（`--set`/`--get`/`--delete` 可各自多键，`--edit` 与 `--init` 不与其他动作同用），无任何动作 flag 时 SHALL 打印用法帮助并以 0 退出。目标文件为解析后的 daemon 配置路径（全局 `-c/--config` 可覆盖，缺省平台位置）。config SHALL 是纯本地文件操作：MUST NOT 依赖运行中的 daemon，MUST NOT 直接改变运行态（写入生效仍经由既有 reload/apply 流程）。daemon 在线且本次动作改写了文件时，SHALL 在输出中提示需要 reload 方可生效；在线检测失败不阻塞动作本身。

#### Scenario: 无动作打印帮助

- **WHEN** 执行 `xkeeper config`
- **THEN** 打印五个动作 flag 的用法说明，以 0 退出

#### Scenario: 离线可用

- **WHEN** daemon 未运行，执行 `xkeeper config --set port=7311`
- **THEN** 文件被写入并校验通过，输出中说明 daemon 当前离线、下次启动生效

#### Scenario: 动作互斥

- **WHEN** 同时给定 `--init` 与 `--get port`
- **THEN** 以非零码退出并说明一次只能执行一类动作

### Requirement: config --set 写入

`xkeeper config --set key=value` SHALL 仅接受 `[daemon]` 表的已知键白名单：`log_level`、`log_dir`、`monitor_interval`、`host`、`port`、`auth_token`、`log_buffer_lines`、`app_dir`，值按目标字段类型解析（`port` 为 1–65535 整数、`monitor_interval` 为正浮点、`log_buffer_lines` 为非负整数、`log_level` 限定既有日志级别集合、其余为字符串/路径）；未知键或类型不符 MUST 拒绝并以非零码退出，不落盘。写入 SHALL 采用读-改-写：文件中未被本次触碰的键、顺序与注释 SHALL 保持原样；写后 SHALL 对整个配置做整体校验，校验失败 MUST 回滚为原文件内容（磁盘上不留半成品）并以非零码退出。daemon 配置文件不存在时 set MUST 拒绝并提示先执行 `--init`。`--set` SHALL 可重复以一次写入多个键。成功 SHALL 打印写入的键与新值；目标 daemon 在线时 SHALL 提示需 reload 生效。

#### Scenario: 强类型拒绝

- **WHEN** 执行 `xkeeper config --set port=abc` 或 `xkeeper config --set foo=1`
- **THEN** 非零码退出并说明类型不符/未知键，配置文件内容不变

#### Scenario: 校验失败回滚

- **WHEN** 一次 `--set` 写入使整体配置非法（如 `--set log_level=verbose`）
- **THEN** 命令非零退出，文件内容保持写入前的原样（含注释）

#### Scenario: 未触碰内容保持

- **WHEN** 对一份带手工注释与自定义键序的合法配置执行 `--set port=8080`
- **THEN** 仅 port 值改变，其余键的顺序、值与全部注释保持原样

#### Scenario: 缺失文件拒绝

- **WHEN** daemon 配置文件不存在，执行 `xkeeper config --set port=7311`
- **THEN** 非零码退出，提示先执行 `xkeeper config --init`

### Requirement: config --get 读取

`xkeeper config --get <key>` SHALL 接受与 set 相同的白名单裸键名，打印该键的生效值：配置文件中显式设置了该键 SHALL 打印文件值，未设置 SHALL 打印内置默认值（如 `port` 未配置打印 7310，`log_dir` 未配置打印平台缺省路径）。输出 SHALL 仅为值本身（便于脚本消费）。未知键 MUST 以非零码拒绝。配置文件不存在时 SHALL 等价于全默认配置（所有键返回内置默认值）。`--get` SHALL 可重复以一次读取多个键（每键一行 `key=value` 形式；单键时仅打印值）。

#### Scenario: 回退内置默认

- **WHEN** 配置文件存在但未写 `port`，执行 `xkeeper config --get port`
- **THEN** 打印 `7310`，以 0 退出

#### Scenario: 文件值优先

- **WHEN** 配置文件写了 `port = 8080`，执行 `xkeeper config --get port`
- **THEN** 打印 `8080`

#### Scenario: 文件缺失全默认

- **WHEN** daemon 配置文件不存在，执行 `xkeeper config --get host`
- **THEN** 打印 `127.0.0.1`

### Requirement: config --delete 删除

`xkeeper config --delete <key>` SHALL 按 TOML 点路径解析删除目标：省略表名的裸键 SHALL 视为 `[daemon]` 表内叶键的简写（`port` 等价 `daemon.port`）；`表.键` 形式 SHALL 删除指定表内的叶键；点路径解析命中已知表名时 SHALL 删除整张表。可删除目标 SHALL 受与 set/get 相同的已知键白名单约束（本能力当前仅 `[daemon]` 8 键；后续能力扩展键表后自动纳入同套机制）。删除 SHALL 使目标回到「未配置」态：有内置默认值的键随删除回退默认值；未被触碰的键、顺序与注释 SHALL 保持原样；写后 SHALL 整体校验并原子落盘（校验失败回滚不落盘），daemon 在线时 SHALL 提示需 reload 生效。目标在文件中本不存在时 SHALL 幂等成功（文件不变，输出说明无变化）；未知键或未知表 MUST 以非零码拒绝且文件不变。daemon 配置文件不存在时 delete MUST 拒绝并提示先执行 `--init`。`--delete` SHALL 可重复以一次删除多个目标。

#### Scenario: 删键回退内置默认

- **WHEN** 配置文件写了 `port = 8080`，执行 `xkeeper config --delete port`
- **THEN** 文件中 `port` 行被移除、其余内容（含注释与键序）原样，`config --get port` 此后打印默认值 `7310`

#### Scenario: 目标不存在幂等成功

- **WHEN** 配置文件未设置 `host`，执行 `xkeeper config --delete host`
- **THEN** 以 0 退出，文件内容不变，输出说明该键本未设置

#### Scenario: 未知键拒绝

- **WHEN** 执行 `xkeeper config --delete foo` 或 `xkeeper config --delete webui`（当前键表无此表）
- **THEN** 非零码退出并说明未知键，配置文件内容不变

#### Scenario: 删除与回滚

- **WHEN** 一次 `--delete` 使整体配置非法（如删除必填后无法通过校验的组合场景）
- **THEN** 命令非零退出，文件内容保持写入前的原样（含注释）

### Requirement: config --edit 编辑

`xkeeper config --edit` SHALL 沿用原 `edit` 子命令语义：用 `$VISUAL`（优先）或 `$EDITOR`（均未设置时取平台默认：Unix `vi`、Windows `notepad`）打开解析后的 daemon 配置路径；配置文件不存在时 SHALL 先连同父目录创建再打开；编辑器退出后 SHALL 校验该配置，合法以 0 退出、非法逐条输出错误并以 2 退出且文件保持编辑器保存的内容不回滚；编辑器启动失败或异常退出以非零码退出。

#### Scenario: 编辑后校验通过

- **WHEN** 编辑器中写入合法配置并正常退出
- **THEN** 校验通过，以 0 退出

#### Scenario: 保存非法配置

- **WHEN** 编辑器中写入未知字段并退出
- **THEN** 错误逐条列出、退出码 2，文件保持编辑器保存的内容

### Requirement: config --init 初始化

`xkeeper config --init` SHALL 在解析后的 daemon 配置路径生成全新配置文件（父目录不存在时先创建）：渲染 `[daemon]` 表全部已知键（log_level、log_dir、monitor_interval、host、port、auth_token、log_buffer_lines、app_dir）并赋内置默认值，逐键附说明注释；`auth_token` SHALL 渲染为空值并附设置指引注释；另 SHALL 渲染一段全部注释掉的 `[app-default]` 模板段作为提示；`app_dir` SHALL 渲染相对值 `apps`（注释说明其相对配置文件所在目录解析）。目标位置已存在任何配置文件时 MUST 拒绝（非零码退出），MUST NOT 覆盖或修改既有文件。init SHALL 连带初始化 app 配置目录：按渲染的 `app_dir` 创建目录，并在其中生成 `example.toml.sample`——内容为全部注释掉的 app 配置模板（`[app]` 表与 `[program.*]` 样例），已存在时 SHALL 跳过且不报错。成功 SHALL 打印生成的配置文件路径、app 配置目录与后续步骤提示（如 `xkeeper config --edit`、`xkeeper add`）。

#### Scenario: 全新机器初始化

- **WHEN** 在既无配置文件也无 app 目录的机器执行 `xkeeper config --init`
- **THEN** 生成含全部默认值与注释的 daemon 配置文件、app 配置目录及 `example.toml.sample`，打印路径与下一步提示，以 0 退出

#### Scenario: 拒绝覆盖既有配置

- **WHEN** daemon 配置文件已存在，执行 `xkeeper config --init`
- **THEN** 非零码退出，说明文件已存在拒绝初始化，既有文件内容不变

#### Scenario: example 样例不参与加载

- **WHEN** init 完成后执行 `xkeeper list`（或触发注册表扫描）
- **THEN** `example.toml.sample` 不作为应用出现（`.sample` 后缀不被注册表扫描）

#### Scenario: 重复 init 幂等安全

- **WHEN** 连续两次执行 `xkeeper config --init`（第二次前已手工删除 daemon 配置文件但保留 app 目录与 example）
- **THEN** 第二次成功重建配置文件，`example.toml.sample` 保持原样未被改写

## REMOVED Requirements

### Requirement: edit 子命令

**Reason**: 编辑入口统一并入 `config` 子命令（`config --edit`），避免同一能力两个入口；`--init` 同时承接了「首次配置新机器」场景，独立 edit 的存在价值下降。

**Migration**: `xkeeper edit` 改用 `xkeeper config --edit`；语义（$VISUAL/$EDITOR、缺失创建、编辑后校验 0/2、不回滚）完整保留在「config --edit 编辑」requirement 中。
