# xkeeper Ansible 部署（role 化）

以 Ansible role（`roles/xkeeper/`）封装部署流程：分发二进制 → 安装 daemon 配置 →
`xkeeper service install --now`（复用 xkeeper 自身的服务注册能力）。
默认**免 inventory 本机部署**：`ansible.cfg` 已内置隐式 localhost，
`cd .ansible && ansible-playbook site.yml` 即可。

## 环境要求

- 控制机：Ansible ≥ 2.12
- 目标机：Linux + systemd + Python 3，root 权限（play 已设 `become: true`）
- xkeeper 的 Linux 二进制（`cargo build --release` 产物或 release 下载）与一份 daemon 配置 TOML

> Windows 目标机不支持（`xkeeper service` 本身仅实现 systemd）。

## 快速开始（本机部署）

```bash
cargo build --release                      # 先产出二进制
cd .ansible
ansible-playbook site.yml \
  -e xkeeper_binary_src=../target/release/xkeeper \
  -e xkeeper_config_src=../examples/daemon.toml
```

`become` 需要 sudo；无免密 sudo 时加 `--ask-become-pass`。
验证结果：`systemctl is-active xkeeper` 为 active，`xkeeper status` 可查询。

预演（不落盘）加 `--check`：护栏预检（`stat`/`systemctl is-active`）仍会真实
执行，可提前看到既有安装拦截效果。

## role 引用（写你自己的 playbook / inventory）

role 不绑定 hosts 与连接方式，可直接被其他 playbook 复用：

```yaml
- name: Deploy xkeeper
  hosts: all
  become: true
  roles:
    - xkeeper
```

```bash
# inventory/prod.yml
all:
  hosts:
    node-1:
      ansible_host: 10.0.0.11
      ansible_user: deploy
  vars:
    xkeeper_binary_src: ./dist/xkeeper          # 控制机上的二进制路径
    xkeeper_config_src: ./conf/daemon.toml     # 控制机上的 daemon 配置
```

```bash
ansible-playbook -i inventory/prod.yml -e @extra.yml site.yml
# 或把 role 拷到你的项目 roles/ 下引用
```

## 变量表

| 变量 | 必填 | 默认 | 说明 |
|---|---|---|---|
| `xkeeper_binary_src` | 是 | — | 控制机上的二进制文件路径，或 `http(s)://` 下载 URL |
| `xkeeper_config_src` | 是 | — | 控制机上的 daemon 配置文件路径，复制到目标机 |
| `xkeeper_binary_dest` | 否 | `/usr/local/bin/xkeeper` | 目标机二进制落位路径 |
| `xkeeper_config_dest` | 否 |  `/etc/xkeeper/daemon.toml` | 目标机 daemon 配置路径 |
| `xkeeper_service_name` | 否 | `xkeeper` | systemd unit 名（传给 `service install --name`） |
| `xkeeper_service_user` | 否 | 空（不传） | 服务运行用户（传给 `service install --user`） |
| `xkeeper_binary_checksum` | 否 | 空 | URL 来源时的 sha256 值，用于校验下载产物 |
| `xkeeper_force_overwrite` | 否 | `false` | 强制覆盖开关，见下节 |

## 安全护栏：默认不触碰既有安装

role 在对目标机做**任何变更之前**做只读预检，检测以下痕迹：

- 同名 systemd unit 文件（`/etc/systemd/system/<service_name>.service`）
- 服务处于运行状态或已 enable
- 默认二进制路径已有文件
- 默认 daemon 配置路径已有文件

**发现任一痕迹且未设置强制覆盖变量时，立即失败，明确列出检测到的痕迹，
目标机不做任何修改、服务不重启**——防止一键脚本误触生产环境：

```
检测到目标机已存在 xkeeper 安装，为避免误触生产环境，本次执行已停止，
目标机未做任何修改。检测到的痕迹：service is active; binary exists at ...
```

仅当你**明确**要覆盖既有安装时，设置强制变量后再执行（将覆盖二进制与配置、
按需重启服务）：

```bash
ansible-playbook site.yml -e xkeeper_force_overwrite=true -e ...
```

两种例外情况不会触发护栏拦截：

- **幂等重跑**：目标机上的安装与本轮要部署的内容完全一致（二进制与配置
  checksum 相同，服务运行中且已开机自启）时，视为自身部署产物，跳过且不重启。
- **全新主机**：无任何痕迹时正常部署，无交互。

## 幂等性

对已部署且内容未变的机器原样重跑是安全无害的：不产生文件变更、不重启服务。
内容有变化（升级版本/换配置）时按上节规则：未设强制变量则拦截，设置后覆盖并重启。

## 目录结构

```
.ansible/
  ansible.cfg                  # 隐式 localhost inventory，免 inventory 即用
  site.yml                     # 本机部署入口 playbook（hosts: localhost + connection: local）
  roles/xkeeper/
    defaults/main.yml          # 变量缺省
    tasks/main.yml             # 入口：断言 → 源准备 → 护栏 → 安装 → 清理
    tasks/guard.yml            # 既有安装只读预检 + 幂等命中判定
    tasks/install.yml          # 分发二进制/配置 + service install + 验证
    handlers/main.yml          # 覆盖部署时按需重启
```
