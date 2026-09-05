# Tasks: add-ci

## 1. CI 工作流

- [x] 1.1 创建 `.github/workflows/ci.yml`：ubuntu-latest + windows-latest 矩阵、stable 工具链、rust-cache、`cargo test`；YAML 可解析且结构核对（验证：Python yaml.safe_load 解析通过 + job/step 数量核对）
- [ ] 1.2 CI 在两个平台全部通过（验证：推送 GitHub 后 Actions 两次运行均绿；需先配置远程，当前环境无远程故保持未勾）
