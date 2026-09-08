## 1. Playbook 骨架与预检护栏

- [x] 1.1 创建 `.ansible/site.yml`（入口 play：`hosts: localhost` + `connection: local` + `become: true`、`any_errors_fatal: true`，调用 `xkeeper` role）与 role 断言任务（`xkeeper_binary_src`、`xkeeper_config_src` 缺失时 `ansible.builtin.assert` 失败）；验证：`ansible-playbook --syntax-check` 通过
- [x] 1.2 实现既有安装预检（`roles/xkeeper/tasks/guard.yml`）：只读探测四项痕迹（unit 文件 `stat`、`systemctl is-active`、`xkeeper_binary_dest` 与 `xkeeper_config_dest` 的 `stat`），`failed_when: false` 包裹；验证：对已有 xkeeper 的目标机 dry-run 能命中全部痕迹项
- [x] 1.3 实现幂等命中判定：比对现有二进制/配置 checksum 与待分发源 checksum，一致且服务 running+enabled 时跳过冲突判定且不触发重启；验证：部署成功后原样重跑，playbook 输出显示无文件变更、服务未重启
- [x] 1.4 实现冲突 fail fast：未设 `xkeeper_force_overwrite` 时，命中任一非幂等痕迹即 `fail`，错误信息列出具体痕迹与绕过提示（设 `xkeeper_force_overwrite: true`）；验证：对已有安装执行退出码非零、信息可读、目标机文件与服务状态不变
- [x] 1.5 实现强制覆盖路径：`xkeeper_force_overwrite: true` 时预检降级为 `debug` 记录，放行后续全部步骤；验证：强制模式下对既有安装重部署成功且服务以新内容重启

## 2. 部署与注册任务

- [x] 2.1 实现二进制分发：`xkeeper_binary_src` 为本地路径时走 `ansible.builtin.copy`、为 URL 时走 `ansible.builtin.get_url`（可选 checksum 校验变量），落位 `xkeeper_binary_dest` 并置可执行位；验证：两种来源分别在临时主机/localhost 上产物一致（`sha256sum` 比对）
- [x] 2.2 实现核心配置安装：`copy` `xkeeper_config_src` → `xkeeper_config_dest`（mode 0644）；验证：目标机文件内容与源一致
- [x] 2.3 实现服务注册：`command: <dest> service install --now --name <xkeeper_service_name>`（可选 `--user`），`changed_when`/`register` 按输出判定；验证：`systemctl is-enabled` 与 `is-active` 均 via systemd 返回肯定结果
- [x] 2.4 文件变化触发服务重启（handler 或条件任务），仅在强制覆盖路径或首次部署时发生；验证：覆盖二进制后重跑，服务重启且 running

## 3. 免 inventory 本机入口与文档

- [x] 3.1 创建 `.ansible/ansible.cfg`（`inventory = localhost,` 隐式单机清单），`site.yml` 显式 `connection: local`——免编写 inventory 即可本机部署；验证：`cd .ansible && ansible-playbook site.yml -e ...` 无 inventory 警告
- [x] 3.2 创建 `.ansible/README.md`：最少可用变量组合、二进制两种来源说明、role 复用与自有 playbook/inventory 的扩展步骤、强制覆盖变量语义与风险提示、目标机要求（Linux + systemd + Python）；验证：按文档从零操作可在本机完成一次部署
- [x] 3.3 主仓 README 部署章节追加 `.ansible/` 指引（两三行 + 链接）；验证：`cargo build`/仓库不涉及，人工核对文档链接可达

## 4. 集成验证

- [x] 4.1 本地冒烟测试：免 inventory 走通全新部署（二进制 + 测试配置），验证服务 running、`xkeeper status` 正常；随后原样重跑验证幂等；再覆盖配置重跑验证强制覆盖提示出现且未设变量时安全失败
- [x] 4.2 清理与善后验证：本机执行 `xkeeper service uninstall` 恢复原状，确认 `systemctl is-enabled xkeeper` 失败、unit 文件已删；验证：冒烟测试不留残余

## 5. Role 化重构（应用户要求：role 下沉 + 免 inventory）

- [x] 5.1 部署逻辑从单体 `site.yml` 拆入 `roles/xkeeper/`（defaults/tasks/handlers，预检独立为 `guard.yml`、安装为 `install.yml`），role 不绑定 hosts/连接方式；删除旧 `deploy/ansible/`
- [x] 5.2 `ansible.cfg` 隐式 localhost + `site.yml` `connection: local` 取代 `inventory/localhost.yml` 样例；`meta: end_host` 幂等跳过保留在入口任务流
- [x] 5.3 同步主 README、openspec 工件（proposal/design/spec delta/tasks）中的路径与库存样例描述；验证：`openspec validate --change` 通过
