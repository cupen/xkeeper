## 1. 脚手架生成核心（registry）

- [x] 1.1 `registry.rs` 新增脚手架渲染：以 `AddOptions` 扩展（args/env/workdir/source path）渲染 `AppRaw` → `toml::to_string_pretty` + 头部注释提示，生成 `app_dir/<name>.toml`；单测验证生成文件可被 `AppRaw::load` 回读且 command/work_dir 为绝对路径
- [x] 1.2 `registry::add` 路径分派：目录→目录流程；扩展名 `toml`→既有文件注册；其他普通文件→脚手架；路径不存在/目录缺 xkeeper.toml 报错不变；单测覆盖三类分派
- [x] 1.3 全量再生 + 比对：同名 add（脚手架来源）校验通过后重写文件，渲染文本与磁盘旧内容比对返回 changed/unchanged；单测覆盖 no-change、changed（含 args 差异）两分支
- [x] 1.4 `remove` 扩展：canonicalize 后父目录 == `app_dir` 的注册记录连带删除文件并返回被删路径；单测覆盖「生成文件被删」「外部本体保留」两分支
- [x] 1.5 保留字 `all`：`registry::add` 拒绝 `--name all` 及 stem 推导为 `all` 的注册，错误说明保留字；单测覆盖
- [x] 1.6 unix 无执行位警告（`#[cfg(unix)]` 测试可跳过执行位断言的双平台差异，Windows 无此概念）

## 2. CLI 层（main.rs）

- [x] 2.1 `Add` 子命令新增 `--args <字符串>`（`split_command` 切分）、`--env K=V`（可重复，缺 `=`/空 key 报 `EXIT_CONFIG`）、`--workdir <路径>`、`--apply`；`cargo build` 通过
- [x] 2.2 add 成功输出完整生成信息（文件路径、app/program 名、command、args、env、work_dir）+ changed/no-change 提示 + 在线 pending 时提示 `xkeeper apply <app>`；手工核对 help 文本
- [x] 2.3 `--apply`：在线时调用 `client::apply(Some(app), None, false)` 打印结果；离线时非零退出并提示配置已生成与补救命令；单测/集成测试覆盖离线分支（仿 `sync_tests` 假服务器）
- [x] 2.4 `Apply` 子命令帮助文本注明 `all` 为全量关键字、不可用作 app 名；`repeat add` 的 help 注明全量再生语义

## 3. apply 范围 `all` 关键字（server/supervisor）

- [x] 3.1 `server.rs` `/v1/apply` 范围解析：`app == "all"` → `ApplyScope::All`；单测覆盖 `all`/`<app>`/`<app> <program>` 三态
- [x] 3.2 shell 子命令（`shell.rs`）apply 同步该解析；确认 `/api` 复用同源逻辑无第二份实现

## 4. e2e 与回归

- [x] 4.1 `xtask` e2e 新增脚手架场景：生成（绝对路径/打印信息）→ 在线 pending → `apply abc` 拉起 → 重复执行 no-change → 改 `--args` 重跑 changed → `--apply` 一步生效 → `apply all` 全量 → `remove` 删文件；`cargo run -p xtask -- e2e` 全绿
- [x] 4.2 全量回归：`cargo test`、`cargo run -- validate`；确认既有 e2e（检出/apply/范围/add 场景）不回归

## 5. 文档收尾

- [x] 5.1 README/AGENTS.md 命令示例补充：脚手架 add、`--apply`、`apply all` 保留字说明
- [x] 5.2 前端无关改动确认：`webui/` 零修改（无 tsc/test/build 需求），记录在 PR 描述
