## Purpose

定义 xkeeper-bench 负载基准测量能力：以独立二进制驱动真实 xkeeper daemon 按选定 case 产生真实日志负载，采集吞吐、内存、轮转与完整性指标并输出报告，用于容量评估与版本间性能回归对比。

## ADDED Requirements

### Requirement: 命令形态与用例选择

`xkeeper-bench` SHALL 作为独立可执行二进制随 workspace 构建分发，与 `xkeeper` daemon 主二进制互不包含。负载场景 SHALL 通过必选参数 `--case <name>` 选择；v1 SHALL 支持 `firehose`、`rotation`、`fanout`、`drip` 四个用例。传入未支持的 case 名时 MUST 打印支持的 case 清单并以非零码退出。

#### Scenario: 选择 case

- **WHEN** 执行 `xkeeper-bench --case firehose`（其余参数取缺省）
- **THEN** bench 按 firehose 场景完成一轮基准测量并正常退出

#### Scenario: 未知 case 拒绝

- **WHEN** 执行 `xkeeper-bench --case turbo`
- **THEN** bench 不启动任何负载，打印 `firehose/rotation/fanout/drip` 支持清单，以非零码退出

### Requirement: 自带隔离环境拓扑

未指定 `--connect` 时，bench SHALL 自建隔离测量环境：在临时 workspace 目录中生成 daemon 配置与 bench app 配置，拉起真实 `xkeeper` daemon 进程，注册并 apply 启动负载程序；测量结束后停止 daemon 并删除临时 workspace。`--keep` 指定时 SHALL 保留 workspace 供检查。daemon 二进制位置 SHALL 可用 `--daemon <path>` 指定，缺省在工作区 target 目录发现。

#### Scenario: 缺省自跑完整周期

- **WHEN** 在仓库目录执行 `xkeeper-bench --case firehose`
- **THEN** bench 拉起自己的 daemon 与负载程序完成测量，退出后该临时 workspace 不存在，系统内无遗留 bench 进程

#### Scenario: --keep 保留现场

- **WHEN** 执行 `xkeeper-bench --case firehose --keep` 且测量完成
- **THEN** bench 退出时打印保留的 workspace 路径，daemon 已停止，workspace 内容仍在磁盘上

### Requirement: 连接既有 daemon 拓扑

指定 `--connect <addr>` 时，bench SHALL 连接该地址的运行中 daemon 并通过既有 `/v1` 控制面接口注册名为 `xkeeper-bench-<case>[-<序号>]` 形态的临时 app、apply 启动负载、采集指标、stop 并卸载。守护进程开启 Bearer 鉴权时 SHALL 支持 `--token <token>` 携带凭据。连接失败或鉴权失败 MUST 以非零码退出且不在目标 daemon 上留下任何注册痕迹。

#### Scenario: 对运行中 daemon 测量

- **WHEN** daemon 运行于 `127.0.0.1:7310`，执行 `xkeeper-bench --case firehose --connect 127.0.0.1:7310`
- **THEN** 该 daemon 上出现 `xkeeper-bench-` 前缀的临时 app 并产生日志负载，测量结束后该 app 被卸载

#### Scenario: 目标不可达

- **WHEN** 执行 `xkeeper-bench --case firehose --connect 127.0.0.1:9` 且该地址无 daemon
- **THEN** bench 以非零码退出，报错说明连接失败

### Requirement: 压测资源清理

无论测量成功、失败或被中断，bench 退出前 SHALL 清理其创建的资源：connect 模式卸载临时 app 并删除其在 daemon 日志目录下的日志文件；spawn 模式删除临时 workspace（`--keep` 除外）。目标 daemon 上已存在 `xkeeper-bench-` 前缀同名 app 时 MUST 拒绝开跑（非零码退出），MUST NOT 覆写或删除非 bench 创建的任何 app 与文件。

#### Scenario: 失败也清理

- **WHEN** 测量中途 bench 内部出错退出
- **THEN** 目标 daemon 上不留 `xkeeper-bench-` 前缀 app，其日志文件已删除

#### Scenario: 同名拒跑

- **WHEN** 目标 daemon 已存在名为 `xkeeper-bench-firehose` 的 app，执行 `xkeeper-bench --case firehose --connect <addr>`
- **THEN** bench 以非零码退出且该既有 app 未被修改

### Requirement: 负载参数语义

负载量界 SHALL 由以下参数共同界定，任一先到即终止产出：

- `--log-rows <n>`：每个负载程序产出的总行数，`0` 或缺省表示不限；
- `--log-row-size <bytes>`：单行载荷字节数（不含换行），缺省 128；行内容 SHALL 为逼真文本（时间戳、程序与流内单调行序号、填充文本），保证每行可独立对账；
- `--log-total-size <bytes>`：全部负载程序合计产出字节上限，缺省不限；
- `--rate <rows/s>`：每程序产出速率，`0` 或缺省表示全速（drip 用例例外：缺省采用其内置速率，见 drip 用例要求）；
- `--duration <secs>`：墙钟上限，缺省 30；
- `--programs <n>`：fanout 程序数，缺省 4（仅 fanout 用例消费）。

bench MUST 拒绝非法组合（负数、非数字）并以非零码退出。

#### Scenario: 行数先到终止

- **WHEN** `--case firehose --log-rows 10000 --duration 300`，全速输出
- **THEN** 负载程序在产出 10000 行后结束，总耗时远小于 300 秒，bench 报告 rows 等于 10000

#### Scenario: 时长先到终止

- **WHEN** `--case firehose --rate 100`（rows 与 total-size 不限）
- **THEN** 约 30 秒后负载结束，报告 rows 约为 3000（允许速率抖动误差）

#### Scenario: 总量上限先到终止

- **WHEN** `--case firehose --log-row-size 100 --log-total-size 1000` 且 `--log-rows 0`
- **THEN** 负载程序产出约 10 行后结束，总字节数不超过上限加单行冗余

### Requirement: 日志生成器行为

负载程序 SHALL 是 bench 二进制自我重入的隐藏子进程模式（对用户不作为公开 case 暴露），以 daemon 普通子进程身份向 stdout 与 stderr 产出真实日志。生成器 SHALL 按指定速率控速、按 `--log-row-size` 渲染行、每行嵌入其所在流内单调递增的行序号，并支持按固定比例把产出分流到 stderr（fanout 用例双流：每 10 行取 1 行写入 stderr，其余写 stdout；比例 v1 内置固定、不设参数）。生成器被停止（stop 信号/超时）时 SHALL 及时退出，不悬挂。

#### Scenario: 双流产出

- **WHEN** fanout 用例运行
- **THEN** 每个负载程序的 out 流与 err 流日志文件都在增长，err 流行数约为总行数的 1/10，两流可分别按序号对账

#### Scenario: 优雅终止

- **WHEN** bench 在测量结束后对负载程序执行 stop
- **THEN** 负载进程在 stop 超时内自行退出，daemon 不需要强杀

### Requirement: firehose 用例

`--case firehose` SHALL 以单个负载程序全速（或 `--rate` 指定速率）产出日志，测量泵/落盘/轮转链路的吞吐上限：报告该程序的 rows/s 与 bytes/s。缺省参数下 SHALL 优先以 `--duration` 界定负载量。

#### Scenario: 全速吞吐测量

- **WHEN** `xkeeper-bench --case firehose --duration 10`
- **THEN** 报告给出非零的 rows/s 与 bytes/s 吞吐数字，测量约 10 秒后结束

### Requirement: rotation 用例

`--case rotation` SHALL 以显著小于产出总量的轮转阈值运行（bench 自动设定 `log_max_size` 与足够的 `log_rotate_keep`，使测量期内发生多次轮转且不触发最旧文件删除），测量轮转开销并校验轮转过程中日志逐行完整：盘上当前文件与全部轮转文件合并后，行序号 SHALL 连续、无缺口、无重复、无乱序。报告 SHALL 包含轮转次数。

#### Scenario: 高频轮转完整性

- **WHEN** `xkeeper-bench --case rotation --log-rows 200000 --log-row-size 128`
- **THEN** 测量期内日志发生多次轮转，报告 rotation count 大于 0，完整性校验通过

### Requirement: fanout 用例

`--case fanout` SHALL 并行运行 `--programs <n>` 个负载程序（各自独立 app 程序），每程序向 stdout 与 stderr 双流产出，测量 supervisor 多程序下的聚合吞吐与每程序吞吐。程序名 SHALL 为 `xkeeper-bench-fanout-<序号>` 形态。

#### Scenario: 多程序聚合

- **WHEN** `xkeeper-bench --case fanout --programs 8 --duration 10`
- **THEN** 8 个负载程序并行产出，报告给出聚合吞吐与每程序吞吐分解

### Requirement: drip 用例

`--case drip` SHALL 以低速率长时间稳态产出，验证 daemon 内存有界性：报告 daemon 进程 RSS 随时间的峰值与均值。未显式指定 `--rate` 时 SHALL 采用内置缺省速率 100 行/秒；显式指定 `--rate`（含 `0` = 全速）时覆盖缺省。缺省 `--duration` 下即可运行。

#### Scenario: 稳态 RSS 有界

- **WHEN** `xkeeper-bench --case drip --rate 100 --duration 60`
- **THEN** 报告给出 60 秒内 daemon RSS 峰值与均值，峰值不随产出总量线性增长

#### Scenario: 缺省低速稳态

- **WHEN** `xkeeper-bench --case drip`（未指定 `--rate`）
- **THEN** 负载以约 100 行/秒产出而非全速，报告给出 daemon RSS 峰值与均值

### Requirement: 指标采集与报告

bench SHALL 完全从外部采集指标——读取 daemon 既有 `/v1` 状态投影、检查磁盘日志文件与轮转文件、对 daemon 进程做外部 RSS 采样——MUST NOT 要求 daemon 新增接口或修改状态投影。报告 SHALL 以人类可读表格输出到 stdout，包含：case 名、生效参数、每程序与聚合 rows/s 与 bytes/s、产出总行数与总字节、wall time、轮转次数、daemon RSS 峰值/均值、完整性校验结论。指定 `--json <path>` 时 SHALL 另行写出等价的机器可读 JSON 文件（核心字段与表格一致，允许包含额外明细）。诊断与进度信息 SHALL 输出到 stderr，不污染 stdout 报告。

#### Scenario: 表格与 JSON 双输出

- **WHEN** `xkeeper-bench --case firehose --json report.json` 正常完成
- **THEN** stdout 呈现人类可读指标表格，`report.json` 存在且包含 case、rows、bytes、rows_per_sec、bytes_per_sec、wall_time、rotation_count、integrity 等字段

#### Scenario: 指标不依赖 daemon 改动

- **WHEN** 对任意未修改的标准 daemon 运行任一 case
- **THEN** bench 正常产出全部指标（RSS 采样不可用的平台上该项报告为 null，其余指标不受影响）

### Requirement: 日志完整性校验

bench SHALL 对每个 case 校验日志完整性：负载行内嵌的每流（程序 × 流）行序号在盘上日志（当前文件 + 各轮转文件）中 SHALL 无缺口、无重复、无乱序；实际读到的行数 SHALL 与按轮转保留策略推算的期望一致。校验结论 SHALL 进入报告与 JSON。校验失败时 bench MUST 以非零码退出。

#### Scenario: 完整性失败可见

- **WHEN** 测量期间日志出现丢行（如外部干预删除某轮转文件）
- **THEN** 报告 integrity 为失败，bench 以非零码退出

### Requirement: 退出码语义

bench 退出码 SHALL 仅反映运行本身的成败：测量正常完成（含完整性校验）返回 0；参数错误、环境搭建失败、连接失败、测量中断、完整性校验失败返回非零。MUST NOT 因吞吐数值高低而改变退出码（阈值对比由使用者比较两次 JSON 完成）。

#### Scenario: 吞吐高低不改退出码

- **WHEN** 两次 `--case firehose` 分别在快机与慢机上完成
- **THEN** 两次退出码均为 0，仅 JSON 中吞吐数值不同
