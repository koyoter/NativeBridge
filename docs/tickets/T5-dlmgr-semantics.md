# T5: dlmgr 完整语义（兜底链/续传/重试/限速/回调）

Blocked by: T4
Blocks: T6

> **状态（2026-09-12）**：实现完成，纯函数单测通过（transition 状态机/限速/队列/sink）。
> 下方"验收标准"中的 **mock server 集成矩阵延后至 C# 端**补齐（2026-09-11 评审决策），
> 本票在那一部分完成前不得关闭。

## 目标

按设计文档 §3.2/§3.3 补齐执行链全部语义：双 URL 三道闸兜底、会话内 Range 续传、重试退避、双桶限速、reporter 进度回调。

## 范围

- 执行链（每任务单连接）：
  - `range_url` 首发即带 `Range: bytes={off}-`；闸门 = 非 206 / Content-Range 总长 ≠ size / 流式 crc32 ≠ 预期 → 判"源不可信"。
  - 兜底：每任务至多一次切 `full_url` 从 0 重下（sink.reset），不消耗重试；`full_url` 上 Content-Length + crc 双闸，失败消耗 retry（1s/2s/4s 指数退避）；用尽 → Failed。
  - 会话内断线：重连 `Range: bytes=off-`，206 续写（crc 状态延续）；200 → 判源不可信走兜底；416 → 走兜底。
- `dlmgr/rate.rs`：全局 + 单任务双令牌桶，实际速率取 min，0 = 不限，`set_global_speed` 动态生效；写 sink 前取 token。
- `dlmgr/report.rs`：reporter 线程 10Hz，快照后先逐任务 `cb(task_id,state,done,total,bps,err)` 再全局 `cb(...)`；取消/失败也上报终态。
- 错误码定稿：`UNSUPPORTED_RANGE / SIZE_MISMATCH / CRC_MISMATCH / NETWORK / TIMEOUT / CANCELED`。
- `dlmgr_pause/resume/cancel/cancel_all/active_count` 完整实现。

## 验收标准

- 本地 mock server 单测矩阵：
  - range 源返回 200 → 自动切 full_url 成功，任务 Done 且 `UNSUPPORTED_RANGE` 未出现（兜底不算失败）。
  - range 源 crc 错 / 总长错 → 切 full_url；full_url 也错 → Failed 且错误码正确。
  - 下载中途断连 → 恢复后 206 续传（server 端断言收到 `Range: bytes=off-`），最终 crc 通过，数据无重复无缺口。
  - full_url 连续失败 3 次 → Failed；重试间隔实测 ≈ 1s/2s/4s。
- 限速：全局 1MB/s 单任务不限 → 实测速率 ≈ 1MB/s；单任务 512KB/s 全局 1MB/s → ≈512KB/s；`set_global_speed(0)` 立即恢复全速。
- 回调：10Hz 节流下频率不超 ~11 次/秒；先任务后全局顺序稳定；cancel 后收到 Canceled 终态。
- 下载总字节与进度累计一致（断连续传不重复计数）。
