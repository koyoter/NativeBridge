//! curl easy 句柄封装（T4）：URL / DOH / 写回调 / Range 头 setopt + 响应码、
//! Content-Length、Content-Range 读取。直调 `crate::modules::curlw` 的 Rust 绑定
//! （不走 C ABI）。curlw/bindings 对 dlmgr 不可见（私有模块），此处按值镜像
//! 必要常量子集；curl_slist 类型不可命名，以 `*mut c_void` 持有、调用点用
//! `as *mut _` 借参数位置推断还原。

use std::ffi::{CStr, CString};
use std::ptr;

use libc::{c_char, c_void};

use crate::modules::curlw::{
    curlw_easy_cleanup, curlw_easy_getinfo_long, curlw_easy_header, curlw_easy_init,
    curlw_easy_perform, curlw_easy_setopt_long, curlw_easy_setopt_pointer,
    curlw_easy_setopt_string, curlw_header_value, curlw_slist_append, curlw_slist_free_all,
};

// --- curl 常量镜像（仅 dlmgr 用到的子集）---
pub(crate) const CURLOPT_WRITEDATA: i32 = 10001;
pub(crate) const CURLOPT_URL: i32 = 10002;
pub(crate) const CURLOPT_HTTPHEADER: i32 = 10023;
pub(crate) const CURLOPT_WRITEFUNCTION: i32 = 20011;
pub(crate) const CURLOPT_SHARE: i32 = 10100;
pub(crate) const CURLOPT_NOSIGNAL: i32 = 99;
pub(crate) const CURLOPT_LOW_SPEED_LIMIT: i32 = 19;
pub(crate) const CURLOPT_LOW_SPEED_TIME: i32 = 20;
pub(crate) const CURLOPT_CONNECTTIMEOUT_MS: i32 = 156;
pub(crate) const CURLOPT_DOH_URL: i32 = 10279;

pub(crate) const CURLINFO_RESPONSE_CODE: i32 = 0x20_0002; // CURLINFO_LONG + 2
pub(crate) const CURLINFO_CONTENT_LENGTH_DOWNLOAD_T: i32 = 0x60_000F; // CURLINFO_OFF_T + 15

pub(crate) const CURLE_OK: i32 = 0;
pub(crate) const CURLE_FAILED_INIT: i32 = 2;
pub(crate) const CURLE_OPERATION_TIMEDOUT: i32 = 28;

pub(crate) const CURLH_HEADER: u32 = 1;
pub(crate) const CURLSHOPT_SHARE: i32 = 1;
pub(crate) const CURL_LOCK_DATA_DNS: i32 = 3;

// 卡死保护：连接 15s、传输停滞 30s 断开——保证 cancel/shutdown 在对端挂死时也可达。
const CONNECT_TIMEOUT_MS: i64 = 15_000;
const STALL_LOW_SPEED_LIMIT: i64 = 1; // B/s
const STALL_LOW_SPEED_TIME: i64 = 30; // s

/// curl 写回调签名（与 curlw 的 curl_write_callback 一致）。
pub(crate) type WriteCb = unsafe extern "C" fn(*mut c_char, usize, usize, *mut c_void) -> usize;

/// 每 worker 一个 easy 句柄；share（DNS）与 DOH 为一次性配置，跨任务复用。
pub(crate) struct Easy {
    h: *mut c_void,
}

impl Easy {
    pub(crate) fn new_configured(share: *mut c_void, doh: Option<&str>) -> Option<Easy> {
        let h = unsafe { curlw_easy_init() };
        if h.is_null() {
            return None;
        }
        let e = Easy { h };
        unsafe {
            if curlw_easy_setopt_long(h, CURLOPT_NOSIGNAL, 1) != CURLE_OK
                || curlw_easy_setopt_pointer(h, CURLOPT_SHARE, share) != CURLE_OK
                || curlw_easy_setopt_long(h, CURLOPT_CONNECTTIMEOUT_MS, CONNECT_TIMEOUT_MS) != CURLE_OK
                || curlw_easy_setopt_long(h, CURLOPT_LOW_SPEED_LIMIT, STALL_LOW_SPEED_LIMIT) != CURLE_OK
                || curlw_easy_setopt_long(h, CURLOPT_LOW_SPEED_TIME, STALL_LOW_SPEED_TIME) != CURLE_OK
            {
                e.cleanup();
                return None;
            }
        }
        if let Some(u) = doh {
            if !e.set_string(CURLOPT_DOH_URL, u) {
                e.cleanup();
                return None;
            }
        }
        Some(e)
    }

    pub(crate) fn cleanup(self) {
        unsafe { curlw_easy_cleanup(self.h) };
    }

    pub(crate) fn raw(&self) -> *mut c_void {
        self.h
    }

    fn set_string(&self, opt: i32, v: &str) -> bool {
        match CString::new(v) {
            Ok(c) => unsafe { curlw_easy_setopt_string(self.h, opt, c.as_ptr()) == CURLE_OK },
            Err(_) => false, // 内嵌 NUL 的非法 UTF-8 串
        }
    }

    pub(crate) fn set_url(&self, url: &str) -> bool {
        self.set_string(CURLOPT_URL, url)
    }

    pub(crate) fn set_write_cb(&self, cb: WriteCb, ctx: *mut c_void) -> bool {
        unsafe {
            // setopt_pointer 接受 functionpoint 选项（WRITEFUNCTION 属 20000 段）
            curlw_easy_setopt_pointer(self.h, CURLOPT_WRITEFUNCTION, cb as *mut c_void) == CURLE_OK
                && curlw_easy_setopt_pointer(self.h, CURLOPT_WRITEDATA, ctx) == CURLE_OK
        }
    }

    /// 挂自定义请求头；head 为 HeaderList::build 产物（None 传 null 清除）。
    pub(crate) fn set_header_list(&self, head: *mut c_void) -> bool {
        unsafe { curlw_easy_setopt_pointer(self.h, CURLOPT_HTTPHEADER, head) == CURLE_OK }
    }

    pub(crate) fn perform(&self) -> i32 {
        unsafe { curlw_easy_perform(self.h) }
    }

    fn get_long(&self, info: i32) -> i64 {
        let mut v = 0i64;
        unsafe { curlw_easy_getinfo_long(self.h, info, &mut v) };
        v
    }

    /// 响应码（getinfo_long，CURLINFO_LONG 段）。
    pub(crate) fn response_code(&self) -> i64 {
        self.get_long(CURLINFO_RESPONSE_CODE)
    }

    /// Content-Length（getinfo_long 自动分发 OFF_T）；未知（chunked 等）为 -1。
    pub(crate) fn content_length(&self) -> i64 {
        self.get_long(CURLINFO_CONTENT_LENGTH_DOWNLOAD_T)
    }

    /// Content-Range 头解析 → (起始偏移, 总长)。形如 "bytes 0-99/1234"；
    /// "bytes */total"（416）或缺失/不可解析 → None。
    pub(crate) fn content_range(&self) -> Option<(u64, u64)> {
        const NAME: &[u8] = b"Content-Range\0";
        let mut out: *mut c_void = ptr::null_mut();
        let ec = unsafe {
            curlw_easy_header(
                self.h,
                NAME.as_ptr() as *const c_char,
                0,
                CURLH_HEADER,
                -1,
                &mut out as *mut *mut c_void as *mut _,
            )
        };
        if ec != 0 || out.is_null() {
            return None;
        }
        let val = unsafe { curlw_header_value(out as *const _) };
        if val.is_null() {
            return None;
        }
        let s = unsafe { CStr::from_ptr(val) }.to_str().ok()?;
        let slash = s.rfind('/')?;
        let total: u64 = s[slash + 1..].trim().parse().ok()?;
        // 起始偏移取第一个 '-' 之前、剥掉 "bytes " 前缀的数字（取 dash 之后会错拿
        // "50-99" 的结束偏移 99）；"bytes */total"（416）无 '-'，自然返回 None。
        let dash = s[..slash].find('-')?;
        let start: u64 = s[..slash][..dash].split_whitespace().last()?.parse().ok()?;
        Some((start, total))
    }
}

/// 自定义请求头列表；curl_slist 不拷贝字符串，故 CString 持有到 perform 返回。
pub(crate) struct HeaderList {
    pub(crate) head: *mut c_void,
    _keep: Vec<CString>,
}

impl HeaderList {
    /// range = Some(off) → "Range: bytes={off}-"（首发 off=0 自带探测）；
    /// None → 空表（full_url 普通 GET，setopt null 清除自定义头）。
    pub(crate) fn build(range: Option<u64>) -> HeaderList {
        let mut head: *mut c_void = ptr::null_mut();
        let mut keep: Vec<CString> = Vec::new();
        if let Some(off) = range {
            let c = CString::new(format!("Range: bytes={off}-")).expect("no interior NUL");
            unsafe {
                head = curlw_slist_append(ptr::null_mut(), c.as_ptr()) as *mut c_void;
            }
            keep.push(c);
        }
        HeaderList { head, _keep: keep }
    }
}

impl Drop for HeaderList {
    fn drop(&mut self) {
        if !self.head.is_null() {
            unsafe { curlw_slist_free_all(self.head as *mut _) };
        }
    }
}
