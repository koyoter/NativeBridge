use super::*;

#[test]
fn checked_alignment_rejects_overflow() {
    assert_eq!(checked_align4k(0), Some(0));
    assert_eq!(checked_align4k(1), Some(4096));
    assert_eq!(checked_align4k(u64::MAX - 4095), Some(u64::MAX - 4095));
    assert_eq!(checked_align4k(u64::MAX - 4094), None);
    assert_eq!(checked_align4k(u64::MAX), None);
}

#[test]
fn complete_slot_with_invalid_extent_is_not_parsed() {
    for (offset, size, logical) in [(0, u64::MAX, 4096), (4096, 4096, 4096), (1, 1, 4096)] {
        let mut entries = [Entry {
            name_off: 0,
            name_len: 1,
            state: ST_ACTIVE,
            offset,
            size,
            crc: 0,
        }];
        let img = build_region(1, logical, &mut entries, b"x\0");
        let slot = scan_slot(&img.bytes, 0).unwrap();
        assert_eq!(slot.total, img.bytes.len() as u64);
        assert!(slot.parsed.is_none());
    }
    assert!(scan_slot(&[], usize::MAX).is_none());
}

/// R04：重建紧凑 blob 必须先取旧偏移再覆写 name_off——
/// 名字 blob 含死字节（abort/delete 残留）时，条目不得挂到错误名字下。
#[test]
fn build_region_reads_old_offsets_before_rewrite() {
    let old_blob: Vec<u8> = {
        let mut b = Vec::new();
        b.extend_from_slice(b"AAA");
        b.push(0);
        b.extend_from_slice(b"BBB"); // 已删除条目留下的死字节
        b.push(0);
        b.extend_from_slice(b"CCC");
        b.push(0);
        b
    };
    let mut entries = vec![
        Entry {
            name_off: 0,
            name_len: 3,
            state: ST_ACTIVE,
            offset: 0,
            size: 4,
            crc: 0,
        },
        Entry {
            name_off: 8,
            name_len: 3,
            state: ST_ACTIVE,
            offset: 4096,
            size: 4,
            crc: 0,
        },
    ];
    let img = build_region(1, 4096, &mut entries, &old_blob);
    let names = &img.bytes[img.blob_off..];
    assert_eq!(names, b"AAA\0CCC\0"); // 修复前：C 会读到死字节 "BBB"
    assert_eq!(entries[1].name_off, 4);
}
