// [lib] name = "NativeBridge"（导出库名，不可改）触发 crate 级 non_snake_case，
// 无文件级豁免点，是唯一保留的 crate 级 allow；其余 lint 已收敛到
// bindings.rs 的文件级 allow 与逐项删除死码。
#![allow(non_snake_case)]

mod modules;

use libc::c_char;
use std::ffi::CString;
use std::sync::OnceLock;

#[no_mangle]
pub unsafe extern "C" fn nativebridge_version() -> *const c_char {
    static VERSION: OnceLock<CString> = OnceLock::new();
    VERSION
        .get_or_init(|| {
            let (curl_v, ssl_v, h2_v) = modules::curlw::version_components();
            let s = format!(
                "NativeBridge {} [Rust] [curlw] (curl {}, ssl {}, nghttp2 {})",
                env!("CARGO_PKG_VERSION"),
                curl_v,
                ssl_v,
                h2_v
            );
            CString::new(s).unwrap()
        })
        .as_ptr()
}
