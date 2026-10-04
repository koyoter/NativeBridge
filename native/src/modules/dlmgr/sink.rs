//! sink vtable + 注册表（u64 句柄，Q9）+ FileSink 参考实现（每任务独立句柄，pwrite 落普通文件）。
//!
//! 契约（§3.2）：`prepare` 开始任务（返回 0 成功）、`write(rel_off,..)` 写入、
//! `reset` 兜底/重试切回从 0 重下、`finish` 全部校验通过落定（VfsSink 即 commit）、
//! `abort` 任务作废（失败/取消）。句柄经 `register_full` 注册存活，
//! 任务终态后由宿主调 `sink_release`。

use std::collections::HashMap;
use std::fs::File;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use libc::c_void;

use crate::modules::util::pwrite;

/// C ABI 函数指针表（glue 以此桥接 vfs；dlmgr 只认这张表，不认识 vfs）。
#[repr(C)]
pub(crate) struct SinkVTable {
    pub ctx: *mut libc::c_void,
    pub prepare: unsafe extern "C" fn(ctx: *mut libc::c_void, size: u64) -> i32, // 开始任务，返回 0 成功
    pub write: unsafe extern "C" fn(ctx: *mut libc::c_void, rel_off: u64, data: *const u8, len: usize) -> i32,
    pub reset: unsafe extern "C" fn(ctx: *mut libc::c_void) -> i32, // 兜底切换：从 0 重下
    pub finish: unsafe extern "C" fn(ctx: *mut libc::c_void) -> i32, // 全部校验通过，落定
    pub abort: unsafe extern "C" fn(ctx: *mut libc::c_void) -> i32, // 任务作废
}
// 注册表跨线程存取：ctx 由调用方保证生命周期（终态后才 release），函数指针只读。
unsafe impl Send for SinkVTable {}
unsafe impl Sync for SinkVTable {}

/// 注册表条目：vtable + 可选析构（FileSink 的 Box 在 release 时回收；
/// glue 的 VfsSink 生命周期由 vfs writer 的 commit/abort 收口，dtor 为 None）。
struct SinkEntry {
    vt: SinkVTable,
    dtor: Option<unsafe extern "C" fn(*mut c_void)>,
}

impl Clone for SinkVTable {
    fn clone(&self) -> Self {
        *self
    }
}
impl Copy for SinkVTable {}

fn registry() -> &'static Mutex<HashMap<u64, SinkEntry>> {
    static R: OnceLock<Mutex<HashMap<u64, SinkEntry>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_handle() -> &'static AtomicU64 {
    static N: AtomicU64 = AtomicU64::new(0);
    &N
}

pub(crate) fn register_full(vt: SinkVTable, dtor: Option<unsafe extern "C" fn(*mut c_void)>) -> u64 {
    let h = next_handle().fetch_add(1, Ordering::Relaxed) + 1; // 0 保留为失败值
    registry().lock().unwrap().insert(h, SinkEntry { vt, dtor });
    h
}

/// 任务终态后释放；带析构的 sink（FileSink）在此回收 ctx。
pub(crate) fn sink_release(h: u64) {
    if h == 0 {
        return;
    }
    if let Some(e) = registry().lock().unwrap().remove(&h) {
        if let Some(d) = e.dtor {
            unsafe { d(e.vt.ctx) };
        }
    }
}

/// 取一份 vtable 拷贝（worker 执行期间注册表项保证存活：宿主约定终态后才 release）。
pub(crate) fn sink_lookup(h: u64) -> Option<SinkVTable> {
    registry().lock().unwrap().get(&h).map(|e| e.vt)
}

// --- FileSink：pwrite 落普通文件（每 sink 一个独立 OS 句柄）---

pub(crate) struct FileSink {
    file: File,
}

impl FileSink {
    /// 打开（截断）文件并注册；返回 u64 句柄。
    pub(crate) fn create(path: &str, _size: u64) -> Option<u64> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .ok()?;
        let ctx = Box::into_raw(Box::new(FileSink { file })) as *mut c_void;
        Some(register_full(
            SinkVTable {
                ctx,
                prepare: fs_prepare,
                write: fs_write,
                reset: fs_reset,
                finish: fs_finish,
                abort: fs_abort,
            },
            Some(fs_drop),
        ))
    }
}

unsafe extern "C" fn fs_prepare(ctx: *mut c_void, size: u64) -> i32 {
    let fs = &*(ctx as *const FileSink);
    match fs.file.set_len(size) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

unsafe extern "C" fn fs_write(ctx: *mut c_void, rel_off: u64, data: *const u8, len: usize) -> i32 {
    let fs = &*(ctx as *const FileSink);
    let buf = std::slice::from_raw_parts(data, len);
    match pwrite(&fs.file, rel_off, buf) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

unsafe extern "C" fn fs_reset(ctx: *mut c_void) -> i32 {
    let fs = &*(ctx as *const FileSink);
    match fs.file.set_len(0) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

unsafe extern "C" fn fs_finish(ctx: *mut c_void) -> i32 {
    let fs = &*(ctx as *const FileSink);
    match fs.file.sync_all() {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

unsafe extern "C" fn fs_abort(ctx: *mut c_void) -> i32 {
    let fs = &*(ctx as *const FileSink);
    // 任务作废：截为空文件，避免残留半截内容被误用
    let _ = fs.file.set_len(0);
    0
}

unsafe extern "C" fn fs_drop(ctx: *mut c_void) {
    drop(Box::from_raw(ctx as *mut FileSink));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_sink_roundtrip_and_release() {
        let mut path = std::env::temp_dir();
        path.push(format!("dlmgr_sink_test_{}.bin", std::process::id()));
        let h = FileSink::create(path.to_str().unwrap(), 8).expect("create sink");
        let vt = sink_lookup(h).expect("registered");
        unsafe {
            assert_eq!((vt.prepare)(vt.ctx, 8), 0);
            let data = b"abcdefgh";
            assert_eq!((vt.write)(vt.ctx, 2, data.as_ptr(), 6), 0);
            assert_eq!((vt.write)(vt.ctx, 0, data.as_ptr(), 2), 0);
            assert_eq!((vt.finish)(vt.ctx), 0);
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"ababcdef");
        sink_release(h); // 走 dtor 回收 Box
        sink_release(h); // 二次释放无害
        let _ = std::fs::remove_file(path);
    }
}
