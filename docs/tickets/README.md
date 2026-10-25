# Tickets：VFS + 下载管理器

设计依据：[../vfs-dlmgr-design.md](../vfs-dlmgr-design.md)。两条轨可并行推进，T6 汇合。

```
vfs 轨:  T1 ──→ T2 ──→ T3 ──┐
                            ├──→ T6 端到端联调
dlmgr 轨: T4 ──→ T5 ────────┘
```

| Ticket | 内容 | 依赖 | 轨 | 状态（2026-10-25） |
|---|---|---|---|---|
| [T1](T1-vfs-format-core.md) | VFS 磁盘格式 + 内存索引 + 基础读写/崩溃恢复 | — | vfs | ✅ 单测通过；F_FULLFSYNC 2026-10-24 落地（apple 家族全落盘点） |
| [T2](T2-vfs-compaction.md) | 碎片整理（Scan/Move/Finalize + BUSY 状态机） | T1 | vfs | ✅ 单测通过 |
| [T3](T3-vfs-ffi-csharp.md) | VFS FFI 导出 + C# `Vfs.cs`（enumerate/mmap 读） | T1, T2 | vfs | ✅ 单测通过；C# mmap 联调随 T6 |
| [T4](T4-dlmgr-skeleton.md) | dlmgr 骨架（配置/优先级队列/worker 池/FileSink/curlw 后端） | — | dlmgr | ✅ 单测通过 |
| [T5](T5-dlmgr-semantics.md) | 双 URL 兜底链/会话内续传/重试/双桶限速/reporter 回调 | T4 | dlmgr | ✅ 关票 2026-10-24：C# mock 矩阵补齐（断连续传 Range 断言/退避 1s2s4s 实测/crc+总长兜底/限速三态/10Hz+先任务后全局/字节对账），rc.1 CI csharp-test-win/mac 全绿 |
| [T6](T6-e2e-glue.md) | glue 层 VfsSink + 下载进 VFS 端到端 + 打包验证 | T3, T5 | 汇合 | ✅ 关票 2026-10-25：构建矩阵=CI 17 job ✓、dist 产物断言已进 CI（2026-10-24）、下载期间物理大小不变已测、e2e 由 DlmgrTests 覆盖；killrecovery.ps1 人工运行**通过**（2026-10-25，Windows：stage1 flush 后整树杀 → stage2 重启恢复 1/1 绿；脚本当日补 UTF-8 BOM 修 PS 5.1 解析，85ba9b4） |

状态口径（2026-10-25）：T5 关票依据 = rc.1 tag（run 40092256349）csharp-test-win/mac + 17 平台构建全绿
（dist 的版本一致性断言按设计拒绝 rc tag 的旧 ver，属预期）。T6 关票依据 = killrecovery.ps1 2026-10-25
人工运行通过（stage2 恢复语义 1/1 绿）；杀进程重启按 2026-09-11 评审决策保持手动验证口径。
