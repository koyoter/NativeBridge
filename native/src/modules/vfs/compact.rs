// vfs/compact.rs — 异步碎片整理状态机 Idle → Scan → Move → Finalize → Idle（设计 §2.3）。
//
// Move 规则（§1.1-5）：Active 条目按 offset 升序两指针；
//   offset == cursor          → 原地不动（仅读校验 crc，不搬）；
//   gap ≥ align4k(size)       → 前移进空洞——空洞只允许是 Deleted/Bad 区或真垃圾，
//                               不得命中任何已搬条目的旧源区（Move 前已把当前索引
//                               落盘，源区在磁盘 header 下仍是 Active，翻转前必须
//                               完好；命中则降级为搬到末尾，excluded_any 置位）；
//   否则                      → 搬到当前数据末尾之后（≥ 旧逻辑末尾）。
// 单次拷贝自身不重叠写；崩溃安全 = "Move 前落盘 + 目标不覆盖任何源区 + header 只在
// Finalize 翻转一次"：翻转前旧 header 引用的数据完好，翻转后新 header 生效。
//
// 被排挤（excluded_any）或发现 Bad 的轮次会留下可回收的洞 → 再跑一轮 Scan→Move→
// Finalize（上限 8 轮）直到收敛；每轮 Finalize 都翻转一次 header，percent 逐轮回摆。
//
// Finalize：fsync files.vfs → 新 region 一次性翻转（剔除 Deleted/Bad、全部新 offset、
// 新逻辑大小）→ set_len(新逻辑 + reserve)（截垃圾 + 预扩容，失败无害）。
// 顺序与设计 §2.3 一致：先翻转后截断——反序会在崩溃窗口内截掉旧 header 引用的数据
// （reserve=0 且末尾搬移条目的旧源区超出新逻辑 / 全删光 new_logical=0 时）。

use std::fs::{File, OpenOptions};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use libc::{c_char, c_int, c_void};

use super::format::{self, Entry, ST_ACTIVE, ST_BAD};
use super::{ioerr, VfsError, VfsShared, COMPACT_FINALIZE, COMPACT_IDLE, COMPACT_MOVE, COMPACT_SCAN};

/// Move 分块大小（~4MB）。
const CHUNK: usize = 4 << 20;

pub(crate) type ProgressCb = unsafe extern "C" fn(user: *mut c_void, percent: c_int, name: *const c_char);
pub(crate) type DoneCb = unsafe extern "C" fn(user: *mut c_void, err: c_int);

pub(crate) struct PlanItem {
    pub(crate) name: String,
    pub(crate) old_off: u64,
    pub(crate) new_off: u64,
    pub(crate) size: u64,
    pub(crate) crc: u32,
}

pub(crate) struct Plan {
    pub(crate) items: Vec<PlanItem>,
    /// 剔除坏块前的新逻辑大小。
    pub(crate) new_logical: u64,
    /// 需要实际搬移的字节数（进度分母）。
    pub(crate) copy_total: u64,
    /// 有 case-A 因命中已搬源区被排挤到末尾 → 本轮留下下一轮可回收的洞。
    pub(crate) excluded_any: bool,
}

/// Scan：Active 条目按 §1.1-5 分配新 offset。纯函数，便于单测。
pub(crate) fn build_plan(mut actives: Vec<(String, Entry)>, old_logical: u64) -> Plan {
    actives.sort_by_key(|(_, e)| e.offset);
    let mut cursor = 0u64;
    let mut data_end = old_logical;
    let mut new_logical = 0u64;
    let mut items = Vec::with_capacity(actives.len());
    // 已搬条目的旧源区：case-A 不得落入。Move 开始前索引已落盘，源区在磁盘 header
    // 下仍是 Active——覆盖它会让"翻转前旧 header 恒成立"的崩溃安全承诺失效。
    let mut moved_sources: Vec<(u64, u64)> = Vec::new();
    let mut excluded_any = false;
    for (name, e) in actives {
        let a = format::align4k(e.size);
        let target = if e.offset == cursor {
            cursor += a;
            e.offset
        } else {
            // 候选 [cursor, cursor+a) 与任一已搬源区相交 → 排挤到末尾
            let hits_moved = moved_sources.iter().any(|&(s, t)| cursor < t && s < cursor + a);
            if !hits_moved && e.offset - cursor >= a {
                let t = cursor;
                cursor += a;
                t
            } else {
                if hits_moved {
                    excluded_any = true;
                }
                let t = data_end;
                data_end += a;
                t
            }
        };
        if target != e.offset {
            moved_sources.push((e.offset, e.offset + a));
        }
        new_logical = new_logical.max(target + a);
        items.push(PlanItem { name, old_off: e.offset, new_off: target, size: e.size, crc: e.crc });
    }
    let copy_total = items.iter().filter(|i| i.new_off != i.old_off).map(|i| i.size).sum();
    Plan { items, new_logical, copy_total, excluded_any }
}

/// 由 Vfs::compact 在持有 inner 锁、置位 compacting 之后调用。
/// 回调以裸函数指针 + usize user 跨线程传递（fn 指针本身 Send+Sync）。
pub(crate) fn start(
    shared: Arc<VfsShared>,
    reserve_extra: u64,
    progress: Option<ProgressCb>,
    done: Option<DoneCb>,
    user: usize,
) -> Result<(), VfsError> {
    shared.ctl.state.store(COMPACT_SCAN, Ordering::Relaxed);
    let spawned = std::thread::Builder::new()
        .name("vfs-compact".into())
        .spawn(move || run(shared, reserve_extra, progress, done, user));
    spawned.map(|_| ()).map_err(ioerr)
}

fn run(shared: Arc<VfsShared>, reserve_extra: u64, progress: Option<ProgressCb>, done: Option<DoneCb>, user: usize) {
    let code = match compact_run(&shared, reserve_extra, progress, user) {
        Ok(()) => 0,
        Err(e) => e.as_i32(),
    };
    {
        let mut g = shared.inner.lock().unwrap();
        g.compacting = false;
    }
    shared.ctl.state.store(COMPACT_IDLE, Ordering::Relaxed);
    if let Some(f) = done {
        unsafe { f(user as *mut c_void, code as c_int) };
    }
}

fn compact_run(shared: &VfsShared, reserve_extra: u64, progress: Option<ProgressCb>, user: usize) -> Result<(), VfsError> {
    // Move 前把当前索引（含未 flush 的 delete）落盘：磁盘 header 与内存一致后，
    // Move 覆盖"已删/垃圾"区才不破坏旧 header 下的 Active 源数据（崩溃安全前提）。
    super::flush_image(shared)?;
    for _ in 0..8 {
        // --- Scan ---
        shared.ctl.state.store(COMPACT_SCAN, Ordering::Relaxed);
        let (plan, old_gen) = {
            let g = shared.inner.lock().unwrap();
            let actives: Vec<(String, Entry)> = g
                .idx
                .entries
                .iter()
                .filter(|e| e.state == ST_ACTIVE)
                .map(|e| (g.idx.name_of(e).unwrap_or("").to_string(), *e))
                .collect();
            (build_plan(actives, g.idx.logical), g.idx.generation)
        };

        // --- Move ---
        shared.ctl.state.store(COMPACT_MOVE, Ordering::Relaxed);
        let rd = match File::open(&shared.data_path) {
            Ok(f) => f,
            Err(_) => return Err(VfsError::Io),
        };
        let wr = match OpenOptions::new().write(true).open(&shared.data_path) {
            Ok(f) => f,
            Err(_) => return Err(VfsError::Io),
        };
        let mut buf = vec![0u8; CHUNK];
        let mut copied = 0u64;
        let mut bad: Vec<String> = Vec::new();
        for item in &plan.items {
            // 原地条目不搬但仍读校验 crc（坏块检测覆盖全部 Active 文件）。
            let moved = item.new_off != item.old_off;
            match copy_verified(&rd, if moved { Some(&wr) } else { None }, item, &mut buf) {
                Ok(true) => {}
                Ok(false) => bad.push(item.name.clone()), // crc 不符 → 标 Bad，Finalize 剔除
                Err(e) => return Err(e),                   // IO 错误 → 放弃整理，旧 header 恒成立
            }
            if moved {
                copied += item.size;
                let pct = if plan.copy_total == 0 { 100 } else { (copied * 100 / plan.copy_total).min(100) };
                shared.ctl.percent.store(pct as u32, Ordering::Relaxed);
                fire_progress(progress, user, pct as c_int, &item.name);
            }
        }
        if !bad.is_empty() {
            let mut g = shared.inner.lock().unwrap();
            for n in &bad {
                if let Some(e) = g.idx.find_mut(n) {
                    e.state = ST_BAD;
                }
            }
        }

        // --- Finalize ---
        shared.ctl.state.store(COMPACT_FINALIZE, Ordering::Relaxed);
        fire_progress(progress, user, 100, "");
        shared.ctl.percent.store(100, Ordering::Relaxed);
        let mut entries: Vec<Entry> = Vec::new();
        let mut blob = Vec::new();
        for i in &plan.items {
            if bad.iter().any(|n| n == &i.name) {
                continue;
            }
            entries.push(Entry {
                name_off: blob.len() as u32,
                name_len: i.name.len() as u16,
                state: ST_ACTIVE,
                offset: i.new_off,
                size: i.size,
                crc: i.crc,
            });
            blob.extend_from_slice(i.name.as_bytes());
            blob.push(0);
        }
        let new_logical = entries.iter().map(|e| e.offset + format::align4k(e.size)).max().unwrap_or(0);
        // 数据先行落盘（§2.3 顺序：fsync files.vfs → 一次性翻转 → 截断/预扩容）。
        if wr.sync_all().is_err() {
            return Err(VfsError::Io);
        }
        let new_gen = old_gen + 1;
        let img = format::build_region(new_gen, new_logical, &mut entries, &blob);
        // 整理的 gen = 扫描时最高 gen + 1，不会命中水位线 GenChanged；任何错误按 Io 上报。
        if shared.persist_image(new_gen, &img.bytes).is_err() {
            return Err(VfsError::Io);
        }
        // 翻转成功后截垃圾+预扩容；失败无害（open 对逻辑外残留忽略 / 物理不足补齐），
        // 不因 set_len 失败把已完成的整理回报为失败。
        let _ = wr.set_len(new_logical + reserve_extra);
        // 翻转成功 → 换内存索引（generation 一并递增）。
        let names = img.bytes[img.blob_off..].to_vec();
        {
            let mut g = shared.inner.lock().unwrap();
            g.idx = super::index::Index::new(new_gen, new_logical, entries, names);
        }
        // 收敛判定：无排挤（无新增可回收的洞）且无 Bad 剔除 → 一轮即终。
        if !plan.excluded_any && bad.is_empty() {
            return Ok(());
        }
    }
    // 8 轮未收敛（理论防御）：布局自洽，剩余洞留待下次整理。
    Ok(())
}

/// 分块读并累计 crc32（wr = Some 时边读边写 = 搬移；None = 仅读校验原地条目）。
/// Ok(false) = 源数据 crc 不符（坏块）。
fn copy_verified(rd: &File, wr: Option<&File>, item: &PlanItem, buf: &mut [u8]) -> Result<bool, VfsError> {
    let mut raw = 0xFFFF_FFFFu32;
    let mut done = 0u64;
    while done < item.size {
        let n = ((item.size - done).min(buf.len() as u64)) as usize;
        super::pread(rd, item.old_off + done, &mut buf[..n]).map_err(|_| VfsError::Io)?;
        raw = format::crc32_update(raw, &buf[..n]);
        if let Some(w) = wr {
            super::pwrite(w, item.new_off + done, &buf[..n]).map_err(|_| VfsError::Io)?;
        }
        done += n as u64;
    }
    Ok(!raw == item.crc)
}

fn fire_progress(progress: Option<ProgressCb>, user: usize, percent: c_int, name: &str) {
    if let Some(f) = progress {
        let c = std::ffi::CString::new(name).unwrap_or_default();
        unsafe { f(user as *mut c_void, percent, c.as_ptr()) };
    }
}
