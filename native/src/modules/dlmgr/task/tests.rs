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
