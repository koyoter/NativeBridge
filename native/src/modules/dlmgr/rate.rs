//! 双令牌桶限速（Q8/Q10）：全局 + 单任务桶，实际速率取 min；0 = 不限，动态生效。

use std::sync::Mutex;
use std::time::{Duration, Instant};

struct BucketState {
    rate_bps: u64, // 0 = 不限
    tokens: f64,
    last: Instant,
}

/// 单桶：容量 = 1 秒配额（允许等量突发），按流逝时间线性回填。
pub(crate) struct TokenBucket {
    st: Mutex<BucketState>,
}

impl TokenBucket {
    pub(crate) fn new(rate_bps: u64) -> TokenBucket {
        TokenBucket {
            st: Mutex::new(BucketState {
                rate_bps,
                tokens: rate_bps as f64,
                last: Instant::now(),
            }),
        }
    }

    /// 动态改速率（0 = 不限）；存量 token 按新容量截断。
    pub(crate) fn set_rate(&self, rate_bps: u64) {
        let mut st = self.st.lock().unwrap();
        Self::refill(&mut st);
        st.rate_bps = rate_bps;
        if rate_bps > 0 {
            st.tokens = st.tokens.min(rate_bps as f64);
        }
    }

    fn refill(st: &mut BucketState) {
        let now = Instant::now();
        let dt = now.duration_since(st.last).as_secs_f64();
        st.last = now;
        if st.rate_bps > 0 && dt > 0.0 {
            st.tokens = (st.tokens + st.rate_bps as f64 * dt).min(st.rate_bps as f64);
        }
    }

    /// 非阻塞取 token；返回实际拿到字节数（可为 0；不限速时 == want）。
    pub(crate) fn try_take(&self, want: usize) -> usize {
        if want == 0 {
            return 0;
        }
        let mut st = self.st.lock().unwrap();
        Self::refill(&mut st);
        if st.rate_bps == 0 {
            return want;
        }
        let n = st.tokens.min(want as f64).floor();
        if n < 1.0 {
            return 0;
        }
        let n = n as usize;
        st.tokens -= n as f64;
        n
    }

    /// 双桶对齐时归还多取的配额。
    pub(crate) fn give_back(&self, n: usize) {
        if n == 0 {
            return;
        }
        let mut st = self.st.lock().unwrap();
        Self::refill(&mut st);
        if st.rate_bps > 0 {
            st.tokens = (st.tokens + n as f64).min(st.rate_bps as f64);
        }
    }
}

/// 全局桶 + 可选单任务桶；acquire 阻塞到拿到 ≥1 字节为止（curl 写回调内调用）。
pub(crate) struct SpeedLimiter {
    global: TokenBucket,
}

impl SpeedLimiter {
    pub(crate) fn new(global_bps: u64) -> SpeedLimiter {
        SpeedLimiter { global: TokenBucket::new(global_bps) }
    }

    pub(crate) fn set_global(&self, bps: u64) {
        self.global.set_rate(bps);
    }

    /// 取 min(全局, 单任务)；先取全局、按任务桶余量归还差值，保证双桶语义。
    /// 阻塞有界：单次至多等 want/rate（curl 写回调块 ≤16KB，正常限速下毫秒级）。
    /// 取消旗标不进桶锁——由调用方 write_cb 在块间检查，这里不轮询。
    pub(crate) fn acquire(&self, task: Option<&TokenBucket>, want: usize) -> usize {
        if want == 0 {
            return 0;
        }
        loop {
            let g = self.global.try_take(want);
            let got = match task {
                Some(tb) => {
                    let t = tb.try_take(g);
                    if t < g {
                        self.global.give_back(g - t);
                    }
                    t
                }
                None => g,
            };
            if got > 0 {
                return got;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(test)]
mod tests;
