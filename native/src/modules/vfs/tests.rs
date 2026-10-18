use super::*;
use std::fs;
use std::sync::atomic::AtomicU32 as TestAtomic;
use std::thread;
use std::time::Duration;

static SEQ: TestAtomic = TestAtomic::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        let p = std::env::temp_dir().join(format!(
            "vfs-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 顺序写满并 commit（顺序写约束路径）。
fn seq_write(v: &Vfs, name: &str, data: &[u8]) -> Result<(), VfsError> {
    let mut w = v.alloc(name, data.len() as u64)?;
    let mut off = 0u64;
    for chunk in data.chunks(1 << 20) {
        w.write_at(off, chunk)?;
        off += chunk.len() as u64;
    }
    w.commit()
}

fn data_of(dir: &Path) -> Vec<u8> {
    fs::read(dir.join("files.vfs")).unwrap()
}

// 测试允许 panic：轮询超时以此失败（全套测试唯一一处 panic!）。
#[allow(clippy::panic)]
fn wait_idle(v: &Vfs) {
    for _ in 0..60_000 {
        if v.compact_status().0 == COMPACT_IDLE {
            return;
        }
        thread::sleep(Duration::from_millis(1));
    }
    panic!("compaction did not finish");
}

fn wait_moving(v: &Vfs) {
    for _ in 0..60_000 {
        if v.compact_status().0 == COMPACT_MOVE {
            return;
        }
        thread::sleep(Duration::from_millis(1));
    }
}

// --- T1 ---

/// 格式 roundtrip：写 → flush → 重开 → 索引一致；commit 后 lookup 可见、generation 递增。
#[test]
fn roundtrip_flush_reopen() {
    let td = TempDir::new();
    let a = vec![7u8; 5000];
    let b: Vec<u8> = (0..3000u32).map(|i| (i * 7 % 251) as u8).collect();
    let v = Vfs::open(&td.0).unwrap();
    assert_eq!(v.get_generation(), 1);
    // alloc/abort 也 bump generation：每次 seq_write = alloc + commit = 2 拍
    seq_write(&v, "alpha.bin", &a).unwrap();
    assert!(v.lookup("alpha.bin").is_ok());
    assert_eq!(v.get_generation(), 3);
    seq_write(&v, "beta.dat", &b).unwrap();
    assert_eq!(v.get_generation(), 5);
    v.flush().unwrap();
    drop(v);

    let v = Vfs::open(&td.0).unwrap();
    assert_eq!(v.get_generation(), 5);
    let ia = v.lookup("alpha.bin").unwrap();
    assert_eq!(ia.size, 5000);
    assert_eq!(ia.state, ST_ACTIVE);
    assert_eq!(ia.crc, format::crc32(&a));
    let raw = data_of(&td.0);
    assert_eq!(&raw[ia.offset as usize..ia.offset as usize + 5000], &a[..]);
    let ib = v.lookup("beta.dat").unwrap();
    assert_eq!(ib.crc, format::crc32(&b));
    assert_eq!(
        &raw[ib.offset as usize..ib.offset as usize + b.len()],
        &b[..]
    );
}

/// 单 region 翻转后另一份可被恢复路径选中（人为破坏 active region）。
#[test]
fn region_recovery_picks_other_region() {
    let td = TempDir::new();
    {
        let v = Vfs::open(&td.0).unwrap();
        // 全新 header：SB(64B) + 空 region(32B) @64 → 首次 flush 追加到 96。
        seq_write(&v, "f1", &[1u8; 100]).unwrap();
        v.flush().unwrap();
    }
    // 破坏 active region（offset 96 处 region 的 entry table 区域）。
    let mut hdr = fs::read(td.0.join("header.vfs")).unwrap();
    assert_eq!(hdr[96 + 40], 0); // 确认打在 entry 表内
    hdr[96 + 40] ^= 0xFF;
    fs::write(td.0.join("header.vfs"), &hdr).unwrap();

    let v = Vfs::open(&td.0).unwrap();
    // 回退到另一份（gen=1 的空 region）。
    assert_eq!(v.get_generation(), 1);
    assert!(matches!(v.lookup("f1"), Err(VfsError::NotFound)));
}

/// Downloading 残留：open 后条目消失且 stat.garbage 计入。
#[test]
fn downloading_leftover_becomes_garbage() {
    let td = TempDir::new();
    {
        let v = Vfs::open(&td.0).unwrap();
        seq_write(&v, "keep", &[9u8; 100]).unwrap();
        let w = v.alloc("dl", 5000).unwrap();
        std::mem::forget(w); // 模拟崩溃：writer 未 abort
        v.flush().unwrap(); // Downloading 条目落盘
        let s = v.stat();
        assert_eq!(s.garbage, 0); // writer 存活期区间仍算预约
    }
    let v = Vfs::open(&td.0).unwrap();
    assert!(matches!(v.lookup("dl"), Err(VfsError::NotFound)));
    assert!(v.lookup("keep").is_ok());
    let s = v.stat();
    assert_eq!(s.garbage, format::align4k(5000));
}

/// flush 前崩溃：未 flush 的 commit 丢失、已 flush 的完好。
#[test]
fn unflushed_commit_lost() {
    let td = TempDir::new();
    {
        let v = Vfs::open(&td.0).unwrap();
        seq_write(&v, "f1", &[1u8; 100]).unwrap();
        v.flush().unwrap();
        seq_write(&v, "f2", &[2u8; 100]).unwrap();
        // 不 flush，直接"崩溃"（drop）
    }
    let v = Vfs::open(&td.0).unwrap();
    assert!(v.lookup("f1").is_ok());
    assert!(matches!(v.lookup("f2"), Err(VfsError::NotFound)));
}

/// 水位线防御：滞后的旧 gen persist 被拒绝（GenChanged）；同 gen 重复 flush 放行；
/// gen 前进后 flush 仍正常推进水位线。
#[test]
fn stale_gen_persist_rejected() {
    let td = TempDir::new();
    let v = Vfs::open(&td.0).unwrap();
    seq_write(&v, "f1", &[1u8; 100]).unwrap(); // gen: 1 → 2
    v.flush().unwrap(); // 水位线落定为 flush 时的 idx.generation = 2
                        // 旧 gen（1 < 水位线 2）模拟滞后的 flush：拒绝且不动 SuperBlock。
    assert!(matches!(
        v.shared.persist_image(1, &[]),
        Err(VfsError::GenChanged)
    ));
    // 同 gen（== 水位线）重复 flush 合法：写闲置槽 + 翻转无害。
    v.flush().unwrap();
    // gen 前进后 flush 正常。
    v.delete("f1").unwrap(); // gen: 2 → 3
    v.flush().unwrap();
}

/// 同名 alloc 三种状态：Active/Downloading → ALREADY_EXISTS；Deleted → 可替换；
/// abort 后区间成垃圾。
#[test]
fn alloc_name_rules() {
    let td = TempDir::new();
    let v = Vfs::open(&td.0).unwrap();
    seq_write(&v, "x", &[1u8; 10]).unwrap();
    assert!(matches!(v.alloc("x", 10), Err(VfsError::AlreadyExists)));
    v.delete("x").unwrap();
    seq_write(&v, "x", &[2u8; 10]).unwrap(); // Deleted → 替换
    assert_eq!(v.lookup("x").unwrap().crc, format::crc32(&[2u8; 10]));
    assert_eq!(v.stat().garbage, format::align4k(10)); // 旧数据区成垃圾

    let w = v.alloc("y", 10).unwrap();
    assert!(matches!(v.alloc("y", 10), Err(VfsError::AlreadyExists))); // Downloading
    w.abort();
    assert!(matches!(v.lookup("y"), Err(VfsError::NotFound)));
    assert_eq!(v.stat().garbage, 2 * format::align4k(10));

    assert!(matches!(v.delete("nope"), Err(VfsError::NotFound)));
    assert!(matches!(v.delete("x"), Ok(()))); // Active → Deleted
    assert!(matches!(v.delete("x"), Ok(()))); // 幂等
}

/// enumerate 两段式：gen 变化返回 GEN_CHANGED，重查收敛。
#[test]
fn enumerate_two_phase() {
    let td = TempDir::new();
    let v = Vfs::open(&td.0).unwrap();
    seq_write(&v, "alpha", &[1u8; 10]).unwrap();
    seq_write(&v, "beta", &[2u8; 20]).unwrap();

    let (gen, count, blen) = v.enumerate_query().unwrap();
    assert_eq!(count, 2);
    let mut names = vec![0u8; blen as usize];
    let mut offs = vec![0u64; count as usize];
    let mut sizes = vec![0u64; count as usize];
    let mut crcs = vec![0u32; count as usize];
    let mut states = vec![0u8; count as usize];
    v.enumerate_read(
        gen,
        &mut names,
        &mut offs,
        &mut sizes,
        &mut crcs,
        &mut states,
    )
    .unwrap();
    let got: Vec<&str> = names
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| std::str::from_utf8(s).unwrap())
        .collect();
    assert!(got.contains(&"alpha") && got.contains(&"beta"));

    seq_write(&v, "gamma", &[3u8; 30]).unwrap(); // gen 变化
    assert!(matches!(
        v.enumerate_read(
            gen,
            &mut names,
            &mut offs,
            &mut sizes,
            &mut crcs,
            &mut states
        ),
        Err(VfsError::GenChanged)
    ));
    let (gen2, count2, blen2) = v.enumerate_query().unwrap();
    assert_eq!(count2, 3);
    let mut names2 = vec![0u8; blen2 as usize];
    let mut offs2 = vec![0u64; count2 as usize];
    let mut sizes2 = vec![0u64; count2 as usize];
    let mut crcs2 = vec![0u32; count2 as usize];
    let mut states2 = vec![0u8; count2 as usize];
    v.enumerate_read(
        gen2,
        &mut names2,
        &mut offs2,
        &mut sizes2,
        &mut crcs2,
        &mut states2,
    )
    .unwrap();
    assert_eq!(states2[2], ST_ACTIVE);
}

/// 2 万条目规模：open + enumerate 索引构建（验收预算 <100ms release，这里只验正确性）。
#[test]
fn scale_20k_entries() {
    let td = TempDir::new();
    {
        let v = Vfs::open(&td.0).unwrap();
        for i in 0..20_000u32 {
            seq_write(&v, &format!("f{i}.bin"), &vec![i as u8; 16]).unwrap();
        }
        v.flush().unwrap();
    }
    let v = Vfs::open(&td.0).unwrap();
    assert_eq!(v.get_generation(), 1 + 40_000); // 每文件 alloc+commit 各一拍
    let (gen, count, blen) = v.enumerate_query().unwrap();
    assert_eq!(count, 20_000);
    let mut names = vec![0u8; blen as usize];
    let mut offs = vec![0u64; count as usize];
    let mut sizes = vec![0u64; count as usize];
    let mut crcs = vec![0u32; count as usize];
    let mut states = vec![0u8; count as usize];
    v.enumerate_read(
        gen,
        &mut names,
        &mut offs,
        &mut sizes,
        &mut crcs,
        &mut states,
    )
    .unwrap();
    let ia = v.lookup("f0.bin").unwrap();
    assert_eq!(ia.crc, format::crc32(&[0u8; 16]));
    assert_eq!(v.lookup("f19999.bin").unwrap().size, 16);
}

#[test]
fn alloc_overflow_preserves_deleted_entry_and_index() {
    let td = TempDir::new();
    let v = Vfs::open(&td.0).unwrap();
    seq_write(&v, "keep", &[7]).unwrap();
    v.delete("keep").unwrap();
    let gen = v.get_generation();
    let logical = v.stat().logical;
    assert!(matches!(
        v.alloc("keep", u64::MAX),
        Err(VfsError::WriteOverflow)
    ));
    assert_eq!(v.lookup("keep").unwrap().state, ST_DELETED);
    assert_eq!(v.get_generation(), gen);
    assert_eq!(v.stat().logical, logical);
    {
        let mut g = v.shared.inner.lock().unwrap();
        g.idx.logical = u64::MAX - 4095;
    }
    assert!(matches!(
        v.alloc("keep", 4096),
        Err(VfsError::WriteOverflow)
    ));
    assert_eq!(v.lookup("keep").unwrap().state, ST_DELETED);
    assert_eq!(v.get_generation(), gen);
    let g = v.shared.inner.lock().unwrap();
    assert_eq!(g.writers, 0);
    assert_eq!(g.idx.entries.len(), 1);
}

#[test]
fn open_rejects_unrecoverable_header_without_mutation() {
    let td = TempDir::new();
    let hp = td.0.join("header.vfs");
    let dp = td.0.join("files.vfs");
    fs::write(&dp, b"payload").unwrap();
    for header in [b"".as_slice(), b"broken header".as_slice()] {
        fs::write(&hp, header).unwrap();
        assert!(matches!(Vfs::open(&td.0), Err(VfsError::Io)));
        assert_eq!(fs::read(&hp).unwrap(), header);
        assert_eq!(fs::read(&dp).unwrap(), b"payload");
    }
}

#[test]
fn truncated_append_recovers_on_open_and_same_handle_retry() {
    for (reopen, cut) in [(false, 17), (true, 17), (false, 40), (true, 40)] {
        let td = TempDir::new();
        let mut v = Vfs::open(&td.0).unwrap();
        seq_write(&v, "keep", &[1]).unwrap();
        v.flush().unwrap();
        let hp = td.0.join("header.vfs");
        let end = fs::metadata(&hp).unwrap().len();
        let mut entries = [Entry {
            name_off: 0,
            name_len: 1,
            state: ST_ACTIVE,
            offset: 0,
            size: 1,
            crc: 0,
        }];
        let img = format::build_region(99, 4096, &mut entries, b"x\0");
        let hf = OpenOptions::new().write(true).open(&hp).unwrap();
        // Model the partial bytes left by a failed append write.
        pwrite(&hf, end, &img.bytes[..cut]).unwrap();
        drop(hf);
        if reopen {
            drop(v);
            v = Vfs::open(&td.0).unwrap();
            assert_eq!(fs::metadata(&hp).unwrap().len(), end);
        }
        seq_write(&v, "later-longer-name", &[2]).unwrap();
        v.flush().unwrap();
        drop(v);
        let v = Vfs::open(&td.0).unwrap();
        assert!(v.lookup("keep").is_ok());
        assert!(v.lookup("later-longer-name").is_ok());
    }
}

#[test]
fn recovery_preserves_complete_bad_slot_and_later_valid_slot() {
    let td = TempDir::new();
    let v = Vfs::open(&td.0).unwrap();
    seq_write(&v, "keep", &[1]).unwrap();
    v.flush().unwrap();
    drop(v);
    let hp = td.0.join("header.vfs");
    let mut bytes = fs::read(&hp).unwrap();
    let mut bad = format::build_region(90, 0, &mut [], &[]).bytes;
    bad[16] ^= 1;
    let bad_off = bytes.len();
    bytes.extend_from_slice(&bad);
    let later = format::build_region(100, 0, &mut [], &[]).bytes;
    bytes.extend_from_slice(&later);
    bytes[..64].copy_from_slice(&format::sb_image(100));
    let end = bytes.len();
    bytes.extend_from_slice(&[0; 7]);
    fs::write(&hp, &bytes).unwrap();
    let v = Vfs::open(&td.0).unwrap();
    assert_eq!(v.get_generation(), 100);
    let recovered = fs::read(&hp).unwrap();
    assert_eq!(recovered.len(), end);
    assert_eq!(&recovered[bad_off..bad_off + bad.len()], &bad);
    seq_write(&v, "next", &[3]).unwrap();
    v.flush().unwrap();
    drop(v);
    assert!(Vfs::open(&td.0).unwrap().lookup("next").is_ok());
}

#[test]
fn compact_reserve_overflow_does_not_publish_or_move() {
    let td = TempDir::new();
    let v = Vfs::open(&td.0).unwrap();
    seq_write(&v, "gap", &[1]).unwrap();
    seq_write(&v, "keep", &[4; 8192]).unwrap();
    v.delete("gap").unwrap();
    let before = fs::read(td.0.join("header.vfs")).unwrap();
    let data = data_of(&td.0);
    assert!(matches!(
        v.compact_with(u64::MAX, None, None, 0),
        Err(VfsError::InvalidArg)
    ));
    // Fits the old logical size, but not this round's append destination.
    assert!(matches!(
        v.compact_with(u64::MAX - v.stat().logical, None, None, 0),
        Err(VfsError::InvalidArg)
    ));
    assert_eq!(v.compact_status().0, COMPACT_IDLE);
    assert_eq!(fs::read(td.0.join("header.vfs")).unwrap(), before);
    assert_eq!(data_of(&td.0), data);
    assert_eq!(v.lookup("keep").unwrap().offset, 4096);
}

// --- T2 ---

/// Scan 两指针三种规则：原地 / 前移进空洞 / 搬到旧逻辑末尾之后；尾部大空洞不搬。
#[test]
fn plan_two_pointer_rules() {
    let e = |off: u64, size: u64| Entry {
        name_off: 0,
        name_len: 0,
        state: ST_ACTIVE,
        offset: off,
        size,
        crc: 0,
    };
    // 首部空洞 → 前移进空洞
    let p = compact::build_plan(vec![("a".into(), e(4096, 4096))], 8192).unwrap();
    assert_eq!(p.items[0].new_off, 0);
    assert_eq!(p.new_logical, 4096);
    // gap < size → 搬到旧逻辑末尾之后；前一条 offset==cursor 原地不动
    let p = compact::build_plan(
        vec![("a".into(), e(0, 4096)), ("b".into(), e(12288, 12288))],
        24576,
    )
    .unwrap();
    assert_eq!(p.items[0].new_off, 0);
    assert_eq!(p.items[1].new_off, 24576);
    assert_eq!(p.new_logical, 24576 + 12288);
    assert!(p.excluded_any);
    // 尾部大空洞 → 不搬
    let p = compact::build_plan(vec![("a".into(), e(0, 4096))], 1 << 20).unwrap();
    assert_eq!(p.items[0].new_off, 0);
    assert_eq!(p.copy_total, 0);
}

/// R02：case-A 目标命中已搬条目的旧源区 → 排挤到末尾（崩溃安全：
/// 磁盘 header 下源区仍是 Active，翻转前必须完好）。
#[test]
fn plan_case_a_never_targets_moved_source() {
    let e = |off: u64| Entry {
        name_off: 0,
        name_len: 0,
        state: ST_ACTIVE,
        offset: off,
        size: 4096,
        crc: 0,
    };
    // a@0 原地；b@4096 已删除（不进 plan）；c@8192 前移进 b 的已删区；
    // d@12288 的候选 [8192,12288) 是 c 的旧源区 → 排挤到旧逻辑末尾之后。
    let p = compact::build_plan(
        vec![
            ("a".into(), e(0)),
            ("c".into(), e(8192)),
            ("d".into(), e(12288)),
        ],
        16384,
    )
    .unwrap();
    assert_eq!(p.items[0].new_off, 0);
    assert_eq!(p.items[1].new_off, 4096);
    assert_eq!(p.items[2].new_off, 16384);
    assert!(p.excluded_any);
    assert_eq!(p.copy_total, 8192);
}

/// 三种删除布局（首部空洞 / 中部小空洞 / 尾部大空洞）整理后无空洞、
/// offset 正确、数据 crc 校验通过、物理大小 == 新逻辑 + reserve。
#[test]
fn compact_three_layouts() {
    let td = TempDir::new();
    let v = Vfs::open(&td.0).unwrap();
    let mk = |i: u8| vec![i; 4096];
    for i in 1..=5u8 {
        seq_write(&v, &format!("f{i}"), &mk(i)).unwrap();
    }
    v.delete("f1").unwrap(); // 首部空洞
    v.delete("f3").unwrap(); // 中部空洞
    v.delete("f5").unwrap(); // 尾部大空洞
    v.compact_with(8192, None, None, 0).unwrap();
    wait_idle(&v);
    assert_eq!(v.compact_status(), (COMPACT_IDLE, 100));

    let s = v.stat();
    assert_eq!(s.logical, 2 * 4096);
    assert_eq!(s.active, 2 * 4096);
    assert_eq!(s.garbage, 0);
    assert_eq!(s.physical, s.logical + 8192); // reserve_extra 生效
    let i2 = v.lookup("f2").unwrap();
    let i4 = v.lookup("f4").unwrap();
    assert_eq!(i2.offset, 0);
    assert_eq!(i4.offset, 4096);
    let raw = data_of(&td.0);
    assert_eq!(&raw[0..4096], &mk(2)[..]);
    assert_eq!(&raw[4096..8192], &mk(4)[..]);
    assert_eq!(i4.crc, format::crc32(&mk(4)));
}

/// gap < size 的条目搬到旧逻辑末尾之后；offset==cursor 原地不动。
#[test]
fn compact_move_beyond_end() {
    let td = TempDir::new();
    let v = Vfs::open(&td.0).unwrap();
    let d1 = vec![1u8; 4096];
    let d2 = vec![2u8; 8192];
    let d3: Vec<u8> = (0..12288u32).map(|i| (i % 253) as u8).collect();
    seq_write(&v, "f1", &d1).unwrap();
    seq_write(&v, "f2", &d2).unwrap();
    seq_write(&v, "f3", &d3).unwrap();
    v.delete("f2").unwrap(); // f3 前空 8192 < 12288 → 搬到旧逻辑末尾(24576)之后
    v.compact_with(0, None, None, 0).unwrap();
    wait_idle(&v);

    assert_eq!(v.lookup("f1").unwrap().offset, 0); // 原地
    assert_eq!(v.lookup("f3").unwrap().offset, 4096);
    let s = v.stat();
    assert_eq!(s.logical, 16384);
    assert_eq!(s.physical, s.logical);
    let raw = data_of(&td.0);
    assert_eq!(&raw[4096..16384], &d3[..]);
}

/// BUSY 并发规则：有 writer → BUSY_WRITERS；整理期间 lookup/alloc/delete/flush 全
/// BUSY_COMPACTING，stat/get_generation/compact_status 不受限；结束后恢复正常。
#[test]
fn compact_busy_rules() {
    let td = TempDir::new();
    let v = Vfs::open(&td.0).unwrap();
    let big = vec![5u8; 16 << 20]; // 16MB，保证 Move 有窗口
    seq_write(&v, "big", &big).unwrap();

    let w = v.alloc("w", 100).unwrap();
    assert!(matches!(v.compact_with(0, None, None, 0), Err(VfsError::BusyWriters)));
    w.abort();

    v.compact_with(0, None, None, 0).unwrap();
    wait_moving(&v);
    assert!(matches!(v.compact_with(0, None, None, 0), Err(VfsError::BusyCompacting)));
    assert!(matches!(v.lookup("big"), Err(VfsError::BusyCompacting)));
    assert!(matches!(v.alloc("n", 10), Err(VfsError::BusyCompacting)));
    assert!(matches!(v.delete("big"), Err(VfsError::BusyCompacting)));
    assert!(matches!(v.flush(), Err(VfsError::BusyCompacting)));
    let _ = v.stat(); // 不受限
    let _ = v.get_generation();
    let _ = v.compact_status();
    wait_idle(&v);

    assert!(v.lookup("big").is_ok());
    seq_write(&v, "n", &[6u8; 10]).unwrap();
    v.delete("n").unwrap();
}

/// crc 损坏文件在整理中被标 Bad 并在 Finalize 剔除；其余文件完好。
#[test]
fn compact_marks_bad_crc() {
    let td = TempDir::new();
    let v = Vfs::open(&td.0).unwrap();
    let d1 = vec![1u8; 4096];
    let d2 = vec![2u8; 4096];
    seq_write(&v, "f1", &d1).unwrap();
    seq_write(&v, "f2", &d2).unwrap();
    // 绕过 writer 直接破坏 f2 的磁盘数据
    let i2 = v.lookup("f2").unwrap();
    let df = OpenOptions::new()
        .write(true)
        .open(td.0.join("files.vfs"))
        .unwrap();
    pwrite(&df, i2.offset, &[0u8; 64]).unwrap();
    drop(df);

    v.compact_with(0, None, None, 0).unwrap();
    wait_idle(&v);
    assert!(matches!(v.lookup("f2"), Err(VfsError::NotFound))); // Finalize 剔除
    let i1 = v.lookup("f1").unwrap();
    assert_eq!(i1.offset, 0);
    let s = v.stat();
    assert_eq!(s.active, format::align4k(4096));
    assert_eq!(s.physical, s.logical);
    let raw = data_of(&td.0);
    assert_eq!(&raw[0..4096], &d1[..]);
}

/// 整理后重开索引完好，可再次整理成功（翻转后状态自洽的验证）。
#[test]
fn compact_twice_with_reopen() {
    let td = TempDir::new();
    let mk = |i: u8| vec![i; 4096];
    {
        let v = Vfs::open(&td.0).unwrap();
        seq_write(&v, "f1", &mk(1)).unwrap();
        seq_write(&v, "f2", &mk(2)).unwrap();
        seq_write(&v, "f3", &mk(3)).unwrap();
        v.delete("f2").unwrap();
        v.compact_with(0, None, None, 0).unwrap();
        wait_idle(&v);
    }
    {
        let v = Vfs::open(&td.0).unwrap();
        assert_eq!(v.lookup("f1").unwrap().offset, 0); // 原地
        assert_eq!(v.lookup("f3").unwrap().offset, 4096);
        let raw = data_of(&td.0);
        assert_eq!(&raw[4096..8192], &mk(3)[..]);
        v.delete("f1").unwrap();
        v.compact_with(0, None, None, 0).unwrap();
        wait_idle(&v);
    }
    let v = Vfs::open(&td.0).unwrap();
    let i3 = v.lookup("f3").unwrap();
    assert_eq!(i3.offset, 0);
    assert_eq!(v.stat().logical, 4096);
    let raw = data_of(&td.0);
    assert_eq!(&raw[0..4096], &mk(3)[..]);
}

// --- 本轮评审回归 ---

/// slot_cap 新格式：等长、不同 framing 的槽按容量安全复用——覆写内容不改变
/// 走槽长度（cap），撕裂写也不会错位后续槽位；旧格式（按内容长度走槽）才
/// 依赖 framing 不变（cap=0 的旧格式槽仍走 Twin 路径的 framing 相同检查）。
#[test]
fn equal_size_slots_with_different_framing_are_capacity_reused() {
    let td = TempDir::new();
    let v = Vfs::open(&td.0).unwrap();
    let old = v.alloc(&"x".repeat(37), 0).unwrap();
    v.flush().unwrap();
    let old_slot = v.shared.layout.lock().unwrap().active;
    old.abort();
    let first = v.alloc("a", 0).unwrap();
    let second = v.alloc("bbb", 0).unwrap();
    v.flush().unwrap();
    let active = v.shared.layout.lock().unwrap().active;
    assert_eq!(old_slot.len, active.len);
    first.commit().unwrap();
    second.commit().unwrap();
    let hp = td.0.join("header.vfs");
    let before = fs::read(&hp).unwrap();
    v.flush().unwrap();
    let after = fs::read(&hp).unwrap();
    // 新内容（2 条目 + "a/bbb" 名字）与该槽等长、framing 不同 → 按容量原位
    // 复用，header 不再增长。
    assert_eq!(after.len(), before.len());
    drop(v);
    let reopened = Vfs::open(&td.0).unwrap();
    assert_eq!(reopened.lookup("a").unwrap().state, ST_ACTIVE);
    assert_eq!(reopened.lookup("bbb").unwrap().state, ST_ACTIVE);
}

/// R05 演化：更小的 region 复用更大的槽位后，槽内出现死尾巴——旧格式按内容
/// 长度走槽会被死尾卡死（其后槽位不可达）；slot_cap 新格式按容量走槽，重开
/// 扫描必须仍然可达、内容一致。
#[test]
fn smaller_region_reuses_larger_slot_and_reopen_stays_reachable() {
    let td = TempDir::new();
    {
        let v = Vfs::open(&td.0).unwrap();
        let long = "L".repeat(3000);
        for i in 0..20u8 {
            seq_write(&v, &format!("{long}{i}"), &[i; 16]).unwrap();
        }
        v.flush().unwrap(); // region1：~62KB 大槽
        for i in 1..20u8 {
            v.delete(&format!("{long}{i}")).unwrap();
        }
        seq_write(&v, "mid", &[9u8; 16]).unwrap();
        v.flush().unwrap(); // 小 region（旧代码写进大槽留 ~59KB 死尾）
        seq_write(&v, &"Z".repeat(60_000), &[1u8; 16]).unwrap();
        v.flush().unwrap(); // 更大 → 追加到死尾之后（旧代码重开时扫不到）
    }
    let v = Vfs::open(&td.0).unwrap();
    assert!(v.lookup(&"Z".repeat(60_000)).is_ok());
    assert!(v.lookup("mid").is_ok());
}

/// R01/R06：alloc/abort 也 bump generation（两段式间任何索引变化都 GEN_CHANGED，
/// 否则 alloc 长名后按旧 gen 读会按新鲜 blen 越界写调用方缓冲）；名字 blob 对外
/// 为紧凑布局，死字节不外漏，名字与数组按下标严格配对。
#[test]
fn enumerate_gen_covers_alloc_abort_and_compact_blob() {
    let td = TempDir::new();
    let v = Vfs::open(&td.0).unwrap();
    seq_write(&v, "alpha", &[1u8; 10]).unwrap();
    let w = v.alloc("beta", 10).unwrap(); // Downloading（alloc bump gen）
    let (gen, count, blen) = v.enumerate_query().unwrap();
    assert_eq!(count, 2);

    // abort 后按旧 gen 读 → GEN_CHANGED（条目数变了）
    w.abort();
    assert!(matches!(
        v.enumerate_read(
            gen,
            &mut vec![0u8; blen as usize],
            &mut vec![0u64; 2],
            &mut vec![0u64; 2],
            &mut vec![0u32; 2],
            &mut vec![0u8; 2]
        ),
        Err(VfsError::GenChanged)
    ));

    // 长名 alloc 后仍按旧 gen 读 → GEN_CHANGED（修复前：blen 变大 → 越界写）
    let w2 = v.alloc(&"x".repeat(4096), 10).unwrap();
    assert!(matches!(
        v.enumerate_read(
            gen,
            &mut vec![0u8; blen as usize],
            &mut vec![0u64; 2],
            &mut vec![0u64; 2],
            &mut vec![0u32; 2],
            &mut vec![0u8; 2]
        ),
        Err(VfsError::GenChanged)
    ));
    w2.abort();

    // 紧凑 blob：abort 留下的死字节（"beta"、长名）不外漏，严格按序配对
    seq_write(&v, "gamma", &[3u8; 10]).unwrap();
    let (gen2, count2, blen2) = v.enumerate_query().unwrap();
    let mut names = vec![0u8; blen2 as usize];
    let mut offs = vec![0u64; count2 as usize];
    let mut sizes = vec![0u64; count2 as usize];
    let mut crcs = vec![0u32; count2 as usize];
    let mut states = vec![0u8; count2 as usize];
    v.enumerate_read(
        gen2,
        &mut names,
        &mut offs,
        &mut sizes,
        &mut crcs,
        &mut states,
    )
    .unwrap();
    let got: Vec<&str> = names
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| std::str::from_utf8(s).unwrap())
        .collect();
    assert_eq!(got, vec!["alpha", "gamma"]);
}

/// R08 / Q11：优雅 close 落盘——未 flush 的 commit 在 close 后重开可见。
#[test]
fn close_flushes_pending_commits() {
    let td = TempDir::new();
    let dir = CString::new(td.0.to_str().unwrap()).unwrap();
    let h = unsafe { vfs_open(dir.as_ptr()) };
    assert!(!h.is_null());
    let v = unsafe { (h as *const Vfs).as_ref().unwrap() };
    seq_write(v, "f1", &[1u8; 10]).unwrap(); // 不显式 flush
    unsafe { vfs_close(h) }; // 优雅 close：best-effort flush
    let v2 = Vfs::open(&td.0).unwrap();
    assert!(v2.lookup("f1").is_ok());
}

/// 路径式打开：自定义文件名成对可用（含父目录按需创建），索引/数据异目录，
/// 目录版薄壳在固定名 header.vfs/files.vfs 上独立成库互不串扰。
#[test]
fn open_paths_custom_names() {
    let td = TempDir::new();
    let idx = td.0.join("sub1").join("base.idx");
    let dat = td.0.join("sub2").join("base.dat");
    let v = Vfs::open_paths(&idx, &dat).unwrap();
    seq_write(&v, "f1", &[7u8; 10]).unwrap();
    v.flush().unwrap();
    drop(v);
    assert!(idx.exists() && dat.exists());
    let v2 = Vfs::open_paths(&idx, &dat).unwrap();
    assert_eq!(v2.lookup("f1").unwrap().size, 10);
    // 目录版薄壳：固定名是另一对文件，看不到自定义库的内容
    let v3 = Vfs::open(&td.0).unwrap();
    assert!(v3.lookup("f1").is_err());
}

/// 路径式打开防呆：同一路径（含大小写/空白差异）与空路径拒绝（InvalidArg）。
#[test]
fn open_paths_rejects_bad_args() {
    let td = TempDir::new();
    let p = td.0.join("same.vfs");
    assert!(matches!(Vfs::open_paths(&p, &p), Err(VfsError::InvalidArg)));
    assert!(matches!(
        Vfs::open_paths(&p, &td.0.join("SAME.VFS")),
        Err(VfsError::InvalidArg)
    ));
    assert!(matches!(
        Vfs::open_paths(Path::new(""), Path::new("x")),
        Err(VfsError::InvalidArg)
    ));
    assert!(matches!(
        Vfs::open_paths(Path::new("x"), Path::new("  ")),
        Err(VfsError::InvalidArg)
    ));
}

/// FFI vfs_open_paths：导出可用、失败返回 NULL（目录版 vfs_open 的 FFI
/// 语义由 close_flushes_pending_commits 覆盖）。
#[test]
fn ffi_open_paths() {
    let td = TempDir::new();
    let i = CString::new(td.0.join("i.vfs").to_str().unwrap()).unwrap();
    let d = CString::new(td.0.join("d.vfs").to_str().unwrap()).unwrap();
    let h = unsafe { vfs_open_paths(i.as_ptr(), d.as_ptr()) };
    assert!(!h.is_null());
    unsafe { vfs_close(h) }; // 先 close：Windows 下打开的句柄会挡住 TempDir 清理
                             // 防呆经 FFI：同路径 → NULL（顺带钉死 index/data 参数顺序不颠倒）
    let same = CString::new(td.0.join("x.vfs").to_str().unwrap()).unwrap();
    assert!(unsafe { vfs_open_paths(same.as_ptr(), same.as_ptr()) }.is_null());
    assert!(unsafe { vfs_open_paths(ptr::null(), d.as_ptr()) }.is_null());
}

#[test]
fn flush_reuses_bigger_free_slot_instead_of_growing_header() {
    let td = TempDir::new();
    let v = Vfs::open(&td.0).unwrap();
    seq_write(&v, "a", &[1; 200]).unwrap();
    v.flush().unwrap(); // 小 region
    seq_write(&v, "b", &[2; 8000]).unwrap();
    v.flush().unwrap(); // 大 region（cap 大）
    v.delete("b").unwrap();
    v.flush().unwrap(); // 内容缩回：应复用大容量空闲槽而非尾部追加
    let hp = td.0.join("header.vfs");
    let stable = fs::metadata(&hp).unwrap().len();
    for _ in 0..6 {
        v.flush().unwrap();
        assert_eq!(
            fs::metadata(&hp).unwrap().len(),
            stable,
            "header.vfs 不应继续增长"
        );
    }
    let v = Vfs::open(&td.0).unwrap();
    assert!(v.lookup("a").is_ok());
    // lookup 对 Deleted 条目返回 Ok（带 state）——断言 b 已成 Deleted 而非 Active。
    assert_eq!(v.lookup("b").unwrap().state, ST_DELETED);
}

#[test]
fn legacy_region_without_slot_cap_still_scans() {
    let td = TempDir::new();
    let v = Vfs::open(&td.0).unwrap();
    seq_write(&v, "a", &[1; 64]).unwrap();
    v.flush().unwrap();
    drop(v);
    // 把现役 region 退化为旧格式（slot_cap=0）：旧文件必须继续可扫可开。
    let hp = td.0.join("header.vfs");
    let mut bytes = fs::read(&hp).unwrap();
    let mut region = format::build_region(2, 4096, &mut Vec::new(), &[]);
    region.bytes[28..32].copy_from_slice(&0u32.to_le_bytes());
    bytes.truncate(format::SB_SIZE as usize);
    bytes.extend_from_slice(&region.bytes);
    bytes[..64].copy_from_slice(&format::sb_image(2));
    fs::write(&hp, &bytes).unwrap();
    let v = Vfs::open(&td.0).unwrap();
    assert!(v.lookup("a").is_err()); // 空 index：旧条目已不在（本测试只验格式兼容）
    v.flush().unwrap();
    assert!(Vfs::open(&td.0).unwrap().lookup("a").is_err());
}

/// 回归（panic 规则）：stat 遇溢出对齐的 size 须饱和而非 panic。
/// 修复前 `format::align4k(u64::MAX)` → checked_align4k 为 None → expect panic。
/// 正常写入路径有 WriteOverflow 拦截，超界条目只能内部注入。
#[test]
fn stat_saturates_on_huge_entry_size() {
    let td = TempDir::new();
    let v = Vfs::open(&td.0).unwrap();
    v.shared
        .inner
        .lock()
        .unwrap()
        .idx
        .entries
        .push(format::Entry {
            name_off: 0,
            name_len: 0,
            state: format::ST_ACTIVE,
            offset: 0,
            size: u64::MAX,
            crc: 0,
        });
    let s = v.stat();
    assert_eq!(s.active, u64::MAX);
    assert_eq!(s.garbage, 0);
}
