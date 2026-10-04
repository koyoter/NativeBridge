//! reporter 线程（Q16）：10Hz 快照，同一轮先逐任务 `cb(task_id,state,done,total,bps,err)`
//! 再全局 `cb(active,done_cnt,failed_cnt,bytes_done,bytes_total,bps)`；
//! 取消/失败同样上报终态。回调在锁外触发，允许重入只读 FFI（如 active_count）。

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::task::{
    TASK_CANCELED, TASK_DONE, TASK_FAILED, TASK_RUNNING, TASK_VERIFYING, TaskShared,
};
use super::Shared;

const REPORT_INTERVAL_MS: u64 = 100; // 10Hz 节流

pub(crate) fn spawn(shared: Arc<Shared>) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("dlmgr-reporter".into())
        .spawn(move || reporter_loop(shared))
        .expect("spawn reporter")
}

fn reporter_loop(sh: Arc<Shared>) {
    // 每任务上次快照 (state, bytes_done)：状态或进度有变化才发逐任务回调，
    // 终态（Done/Failed/Canceled）在变化那一拍发出后不再重复刷屏。
    let mut prev: HashMap<u64, (i32, u64)> = HashMap::new();
    let mut prev_t = Instant::now();

    while !sh.reporter_stop.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(REPORT_INTERVAL_MS));
        if sh.reporter_stop.load(Ordering::Acquire) {
            break;
        }
        prev_t = report_tick(&sh, &mut prev, prev_t);
    }
    // 最终补拍：shutdown 在 join 完 worker 后才置 stop，最后一批终态
    // （cancel_all 标记的 Canceled / 收尾的 Done/Failed）必须上报，
    // 否则 C# 等不到终态就不会 dlmgr_sink_release。
    report_tick(&sh, &mut prev, prev_t);
}

/// 一拍：快照 → 逐任务回调（锁外）→ 退役 → 全局回调。返回本拍时间戳。
fn report_tick(sh: &Shared, prev: &mut HashMap<u64, (i32, u64)>, prev_t: Instant) -> Instant {
    let dt = prev_t.elapsed().as_secs_f64().max(1e-3);
    let now = Instant::now();

    // 快照进栈，回调全部在锁外触发
    let snap: Vec<Arc<TaskShared>> = sh.tasks.lock().unwrap().values().cloned().collect();
    let task_cb = *sh.task_cb.lock().unwrap();
    let global_cb = *sh.global_cb.lock().unwrap();

    let mut active = 0u32;
    let mut bytes_done = 0u64;
    let mut bytes_total = 0u64;
    let mut delta = 0u64;
    let mut retired = Vec::new();

    for t in &snap {
        // Acquire 配对 worker 的 state Release：state==终态可见时 err 必已可见
        let state = t.state.load(Ordering::Acquire);
        let done = t.bytes_done.load(Ordering::Relaxed);
        let prev_done = prev.get(&t.id).map(|p| p.1).unwrap_or(0);
        delta += done.saturating_sub(prev_done);
        let changed = prev.get(&t.id).map_or(true, |p| p.0 != state || p.1 != done);
        // 退役规则：终态任务（Done/Failed/Canceled）首次上报即退役——终态计数
        // 与字节量并入全历史累计器（Canceled 不计成败），本拍不再计入 live 求和
        // （其贡献已由累计器承接，避免同拍双重计数），拍末从 tasks/prev 移除。
        let retire = state == TASK_DONE || state == TASK_FAILED || state == TASK_CANCELED;
        if retire {
            if state == TASK_DONE {
                sh.done_total.fetch_add(1, Ordering::Relaxed);
            } else if state == TASK_FAILED {
                sh.failed_total.fetch_add(1, Ordering::Relaxed);
            }
            sh.bytes_done_total.fetch_add(done, Ordering::Relaxed);
            sh.bytes_total_total.fetch_add(t.size, Ordering::Relaxed);
            retired.push(t.id);
        } else {
            if state == TASK_RUNNING || state == TASK_VERIFYING {
                active += 1;
            }
            bytes_done += done;
            bytes_total += t.size;
        }
        if changed {
            if let Some((cb, user)) = task_cb {
                let bps = (done.saturating_sub(prev_done) as f64 / dt) as u64;
                let err = t.err.load(Ordering::Relaxed);
                unsafe { cb(user.0, t.id, state, done, t.size, bps, err) };
            }
        }
        if !retire {
            prev.insert(t.id, (state, done));
        }
    }

    // 拍末统一退役：从 tasks 与 prev 移除（回调已先发，快照仍持有克隆 Arc）。
    if !retired.is_empty() {
        let mut tasks = sh.tasks.lock().unwrap();
        for id in &retired {
            tasks.remove(id);
            prev.remove(id);
        }
    }

    if let Some((cb, user)) = global_cb {
        let done_cnt = sh.done_total.load(Ordering::Relaxed) as u32;
        let failed_cnt = sh.failed_total.load(Ordering::Relaxed) as u32;
        let bytes_done = sh.bytes_done_total.load(Ordering::Relaxed) + bytes_done;
        let bytes_total = sh.bytes_total_total.load(Ordering::Relaxed) + bytes_total;
        let gbps = (delta as f64 / dt) as u64;
        unsafe { cb(user.0, active, done_cnt, failed_cnt, bytes_done, bytes_total, gbps) };
    }
    now
}

#[cfg(test)]
mod tests {
    use super::*;
    use libc::c_void;
    use std::sync::atomic::AtomicBool;

    use super::super::SendPtr;
    use super::super::task::ERR_CANCELED;

    static REPORTED: AtomicBool = AtomicBool::new(false);

    unsafe extern "C" fn on_task(
        _user: *mut c_void, _id: u64, state: i32, _done: u64, _total: u64, _bps: u64, _err: i32,
    ) {
        if state == TASK_CANCELED {
            REPORTED.store(true, Ordering::SeqCst);
        }
    }

    /// R18：stop 置位后 reporter 必须补拍一拍——shutdown 在 join 完 worker 后才
    /// 置 stop，最后一批终态（cancel_all 的 Canceled 等）不补拍就永远不上报。
    #[test]
    fn final_pass_reports_terminal_after_stop() {
        let sh = Arc::new(Shared::new(1, 0, 0, std::ptr::null_mut()));
        let t = Arc::new(TaskShared::new(7, "n".into(), "r".into(), String::new(), 1, 0, 0, 0));
        t.err.store(ERR_CANCELED, Ordering::Relaxed); // 先 err 后 state
        t.state.store(TASK_CANCELED, Ordering::Release);
        sh.tasks.lock().unwrap().insert(7, t);
        *sh.task_cb.lock().unwrap() = Some((on_task as super::super::DlmgrTaskCb, SendPtr(std::ptr::null_mut())));
        sh.reporter_stop.store(true, Ordering::Release);

        // stop 已置位：while 不进循环，只剩最终补拍一拍
        reporter_loop(sh);
        assert!(REPORTED.load(Ordering::SeqCst));
    }
}
