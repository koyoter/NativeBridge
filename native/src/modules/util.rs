//! vfs / dlmgr 共用的小工具：CRC-32（zlib 语义）与平台 pwrite。
//! 两模块互不引用（Q9），共用本工具不影响该约束。

use std::fs::File;
use std::sync::OnceLock;

#[cfg(not(windows))]
use std::os::unix::fs::FileExt;
#[cfg(windows)]
use std::os::windows::fs::FileExt;

// --- CRC-32（IEEE 802.3 reflected，poly 0xEDB88320，与 zlib crc32 一致）---

fn table() -> &'static [u32; 256] {
    static T: OnceLock<[u32; 256]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0u32; 256];
        let mut i = 0usize;
        while i < 256 {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            t[i] = c;
            i += 1;
        }
        t
    })
}

/// 原始状态流式增量：state 自 0xFFFF_FFFF 起，结束后 `!state` 即 crc32 值。
pub(crate) fn update_raw(state: u32, data: &[u8]) -> u32 {
    let t = table();
    let mut c = state;
    for &b in data {
        c = t[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c
}

/// zlib 语义：可在 crc32 结果上续算；update(0, a++b) == update(update(0, a), b)。
pub(crate) fn update(crc: u32, data: &[u8]) -> u32 {
    !update_raw(!crc, data)
}

pub(crate) fn crc32(data: &[u8]) -> u32 {
    !update_raw(0xFFFF_FFFF, data)
}

// --- 平台 pwrite（绝对偏移，写满）：Windows 的 seek_write 会移动文件指针且可能
// 部分完成，逐段循环写满；POSIX 直接 write_all_at。调用方各自持独占句柄，
// 无同句柄并发问题。---

pub(crate) fn pwrite(f: &File, off: u64, buf: &[u8]) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        let mut off = off;
        let mut buf = buf;
        while !buf.is_empty() {
            let n = f.seek_write(buf, off)?;
            if n == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "pwrite: seek_write wrote 0"));
            }
            buf = &buf[n as usize..];
            off += n as u64;
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        f.write_all_at(buf, off)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_vectors_and_streaming() {
        assert_eq!(update(0, b"123456789"), 0xCBF4_3926);
        assert_eq!(update(0, &[]), 0);
        let part = update(0, b"1234");
        assert_eq!(update(part, b"56789"), 0xCBF4_3926);
    }
}
