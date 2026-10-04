# T1: VFS 磁盘格式 + 内存索引 + 基础读写

Blocked by: —
Blocks: T2, T3

## 目标

按设计文档 §2.1/§2.2 实现 `modules/vfs` 的磁盘格式、打开恢复协议和写入三段式（alloc → write_at → commit/abort），不包含碎片整理与 FFI 导出。

## 范围

- `vfs/format.rs`：SuperBlock / Region / Entry 的序列化与校验（小端、crc32）。
- `vfs/index.rs`：内存索引（`Vec<Entry>` + 名字 blob + `HashMap`），generation 计数，commit/delete 变更。
- `vfs/mod.rs`：`open / close / alloc / writer_write / writer_commit / writer_abort / delete / flush / lookup / stat`（Rust 内部 API，先不含 FFI）。
- flush 协议：fsync files.vfs → 写 inactive region → fsync header.vfs → 更新 active_gen（macOS `F_FULLFSYNC`、Windows `FlushFileBuffers`）。
- 打开恢复：SuperBlock 损坏扫描双 region；Downloading 残留丢弃；逻辑大小外的物理残留忽略。
- writer 每个独立 OS 句柄；顺序写约束 + 边写边算 crc32；越界 `WRITE_OVERFLOW`。
- alloc 规则：`Deleted` 可替换，`Active`/`Downloading` 报 `ALREADY_EXISTS`；无预扩容时物理按需 `set_len`。

## 验收标准

- 单测：格式 roundtrip（写→flush→重开→索引一致）；单 region 翻转后另一份可被恢复路径选中（人为破坏 active region）；Downloading 残留 open 后消失且 stat.garbage 计入。
- 单测：commit 后 `lookup` 可见、generation 递增；写入中途 abort 后区间成垃圾；同名 alloc 三种状态行为正确。
- 单测：flush 前模拟崩溃（直接重开不 flush）→ 未 flush 的 commit 丢失、已 flush 的完好（append-only 不被覆盖）。
- 2 万条目规模下 open + enumerate 内存索引构建 < 100ms（debug 构建放宽）。
- `cargo test -p nativebridge vfs` 全绿；零依赖不变（仅 std + libc）。
