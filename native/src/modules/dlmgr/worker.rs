//! worker 线程池 + §3.2 执行链：双 URL 三道闸兜底 / 会话内 Range 续传 / 重试退避
//! （1s/2s/4s）/ 双桶限速。每 worker 一条 curl easy 阻塞跑单任务（Q5/Q7）。

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use libc::{c_char, c_void, size_t};

use super::curlw_backend::{
    Easy, HeaderList, WriteCb, CURLE_FAILED_INIT, CURLINFO_RESPONSE_CODE,
};
use super::curlw_backend::CURLE_OPERATION_TIMEDOUT;
use crate::modules::curlw::curlw_easy_getinfo_long;
use super::rate::{SpeedLimiter, TokenBucket};
use super::sink::{sink_lookup, SinkVTable};
use super::task::{
    crc32_update, TaskShared, ERR_CANCELED, ERR_CRC_MISMATCH, ERR_NETWORK, ERR_OK,
    ERR_SIZE_MISMATCH, ERR_TIMEOUT, ERR_UNSUPPORTED_RANGE, TASK_CANCELED, TASK_DONE, TASK_FAILED,
    TASK_RUNNING, TASK_VERIFYING,
};
use super::Shared;
use super::SendPtr;

// --- curl 写回调上下文：限速 / 取消 / 闸门全部在这里做（活于单次 perform 期间）---

struct WriteCtx<'a> {
    curl: *mut c_void,
    sink: &'a SinkVTable,
    limiter: &'a SpeedLimiter,
    task_bucket: Option<&'a TokenBucket>,
    task: &'a TaskShared,
    total: u64, // 任务 size（写入越界 = 源不可信）
    range_mode: bool, // true = range_url 阶段，必须 206
    off: &'a AtomicU64,   // 下一个写入偏移（= 已收字节）
    crc: &'a AtomicU32,   // 流式 crc32 状态（断线重连延续）
    bad: &'a AtomicI32,   // 0 无 / 1 非 206 → UNSUPPORTED_RANGE / 2 越界 → SIZE_MISMATCH
    io_err: &'a AtomicBool,
}

/// curl 写回调：写 sink 前从双桶取 token；返回 0 触发中止（cancel / 闸门失败 / sink 错误）。
unsafe extern "C" fn write_cb(ptr: *mut c_char, size: size_t, nmemb: size_t, ud: *mut c_void) -> size_t {
    let ctx = &*(ud as *const WriteCtx);
    let len = size.saturating_mul(nmemb);
    if len == 0 {
        return 0;
    }
    if ctx.task.cancel.load(Ordering::Relaxed)
        || ctx.bad.load(Ordering::Relaxed) != 0
        || ctx.io_err.load(Ordering::Relaxed)
    {
        // 取消/闸门失败/写错误：返回 0 让 curl 以 ABORTED_BY_CALLBACK/WRITE_ERROR 中止
        return 0;
    }
    if ctx.range_mode {
        // 三道闸之一：range 源必须 206。响应码在首包前已定，getinfo 回调内可重入。
        let mut code = 0i64;
        curlw_easy_getinfo_long(ctx.curl, CURLINFO_RESPONSE_CODE, &mut code);
        if code != 206 {
            ctx.bad.store(1, Ordering::Relaxed);
            return 0;
        }
    }
    let mut p = ptr as *const u8;
    let mut left = len;
    while left > 0 {
        if ctx.task.cancel.load(Ordering::Relaxed) {
            return 0;
        }
        // 写 sink 前取 token（双桶取 min；0=不限）——限速即在此生效
        let allow = ctx.limiter.acquire(ctx.task_bucket, left);
        if allow == 0 || allow > left {
            return 0;
        }
        let off = ctx.off.load(Ordering::Relaxed);
        if off.saturating_add(allow as u64) > ctx.total {
            ctx.bad.store(2, Ordering::Relaxed); // 超出任务 size → 源不可信
            return 0;
        }
        if (ctx.sink.write)(ctx.sink.ctx, off, p, allow) != 0 {
            ctx.io_err.store(true, Ordering::Relaxed);
            return 0;
        }
        let chunk = std::slice::from_raw_parts(p, allow);
        let c = crc32_update(ctx.crc.load(Ordering::Relaxed), chunk);
        ctx.crc.store(c, Ordering::Relaxed);
        ctx.off.store(off + allow as u64, Ordering::Relaxed);
        ctx.task.bytes_done.fetch_add(allow as u64, Ordering::Relaxed);
        p = p.add(allow);
        left -= allow;
    }
    len
}

// --- 单次连接尝试的结果 + 跨尝试运行态 + 纯函数状态转移（单测直接覆盖兜底判定）---

/// 一次 perform 的汇总结果。
pub(crate) enum AttemptOutcome {
    Canceled,                    // 任务被取消（旗标，先于一切判定）
    SinkError,                   // sink 写失败（不重试）
    GateFail(i32),               // 回调内已判定的闸门失败：非 206 / 越界
    HttpOk {                     // curl 干净返回
        resp: i64,
        cr: Option<(u64, u64)>,  // Content-Range (起始偏移, 总长)
        clen: i64,               // Content-Length（未知 = -1）
        complete: bool,          // 收满任务 size
        crc_ok: bool,
    },
    NetErr(i32),                 // curl 网络错误码
}

/// 跨尝试运行态。
pub(crate) struct RunState {
    pub(crate) range_phase: bool, // true=range_url 阶段 / false=full_url 阶段
    pub(crate) fallback_used: bool,
    pub(crate) retries: u32,      // 已耗重试次数（共享预算）
    pub(crate) last_err: i32,
}

/// 下一拍动作。
pub(crate) enum Decision {
    Done,             // → Verifying → sink.finish → Done
    Resume,           // 原阶段带断点重试（range：off/crc 延续，耗 retry，先退避）
    RestartFromZero,  // 原阶段从头重下（off>0 时 sink.reset，耗 retry，先退避）
    Fallback(i32),    // 切 full_url 从 0 重下（不耗 retry）
    Fail(i32),        // 终态（ERR_CANCELED → Canceled，其余 → Failed）
}

/// §3.2 状态转移（纯函数）：三道闸判源不可信 → 至多一次兜底；full_url 失败耗 retry。
pub(crate) fn transition(st: &mut RunState, out: &AttemptOutcome, size: u64, retry_count: u32) -> Decision {
    match out {
        AttemptOutcome::Canceled => Decision::Fail(ERR_CANCELED),
        AttemptOutcome::SinkError => Decision::Fail(ERR_NETWORK),
        AttemptOutcome::GateFail(kind) => {
            st.last_err = *kind;
            if st.range_phase && !st.fallback_used {
                Decision::Fallback(*kind) // 源不可信 → 一次性兜底，不消耗重试
            } else if st.retries < retry_count {
                st.retries += 1;
                Decision::RestartFromZero
            } else {
                Decision::Fail(*kind)
            }
        }
        AttemptOutcome::HttpOk { resp, cr, clen, complete, crc_ok } => {
            if st.range_phase {
                if *resp != 206 {
                    // 非 206（200 / 416 / 4xx…）→ 源不支持 range → 兜底
                    st.last_err = ERR_UNSUPPORTED_RANGE;
                    return Decision::Fallback(ERR_UNSUPPORTED_RANGE);
                }
                // Content-Range 闸：总长必须等于任务 size（起始偏移不设闸——错源由 crc 闸兜底）
                if cr.map(|(_, total)| total) != Some(size) {
                    st.last_err = ERR_SIZE_MISMATCH;
                    return Decision::Fallback(ERR_SIZE_MISMATCH);
                }
                if *complete {
                    if *crc_ok {
                        return Decision::Done;
                    }
                    st.last_err = ERR_CRC_MISMATCH;
                    return Decision::Fallback(ERR_CRC_MISMATCH); // 流式 crc ≠ 预期 → 源不可信
                }
                // 响应干净结束但字节不足 → 视同断线，走断点重试
                net_retry(st, ERR_NETWORK, retry_count)
            } else {
                // full_url：Content-Length 闸 + crc 闸，任何失败都消耗 retry
                if (200..300).contains(resp) {
                    if *clen >= 0 && (*clen as u64) != size {
                        st.last_err = ERR_SIZE_MISMATCH;
                    } else if *complete && *crc_ok {
                        return Decision::Done;
                    } else if *complete {
                        st.last_err = ERR_CRC_MISMATCH;
                    } else {
                        st.last_err = ERR_NETWORK; // 响应干净但字节不足
                    }
                } else {
                    st.last_err = ERR_NETWORK;
                }
                if st.retries < retry_count {
                    st.retries += 1;
                    Decision::RestartFromZero
                } else {
                    Decision::Fail(st.last_err)
                }
            }
        }
        AttemptOutcome::NetErr(code) => {
            let err = if *code == CURLE_OPERATION_TIMEDOUT { ERR_TIMEOUT } else { ERR_NETWORK };
            net_retry(st, err, retry_count)
        }
    }
}

/// 网络类失败：同预算内退避重试；range 阶段用尽且未兜底过 → 兜底；full 阶段用尽 → Failed。
fn net_retry(st: &mut RunState, err: i32, retry_count: u32) -> Decision {
    st.last_err = err;
    if st.retries < retry_count {
        st.retries += 1;
        if st.range_phase { Decision::Resume } else { Decision::RestartFromZero }
    } else if st.range_phase && !st.fallback_used {
        Decision::Fallback(err)
    } else {
        Decision::Fail(err)
    }
}

/// 重试退避 1s/2s/4s（第 3 次起封顶 4s）；期间响应取消。false = 已取消。
fn backoff_wait(task: &TaskShared, retry_no: u32) -> bool {
    let secs = 1u64 << (retry_no.saturating_sub(1)).min(2);
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if task.cancel.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !task.cancel.load(Ordering::Relaxed)
}

// --- worker 池 ---

pub(crate) fn spawn(sh: &Arc<Shared>) {
    let share = SendPtr(sh.share.0);
    let doh = sh.doh.lock().unwrap().clone();
    let count = sh.worker_count;
    let mut handles = sh.workers.lock().unwrap();
    for _ in 0..count {
        let s = sh.clone();
        let doh = doh.clone();
        if let Ok(h) = std::thread::Builder::new()
            .name("dlmgr-worker".into())
            .spawn(move || worker_loop(s, share, doh.as_deref()))
        {
            handles.push(h);
        }
    }
}

/// 取任务：shutdown → None 退出；paused 不派发新任务（运行中跑完）。
/// 出队即在锁内置 RUNNING：active_count 出队即计数，消除"已出队仍 Pending"空窗
/// （C# 以 active_count==0 作为整理前置，空窗内启动整理会让任务在 prepare 时冤死）。
fn take_task(sh: &Shared) -> Option<Arc<TaskShared>> {
    let mut q = sh.queue.lock().unwrap();
    loop {
        if q.shutdown {
            return None;
        }
        if !q.paused {
            if let Some(ht) = q.heap.pop() {
                ht.task.state.store(TASK_RUNNING, Ordering::Relaxed);
                return Some(ht.task);
            }
        }
        q = sh.cv.wait(q).unwrap();
    }
}

fn worker_loop(sh: Arc<Shared>, share: SendPtr, doh: Option<&str>) {
    let easy = Easy::new_configured(share.0, doh);
    while let Some(task) = take_task(&sh) {
        match easy.as_ref() {
            Some(e) => run_task(&sh, e, &task),
            None => {
                // curl 初始化失败（宿主未 global_init 等）：任务快速失败，继续排空
                terminal(&task, false, ERR_NETWORK);
            }
        }
    }
    if let Some(e) = easy {
        e.cleanup();
    }
}

/// 任务终态：先 err 后 state（Release）——观察者以 state==终态为触发，
/// state 可见时 err 必已可见，杜绝"Failed + ERR_OK"的撕裂上报。
fn terminal(task: &TaskShared, canceled: bool, err: i32) {
    if canceled {
        task.err.store(ERR_CANCELED, Ordering::Relaxed);
        task.state.store(TASK_CANCELED, Ordering::Release);
    } else {
        task.err.store(err, Ordering::Relaxed);
        task.state.store(TASK_FAILED, Ordering::Release);
    }
}

/// 执行链主体（§3.2）。
fn run_task(sh: &Shared, easy: &Easy, task: &Arc<TaskShared>) {
    // state=RUNNING 已由 take_task 在队列锁内置位（active_count 出队即计数）
    task.err.store(ERR_OK, Ordering::Relaxed);
    task.bytes_done.store(0, Ordering::Relaxed);

    let Some(vt) = sink_lookup(task.sink) else {
        terminal(task, false, ERR_NETWORK); // sink 已被释放
        return;
    };
    if unsafe { (vt.prepare)(vt.ctx, task.size) } != 0 {
        unsafe { (vt.abort)(vt.ctx) };
        terminal(task, false, ERR_NETWORK);
        return;
    }

    // 空文件短路：crc32(∅) == 0，直接校验落定
    if task.size == 0 {
        if task.crc == 0 && unsafe { (vt.finish)(vt.ctx) } == 0 {
            task.err.store(ERR_OK, Ordering::Relaxed);
            task.state.store(TASK_DONE, Ordering::Release);
        } else {
            unsafe { (vt.abort)(vt.ctx) };
            terminal(task, false, ERR_CRC_MISMATCH);
        }
        return;
    }

    let task_bucket = (task.task_bps > 0).then(|| TokenBucket::new(task.task_bps));
    let mut off = 0u64;
    let mut crc = 0u32;
    let mut st = RunState { range_phase: true, fallback_used: false, retries: 0, last_err: ERR_OK };

    loop {
        if task.cancel.load(Ordering::Relaxed) {
            unsafe { (vt.abort)(vt.ctx) };
            terminal(task, true, ERR_CANCELED);
            return;
        }

        let url = if st.range_phase { &task.range_url } else { &task.full_url };
        let out = perform_attempt(sh, task, easy, &vt, &st, &mut off, &mut crc, url, task_bucket.as_ref());

        match transition(&mut st, &out, task.size, sh.retry_count) {
            Decision::Done => {
                task.state.store(TASK_VERIFYING, Ordering::Relaxed);
                if unsafe { (vt.finish)(vt.ctx) } == 0 {
                    task.err.store(ERR_OK, Ordering::Relaxed);
                    task.state.store(TASK_DONE, Ordering::Release);
                } else {
                    unsafe { (vt.abort)(vt.ctx) };
                    terminal(task, false, ERR_NETWORK);
                }
                return;
            }
            Decision::Resume => {
                if !backoff_wait(task, st.retries) {
                    unsafe { (vt.abort)(vt.ctx) };
                    terminal(task, true, ERR_CANCELED);
                    return;
                }
                // range 断点续传：off/crc 状态延续，不 reset
            }
            Decision::RestartFromZero => {
                if !backoff_wait(task, st.retries) {
                    unsafe { (vt.abort)(vt.ctx) };
                    terminal(task, true, ERR_CANCELED);
                    return;
                }
                if off > 0 {
                    // full 阶段从头重下：清掉上次的半截数据。
                    // reset 失败可容忍：FileSink 后续 pwrite 从 0 全量覆盖（已收字节
                    // ≤ size 必被覆盖），glue VfsSink 的 reset 恒为 no-op 成功。
                    unsafe { (vt.reset)(vt.ctx) };
                    off = 0;
                    crc = 0;
                    task.bytes_done.store(0, Ordering::Relaxed);
                }
            }
            Decision::Fallback(kind) => {
                st.fallback_used = true;
                st.range_phase = false;
                st.last_err = kind;
                if task.full_url.is_empty() {
                    unsafe { (vt.abort)(vt.ctx) }; // 无兜底可用
                    terminal(task, false, kind);
                    return;
                }
                // 同上，reset 失败可容忍（从 0 全量覆盖）。
                unsafe { (vt.reset)(vt.ctx) }; // 从 0 重下（不消耗重试）
                off = 0;
                crc = 0;
                task.bytes_done.store(0, Ordering::Relaxed);
            }
            Decision::Fail(err) => {
                unsafe { (vt.abort)(vt.ctx) };
                terminal(task, err == ERR_CANCELED, err);
                return;
            }
        }
    }
}

/// 单次连接：按阶段设 URL / Range 头 / 写回调，perform 后汇总为 AttemptOutcome。
/// `off` 入参为本次请求起始偏移，出参回传实际收到的累计偏移（断点续传的下一拍
/// 请求 `Range: bytes={off}-` 依赖它——不回传则 Resume 永远从 0 重收、crc 必然翻车）。
fn perform_attempt(
    sh: &Shared,
    task: &TaskShared,
    easy: &Easy,
    vt: &SinkVTable,
    st: &RunState,
    off: &mut u64,
    crc_out: &mut u32,
    url: &str,
    task_bucket: Option<&TokenBucket>,
) -> AttemptOutcome {
    let off_a = AtomicU64::new(*off);
    let crc_a = AtomicU32::new(*crc_out);
    let bad_a = AtomicI32::new(0);
    let io_a = AtomicBool::new(false);
    let mut ctx = WriteCtx {
        curl: easy.raw(),
        sink: vt,
        limiter: &sh.limiter,
        task_bucket,
        task,
        total: task.size,
        range_mode: st.range_phase,
        off: &off_a,
        crc: &crc_a,
        bad: &bad_a,
        io_err: &io_a,
    };
    // 首发（off=0）即带 Range: bytes=0- 自带探测；full_url 普通 GET 无 Range。
    let headers = HeaderList::build(st.range_phase.then_some(*off));
    let cb: WriteCb = write_cb;
    if !easy.set_url(url)
        || !easy.set_write_cb(cb, &mut ctx as *mut WriteCtx as *mut c_void)
        || !easy.set_header_list(headers.head)
    {
        return AttemptOutcome::NetErr(CURLE_FAILED_INIT);
    }

    let code = easy.perform();
    drop(headers); // curl_slist 生命周期覆盖整个 perform

    *crc_out = crc_a.load(Ordering::Relaxed);
    *off = off_a.load(Ordering::Relaxed); // 回传累计写偏移（含失败尝试已收到的字节）

    if task.cancel.load(Ordering::Relaxed) {
        return AttemptOutcome::Canceled;
    }
    if io_a.load(Ordering::Relaxed) {
        return AttemptOutcome::SinkError;
    }
    match bad_a.load(Ordering::Relaxed) {
        1 => return AttemptOutcome::GateFail(ERR_UNSUPPORTED_RANGE),
        2 => return AttemptOutcome::GateFail(ERR_SIZE_MISMATCH),
        _ => {}
    }
    if code != super::curlw_backend::CURLE_OK {
        return AttemptOutcome::NetErr(code);
    }
    let resp = easy.response_code();
    let cr = if st.range_phase { easy.content_range() } else { None };
    let clen = easy.content_length();
    let complete = *off == task.size;
    let crc_ok = *crc_out == task.crc;
    AttemptOutcome::HttpOk { resp, cr, clen, complete, crc_ok }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: u64 = 100;

    fn rs(range: bool, fb: bool, retries: u32) -> RunState {
        RunState { range_phase: range, fallback_used: fb, retries, last_err: ERR_OK }
    }

    fn http_ok(resp: i64, cr: Option<(u64, u64)>, clen: i64, complete: bool, crc_ok: bool) -> AttemptOutcome {
        AttemptOutcome::HttpOk { resp, cr, clen, complete, crc_ok }
    }

    #[test]
    fn range_200_falls_back_without_retry() {
        let mut st = rs(true, false, 0);
        assert!(matches!(
            transition(&mut st, &http_ok(200, None, -1, false, false), SIZE, 3),
            Decision::Fallback(e) if e == ERR_UNSUPPORTED_RANGE
        ));
        assert_eq!(st.retries, 0); // 兜底不消耗重试
    }

    #[test]
    fn range_416_falls_back() {
        let mut st = rs(true, false, 1);
        assert!(matches!(
            transition(&mut st, &http_ok(416, None, -1, false, false), SIZE, 3),
            Decision::Fallback(e) if e == ERR_UNSUPPORTED_RANGE
        ));
    }

    #[test]
    fn range_content_range_mismatch_falls_back() {
        let mut st = rs(true, false, 0);
        // 总长不符
        assert!(matches!(
            transition(&mut st, &http_ok(206, Some((0, 99)), -1, false, true), SIZE, 3),
            Decision::Fallback(e) if e == ERR_SIZE_MISMATCH
        ));
        // 起始偏移不设闸（Q1 决策）：总长相符即放行，错源由 crc 兜底；
        // 字节不足（complete=false）视同断线，走断点重试
        assert!(matches!(
            transition(&mut st, &http_ok(206, Some((5, SIZE)), -1, false, true), SIZE, 3),
            Decision::Resume
        ));
        // Content-Range 缺失
        assert!(matches!(
            transition(&mut st, &http_ok(206, None, -1, false, true), SIZE, 3),
            Decision::Fallback(e) if e == ERR_SIZE_MISMATCH
        ));
    }

    #[test]
    fn range_crc_mismatch_falls_back() {
        let mut st = rs(true, false, 0);
        assert!(matches!(
            transition(&mut st, &http_ok(206, Some((0, SIZE)), -1, true, false), SIZE, 3),
            Decision::Fallback(e) if e == ERR_CRC_MISMATCH
        ));
        assert_eq!(st.retries, 0);
    }

    #[test]
    fn range_done_when_complete_and_crc_ok() {
        let mut st = rs(true, false, 0);
        assert!(matches!(
            transition(&mut st, &http_ok(206, Some((0, SIZE)), SIZE as i64, true, true), SIZE, 3),
            Decision::Done
        ));
    }

    #[test]
    fn range_neterr_resumes_then_falls_back_when_exhausted() {
        let mut st = rs(true, false, 0);
        for expect in 1..=3u32 {
            assert!(matches!(transition(&mut st, &AttemptOutcome::NetErr(7), SIZE, 3), Decision::Resume));
            assert_eq!(st.retries, expect);
        }
        // 预算用尽且未兜底 → 兜底
        assert!(matches!(
            transition(&mut st, &AttemptOutcome::NetErr(7), SIZE, 3),
            Decision::Fallback(e) if e == ERR_NETWORK
        ));
    }

    #[test]
    fn timeout_maps_to_timeout_err() {
        let mut st = rs(true, false, 0);
        assert!(matches!(transition(&mut st, &AttemptOutcome::NetErr(CURLE_OPERATION_TIMEDOUT), SIZE, 3), Decision::Resume));
        assert_eq!(st.last_err, ERR_TIMEOUT);
    }

    #[test]
    fn gatefail_on_full_phase_consumes_retry_then_fails() {
        let mut st = rs(false, true, 0);
        assert!(matches!(
            transition(&mut st, &AttemptOutcome::GateFail(ERR_SIZE_MISMATCH), SIZE, 2),
            Decision::RestartFromZero
        ));
        assert_eq!(st.retries, 1);
        assert!(matches!(
            transition(&mut st, &AttemptOutcome::GateFail(ERR_SIZE_MISMATCH), SIZE, 2),
            Decision::RestartFromZero
        ));
        assert!(matches!(
            transition(&mut st, &AttemptOutcome::GateFail(ERR_SIZE_MISMATCH), SIZE, 2),
            Decision::Fail(e) if e == ERR_SIZE_MISMATCH
        ));
    }

    #[test]
    fn full_crc_mismatch_consumes_retry_then_fails() {
        let mut st = rs(false, true, 0);
        assert!(matches!(
            transition(&mut st, &http_ok(200, None, SIZE as i64, true, false), SIZE, 1),
            Decision::RestartFromZero
        ));
        assert_eq!(st.last_err, ERR_CRC_MISMATCH);
        assert!(matches!(
            transition(&mut st, &http_ok(200, None, SIZE as i64, true, false), SIZE, 1),
            Decision::Fail(e) if e == ERR_CRC_MISMATCH
        ));
    }

    #[test]
    fn full_clen_mismatch_gates() {
        let mut st = rs(false, true, 0);
        // 响应 2xx 但 Content-Length ≠ size
        assert!(matches!(
            transition(&mut st, &http_ok(200, None, 50, true, true), SIZE, 0),
            Decision::Fail(e) if e == ERR_SIZE_MISMATCH
        ));
    }

    #[test]
    fn full_neterr_retries_from_zero_then_fails() {
        let mut st = rs(false, true, 0);
        assert!(matches!(transition(&mut st, &AttemptOutcome::NetErr(7), SIZE, 3), Decision::RestartFromZero));
        assert!(matches!(transition(&mut st, &AttemptOutcome::NetErr(7), SIZE, 3), Decision::RestartFromZero));
        assert!(matches!(transition(&mut st, &AttemptOutcome::NetErr(7), SIZE, 3), Decision::RestartFromZero));
        assert!(matches!(
            transition(&mut st, &AttemptOutcome::NetErr(7), SIZE, 3),
            Decision::Fail(e) if e == ERR_NETWORK
        ));
    }

    #[test]
    fn canceled_and_sink_error() {
        let mut st = rs(true, false, 0);
        assert!(matches!(
            transition(&mut st, &AttemptOutcome::Canceled, SIZE, 3),
            Decision::Fail(e) if e == ERR_CANCELED
        ));
        assert!(matches!(
            transition(&mut st, &AttemptOutcome::SinkError, SIZE, 3),
            Decision::Fail(e) if e == ERR_NETWORK
        ));
    }

    #[test]
    fn backoff_responds_to_cancel() {
        let t = TaskShared::new(1, "n".into(), "r".into(), String::new(), 1, 0, 0, 0);
        t.cancel.store(true, Ordering::Relaxed);
        assert!(!backoff_wait(&t, 1)); // 取消即时返回，不真等 1s
    }
}
