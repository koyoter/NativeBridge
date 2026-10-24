use super::*;

#[test]
fn crc32_known_vectors_and_streaming() {
    assert_eq!(update(0, b"123456789"), 0xCBF4_3926);
    assert_eq!(update(0, &[]), 0);
    let part = update(0, b"1234");
    assert_eq!(update(part, b"56789"), 0xCBF4_3926);
}

/// macOS 落盘冒烟（T1 flush 协议）：F_FULLFSYNC 在真实临时文件上可用。
/// 仅在 osx rust-test job 跑（其它平台走 sync_all 分支，无需冒烟）。
#[cfg(target_os = "macos")]
#[test]
fn fsync_full_smoke_macos() {
    let mut p = std::env::temp_dir();
    p.push(format!("nb_fsync_smoke_{}.bin", std::process::id()));
    let f = std::fs::File::create(&p).unwrap();
    fsync_full(&f).unwrap();
    drop(f);
    let _ = std::fs::remove_file(p);
}
