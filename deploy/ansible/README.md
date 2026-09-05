# xkeeper Ansible 部署

一条命令把 xkeeper 部署到多台 Linux 主机并注册为 systemd 服务：分发二进制 →
安装核心配置 → `xkeeper service install --now`（复用 xkeeper 自身的服务注册能力）。

## 环境要求

- 控制机：Ansible ≥ 2.12
- 目标机：Linux + systemd + Python 3，root 权限（`become: true`）
- xkeeper 的 Linux 二进制（`cargo build --release` 产物或 release 下载）与一份核心配置 TOML

> Windows 目标机不支持（`xkeeper service` 本身仅实现 systemd）。

## 快速开始（本地冒烟测试）

```bash
cd deploy/ansible
# 1. 编辑 inventory/localhost.yml，把 xkeeper_binary_src / xkeeper_config_src
#    指向你的二进制与核心配置
ansible-playbook -i inventory/localhost.yml site.yml
```

`inventory/localhost.yml` 使用 `ansible_connection: local` 免 SSH，部署目标就
是控制机自己。本机无免密 sudo 时加 `--ask-become-pass` 或在 inventory 中设置。

## 正式使用：写你自己的 inventory

复制样例结构，替换连接信息即可，playbook 不用改：

```yaml
# inventory/prod.yml
all:
  hosts:
    node-1:
      ansible_host: 10.0.0.11
      ansible_user: deploy
    node-2:
      ansible_host: 10.0.0.12
      ansible_user: deploy
  vars:
    xkeeper_binary_src: ./dist/xkeeper          # 控制机上的二进制路径
    xkeeper_config_src: ./conf/xkeeper.toml     # 控制机上的核心配置
```

```bash
ansible-playbook -i inventory/prod.yml site.yml
```

## 变量表

| 变量 | 必填 | 默认 | 说明 |
|---|---|---|---|
| `xkeeper_binary_src` | 是 | — | 控制机上的二进制文件路径，或 `http(s)://` 下载 URL |
| `xkeeper_config_src` | 是 | — | 控制机上的核心配置文件路径，复制到目标机 |
| `xkeeper_binary_dest` | 否 | `/usr/local/bin/xkeeper` | 目标机二进制落位路径 |
| `xkeeper_config_dest` | 否 | `/etc/xkeeper.toml` | 目标机核心配置路径 |
| `xkeeper_service_name` | 否 | `xkeeper` | systemd unit 名（传给 `service install --name`） |
| `xkeeper_service_user` | 否 | 空（不传） | 服务运行用户（传给 `service install --user`） |
| `xkeeper_binary_checksum` | 否 | 空 | URL 来源时的 sha256 值，用于校验下载产物 |
| `xkeeper_force_overwrite` | 否 | `false` | 强制覆盖开关，见下节 |

## 安全护栏：默认不触碰既有安装

playbook 在对目标机做**任何变更之前**做只读预检，检测以下痕迹：

- 同名 systemd unit 文件（`/etc/systemd/system/<service_name>.service`）
- 服务处于运行状态或已 enable
- 默认二进制路径已有文件
- 默认核心配置路径已有文件

**发现任一痕迹且未设置强制覆盖变量时，playbook 立即失败，明确列出检测到的
痕迹，目标机不做任何修改、服务不重启**——防止一键脚本误触生产环境：

```
检测到目标机已存在 xkeeper 安装，为避免误触生产环境，本次执行已停止，
目标机未做任何修改。检测到的痕迹：service is active; binary exists at ...
```

仅当你**明确**要覆盖既有安装时，设置强制变量后再执行（将覆盖二进制与配置、
按需重启服务）：

```bash
ansible-playbook -i inventory/prod.yml site.yml -e xkeeper_force_overwrite=true
```

两种例外情况不会触发护栏拦截：

- **幂等重跑**：目标机上的安装与本轮要部署的内容完全一致（二进制与配置
  checksum 相同，服务运行中且已开机自启）时，视为自身部署产物，跳过且不重启。
- **全新主机**：无任何痕迹时正常部署，无交互。

## 幂等性

对已部署且内容未变的机器原样重跑 playbook 是安全无害的：不产生文件变更、
不重启服务。内容有变化（升级版本/换配置）时按上节规则：未设强制变量则拦截，
设置后覆盖并重启。
