//! curlw：libcurl 封装模块。本目录 `#[no_mangle] extern "C"` 导出集是 curlw C ABI
//! 的权威契约（单一事实源），C# 侧 1:1 镜像见 `csharp/Curlw.cs`：改动既有签名/布局
//! 须 bump `CURLW_ABI_VERSION`，纯新增导出不 bump。

use libc::{c_char, c_double, c_int, c_long, c_uint, c_void, intptr_t, size_t};
#[cfg(not(windows))]
use libc::{pthread_mutex_t, EINTR, ETIMEDOUT, SHUT_RDWR};
use std::collections::HashMap;
use std::ffi::CStr;
use std::mem;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

mod bindings;
use bindings::*;
mod platform;
use platform::*;

fn is_long_option(opt: CURLoption) -> bool {
    let v = opt as c_uint;
    v > CURLOPTTYPE_LONG as c_uint && v < CURLOPTTYPE_OBJECTPOINT as c_uint
}

fn is_object_option(opt: CURLoption) -> bool {
    let v = opt as c_uint;
    v > CURLOPTTYPE_OBJECTPOINT as c_uint && v < CURLOPTTYPE_FUNCTIONPOINT as c_uint
}

fn is_pointer_option(opt: CURLoption) -> bool {
    let v = opt as c_uint;
    v > CURLOPTTYPE_OBJECTPOINT as c_uint && v < CURLOPTTYPE_OFF_T as c_uint
}

fn is_offt_option(opt: CURLoption) -> bool {
    let v = opt as c_uint;
    v > CURLOPTTYPE_OFF_T as c_uint && v < CURLOPTTYPE_BLOB as c_uint
}

fn is_blob_option(opt: CURLoption) -> bool {
    (opt as c_uint) > CURLOPTTYPE_BLOB as c_uint
}

fn allows_unsigned_long(opt: CURLoption) -> bool {
    matches!(
        opt,
        CURLOPT_HTTPAUTH
            | CURLOPT_PROXYAUTH
            | CURLOPT_SOCKS5_AUTH
            | CURLOPT_SSH_AUTH_TYPES
            | CURLOPT_PROTOCOLS
            | CURLOPT_REDIR_PROTOCOLS
    )
}

fn multi_allows_unsigned_long(opt: CURLMoption) -> bool {
    opt == CURLMOPT_MAXCONNECTS
}

fn try_native_long(opt: CURLoption, value: i64) -> Option<c_long> {
    if mem::size_of::<c_long>() == mem::size_of::<i64>() {
        return Some(value as c_long);
    }
    let min = c_long::MIN as i64;
    let max = c_long::MAX as i64;
    if value < min {
        return None;
    }
    if value > max {
        if !allows_unsigned_long(opt) {
            return None;
        }
        let umax = c_long::MAX as u64;
        if (value as u64) > (umax * 2 + 1) {
            return None;
        }
    }
    Some(value as c_long)
}

fn multi_try_native_long(opt: CURLMoption, value: i64) -> Option<c_long> {
    if mem::size_of::<c_long>() == mem::size_of::<i64>() {
        return Some(value as c_long);
    }
    let min = c_long::MIN as i64;
    let max = c_long::MAX as i64;
    if value < min {
        return None;
    }
    if value > max {
        if !multi_allows_unsigned_long(opt) {
            return None;
        }
        let umax = c_long::MAX as u64;
        if (value as u64) > (umax * 2 + 1) {
            return None;
        }
    }
    Some(value as c_long)
}

fn info_type(info: CURLINFO) -> c_int {
    (info as c_uint & CURLINFO_TYPEMASK as c_uint) as c_int
}


pub(crate) fn version_components() -> (String, String, String) {
    let field = |p: *const c_char| -> String {
        if p.is_null() {
            "?".to_string()
        } else {
            unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
        }
    };
    let v = unsafe { curl_version_info(CURLVERSION_NOW) };
    if v.is_null() {
        ("?".to_string(), "?".to_string(), "?".to_string())
    } else {
        let d = unsafe { &*v };
        (field(d.version), field(d.ssl_version), field(d.nghttp2_version))
    }
}

/// curlw C ABI 版本。
pub const CURLW_ABI_VERSION: c_int = 1;

#[no_mangle]
pub unsafe extern "C" fn curlw_abi_version() -> c_int {
    CURLW_ABI_VERSION
}

#[no_mangle]
pub unsafe extern "C" fn curlw_version_imp() -> *const c_char {
    curl_version()
}

#[no_mangle]
pub unsafe extern "C" fn curlw_create_socket(af: c_int, socktype: c_int, protocol: c_int) -> intptr_t {
    #[cfg(windows)]
    {
        socket(af, socktype, protocol) as intptr_t
    }
    #[cfg(not(windows))]
    {
        socket(af, socktype, protocol) as intptr_t
    }
}

#[no_mangle]
pub unsafe extern "C" fn curlw_shutdown_socket(sockfd: intptr_t) -> c_int {
    #[cfg(windows)]
    {
        shutdown(sockfd as curl_socket_t, SD_BOTH)
    }
    #[cfg(not(windows))]
    {
        shutdown(sockfd as c_int, SHUT_RDWR)
    }
}

#[no_mangle]
pub unsafe extern "C" fn curlw_close_socket(sockfd: intptr_t) -> c_int {
    #[cfg(windows)]
    {
        closesocket(sockfd as curl_socket_t)
    }
    #[cfg(not(windows))]
    {
        close(sockfd as c_int)
    }
}

#[no_mangle]
pub unsafe extern "C" fn curlw_errno() -> c_int {
    get_socket_errno()
}

#[no_mangle]
pub unsafe extern "C" fn curlw_global_init(flags: c_int, max_fd_set: c_uint) -> CURLcode {
    let _lk = global_mtx().lock().unwrap();

    let ec = curl_global_init(flags as c_long);
    if ec != CURLE_OK {
        return ec;
    }

    let mut count = init_count().lock().unwrap();
    if *count == 0 {
        // Fallible allocation so a pool OOM surfaces as CURLE_OUT_OF_MEMORY like
        // the C impl (Box::new would abort under panic=abort). Freed with
        // Box::from_raw in curlw_global_cleanup — same global-allocator Layout.
        let layout = std::alloc::Layout::new::<FdSetPool>();
        let raw = std::alloc::alloc(layout) as *mut FdSetPool;
        if raw.is_null() {
            curl_global_cleanup();
            return CURLE_OUT_OF_MEMORY;
        }
        ptr::write(raw, FdSetPool::new(max_fd_set as usize));
        G_FD_POOL.store(raw, Ordering::Release);
    }
    *count += 1;

    CURLE_OK
}

#[no_mangle]
pub unsafe extern "C" fn curlw_global_cleanup() {
    let _lk = global_mtx().lock().unwrap();

    let mut count = init_count().lock().unwrap();
    if *count == 0 {
        return;
    }

    curl_global_cleanup();
    *count -= 1;

    if *count == 0 {
        let pool_ptr = G_FD_POOL.load(Ordering::Acquire);
        if !pool_ptr.is_null() {
            let _ = Box::from_raw(pool_ptr);
            G_FD_POOL.store(ptr::null_mut(), Ordering::Release);
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn curlw_socket_allocfds() -> *mut fd_set {
    let pool = G_FD_POOL.load(Ordering::Acquire);
    if pool.is_null() {
        return ptr::null_mut();
    }
    // Match the C impl: hand back the pooled slot as-is (fresh chunks are already
    // zero-initialized). Callers zero it via curlw_socket_zerofds before use.
    (*pool).allocate()
}

#[no_mangle]
pub unsafe extern "C" fn curlw_socket_freefds(pfds: *mut fd_set) {
    if pfds.is_null() {
        return;
    }
    let pool = G_FD_POOL.load(Ordering::Acquire);
    if !pool.is_null() {
        (*pool).deallocate(pfds);
    }
}

#[no_mangle]
pub unsafe extern "C" fn curlw_socket_zerofds(pfds: *mut fd_set) {
    if pfds.is_null() {
        return;
    }
    fd_zero(pfds);
}

#[no_mangle]
pub unsafe extern "C" fn curlw_socket_select(
    nfds: c_int,
    readfds: *mut fd_set,
    writefds: *mut fd_set,
    exceptfds: *mut fd_set,
    microseconds: u64,
) -> c_int {
    let mut read_input: fd_set = mem::zeroed();
    let mut write_input: fd_set = mem::zeroed();
    let mut except_input: fd_set = mem::zeroed();
    if !readfds.is_null() {
        fd_copy(&mut read_input, readfds);
    }
    if !writefds.is_null() {
        fd_copy(&mut write_input, writefds);
    }
    if !exceptfds.is_null() {
        fd_copy(&mut except_input, exceptfds);
    }

    let mut remaining_us = microseconds;

    loop {
        let mut tv = timeval {
            tv_sec: (remaining_us / 1_000_000) as c_long,
            tv_usec: (remaining_us % 1_000_000) as c_long,
        };
        let tv_ptr = &mut tv as *mut _;

        let start = Instant::now();
        let n = select(
            nfds,
            readfds,
            writefds,
            exceptfds,
            tv_ptr,
        );

        if n < 0 && get_socket_errno() == get_eintr_errno() {
            let elapsed = start.elapsed().as_micros() as u64;
            if elapsed < remaining_us {
                remaining_us -= elapsed;
                if !readfds.is_null() {
                    fd_copy(readfds, &read_input);
                }
                if !writefds.is_null() {
                    fd_copy(writefds, &write_input);
                }
                if !exceptfds.is_null() {
                    fd_copy(exceptfds, &except_input);
                }
                continue;
            }
            set_socket_timeout_errno();
            return 0;
        }

        if n == 0 {
            set_socket_timeout_errno();
        }
        return n;
    }
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_init() -> *mut CURL {
    curl_easy_init()
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_perform(curl: *mut CURL) -> CURLcode {
    curl_easy_perform(curl)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_cleanup(curl: *mut CURL) {
    curl_easy_cleanup(curl)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_reset(curl: *mut CURL) {
    curl_easy_reset(curl)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_strerror_imp(error: CURLcode) -> *const c_char {
    curl_easy_strerror(error)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_setopt_int(
    handle: *mut CURL,
    option: CURLoption,
    optval: c_int,
) -> CURLcode {
    if !is_long_option(option) {
        return CURLE_BAD_FUNCTION_ARGUMENT;
    }
    curl_easy_setopt_long(handle, option, optval as c_long)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_setopt_long(
    handle: *mut CURL,
    option: CURLoption,
    optval: i64,
) -> CURLcode {
    if !is_long_option(option) {
        return CURLE_BAD_FUNCTION_ARGUMENT;
    }
    let Some(native) = try_native_long(option, optval) else {
        return CURLE_BAD_FUNCTION_ARGUMENT;
    };
    curl_easy_setopt_long(handle, option, native)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_setopt_offt(
    handle: *mut CURL,
    option: CURLoption,
    optval: i64,
) -> CURLcode {
    if !is_offt_option(option) {
        return CURLE_BAD_FUNCTION_ARGUMENT;
    }
    curl_easy_setopt_offt(handle, option, optval as curl_off_t)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_setopt_pointer(
    handle: *mut CURL,
    option: CURLoption,
    optval: *mut c_void,
) -> CURLcode {
    if !is_pointer_option(option) {
        return CURLE_BAD_FUNCTION_ARGUMENT;
    }
    curl_easy_setopt_ptr(handle, option, optval)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_setopt_string(
    handle: *mut CURL,
    option: CURLoption,
    optval: *const c_char,
) -> CURLcode {
    if !is_object_option(option) {
        return CURLE_BAD_FUNCTION_ARGUMENT;
    }
    curl_easy_setopt_ptr(handle, option, optval as *mut c_void)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_setopt_blob(
    handle: *mut CURL,
    option: CURLoption,
    data: *mut c_void,
    len: size_t,
    flags: c_uint,
) -> CURLcode {
    if !is_blob_option(option) {
        return CURLE_BAD_FUNCTION_ARGUMENT;
    }
    let blob = curl_blob { data, len, flags };
    curl_easy_setopt_ptr(handle, option, &blob as *const _ as *mut c_void)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_getinfo_int(
    handle: *mut CURL,
    info: CURLINFO,
    outval: *mut c_int,
) -> CURLcode {
    if outval.is_null() || info_type(info) != CURLINFO_LONG {
        return CURLE_BAD_FUNCTION_ARGUMENT;
    }
    let mut tmp: c_long = 0;
    let ec = curl_easy_getinfo_long(handle, info, &mut tmp);
    if ec == CURLE_OK {
        if tmp < c_int::MIN as c_long || tmp > c_int::MAX as c_long {
            return CURLE_BAD_FUNCTION_ARGUMENT;
        }
        *outval = tmp as c_int;
    }
    ec
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_getinfo_long(
    handle: *mut CURL,
    info: CURLINFO,
    outval: *mut i64,
) -> CURLcode {
    if outval.is_null() {
        return CURLE_BAD_FUNCTION_ARGUMENT;
    }
    let t = info_type(info);
    if t == CURLINFO_OFF_T {
        let mut tmp: curl_off_t = 0;
        let ec = curl_easy_getinfo_offt(handle, info, &mut tmp);
        if ec == CURLE_OK {
            *outval = tmp as i64;
        }
        return ec;
    }
    if t != CURLINFO_LONG {
        return CURLE_BAD_FUNCTION_ARGUMENT;
    }
    let mut tmp: c_long = 0;
    let ec = curl_easy_getinfo_long(handle, info, &mut tmp);
    if ec == CURLE_OK {
        *outval = tmp as i64;
    }
    ec
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_getinfo_double(
    handle: *mut CURL,
    info: CURLINFO,
    outval: *mut c_double,
) -> CURLcode {
    if outval.is_null() || info_type(info) != CURLINFO_DOUBLE {
        return CURLE_BAD_FUNCTION_ARGUMENT;
    }
    curl_easy_getinfo_double(handle, info, outval)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_getinfo_pointer(
    handle: *mut CURL,
    info: CURLINFO,
    outval: *mut *mut c_void,
) -> CURLcode {
    if outval.is_null() {
        return CURLE_BAD_FUNCTION_ARGUMENT;
    }
    let t = info_type(info);
    if t == CURLINFO_STRING {
        let mut tmp: *mut c_char = ptr::null_mut();
        let ec = curl_easy_getinfo_ptr(handle, info, &mut tmp as *mut *mut c_char as *mut c_void);
        if ec == CURLE_OK {
            *outval = tmp as *mut c_void;
        }
        return ec;
    }
    if t == CURLINFO_SLIST {
        let mut tmp: *mut curl_slist = ptr::null_mut();
        let ec = curl_easy_getinfo_ptr(handle, info, &mut tmp as *mut *mut curl_slist as *mut c_void);
        if ec == CURLE_OK {
            *outval = tmp as *mut c_void;
        }
        return ec;
    }
    if t == CURLINFO_SOCKET {
        // CURLINFO_SOCKET yields a curl_socket_t; hand curl a correctly-typed
        // pointer so it writes exactly that width (no c_long aliasing).
        let mut tmp: curl_socket_t = CURL_SOCKET_BAD;
        let ec = curl_easy_getinfo(handle, info, &mut tmp as *mut curl_socket_t);
        if ec == CURLE_OK {
            *outval = tmp as usize as *mut c_void;
        }
        return ec;
    }
    CURLE_BAD_FUNCTION_ARGUMENT
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_set_opensocket_global_cb(cb: SocketManagedCb) {
    G_OPEN_CB.store(cb as *mut c_void, Ordering::Release);
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_set_opensocket_cb(
    handle: *mut CURL,
    userdata: *mut c_void,
) -> CURLcode {
    if G_OPEN_CB.load(Ordering::Acquire).is_null() {
        return CURLE_FAILED_INIT;
    }
    let mut res = curl_easy_setopt_ptr(handle, CURLOPT_OPENSOCKETDATA, userdata);
    if res == CURLE_OK {
        res = curl_easy_setopt_ptr(
            handle,
            CURLOPT_OPENSOCKETFUNCTION,
            open_socket_trampoline as *mut c_void,
        );
    }
    res
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_clear_opensocket_cb(handle: *mut CURL) {
    curl_easy_setopt_ptr(handle, CURLOPT_OPENSOCKETFUNCTION, ptr::null_mut());
    curl_easy_setopt_ptr(handle, CURLOPT_OPENSOCKETDATA, ptr::null_mut());
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_set_closesocket_global_cb(cb: SocketManagedCb) {
    G_CLOSE_CB.store(cb as *mut c_void, Ordering::Release);
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_set_closesocket_cb(
    handle: *mut CURL,
    userdata: *mut c_void,
) -> CURLcode {
    if G_CLOSE_CB.load(Ordering::Acquire).is_null() {
        return CURLE_FAILED_INIT;
    }
    let mut res = curl_easy_setopt_ptr(handle, CURLOPT_CLOSESOCKETDATA, userdata);
    if res == CURLE_OK {
        res = curl_easy_setopt_ptr(
            handle,
            CURLOPT_CLOSESOCKETFUNCTION,
            close_socket_trampoline as *mut c_void,
        );
    }
    res
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_clear_closesocket_cb(handle: *mut CURL) {
    curl_easy_setopt_ptr(handle, CURLOPT_CLOSESOCKETFUNCTION, ptr::null_mut());
    curl_easy_setopt_ptr(handle, CURLOPT_CLOSESOCKETDATA, ptr::null_mut());
}

#[no_mangle]
pub unsafe extern "C" fn curlw_multi_init() -> *mut CURLM {
    curl_multi_init()
}

#[no_mangle]
pub unsafe extern "C" fn curlw_multi_cleanup(multi_handle: *mut CURLM) -> CURLMcode {
    curl_multi_cleanup(multi_handle)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_multi_add_handle(
    multi_handle: *mut CURLM,
    curl_handle: *mut CURL,
) -> CURLMcode {
    curl_multi_add_handle(multi_handle, curl_handle)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_multi_remove_handle(
    multi_handle: *mut CURLM,
    curl_handle: *mut CURL,
) -> CURLMcode {
    curl_multi_remove_handle(multi_handle, curl_handle)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_multi_perform(
    multi_handle: *mut CURLM,
    running_handles: *mut c_int,
) -> CURLMcode {
    curl_multi_perform(multi_handle, running_handles)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_multi_fdset(
    multi_handle: *mut CURLM,
    read_fds: *mut fd_set,
    write_fds: *mut fd_set,
    exc_fds: *mut fd_set,
    max_fd: *mut c_int,
) -> CURLMcode {
    curl_multi_fdset(
        multi_handle,
        read_fds as *mut _,
        write_fds as *mut _,
        exc_fds as *mut _,
        max_fd,
    )
}

#[no_mangle]
pub unsafe extern "C" fn curlw_multi_timeout(
    multi_handle: *mut CURLM,
    milliseconds: *mut i64,
) -> CURLMcode {
    let mut ms: c_long = 0;
    let ec = curl_multi_timeout(multi_handle, &mut ms);
    if !milliseconds.is_null() {
        *milliseconds = ms as i64;
    }
    ec
}

#[no_mangle]
pub unsafe extern "C" fn curlw_multi_poll(
    multi_handle: *mut CURLM,
    extra_fds: *mut curl_waitfd,
    extra_nfds: c_uint,
    timeout_ms: c_int,
    numfds: *mut c_int,
) -> CURLMcode {
    curl_multi_poll(multi_handle, extra_fds, extra_nfds, timeout_ms, numfds)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_multi_wait(
    multi_handle: *mut CURLM,
    extra_fds: *mut curl_waitfd,
    extra_nfds: c_uint,
    timeout_ms: c_int,
    numfds: *mut c_int,
) -> CURLMcode {
    curl_multi_wait(multi_handle, extra_fds, extra_nfds, timeout_ms, numfds)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_multi_wakeup(multi_handle: *mut CURLM) -> CURLMcode {
    curl_multi_wakeup(multi_handle)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_multi_strerror_imp(error: CURLMcode) -> *const c_char {
    curl_multi_strerror(error)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_multi_setopt_int(
    multi_handle: *mut CURLM,
    option: CURLMoption,
    optval: c_int,
) -> CURLMcode {
    if !is_long_option(option as CURLoption) {
        return CURLM_BAD_FUNCTION_ARGUMENT;
    }
    curl_multi_setopt_long(multi_handle, option, optval as c_long)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_multi_setopt_long(
    multi_handle: *mut CURLM,
    option: CURLMoption,
    optval: i64,
) -> CURLMcode {
    if !is_long_option(option as CURLoption) {
        return CURLM_BAD_FUNCTION_ARGUMENT;
    }
    let Some(native) = multi_try_native_long(option, optval) else {
        return CURLM_BAD_FUNCTION_ARGUMENT;
    };
    curl_multi_setopt_long(multi_handle, option, native)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_multi_setopt_offt(
    multi_handle: *mut CURLM,
    option: CURLMoption,
    optval: i64,
) -> CURLMcode {
    if !is_offt_option(option as CURLoption) {
        return CURLM_BAD_FUNCTION_ARGUMENT;
    }
    curl_multi_setopt_offt(multi_handle, option, optval as curl_off_t)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_multi_setopt_pointer(
    multi_handle: *mut CURLM,
    option: CURLMoption,
    optval: *mut c_void,
) -> CURLMcode {
    if !is_pointer_option(option as CURLoption) {
        return CURLM_BAD_FUNCTION_ARGUMENT;
    }
    curl_multi_setopt_ptr(multi_handle, option, optval)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_multi_setopt_string(
    multi_handle: *mut CURLM,
    option: CURLMoption,
    optval: *const c_char,
) -> CURLMcode {
    if !is_object_option(option as CURLoption) {
        return CURLM_BAD_FUNCTION_ARGUMENT;
    }
    curl_multi_setopt_ptr(multi_handle, option, optval as *mut c_void)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_multi_info_read(
    multi_handle: *mut CURLM,
    msgs_in_queue: *mut c_int,
) -> *mut CURLMsg {
    curl_multi_info_read(multi_handle, msgs_in_queue)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_msg_get_msg(msg: *const CURLMsg) -> c_int {
    if msg.is_null() {
        return 0;
    }
    (*msg).msg as c_int
}

#[no_mangle]
pub unsafe extern "C" fn curlw_msg_get_easy_handle(msg: *const CURLMsg) -> *mut CURL {
    if msg.is_null() {
        return ptr::null_mut();
    }
    (*msg).easy_handle
}

#[no_mangle]
pub unsafe extern "C" fn curlw_msg_get_result(msg: *const CURLMsg) -> CURLcode {
    if msg.is_null() {
        return CURLE_OK;
    }
    (*msg).result_code()
}

#[no_mangle]
pub unsafe extern "C" fn curlw_slist_append(
    list: *mut curl_slist,
    value: *const c_char,
) -> *mut curl_slist {
    curl_slist_append(list, value)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_slist_free_all(list: *mut curl_slist) {
    curl_slist_free_all(list)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_free(p: *mut c_void) {
    curl_free(p)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_duphandle(handle: *mut CURL) -> *mut CURL {
    curl_easy_duphandle(handle)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_pause(handle: *mut CURL, action: c_int) -> CURLcode {
    curl_easy_pause(handle, action)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_upkeep(handle: *mut CURL) -> CURLcode {
    curl_easy_upkeep(handle)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_recv(
    curl: *mut CURL,
    buffer: *mut c_void,
    buflen: size_t,
    n: *mut size_t,
) -> CURLcode {
    curl_easy_recv(curl, buffer, buflen, n)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_send(
    curl: *mut CURL,
    buffer: *const c_void,
    buflen: size_t,
    n: *mut size_t,
) -> CURLcode {
    curl_easy_send(curl, buffer, buflen, n)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_escape(
    handle: *mut CURL,
    string: *const c_char,
    length: c_int,
) -> *mut c_char {
    curl_easy_escape(handle, string, length)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_unescape(
    handle: *mut CURL,
    string: *const c_char,
    inlength: c_int,
    outlength: *mut c_int,
) -> *mut c_char {
    curl_easy_unescape(handle, string, inlength, outlength)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_version_info() -> *const curl_version_info_data {
    curl_version_info(CURLVERSION_NOW)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_verinfo_features(d: *const curl_version_info_data) -> c_int {
    if d.is_null() { 0 } else { (*d).features }
}

#[no_mangle]
pub unsafe extern "C" fn curlw_verinfo_version_num(d: *const curl_version_info_data) -> c_uint {
    if d.is_null() { 0 } else { (*d).version_num }
}

#[no_mangle]
pub unsafe extern "C" fn curlw_verinfo_version(d: *const curl_version_info_data) -> *const c_char {
    if d.is_null() { ptr::null() } else { (*d).version }
}

#[no_mangle]
pub unsafe extern "C" fn curlw_verinfo_ssl_version(d: *const curl_version_info_data) -> *const c_char {
    if d.is_null() { ptr::null() } else { (*d).ssl_version }
}

#[no_mangle]
pub unsafe extern "C" fn curlw_verinfo_libz_version(d: *const curl_version_info_data) -> *const c_char {
    if d.is_null() { ptr::null() } else { (*d).libz_version }
}

#[no_mangle]
pub unsafe extern "C" fn curlw_verinfo_nghttp2_version(d: *const curl_version_info_data) -> *const c_char {
    if d.is_null() { ptr::null() } else { (*d).nghttp2_version }
}

#[no_mangle]
pub unsafe extern "C" fn curlw_verinfo_quic_version(d: *const curl_version_info_data) -> *const c_char {
    if d.is_null() { ptr::null() } else { (*d).quic_version }
}

#[no_mangle]
pub unsafe extern "C" fn curlw_verinfo_cainfo(d: *const curl_version_info_data) -> *const c_char {
    if d.is_null() { ptr::null() } else { (*d).cainfo }
}

#[no_mangle]
pub unsafe extern "C" fn curlw_verinfo_capath(d: *const curl_version_info_data) -> *const c_char {
    if d.is_null() { ptr::null() } else { (*d).capath }
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_header(
    handle: *mut CURL,
    name: *const c_char,
    nameindex: size_t,
    origin: c_uint,
    request: c_int,
    hout: *mut *mut curl_header,
) -> CURLHcode {
    curl_easy_header(handle, name, nameindex, origin, request, hout)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_easy_nextheader(
    handle: *mut CURL,
    origin: c_uint,
    request: c_int,
    prev: *mut curl_header,
) -> *mut curl_header {
    curl_easy_nextheader(handle, origin, request, prev)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_header_name(h: *const curl_header) -> *const c_char {
    if h.is_null() { ptr::null() } else { (*h).name }
}

#[no_mangle]
pub unsafe extern "C" fn curlw_header_value(h: *const curl_header) -> *const c_char {
    if h.is_null() { ptr::null() } else { (*h).value }
}

#[no_mangle]
pub unsafe extern "C" fn curlw_header_amount(h: *const curl_header) -> size_t {
    if h.is_null() { 0 } else { (*h).amount }
}

#[no_mangle]
pub unsafe extern "C" fn curlw_header_index(h: *const curl_header) -> size_t {
    if h.is_null() { 0 } else { (*h).index }
}

#[no_mangle]
pub unsafe extern "C" fn curlw_header_origin(h: *const curl_header) -> c_uint {
    if h.is_null() { 0 } else { (*h).origin }
}

#[no_mangle]
pub unsafe extern "C" fn curlw_share_init() -> *mut CURLSH {
    curl_share_init()
}

#[no_mangle]
pub unsafe extern "C" fn curlw_share_cleanup(share: *mut CURLSH) -> CURLSHcode {
    // Hold the map lock across cleanup + erase so the lock context can't be seen
    // out of sync with libcurl's view of the share (matches the C impl). A null
    // share is handled by curl_share_cleanup, which returns CURLSHE_INVALID.
    let key = share as usize;
    let mut map = share_locks().lock().unwrap();
    let ec = curl_share_cleanup(share);
    if ec == CURLSHE_OK {
        map.remove(&key);
    }
    ec
}

#[no_mangle]
pub unsafe extern "C" fn curlw_share_setopt_int(
    share: *mut CURLSH,
    option: CURLSHoption,
    value: c_int,
) -> CURLSHcode {
    if option != CURLSHOPT_SHARE && option != CURLSHOPT_UNSHARE {
        return CURLSHE_BAD_OPTION;
    }
    curl_share_setopt_int(share, option, value)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_share_enable_default_locks(share: *mut CURLSH) -> CURLSHcode {
    if share.is_null() {
        return CURLSHE_INVALID;
    }
    let key = share as usize;
    // Hold the map lock across the whole install (insert + setopt + rollback) so a
    // concurrent enable/cleanup can't race the callbacks (matches the C impl).
    // curl_share_setopt only stores the callbacks; it never invokes them, and the
    // callbacks take the per-share locks, not this map lock, so there is no reentry.
    let mut map = share_locks().lock().unwrap();
    if map.contains_key(&key) {
        return CURLSHE_OK;
    }
    let Some(ls) = ShareLockSet::new() else {
        return CURLSHE_NOMEM;
    };
    let lock_set_ptr = &*ls as *const ShareLockSet as *mut c_void;
    map.insert(key, ls);

    let mut ec = curl_share_setopt_ptr(share, CURLSHOPT_USERDATA, lock_set_ptr);
    if ec == CURLSHE_OK {
        ec = curl_share_setopt_ptr(share, CURLSHOPT_LOCKFUNC, share_lock as *mut c_void);
    }
    if ec == CURLSHE_OK {
        ec = curl_share_setopt_ptr(share, CURLSHOPT_UNLOCKFUNC, share_unlock as *mut c_void);
    }

    if ec != CURLSHE_OK {
        map.remove(&key);
        curl_share_setopt_ptr(share, CURLSHOPT_LOCKFUNC, ptr::null_mut());
        curl_share_setopt_ptr(share, CURLSHOPT_UNLOCKFUNC, ptr::null_mut());
        curl_share_setopt_ptr(share, CURLSHOPT_USERDATA, ptr::null_mut());
    }
    ec
}

#[no_mangle]
pub unsafe extern "C" fn curlw_share_strerror_imp(error: CURLSHcode) -> *const c_char {
    curl_share_strerror(error)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_mime_init(easy: *mut CURL) -> *mut curl_mime {
    curl_mime_init(easy)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_mime_free(mime: *mut curl_mime) {
    curl_mime_free(mime)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_mime_addpart(mime: *mut curl_mime) -> *mut curl_mimepart {
    curl_mime_addpart(mime)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_mime_name(part: *mut curl_mimepart, name: *const c_char) -> CURLcode {
    curl_mime_name(part, name)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_mime_data(
    part: *mut curl_mimepart,
    data: *const c_char,
    datasize: size_t,
) -> CURLcode {
    curl_mime_data(part, data, datasize)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_mime_filedata(part: *mut curl_mimepart, filename: *const c_char) -> CURLcode {
    curl_mime_filedata(part, filename)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_mime_filename(part: *mut curl_mimepart, filename: *const c_char) -> CURLcode {
    curl_mime_filename(part, filename)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_mime_type(part: *mut curl_mimepart, mimetype: *const c_char) -> CURLcode {
    curl_mime_type(part, mimetype)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_mime_encoder(part: *mut curl_mimepart, encoding: *const c_char) -> CURLcode {
    curl_mime_encoder(part, encoding)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_mime_headers(
    part: *mut curl_mimepart,
    headers: *mut curl_slist,
    take_ownership: c_int,
) -> CURLcode {
    curl_mime_headers(part, headers, take_ownership)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_mime_subparts(part: *mut curl_mimepart, subparts: *mut curl_mime) -> CURLcode {
    curl_mime_subparts(part, subparts)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_url() -> *mut CURLU {
    curl_url()
}

#[no_mangle]
pub unsafe extern "C" fn curlw_url_cleanup(handle: *mut CURLU) {
    curl_url_cleanup(handle)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_url_dup(in_: *const CURLU) -> *mut CURLU {
    curl_url_dup(in_)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_url_get(
    handle: *const CURLU,
    what: CURLUPart,
    part: *mut *mut c_char,
    flags: c_uint,
) -> CURLUcode {
    curl_url_get(handle, what, part, flags)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_url_set(
    handle: *mut CURLU,
    what: CURLUPart,
    part: *const c_char,
    flags: c_uint,
) -> CURLUcode {
    curl_url_set(handle, what, part, flags)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_url_strerror_imp(error: CURLUcode) -> *const c_char {
    curl_url_strerror(error)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_ws_recv(
    handle: *mut CURL,
    buffer: *mut c_void,
    buflen: size_t,
    recv: *mut size_t,
    meta: *mut *const curl_ws_frame,
) -> CURLcode {
    curl_ws_recv(handle, buffer, buflen, recv, meta)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_ws_send(
    handle: *mut CURL,
    buffer: *const c_void,
    buflen: size_t,
    sent: *mut size_t,
    fragsize: i64,
    flags: c_uint,
) -> CURLcode {
    curl_ws_send(handle, buffer, buflen, sent, fragsize as curl_off_t, flags)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_ws_meta(handle: *mut CURL) -> *const curl_ws_frame {
    curl_ws_meta(handle)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_wsframe_flags(f: *const curl_ws_frame) -> c_int {
    if f.is_null() { 0 } else { (*f).flags }
}

#[no_mangle]
pub unsafe extern "C" fn curlw_wsframe_offset(f: *const curl_ws_frame) -> i64 {
    if f.is_null() { 0 } else { (*f).offset as i64 }
}

#[no_mangle]
pub unsafe extern "C" fn curlw_wsframe_bytesleft(f: *const curl_ws_frame) -> i64 {
    if f.is_null() { 0 } else { (*f).bytesleft as i64 }
}

#[no_mangle]
pub unsafe extern "C" fn curlw_wsframe_len(f: *const curl_ws_frame) -> size_t {
    if f.is_null() { 0 } else { (*f).len }
}

#[no_mangle]
pub unsafe extern "C" fn curlw_multi_assign(
    multi_handle: *mut CURLM,
    sockfd: intptr_t,
    sockp: *mut c_void,
) -> CURLMcode {
    curl_multi_assign(multi_handle, sockfd as curl_socket_t, sockp)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_multi_socket_action(
    multi_handle: *mut CURLM,
    s: intptr_t,
    ev_bitmask: c_int,
    running_handles: *mut c_int,
) -> CURLMcode {
    curl_multi_socket_action(multi_handle, s as curl_socket_t, ev_bitmask, running_handles)
}

#[no_mangle]
pub unsafe extern "C" fn curlw_multi_get_handles(multi_handle: *mut CURLM) -> *mut *mut CURL {
    curl_multi_get_handles(multi_handle)
}
