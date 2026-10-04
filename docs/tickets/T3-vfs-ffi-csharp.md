# T3: VFS FFI 导出 + C# Vfs.cs

Blocked by: T1, T2
Blocks: T6

## 目标

把 T1/T2 的 Rust 内部 API 按 curlw 惯例导出为 C ABI（设计 §2.4），并交付 C# 侧 `Vfs.cs` 绑定与 `VfsReader`（设计 §2.5）。

## 范围

- Rust `#[no_mangle] extern "C"` 导出全集（权威 C ABI 契约：`rust/src/modules/vfs/mod.rs`，`VFS_ABI_VERSION 1`）：open/close/flush/alloc/writer 三件套/delete/lookup/enumerate 两段式/get_generation/commit 回调/stat/compact/compact_status。
- 惯例：UTF-8 `*const c_char`、opaque 句柄、out 参数、错误码 i32、回调 keep-alive 语义写入头注释。
- `csharp/Vfs.cs`：手写 P/Invoke（cdecl、`byte[]` UTF-8、禁 `CharSet.Ansi`），`VfsReader`：`RefreshIndex()`（enumerate → `Dictionary`，检测 physical 变化重建 `MemoryMappedFile`）、`TryGet`、`ReadRaw`（unsafe 指针切片）、generation 轮询 + 可选 commit 回调事件。
- `build1.ps1`/`build.yml` 无需变更（Rust 模块无条件打包），验证 cdylib 导出表含全部 `vfs_*` 符号。

## 验收标准

- C# 冒烟测试（编辑器内或独立 console host）：Rust 写 3 个文件 → commit → `VfsReader` 刷新索引 → `ReadRaw` 内容逐字节一致。
- 软删除 + `vfs_compact` 异步 → C# 收 done 后刷新索引，被删文件消失、offset 已更新、数据仍可读。
- 下载模拟（后台线程写 + commit）期间 C# 持续读已 commit 文件，无撕裂、无异常（可见性验证）。
- enumerate 两段式在 gen 变化时返回 `GEN_CHANGED`，C# 重查收敛。
- 头文件与 Curlw.h 风格一致；`vfs_abi_version()` 导出可查询。
