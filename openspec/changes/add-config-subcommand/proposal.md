## Why

xkeeper 目前管理 daemon 配置只有两条路：手写 TOML（配 `validate` 校验）和 `edit` 子命令（$EDITOR 全文编辑）。缺一个脚本化、原子、可发现的配置入口——初始化一台新机器只能 `edit` 出一个空文件或手工抄 README；改一个端口要在编辑器里找行；`--get` 查询生效值根本不存在；删掉一个手工覆盖、让键回退内置默认只能进编辑器找行删除。`config` 子命令把 init/set/get/delete/edit 收进一个统一入口，同时保留既有离线可用、写后必校验的哲学。

## What Changes

- 新增 `xkeeper config` 子命令，五个动作 flag：
  - `--set key=value`（可重复）：白名单强类型写入 `[daemon]` 表已知键；读-改-写后整体 validate，失败回滚不落盘；daemon 在线时打印需 reload 提示，离线完全可用。
  - `--get <key>`：打印生效值——文件显式值优先，未设置回退内置默认（如 port 未配置打印 7310）。
  - `--delete <key>`（可重复）：按 TOML 点路径解析删除——省略表名的裸键是 `[daemon]` 内叶键的简写；点路径 `表.键` 删指定表内叶键；点路径解析命中已知表名时删整表。删除即回到「未配置」态（有默认值的键回退内置默认）。目标不存在时幂等成功（文件不变），未知键拒绝。写路径（读-改-写 + 整体校验 + 原子落盘）与在线提示与 `--set` 相同。本 change 的已知键表仅含 `[daemon]` 8 键；后续能力（如 `[webui]` 段）扩展键表后自动获得同套 set/get/delete 机制。
  - `--edit`：$VISUAL/$EDITOR 打开 + 编辑后校验（沿用既有 edit 行为，含文件缺失时创建）。
  - `--init`：初始化 daemon 配置文件——渲染 `[daemon]` 全部 8 键的内置默认值（逐键注释说明）+ 注释态 `[app-default]` 模板段；目标文件已存在 MUST 拒绝（不覆盖）；连带创建 app 配置目录与 `app_dir/example.toml.sample`（全注释 app 配置模板，`.sample` 后缀避开注册表扫描，已存在则跳过）。
- **BREAKING** 删除独立 `xkeeper edit` 子命令，编辑入口统一为 `config --edit`。
- 作用范围仅 daemon 配置文件（`--config` 指定或平台缺省路径）；app 配置继续走 add/apply 体系，不进入 config 子命令的 set/get 键空间。

## Capabilities

### New Capabilities

（无。）

### Modified Capabilities

- `configuration`: 新增 `config 子命令`（动作集合与形态）、`config --set 写入`、`config --get 读取`、`config --delete 删除`、`config --init 初始化` 五条 requirement；**移除** `edit 子命令` requirement（编辑入口并入 `config --edit`，行为语义随之迁移）。

## Impact

- `src/main.rs`：删除 `Edit` 子命令与分发分支，新增 `Config` 子命令与五个动作分发。
- `src/config.rs`：新增读改写辅助（toml_edit 保持未触碰键的格式与注释，含键/表节点删除）、生效值合并查询、init 模板渲染。
- 复用既有 `$VISUAL/$EDITOR` 解析与 validate 流程（原 `edit_config()` 逻辑迁入 config 动作）。
- `openspec/specs/configuration/spec.md` 归档时合入上述增删。
- `xtask/src/e2e.rs` 增加 config 子命令场景；`README.md` / `AGENTS.md` 增补 config 用法、移除 edit 引用。
- 不影响：supervisor/pump/webui/控制面路由；`/v1`、`/api`、WS 语义零改动（config 是纯本地文件操作，不新增在线接口；「在线检测」仅用于提示语）。
