# Proposal: add-process-scaffold

## Why

`xkeeper add` 目前只接受「已存在 xkeeper.toml 的部署目录」或「现成 .toml 配置文件」，把一个裸可执行程序纳入守护必须先手工编写部署配置再注册，上手门槛高。目标是让 `xkeeper add ./path/to/proc --name abc` 直接从可执行程序脚手架生成 app 配置（落在 `app_dir`，如 `conf.d/abc.toml`），并打通「生成 → pending → apply」链路：重复执行检测变更并提示需 apply，`--apply` 一步到位。同时修正 `xkeeper apply all` 被当作名为 "all" 的 app 处理的语义缺口。

## What Changes

- `add` 按路径形态智能分派：目录 → 现有流程（须含 xkeeper.toml）；`.toml` 文件 → 现有注册流程；其他存在的普通文件（可执行程序）→ 脚手架生成新流程。
- 脚手架生成：在 `app_dir` 下生成**真实文件** `<name>.toml`（无链接）；`command` 一律写绝对路径（canonicalize），`work_dir` 缺省为执行 add 时的当前目录（绝对路径写入）。
- 新增生成 flag：`--args "<字符串>"`（shell 词法切分为 argv）、`--env K=V`（可重复）、`--workdir <路径>`；app 名缺省取可执行文件名 stem，`--name` 优先；program 名 = app 名（`[program.<name>]`）。
- 重复执行同名 add = **全量再生**（以本次参数重写整个文件，手改内容会被覆盖——已确认接受）；再生后与磁盘旧内容比对：有差异 → 打印 changed 并提示需 `xkeeper apply <app>`（或 `xkeeper apply` 全量）；无差异 → 打印 no change。
- `add --apply`：注册/再生成功后立即对该 app 执行 apply（仅本 app 范围）；守护进程离线时报错、非零退出，提示配置已生成及后续 `xkeeper apply <app>` 补救方式。
- 执行后完整打印生成信息：配置文件路径、app 名、program 名、command、args、env、work_dir、下一步提示（非交互纯文本，脚本友好）。
- `xkeeper apply all`：字面量 `all` 保留为全量关键字，与裸 `xkeeper apply` 等价；文档注明 `all` 不可用作 app 名。**BREAKING**（对确有名为 "all" 的 app 的部署，其 apply 语法需改用裸 `apply`；实际影响可忽略）。
- `remove` 语义扩展：注册记录为「位于 `app_dir` 内的真实文件」时，remove 连带删除该文件并打印路径；外部部署目录注册（链接）的既有语义不变（只删链接、保留本体）。
- webui 不新增 add 表单（CLI-only 变更）；`/v1`、`/api`、WS 帧无结构改动。

## Capabilities

### New Capabilities

（无）

### Modified Capabilities

- `app-registry`: add 的注册来源新增「可执行程序脚手架生成」（绝对路径、work_dir 缺省、新 flag、全量再生与变更提示、`--apply`）；remove 对 `app_dir` 内真实文件的注销语义；离线/在线同步行为沿用。
- `apply-workflow`: apply 范围新增字面量 `all` 关键字（= 全量）。

## Impact

- `src/main.rs`：`Add` 子命令新增 `--args/--env/--workdir/--apply` flag 与路径分派；`Apply` 子命令 `all` 关键字。
- `src/registry.rs`：脚手架生成（模板渲染、全量再生、变更比对）、`remove` 对 app_dir 内真实文件的删除、add 后返回 changed 信息供 CLI 打印。
- `src/supervisor.rs` / `src/server.rs`：apply 范围解析将 `all` 视为 `ApplyScope::All`。
- `xtask` e2e：新增脚手架场景（生成/重复执行 no-change/changed 提示/`--apply`/`apply all`/remove 删文件）。
- 平台差异：无新平台分支（文件写入与路径处理均跨平台）；unix 无执行位仅警告。
