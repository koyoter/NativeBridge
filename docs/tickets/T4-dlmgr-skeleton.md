# T4: dlmgr 骨架（配置/队列/worker 池/FileSink/curlw 后端）

Blocked by: —
Blocks: T5

## 目标

按设计文档 §3.1/§3.4 搭起 `modules/dlmgr` 的运行骨架：能配置、能排队、能用 curlw 真实下载到普通文件。不含兜底链/限速/回调细节（T5）。

## 范围

- `dlmgr/mod.rs`：管理器生命周期（create/add_doh_url/start/shutdown）、任务注册表、句柄安全关闭（cancel all + join）。
- `dlmgr/task.rs`：任务结构 + 优先级队列（priority 大者先取、同级 FIFO、不抢占）+ 状态机骨架（Pending/Running/Done/Failed/Canceled）。
- `dlmgr/worker.rs`：N 个 std worker 线程，condvar 取任务；每 worker 一个 curl easy 句柄 + 全局 share（DNS 锁），`CURLOPT_DOH_URL` 注入；直调 `modules/curlw` Rust 绑定（不走 C ABI）。
- `dlmgr/curlw_backend.rs`：easy 句柄封装（setopt URL/DOH/write 回调/取响应码与 Content-Range）。
- `dlmgr/sink.rs`：sink vtable + `FileSink`（每任务独立句柄，pwrite 落普通文件）+ 注册表（u64 句柄）。
- 基础错误路径：任务失败即 Failed（T5 再细化重试/兜底）。

## 验收标准

- 单测：优先级队列顺序（插队生效、同级先进先出）；shutdown 无泄漏无死锁（任务运行中 shutdown 可退出）。
- 集成测试（可用本地 http server 或 file:// 之外的本地回环）：并发 4 worker 下载多个文件到 FileSink，内容与源一致；单任务失败进入 Failed。
- worker_count=1 时严格串行；DOH URL 配置后请求正常发出（可观察 DNS 行为或至少 setopt 不报错）。
- 零依赖不变；panic=abort 下无 unwind 跨 FFI。
