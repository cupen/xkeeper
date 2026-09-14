## MODIFIED Requirements

### Requirement: 注册应用（add）

`xkeeper add [path] [flags]` SHALL 注册一个应用，按 `path` 形态分派：

- `path` 为部署目录（取其中默认名 `xkeeper.toml`，如 `xkeeper add .`）或直接指向 `.toml` 配置文件时，走既有注册流程：校验（文件存在、格式合法、程序名全局唯一、依赖不成环）；在 `app_dir` 创建以应用名命名的 `<name>.toml` 链接指向配置本体（链接即注册记录）；将微调 flag 幂等写入该 app 配置的 `[app]` 表（`--autostart/--no-autostart`、`--autorestart`、`--restart-backoff`、`--priority` 等，作为应用级默认；flag 写入失败 SHALL 回滚整个注册）。
- `path` 为其他存在的普通文件（可执行程序）时，走脚手架生成流程：SHALL 在 `app_dir` 下生成真实配置文件 `<name>.toml`（非链接，注册记录即文件本身），内容为单程序 app：program 名 = 应用名，`[app]` 表与 `[program.<name>]` 表按给定 flag 渲染。`command` SHALL 写绝对路径（canonicalize 解析后）；`work_dir` SHALL 缺省取执行 add 命令时的当前工作目录（以绝对路径写入），可用 `--workdir <路径>` 覆盖。启动参数 SHALL 以 `--args "<字符串>"` 传入（按 shell 词法切分为 argv），环境变量 SHALL 以 `--env K=V` 传入（可重复）。应用名 SHALL 取 `--name`，缺省取可执行文件名（去扩展名的 stem）。脚手架 SHALL 至少同样支持：`--name`、`--description`、`--autostart/--no-autostart`、`--autorestart`、`--restart-backoff`、`--priority`。

应用名 `all` 为保留字（见 apply-workflow），add MUST 拒绝以 `all` 注册（含 `--name all` 与文件名 stem 推导出的 `all`）。

重复执行：目录/`.toml` 来源的同名 add 保持既有 upsert 语义（更新链接与 flag，不产生重复）；可执行程序来源的同名 add SHALL **全量再生**——以本次给定参数重写整个生成文件（未给 flag 的字段回到缺省，文件上手工编辑的内容会被覆盖），再生前 MUST 校验通过才落盘。再生后 SHALL 将新内容与磁盘旧内容比对：有差异时打印变更提示并明确指出需执行 `xkeeper apply <app>`（或全量 `xkeeper apply`）方可生效；无差异时打印「无变更」类提示。

`add --apply` SHALL 在注册/再生成功后立即对该 app 执行一次 apply（仅本 app 范围，MUST NOT 波及其他 app 的 pending）；守护进程离线时 MUST 以非零码报错退出，错误信息说明配置已生成并提示守护进程在线后执行 `xkeeper apply <app>`。

注册与再生成功后 SHALL 以非交互纯文本完整打印：生成的配置文件路径、应用名、程序名、command、args、env、work_dir，以及下一步提示（在线时提示 pending 与 apply 命令）。在线同步行为沿用「离线可用」requirement：注册持久化后触发重扫进入 pending，由 `xkeeper apply` 落地。

#### Scenario: 从部署目录注册并指定名字

- **WHEN** 在应用部署目录内执行 `xkeeper add . --name gateway --autorestart on-failure`
- **THEN** `app_dir` 出现以 gateway 命名的链接，部署目录 app 配置的 `[app]` 表写入 `autorestart = "on-failure"`，命令成功

#### Scenario: 未指定 --name 时取目录名

- **WHEN** 在 `/opt/myapp` 目录内执行 `xkeeper add .`（未给 `--name`）
- **THEN** 应用名为 `myapp`

#### Scenario: 守护进程运行中注册即时生效

- **WHEN** 守护进程运行中执行 add（任一来源）
- **THEN** 注册持久化，并等效于触发一次 reload（重扫检出）：该应用进入 pending，程序尚未拉起；执行 `xkeeper apply` 后按 autostart 语义生效

#### Scenario: 从可执行程序脚手架生成并注册

- **WHEN** 在 `/opt/srv` 目录内执行 `xkeeper add ./web-server --name abc --args "--port 8080" --env LOG=debug`
- **THEN** `app_dir` 下生成真实文件 `abc.toml`，其中 program 名为 `abc`，`command` 为 `/opt/srv/web-server` 的绝对路径，`args = ["--port", "8080"]`，`env` 含 `LOG=debug`，`work_dir` 为 `/opt/srv`；命令完整打印上述生成信息并以 0 退出

#### Scenario: 未指定 --name 时取可执行文件名

- **WHEN** 执行 `xkeeper add /opt/srv/web-server`（未给 `--name`）
- **THEN** 应用名为 `web-server`

#### Scenario: 生成配置使用绝对路径

- **WHEN** 以相对路径 `./target/release/proc` 执行脚手架 add
- **THEN** 生成文件中 `command` 与 `work_dir` 均为绝对路径

#### Scenario: 重复执行无差异提示无变更

- **WHEN** 以完全相同参数对同名 app 再次执行脚手架 add
- **THEN** 文件内容不变，输出「无变更」类提示，以 0 退出

#### Scenario: 重复执行有差异提示需 apply

- **WHEN** 以不同 `--args` 对同名 app 再次执行脚手架 add
- **THEN** 生成文件被全量重写（手工编辑内容被覆盖），输出变更提示并指明需 `xkeeper apply <app>` 生效；守护进程在线时该差异已进入 pending

#### Scenario: --apply 在线一步生效

- **WHEN** 守护进程运行中执行 `xkeeper add ./proc --name abc --apply`
- **THEN** 配置生成后自动以本 app 范围执行 apply，`abc` 的程序被拉起，其他 app 的 pending 不受影响，输出含 apply 结果

#### Scenario: --apply 离线报错

- **WHEN** 守护进程离线时执行 `xkeeper add ./proc --name abc --apply`
- **THEN** 以非零码退出，错误说明配置已生成于 `<app_dir>/abc.toml`，并提示守护进程在线后执行 `xkeeper apply abc`

#### Scenario: 保留字 all 不可注册

- **WHEN** 执行 `xkeeper add ./proc --name all`
- **THEN** 注册被拒绝并说明 `all` 为 apply 全量保留字

#### Scenario: 注册被校验拒绝

- **WHEN** add 指向不存在的文件，或生成的/给定的配置校验失败
- **THEN** 注册失败并指明原因，`app_dir` 保持不变

#### Scenario: 重复注册更新而非重复条目

- **WHEN** 对同名应用（目录或 `.toml` 来源）再次 add 并修改 flag
- **THEN** 链接与 `[app]` 表字段被更新，不产生重复注册

### Requirement: 移除与查看

`xkeeper remove <app>` SHALL 删除 `app_dir` 中该应用的注册记录并注销；守护进程运行中时其程序 SHALL 被停止并清理。注册记录为链接（外部部署目录注册）时，部署目录中的配置本体 MUST NOT 被删除；注册记录为位于 `app_dir` 内的真实文件（脚手架生成）时，SHALL 连带删除该文件并打印被删除的路径。`xkeeper list` SHALL 扫描 `app_dir` 输出全部已注册应用（名称、配置本体路径、生效的默认值、程序概要）。

#### Scenario: 移除运行中的应用

- **WHEN** 守护进程运行中执行 `xkeeper remove web`
- **THEN** web 的程序被停止并从状态中消失，`app_dir/web.toml` 注册记录被清理，部署目录中的 `xkeeper.toml` 仍存在

#### Scenario: 移除脚手架生成的应用删除生成文件

- **WHEN** 对脚手架生成的 app 执行 `xkeeper remove abc`
- **THEN** `app_dir/abc.toml` 被删除且路径被打印，该 app 从注册表中消失

#### Scenario: 列出注册表

- **WHEN** 执行 `xkeeper list`
- **THEN** 输出每个已注册应用的名称、配置本体路径与生效的默认值
