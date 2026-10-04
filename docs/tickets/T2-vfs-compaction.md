# T2: VFS 碎片整理

Blocked by: T1
Blocks: T3

## 目标

按设计文档 §2.3 实现异步碎片整理状态机 `Idle → Scan → Move → Finalize → Idle`，保证崩溃安全与 BUSY 并发规则。

## 范围

- `vfs/compact.rs`：内部线程 + 状态机 + 进度/完成回调。
- Move 规则（设计 §1.1-5）：offset 升序两指针；`offset==cursor` 不动；`gap ≥ size` 前移进空洞；否则搬到当前数据末尾之后。读写双句柄、~4MB 分块拷贝、边读边校验 crc32，坏块标 `Bad`。
- Finalize：fsync → 新 header 一次性翻转（剔除 Deleted/Bad、全部新 offset、新逻辑大小）→ `set_len(新逻辑 + reserve_extra)` → flush → generation++。
- 并发规则：有 writer 或整理中 → `BUSY_WRITERS`/`BUSY_COMPACTING`；整理期间 lookup/enumerate/alloc/delete/commit 全 `BUSY`；`stat/compact_status/get_generation` 不受限。
- 崩溃安全：搬迁目标恒 ≥ 旧逻辑末尾，翻转前旧 header 恒成立。

## 验收标准

- 单测：构造删→整场景（首部空洞/中部小空洞/尾部大空洞三布局），整理后无空洞、offset 正确、数据 crc 校验通过、`reserve_extra` 后物理大小 == 新逻辑 + reserve。
- 单测：Move 中途注入 IO 错误/崩溃点 → 重开后旧索引完好、数据未损坏、可再次整理成功。
- 单测：整理期间发起 lookup/alloc/delete 返回 `BUSY_*`；有活跃 writer 时 `vfs_compact` 拒绝。
- 单测：crc 损坏文件在整理中被标 Bad 并在 Finalize 剔除，回调/结果可见。
- 进度回调从 0% 推进到 100%，Finalize 后触发 done 回调。
