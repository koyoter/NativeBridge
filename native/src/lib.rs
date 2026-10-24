// [lib] name = "NativeBridge"（导出库名，不可改）触发 crate 级 non_snake_case，
// 无文件级豁免点，是唯一保留的 crate 级 allow；其余 lint 已收敛到
// bindings.rs 的文件级 allow 与逐项删除死码。
#![allow(non_snake_case)]

mod modules;

use libc::c_char;
use std::ffi::CString;
use std::sync::OnceLock;

/// 返回静态版本串指针。规则 2 的**文档化例外**：crate 根版本导出不属于任何
/// 模块，`unsafe` 仅因跨 ABI 返回裸指针。
///
/// # Safety
/// 返回值指向进程生命周期的静态存储（OnceLock），调用方不得 free 或改写。
#[no_mangle]
// 例外位点（可证不失败）：版本串由 format! 与 C 字符串组件拼接，不可能含内部 NUL。
#[allow(clippy::expect_used)]
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
            CString::new(s).expect("version string cannot contain NUL")
        })
        .as_ptr()
}
