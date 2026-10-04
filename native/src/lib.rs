#![allow(unused_macros)]
#![allow(nonstandard_style)]
#![allow(dead_code)]
#![allow(private_interfaces)]

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
