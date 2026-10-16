use super::*;

#[test]
fn unlimited_returns_full_want() {
    let b = TokenBucket::new(0);
    assert_eq!(b.try_take(1 << 20), 1 << 20);
    let l = SpeedLimiter::new(0);
    assert_eq!(l.acquire(None, 4096), 4096);
}

#[test]
fn burst_capped_then_refills_at_rate() {
    let b = TokenBucket::new(1000);
    assert_eq!(b.try_take(2000), 1000); // 突发 = 1s 配额
    assert_eq!(b.try_take(2000), 0); // 余量为空
    std::thread::sleep(Duration::from_millis(600));
    let got = b.try_take(2000);
    assert!((300..=900).contains(&got), "refilled {got}");
}

#[test]
fn double_bucket_takes_min() {
    let l = SpeedLimiter::new(1000);
    let task = TokenBucket::new(400);
    assert_eq!(l.acquire(Some(&task), 2000), 400); // min(1000, 400)
}

#[test]
fn acquire_blocks_until_tokens() {
    let l = SpeedLimiter::new(0); // 全局不限
    let task = TokenBucket::new(1000);
    assert_eq!(l.acquire(Some(&task), 1000), 1000); // 突发
    let t0 = Instant::now();
    // 部分配额是生产契约（返回实际拿到的 ≥1 字节，write_cb 循环消费）：
    // 循环累计到 500 后再验耗时，不得断言"一次拿满"。
    let mut got = 0usize;
    while got < 500 {
        got += l.acquire(Some(&task), 500 - got);
    }
    assert_eq!(got, 500);
    assert!(t0.elapsed() >= Duration::from_millis(400), "rate limit did not block");
}

#[test]
fn set_rate_dynamic() {
    let b = TokenBucket::new(100);
    b.set_rate(0); // 0 = 立即恢复不限
    assert_eq!(b.try_take(1 << 20), 1 << 20);
    let l = SpeedLimiter::new(1000);
    l.set_global(0);
    let task = TokenBucket::new(0);
    assert_eq!(l.acquire(Some(&task), 1 << 20), 1 << 20);
}
