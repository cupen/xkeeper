# Design: add-process-scaffold

## Context

`add` 现有三类来源分派逻辑已在 `src/main.rs`（CLI flag 与 legacy 导入）与 `src/registry.rs::add`（校验、链接、flag 写入）中；`apply` 链路（reload→pending→apply、`ApplyScope::All|App|Program`）在 `src/supervisor.rs` 与 `src/server.rs:410` 落地，`/v1/apply` 已可用。生成的 app 配置格式即 `config::AppRaw`（`[app]` + `[program.*]`），`registry::apply_flags` 已用 toml_edit 做幂等 flag 写入。这些能力全部复用，不新增外部依赖。

## Goals / Non-Goals

**Goals:**

- `add` 对「普通文件」来源做脚手架：渲染 `AppRaw` TOML → 写入 `app_dir/<name>.toml` 真实文件 → 走既有校验与在线同步。
- 重复执行的可预测语义：全量再生 + 内容比对 + changed/no-change 提示。
- `--apply` 复用 `client::Client::apply(app=Some(name), program=None)`，与本 app 范围 apply 完全同源。
- `apply all` 关键字在 CLI 与 `/v1/apply` 两侧同源收口。

**Non-Goals:**

- webui add 表单、`/api`/WS 新帧（CLI-only）。
- 从目录脚手架（目录仍须含 `xkeeper.toml`）。
- 合并式（merge）再生成——用户已明确选择全量再生。
- 独立 `xkeeper-apply` 二进制（`apply` 保持子命令）。

## Decisions

1. **复用 `add` + 路径形态分派**（而非新子命令 `create/new`）：与用户心智 `xkeeper add ./proc --name abc` 一致，分派规则机械可判——目录→目录流程；扩展名为 `toml` 的文件→配置注册流程；其他普通文件→脚手架。备选的独立子命令被否：多一条命令的记忆成本，收益仅是职责纯正。
2. **生成文件直接落 `app_dir`、remove 连带删文件**：按用户明确要求（`app_dir=conf.d` 下生成）。remove 判据用「canonicalize 后路径的父目录 == `app_dir`」，而非 symlink 探测——Windows 上 `make_link` 有 hardlink 回退，链接类型不可靠；路径判据跨平台确定。外部目录注册的既有语义（只删链接、保留本体）不动。
3. **全量再生而非 merge upsert**：用户拍板，语义简单可预期；代价是文件上的手工编辑会被覆盖——由「再生后打印 changed + 需 apply」提示与完整打印的生成内容兜底可见性。merge 方案（`apply_flags` 扩展）被否：无法表达「去掉某个 env/args」的删除意图，重复执行行为依赖隐式合并规则，不可预测。
4. **变更比对在渲染文本层做**：再生内容渲染为字符串后与磁盘旧内容直接比较；有差异即 changed。不解析回 `AppRaw` 比较语义树——生成器是确定性的，文本等价即语义等价；语义层比较反而要处理注释丢失等伪差异。
5. **模板渲染手写 TOML 字符串**（`toml` crate 序列化 `AppRaw` + 头部注释）：`AppRaw` 已 `Serialize`，`toml::to_string_pretty` 保证可回读；注释提示（可编辑字段示例）以字符串前缀拼接。不引入 `toml_edit` 生成（保留注释的能力只在编辑既有文件时需要，再生语义下无意义）。
6. **`--args` 用既有 `config::split_command` 切分**：与 app 配置里单行 command 的切分规则同源（引号感知、无 shell）；`--env K=V` 可重复，缺 `=` 或空 key 报 `EXIT_CONFIG`。
7. **`all` 保留字在 `server.rs` 范围解析处收口**：`app == "all"` → `ApplyScope::All`；同时在 `registry::add` 拒绝 `--name all`（含 stem 推导结果为 `all`），错误信息说明保留字原因。备选「注册表里无 all 才视作全量」被否：行为依赖状态，脚本不可预测。
8. **`--apply` 的离线失败用非零退出**：与 `sync_if_online` 的「不静默」哲学一致——用户显式要求生效而未生效，必须失败可见；错误信息给出补救路径（`xkeeper apply abc`）。在线路径：add（含 sync_if_online 的 reload）成功后调用 `client::apply(Some(app), None, false)` 并打印 `render_apply` 等价结果。
9. **CLI 输出完整打印生成信息**：打印文件路径、app/program 名、command、args、env、work_dir、下一步提示；纯文本、无交互确认，保持脚本友好与既有 CLI 风格。
10. **unix 无执行位仅警告**：`add` 不做执行位硬校验（Windows 无此概念，双平台行为需一致），警告文本提示可能启动失败。

## Risks / Trade-offs

- [全量再生覆盖手改] → 靠 changed 提示与完整打印暴露；文档/help 注明「重复 add 会以本次参数重写文件」。
- [换目录重跑 add 会连带改写 work_dir（缺省=当前目录）] → 生成信息完整打印 work_dir，肉眼可查；可用 `--workdir` 显式固定。
- [`all` 成为保留字是对既定行为的 BREAKING] → 实际影响趋近于零（名为 "all" 的 app 属病态命名）；validate/add 两侧拒绝给出明确错误。
- [remove 删除生成文件是不可恢复操作] → 删除后打印被删路径；不做回收站/备份（与「remove 不删外部本体」的保守语义形成对照，help 注明差异）。
- [脚手架生成文件位于 app_dir，被周期重扫当作配置本体] → 与设计一致（注册记录即文件本身）；文件非法时走既有单 app 隔离路径，无新增风险。

## Migration Plan

纯增量，无数据迁移。`apply all` 的 BREAKING 通过 add 拒绝注册保留字 + 帮助文本双保险。回滚即 revert 提交，生成的 `app_dir/*.toml` 是普通文件，旧二进制亦能读取。

## Open Questions

（无——次要点已记为假设：program 名=app 名；app 名缺省取可执行文件 stem；`.toml` 扩展名一律走注册流程；e2e 覆盖脚手架场景。）
