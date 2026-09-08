## 1. config.rs 默认值与禁用语义

- [x] 1.1 `DaemonConfig::default()` 的 `log_dir` 改为平台分支默认：unix 固定 `/tmp/xkeeper/logs`，Windows 用 `std::env::temp_dir()/xkeeper/logs`（见 design D3）。验证：config 单测断言默认 `log_dir` 为绝对路径且 unix 下等于 `/tmp/xkeeper/logs`
- [x] 1.2 `log_max_size` 四层取值与 `parse_size` 之后应用兜底映射：未在任何层声明 → `Some(50 * 1024 * 1024)`；解析值 `Some(0)` → `None`（禁用轮转）；其余原样（见 design D1/D2）。验证：config 单测覆盖三种情形（未声明得 52428800、`"0"` 得 None、显式 `"10MB"` 得 10485760）
- [x] 1.3 `log_rotate_keep` 兜底 `unwrap_or(5)` → `unwrap_or(2)`。验证：config 单测断言未声明时得 2，显式值不被覆盖
- [x] 1.4 `DaemonConfig` 校验新增绝对路径规则：`log_dir` 相对路径报错，错误信息含字段名 `daemon.log_dir` 与绝对路径修复示例；空值拒绝保留（见 design D5）。验证：config 单测——相对路径被拒、绝对路径通过、空值被拒；`xkeeper run -c <相对 log_dir 配置>` 启动失败且错误可读
- [x] 1.5 排查自产 core 配置的代码路径（`service install` 等）：若有写入/模板化 `log_dir` 的地方，确认为绝对路径或不含该字段，避免自产配置过不了自家校验。验证：`grep -rn "log_dir" src/` 逐一核对写入点
- [x] 1.6 交叉确认 Windows 分支可编译：`cargo check`（unix）通过后，若本机有 windows target 工具链则 `cargo check --target x86_64-pc-windows-msvc`，否则人工审阅 `cfg` 分支（AGENTS 约束：平台差异两条路径都要过）

## 2. 行为验证

- [x] 2.1 最小程序定义（不写轮转字段）经完整 resolve 后 `log_max_size == Some(50MB)`、`log_rotate_keep == 2`；轮转机制本身依赖既有 pump 测试（显式小参数）不重复造 50MB 用例。验证：`cargo test` 全绿
- [x] 2.2 手动冒烟：无 core 配置空启动拉起一个持续输出的程序，确认日志出现在 `/tmp/xkeeper/logs/<name>.out.log`（与 core 配置位置无关）；再以 `log_max_size = "0"` 注册同一程序确认不轮转；最后以相对 `log_dir` 的 core 配置启动，确认拒绝并输出可行动错误。验证：`ls /tmp/xkeeper/logs` 观察文件与 `.1` 轮转文件的出现/缺席，CLI 错误输出符合 1.4

## 3. 文档与收尾

- [x] 3.1 README 日志章节同步：默认值表（`log_dir` 缺省 `/tmp/xkeeper/logs` 且必须绝对路径、`log_max_size` 缺省 `50MB`、`log_rotate_keep` 缺省 2）、`"0"` 禁用写法、/tmp 易失提示（需跨重启留存则显式配置如 `/var/log/xkeeper`）、升级说明（旧日志留在 `<core 配置目录>/logs` 不迁移；相对 `log_dir` 须改绝对路径）。验证：对照 README 与实现逐条核对
- [x] 3.2 全量回归：`cargo build --release` + `cargo test`，确认无既有测试因默认值变化而失败（若有测试硬编码旧默认 `logs`/keep 5，改为显式声明而非改断言依赖新默认）。验证：构建与测试输出零失败
