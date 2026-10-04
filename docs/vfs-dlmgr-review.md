# VFS / 下载管理器改动审查总结

> 注（2026-10）：`src/nativebridge/native/` 已删除，Rust 为唯一实现；下文出现的 native/ 头文件路径与行号为审查时快照，仅作历史记录。

## 结论与范围

**当前审查版本不建议合并，尚不满足设计文档与 T1～T6 的验收要求。** 主要风险包括越界写、持久化数据丢失、整理崩溃恢复失败、下载状态机错误，以及 C# 映射和回调生命周期问题。

- 范围：本次对话上一轮审查时的 Git 工作区改动，包含未跟踪新增文件。
- 依据：[技术设计](vfs-dlmgr-design.md)、[任务与验收标准](tickets/README.md)。
- 方法：按 Ponytail 原则，由三个子代理分别审查 VFS、dlmgr、C#/ABI，主代理去重、复核并执行针对性验证；不把纯风格偏好列为缺陷。
- 状态：以下为审查发现，**尚未修复**。本总结不代表重新验证了之后的代码变更；行号对应审查快照。
- 证据：“已复现”表示运行验证；“现场模拟”表示构造对应磁盘状态验证恢复行为，不等同于真实杀进程测试；其余为源码或并发时序确认。

文中代码路径均相对仓库根目录。

## P1：合并前优先修复

### R01：两段式枚举可能越界写入名字缓冲区

- 位置：`src/nativebridge/rust/src/modules/vfs/mod.rs:841`。
- 触发与影响：query 后 abort/重新 alloc，条目数和 generation 可以不变，但名字 blob 增长；read 按新长度写入旧容量缓冲区。**已复现：query 声明 2 字节，read 返回成功并写入其后的字节。**
- 最小修复：所有影响枚举输出的结构变化均使快照版本失效，保证版本检查、尺寸检查与输出一致。对应 T3 的 `GEN_CHANGED` 契约。

### R02：整理 Move 覆盖旧 header 仍引用的源数据

- 位置：`src/nativebridge/rust/src/modules/vfs/compact.rs:57`。
- 触发与影响：D/A/B 各占 4KB，删除 D 后，A 前移、B 写入 A 的旧位置；新 header 发布前，旧索引中的 A 已读到 B。**已通过 Move 阶段快照复现**，此时崩溃或后续 I/O 失败无法恢复旧数据。
- 最小修复：搬迁目标必须避开旧持久化索引引用的全部源区，不能提前复用已搬文件的旧源区。对应 T2 的失败恢复要求。

### R03：Finalize 在发布新 header 前截断数据

- 位置：`src/nativebridge/rust/src/modules/vfs/compact.rs:180`。
- 触发与影响：`set_len` 先于 `persist_image`；在二者之间崩溃，旧索引指向的数据已经被截掉。**对应磁盘现场模拟后读到零。**
- 最小修复：同步数据 → 持久化新 header → 截断/预扩容 → 再同步。对应设计 §2.3 的 Finalize 顺序。

### R04：序列化名字时覆盖旧偏移，造成错误改名

- 位置：`src/nativebridge/rust/src/modules/vfs/format.rs:171`。
- 触发与影响：先改写 `name_off`，再用它读取旧 blob。**已复现：alloc dead → abort → 写入 live → flush → reopen 后，live 消失，其数据登记到 dead 名下。**
- 最小修复：先按旧偏移提取名字，再设置新偏移。对应 T1 的索引持久化要求。

### R05：较小 region 复用大槽位，破坏后续扫描

- 位置：`src/nativebridge/rust/src/modules/vfs/mod.rs:187`。
- 触发与影响：允许新 region 小于旧槽位，但扫描器没有独立槽位容量，只能按新长度前进。**已复现：整理后再提交、flush、重开，generation 从 6 回退到 5，新文件消失。**
- 最小修复：当前格式下仅等长复用，否则追加；不必立即设计新的槽位分配系统。

### R06：枚举名字与 offset/size 数组错配

- 位置：`src/nativebridge/rust/src/modules/vfs/mod.rs:495`。
- 触发与影响：entries 使用 `swap_remove`，名字 blob 保留历史内容；C# 按顺序配对后，将现存文件的数据挂到已 abort 的名字下。**已复现，无需并发或 flush。**
- 最小修复：按当前 entries 顺序重新打包枚举名字，query 返回对应长度。与 R01、R04 独立，修复其中一项不会自动修复其他项。

### R07：旧 flush 快照可以在 compact 完成后发布

- 位置：`src/nativebridge/rust/src/modules/vfs/mod.rs:424`。
- 触发与影响：flush 取得快照后释放 `inner` 锁；compact 搬迁、截断并发布新索引后，旧 flush 仍可发布旧 generation/offset，导致重开使用失效位置。`persist_lock` 只保护写入，不保护快照顺序。
- 最小修复：将状态检查、快照、同步与发布纳入一致的串行化范围。证据为锁与执行时序审查，未做调度注入复现。

### R08：优雅 close 不落盘，正常退出也丢提交

- 位置：`src/nativebridge/rust/src/modules/vfs/mod.rs:707`。
- 触发与影响：`vfs_close` 只释放 `Arc`，没有 flush。**已复现：commit 后直接 close/reopen，文件消失。**
- 最小修复：在 native 关闭路径复用 flush，并明确失败处理，不能只补 C#。对应设计 Q11“显式 flush + 优雅 close 落盘”。

### R09：合法 206 被错误判为 Range 不可信

- 位置：`src/nativebridge/rust/src/modules/dlmgr/curlw_backend.rs:158`、`src/nativebridge/rust/src/modules/dlmgr/worker.rs:154`。
- 触发与影响：`Content-Range: bytes 0-99/100` 被解析出的“起点”实际为 99，再与下载后的偏移 100 比较，正常响应误走 full；没有 full 时失败。
- 最小修复：解析真正的 start，并与本次请求起点比较；结束偏移另用于完成判定及续传。仅修解析不足以解决。对应 T5。

### R10：实际偏移未回传，续传和 full 重试失效

- 位置：`src/nativebridge/rust/src/modules/dlmgr/worker.rs:427`。
- 触发与影响：`perform_attempt` 只回传 CRC，调用方 `off` 一直为 0；断线后仍请求 `bytes=0-`，却累计旧 CRC。full 重试的 `if off > 0` 也不执行，reset、CRC/进度清零被跳过。
- 最小修复：所有返回路径同步实际偏移，完整重下分支统一重置 sink、偏移、CRC 与进度。对应 T5 的断点续传和重试要求。

### R11：Windows mmap 打开共享模式冲突

- 位置：`src/nativebridge/csharp/Vfs.cs:363`。
- 触发与影响：路径版本的 `CreateFromFile` 与 Rust 长驻读写句柄共享权限冲突，非空 VFS 的 `RefreshIndex()` 抛 sharing violation。**Windows/.NET 最小复现确认。**
- 最小修复：显式用 `FileAccess.Read`、`FileShare.ReadWrite` 创建 `FileStream`，通过流建立映射，并明确释放归属。对应 T3/T6。

### R12：Compact 前保留映射，Windows 截断失败

- 位置：`src/nativebridge/csharp/Vfs.cs:487`。
- 触发与影响：修复 R11 后，旧映射仍阻止 native `set_len` 缩短文件；done 后再刷新释放映射已经太晚。**Windows 映射期间缩短文件的失败已复现。**
- 最小修复：整理前停止读取、失效 raw span 并释放映射；整理期间拒绝缓存读取，结束后重新枚举、映射。

### R13：回调保活范围不足，可能调用失效函数指针

- 位置：`src/nativebridge/csharp/DownloadManager.cs:189`、`src/nativebridge/csharp/Vfs.cs:505`。
- 触发与影响：dlmgr 的全局单槽 delegate 被其他 manager 注册/清空覆盖；VFS 替换回调也立即丢弃旧引用，而 native 可能已复制旧指针、尚未调用。GC 后存在失效回调风险。
- 最小修复：复用 `src/nativebridge/csharp/Curlw.cs:779` 的按 handle 保存回调集合方式，等原生调用彻底结束后释放，不能只保存最后一个 delegate。两个模块均需修复。

## P2：其他明确问题

| 编号 | 位置 | 问题与最小修复方向 |
|---|---|---|
| R14 | `src/nativebridge/csharp/Vfs.cs:166` | 导入名为 `vfs_writer_write_imp`，但 native 只导出 `vfs_writer_write`。补 `EntryPoint`，否则运行时找不到入口。 |
| R15 | `src/nativebridge/native/modules/vfs/vfs.h:41` | 缺少 `<stddef.h>`，独立 C 编译报 `unknown type name 'size_t'`。已编译验证。 |
| R16 | `src/nativebridge/rust/src/modules/dlmgr/worker.rs:249` | 出队到 Running 存在空窗；pause 后 `active_count==0` 仍可能有任务随后开始写入。在出队临界区内发布运行状态。 |
| R17 | `src/nativebridge/rust/src/modules/dlmgr/worker.rs:275` | 终态先于错误码发布，reporter 可能永久上报 `Failed/Canceled + ERR_OK`。先设置最终字段，再通过 Release/Acquire 发布和读取终态。 |
| R18 | `src/nativebridge/rust/src/modules/dlmgr/mod.rs:354` | shutdown 直接停止 reporter，可能漏最后的 Canceled/Done/Failed 回调。退出前完成最后一次快照。 |
| R19 | `src/nativebridge/rust/src/modules/dlmgr/rate.rs:110` | 等 token 时不检查取消，shutdown 被迫等待已取消任务获得配额。等待循环及写入前检查取消。 |
| R20 | `src/nativebridge/rust/src/modules/dlmgr/worker.rs:371` | 忽略 `sink.reset()` 返回值，失败后仍继续写。失败立即 abort 并进入 Failed。 |
| R21 | `src/nativebridge/native/modules/dlmgr/dlmgr.h:29` | 注释承诺 shutdown 后调用安全失败，但 handle 已释放。修正文档并要求调用方清零句柄，不新增注册表来兑现错误注释。 |
| R22 | `src/nativebridge/dist1.sh:34` | 打包只复制 `Curlw.cs`，没有包含 `Vfs.cs`、`DownloadManager.cs`，违反 T6 的产物要求。 |

## 文档和设计需要澄清的地方

1. **整理算法描述矛盾。** `vfs-dlmgr-design.md:43` 允许向前填洞，`:92` 又要求全部搬迁目标位于旧逻辑末尾之后。应先统一算法与崩溃安全不变量，不能把当前数据损坏解释为符合其中一句描述。
2. **curlw 接入偏离约定。** `src/nativebridge/rust/src/modules/dlmgr/curlw_backend.rs:12` 调用 C ABI wrapper 并再次镜像常量，而 T4/设计 §3.4 要求直接使用 Rust bindings。优先调整现有可见性并复用，不新增抽象层。
3. **集成验收证据不足。** 当前内部单测没有覆盖文档要求的真实 HTTP 断连续传、C# mmap 联调和进程崩溃恢复矩阵；不能据此宣布 T3/T5/T6 完成。

## 已执行验证与限制

| 验证 | 结果与解释 |
|---|---|
| `cargo check --offline` | 通过。 |
| 两份 C# 文件宿主编译 | 通过，有未使用变量警告；不代表 P/Invoke 或 IL2CPP 运行验证通过。 |
| 隔离执行现有 VFS/rate/task/sink 测试 | 22 项中 21 通过、1 失败。 |
| 现有限速测试失败分析 | `src/nativebridge/rust/src/modules/dlmgr/rate.rs:153` 错把“获取部分配额”断言为“一次拿满”。应循环累计配额后检查耗时，不应反改生产契约；与 R19 独立。 |
| 7 个临时 VFS 回归复现 | 全部触发预期缺陷，覆盖 R01～R06、R08；失败用于证明 bug，不能表述为测试通过。 |
| Windows mmap 两个场景 | 共享模式冲突及映射期间截断失败均确认。 |
| `vfs.h` 独立 C 编译 | 因缺少 `size_t` 定义失败。 |
| 完整 `cargo test --offline` | 缺少 `curl.lib`，无法链接；未完成完整测试。 |
| HTTP 端到端、真实杀进程、IL2CPP、跨平台构建 | 本轮未完成，不声称通过。 |

审查时未修改业务源码。临时复现代码保留在被 Git 忽略的 `src/nativebridge/rust/target/review-vfs.rs`，不属于正式回归测试交付。

## 建议修复顺序

- [ ] 先修 VFS 越界写、名称映射、持久化与整理恢复；将临时复现迁入正式回归测试。
- [ ] 修复 C#/native 回调和 mmap 生命周期，以及 ABI 入口、头文件问题。
- [ ] 修复下载 Range/重试状态机、暂停与关闭语义，补真实本地 HTTP 测试。
- [ ] 澄清整理设计，补齐 T3/T5/T6 联调、打包和平台验收后重新审查。

原则：优先修根因并复用现有机制；当前不需要大规模重构。
