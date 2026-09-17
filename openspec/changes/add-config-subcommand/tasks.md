## 1. CLI 骨架与 edit 迁移

- [x] 1.1 main.rs 定义 `Cmd::Config`（`--set K=V` 可重复、`--get <key>` 可重复、`--delete <key>` 可重复、`--edit`、`--init`，clap conflicts_with 互斥；无动作 flag 分发帮助并 0 退出）与分发分支；单测覆盖参数解析与动作互斥拒绝
- [x] 1.2 迁移 `edit_config()` 到 `config --edit` 分支（行为不变：$VISUAL/$EDITOR、缺失创建、校验 0/2、不回滚），删除 `Cmd::Edit` 与分发；`cargo build` 过、全库 grep 无 `Cmd::Edit` 残留

## 2. 白名单键表与 set/get/delete

- [x] 2.1 config.rs 建静态键表：8 键的 {类型解析器, 内置默认值}（port u16 1–65535、monitor_interval f64>0、log_buffer_lines usize、log_level 枚举、其余字符串），默认值取自 `DaemonSettings::default()`/`default_log_dir()` 不复制字面量；单测覆盖每键合法/非法值
- [x] 2.2 `--get`：缺失文件按全默认回答、文件值优先回退默认、单键裸值/多键 `key=value`、未知键非零拒绝；单测覆盖三种来源场景
- [x] 2.3 `--set`：toml_edit 读改写只动目标键；写临时文件 → 整体 load 校验 → rename 落盘，失败删临时文件目标不动；缺失文件拒绝提示 --init；可重复多键；单测覆盖「未触碰注释与键序保持」「校验失败回滚」「缺失文件拒绝」
- [x] 2.4 `--delete`：键参数按 TOML 点路径解析（裸键 = `[daemon]` 叶键简写、`表.键` = 指定表叶键、命中已知表名 = 删整表；本 change 键表仅 [daemon] 8 键，表名形式暂无有效目标）；命中叶键用 toml_edit 移除节点（注释与键序保持）、回到未配置态；目标不存在幂等成功文件不变；未知键非零拒绝；原子落盘与校验回滚同 set；缺失文件拒绝提示 --init；单测覆盖「删键回退默认」「幂等无变化」「未知键拒绝」「未触碰内容保持」

## 3. init

- [x] 3.1 daemon 配置模板渲染：`[daemon]` 8 键默认值 + 逐键注释（auth_token 空值 + 指引注释、app_dir 相对值注释说明相对配置目录解析）+ 全注释 `[app-default]` 模板段；单测断言渲染结果可被 `DaemonConfig` load 且值等于内置默认
- [x] 3.2 init 动作：存在即拒（文件不动、非零）、父目录创建、app_dir 按渲染值相对 config_dir 解析创建、`example.toml.sample` 全注释模板（存在跳过）、成功打印路径与下一步提示；单测覆盖「全新初始化」「拒绝覆盖」「example 幂等跳过」
- [x] 3.3 注册表扫描不加载 `.sample` 后缀验证：对 init 产物目录跑 list/扫描路径测试，断言无 example 应用出现

## 4. 在线提示

- [x] 4.1 set/delete/edit/init 成功后一次性 `/v1/health` 短超时探测（在线 → 提示 reload 生效；离线/失败 → 离线提示语），复用既有控制面地址解析；探测不重试不阻塞；单测以 mock 地址覆盖两分支

## 5. e2e 与文档

- [x] 5.1 xtask e2e 增加 config 场景段：init → get 默认值 → set 合法键 → get 文件值 → 非法 set 拒绝且文件不变 → delete 已设键回退默认 → delete 幂等无变化 → 未知键 delete 拒绝 → 已存在 init 拒绝 → example.toml.sample 不进 list → `config --edit`（EDITOR=fake 脚本）校验路径；`cargo run -p xtask -- e2e` 全绿
- [x] 5.2 README/AGENTS.md：config 子命令用法（五动作示例）、`xkeeper edit` 引用全部改为 `xkeeper config --edit`、架构行更新（CLI 子命令清单去 edit 增 config）
- [ ] 5.3 双平台检查：`cargo check`（unix）与 Windows 目标交叉编译检查（`cargo check --target x86_64-pc-windows-msvc` 或 CI 等价），init 模板中平台差异默认值（log_dir）在 windows 渲染正确
