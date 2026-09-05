## Context

仓库已具备 `xkeeper service install`（systemd 注册，见 `service-registration` 能力）与平台默认路径约定（二进制由用户放置、核心配置默认 `/etc/xkeeper.toml`）。当前缺的是"多台主机一键部署"的自动化层。约束：目标机限定 Linux + systemd；Ansible 运行在控制机，目标机只需要 Python + systemd；不改动 Rust 源码。

## Goals / Non-Goals

**Goals:**

- 单 playbook 覆盖"分发二进制 → 装核心配置 → `service install --now`"全流程。
- 预检护栏：默认对既有安装零影响，fail fast 且信息可读（说清检测到什么、为什么停、怎么绕过）。
- 本地 inventory 样例可跑通冒烟测试，同时是用户 inventory 的模板。

**Non-Goals:**

- 不支持 Windows 目标机（与 `service-registration` 边界一致）。
- 不做应用级（`xkeeper add` 的各 app 部署文件）分发——核心配置先行，app 分发留作后续扩展。
- 不内建 CI/CD、滚动升级编排、多版本共存；本 playbook 只管单版本原地部署。
- 不从源码构建二进制（用户用 release 产物或自行 `cargo build --release`，playbook 只做分发）。

## Decisions

### 1. 目录与文件布局

```
deploy/ansible/
  site.yml                  # 唯一 playbook
  inventory/localhost.yml   # 本地样例（连接=local）
  README.md                 # 变量表 + 用法 + 自定义 inventory 指引
```

备选：`ansible/` 放仓库根。选 `deploy/ansible/` 是给未来其他部署形态（compose、k8s manifest）留位。

### 2. 变量约定（全部带安全缺省）

| 变量 | 缺省 | 说明 |
|---|---|---|
| `xkeeper_binary_src` | 必填 | 本地文件路径或 URL（ansible `get_url`/`copy` 自动分派） |
| `xkeeper_config_src` | 必填 | 控制机核心配置文件路径，复制到 `/etc/xkeeper.toml` |
| `xkeeper_binary_dest` | `/usr/local/bin/xkeeper` | 目标机二进制路径 |
| `xkeeper_config_dest` | `/etc/xkeeper.toml` | 目标机核心配置路径 |
| `xkeeper_service_name` | `xkeeper` | unit 名，传给 `service install --name` |
| `xkeeper_service_user` | （不设则不传） | 运行用户，传给 `service install --user` |
| `xkeeper_force_overwrite` | `false` | 强制覆盖开关（见决策 4） |

备选：二进制来源拆成两个变量（`src` + `is_url` 布尔）。拒绝之——`copy`（本地路径存在）与 `get_url`（字符串可解析为 URL）按值形态自动分派，少一个易错的开关变量。

### 3. 预检用 `pre_tasks` + `block`，不做变更前置

预检放在 `pre_tasks`，全部为只读命令（`stat`、`systemctl is-active`、`test -f`），命中任意痕迹即 `fail`。关键点：

- 检测项四条：同名 unit 文件存在（`/etc/systemd/system/<name>.service`）、`systemctl is-active` 非 inactive、`xkeeper_binary_dest` 已存在、`xkeeper_config_dest` 已存在。unit 名按 `xkeeper_service_name` 动态拼，用户自定义 unit 名也能被查到。
- 失败信息拼出具体痕迹列表 + 提示 `xkeeper_force_overwrite: true` 可绕过。`any_errors_fatal: true` 防止多机场景下部分机继续跑。
- **幂等命中识别**：预检命中时先比对现有二进制/配置 checksum 与本次要分发的 checksum；完全一致且服务 running/enabled 判定为"自身部署产物，无变化"，视为幂等通过（不重启）；有不一致但未设强制变量 → 按冲突 fail。这满足 spec 的幂等重跑要求，且不需要额外状态文件。
- 预检阶段只读，因此天然满足"失败前目标机零变更"。

备选：生成标记文件（如 `/etc/xkeeper.deployed-by-ansible`）记录 playbook 部署指纹。更精确但引入额外状态与清理语义，checksum 比对已覆盖核心场景，标记文件留作后续增强。

### 4. 强制覆盖走部署级单点开关

`xkeeper_force_overwrite: true` 时预检降级为"记录检测结果"（debug 输出），放行后续步骤：`copy`/`get_url` 覆盖二进制与配置，服务重启（`service install` 幂等；文件变化后用 handler `systemctl restart`）。不设则一律 fail。单点开关而非逐项（如"只覆盖二进制不覆盖配置"）——逐项组合指数爆炸且容易半新半旧，违背"部署 = 一致快照"的直觉。

### 5. 服务注册复用 `xkeeper service install --now`

不在 playbook 里手写 unit 模板。理由：unit 生成规则（ExecStart、TimeoutStopSec 估算、SIGTERM）已是 `service-registration` 能力的 spec，再写一份 Ansible 模板必然漂移。playbook 只负责把二进制与配置放到位，然后 `command: xkeeper service install --now --name <n>`（含 `become: true`）。幂等性由 `service install` 自身保证（内容一致幂等跳过）。

备选：Ansible 原生 `ansible.builtin.systemd` + 自己维护 unit 模板。能力重复且双源漂移，拒绝。

### 6. 本地 inventory 样例即参考模板

`inventory/localhost.yml` 用 INI 或 YAML 均可，选 YAML（用户扩展到多机组更自然）：

```yaml
all:
  hosts:
    xkeeper-local:
      ansible_host: 127.0.0.1
      ansible_connection: local
      xkeeper_binary_src: ./dist/xkeeper
      xkeeper_config_src: ./conf/xkeeper.toml
```

`ansible_connection: local` 免 SSH，本地冒烟零配置。README 给出"复制此文件改连接信息"的最短路径。

## Risks / Trade-offs

- [checksum 比对判定幂等，命中冲突但实际是用户手工装的相同版本] → 行为仍安全：未设强制变量即 fail 并说明痕迹，用户确认后强制或清理，宁可保守。
- [`service install` 需 root，本地样例用户可能非 root] → 预检前置的 `become` 检查与 README 明示（`sudo` 或 inventory 设 `ansible_become`）；预检本身只读，无 root 也能跑完并给出清晰失败。
- [`systemctl is-active` 在无 systemd 环境（容器）报错] → 预检用 `failed_when: false` 包裹并按输出判读；README 注明目标机需 systemd。
- [URL 来源在外网不可达/产物校验缺失] → `get_url` 必填 `checksum` 变量（可选，给了就校验），README 建议固定 checksum；不做自动重试编排。

## Migration Plan

纯新增目录，无存量迁移。回滚 = 删除 `deploy/ansible/`。目标机卸载走既有 `xkeeper service uninstall`，playbook 不提供 destroy 编排（避免"一键删生产"类风险，与本次护栏主题一致）。

## Open Questions

无。
