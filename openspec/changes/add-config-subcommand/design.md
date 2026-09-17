## Context

现状：daemon 配置 = `DaemonConfig`（`[daemon]` 8 键 `DaemonSettings` 全有内置默认 + 可选 `[app-default]`），文件缺失即全默认运行；缺省路径 unix `/etc/xkeeper/daemon.toml`、Windows `%APPDATA%\xkeeper\daemon.toml`，全局 `-c/--config` 可覆盖。已有 `edit` 子命令（$EDITOR + 编辑后校验）与 `validate` 子命令。`app_dir` 相对**配置文件所在目录**解析（`config::resolve_path(app_dir, config_dir)`），缺省 `apps`。`app_dir` 下 `.toml` 会被注册表扫描。依赖里已有 `toml_edit = "0.25"`（add 再生已在用）与 `toml`。configuration 规范现有 `edit 子命令` requirement 将被移除。

## Goals / Non-Goals

**Goals:**

- 一个子命令收口 daemon 配置的 init/set/get/delete/edit，脚本友好（get 输出裸值、set 强类型、delete 幂等、原子落盘）。
- 保留用户手写文件的注释与键序（读改写不全文重渲染）。
- 完全离线可用；在线检测只影响提示语。

**Non-Goals:**

- 不管 app 配置文件的 set/get（app 继续走 add 再生/手编 + apply）。
- 不做交互式向导、不做键值 diff 展示、不新增 daemon 在线接口。
- 不支持 `[app-default]` 键的 set（init 仅以注释模板提示其存在）。

## Decisions

- **D1 动作模型 = flag 式、一次一类动作**：`--set`（可重复 K=V）、`--get`（可重复键）、`--delete`（可重复键）、`--edit`、`--init` 用 clap `conflicts_with` 表达互斥（set/get/delete 各自多键是同类内多值，不算跨动作）；无动作 flag 走 clap 缺省 help（`arg_required_else_help` 不适用——config 带全局 `--config` 也合法，改为动作缺省时分发 help 并 0 退出）。否掉子子命令 `config set/get/...`：用户明确指定 flag 形态，且与既有 `--app`/`--restart` 这类 flag 风格一致。
- **D2 读改写用 toml_edit**：set 只改目标键的值节点、delete 只移除目标键/表节点，未触碰键的顺序、注释、空白原样保留（与 add 再生同引擎）。否掉 `toml` serde 全量反序列化再序列化——会丢用户注释。校验复用 `DaemonConfig` 解析路径（`deny_unknown_fields` + 类型），对写后文本整体 load。
- **D3 白名单 = 静态键表单一事实源**：一张表定义 8 键的 {键名, 类型解析器, 内置默认值}；`--set` 的类型校验、`--get` 的默认回退、`--init` 的默认渲染全部读这张表，三处永不漂移。类型规则：`port` u16（1–65535）、`monitor_interval` f64>0、`log_buffer_lines` usize、`log_level` ∈ {trace,debug,info,warn,error}（与 env_logger 过滤一致）、`log_dir`/`app_dir`/`host`/`auth_token` 字符串。否掉任意 TOML 路径宽松写：`deny_unknown_fields` 会让未知键在运行期才爆，违背「配置错误在写入时暴露」。
- **D4 set/delete 落盘原子性**：写临时文件（同目录 `.tmp` 后缀）→ 对临时文件整体 load 校验 → `rename` 覆盖目标；校验失败删除临时文件、目标不动。这同时给出「校验失败回滚」与「进程中断不留半成品」。配置文件缺失时 set/delete 直接拒绝并提示 `--init`（避免写动作隐式生成半份文件）；`--get` 对缺失文件按全默认回答（与 daemon `load_or_default` 运行语义一致）。
- **D5 init = 静态模板 + 同源默认值**：daemon 配置模板为代码内静态字符串，默认值经 D3 表注入；`[app-default]` 模板段全注释。`example.toml.sample` 为独立静态模板常量（`[app]` + `[program.example]` 样例全注释）。app_dir 按「渲染值 + 相对 config_dir 解析」创建（与 supervisor 运行时同一解析规则）。存在性检查先行：daemon 配置文件存在即拒（非零）；`example.toml.sample` 存在则跳过不报错（配合「只删配置文件重跑 init」的幂等场景）。`.sample` 后缀天然避开注册表扫描（只扫 `.toml`），无需 daemon 侧改动。
- **D6 在线检测 = 一次性 /v1/health 探测**：写动作（set/delete/edit/init）完成后对既有控制面地址发一次短超时 health 请求，在线则输出「执行 xkeeper reload 后生效」提示；失败/超时输出离线提示语。仅提示，不阻塞、不重试、不加 daemon 接口。config 不进 shell 客户端（本地文件操作与交互客户端无关）。
- **D7 edit 迁移**：`edit_config()` 整体迁到 `config --edit` 分支（行为零变化，含缺失文件先创建）；`Cmd::Edit` 与其 help 文本删除（**BREAKING**）；AGENTS.md 架构行、README 中 `xkeeper edit` 引用同步改为 `xkeeper config --edit`。
- **D8 delete 键参数 = TOML 点路径统一解析**：省略表名的裸键是 `[daemon]` 内叶键的简写（`port` ≡ `daemon.port`）；点路径 `表.键` 删指定表内叶键；点路径解析命中已知表名（如未来的 `webui`）时删整表。解析按 D3 键表做白名单命中：命中叶键 → 删键（回到「未配置」态，有默认值的键回退内置默认）；命中表 → 删整表；未命中 → 非零拒绝。目标不存在时幂等成功且文件不变（脚本清理友好；否掉「不存在即报错」——会逼脚本先查询再删除）。本 change 键表仅含 `[daemon]` 8 键，故表名删整表的形式在本 change 无有效目标（`--delete webui` 被拒为未知键）；后续能力扩展键表后自动解锁，机制无需再改。

## Risks / Trade-offs

- [toml_edit 对某些畸形但可解析文件的往返保真差异] → set 只在合法配置上动手（load 先行）；对现有合法配置做一次格式保留回归测试（注释+自定义键序样例）。
- [get 的路径类默认值平台差异（log_dir）] → 默认值统一从既有 `DaemonSettings::default()`/`default_log_dir()` 取，不复制字面量。
- [init 渲染 `/tmp/xkeeper/logs` 等易失路径] → 模板注释沿用 README 既有提示（易失性说明 + 固定路径建议）。
- [删除 edit 的用户迁移成本] → 仓库尚处 v0.1，README/AGENTS 同步更新；spec REMOVED 记录 Migration。

## Migration Plan

纯 CLI 面变更：新增 Config 子命令、删除 Edit 子命令。回滚 = revert 提交。无数据迁移；已有配置文件不受影响（config 子命令只按需触碰）。

## Open Questions

（无——影响 spec/方案/任务拆分的点已在拷问轮收束。）
