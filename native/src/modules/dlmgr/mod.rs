// 下载管理器轨道（T4 骨架 / T5 完整语义）实现于此目录。
//
//! dlmgr：下载管理器（设计 §3）。vfs 与 dlmgr 编译期互不引用（Q9）：
//! dlmgr 只认识 sink vtable，组合由 glue 层完成。
//! * 线程模型（Q5）：N 条 worker 阻塞跑 curl easy 单任务；reporter 10Hz 回调（Q16）。
//! * FFI 惯例与 curlw 一致：UTF-8 `*const c_char`、opaque 句柄、cdecl、
//!   panic=abort 下 FFI 边界不做 catch_unwind。curlw_global_init 由宿主（C#）负责。
//! * C ABI 权威契约：本目录 FFI 导出即 dlmgr ABI 的单一事实源（C# 镜像：
//!   csharp/DownloadManager.cs）；改动既有签名须 bump DLMGR_ABI_VERSION。
pub(crate) mod curlw_backend;
pub(crate) mod rate;
pub(crate) mod report;
pub(crate) mod sink;
pub(crate) mod task;
pub(crate) mod worker;

use std::cell::Cell;
use std::collections::{BinaryHeap, HashMap};
use std::ffi::CStr;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;

use libc::{c_char, c_void};

use crate::modules::curlw::{
    curlw_share_cleanup, curlw_share_enable_default_locks, curlw_share_init, curlw_share_setopt_int,
};
use curlw_backend::{CURLSHOPT_SHARE, CURL_LOCK_DATA_DNS};
use rate::SpeedLimiter;
use sink::FileSink;
use task::{HeapTask, TaskShared, ERR_CANCELED, TASK_CANCELED, TASK_RUNNING, TASK_VERIFYING};

/// 任务回调：`cb(user, task_id, state, done, total, bps, err)`。
pub(crate) type DlmgrTaskCb = unsafe extern "C" fn(*mut c_void, u64, i32, u64, u64, u64, i32);
/// 全局回调：`cb(user, active, done_cnt, failed_cnt, bytes_done, bytes_total, bps)`。
pub(crate) type DlmgrGlobalCb = unsafe extern "C" fn(*mut c_void, u32, u32, u32, u64, u64, u64);

/// 跨线程持有的裸指针（回调 user / curl share 句柄）：仅存取，不解引用。
#[derive(Clone, Copy)]
pub(crate) struct SendPtr(pub(crate) *mut c_void);
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}

// Mark dlmgr-owned threads at their entry point, before callbacks can run. The Arc's
// allocation has a stable address even before its JoinHandle is stored (or after it
// is taken by shutdown). Reject shutdown of *any* manager from these threads: a
// reporter callback shutting down a different manager could join a reporter that
// is waiting on this one, and workers can re-enter through sink callbacks.
thread_local! {
    static INTERNAL_MANAGER: Cell<*const Shared> = Cell::new(ptr::null());
}

pub(crate) struct InternalThreadGuard {
    previous: *const Shared,
}

impl InternalThreadGuard {
    fn enter(sh: &Arc<Shared>) -> Self {
        let previous = INTERNAL_MANAGER.with(|current| {
            let previous = current.get();
            current.set(Arc::as_ptr(sh));
            previous
        });
        Self { previous }
    }
}

impl Drop for InternalThreadGuard {
    fn drop(&mut self) {
        INTERNAL_MANAGER.with(|current| current.set(self.previous));
    }
}

struct QueueState {
    heap: BinaryHeap<HeapTask>,
    paused: bool,
    shutdown: bool,
    next_seq: u64,
}

/// 管理器共享态：FFI / worker / reporter 三方通过 Arc<Shared> 协作。
pub(crate) struct Shared {
    queue: Mutex<QueueState>,
    cv: Condvar,
    tasks: Mutex<HashMap<u64, Arc<TaskShared>>>,
    limiter: SpeedLimiter,
    retry_count: u32,
    worker_count: u32,
    share: SendPtr, // *mut CURLSH（DNS 共享）
    doh: Mutex<Option<String>>,
    /// CA 证书 bundle（PEM）：worker 建柄时快照进 CURLOPT_CAINFO_BLOB。
    /// 移动端 BoringSSL 无编译期默认 CA 路径，不设则 HTTPS 握手必败。
    ca_blob: Mutex<Option<Vec<u8>>>,
    /// CURLOPT_HTTP_VERSION 透传值（0 = curl 默认协商）；worker 建柄时快照。
    http_version: AtomicI64,
    task_cb: Mutex<Option<(DlmgrTaskCb, SendPtr)>>,
    global_cb: Mutex<Option<(DlmgrGlobalCb, SendPtr)>>,
    reporter_stop: AtomicBool,
    started: AtomicBool,
    next_task_id: AtomicU64,
    // 全历史累计器：任务终态上报一拍后即退役（从 tasks 移除），终态计数与字节量
    // 沉淀于此，全局回调的 done/failed/bytes 全历史语义不随退役而丢。
    done_total: AtomicU64,
    failed_total: AtomicU64,
    bytes_done_total: AtomicU64,
    bytes_total_total: AtomicU64,
    workers: Mutex<Vec<JoinHandle<()>>>,
    reporter: Mutex<Option<JoinHandle<()>>>,
}

impl Shared {
    fn new(worker_count: u32, global_bps: u64, retry_count: u32, share: *mut c_void) -> Shared {
        Shared {
            queue: Mutex::new(QueueState {
                heap: BinaryHeap::new(),
                paused: false,
                shutdown: false,
                next_seq: 0,
            }),
            cv: Condvar::new(),
            tasks: Mutex::new(HashMap::new()),
            limiter: SpeedLimiter::new(global_bps),
            retry_count,
            worker_count: worker_count.clamp(1, 64),
            share: SendPtr(share),
            doh: Mutex::new(None),
            ca_blob: Mutex::new(None),
            http_version: AtomicI64::new(0),
            task_cb: Mutex::new(None),
            global_cb: Mutex::new(None),
            reporter_stop: AtomicBool::new(false),
            started: AtomicBool::new(false),
            next_task_id: AtomicU64::new(0),
            done_total: AtomicU64::new(0),
            failed_total: AtomicU64::new(0),
            bytes_done_total: AtomicU64::new(0),
            bytes_total_total: AtomicU64::new(0),
            workers: Mutex::new(Vec::new()),
            reporter: Mutex::new(None),
        }
    }

    /// 从队列摘除 id（若在排队）；返回是否摘到。锁序：queue 单锁，无嵌套。
    fn remove_queued(&self, id: u64) -> bool {
        let mut q = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        if q.heap.is_empty() {
            return false;
        }
        let mut removed = false;
        let kept: Vec<HeapTask> = q
            .heap
            .drain()
            .filter(|ht| {
                if ht.task.id == id {
                    removed = true;
                    false
                } else {
                    true
                }
            })
            .collect();
        for ht in kept {
            q.heap.push(ht);
        }
        removed
    }

    /// 改排队中任务的下载权重（priority 大者先派发）：drain 重排即可，队列短，
    /// O(n) 足够。仅排队中的任务可改——运行中不可抢占、终态已退役，均返回 false。
    /// 同级排序沿用原 seq（FIFO 不变）。锁序：queue 单锁，无嵌套。
    fn set_priority(&self, id: u64, priority: i32) -> bool {
        let mut q = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        if q.heap.is_empty() {
            return false;
        }
        let mut changed = false;
        let kept: Vec<HeapTask> = q
            .heap
            .drain()
            .map(|ht| {
                if ht.task.id == id {
                    changed = true;
                    HeapTask {
                        priority,
                        seq: ht.seq,
                        task: ht.task,
                    }
                } else {
                    ht
                }
            })
            .collect();
        for ht in kept {
            q.heap.push(ht);
        }
        changed
    }

    /// 已排队任务的取消：直接终态（worker 未运行它，不碰 sink）。
    /// 先 err 后 state（Release），与 worker::terminal 同纪律。
    fn mark_canceled(&self, t: &TaskShared) {
        t.cancel.store(true, Ordering::Relaxed);
        t.err.store(ERR_CANCELED, Ordering::Relaxed);
        t.state.store(TASK_CANCELED, Ordering::Release);
    }

    /// cancel_all：清空队列（逐个标 Canceled）+ 给运行中任务打取消旗标。
    fn cancel_all(&self) {
        let drained: Vec<HeapTask> = self.queue.lock().unwrap_or_else(PoisonError::into_inner).heap.drain().collect();
        let tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
        for ht in drained {
            self.mark_canceled(&ht.task);
        }
        for t in tasks.values() {
            t.cancel.store(true, Ordering::Relaxed);
        }
        drop(tasks);
        self.cv.notify_all();
    }
}

unsafe fn shared(mgr: *mut c_void) -> Option<&'static Arc<Shared>> {
    (mgr as *const Arc<Shared>).as_ref()
}

unsafe fn cstr_to_string(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    Some(CStr::from_ptr(p).to_string_lossy().into_owned())
}

// --- FFI（§3.1）---

/// 创建管理器；返回 opaque 句柄（Box<Arc<Shared>>），失败返回 NULL。
/// worker_count 会被夹到 1..=64；global_bps=0 不限速；retry_count 为重试预算（默认 3）。
#[no_mangle]
pub unsafe extern "C" fn dlmgr_create(
    worker_count: u32,
    global_bps: u64,
    retry_count: u32,
) -> *mut c_void {
    // 全局一个 share（DNS 缓存共享）+ 默认锁集
    let share = curlw_share_init();
    if share.is_null() {
        return ptr::null_mut();
    }
    if curlw_share_setopt_int(share, CURLSHOPT_SHARE, CURL_LOCK_DATA_DNS) != 0
        || curlw_share_enable_default_locks(share) != 0
    {
        curlw_share_cleanup(share);
        return ptr::null_mut();
    }
    let boxed = Box::into_raw(Box::new(Arc::new(Shared::new(
        worker_count,
        global_bps,
        retry_count,
        share,
    ))));
    boxed as *mut c_void
}

/// 设置 DOH 服务器（CURLOPT_DOH_URL；worker 每柄一份，重复调用以最后一条为准）。
#[no_mangle]
pub unsafe extern "C" fn dlmgr_add_doh_url(mgr: *mut c_void, url: *const c_char) -> i32 {
    let Some(sh) = shared(mgr) else { return -1 };
    let Some(url) = cstr_to_string(url) else {
        return -1;
    };
    if url.is_empty() {
        return -1;
    }
    *sh.doh.lock().unwrap_or_else(PoisonError::into_inner) = Some(url);
    0
}

/// 设置 CA 证书 bundle（PEM 或 DER 拼接；CURLOPT_CAINFO_BLOB 透传）。
/// worker 建柄时一次性快照，须在 Start 前调用；重复调用以最后一条为准。
/// data=NULL/len=0 清空。返回 0 成功，-1 参数错（mgr 无效）。
/// 移动端（iOS/Android/tvOS）BoringSSL 无编译期默认 CA 路径，HTTPS 必须显式设置。
#[no_mangle]
pub unsafe extern "C" fn dlmgr_set_ca_blob(mgr: *mut c_void, data: *const u8, len: usize) -> i32 {
    let Some(sh) = shared(mgr) else { return -1 };
    if data.is_null() || len == 0 {
        *sh.ca_blob.lock().unwrap_or_else(PoisonError::into_inner) = None;
        return 0;
    }
    *sh.ca_blob.lock().unwrap_or_else(PoisonError::into_inner) = Some(std::slice::from_raw_parts(data, len).to_vec());
    0
}

/// 设置 HTTP 协议版本（CURLOPT_HTTP_VERSION 透传）：0=curl 默认协商、1=1.0、
/// 2=1.1、3=2.0、4=2TLS、5=2 PriorKnowledge、30=3（失败回退）、31=3ONLY。
/// worker 建柄时一次性快照，须在 Start 前调用。返回 0 成功，-1 参数错。
#[no_mangle]
pub unsafe extern "C" fn dlmgr_set_http_version(mgr: *mut c_void, version: i64) -> i32 {
    let Some(sh) = shared(mgr) else { return -1 };
    match version {
        0 | 1 | 2 | 3 | 4 | 5 | 30 | 31 => {
            sh.http_version.store(version, Ordering::Relaxed);
            0
        }
        _ => -1,
    }
}

/// 启动 worker 池 + reporter 线程（幂等）。curlw_global_init 由宿主负责，dlmgr 不调用。
/// 先起 reporter：它承担全部终态回调，起不来必须报错回滚 started（宿主可重试），
/// 且必须在 worker 之前——worker::spawn 无幂等护栏，失败重试会重复入池。
#[no_mangle]
pub unsafe extern "C" fn dlmgr_start(mgr: *mut c_void) -> i32 {
    let Some(sh) = shared(mgr) else { return -1 };
    if sh.started.swap(true, Ordering::SeqCst) {
        return 0;
    }
    let reporter = match report::spawn(sh.clone()) {
        Ok(h) => h,
        Err(_) => {
            sh.started.store(false, Ordering::SeqCst);
            return -1;
        }
    };
    *sh.reporter.lock().unwrap_or_else(PoisonError::into_inner) = Some(reporter);
    worker::spawn(sh);
    0
}

/// 入队任务（priority 大者先取、同级 FIFO、不抢占；排队中可经
/// dlmgr_set_priority 动态改权重）。sink 为已注册 sink 的句柄。
/// full_url 可为 NULL/空 = 无兜底。task_id 出参可空。成功 *task_id = id，返回 0。
#[no_mangle]
pub unsafe extern "C" fn dlmgr_enqueue(
    mgr: *mut c_void,
    range_url: *const c_char,
    full_url: *const c_char,
    name: *const c_char,
    size: u64,
    crc32: u32,
    priority: i32,
    task_bps: u64,
    sink: u64,
    task_id: *mut u64,
) -> i32 {
    let Some(sh) = shared(mgr) else { return -1 };
    let Some(range_url) = cstr_to_string(range_url) else {
        return -1;
    };
    if range_url.is_empty() {
        return -1;
    }
    let full_url = cstr_to_string(full_url).unwrap_or_default();
    // name 是 ABI 保留位（§3.1）：任务回调签名不带 name，native 不存储。
    let _ = name;
    if sink == 0 || sink::sink_lookup(sink).is_none() {
        return -1; // 未注册的 sink 句柄
    }
    {
        let mut q = sh.queue.lock().unwrap_or_else(PoisonError::into_inner);
        if q.shutdown {
            return -1;
        }
        let id = sh.next_task_id.fetch_add(1, Ordering::Relaxed) + 1;
        let t = Arc::new(TaskShared::new(
            id, range_url, full_url, size, crc32, task_bps, sink,
        ));
        sh.tasks.lock().unwrap_or_else(PoisonError::into_inner).insert(id, t.clone());
        q.next_seq += 1;
        let seq = q.next_seq;
        q.heap.push(HeapTask {
            priority,
            seq,
            task: t,
        });
        if !task_id.is_null() {
            *task_id = id;
        }
    }
    sh.cv.notify_all();
    0
}

/// 取消单个任务：排队中的直接置 Canceled；运行中的打取消旗标（写回调中止）。
/// 已退役（终态上报后移除）的任务同样返回 -1。
#[no_mangle]
pub unsafe extern "C" fn dlmgr_cancel(mgr: *mut c_void, id: u64) -> i32 {
    let Some(sh) = shared(mgr) else { return -1 };
    let removed = sh.remove_queued(id);
    let t = sh.tasks.lock().unwrap_or_else(PoisonError::into_inner).get(&id).cloned();
    let Some(t) = t else { return -1 };
    if removed {
        sh.mark_canceled(&t);
    } else {
        t.cancel.store(true, Ordering::Relaxed);
    }
    sh.cv.notify_all();
    0
}

/// 取消全部任务。
#[no_mangle]
pub unsafe extern "C" fn dlmgr_cancel_all(mgr: *mut c_void) -> i32 {
    let Some(sh) = shared(mgr) else { return -1 };
    sh.cancel_all();
    0
}

/// 动态改排队中任务的下载权重（priority 大者先派发、同级 FIFO、不抢占）。
/// 仅排队中的任务可改：运行中不可抢占、终态已退役、id 不存在，均返回 -1。
/// paused 期间同样生效；纯新增导出，不 bump DLMGR_ABI_VERSION。
#[no_mangle]
pub unsafe extern "C" fn dlmgr_set_priority(mgr: *mut c_void, task_id: u64, priority: i32) -> i32 {
    let Some(sh) = shared(mgr) else { return -1 };
    if sh.set_priority(task_id, priority) {
        0
    } else {
        -1
    }
}

/// 暂停派发（运行中跑完）；C# 以 active_count==0 作为整理前置。
#[no_mangle]
pub unsafe extern "C" fn dlmgr_pause(mgr: *mut c_void) -> i32 {
    let Some(sh) = shared(mgr) else { return -1 };
    sh.queue.lock().unwrap_or_else(PoisonError::into_inner).paused = true;
    0
}

/// 恢复派发。
#[no_mangle]
pub unsafe extern "C" fn dlmgr_resume(mgr: *mut c_void) -> i32 {
    let Some(sh) = shared(mgr) else { return -1 };
    sh.queue.lock().unwrap_or_else(PoisonError::into_inner).paused = false;
    sh.cv.notify_all();
    0
}

/// 全局限速动态生效（bps=0 不限）。
#[no_mangle]
pub unsafe extern "C" fn dlmgr_set_global_speed(mgr: *mut c_void, bps: u64) -> i32 {
    let Some(sh) = shared(mgr) else { return -1 };
    sh.limiter.set_global(bps);
    0
}

/// 任务级回调（可传 NULL 清除）。reporter 线程 10Hz 触发；delegate 由宿主保活。
#[no_mangle]
pub unsafe extern "C" fn dlmgr_set_task_callback(
    mgr: *mut c_void,
    cb: Option<DlmgrTaskCb>,
    user: *mut c_void,
) -> i32 {
    let Some(sh) = shared(mgr) else { return -1 };
    *sh.task_cb.lock().unwrap_or_else(PoisonError::into_inner) = cb.map(|f| (f, SendPtr(user)));
    0
}

/// 全局回调（可传 NULL 清除）。先逐任务后全局；delegate 由宿主保活。
#[no_mangle]
pub unsafe extern "C" fn dlmgr_set_global_callback(
    mgr: *mut c_void,
    cb: Option<DlmgrGlobalCb>,
    user: *mut c_void,
) -> i32 {
    let Some(sh) = shared(mgr) else { return -1 };
    *sh.global_cb.lock().unwrap_or_else(PoisonError::into_inner) = cb.map(|f| (f, SendPtr(user)));
    0
}

/// 运行中（Running/Verifying）任务数。
#[no_mangle]
pub unsafe extern "C" fn dlmgr_active_count(mgr: *mut c_void) -> u32 {
    let Some(sh) = shared(mgr) else { return 0 };
    let tasks = sh.tasks.lock().unwrap_or_else(PoisonError::into_inner);
    let mut n = 0u32;
    for t in tasks.values() {
        let s = t.state.load(Ordering::Relaxed);
        if s == TASK_RUNNING || s == TASK_VERIFYING {
            n += 1;
        }
    }
    n
}

/// 注册 FileSink（每任务独立 OS 句柄，pwrite 落普通文件）；失败返回 0。
/// 终态后由宿主调 sink_release(h) 回收。
#[no_mangle]
pub unsafe extern "C" fn dlmgr_sink_create_file(
    mgr: *mut c_void,
    path: *const c_char,
    size: u64,
) -> u64 {
    let _ = mgr; // 保留 ABI 位置（§3.1）；FileSink 自包含
    let Some(path) = cstr_to_string(path) else {
        return 0;
    };
    FileSink::create(&path, size).unwrap_or(0)
}

/// 任务终态后由宿主释放 sink（经注册表 dtor 回收 FileSink/VfsSink 的 ctx）。
/// 只能在对应任务上报 Done/Failed/Canceled 之后调用。
#[no_mangle]
pub unsafe extern "C" fn dlmgr_sink_release(sink: u64) {
    sink::sink_release(sink);
}

/// Legacy void ABI. Same behavior as dlmgr_shutdown_try, but rejection is a no-op:
/// the handle remains live. Callers needing a result (especially callbacks) must
/// use dlmgr_shutdown_try and arrange shutdown on an external thread.
#[no_mangle]
pub unsafe extern "C" fn dlmgr_shutdown(mgr: *mut c_void) {
    let _ = dlmgr_shutdown_try(mgr);
}

/// Cancel all tasks, join workers/reporter, release share, and consume mgr.
/// Returns 0 on success (NULL is a successful no-op), -1 from ANY dlmgr internal
/// worker/reporter thread, including sink/VFS commit callbacks. Rejection changes
/// no state and does not consume the handle, even during an external shutdown.
/// The all-manager restriction prevents cross-manager join cycles as well as
/// self-joins. Dispatch shutdown to an external thread; callbacks must not wait
/// for that shutdown to finish. The host must serialize external shutdown with
/// other handle use (including start and another shutdown); success invalidates mgr.
/// Additive export: the legacy void ABI and DLMGR_ABI_VERSION remain unchanged.
#[no_mangle]
pub unsafe extern "C" fn dlmgr_shutdown_try(mgr: *mut c_void) -> i32 {
    // Check TLS before dereferencing mgr, acquiring locks, or taking join handles.
    if INTERNAL_MANAGER.with(|current| !current.get().is_null()) {
        return -1;
    }
    let Some(sh) = shared(mgr) else { return 0 };
    {
        let mut q = sh.queue.lock().unwrap_or_else(PoisonError::into_inner);
        q.shutdown = true;
        q.paused = false;
    }
    sh.cancel_all();
    sh.cv.notify_all();

    let workers = std::mem::take(&mut *sh.workers.lock().unwrap_or_else(PoisonError::into_inner));
    for w in workers {
        let _ = w.join();
    }
    // Release 配对 reporter 的 Acquire：stop 可见时全部 worker 的终态写入必已可见，
    // reporter 的最终补拍才能把最后一批终态发出去。
    sh.reporter_stop.store(true, Ordering::Release);
    let reporter = sh.reporter.lock().unwrap_or_else(PoisonError::into_inner).take();
    if let Some(r) = reporter {
        let _ = r.join();
    }

    curlw_share_cleanup(sh.share.0); // worker 已全部退出
    drop(Box::from_raw(mgr as *mut Arc<Shared>));
    0
}


/// dlmgr C ABI 版本。
pub const DLMGR_ABI_VERSION: i32 = 1;

#[no_mangle]
pub unsafe extern "C" fn dlmgr_abi_version() -> i32 {
    DLMGR_ABI_VERSION
}

#[cfg(test)]
mod tests;
