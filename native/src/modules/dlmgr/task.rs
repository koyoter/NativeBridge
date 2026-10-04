//! 任务结构 + 优先级队列（priority 大者先取、同级 FIFO、不抢占）+ 状态/错误码常量。
//! CRC-32 在 modules/util.rs（与 vfs 共用）。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64};

pub(crate) use crate::modules::util::update as crc32_update;

// --- 任务状态（FFI 契约钉死：Pending=0 Running=1 Verifying=2 Done=3 Failed=4 Canceled=5）---
pub(crate) const TASK_PENDING: i32 = 0;
pub(crate) const TASK_RUNNING: i32 = 1;
pub(crate) const TASK_VERIFYING: i32 = 2;
pub(crate) const TASK_DONE: i32 = 3;
pub(crate) const TASK_FAILED: i32 = 4;
pub(crate) const TASK_CANCELED: i32 = 5;

// --- 任务错误码（OK=0 UNSUPPORTED_RANGE=1 SIZE_MISMATCH=2 CRC_MISMATCH=3 NETWORK=4 TIMEOUT=5 CANCELED=6）---
pub(crate) const ERR_OK: i32 = 0;
pub(crate) const ERR_UNSUPPORTED_RANGE: i32 = 1;
pub(crate) const ERR_SIZE_MISMATCH: i32 = 2;
pub(crate) const ERR_CRC_MISMATCH: i32 = 3;
pub(crate) const ERR_NETWORK: i32 = 4;
pub(crate) const ERR_TIMEOUT: i32 = 5;
pub(crate) const ERR_CANCELED: i32 = 6;

/// 任务共享态：入队后 worker / reporter / FFI 三方仅做原子读写，无逐字段锁。
pub(crate) struct TaskShared {
    pub(crate) id: u64,
    pub(crate) name: String,
    pub(crate) range_url: String,
    pub(crate) full_url: String, // 空 = 无兜底可用
    pub(crate) size: u64,
    pub(crate) crc: u32,
    pub(crate) task_bps: u64, // 0 = 不限
    pub(crate) sink: u64,
    pub(crate) state: AtomicI32,
    pub(crate) err: AtomicI32,
    pub(crate) bytes_done: AtomicU64, // 逻辑进度（与写偏移同步，reset 时归零）
    pub(crate) cancel: AtomicBool,
}

impl TaskShared {
    pub(crate) fn new(
        id: u64,
        name: String,
        range_url: String,
        full_url: String,
        size: u64,
        crc: u32,
        task_bps: u64,
        sink: u64,
    ) -> TaskShared {
        TaskShared {
            id,
            name,
            range_url,
            full_url,
            size,
            crc,
            task_bps,
            sink,
            state: AtomicI32::new(TASK_PENDING),
            err: AtomicI32::new(ERR_OK),
            bytes_done: AtomicU64::new(0),
            cancel: AtomicBool::new(false),
        }
    }
}

/// 优先级队列元素：priority 大者先取；同级按 seq（单调递增）先进先出；不抢占。
pub(crate) struct HeapTask {
    pub(crate) priority: i32,
    pub(crate) seq: u64,
    pub(crate) task: Arc<TaskShared>,
}

impl PartialEq for HeapTask {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.seq == other.seq
    }
}
impl Eq for HeapTask {}
impl PartialOrd for HeapTask {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
// BinaryHeap 是大顶堆：优先级大者视为“大”；同级时 seq 小者视为“大”（FIFO）。
impl Ord for HeapTask {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.priority
            .cmp(&other.priority)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BinaryHeap;

    fn t(seq: u64) -> Arc<TaskShared> {
        Arc::new(TaskShared::new(
            seq,
            format!("f{seq}"),
            format!("r{seq}"),
            String::new(),
            1,
            0,
            0,
            0,
        ))
    }

    fn push(h: &mut BinaryHeap<HeapTask>, priority: i32, seq: u64) {
        h.push(HeapTask { priority, seq, task: t(seq) });
    }

    fn drain(h: &mut BinaryHeap<HeapTask>) -> Vec<u64> {
        std::iter::from_fn(|| h.pop()).map(|x| x.seq).collect()
    }

    #[test]
    fn higher_priority_first_then_fifo_within_level() {
        let mut h = BinaryHeap::new();
        push(&mut h, 0, 0);
        push(&mut h, 5, 1);
        push(&mut h, 5, 2);
        push(&mut h, 1, 3);
        push(&mut h, 5, 4);
        assert_eq!(drain(&mut h), vec![1, 2, 4, 3, 0]);
    }

    #[test]
    fn same_priority_is_strict_fifo() {
        let mut h = BinaryHeap::new();
        for seq in 0..8u64 {
            push(&mut h, 7, seq);
        }
        assert_eq!(drain(&mut h), (0..8).collect::<Vec<_>>());
    }
}
