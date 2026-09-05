# Design: add-ci

## Context

app-supervisor 归档时遗留 2.1（Unix 运行时验证）与 8.3（可选 CI）。仓库无
远程、开发机为 Windows，cfg(unix) 代码路径（process_group/killpg/symlink）
没有执行环境。`skip_specs: true`：纯 CI 工具变更，无可观察行为变化。

## Goals / Non-Goals

**Goals:** 推送即跑的双平台 `cargo test` 矩阵；依赖缓存控制 CI 时长。
**Non-Goals:** MSRV 矩阵、release 发布流水线、clippy/fmt 门禁（后续按需加）。

## Decisions

- **stable 单工具链 + dtolnay/rust-toolchain@stable**：当前 Cargo.toml 未声明
  rust-version 下限，先只守护 stable；备选的 MSRV 矩阵在确定下限后加。
- **Swatinem/rust-cache@v2**：标准缓存方案，免去自管 Cargo 缓存。
- **只跑 `cargo test`**：test 隐含完整构建检查；独立 `cargo build`/`clippy`
  步骤对当前规模收益为零。
- **`fail-fast: false`**：双平台结果都要看到，一个平台红不许砍掉另一个。

## Risks / Trade-offs

- [Windows runner 上真实子进程测试（ping/cmd）偶发超时] → 测试自带 8s 驱动
  上限与幂等清理；若 CI 偶发红，重跑定位后再调时序。
- [仓库尚无远程，CI 状态未知] → 推送到 GitHub 后闭环任务 1.2。

## Open Questions

（无）
