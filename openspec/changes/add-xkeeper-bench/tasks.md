## 1. Crate 骨架与 CLI

- [x] 1.1 新建 `xkeeper-bench/` workspace 成员 crate（Cargo.toml：clap/anyhow/serde/serde_json/ureq），加入根 Cargo.toml members；`cargo build -p xkeeper-bench` 通过且 daemon 包依赖零变化（`cargo tree -p xkeeper` 对照）
- [x] 1.2 定义 clap CLI：`--case <firehose|rotation|fanout|drip>`（必选）、`--log-rows/--log-row-size/--log-total-size/--rate/--duration/--programs`、`--connect/--token/--daemon/--json/--keep`；未知 case 打印支持清单非零退出（单测覆盖参数解析与拒绝路径）

## 2. 日志生成器（自我重入）

- [x] 2.1 实现隐藏子命令 `__generate`：令牌桶控速逐行写 stdout/stderr，行格式 `[k] <seq> <ts> <fill>` 按 `--log-row-size` 渲染，三量界（rows/total-size/duration）先到即优雅退出；单测验证行数、行宽、序号单调
- [x] 2.2 速率控制精度验证：`--rate 100` 下实测速率误差在合理带宽（±20%）；全速模式吞吐不因控速逻辑劣化（简单基准脚本或 ignored test）

## 3. 运行拓扑

- [x] 3.1 spawn 拓扑：临时 workspace 生成（daemon.toml + app_dir + logs）、`--daemon` 发现逻辑（显式路径 → target/{debug,release}）、随机空闲端口拉起 daemon、经 `/v1` 注册/apply/stop/shutdown、退出删 workspace（`--keep` 保留并打印路径）；集成测试跑一轮完整周期并断言无遗留进程
- [x] 3.2 connect 拓扑：连 `--connect` 地址（`--token` Bearer）、注册 `xkeeper-bench-<case>` 前缀 app、同名拒跑、连接/鉴权失败非零退出且无注册痕迹；单测 + 对真实 daemon 手册验证
- [x] 3.3 清理路径：成败与 ctrl-C 中断均执行清理（connect 删 app + 日志文件；spawn 删 workspace）；测试模拟中途失败断言无残留

## 4. Case 实现

- [x] 4.1 firehose：单程序全速/定速产出，报告 rows/s 与 bytes/s；集成测试 `--duration 3` 小规模跑通
- [x] 4.2 rotation：按 D9 公式自动设定 log_max_size/log_rotate_keep，验证多次轮转且无文件删除；集成测试断言 rotation count > 0
- [x] 4.3 fanout：`--programs` 个程序并行双流产出，程序名 `xkeeper-bench-fanout-<序号>`，聚合与每程序吞吐分解；双流按 10:1 固定分流；集成测试 `--programs 3` 跑通并断言 err 流行数约为总行数 1/10
- [x] 4.4 drip：定速稳态产出 + daemon RSS 外部采样（unix `/proc/<pid>/status` 1s 轮询，其他平台 null）；集成测试短时长跑通并产出 peak/avg；断言未指定 `--rate` 时 drip 采用内置 100 行/秒缺省速率而非全速

## 5. 指标采集与报告

- [x] 5.1 指标聚合：生成器计数回传（子进程退出码/约定通道）+ 读盘对账行数字节 + 轮转文件计数 + wall time；实现 D8 JSON schema 并落 `--json` 路径；单测覆盖 schema 字段
- [x] 5.2 人类可读表格输出到 stdout、进度/诊断到 stderr；快照测试校验输出不含进度噪声
- [x] 5.3 完整性对账：按轮转序拼接日志文件校验 seq 严格递增无缺口；丢行注入测试 → integrity fail → 非零退出码
- [x] 5.4 退出码语义单测：参数错误/连接失败/完整性失败非零，正常完成与吞吐高低无关恒为 0

## 6. e2e 与文档

- [x] 6.1 xtask e2e 增加 bench 冒烟段：短 firehose（如 `--log-rows 20000 --duration 30`）验证接线、JSON 文件生成与退出码；`cargo run -p xtask -- e2e` 全绿
- [x] 6.2 README 与 AGENTS.md 增补 bench 段落（用途、四 case、参数表、与 stress.rs 的关系、connect 注意事项）；跨平台检查：unix 全路径验证，Windows 路径编译通过（`cargo check --target x86_64-pc-windows-msvc` 或 CI 等价）且 RSS 项按规格降级 null
