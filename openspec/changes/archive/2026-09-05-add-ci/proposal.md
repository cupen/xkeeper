# Proposal: add-ci — 跨平台 CI 矩阵

## Why

app-supervisor 变更的 Unix 运行时路径（`Command::process_group`、`killpg`、
Unix symlink、`sh -c` 分支的测试）无法在 Windows 开发机上验证，任务 2.1 与
8.3 因此遗留。需要 GitHub Actions 双平台矩阵在每次推送时运行 `cargo test`，
让 cfg(unix) 用例获得执行环境并持续守护两个平台的行为一致性。

## What Changes

- 新增 `.github/workflows/ci.yml`：`ubuntu-latest` + `windows-latest` 矩阵，
  stable 工具链（dtolnay/rust-toolchain），`Swatinem/rust-cache` 依赖缓存，
  运行 `cargo test`（含构建检查）。
- 无产品代码改动、无新依赖。

## Capabilities

### New Capabilities

（无——纯 CI 工具类变更，不改变任何可观察行为；`.openspec.yaml` 已声明
`skip_specs: true`。）

### Modified Capabilities

（无。）

## Impact

- 仅新增 `.github/workflows/ci.yml`。
- 仓库推送到 GitHub 远程后 CI 即生效；`add-ci` 任务 1.2（双平台绿）依赖
  该远程存在，配置远程并推送后即闭环。
