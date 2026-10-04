# vfs + dlmgr 评审核对与修复决策（grilling 记录）

- 日期：2026-09-11
- 分支：`rust`（工作区未提交修改：vfs / dlmgr / glue / C# 绑定 / 文档，约 4.5k 行）
- 状态：**已收口** —— 全部决策落定（按推荐项：Q1=A / Q2=A / Q3=显式接口+ReleaseMapping 公开 / Q4=A / Q5=A / Q6=一批），修复已由 6 个子代理并行实施并通过 `cargo check --tests`；单测**执行**待 D 盘修复 + build1.ps1 完整构建链（见 §3）
- 背景：外部引擎对本分支给出评审（P0×1 / P1×2 / P2×3 / P3 及若干小点）。本文档记录逐条验证的结果、核对过程中澄清的机制、以及修复方案的决策树。

---

## 1. 评审验证结论（已逐条核实，全部属实）

### P0 — range 路径 Content-Range 闸门恒失败（断点续传整体失效）✅

两处错位叠加，导致任何 206 响应都过不了闸 → 一律 `Fallback(ERR_SIZE_MISMATCH)`；无 `full_url` 的任务直接终态失败（worker.rs:366-370）。

**Bug 1：解析取错字段**（curlw_backend.rs:156-159）。对 `Content-Range: bytes 0-99/100`：

```rust
let slash = s.rfind('/')?;                        // → "100"  ✓ 总长正确
let total: u64 = s[slash + 1..].trim().parse().ok()?;
let dash = s[..slash].rfind('-')?;                // rfind 取最后一个 '-'
let start: u64 = s[..slash][dash + 1..].trim().parse().ok()?;  // → "99" ✗ 取到结束偏移
```

`rfind('-')` 取到的是**结束偏移**（99），与 doc 注释声称的"起始偏移"（0/断点）相悖。应为起始偏移 50 之类时取到 99/结束值。

**Bug 2：闸门比较对象错位**（worker.rs:153-157）。`perform_attempt` 把**尝试结束后的累计偏移** `now_off` 塞进 `HttpOk.off`（worker.rs:447-449），而 `transition` 的注释与闸门语义期望它是"本次请求起始偏移（断点）"：

```rust
// 注释：起始偏移必须等于断点、总长必须等于任务 size
if *cr != Some((*off, size)) {   // off 实为收完后的累计偏移
```

**示例**（100 字节文件、断点 50）：服务器正确回 `Content-Range: bytes 50-99/100`，解析得 `(99, 100)`（应为 `(50, 100)`），闸门拿它与 `Some((100, 100))` 比较 → 恒不等 → 兜底。

**两个现有单测按当前实现必挂**（纯函数手推可确定性证明；本机因缺少 curl 静态库 + D 盘硬件错误未能实际执行）：

- `range_done_when_complete_and_crc_ok`（worker.rs:515-522）：构造 `cr=Some((0,100))`、`off=100` → 闸门不等 → 实得 `Fallback(SIZE_MISMATCH)`，断言 `Done` 失败；
- `range_crc_mismatch_falls_back`（worker.rs:505-513）：crc 闸排在 Content-Range 闸之后，实得 `SIZE_MISMATCH` 而非断言的 `CRC_MISMATCH`。

其余 `range_*` 测试恰好通过（`range_content_range_mismatch_falls_back` 属"错错相抵"地通过）。

**⚠️ 核对中的新发现（影响修法）**：设计文档 §3（design.md:35,142）与 T5 规格定义的闸门只有三道——**非 206 / 总长≠size / crc 不符**，并没有"起始偏移==断点"这道闸（那是 worker.rs 注释里实现自加的）。起始偏移错误最终必被 crc32 闸兜住（crc 覆盖全部写入字节），起始闸的增量价值只是快速失败。→ 引出 **Q1**。

### P1 — flush 与 compact 并发可使 header 回退到旧布局 ✅

机制（双缓冲翻转）：`header.vfs` = SuperBlock（SB）+ region A/B 两份完整索引；更新永远是"新索引写进闲置槽 → fsync → SB 翻转指向它 → fsync"。做"persist+翻转"的有两处：

- `flush()`（mod.rs:423-435）：快照内存索引（gen=N）→ **锁外**慢 I/O（fsync files.vfs + build_region）→ `persist_image(N, 旧布局)`；
- compact Finalize（compact.rs:178-196）：`persist_image(N+1, 新布局)` → 之后才换内存索引。

竞态时间线：

```
flush线程:   快照gen=N ──→ fsync files.vfs（慢）──────────→ persist(N, 旧布局)  ✗后到
compact线程:        compact启动 → Move搬数据 → persist(N+1,新布局) → 换内存索引 ✓先完成
```

`compact()` 入口检查 `writers==0 && !compacting`，**不感知 flush 在途**；flush 只在自己的快照瞬间检查 `compacting`。`persist_lock` 只保证两次 persist 不交错，**管不了乱序**。滞后的 flush 把 SB 翻回旧 region（旧 offset），而数据已被 compact 物理搬家 → 崩溃重启后按旧布局读 = 数据错乱。内存中 `layout` 同步被污染（active 指回旧槽），但下次 flush 可自愈——崩溃窗口内的磁盘状态是坏的。

→ 引出 **Q2**。

### P1 — Windows 上 C# 持 mmap 期间 compact 的截短会失败 ✅（适用性已按"单次 compact"约束降级）

Windows 硬规则：文件存在任何活动映射视图时，`SetEndOfFile` 截短报 `ERROR_USER_MAPPED_FILE`（扩大不受限）。compact Finalize 的 `set_len(new_logical + reserve)`（compact.rs:180）是整理的常态缩径操作。

当前 C# 侧：`VfsReader.ReleaseMapping()` 为 private（Vfs.cs:376），`Compact()`（Vfs.cs:487）不释放映射，`RefreshIndex()` 末尾固定调 `EnsureMapping()`（Vfs.cs:345）——只要 `physical > 0` 就会把 files.vfs 映射建好（Vfs.cs:360-372）。

**核对中确认的关键事实**：
- enumerate 列清单本身**纯内存应答**（vfs_enumerate_* → index.rs 内存索引，零文件 I/O），设计上不涉 mmap——炸点是 `RefreshIndex` 的实现副作用顺手建了数据映射，不是"列清单要读数据"；
- 用户约束：**整个程序周期只 compact 一次，且发生在任何 C# 映射建立之前** → 该 P1 在此流程下不会触发，降级处理 → **Q3**（已定向为显式映射生命周期接口，见下）。

### P2 — 三项 ✅

1. **bindings.rs 追加常量是死代码**：`mod bindings` 为私有模块（curlw/mod.rs:12），dlmgr 不可见；追加的 `CURLOPT_DOH_URL` + 12 个 `CURLE_*`（bindings.rs 末尾，注释称"供下载管理器的错误分类使用"）全仓库零引用——dlmgr 实际在 `curlw_backend.rs:18-44` 按值镜像常量（注释已说明镜像是有意为之），且错误分类只映射 28→TIMEOUT、其余一律 NETWORK（worker.rs:190-192），根本没用到这批码。lib.rs:3 有 crate 级 `#![allow(dead_code)]`，连警告都不会出。→ **Q4**。
2. **任务终态后永不移除**：`Shared.tasks`（HashMap，mod.rs:58）只插不删（insert @ mod.rs:229，全库无 remove）；reporter 的 `prev` 表同样只增不减（report.rs:28,76）。`dlmgr_active_count`（mod.rs:308-319）与每 100ms 的 reporter 快照（report.rs:40）都是 O(历史任务总数) → 长会话内存/CPU 持续增长。注意语义耦合：全局回调的 `done_cnt/failed_cnt/bytes_done/bytes_total` 目前是**全历史累计**（靠遍历全部任务算出），任务一删数字就缩水 → 需拆独立累计器。→ **Q5**。
3. **dist1.sh 未打包新文件**：只 `cp Curlw.cs`（dist1.sh:34），`Vfs.cs`/`DownloadManager.cs` 缺失；且写 `NativeBridge.asmdef` 而实际文件名是 `NativeBridgeF.asmdef`（`|| true` 静默吞掉，dist1.sh:35）。T6 验收硬性要求产物包含两个新 C# 文件。→ 直接修，无决策点。

### P3 — 测试覆盖与 ticket 验收的差距 ✅

- T4/T5 要求的本地 mock server HTTP 集成矩阵（4 worker 并发、断连续传断言、兜底矩阵、限速/退避实测）不在本 diff；目前只有 `transition` 纯函数单测 + FileSink 单测。**用户决策：延后，后续在 C# 端补**。
- T6 端到端（建库→删→整理→C# mmap 只读并发下载→逐文件 crc 校验→杀进程崩溃恢复→平台冒烟）未体现。**用户决策：延后，后续在 C# 端补**。注意 T6 第 1/4 步（纯 VFS 侧：writer API 直写 + 杀进程）不依赖 mock server，真要做随时可补。
- 本机验证受阻：D 盘 `cargo test` 报 **os error 483（设备硬件致命错误）**，建议尽快 `chkdsk D: /f /r`；且测试链接依赖 build1.ps1 的 curl 静态库环境（本机未构建，`libcurl.lib/.a` 不存在）。

### 小点（全部核实成立）

| # | 点 | 证据 |
|---|---|---|
| 1 | `sink_register`（无 dtor 版）零调用方；glue 直接用 `register_full` | sink.rs:64；glue.rs:104 |
| 2 | `DlmgrDLL.s_taskCb/s_globalCb` 进程级静态单例，多 manager 并存会顶掉 keep-alive（注释已假设单例） | DownloadManager.cs:185-186,177-178；建议在 dlmgr.h 写死该约定（dlmgr.h 现状未逐行核对） |
| 3 | `dlvfs_sink_create_for_vfs` 的 `size` 参数 native 侧占位忽略，C# 注释 "enforced at prepare time" 易误导（真实 size 来自 `dlmgr_enqueue` → `prepare(task.size)`） | glue.rs:87；DownloadManager.cs:235-236 |
| 4 | `dlmgr_add_doh_url` 行为为 last-wins（`worker.rs:227` 只取 `.last()`）。**微调评审**：Rust doc 已注明"取最后一条"（mod.rs:173），仅 C# 侧未提，"措辞易误解"轻微夸大 | mod.rs:173 |
| 5 | 同目录 `vfs_open` 双实例无防护，vfs.h 未注明宿主单实例约定（vfs.h:30 仅写 compact 回调单线程） | mod.rs(vfs):698-704 |
| 6 | `VfsReader.RefreshIndex` 在 compacting 期间因 BUSY(4) 抛 IOException（`enumerate_query` → `ThrowOnError`）；调用时机应写在注释里 | Vfs.cs:307-308,543-549 |
| 7 | header.vfs 只增不减（persist 只在闲置槽放不下时尾部追加，从不截短）——设计内权衡，后续可考虑与 compact 联动收编 | mod.rs(vfs):182-199 |

### 评审的亮点（确认成立）

VFS 轨道质量高：双缓冲翻转、两指针三规则整理、崩溃安全论证与实现一致、测试矩阵扎实（恢复/残留/三布局/Bad 剔除/BUSY/2 万条目）。`transition` 抽纯函数做状态机单测的思路正确——正是这些测试暴露了 P0。writer"回退重写 + commit 整读重算 crc"干净承接 dlmgr 的 reset 语义（glue `vs_reset` 为 no-op 的依据成立）。

---

## 2. 决策树与各分叉

### Q1 — P0 修复的闸门语义 【待拍板】

**问题**：修 P0 时，要不要保留实现自加的"起始偏移==断点"闸？

- **A（推荐）按规格只保留总长闸**：只修 `content_range()` 解析（取 `"bytes "` 之后、`-` 之前的起始偏移），`transition` 闸门只比 `total == size`，删掉起始偏移比较。改动最小、与设计文档/T5 一致；起始偏移错误由 crc 闸兜底。
- **B 保留起始偏移闸（防御纵深）**：需把"本次请求起始偏移"传入 `transition`（`HttpOk` 增加 `req_off` 字段，或把该校验移入 `perform_attempt`），并补对应用例。快速失败，但多一套状态传递。

### Q2 — flush/compact 竞态修法 【待拍板】

- **A（推荐）gen 水位线**：`persist_image`（唯一翻转入口）内记水位线，只允许 gen 单调递增时翻转，迟到的旧 gen persist 被拒（返回错误，宿主重 flush 即恢复）。集中防御，未来新增 persist 调用点自动受保护；compact 自身 gen=old+1 天然满足单调，不会误伤。
- **B flush 持锁重检**：拆开 `persist_image`，flush 先拿 `persist_lock`、持锁重检 `compacting`/gen（变了返回 `BusyCompacting`/`GenChanged`）再 persist。点防御，改动局部，但只堵 flush 这一个已知口子。

两案中，被拒的 flush 均无害：宿主重试即以新 gen 成功，数据不坏。

### Q3 — 映射生命周期 【已定向，剩一个子项】

**用户决策**：不做库内隐藏副作用，**提供显式接口、由调用方控制映射的建立与重建时机**。落地形态（VfsReader）：

1. `RefreshIndex()` 去掉末尾 `EnsureMapping()` 调用（Vfs.cs:345）→ 回归纯索引操作；
2. `EnsureMapping()` 改 public：幂等语义——映射不存在则创建（首次调用）、物理大小变了则重建、无变化则原地返回；
3. `TryReadRaw()` 保持现状：未映射返回 `false`（不自动建、不抛异常）；
4. XML 文档写明协议：`RefreshIndex(); EnsureMapping(); 再读`；compact done 回调后同样先这两步再读（offset 可能整体偏移）。

**子项（待确认）**：`ReleaseMapping()` 是否一并公开（与 Ensure 成对，一行改动）。当前单次 compact 流程用不上，属接口对称性保险。→ 推荐**公开**。

### Q4 — bindings.rs 死常量 【待拍板】

- **A（推荐）撤掉追加**：curlw 轨道保持已合入原状；镜像的去重重构单独立票（届时 `mod bindings` 改 `pub(crate)` + `curlw_backend` 复用）。
- **B 现在就去重**：`mod bindings` 改 `pub(crate)`，`curlw_backend` 删镜像复用 binding 常量。更 DRY，但触碰已合入的 curlw 代码。

### Q5 — 任务终态后的退役 【待拍板】

- **A（推荐）自动退役 + 独立累计器**：终态在 reporter"变化那一拍"上报一次后，从 `tasks` 与 `prev` 移除；`done_cnt/failed_cnt/bytes_done/bytes_total` 拆成独立 `AtomicU64` 全历史累计器（保住全局回调语义）。副作用：取消已终态任务返回 -1（查无此任务），需写进头文件。
- **B 显式 API**：加 `dlmgr_forget(mgr, id)`，宿主手动退役。负担转移给宿主，忘则泄漏。
- **C 暂不修**：短会话无碍，长会话 O(历史总量) 劣化。

### Q6 — 批次与测试安排 【部分已定，批次待确认】

已定（用户决策）：
- T5 mock server 集成矩阵：**延后**，后续 C# 端补；
- T6 端到端测试：**延后**，后续 C# 端补；
- 合入门槛调整为：P0 修复后**现有单测全绿**（纯函数单测已存在，属自洽门槛，不算新增测试工作；执行需 D 盘修复 + build1.ps1 完整构建链）。

待确认：
- 批次划分：**一批全修**（P0 + Q2 竞态 + dist1.sh + Q4 撤除 + Q5 若选修）vs 只先修 P0。→ 推荐一批。

---

## 3. 决策总表（当前状态）

| 问题 | 选项 | 推荐 | 状态 |
|---|---|---|---|
| Q1 P0 闸门语义 | A=按规格只比总长 / B=保留起始偏移闸 | A | ✅ A 已实施 |
| Q2 竞态修法 | A=gen 水位线 / B=flush 持锁重检 | A | ✅ A 已实施 |
| Q3 映射生命周期 | 显式接口、调用方控制（方案见上） | — | ✅ 已实施（ReleaseMapping 一并公开；顺带修正 VfsRawSpan 过时 doc） |
| Q4 bindings 死常量 | A=撤掉 / B=pub(crate) 去重 | A | ✅ A 已实施（git restore 回 HEAD） |
| Q5 任务退役 | A=自动退役+独立累计器 / B=显式 API / C=不修 | A | ✅ A 已实施 |
| Q6 批次 | 一批全修 / 只修 P0 | 一批 | ✅ 一批已实施（T5/T6 测试延后已定） |

环境待办：`chkdsk D: /f /r`（os error 483 硬件错误）；跑全量单测需 build1.ps1 构建 curl 静态库。

---

## 4. 修复清单草案（按推荐项预览，待决策表确认后执行）

> **2026-09-11 更新：以下 7 项已全部实施**（6 个文件域互不重叠的子代理并行完成），集成验证 `cargo check --tests` 通过（含一处子代理笔误修正：persist_image 中 `hf.metadata()` 漏 `.len()`）。单测执行与 T5/T6 测试延后事项见 §2-Q6 与 §3。

1. **P0**（curlw_backend.rs + worker.rs）：`content_range()` 改为解析起始偏移；Q1=A 时删除起始偏移比较、闸门只比 `total == size`；Q1=B 时给 `HttpOk` 加 `req_off` 并比较 `cr == Some((req_off, size))`。
2. **Q2=A**（vfs/mod.rs）：`persist_image` 增加 gen 水位线（如 `AtomicU64`/Mutex 记 last-flipped gen，仅 gen 递增才执行写槽+翻转，否则返回错误）；确认 compact 路径（gen=old+1）不受影响。
3. **Q3**（Vfs.cs）：`RefreshIndex()` 移除 `EnsureMapping()` 调用；`EnsureMapping()`/`ReleaseMapping()` 改 public；`TryReadRaw` 未映射返回 false 的语义写进文档；XML 注明调用协议（含 compact done 后先 RefreshIndex+EnsureMapping）。
4. **dist1.sh**：补 `Vfs.cs`、`DownloadManager.cs` 的拷贝；asmdef 文件名改为 `NativeBridgeF.asmdef`。
5. **Q4=A**：撤销 bindings.rs 末尾追加的 13 个常量。
6. **Q5=A**（dlmgr/mod.rs + report.rs）：终态上报一拍后移除 `tasks`/`prev` 条目；`done_cnt/failed_cnt/bytes_done/bytes_total` 改独立累计器；头文件注明"已终态任务 cancel 返回 -1"。
7. **验证**：现有单测全绿（磁盘修复后跑完整构建链）；T5/T6 测试延后至 C# 端（另行立票）。

---

# 第二轮评审（2026-09-12）：21 项残留问题核对与修复

- 状态：**已收口并实施**（21 项逐条对码核实，全部属实；Q1–Q7 按推荐项拍板，修复已实施，`cargo check --tests` 通过）
- 背景：外部引擎给出 21 项未修清单（R01–R21）。本轮逐条核实存在性与必要性，证据行号全部对上。

## 1. 核实结论（要点）

| 项 | 结论 | 备注 |
|---|---|---|
| R04 build_region 先改 name_off 再读 | 属实，必修 | 教科书式 use-after-overwrite；现有单测无 "abort→flush→reopen" 路径故未暴露 |
| R10 off 未回传 | 属实，必修 | 只回写 crc 不回写 off；"任何中断=最终失败"仅在无 full_url 时成立（有 full_url 时烧兜底后仍可成功） |
| R06 enumerate 名字错配 | 属实，必修 | 复现甚至无需第二次 commit：query→abort→read（gen 未动即放行） |
| R01 enumerate 越界写 | 属实，必修 | alloc/abort 不 bump gen → gen 闸形同虚设 → 按新鲜 blen 切调用方缓冲 |
| R05 小 region 复用大槽位 | 属实，必修 | 死尾巴卡死顺序扫描，其后追加槽位重开时不可达 → gen 回退/新文件消失 |
| R08 close 不落盘 | 属实，必修 | 违反 Q11（"优雅 close 落盘"） |
| R11 CreateFromFile 共享冲突 | 属实，必修 | 默认重载 FileShare.Read 与 Rust 常驻读写句柄冲突，Windows 必炸 |
| R14 缺 EntryPoint | 属实，必修 | vfs_writer_write_imp 调用即 EntryPointNotFoundException |
| R18 shutdown 丢终态回调 | 属实，必修 | stop 置位后无补拍 → C# 等不到终态 → sink 泄漏 |
| R17 terminal 先 state 后 err | 属实，必修 | Relaxed 双原子无序，Failed+ERR_OK 撕裂上报 |
| R15 / R21 头文件 | 属实，必修 | stddef.h 缺失；shutdown 后句柄已 free 注释失实 |
| R02 case-A 覆写已搬源区 | 属实 | 复现序列：A@0/B@4096删/C@8192/D@12288，D 的 case-A 目标=C 旧源区；崩溃窗口内旧 header 下 C 数据被毁 |
| R03 Finalize 先截断后翻转 | 属实 | 与 §2.3:91 顺序相反；reserve=0 且末尾搬移源区超出新逻辑（或全删光）时崩溃窗口毁数据 |
| §1.1-5 与 §2.3:92 矛盾 | 属实 | §2.3 断言"所有搬迁目标 ≥ 旧逻辑末尾"与 case-A 前移进洞直接冲突；本轮以"Move 前落盘 + 排除已搬源区"裁决 |
| R16 take_task→RUNNING 空窗 | 属实，影响降级 | 空窗内 compact 启动 → 任务 prepare 被 BUSY 拒绝冤死（数据无损），非"整理前置失效" |
| R13 回调单槽保活 | 属实 | 多 manager / 重复注册会 GC 掉存活 delegate → native 回调进已释放 thunk |
| R19 acquire 不查取消 | 属实，不修 | 阻塞有界（≤ want/rate，正常限速毫秒级）；取消由 write_cb 块间检查 |
| R20 reset 返回值忽略 | 属实，不修 | FileSink reset 失败后 pwrite 从 0 全量覆盖（已收 ≤ size 必被覆盖）；glue reset 恒成功 |

## 2. 决策表

| 问题 | 选项 | 状态 |
|---|---|---|
| Q1 批次 | 一批全修 | ✅ 一批已实施 |
| Q2 R02/R03 | A=全修（重排 Finalize + Move 前落盘 + build_plan 排除已搬源区） | ✅ A 已实施 |
| Q3 R01/R06 | A=根因修（alloc/abort bump gen + enumerate 紧凑 blob，无 ABI 变化） | ✅ A 已实施 |
| Q4 R13 | A=照 Curlw per-handle 字典 | ✅ A 已实施 |
| Q5 R16 | 修（take_task 锁内置 RUNNING） | ✅ 已实施 |
| Q6 R19/R20 | A=都不修，注释说明 | ✅ A 已实施 |
| Q7 R08 | A=vfs_close 内 best-effort flush | ✅ A 已实施 |

## 3. 实施摘要与行为变化

- **R02 副作用（重要）**：排除已搬源区后，case-A 被排挤的条目搬到末尾、留下本轮不可回收的洞 → compact_run 改为多轮 Scan→Move→Finalize（上限 8 轮，`excluded_any || !bad` 触发下一轮），最终布局与既有测试期望一致（compact_three_layouts / compact_move_beyond_end / compact_marks_bad_crc 全部按原断言通过）；进度 percent 逐轮回摆属正常。§1.1-5/§2.3 矛盾就此裁决：崩溃安全承诺升级为"Move 前落盘 + 目标不覆盖任何源区 + header 只在 Finalize 翻转一次"。
- **R01 副作用**：generation 现在覆盖 alloc/abort（Q12 语义补全，C# 可感知 Downloading 条目出现/消失）；roundtrip/scale_20k 的 gen 断言已同步（每文件 alloc+commit = 2 拍）。
- **R05 副作用**：persist 仅等长复用闲置槽位，其余一律追加 → header.vfs 增长略快（原"只增不减"权衡内）。
- **R18**：reporter 循环体抽为 report_tick，stop 置位后补拍一拍；reporter_stop 升级 Release/Acquire。
- **R17**：全部终态写入改为先 err 后 state（Release），reporter 侧 state Acquire。
- **R11**：EnsureMapping 改用显式 `FileStream(FileMode.Open, FileAccess.Read, FileShare.ReadWrite)` + `CreateFromFile(FileStream,...)`（MMF 持有流）。
- **R13**：DlmgrDLL 回调保活改 per-handle `Dictionary<IntPtr, List<Delegate>>`；dlmgr_shutdown 变 C# 包装（imp + 清理该 handle 条目）；null 清注册时保留旧 delegate（防在途回调踩空）。
- 新增单测：build_region 旧偏移（R04）、persist 不复用大槽（R05）、enumerate gen 覆盖 alloc/abort + 紧凑 blob（R01/R06）、close 落盘（R08）、build_plan 排挤（R02）、reporter 最终补拍（R18）。
- 验证：`cargo check --tests` 通过。单测执行仍受限于构建链（需 build1.ps1 产出 curl 静态库；本机未构建），与上轮环境待办一致。

---

# 第三轮评审（2026-09-12）：修复核实 + 残留 5 项 + 单测首次执行

- 状态：**已收口并实施**（Q1–Q4 全部按推荐项拍板）
- 背景：对工作区当前代码逐条核实前两轮修复是否落位，并首次跑通全量单测。

## 1. 核实结论

前两轮声称的全部修复（P0、R01–R18、R21、R22、Q1–Q6 决策）**逐条对码属实**，无虚报；
`cargo check --tests` 通过。发现 5 个前两轮未覆盖的残留点（见 §2-Q2）。

## 2. 决策表

| 问题 | 选项 | 状态 |
|---|---|---|
| Q1 合入门槛 | 全量单测真实执行且全绿（T5/T6 维持延后） | ✅ 达成：42/42 绿 |
| Q2 残留 5 项 | 一批全修（见下） | ✅ 已实施 |
| Q3 提交策略 | 按轨拆 commit（vfs / dlmgr / docs / dist1），留在 rust 分支不碰 main | ✅ 已实施 |
| Q4 延后票 | tickets README 加状态列；T5/T6 标注延后与关闭条件 | ✅ 已实施 |

Q2 五项：① VfsReader.SetCommitCallback 替换/清除的 GC 窗口（照 DlmgrDLL 的
keep-alive List 方式，R13 的 VFS 侧补齐）；② design.md §1.1-5/§2.3 同步为已实施
的整理规则（排除已搬源区 + 多轮收敛 + 先翻转后截断），Q12 语义补全 alloc/abort；
③ vfs.h 补"同目录单实例"宿主约定 + Vfs.cs RefreshIndex 补整理期 BUSY 说明；
④ worker.rs 三处终态/Done 写入统一为"先 err 后 state（Release）"纪律（原 Done
路径 state 先行 + Relaxed，实际无害但与 R17 注释自相矛盾）；⑤ `acquire_blocks_until_tokens`
测试按一轮评审裁决改为循环累计配额后验耗时（部分配额是生产契约，原断言"一次拿满"必挂）。

## 3. 构建链与单测执行（上轮环境待办收口）

- 根因：`install_win32_x64` 只有 boringssl/nghttp3/zlib（DLL 口味），ngtcp2 的 cmake
  中断、nghttp2/curl 未拉取 → `cargo test` 链接 LNK1181（curl.lib 不存在）。
- 修复：`NATIVEBRIDGE=1 build.ps1 win32 x64 zlib,curl` 重建静态 zs.lib/libcurl.lib
  （curl 静态化与 zs.lib 均以 NATIVEBRIDGE=1 为开关；首次 curl 构建因未带该开关产出
  DLL 导入库，删除安装目录后带开关重建成功）。
- 结果：`cargo test` **42 passed / 0 failed**（首轮 41/1，唯一失败即 §2-Q2⑤ 的已知错
  测试）。P0 修复后此前"错错相抵"的两个 range 测试现为真通过。
- NB_* 环境变量口径与 build1.ps1 一致（curl/ssl/crypto/nghttp2/nghttp3/ngtcp2×2/zs + NB_LIB_DIRS）。
