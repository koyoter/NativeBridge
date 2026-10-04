# VFS + 下载管理器 技术设计（终稿）

> 状态：已评审确认（2026-08-30）；2026-09-11/12 经两轮评审修订，碎片整理规则与
> generation 语义以本文当前描述为准（修订裁决记录见 [vfs-dlmgr-grilling.md](vfs-dlmgr-grilling.md)）。
> 范围：`src/nativebridge` 下两个新模块 `vfs`、`dlmgr`（外加薄 glue 层），基于现有 `curlw`。
> 实现进度与任务拆分见 [tickets/README.md](tickets/README.md)。

## 0. 目标与关系

- **方案一 VFS**：简化版虚拟文件容器（`header.vfs` 索引 + `files.vfs` 数据），只存文件名+后缀、无目录概念。支持软删除、碎片整理、C# FileMapping 读 + Rust 持续写、空间预扩容。
- **方案二 dlmgr**：下载管理器。DOH、worker 线程数、双 URL 兜底、HTTP Range、全局+单任务限速、单任务/全局进度回调、可直接下载进 VFS。
- **独立但兼容**：Rust 层 `vfs` 与 `dlmgr` 编译期互不依赖，`dlmgr` 只认识 sink vtable；组合由 glue 层完成。C# 层两套独立 P/Invoke 绑定，可只绑其一。
- 主要流程：删除无用文件 → 碎片整理（+预扩容）→ C# 只读 → 下载新一批文件（Rust 写，C# 可读已 commit 文件）。

## 1. 决策记录（grilling Q1–Q17）

| # | 决策 |
|---|---|
| Q1 | header.vfs 不暴露给 C#，索引查询走 FFI（Rust 内存索引应答，热路径零文件 I/O）；C# 的 FileMapping 只映射 files.vfs 数据区 |
| Q2 | 文件数不设硬上限、可扩展，量级 ≤ 2 万；文件名放独立 StringArea，不进定长槽位 |
| Q3 | 空洞平时不复用：新数据一律追加，软删除/坏数据的空间只在碎片整理时回收 |
| Q4 | 整理结束立即 truncate 垃圾并预扩容到 `最终大小 + reserve_extra`；下载期间物理大小不变 → C# 一次 map 到底 |
| Q5 | dlmgr 用 Rust 自建 worker 线程池，每线程一条 curl easy 阻塞跑单任务（单连接） |
| Q6 | 不做跨重启续传：open 时 Downloading 残留直接丢弃（数据成垃圾，留给下次整理） |
| Q7 | 单文件不拆多 range 并行；任务间并行由线程数控制 |
| Q8+Q17② | 限速 = 全局令牌桶 + 单任务令牌桶，实际速率取 min；均可动态设置 |
| Q9 | 兼容缝 = sink vtable；glue 层（同 crate）桥接 vfs↔dlmgr，两模块互不 include |
| Q10 | 零依赖政策不变（std + libc）；crc32/令牌桶/优先级队列/线程池全部手写 |
| Q11 | 无定时落盘：只有显式 `vfs_flush()` + 优雅 close 时落盘；崩溃丢最近未 flush 的 commit（重下即可） |
| Q12 | C# 感知索引变化：generation 必有（alloc/abort/commit/delete/整理都 bump）+ 可选 commit 回调 |
| Q13 | 整理为异步任务：有写入者或正在整理 → 拒绝（BUSY）；整理期间 lookup/enumerate/alloc/delete 全部 BUSY |
| Q14 | 会话内断线重连要 Range 续传（不跨重启） |
| Q15 | manifest 提供每文件 size + crc32，commit 前强制校验 |
| Q16 | 进度回调走专用 reporter 线程，10Hz，单线程节流，先逐任务后全局 |
| Q17 | 重试默认 3 次指数退避（1s/2s/4s）；FileSink v1 一并做；任务优先级要做（队列插队） |
| 补充 | 任务携带双 URL：`range_url`（主）+ `full_url`（兜底）；对 range 源过三道闸（非 206 / 总长≠size / crc32 不符），任一不过判定"源不可信"→ 整任务切 full_url 从 0 重下（不消耗重试） |

### 1.1 默认决策（评审中未逐条确认，实现按此执行，可推翻）

1. **首发请求即带 Range 头**（`Range: bytes=0-`）：range 支持性从第一个响应就开始验证，不等第一次断线。
2. **同名 alloc**：对 `Deleted` 条目允许替换（旧数据区成垃圾）；对 `Active`/`Downloading` 返回 `ALREADY_EXISTS`。即"替换 = C# 先 delete"。
3. **优先级不抢占**：高优先级任务先被空闲 worker 取走，运行中任务不被暂停。
4. **fallback 一次性**：range_url → full_url 的切换每任务至多一次，不消耗重试次数；full_url 上的失败才消耗重试。
5. **整理 Move 规则细化**（2026-09-12 修订，R02 裁决）：按 offset 升序两指针；`offset==cursor` 原地不动；`gap ≥ align4k(size)` 且候选空洞**不命中任何已搬条目的旧源区**时前移进空洞（非重叠拷贝）；否则（含命中已搬源区的 case-A）搬到当前数据末尾之后并置 `excluded_any`。崩溃安全根基 = "**Move 前把当前索引落盘 + 搬迁目标不覆盖任何源区 + header 只在 Finalize 翻转一次**"；被排挤或剔 Bad 的轮次留下可回收的洞 → 多轮 Scan→Move→Finalize（上限 8 轮）直到收敛。

## 2. 方案一：VFS

### 2.1 磁盘格式（小端）

```
header.vfs = SuperBlock(64B) + Region A + Region B        （双缓冲，单 active）

SuperBlock:
  magic[8] = "1KVFSHDR" | format_version u32 | active_gen u64 | sb_crc u32 | reserved

Region:
  region头(32B): gen u64 | entry_count u32 | string_area_len u32 | region_crc u32
  EntryTable: Entry × entry_count
  StringArea: UTF-8 文件名（含后缀）连续存放，NUL 结尾便于 C# 侧切分

Entry (32B):
  name_off u32 | name_len u16 | state u8 | reserved u8
  offset u64 | size u64 | crc32 u32 | entry_crc u32

state: Active=0, Deleted=1, Downloading=2, Bad=3
```

- 翻转时按需写更大的 region（header.vfs 尾部追加），容量无硬上限；2 万文件约 3MB。
- `files.vfs` = 纯数据区，文件按 **4K 对齐**连续摆放，无内联头。`data_logical_size`（SuperBlock/region 内）决定逻辑大小；物理大小 ≥ 逻辑（预扩容富余）。

### 2.2 打开与读写协议

- **open**：读 SuperBlock（sb_crc 坏则扫描双 region 取 gen 最高且校验通过者）→ 载入 active region → 建内存索引（`Vec<Entry>` + 名字 blob + `HashMap<String, idx>`）。`Downloading` 残留条目丢弃（数据成垃圾）；逻辑大小之外的多余物理数据（崩溃残留）可安全忽略/截断。
- **写入三段式**：
  1. `alloc(name, size)`：校验（无整理、无同名 Active/Downloading）→ 登记 Downloading 条目、在逻辑末尾预留 4K 对齐区间、物理不足则 `set_len` 扩容 → 返回 writer 句柄。
  2. `writer_write(rel_off, buf)`：用 writer **自己的 OS 句柄** pwrite 到 `entry.offset + rel_off`（Windows std 同句柄并发写不安全 → 每写入者独立句柄），越界即错；边写边累计 crc32（约束：顺序写满 `[0,size)`）。
  3. `writer_commit()`：校验写满 → 条目置 Active、crc 入索引 → generation++ → 触发 commit 回调。`writer_abort()`：条目丢弃，区间成垃圾。
  - **commit 后数据永不移动**（整理期整体锁死），C# 已读数据不可能被破坏。
- **C# 可见性**：commit 即刻可见（内存索引应答 FFI）；C# 经 `MemoryMappedFile` 读 files.vfs，本地页缓存与 pwrite 天然一致，无需 flush 即可读。**唯一防撕裂闸门 = 只读 Active 条目**。
- **flush（显式，无定时）**：fsync files.vfs（Windows `FlushFileBuffers`；macOS 需 `F_FULLFSYNC`）→ 序列化写满 inactive region → fsync header.vfs → 原地更新 SuperBlock.active_gen。
- **线程模型**：VFS 句柄多线程安全（索引内部 Mutex）；单个 writer 仅限创建线程使用。

### 2.3 碎片整理（异步任务）

```
Idle → Scan → Move → Finalize → Idle
```

- **入口条件**：无活跃 writer 且不在整理中，否则 `BUSY`。期间 `lookup/enumerate/alloc/delete/commit` 全部 `BUSY`；`stat/compact_status/get_generation` 正常。
- **Scan**：Active 条目按 offset 排序，算最终逻辑大小；Deleted/Bad 区全部算垃圾。
- **Move**（规则见 1.1-5）：读、写用两个独立句柄；分块（~4MB）拷贝，边读边校验 crc32，读不动 → 标 Bad（Finalize 时剔除）；进度回调（percent + 当前文件名）。
- **Finalize**：fsync files.vfs → 组新 header（剔除 Deleted/Bad，全部新 offset）→ **一次性翻转** → `set_len(新逻辑大小 + reserve_extra)`（截垃圾 + 预扩容一步完成）→ flush。generation++，**C# 必须重新 enumerate**（offset 可能整体偏移）。
- **崩溃安全根基**：Move 前先把当前索引（含未 flush 的 delete）落盘，且搬迁目标不覆盖任何源区（case-A 命中已搬源区则排挤到末尾，留待下一轮回收）→ 翻转前旧 header 引用的数据完好；翻转后崩溃则新 header 生效，文件物理偏大，无害。

### 2.4 FFI 导出

```
vfs_open(dir) → handle                       vfs_close(handle)
vfs_open_paths(index_path, data_path) → handle   // 路径式打开：索引/数据分别指定，
                                                 // 可同目录异名或异目录；空路径或
                                                 // 同一路径（去空白+大小写折叠比较）
                                                 // → INVALID_ARG。纯新增，ABI 版本不变。
vfs_flush(handle)
vfs_alloc(handle, name, size) → writer       vfs_writer_write(w, rel_off, buf, len)
                                             vfs_writer_commit(w) | vfs_writer_abort(w)
vfs_delete(handle, name)
vfs_lookup(h, name, →off, →size, →state, →crc)          // out 参数可空
vfs_enumerate_query(h, →count, →blob_len)               // 两段式，gen 变了返回 GEN_CHANGED
vfs_enumerate_read(h, names_buf, off[], size[], crc[], state[], cap)
vfs_get_generation(h) → u64
vfs_set_commit_callback(h, cb(user,name,off,size,crc), user)   // 可选
vfs_stat(h, →logical, →physical, →total, →active, →deleted, →garbage)
vfs_compact(h, reserve_extra u64, progress_cb, done_cb, user)  // 异步
vfs_compact_status(h) → state, percent
```

错误码（i32，模块内编号）：`OK / IO / NOT_FOUND / ALREADY_EXISTS / BUSY_COMPACTING / BUSY_WRITERS / CRC_MISMATCH / WRITE_OVERFLOW / GEN_CHANGED / BUFFER_TOO_SMALL / INVALID_ARG / STATE_INVALID`。

### 2.5 C# 侧（`csharp/Vfs.cs`）

`VfsReader`：`Open(indexPath, dataPath)`（路径式主构造）与 `Open(dir)`（目录薄壳重载，等价 `dir/header.vfs + dir/files.vfs`）；属性 `IndexFilePath` / `DataFilePath`；`RefreshIndex()`（enumerate → `Dictionary<string,(off,size,state,crc)>`，对比 physical 变化则重建 mapping）；`TryGet(name)`；`ReadRaw(name)` → 非托管指针 + 长度（unsafe，映射视图内切片）；generation 轮询或 commit 回调触发刷新。

## 3. 方案二：dlmgr

### 3.1 配置与任务（FFI 扁平参数，不跨 ABI 传 struct）

```
dlmgr_create(worker_count u32=4, global_bps u64=0, retry_count u32=3) → mgr
dlmgr_add_doh_url(mgr, url)          // 启动前逐条添加（CURLOPT_DOH_URL）
dlmgr_start(mgr)
dlmgr_enqueue(mgr, range_url, full_url, name, size, crc32, priority i32, task_bps, sink, →task_id)
dlmgr_cancel(mgr, id) / dlmgr_cancel_all(mgr)
dlmgr_pause(mgr) / dlmgr_resume(mgr) // 停止派发新任务，运行中跑完；C# 以 active_count==0 作为整理前置
dlmgr_set_global_speed(mgr, bps)     // 动态生效
dlmgr_set_task_callback(mgr, cb, user) / dlmgr_set_global_callback(mgr, cb, user)
dlmgr_active_count(mgr) / dlmgr_shutdown(mgr)
dlmgr_sink_create_file(mgr, path, size) → sink   // FileSink（每任务独立句柄）
```

任务字段：双 URL + size（必填）+ crc32（必填）+ priority（大者先取、同级 FIFO、不抢占）+ 单任务 bps。**双令牌桶**（全局/单任务），实际速率 = min，0 = 不限。

### 3.2 执行链（每任务单连接，worker 阻塞跑 curl easy）

```
Pending → Running:
  range_url: GET + Range: bytes={off}-（首发 off=0，自带探测）
    ├ 206 且 Content-Range 总长 == size → 流式下载（边写边累计 crc32）
    │    断线 → 重连 Range: bytes=off-；206 续写（crc 状态延续）；200 → 判源不可信 → 兜底
    ├ 200 / 总长≠size / crc32≠预期 → 判源不可信 → 一次性切 full_url 从 0 重下（不消耗 retry）
    └ 网络错误/超时 → 同 URL 带断点重试（≤ retry_count）→ 用尽且未兜底过 → 兜底
  full_url: 普通 GET，Content-Length==size 闸 + crc 闸；失败消耗 retry（1s/2s/4s 指数退避）
→ Verifying（crc 流式已算完，直接判定）→ sink.finish()（VfsSink 即 commit）→ Done
用尽 retry / 主动取消 → Failed / Canceled
错误码：UNSUPPORTED_RANGE / SIZE_MISMATCH / CRC_MISMATCH / NETWORK / TIMEOUT / CANCELED
```

sink vtable（C ABI 函数指针，glue 以此桥接 vfs）：
`{ ctx, prepare(size), write(rel_off, ptr, len), reset(), finish(), abort() }`，sink 以 u64 句柄在 mgr 注册表内存活。

### 3.3 回调（reporter 线程，10Hz）

同一轮先逐任务 `cb(task_id, state, done, total, bps, err)`，再全局 `cb(active, done_cnt, failed_cnt, bytes_done, bytes_total, bps)`。C# 静态 delegate + `[MonoPInvokeCallback]` + GCHandle keep-alive（照抄 Curlw.cs 纪律），主线程分发由 C# 自理。

### 3.4 curlw 集成

dlmgr crate 内直调 `modules/curlw` 的 Rust 绑定：每 worker 一个 easy handle + 全局 share handle（DNS 锁共享），DOH 经 `CURLOPT_DOH_URL` 注入。

## 4. 模块落位与共享约定

```
rust:   src/nativebridge/rust/src/modules/
          vfs/{mod.rs, format.rs, index.rs, compact.rs}
          dlmgr/{mod.rs, task.rs, worker.rs, rate.rs, report.rs, curlw_backend.rs}
          glue.rs                                       # vfs↔dlmgr 桥，唯一同时依赖两者处
C ABI:  rust/src/modules/vfs/mod.rs   # VFS_ABI_VERSION 1（权威契约）
        rust/src/modules/dlmgr/mod.rs # DLMGR_ABI_VERSION 1（权威契约）
C#:     src/nativebridge/csharp/Vfs.cs, DownloadManager.cs
```

- 沿用 curlw 惯例：UTF-8 字符串（`byte[]`，禁 `CharSet.Ansi`）、opaque `IntPtr` 句柄、不跨 ABI 传 struct、回调 keep-alive、cdecl、`panic=abort`（FFI 边界不吞 unwind）。
- 纯新增导出不 bump ABI version；改签名/布局必须 bump 并同步 C#。
- 零依赖（std + libc）；tvOS `-Z build-std` 不受影响。
- 平台矩阵：win32(x64/arm64)、linux(x64/arm64)、android(armv7/arm64/x86/x64, API 21+)、osx(x64/arm64)、ios、tvos。

## 5. 实现顺序

vfs 轨（T1→T2→T3）与 dlmgr 轨（T4→T5）可并行，T6 汇合。详见 [tickets/README.md](tickets/README.md)。
