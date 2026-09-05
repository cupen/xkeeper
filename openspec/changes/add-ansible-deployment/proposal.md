## Why

xkeeper 已具备 `xkeeper service install` 一条命令注册 systemd 服务的能力，但多机部署仍需逐台手工执行"拷贝二进制 → 写核心配置 → service install"。提供一个开箱即用的 Ansible playbook，用户只需写出自己的 inventory 即可批量部署；仓库内附一个本地（localhost）inventory 既作冒烟测试，也作用户 inventory 的参考样例。

## What Changes

- 新增 `deploy/ansible/` 目录，包含：
  - `site.yml` playbook：分发 xkeeper 二进制（本地文件路径或 URL 两种来源，变量指定）、安装核心配置到目标机、调用 `xkeeper service install --now` 注册并启动服务。
  - `inventory/localhost.yml`：本地单机 inventory 样例（`ansible_connection=local`），用于本地冒烟测试与作为用户编写 inventory 的参考。
  - `README.md`：使用说明——如何写自己的 inventory、变量表、二进制来源两种方式。
- **安全护栏（默认拒绝触碰既有安装）**：playbook 在执行任何变更前 MUST 预检目标机是否已存在 xkeeper（systemd unit 已存在 / 服务在运行 / 二进制或核心配置已存在于默认路径）。发现既有安装时立即失败并说明原因与绕过方式，不做任何修改、不重启服务。
- **强制覆盖变量**：仅当用户在 inventory 或 extra-vars 中显式设置强制变量（`xkeeper_force_overwrite: true`）时，才允许覆盖既有安装（含重启）。
- 主仓 README 的部署章节追加指向 `deploy/ansible/` 的说明。

## Capabilities

### New Capabilities

- `ansible-deployment`: Ansible 部署工具的行为要求——目标分发与配置、既有安装预检与快速失败、强制覆盖变量语义、幂等性、本地 inventory 样例的存在与用法。

### Modified Capabilities

## Impact

- 新增 `deploy/ansible/`（playbook、inventory 样例、说明文档）；不改动 Rust 源码与前端。
- 依赖宿主机 Ansible（≥ 2.12）与目标机 Linux + systemd；服务注册复用既有 `xkeeper service install`，不新增 Rust 依赖。
- README 部署章节小幅更新。
- 假设（记录备查）：仅支持 Linux 目标机（Windows 服务注册当前不受支持，与 `service-registration` 能力边界一致）；二进制默认从变量指定的本地文件分发，URL 下载为可选来源；核心配置文件同样由变量指定本地路径后复制到 `/etc/xkeeper.toml`。
