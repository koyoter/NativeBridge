use super::*;

#[test]
fn file_sink_roundtrip_and_release() {
    let mut path = std::env::temp_dir();
    path.push(format!("dlmgr_sink_test_{}.bin", std::process::id()));
    let h = FileSink::create(path.to_str().unwrap(), 8).expect("create sink");
    let vt = sink_lookup(h).expect("registered");
    // SAFETY: vt/ctx 由 sink_lookup 从注册表取出的存活条目同源发放；
    // sink 在本测试末尾才 release，调用串行发生在单测试线程上。
    unsafe {
        assert_eq!((vt.prepare)(vt.ctx, 8), 0);
        let data = b"abcdefgh";
        assert_eq!((vt.write)(vt.ctx, 2, data.as_ptr(), 6), 0);
        assert_eq!((vt.write)(vt.ctx, 0, data.as_ptr(), 2), 0);
        assert_eq!((vt.finish)(vt.ctx), 0);
    }
    assert_eq!(std::fs::read(&path).unwrap(), b"ababcdef");
    sink_release(h); // 走 dtor 回收 Box
    sink_release(h); // 二次释放无害
    let _ = std::fs::remove_file(path);
}
