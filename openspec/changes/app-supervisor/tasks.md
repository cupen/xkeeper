# Tasks: app-supervisor

实现顺序按依赖排列：分层配置与注册表 → 运行时/平台 → 日志泵 → 健康检查 → 控制平面服务端 → 排序与 reload → CLI 客户端 → 收尾。设计依据见 [design.md](design.md)，行为契约见 `specs/*/spec.md`。

## 1. 分层配置与注册表（src/config.rs、src/registry.rs）

- [x] 1.1 实现 core/app 分层 schema：core（`[daemon]` 全字段含 `app_dir` + 可选 `[app-default]`，任何应用条目被未知字段校验拒绝）、app（可选 `[app]` 应用级默认 + `[program.<name>]` map 接口 + 扁平字段 `work_dir`/`env`/`log_max_size`/`log_rotate_keep`/`health_check` 单字符串协议分发/单行 command 拆分）、平台默认路径解析（Linux `/etc/xkeeper.toml`、Windows `%APPDATA%\xkeeper\xkeeper.toml`、`app_dir` 缺省 core 同级 `apps/`）与 `-c` 覆盖（验证：schema 反序列化与路径解析单元测试，`cargo test config::`）
- [x] 1.2 实现四层字段优先级解析（`[program.*]` 显式 > app `[app]` > core `[app-default]` > 内置默认）与全局默认值；空启动语义：core 文件缺失时以默认值空跑（验证：优先级单元测试 + 空启动集成测试）
- [x] 1.3 扩展校验：`depends_on` 成环检测（可跨应用）、数值范围、`log_max_size` 人类可读大小解析、`health_check` 协议分发解析与校验、单行 command 与显式 args 互斥、跨应用程序名（映射键）全局唯一、应用名/程序名安全字符；错误信息包含文件路径、应用与程序名（验证：逐条非法样例单元测试）
- [x] 1.4 实现注册表操作：add（校验 + 幂等 upsert + flag 写入 [app]；core 配置不再被写入——注册记录即 app_dir 链接）、remove、list；core 配置原子写入（临时文件 + 替换），文件缺失时创建；add/remove 同步维护 `app_dir` 链接（symlink → 同卷硬链接 → 告警跳过的降级链）（验证：注册表与链接单元测试 + 离线 add/remove/list 集成测试）
- [x] 1.5 实现 legacy 导入：检测含 `[daemon]` 段的 v0.1 单文件，`[[program]]` 注册为应用、`[daemon]` 设置输出并入 core 的提示、不修改原文件（验证：v0.1 `config.toml` 固定夹具的导入测试）
- [x] 1.6 实现按 app 定义哈希与 diff（新增/删除/变更三类）（验证：diff 单元测试三场景）
- [x] 1.7 `validate` 子命令适配新模型：默认校验 core 及注册表引用的全部 app 文件，`validate <path>` 单独校验一个 app 文件（验证：合法/非法配置下 validate 的集成测试）

## 2. 平台层与进程运行时（src/platform.rs、src/program.rs）

- [ ] 2.1 Unix 增加 spawn 后独立进程组（pre_exec setpgid）与 killpg 停止路径；Windows 维持 Job Object 兜底；平台差异全部留在 platform.rs（验证：cfg(unix) 测试覆盖孙进程清理；本机 `cargo build` 无回归，Windows 冒烟不回归）
- [x] 2.2 重构状态机为 7 态（stopped/starting/running/stopping/exited/backoff/fatal）+ unhealthy 标记，引入 `startsecs`（达标才算 started）与 `startretries` 预算（耗尽 → fatal 并记录原因）（验证：真实子进程状态机单元测试覆盖每个迁移边）
- [x] 2.3 实现重启策略 `never/on-failure/always` 与 `exit_codes` 期望退出码判定，稳态退避计数与 startretries 预算相互独立（验证：三种策略 × 期望/非期望退出码的单元测试）
- [x] 2.4 统一优雅停止路径：请求优雅终止 → 等待 `stop_timeout` → 强制终止并记录使用了强杀；`stopped` 终态确定（验证：超时强杀集成测试 + 正常停止测试）

## 3. 日志泵（src/pump.rs）

- [x] 3.1 spawn 改造为管道 + 每流泵线程：读 → 追加写当前文件，写失败告警不阻塞；程序重启后追加不覆盖（验证：集成测试断言子进程输出出现在 out/err 日志且重启后仍在追加）
- [x] 3.2 写路径上实现按大小轮转：超过 `max_size` 时 close → 重命名 `.1/.2/...` → 保留 `rotate_keep` 份 → 重新打开（验证：临时目录驱动的轮转单元测试 + 集成测试产生轮转文件）
- [x] 3.3 实现环形缓冲（`log_buffer_lines`）与 follow 订阅注册表（每客户端 mpsc 广播）（验证：环形缓冲 tail 语义单元测试；订阅收发单元测试）

## 4. 健康检查（src/health.rs）

- [x] 4.1 实现单一检查器线程：按 `interval` 调度，支持 `tcp/http/exec` 三种探测与 `timeout`，`start_period` 内失败不计入连续失败（验证：本地临时 TCP/HTTP 监听与 echo 命令的单元测试）
- [x] 4.2 连续失败达 `retries` 置 unhealthy 并广播状态变化；`restart_on_unhealthy` 时向主循环投递等效崩溃的重启命令（验证：健康端点由通到断的集成测试，观察 unhealthy → 重启 → unhealthy 清除）

## 5. 控制平面服务端（src/server.rs、src/supervisor.rs）

- [x] 5.1 引入 `SupervisorState` 共享锁与命令队列：API 线程读快照，控制命令带一次性回执由主循环执行，Condvar 立即唤醒（验证：命令往返单元测试 + stop 类命令由主循环串行执行的断言）
- [x] 5.2 用 tiny_http 实现 `/v1/status|programs|programs/{name}` 读端点与 `start|stop|restart` 动作端点，统一 JSON 错误体与 404/409 语义；程序详情包含所属应用字段（验证：起真实守护进程的 API 集成测试，含 404 与 start@running→409）
- [x] 5.3 实现 `auth_token` 鉴权（Bearer 缺失/错误 → 401）与 `/v1/health` 免鉴权（验证：鉴权矩阵集成测试）
- [x] 5.4 实现 `/v1/programs/{name}/logs`（`stream/tail/follow`，chunked 流式响应）与 follow 并发上限（超出 → 429）（验证：tail 集成测试 + follow 收到新增行的集成测试）
- [x] 5.5 实现 `/v1/reload` 与 `/v1/shutdown` 端点；守护进程启动遇端口占用时以明确错误退出（验证：reload/shutdown 集成测试；端口占用场景用预占用端口测试或人工验证并记录）
- [x] 5.6 控制操作同步语义确认：stop/restart 阻塞至完成（受 stop_timeout 上限），返回执行后状态（验证：慢退出进程的 stop 请求耗时可测）

## 6. 排序启动与 reload 执行（src/supervisor.rs）

- [x] 6.1 实现启动编排：priority 升序稳定排序启动，`depends_on` 未 running 时等待、依赖永久不可用时进入 fatal（原因注明），守护进程关闭按逆序停止（验证：依赖排序集成测试三场景）
- [x] 6.2 执行按 app 粒度的 reload：注册表变化总是应用、app 文件 diff 触发停止-重建、单个 app 文件非法时隔离失败（保持旧定义 + 上报）、core 中守护进程字段热应用与不可热更字段提示（验证：四场景 reload 集成测试）

## 7. CLI 客户端（src/client.rs、src/main.rs）

- [x] 7.1 实现控制子命令 `status/start/stop/restart/pid/reload/shutdown`（映射 API），退出码约定 0/1/2/3，守护进程不可达 → 3（验证：无守护进程时各命令退出码集成测试 + 有守护进程时的端到端测试）
- [x] 7.2 实现 `log` 命令（映射 `/v1/programs/{name}/logs` 端点；`--tail N`、`-f/--follow` 流式渲染，Ctrl+C 退出）（验证：tail 集成测试 + follow 手动冒烟留档）
- [x] 7.3 实现注册子命令 `add/remove/list`：`add` 支持目录（`add .`，取其中 `xkeeper.toml`）与文件路径，`--name` 缺省取部署目录名，微调 flag 幂等写入 app 配置 `[app]` 表（含 `--name/--description/--autostart/--no-autostart/--autorestart/--restart-backoff/--priority`，写入失败则回滚）；`app_dir` 建链即注册、`list` 扫描 `app_dir`；离线可用、在线持久化后触发同步（验证：`add .` 离线注册 → 启动守护进程自动拉起且 `app_dir` 链接存在；在线 add 即时生效的集成测试）

## 8. 文档与收尾

- [x] 8.1 README 重写：分层配置与注册模型（core 只做全局 + `[app-default]`、`app_dir` 链接注册、四层优先级、`add/remove/list` 用法与 `[program.*]` 简化写法、v0.1 迁移）、控制平面与 CLI 用法、平台行为矩阵、systemd/nssm 示例；提供 core 与 app 配置样例文件（验证：文档通读 + `validate` 通过示例配置）
- [x] 8.2 全量回归：`cargo test` 全绿 + 端到端冒烟（全新目录空启动 → 在示例应用目录 `add . --name demo --autorestart on-failure` → 检查 `app_dir` 链接 → status/stop → 修改部署目录 app 文件 → reload → remove（验证链接清理）→ shutdown，Windows 上追加强杀守护进程验证无孤儿进程）（验证：冒烟输出留档）
- [ ] 8.3 （可选）添加 GitHub Actions 矩阵（ubuntu-latest + windows-latest）运行 `cargo test`，使 cfg(unix) 用例获得执行环境（验证：CI 两个平台均通过）
