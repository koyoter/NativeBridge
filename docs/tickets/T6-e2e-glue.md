# T6: glue 层 + 端到端联调 + 打包验证

Blocked by: T3, T5
Blocks: —

> **状态（2026-09-12）**：glue.rs / DownloadManager.cs / dist1.sh 打包均已落地，
> `cargo test` 全量绿；下方"端到端集成测试 + 构建矩阵冒烟"**延后至 C# 端联调**
> （2026-09-11 评审决策），本票在那之前不得关闭。

## 目标

按设计文档 §3.1（sink）/§4 落地 `modules/glue.rs` 桥接，完成"删 → 整 → 预扩容 → C# 只读 → 下载进 VFS → C# 读新文件"全链路验证，并过一遍各平台构建与打包。

## 范围

- `glue.rs`：用 vfs writer 实现 dlmgr sink vtable（`prepare/write_at/reset/finish/abort`），`dlvfs_sink_create_for_vfs(vfs, name, size) → sink` FFI；abort 时 writer_abort、finish 时校验后 writer_commit。
- 确认依赖方向：vfs 与 dlmgr 互不引用，仅 glue 依赖两者（代码审查项）。
- C# 侧 `DownloadManager.cs`（dlmgr 全套 P/Invoke + 回调 delegate keep-alive）。
- 端到端集成测试（本地 mock CDN：range 源 + full 源两套 URL）：
  1. 建库写入若干文件 → 删除部分 → compact(reserve_extra=清单总量)；
  2. C# mmap 只读旧文件同时 Rust 下载新一批（并发、限速生效）；
  3. 全部 commit 后 C# 刷新 generation/enumerate → 逐文件内容校验；
  4. 中途杀进程重启 → 已 commit 文件完好、Downloading 残留被清理、可重新整理。
- 构建矩阵冒烟：win32 x64/arm64、android armv7/arm64、osx、ios、tvos（至少编译 + 链接 + 导出符号齐全）；`dist1.sh` 产物包含 `Vfs.cs`/`DownloadManager.cs`。

## 验收标准

- 端到端测试全绿，且全程 C# 读到的已 commit 数据与源 crc32 一致（无撕裂、无互相破坏）。
- 整理后物理大小 == 预期最终大小（逻辑 + reserve），下载期间物理大小不变（stat 断言）。
- 重启恢复场景通过（设计 §2.2/§2.3 崩溃矩阵实测）。
- 各平台 CI 构建通过；`nativebridge` ABI 版本流程走查（新增导出不 bump、头文件与 C# 一致）。
- 零依赖复查：`cargo tree` 无新增第三方依赖。
