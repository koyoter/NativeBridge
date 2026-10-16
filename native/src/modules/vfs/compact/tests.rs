use super::*;

#[test]
fn plan_rejects_invalid_extents_and_overflow() {
    let e = |offset, size| Entry {
        name_off: 0,
        name_len: 0,
        state: ST_ACTIVE,
        offset,
        size,
        crc: 0,
    };
    assert!(matches!(
        build_plan(vec![("x".into(), e(0, u64::MAX))], 4096),
        Err(VfsError::WriteOverflow)
    ));
    assert!(matches!(
        build_plan(vec![("x".into(), e(4096, 4096))], 4096),
        Err(VfsError::InvalidArg)
    ));
    assert!(matches!(
        build_plan(vec![("x".into(), e(1, 1))], 4096),
        Err(VfsError::InvalidArg)
    ));
    assert!(matches!(
        build_plan(
            vec![("x".into(), e(0, 8192)), ("y".into(), e(4096, 4096))],
            8192
        ),
        Err(VfsError::InvalidArg)
    ));
    let logical = u64::MAX - 4095;
    assert!(matches!(
        build_plan(vec![("x".into(), e(4096, 8192))], logical),
        Err(VfsError::WriteOverflow)
    ));
    assert!(build_plan(Vec::new(), 1).is_err());
}
