use super::*;

#[test]
fn crc32_known_vectors_and_streaming() {
    assert_eq!(update(0, b"123456789"), 0xCBF4_3926);
    assert_eq!(update(0, &[]), 0);
    let part = update(0, b"1234");
    assert_eq!(update(part, b"56789"), 0xCBF4_3926);
}
