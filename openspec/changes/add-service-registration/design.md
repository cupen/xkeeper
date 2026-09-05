## Context

xkeeper 现在是 daemon（`xkeeper run`）+ CLI client 架构，核心配置默认 `/etc/xkeeper.toml`（Linux）。README 手工提供 systemd unit 模板。`libc` 已是 unix 依赖（可做 euid 检查），无 systemd 相关 crate。现有 `config.rs::is_valid_name`、`main.rs::default_core_path` 可复用。

## Goals / Non-Goals

**Goals:**
- `xkeeper service install|uninstall` 一条命令完成 systemd 注册/注销，幂等可重复执行。
- unit 内容与 xkeeper 自身语义一致：SIGTERM 优雅停机、崩溃由 systemd 拉起。
- Windows 分支可编译，运行时报明确"不支持"。

**Non-Goals:**
- Windows 服务注册（后续独立 change）。
- 支持 `Type=notify`/sd_notify、socket activation、user-level systemd（`--user` unit 装到 `~/.config/systemd/user`）。
- 管理其他进程的 systemd unit（xkeeper 只注册它自己）。

## Decisions

1. **调用系统 `systemctl`，不引入 systemd crate**。crate 需要链接 libsystemd 且并不提供 unit 安装语义；`systemctl` 在任何 systemd 主机上必然存在。`std::process::Command` 执行 `daemon-reload`/`enable`/`start`/`stop`/`disable`，捕获 stderr 用于报错。*备选*：手写 DBus 调用 — 复杂度不成比例，放弃。

2. **代码放在新模块 `src/service.rs`，用 `#[cfg(unix)]` / `#[cfg(windows)]` 分离**。CLI 子命令在所有平台都存在（发现性好），Windows 分支直接 `bail!("service registration is not supported on Windows yet")`。unit 模板渲染、名称校验等纯逻辑做成平台无关函数便于测试。

3. **unit 模板为固定骨架 + 占位符**（[Unit] After=network.target；[Service] ExecStart/Restart=always/RestartSec=3/KillSignal=SIGTERM/TimeoutStopSec；[Install] WantedBy=multi-user.target）。不做可配置模板文件 — 字段少、出错面小；用户要定制可直接改生成后的 unit。
   - `ExecStart`：`std::env::current_exe()` 规范化后的绝对路径 + `run -c <核心配置绝对路径>`。核心配置不要求存在（daemon 支持无配置缺省运行）。
   - `TimeoutStopSec`：尝试加载核心配置 + 已注册 app，取所有 program `stop_timeout` 最大值的 2 倍加 10s 余量（粗略覆盖顺序停止的最坏情况）；配置不可加载时回退 90s。
   - `--user <name>` 仅写入 `User=` 行；缺省省略（root 运行）。

4. **root 检查用 `libc::geteuid() == 0`**，在任何写操作之前执行。Windows 检查在前、平台错误先返回。

5. **覆盖语义**：目标 unit 已存在时读取现有内容逐字节比较 — 相同则跳过写文件、仅执行 `daemon-reload` + `enable` 收敛状态（幂等）；不同则报错提示 `--force`。`--force` 直接覆盖并重新 reload + enable。

6. **install 步骤顺序**：权限检查 → 解析路径 → 渲染 unit → 写文件 → `daemon-reload` → `enable` →（`--now` 时 `start`）。写文件后任一步失败即报错退出，不自动回滚文件（`uninstall` 可清理；半安装状态在错误信息中说明）。

7. **unit 名校验**：只允许字母数字、`.` `_` `-`，且非空、不以 `.` 结尾；非法时在写文件前失败。复用场景与 `is_valid_name` 不同，单独实现小函数。

## Risks / Trade-offs

- [无法在 CI 中测真实 systemctl] → systemctl 调用集中在一个函数，集成行为靠手动验证；模板渲染与名称校验做单元测试（golden 断言）。
- [TimeoutStopSec 估算可能偏小（程序极多、串行停止）] → 2×max+10s 覆盖常规场景，且 xkeeper 自身 Ctrl+C 逻辑先于 systemd 强杀；文档说明可手改 unit。
- [用户移动了二进制或配置后 unit 失效] → 卸载重装即可；不追 symlink/迁移。
- [`--now` 启动失败（端口占用等）] → unit 已注册且 enabled，错误信息明确指出是 start 失败而非安装失败。

## Migration Plan

纯新增子命令，无迁移。README 中"作为系统服务运行"改为推荐 `xkeeper service install`，手工 unit 方式保留为备选。回滚 = `xkeeper service uninstall` 或直接删除生成的 unit。

## Open Questions

无。
