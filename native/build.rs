use std::env;
use std::path::Path;

fn link_lib_path(target_os: &str, lib_path: &str) {
    let p = Path::new(lib_path);
    if !p.exists() {
        eprintln!("cargo:warning=Library path does not exist: {}", lib_path);
        return;
    }
    if let Some(parent) = p.parent() {
        println!("cargo:rustc-link-search=native={}", parent.display());
    }
    let stem = p.file_stem().unwrap().to_string_lossy();
    let lib_name = if target_os == "windows" {
        stem.trim_end_matches(".lib").to_string()
    } else {
        stem.trim_start_matches("lib").to_string()
    };
    println!("cargo:rustc-link-lib=static={}", lib_name);
}

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    println!("cargo:rustc-cfg=rust_nativebridge");

    let nb_libs_order = [
        ("NB_CURL_LIBRARY", "curl"),
        ("NB_NGTCP2_LIBRARY", "ngtcp2"),
        ("NB_NGTCP2_CRYPTO_LIBRARY", "ngtcp2_crypto_boringssl"),
        ("NB_NGHTTP3_LIBRARY", "nghttp3"),
        ("NB_NGHTTP2_LIBRARY", "nghttp2"),
        ("NB_SSL_LIBRARY", "ssl"),
        ("NB_CRYPTO_LIBRARY", "crypto"),
        ("NB_ZLIB_LIBRARY", "z"),
    ];

    for (env_var, fallback_name) in &nb_libs_order {
        println!("cargo:rerun-if-env-changed={}", env_var);
        if let Ok(lib_path) = env::var(env_var) {
            if !lib_path.is_empty() {
                link_lib_path(&target_os, &lib_path);
                continue;
            }
        }
        println!("cargo:rustc-link-lib=static={}", fallback_name);
    }

    println!("cargo:rerun-if-env-changed=NB_LIB_DIRS");
    if let Ok(extra_dirs) = env::var("NB_LIB_DIRS") {
        for dir in env::split_paths(&extra_dirs) {
            println!("cargo:rustc-link-search=native={}", dir.display());
        }
    }

    match target_os.as_str() {
        "windows" => {
            println!("cargo:rustc-link-lib=ws2_32");
            println!("cargo:rustc-link-lib=crypt32");
            println!("cargo:rustc-link-lib=bcrypt");
            println!("cargo:rustc-link-lib=iphlpapi");
            println!("cargo:rustc-link-lib=secur32");
            println!("cargo:rustc-link-lib=advapi32");
            println!("cargo:rustc-link-lib=user32");
            println!("cargo:rustc-link-lib=gdi32");
            println!("cargo:rustc-link-lib=wldap32");
            println!("cargo:rustc-link-lib=normaliz");
        }
        "linux" => {
            println!("cargo:rustc-link-lib=pthread");
            println!("cargo:rustc-link-lib=dl");
            println!("cargo:rustc-link-lib=m");
            println!("cargo:rustc-link-lib=stdc++");
        }
        "android" => {
            println!("cargo:rerun-if-env-changed=NB_ANDROID_CXX_LIBRARY");
            println!("cargo:rerun-if-env-changed=NB_ANDROID_CXXABI_LIBRARY");
            println!("cargo:rustc-link-lib=log");
            println!("cargo:rustc-link-lib=android");
            // Keep the Unity plugin self-contained instead of requiring the APK
            // to package an NDK-version-matched libc++_shared.so.
            let cxx_library = env::var("NB_ANDROID_CXX_LIBRARY")
                .expect("NB_ANDROID_CXX_LIBRARY is not set by build1.ps1");
            let cxxabi_library = env::var("NB_ANDROID_CXXABI_LIBRARY")
                .expect("NB_ANDROID_CXXABI_LIBRARY is not set by build1.ps1");
            link_lib_path(&target_os, &cxx_library);
            link_lib_path(&target_os, &cxxabi_library);
            // Fail the build when an Android ABI symbol cannot be resolved.
            println!("cargo:rustc-link-arg=-Wl,--no-undefined");
        }
        "macos" => {
            println!("cargo:rustc-link-lib=framework=CoreFoundation");
            println!("cargo:rustc-link-lib=framework=SystemConfiguration");
            println!("cargo:rustc-link-lib=c++");
            println!("cargo:rustc-link-arg=-Wl,-install_name,@rpath/NativeBridge.dylib");
        }
        "ios" | "tvos" => {
            println!("cargo:rustc-link-lib=framework=CoreFoundation");
            println!("cargo:rustc-link-lib=framework=SystemConfiguration");
            println!("cargo:rustc-link-lib=c++");
        }
        _ => {}
    }
}
