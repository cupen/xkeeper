## 1. service 模块骨架

- [x] 1.1 新建 `src/service.rs`：定义 `ServiceOptions`（unit 名、运行用户、force、now）与 `install/uninstall` 入口；Windows 下 `install`/`uninstall` 返回 "not supported on Windows" 错误。验证：`cargo check` 在两个平台目标（cfg 分支）通过，Windows 分支函数编译无警告
- [x] 1.2 实现 unit 名校验函数（仅字母数字与 `.` `_` `-`，非空、不以 `.` 结尾）并写单元测试（合法名、含 `/`、空白、结尾 `.` 均拒绝）。验证：`cargo test service::`
- [x] 1.3 实现 unit 模板渲染函数：ExecStart（可执行文件绝对路径 + `run -c <核心配置绝对路径>`）、Restart=always、RestartSec=3、KillSignal=SIGTERM、TimeoutStopSec、可选 `User=`、[Install] WantedBy=multi-user.target；单元测试 golden 断言（含/不含 `User=` 两种）。验证：`cargo test service::`

## 2. install / uninstall 实现

- [x] 2.1 实现 root 检查（`libc::geteuid()`，仅 unix）与路径解析（`current_exe` 规范化、核心配置 `-c` 或平台默认转绝对路径）。验证：单元测试覆盖路径解析（core 已存在与缺省两种输入）
- [x] 2.2 实现 `install`：权限/平台检查 → 名称校验 → 渲染 → 写 `/etc/systemd/system/<name>.service` → `daemon-reload` → `enable` →（`--now`）`start`；已存在 unit 内容相同则幂等跳过写、不同则报错提示 `--force`；systemctl 调用封装为单一函数并捕获 stderr。验证：单元测试覆盖"内容相同幂等/不同拒绝/--force 覆盖"的决策函数（systemctl 调用以注入方式 mock 或拆出纯逻辑）
- [x] 2.3 实现 `uninstall`：`stop`（在运行时）→ `disable` → 删除 unit 文件 → `daemon-reload`；unit 不存在时幂等成功并提示未安装。验证：单元测试覆盖文件不存在分支的幂等决策逻辑
- [x] 2.4 实现非 root 时不写任何文件：权限检查在所有副作用之前。验证：代码审查 + 非 root 环境手动执行 `xkeeper service install` 报错且 `/etc/systemd/system/` 无新文件

## 3. CLI 接入

- [x] 3.1 `src/main.rs` 新增 `service install|uninstall` 子命令组（`--now`、`--force`、`--name`、`--user`、全局 `-c` 生效）并分发到 `service::install/uninstall`。验证：`cargo run -- service --help` 输出正确，Windows 下 `cargo run -- service install` 报不支持错误
- [x] 3.2 `validate` 或 `service install` 前置提示：安装时若核心配置/已注册 app 可加载，计算 `TimeoutStopSec`（2×最大 `stop_timeout` + 10s），不可加载回退 90s。验证：单元测试覆盖可加载与回退两条路径

## 4. 文档与整体验证

- [x] 4.1 更新 README："作为系统服务运行" 改为 `xkeeper service install` / `uninstall` 用法，保留手工 unit 为备选；配置参考补充 `service` 命令说明。验证：README 阅读自查，命令与 `--help` 输出一致
- [x] 4.2 Linux 环境端到端手动验证：root 下 `xkeeper service install --now` → `systemctl status xkeeper` running、`systemctl is-enabled xkeeper` enabled → 杀掉 xkeeper 进程被 systemd 拉起 → `xkeeper service uninstall` 清理干净。验证：按上述步骤逐条确认并在 PR 描述附输出
- [x] 4.3 `cargo test` 全量通过、`cargo clippy` 无新警告。验证：CI/本地输出
