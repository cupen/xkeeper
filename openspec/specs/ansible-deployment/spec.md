# ansible-deployment Specification

## Purpose
提供一个 Ansible 部署方案，把 xkeeper 批量安装到多台 Linux 主机并注册为 systemd 服务；用户只需编写自己的 inventory。默认部署必须对既有 xkeeper 安装零影响——发现既有安装时快速失败并说明原因，只有显式变量才能强制覆盖。

## Requirements

### Requirement: playbook 分发二进制并注册服务

`xkeeper` Ansible role（`.ansible/roles/xkeeper`，本机入口 `.ansible/site.yml`）SHALL 在每台目标机上完成：将 xkeeper 二进制分发到目标路径、安装daemon 配置到目标机、执行 `xkeeper service install --now` 注册 systemd 服务并启动。二进制来源 MUST 支持两种变量指定方式：控制机上的本地文件路径，或可下载的 URL。daemon 配置 MUST 由变量指定控制机上的本地文件路径，复制到目标机的平台默认daemon 配置路径。

#### Scenario: 部署到全新主机

- **WHEN** 目标机无任何 xkeeper 痕迹，用户以变量指定本地二进制文件与daemon 配置文件后执行 playbook
- **THEN** 二进制与daemon 配置出现在目标机默认路径，`systemctl is-enabled xkeeper` 返回 enabled 且服务处于 running 状态

#### Scenario: URL 来源分发二进制

- **WHEN** 二进制来源变量为 URL 而非本地路径
- **THEN** playbook 在控制机或目标机取得该 URL 的产物并完成分发，注册结果与本地文件来源一致

### Requirement: 检测到既有安装时默认快速失败

playbook 在对目标机做任何变更（写文件、注册服务、重启、停止）之前 SHALL 预检该机是否已存在 xkeeper 安装，检测手段 MUST 覆盖：同名 systemd unit 已存在、服务处于运行状态、默认二进制路径或默认daemon 配置路径已有文件。发现任一痕迹时 playbook MUST 立即失败（fail fast），失败信息 MUST 说明检测到的具体痕迹、未做任何修改的原因，以及如何使用强制覆盖变量绕过；且 MUST NOT 已对目标机产生任何变更。

#### Scenario: 目标机已运行 xkeeper 服务

- **WHEN** 目标机已有运行中的 xkeeper systemd 服务，用户未设置强制覆盖变量执行 playbook
- **THEN** playbook 失败退出，错误输出指明"发现既有运行中的 xkeeper 服务"，目标机上服务保持运行、文件未变

#### Scenario: 目标机仅有残留文件

- **WHEN** 目标机无 systemd unit 但默认daemon 配置路径已存在文件，用户未设置强制覆盖变量执行 playbook
- **THEN** playbook 失败退出并指明检测到的残留路径，该文件内容未被修改

#### Scenario: 预检通过后正常部署

- **WHEN** 目标机无任何 xkeeper 痕迹
- **THEN** playbook 继续执行完整部署流程，不因预检产生额外交互

### Requirement: 强制覆盖变量显式授权改写既有安装

仅当用户在 inventory、playbook 变量或 extra-vars 中显式设置强制覆盖变量为真时，playbook SHALL 允许对既有安装执行改写（覆盖二进制/配置、重启服务）。强制覆盖变量未设置（含显式设为假）时，既有安装检测一律按上一条要求快速失败。强制变量 MUST 为部署级单点开关，不得需要用户逐项指定覆盖范围。

#### Scenario: 强制覆盖执行重部署

- **WHEN** 目标机已有运行中的 xkeeper 服务，用户设置强制覆盖变量为真后执行 playbook
- **THEN** playbook 覆盖二进制与daemon 配置，服务以新内容重启，最终处于 running 状态

#### Scenario: 强制变量缺省时无可交互泄漏

- **WHEN** 用户未设置强制覆盖变量且目标机存在既有安装
- **THEN** playbook 不出现任何交互式询问，直接失败退出

### Requirement: 幂等重跑

对已由本 playbook 部署且无版本/配置变化的目标机重复执行 playbook（强制覆盖变量保持缺省）SHALL 成功且无有害副作用：检测到的既有安装属于自身部署产物时 SHALL 视为幂等命中而非冲突，服务不发生不必要的重启。

#### Scenario: 重复执行同一 playbook

- **WHEN** playbook 首次部署成功后原样重跑第二次
- **THEN** 第二次执行成功退出，服务保持运行未被重启，文件内容未变

### Requirement: 免 inventory 本机部署

仓库 SHALL 提供开箱即用的本机部署入口：`.ansible/ansible.cfg` 内置隐式 localhost，`.ansible/site.yml` 以 `connection: local` 调用 `xkeeper` role，用户无需编写 inventory 即可在控制机本机完成部署与冒烟测试；role 本身 MUST 不绑定 hosts 与连接方式，用户编写自有 inventory 后无需修改 role 即可扩展到多主机。附带的说明文档 MUST 给出最少可用变量组合。

#### Scenario: 免 inventory 本机冒烟测试

- **WHEN** 用户按说明文档准备二进制与daemon 配置后，在 `.ansible/` 下直接执行 `ansible-playbook site.yml`
- **THEN** 本机完成部署且服务注册成功，全程无需编写或修改 inventory

#### Scenario: 扩展到多主机

- **WHEN** 用户编写自有 inventory（远程主机 + 连接信息）并以 `-i` 传入
- **THEN** role 无需改动即可对该清单执行
