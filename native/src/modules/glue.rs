//! glue 层（Q9）：vfs ↔ dlmgr 的唯一交汇点 —— 只有本文件同时 use 两者。
//!
//! 用 vfs writer 三段式实现 dlmgr 的 SinkVTable：
//! * `prepare(size)` → `vfs.alloc`（同名 Active/Downloading → ALREADY_EXISTS，整理中 → BUSY_*）
//! * `write(off,..)` → `writer.write_at`（越界 → WRITE_OVERFLOW；crc 由 vfs 边写边算）
//! * `reset()`       → 无操作：vfs `write_at` 允许回退重写（commit 时整读重算 crc），
//!   兜底/重试的下一笔 `write(0,..)` 天然从 0 重下
//! * `finish()`      → `writer.commit`（校验写满+crc → Active → generation++，C# 即刻可见）
//! * `abort()`       → `writer.abort`（条目丢弃，区间成垃圾）
//!
//! 生命周期：句柄经 `dlmgr_sink_release`（任务终态后由宿主调用）经注册表 dtor 回收；
//! `vfs` 句柄存活期必须覆盖每个由它创建的 sink（vfs 模块文档中的宿主约定）。

use std::ffi::CStr;
use std::slice;

use libc::{c_char, c_void};

use crate::modules::dlmgr::sink::{register_full, SinkVTable};
use crate::modules::vfs::{Vfs, VfsError, VfsWriter};

struct VfsSinkCtx {
    /// vfs_open 的 Arc 句柄（`Arc::into_raw` 产物），仅借用不拥有。
    vfs: *const Vfs,
    name: String,
    writer: Option<VfsWriter>,
}

unsafe extern "C" fn vs_prepare(ctx: *mut c_void, size: u64) -> i32 {
    let c = &mut *(ctx as *mut VfsSinkCtx);
    match (*c.vfs).alloc(&c.name, size) {
        Ok(w) => {
            c.writer = Some(w);
            0
        }
        Err(e) => e.as_i32(),
    }
}

unsafe extern "C" fn vs_write(ctx: *mut c_void, rel_off: u64, data: *const u8, len: usize) -> i32 {
    let c = &mut *(ctx as *mut VfsSinkCtx);
    let Some(w) = c.writer.as_mut() else {
        return VfsError::StateInvalid.as_i32(); // prepare 未成功（或已 finish/abort）
    };
    let buf = slice::from_raw_parts(data, len);
    match w.write_at(rel_off, buf) {
        Ok(()) => 0,
        Err(e) => e.as_i32(),
    }
}

unsafe extern "C" fn vs_reset(_ctx: *mut c_void) -> i32 {
    // vfs write_at 允许回退重写，无需显式动作（见模块注释）。
    0
}

unsafe extern "C" fn vs_finish(ctx: *mut c_void) -> i32 {
    let c = &mut *(ctx as *mut VfsSinkCtx);
    match c.writer.take() {
        Some(w) => match w.commit() {
            Ok(()) => 0,
            Err(e) => e.as_i32(),
        },
        None => VfsError::StateInvalid.as_i32(),
    }
}

unsafe extern "C" fn vs_abort(ctx: *mut c_void) -> i32 {
    let c = &mut *(ctx as *mut VfsSinkCtx);
    if let Some(w) = c.writer.take() {
        w.abort();
    }
    0
}

unsafe extern "C" fn vs_drop(ctx: *mut c_void) {
    drop(Box::from_raw(ctx as *mut VfsSinkCtx));
}

/// 为"下载进 VFS"创建 sink：`vfs` 为 vfs_open 句柄，`name` 为 VFS 内文件名。
/// 返回传给 dlmgr_enqueue 的 sink 句柄；0 = 失败（vfs 为空 / name 非 UTF-8）。
/// sink 数据流：worker 经 vtable 写入，finish 时 commit 落定、abort 时区间成垃圾。
#[no_mangle]
pub unsafe extern "C" fn dlvfs_sink_create_for_vfs(
    vfs: *mut c_void,
    name: *const c_char,
    _size: u64, // 占位（§3.1 ABI 对称）；真实 size 由 prepare 下发
) -> u64 {
    if vfs.is_null() || name.is_null() {
        return 0;
    }
    let Ok(name) = CStr::from_ptr(name).to_str() else {
        return 0; // 非 UTF-8 文件名
    };
    if name.is_empty() {
        return 0;
    }
    let ctx = Box::into_raw(Box::new(VfsSinkCtx {
        // vfs_open = Arc::into_raw(Arc<Vfs>)，返回值即指向 Vfs 的裸指针，直接借用。
        vfs: vfs as *const Vfs,
        name: name.to_string(),
        writer: None,
    }));
    let h = register_full(
        SinkVTable {
            ctx: ctx as *mut c_void,
            prepare: vs_prepare,
            write: vs_write,
            reset: vs_reset,
            finish: vs_finish,
            abort: vs_abort,
        },
        Some(vs_drop),
    );
    if h == 0 {
        drop(Box::from_raw(ctx));
    }
    h
}
