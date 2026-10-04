# Tickets：VFS + 下载管理器

设计依据：[../vfs-dlmgr-design.md](../vfs-dlmgr-design.md)。两条轨可并行推进，T6 汇合。

```
vfs 轨:  T1 ──→ T2 ──→ T3 ──┐
                            ├──→ T6 端到端联调
dlmgr 轨: T4 ──→ T5 ────────┘
```

| Ticket | 内容 | 依赖 | 轨 | 状态（2026-09-12） |
|---|---|---|---|---|
| [T1](T1-vfs-format-core.md) | VFS 磁盘格式 + 内存索引 + 基础读写/崩溃恢复 | — | vfs | ✅ 单测通过 |
| [T2](T2-vfs-compaction.md) | 碎片整理（Scan/Move/Finalize + BUSY 状态机） | T1 | vfs | ✅ 单测通过 |
| [T3](T3-vfs-ffi-csharp.md) | VFS FFI 导出 + C# `Vfs.cs`（enumerate/mmap 读） | T1, T2 | vfs | ✅ 单测通过；C# mmap 联调随 T6 |
| [T4](T4-dlmgr-skeleton.md) | dlmgr 骨架（配置/优先级队列/worker 池/FileSink/curlw 后端） | — | dlmgr | ✅ 单测通过 |
| [T5](T5-dlmgr-semantics.md) | 双 URL 兜底链/会话内续传/重试/双桶限速/reporter 回调 | T4 | dlmgr | ⏸ 纯函数单测通过；mock server 集成矩阵延后至 C# 端 |
| [T6](T6-e2e-glue.md) | glue 层 VfsSink + 下载进 VFS 端到端 + 打包验证 | T3, T5 | 汇合 | ⏸ 代码已落地；端到端/平台矩阵延后至 C# 端联调 |

状态口径：**"单测通过" = `cargo test` 全量 42/42 绿**（2026-09-12，含两轮评审回归用例）。
T5 的 mock server HTTP 矩阵与 T6 的端到端/杀进程/平台冒烟，按 2026-09-11 评审决策**延后至
C# 端**补齐——两张票的验收标准原文保持不变，作为延后工作的对照清单，勿当"已完成"关闭。
