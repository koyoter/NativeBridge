// VFS 轨道（T1 格式核心 / T2 碎片整理 / T3 FFI）实现于此目录。
//
//! VFS：简化版虚拟文件容器（header.vfs 索引 + files.vfs 数据），设计 §2。
//!
//! * 磁盘格式 / 恢复：`format.rs`；内存索引：`index.rs`；碎片整理：`compact.rs`。
//! * 写入三段式 alloc → write_at → commit/abort；commit 后数据永不移动。
//! * 崩溃安全根基：整理目标不覆盖任何源数据 + header 只在 Finalize 翻转一次。
//! * 线程模型：`Vfs` 多线程共享（内部全 Mutex/原子）；单个 writer 限创建方使用（Send 即可）。
//! * FFI 惯例与 curlw 一致：UTF-8 `*const c_char`、opaque 句柄、错误码 i32、
//!   panic=abort 下 FFI 边界不做 catch_unwind。
//! * C ABI 权威契约：本目录 FFI 导出即 vfs ABI 的单一事实源（C# 镜像：csharp/Vfs.cs），
//!   改动既有签名须 bump VFS_ABI_VERSION，纯新增导出不 bump。同一 index/data 对
//!   只允许一个打开句柄：双实例会各自盲目翻转共享的 SuperBlock（宿主约定）。
pub(crate) mod compact;
pub(crate) mod format;
pub(crate) mod index;

use std::ffi::{CStr, CString};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use libc::{c_char, c_int, c_void};

#[cfg(windows)]
use std::os::windows::fs::FileExt;
#[cfg(not(windows))]
use std::os::unix::fs::FileExt;

use format::{Entry, SlotScan, ST_ACTIVE, ST_DELETED, ST_DOWNLOADING, SB_GEN_OFF, SB_SIZE};
use index::Index;

// pwrite 平台实现与 vfs/dlmgr 共用（modules/util.rs）。
pub(crate) use crate::modules::util::pwrite;

// --- 错误码（§2.4 顺序）---
pub(crate) const VFS_OK: i32 = 0; // IO=1 ...（其余见 VfsError::as_i32）

/// 整理状态机（vfs_compact_status 的 state）。
pub(crate) const COMPACT_IDLE: u32 = 0;
pub(crate) const COMPACT_SCAN: u32 = 1;
pub(crate) const COMPACT_MOVE: u32 = 2;
pub(crate) const COMPACT_FINALIZE: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VfsError {
    Io,
    NotFound,
    AlreadyExists,
    BusyCompacting,
    BusyWriters,
    WriteOverflow,
    GenChanged,
    BufferTooSmall,
    InvalidArg,
    StateInvalid,
}

impl VfsError {
    /// FFI 错误码映射（§2.4 顺序：OK=0 IO=1 NOT_FOUND=2 ALREADY_EXISTS=3
    /// BUSY_COMPACTING=4 BUSY_WRITERS=5 CRC_MISMATCH=6 WRITE_OVERFLOW=7
    /// GEN_CHANGED=8 BUFFER_TOO_SMALL=9 INVALID_ARG=10 STATE_INVALID=11；
    /// CRC_MISMATCH=6 为读取方预留，原生侧从不产生）。
    pub(crate) fn as_i32(&self) -> i32 {
        match self {
            VfsError::Io => 1,
            VfsError::NotFound => 2,
            VfsError::AlreadyExists => 3,
            VfsError::BusyCompacting => 4,
            VfsError::BusyWriters => 5,
            VfsError::WriteOverflow => 7,
            VfsError::GenChanged => 8,
            VfsError::BufferTooSmall => 9,
            VfsError::InvalidArg => 10,
            VfsError::StateInvalid => 11,
        }
    }
}

fn ioerr(_: io::Error) -> VfsError {
    VfsError::Io
}

/// 路径式打开的父目录按需创建；裸文件名（父目录为空）落在当前目录——
/// std 的 create_dir_all 对空路径直接返回 Ok，无需自己判断。
fn ensure_parent(file: &Path) -> Result<(), VfsError> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(ioerr)?;
    }
    Ok(())
}

fn code(r: Result<(), VfsError>) -> i32 {
    match r {
        Ok(()) => VFS_OK,
        Err(e) => e.as_i32(),
    }
}

// --- 平台 pread（绝对偏移，读满）---
// pwrite 在 modules/util.rs（vfs/dlmgr 共用）；每个使用者都持独占句柄
//（writer 各自持句柄），故无同句柄并发问题。

pub(crate) fn pread(f: &File, off: u64, buf: &mut [u8]) -> io::Result<()> {
    #[cfg(windows)]
    {
        let mut off = off;
        let mut done = 0usize;
        while done < buf.len() {
            let n = f.seek_read(&mut buf[done..], off)?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "vfs: seek_read read 0"));
            }
            done += n as usize;
            off += n as u64;
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        f.read_exact_at(buf, off)
    }
}

// --- 内部状态 ---

struct Inner {
    idx: Index,
    /// 存活 writer 数（含未 commit/abort 的）。
    writers: u32,
    compacting: bool,
}

pub(crate) struct CompactCtl {
    pub(crate) state: AtomicU32,
    pub(crate) percent: AtomicU32,
}

#[derive(Clone, Copy)]
struct Slot {
    off: u64,
    len: u64,
}

struct RegionLayout {
    active: Slot,
    inactive: Option<Slot>,
}

/// commit 回调：§2.4 `cb(user, name, off, size, crc)`。存裸 fn 指针（Send+Sync），user 以 usize 保存。
pub(crate) type VfsCommitFn = unsafe extern "C" fn(user: *mut c_void, name: *const c_char, off: u64, size: u64, crc: u32);

struct VfsShared {
    /// files.vfs 实际路径：writer/整理重开句柄用。目录版 open 拼固定名，
    /// 路径版 open_paths 原样传入（索引文件路径打开后只经 header_file 句柄
    /// 访问，无需保存）。写数据一律走 writer/整理的独立句柄。
    data_path: PathBuf,
    data_file: Mutex<File>,
    header_file: Mutex<File>,
    /// 序列化 region + SuperBlock 翻转的互斥（flush 与整理 Finalize 共用）。
    persist_lock: Mutex<()>,
    /// 已持久化的最高 generation 水位线：拦截滞后的旧 gen persist 把 SuperBlock
    /// 翻回旧 region（persist_lock 只防交错不防乱序）。
    persisted_gen: Mutex<u64>,
    inner: Mutex<Inner>,
    layout: Mutex<RegionLayout>,
    commit_cb: Mutex<Option<(VfsCommitFn, usize)>>,
    ctl: CompactCtl,
}

impl VfsShared {
    /// 写 inactive region（放不下则尾部追加）→ fsync header → 原地翻 SuperBlock.active_gen → fsync。
    /// 水位线防御：gen 低于已持久化水位线的滞后调用（旧 flush 与整理 Finalize 竞速）
    /// 返回 GenChanged，防止把 SuperBlock 翻回旧 region；等于水位线放行——同 gen 的
    /// 重复 flush 合法（写闲置槽 + 翻转无害），只有"倒退"才拦截。
    fn persist_image(&self, gen: u64, bytes: &[u8]) -> Result<(), VfsError> {
        let _pl = self.persist_lock.lock().unwrap();
        if gen < *self.persisted_gen.lock().unwrap() {
            return Err(VfsError::GenChanged);
        }
        let hf = self.header_file.lock().unwrap();
        let hlen = hf.metadata().map_err(ioerr)?.len();
        let mut layout = self.layout.lock().unwrap();
        // 仅等长复用：更小的 region 写进大槽会留死尾巴，重开时顺序扫描会卡在
        // 死字节上终止，其后追加的槽位全部不可达（gen 回退、新文件消失）。
        let dest = match layout.inactive {
            Some(s) if bytes.len() as u64 == s.len => s,
            _ => Slot { off: hlen, len: bytes.len() as u64 },
        };
        pwrite(&hf, dest.off, bytes).map_err(ioerr)?;
        hf.sync_all().map_err(ioerr)?;
        pwrite(&hf, SB_GEN_OFF, &format::sb_patch(gen)).map_err(ioerr)?;
        hf.sync_all().map_err(ioerr)?;
        let old = layout.active;
        layout.active = dest;
        layout.inactive = Some(old);
        // 翻转成功才推进水位线；失败保持原值，同 gen 可重试。
        *self.persisted_gen.lock().unwrap() = gen;
        Ok(())
    }

    /// commit 回调在锁外触发（允许回调内重入 VFS 查询）。
    fn fire_commit(&self, name: &str, off: u64, size: u64, crc: u32) {
        let cb = self.commit_cb.lock().unwrap().clone();
        if let Some((f, user)) = cb {
            let c = CString::new(name).unwrap_or_default();
            unsafe { f(user as *mut c_void, c.as_ptr(), off, size, crc) };
        }
    }
}

/// 索引落盘共用体：fsync files.vfs → build_region → persist_image（含翻转）。
/// `Vfs::flush`（拒绝整理中）与整理 Move 前的落盘共用；不做 compacting 检查。
pub(crate) fn flush_image(shared: &VfsShared) -> Result<(), VfsError> {
    let (mut entries, names, gen, logical) = {
        let g = shared.inner.lock().unwrap();
        (g.idx.entries.clone(), g.idx.names.clone(), g.idx.generation, g.idx.logical)
    };
    shared.data_file.lock().unwrap().sync_all().map_err(ioerr)?;
    let img = format::build_region(gen, logical, &mut entries, &names);
    shared.persist_image(gen, &img.bytes)
}

// --- 公共 API（glue 层使用）---

#[derive(Clone, Copy, Debug)]
pub(crate) struct VfsEntryInfo {
    pub(crate) offset: u64,
    pub(crate) size: u64,
    pub(crate) state: u8,
    pub(crate) crc: u32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct VfsStat {
    pub(crate) logical: u64,
    pub(crate) physical: u64,
    /// 全部登记条目的 4K 对齐跨度之和（Active+Deleted+Bad+Downloading）。
    pub(crate) total: u64,
    pub(crate) active: u64,
    pub(crate) deleted: u64,
    /// logical 中不属于任何条目的空洞（abort 区间、被替换的 Deleted、Downloading 残留）。
    pub(crate) garbage: u64,
}

pub(crate) struct Vfs {
    shared: Arc<VfsShared>,
}

impl Vfs {
    /// 打开（或初始化）一个 VFS 目录：拼固定名 header.vfs + files.vfs 后走
    /// open_paths（目录版薄壳，仅为兼容保留）。
    pub(crate) fn open(dir: &Path) -> Result<Vfs, VfsError> {
        Self::open_paths(&dir.join("header.vfs"), &dir.join("files.vfs"))
    }

    /// 打开（或初始化）一对 VFS 文件：index 为双缓冲索引（目录版叫 header.vfs），
    /// data 为 4K 对齐数据区（目录版叫 files.vfs），两者可任意命名/异目录。
    /// 恢复协议见设计 §2.2：SuperBlock 损坏扫描双 region 取 gen 最高且校验通过
    /// 者；Downloading 残留丢弃；逻辑大小之外的物理残留忽略。
    pub(crate) fn open_paths(index: &Path, data: &Path) -> Result<Vfs, VfsError> {
        // 防呆：空路径（含纯空白），或两路径指向同一文件（去空白 + 大小写折叠
        // 的字符串级比较，不做 symlink/硬链接规范化）——索引与数据同文件会让
        // 双缓冲索引 pwrite 到数据区上，毁库。
        let norm = |p: &Path| p.to_string_lossy().trim().to_lowercase();
        let (ni, nd) = (norm(index), norm(data));
        if ni.is_empty() || nd.is_empty() || ni == nd {
            return Err(VfsError::InvalidArg);
        }
        ensure_parent(index)?;
        ensure_parent(data)?;
        let data_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(data)
            .map_err(ioerr)?;
        let header_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(index)
            .map_err(ioerr)?;

        let header_bytes = std::fs::read(index).unwrap_or_default();
        let sb_gen = format::read_sb(&header_bytes);
        // 顺序扫描 region 槽位（头部不可读即终止——后续槽位无法定位）。
        let mut slots: Vec<SlotScan> = Vec::new();
        let mut pos = SB_SIZE as usize;
        while let Some(s) = format::scan_slot(&header_bytes, pos) {
            pos += s.total as usize;
            slots.push(s);
        }
        // 选 active：sb 有效时 gen 匹配者优先，否则取 gen 最高且校验通过者。
        let mut best: Option<&SlotScan> = None;
        for s in &slots {
            let Some(p) = &s.parsed else { continue };
            let better = match best.map(|b| b.parsed.as_ref().unwrap()) {
                None => true,
                Some(bp) => {
                    if Some(p.gen) == sb_gen && Some(bp.gen) != sb_gen {
                        true
                    } else if Some(bp.gen) == sb_gen {
                        false
                    } else {
                        p.gen > bp.gen
                    }
                }
            };
            if better {
                best = Some(s);
            }
        }

        let (generation, logical, entries, names, layout) = match best {
            Some(s) => {
                let p = s.parsed.as_ref().unwrap();
                // Downloading 残留：条目丢弃，区间成垃圾（逻辑大小不动 → stat.garbage 计入）。
                let entries: Vec<Entry> =
                    p.entries.iter().filter(|e| e.state != ST_DOWNLOADING).copied().collect();
                let active = Slot { off: s.off, len: s.total };
                let inactive = slots
                    .iter()
                    .filter(|o| o.off != s.off)
                    .max_by_key(|o| o.total)
                    .map(|o| Slot { off: o.off, len: o.total });
                (p.gen, p.logical, entries, p.names.clone(), RegionLayout { active, inactive })
            }
            None => {
                // 全新或不可恢复：重置 header.vfs（SB + 空 region，gen=1）。
                let img = format::build_region(1, 0, &mut Vec::new(), &[]);
                let mut buf = Vec::with_capacity(SB_SIZE as usize + img.bytes.len());
                buf.extend_from_slice(&format::sb_image(1));
                buf.extend_from_slice(&img.bytes);
                header_file.set_len(0).map_err(ioerr)?;
                pwrite(&header_file, 0, &buf).map_err(ioerr)?;
                header_file.sync_all().map_err(ioerr)?;
                (
                    1,
                    0,
                    Vec::new(),
                    Vec::new(),
                    RegionLayout { active: Slot { off: SB_SIZE, len: img.bytes.len() as u64 }, inactive: None },
                )
            }
        };

        // 物理小于逻辑（外部截断）则补齐；逻辑之外的物理残留忽略。
        if data_file.metadata().map(|m| m.len()).unwrap_or(0) < logical {
            data_file.set_len(logical).map_err(ioerr)?;
        }

        let shared = Arc::new(VfsShared {
            data_path: data.to_path_buf(),
            data_file: Mutex::new(data_file),
            header_file: Mutex::new(header_file),
            persist_lock: Mutex::new(()),
            // 水位线以打开时的 generation 起算（有 region 取 p.gen，全新为 1）。
            persisted_gen: Mutex::new(generation),
            inner: Mutex::new(Inner {
                idx: Index::new(generation, logical, entries, names),
                writers: 0,
                compacting: false,
            }),
            layout: Mutex::new(layout),
            commit_cb: Mutex::new(None),
            ctl: CompactCtl { state: AtomicU32::new(COMPACT_IDLE), percent: AtomicU32::new(0) },
        });
        Ok(Vfs { shared })
    }

    /// 写入三段式第 1 步：登记 Downloading 条目并在逻辑末尾预留 4K 对齐区间。
    /// 同名规则（§1.1-2）：Deleted 可替换（旧数据区成垃圾）；Active/Downloading → ALREADY_EXISTS。
    pub(crate) fn alloc(&self, name: &str, size: u64) -> Result<VfsWriter, VfsError> {
        if name.is_empty() || name.len() > u16::MAX as usize {
            return Err(VfsError::InvalidArg);
        }
        let aligned = format::align4k(size);
        let mut g = self.shared.inner.lock().unwrap();
        if g.compacting {
            return Err(VfsError::BusyCompacting);
        }
        if let Some(e) = g.idx.find(name) {
            if e.state != ST_DELETED {
                return Err(VfsError::AlreadyExists);
            }
            g.idx.remove(name);
        }
        let offset = format::align4k(g.idx.logical);
        let logical = offset + aligned;
        g.idx.logical = logical;
        g.idx.insert(name, Entry { name_off: 0, name_len: 0, state: ST_DOWNLOADING, offset, size, crc: 0 });
        // alloc/abort 同样 bump generation（Q12 意图：C# 感知任何索引变化）：
        // enumerate 两段式靠它失效旧 gen，否则 alloc 长名后按旧缓冲读会越界写。
        g.idx.generation += 1;
        g.writers += 1;
        // 物理不足则扩容（inner 锁内完成，保证预约与容量一致）；失败回滚登记。
        let grown = (|| -> io::Result<()> {
            let df = self.shared.data_file.lock().unwrap();
            let phys = df.metadata()?.len();
            if phys < logical {
                df.set_len(logical)?;
            }
            Ok(())
        })();
        if grown.is_err() {
            g.idx.remove(name);
            g.writers -= 1;
            return Err(VfsError::Io);
        }
        drop(g);
        // 每写入者独立 OS 句柄（Windows 同句柄并发 pwrite 不安全）。
        let file = match OpenOptions::new().read(true).write(true).open(&self.shared.data_path) {
            Ok(f) => f,
            Err(_) => {
                if let Ok(mut g) = self.shared.inner.lock() {
                    g.idx.remove(name);
                    g.writers -= 1;
                }
                return Err(VfsError::Io);
            }
        };
        Ok(VfsWriter {
            shared: Arc::clone(&self.shared),
            file,
            name: name.to_string(),
            offset,
            size,
            written: 0,
            crc_raw: 0xFFFF_FFFF,
            crc_dirty: false,
            done: false,
        })
    }

    /// 软删除：Active → Deleted（generation++）；已 Deleted 幂等；Downloading/Bad 拒绝。
    pub(crate) fn delete(&self, name: &str) -> Result<(), VfsError> {
        let mut g = self.shared.inner.lock().unwrap();
        if g.compacting {
            return Err(VfsError::BusyCompacting);
        }
        let e = g.idx.find_mut(name).ok_or(VfsError::NotFound)?;
        match e.state {
            ST_ACTIVE => {
                e.state = ST_DELETED;
                g.idx.generation += 1;
                Ok(())
            }
            ST_DELETED => Ok(()),
            _ => Err(VfsError::StateInvalid),
        }
    }

    /// 显式落盘：fsync files.vfs → 序列化写 inactive region → fsync header.vfs → 更新 active_gen。
    /// 无定时落盘；崩溃丢最近未 flush 的 commit。滞后的旧 gen persist（与整理
    /// Finalize 竞速）被水位线拒绝，以 GenChanged 浮出。
    pub(crate) fn flush(&self) -> Result<(), VfsError> {
        {
            let g = self.shared.inner.lock().unwrap();
            if g.compacting {
                return Err(VfsError::BusyCompacting);
            }
        }
        flush_image(&self.shared)
    }

    /// 内存索引应答（热路径零文件 I/O）。整理期间 BUSY。
    pub(crate) fn lookup(&self, name: &str) -> Result<VfsEntryInfo, VfsError> {
        let g = self.shared.inner.lock().unwrap();
        if g.compacting {
            return Err(VfsError::BusyCompacting);
        }
        let e = g.idx.find(name).ok_or(VfsError::NotFound)?;
        Ok(VfsEntryInfo { offset: e.offset, size: e.size, state: e.state, crc: e.crc })
    }

    /// enumerate 两段式第 1 步：返回 (generation, 条目数, 名字 blob 字节数)。
    /// blob 字节数为紧凑口径（与 enumerate_read 的实际写入一致）。
    pub(crate) fn enumerate_query(&self) -> Result<(u64, u32, u32), VfsError> {
        let g = self.shared.inner.lock().unwrap();
        if g.compacting {
            return Err(VfsError::BusyCompacting);
        }
        Ok((g.idx.generation, g.idx.entries.len() as u32, g.idx.compact_blob_len()))
    }

    /// FFI 用的尺寸预查（gen 变化即 GEN_CHANGED，避免按过期 blob_len 建切片）。
    fn enumerate_meta(&self, gen: u64) -> Result<(usize, usize), VfsError> {
        let g = self.shared.inner.lock().unwrap();
        if g.compacting {
            return Err(VfsError::BusyCompacting);
        }
        if gen != g.idx.generation {
            return Err(VfsError::GenChanged);
        }
        Ok((g.idx.entries.len(), g.idx.compact_blob_len() as usize))
    }

    /// enumerate 两段式第 2 步：gen 不符返回 GEN_CHANGED；缓冲不足返回 BUFFER_TOO_SMALL。
    /// names 收到紧凑名字 blob（每名 NUL 结尾，无死字节），数组按下标与条目一一对应。
    pub(crate) fn enumerate_read(
        &self,
        gen: u64,
        names: &mut [u8],
        off: &mut [u64],
        size: &mut [u64],
        crc: &mut [u32],
        state: &mut [u8],
    ) -> Result<(), VfsError> {
        let g = self.shared.inner.lock().unwrap();
        if g.compacting {
            return Err(VfsError::BusyCompacting);
        }
        if gen != g.idx.generation {
            return Err(VfsError::GenChanged);
        }
        let count = g.idx.entries.len();
        let blen = g.idx.compact_blob_len() as usize;
        if off.len() < count
            || size.len() < count
            || crc.len() < count
            || state.len() < count
            || names.len() < blen
        {
            return Err(VfsError::BufferTooSmall);
        }
        let mut cur = 0usize;
        for (i, e) in g.idx.entries.iter().enumerate() {
            off[i] = e.offset;
            size[i] = e.size;
            crc[i] = e.crc;
            state[i] = e.state;
            // 紧凑重写：内存 blob 只增不减，死字节按序剔除后与数组严格配对
            let (s, l) = (e.name_off as usize, e.name_len as usize);
            names[cur..cur + l].copy_from_slice(&g.idx.names[s..s + l]);
            names[cur + l] = 0;
            cur += l + 1;
        }
        Ok(())
    }

    pub(crate) fn get_generation(&self) -> u64 {
        self.shared.inner.lock().unwrap().idx.generation
    }

    /// 整理期间不受限（§2.3）。
    pub(crate) fn stat(&self) -> VfsStat {
        let g = self.shared.inner.lock().unwrap();
        let mut total = 0u64;
        let mut active = 0u64;
        let mut deleted = 0u64;
        for e in &g.idx.entries {
            let a = format::align4k(e.size);
            total += a;
            match e.state {
                ST_ACTIVE => active += a,
                ST_DELETED => deleted += a,
                _ => {}
            }
        }
        let garbage = g.idx.logical.saturating_sub(total);
        let physical = self.shared.data_file.lock().unwrap().metadata().map(|m| m.len()).unwrap_or(0);
        VfsStat { logical: g.idx.logical, physical, total, active, deleted, garbage }
    }

    /// 异步碎片整理（§2.3）。有 writer → BUSY_WRITERS；整理中 → BUSY_COMPACTING。
    pub(crate) fn compact(&self, reserve_extra: u64) -> Result<(), VfsError> {
        self.compact_with(reserve_extra, None, None, 0)
    }

    pub(crate) fn compact_with(
        &self,
        reserve_extra: u64,
        progress: Option<compact::ProgressCb>,
        done: Option<compact::DoneCb>,
        user: usize,
    ) -> Result<(), VfsError> {
        {
            let mut g = self.shared.inner.lock().unwrap();
            if g.compacting {
                return Err(VfsError::BusyCompacting);
            }
            if g.writers != 0 {
                return Err(VfsError::BusyWriters);
            }
            g.compacting = true;
        }
        if compact::start(Arc::clone(&self.shared), reserve_extra, progress, done, user).is_err() {
            let mut g = self.shared.inner.lock().unwrap();
            g.compacting = false;
            self.shared.ctl.state.store(COMPACT_IDLE, Ordering::Relaxed);
            return Err(VfsError::Io);
        }
        Ok(())
    }

    /// (state, percent)——整理期间不受限。
    pub(crate) fn compact_status(&self) -> (u32, u32) {
        (
            self.shared.ctl.state.load(Ordering::Relaxed),
            self.shared.ctl.percent.load(Ordering::Relaxed),
        )
    }
}

// --- 写入三段式：writer ---

pub(crate) struct VfsWriter {
    shared: Arc<VfsShared>,
    /// writer 自己的 OS 句柄（与 Vfs 的句柄、其他 writer 互不影响）。
    file: File,
    name: String,
    /// 数据区绝对偏移（writer 存活期整理被 BUSY 挡住，偏移恒定）。
    offset: u64,
    size: u64,
    written: u64,
    /// 增量 crc 状态（0xFFFF_FFFF 起，commit 时取反）。
    crc_raw: u32,
    /// 发生过回退重写 → 增量 crc 失效，commit 时经自身句柄整读重算。
    crc_dirty: bool,
    done: bool,
}

impl VfsWriter {
    /// 写入 [rel_off, rel_off+len) ⊆ [0, size)。顺序写（rel_off == 已写位置）走增量 crc；
    /// 回退重写（dlmgr 换源 reset）允许，commit 时整读重算；跳跃/越界 → WRITE_OVERFLOW。
    pub(crate) fn write_at(&mut self, rel_off: u64, data: &[u8]) -> Result<(), VfsError> {
        if self.done {
            return Err(VfsError::StateInvalid);
        }
        let end = rel_off.checked_add(data.len() as u64).ok_or(VfsError::WriteOverflow)?;
        if end > self.size || rel_off > self.written {
            return Err(VfsError::WriteOverflow);
        }
        if rel_off < self.written {
            self.crc_dirty = true;
        }
        if !data.is_empty() {
            pwrite(&self.file, self.offset + rel_off, data).map_err(ioerr)?;
            if !self.crc_dirty {
                self.crc_raw = format::crc32_update(self.crc_raw, data);
            }
            self.written = self.written.max(end);
        }
        Ok(())
    }

    /// 校验写满 + crc → 条目 Active → generation++ → commit 回调。
    pub(crate) fn commit(mut self) -> Result<(), VfsError> {
        self.do_commit()
    }

    /// 条目丢弃，区间成垃圾。
    pub(crate) fn abort(mut self) {
        self.do_abort();
    }

    fn do_commit(&mut self) -> Result<(), VfsError> {
        if self.done {
            return Err(VfsError::StateInvalid);
        }
        if self.written != self.size {
            return Err(VfsError::StateInvalid); // 未写满
        }
        let crc = if self.crc_dirty {
            let mut raw = 0xFFFF_FFFFu32;
            let mut buf = vec![0u8; 4 << 20];
            let mut done = 0u64;
            while done < self.size {
                let n = ((self.size - done).min(buf.len() as u64)) as usize;
                pread(&self.file, self.offset + done, &mut buf[..n]).map_err(ioerr)?;
                raw = format::crc32_update(raw, &buf[..n]);
                done += n as u64;
            }
            !raw
        } else {
            !self.crc_raw
        };
        {
            let mut g = self.shared.inner.lock().unwrap();
            if g.compacting {
                return Err(VfsError::BusyCompacting);
            }
            let Some(e) = g.idx.find_mut(&self.name) else { return Err(VfsError::StateInvalid) };
            if e.state != ST_DOWNLOADING {
                return Err(VfsError::StateInvalid);
            }
            e.state = ST_ACTIVE;
            e.crc = crc;
            g.idx.generation += 1;
            g.writers -= 1;
            self.done = true;
        }
        self.shared.fire_commit(&self.name, self.offset, self.size, crc);
        Ok(())
    }

    fn do_abort(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        if let Ok(mut g) = self.shared.inner.lock() {
            g.idx.remove(&self.name);
            g.idx.generation += 1; // 条目数变了，enumerate 旧 gen 必须失效
            g.writers = g.writers.saturating_sub(1);
        }
    }
}

impl Drop for VfsWriter {
    fn drop(&mut self) {
        // commit 失败/未收尾即 abort 语义（Drop 保证 writers 计数回收）。
        self.do_abort();
    }
}

// ============================================================================
// FFI（设计 §2.4）。句柄：vfs_open 返回 Arc<Vfs> 裸指针；glue 只做 &* 借用，
// 只有 vfs_close 消费（drop(Arc::from_raw)）。
// ============================================================================

unsafe fn cstr_arg(p: *const c_char) -> Result<String, VfsError> {
    if p.is_null() {
        return Err(VfsError::InvalidArg);
    }
    CStr::from_ptr(p).to_str().map(|s| s.to_string()).map_err(|_| VfsError::InvalidArg)
}

/// vfs C ABI 版本。
pub const VFS_ABI_VERSION: c_int = 1;

#[no_mangle]
pub unsafe extern "C" fn vfs_abi_version() -> c_int {
    VFS_ABI_VERSION
}

#[no_mangle]
pub unsafe extern "C" fn vfs_open(dir: *const c_char) -> *mut c_void {
    let Ok(d) = cstr_arg(dir) else { return ptr::null_mut() };
    match Vfs::open(Path::new(&d)) {
        Ok(v) => Arc::into_raw(Arc::new(v)) as *mut c_void,
        Err(_) => ptr::null_mut(),
    }
}

/// 路径式打开：索引与数据文件分别指定（vfs_open 的超集，同目录异名、异目录
/// 皆可）。纯新增导出，不 bump VFS_ABI_VERSION。防呆拒绝（空路径/同路径）与
/// IO 失败均返回 NULL。
#[no_mangle]
pub unsafe extern "C" fn vfs_open_paths(index: *const c_char, data: *const c_char) -> *mut c_void {
    let (Ok(i), Ok(d)) = (cstr_arg(index), cstr_arg(data)) else { return ptr::null_mut() };
    match Vfs::open_paths(Path::new(&i), Path::new(&d)) {
        Ok(v) => Arc::into_raw(Arc::new(v)) as *mut c_void,
        Err(_) => ptr::null_mut(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn vfs_close(h: *mut c_void) {
    if !h.is_null() {
        let v = Arc::from_raw(h as *const Vfs);
        // Q11：优雅 close 落盘。best-effort——整理中（Busy）或 IO 失败不阻塞 close。
        let _ = v.flush();
        drop(v);
    }
}

#[no_mangle]
pub unsafe extern "C" fn vfs_flush(h: *mut c_void) -> c_int {
    let Some(v) = (h as *const Vfs).as_ref() else { return VfsError::InvalidArg.as_i32() as c_int };
    code(v.flush())
}

#[no_mangle]
pub unsafe extern "C" fn vfs_alloc(h: *mut c_void, name: *const c_char, size: u64) -> *mut c_void {
    let Some(v) = (h as *const Vfs).as_ref() else { return ptr::null_mut() };
    let Ok(n) = cstr_arg(name) else { return ptr::null_mut() };
    match v.alloc(&n, size) {
        Ok(w) => Box::into_raw(Box::new(w)) as *mut c_void,
        Err(_) => ptr::null_mut(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn vfs_writer_write(w: *mut c_void, rel_off: u64, buf: *const c_void, len: usize) -> c_int {
    let Some(wr) = (w as *mut VfsWriter).as_mut() else { return VfsError::InvalidArg.as_i32() as c_int };
    if len != 0 && buf.is_null() {
        return VfsError::InvalidArg.as_i32() as c_int;
    }
    let data: &[u8] = if len == 0 { &[] } else { std::slice::from_raw_parts(buf as *const u8, len) };
    code(wr.write_at(rel_off, data))
}

#[no_mangle]
pub unsafe extern "C" fn vfs_writer_commit(w: *mut c_void) -> c_int {
    let Some(wr) = (w as *mut VfsWriter).as_mut() else { return VfsError::InvalidArg.as_i32() as c_int };
    code(Box::from_raw(wr as *mut VfsWriter).commit())
}

#[no_mangle]
pub unsafe extern "C" fn vfs_writer_abort(w: *mut c_void) -> c_int {
    let Some(wr) = (w as *mut VfsWriter).as_mut() else { return VfsError::InvalidArg.as_i32() as c_int };
    Box::from_raw(wr as *mut VfsWriter).abort();
    VFS_OK as c_int
}

#[no_mangle]
pub unsafe extern "C" fn vfs_delete(h: *mut c_void, name: *const c_char) -> c_int {
    let Some(v) = (h as *const Vfs).as_ref() else { return VfsError::InvalidArg.as_i32() as c_int };
    let Ok(n) = cstr_arg(name) else { return VfsError::InvalidArg.as_i32() as c_int };
    code(v.delete(&n))
}

/// out 参数可空。state 取值见文件状态枚举（Active=0 Deleted=1 Downloading=2 Bad=3）。
#[no_mangle]
pub unsafe extern "C" fn vfs_lookup(
    h: *mut c_void,
    name: *const c_char,
    out_off: *mut u64,
    out_size: *mut u64,
    out_state: *mut c_int,
    out_crc: *mut u32,
) -> c_int {
    let Some(v) = (h as *const Vfs).as_ref() else { return VfsError::InvalidArg.as_i32() as c_int };
    let Ok(n) = cstr_arg(name) else { return VfsError::InvalidArg.as_i32() as c_int };
    match v.lookup(&n) {
        Err(e) => e.as_i32() as c_int,
        Ok(i) => {
            if !out_off.is_null() {
                *out_off = i.offset;
            }
            if !out_size.is_null() {
                *out_size = i.size;
            }
            if !out_state.is_null() {
                *out_state = i.state as c_int;
            }
            if !out_crc.is_null() {
                *out_crc = i.crc;
            }
            VFS_OK as c_int
        }
    }
}

/// enumerate 两段式第 1 步：→ gen、→ count、→ blob_len（out 可空）。
#[no_mangle]
pub unsafe extern "C" fn vfs_enumerate_query(
    h: *mut c_void,
    out_gen: *mut u64,
    out_count: *mut u32,
    out_blob_len: *mut u32,
) -> c_int {
    let Some(v) = (h as *const Vfs).as_ref() else { return VfsError::InvalidArg.as_i32() as c_int };
    match v.enumerate_query() {
        Err(e) => e.as_i32() as c_int,
        Ok((gen, count, blen)) => {
            if !out_gen.is_null() {
                *out_gen = gen;
            }
            if !out_count.is_null() {
                *out_count = count;
            }
            if !out_blob_len.is_null() {
                *out_blob_len = blen;
            }
            VFS_OK as c_int
        }
    }
}

/// enumerate 两段式第 2 步：gen 变了返回 GEN_CHANGED（不写缓冲）；
/// cap < count 返回 BUFFER_TOO_SMALL。state 数组为 c_int（值 0..3）。
#[no_mangle]
pub unsafe extern "C" fn vfs_enumerate_read(
    h: *mut c_void,
    gen: u64,
    names_buf: *mut u8,
    off: *mut u64,
    size: *mut u64,
    crc: *mut u32,
    state: *mut c_int,
    cap: usize,
) -> c_int {
    let Some(v) = (h as *const Vfs).as_ref() else { return VfsError::InvalidArg.as_i32() as c_int };
    let (count, blen) = match v.enumerate_meta(gen) {
        Ok(x) => x,
        Err(e) => return e.as_i32() as c_int,
    };
    if cap < count {
        return VfsError::BufferTooSmall.as_i32() as c_int;
    }
    if (blen != 0 && names_buf.is_null()) || off.is_null() || size.is_null() || crc.is_null() || state.is_null() {
        return VfsError::InvalidArg.as_i32() as c_int;
    }
    let names: &mut [u8] = if blen == 0 { &mut [] } else { std::slice::from_raw_parts_mut(names_buf, blen) };
    let offs = std::slice::from_raw_parts_mut(off, cap);
    let sizes = std::slice::from_raw_parts_mut(size, cap);
    let crcs = std::slice::from_raw_parts_mut(crc, cap);
    let mut states = vec![0u8; cap];
    match v.enumerate_read(gen, names, offs, sizes, crcs, &mut states) {
        Err(e) => e.as_i32() as c_int,
        Ok(()) => {
            for i in 0..count {
                *state.add(i) = states[i] as c_int;
            }
            VFS_OK as c_int
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn vfs_get_generation(h: *mut c_void) -> u64 {
    match (h as *const Vfs).as_ref() {
        Some(v) => v.get_generation(),
        None => 0,
    }
}

/// 可选 commit 回调；cb 传 NULL 清除。回调可能在任意调用 commit 的线程触发，
/// 原生侧只存裸指针 —— C# 必须保证委托在句柄存活期有效（keep-alive）。
#[no_mangle]
pub unsafe extern "C" fn vfs_set_commit_callback(h: *mut c_void, cb: Option<VfsCommitFn>, user: *mut c_void) -> c_int {
    let Some(v) = (h as *const Vfs).as_ref() else { return VfsError::InvalidArg.as_i32() as c_int };
    let mut g = v.shared.commit_cb.lock().unwrap();
    *g = cb.map(|f| (f, user as usize));
    VFS_OK as c_int
}

/// 全部 out 可空。physical = files.vfs 当前物理大小。
#[no_mangle]
pub unsafe extern "C" fn vfs_stat(
    h: *mut c_void,
    out_logical: *mut u64,
    out_physical: *mut u64,
    out_total: *mut u64,
    out_active: *mut u64,
    out_deleted: *mut u64,
    out_garbage: *mut u64,
) -> c_int {
    let Some(v) = (h as *const Vfs).as_ref() else { return VfsError::InvalidArg.as_i32() as c_int };
    let s = v.stat();
    if !out_logical.is_null() {
        *out_logical = s.logical;
    }
    if !out_physical.is_null() {
        *out_physical = s.physical;
    }
    if !out_total.is_null() {
        *out_total = s.total;
    }
    if !out_active.is_null() {
        *out_active = s.active;
    }
    if !out_deleted.is_null() {
        *out_deleted = s.deleted;
    }
    if !out_garbage.is_null() {
        *out_garbage = s.garbage;
    }
    VFS_OK as c_int
}

/// 异步整理。progress_cb(user, percent, name) 在 Move 期间推进 0→100；
/// done_cb(user, err) 在结束（成功或失败）时触发一次，之后 state 回 Idle。
/// 回调在整理线程触发；原生侧只存裸指针 —— C# 必须保证委托存活（keep-alive）。
#[no_mangle]
pub unsafe extern "C" fn vfs_compact(
    h: *mut c_void,
    reserve_extra: u64,
    progress_cb: Option<compact::ProgressCb>,
    done_cb: Option<compact::DoneCb>,
    user: *mut c_void,
) -> c_int {
    let Some(v) = (h as *const Vfs).as_ref() else { return VfsError::InvalidArg.as_i32() as c_int };
    code(v.compact_with(reserve_extra, progress_cb, done_cb, user as usize))
}

/// out 可空：state ∈ {Idle=0, Scan=1, Move=2, Finalize=3}，percent 0..100。
#[no_mangle]
pub unsafe extern "C" fn vfs_compact_status(h: *mut c_void, out_state: *mut c_int, out_percent: *mut c_int) -> c_int {
    let Some(v) = (h as *const Vfs).as_ref() else { return VfsError::InvalidArg.as_i32() as c_int };
    let (s, p) = v.compact_status();
    if !out_state.is_null() {
        *out_state = s as c_int;
    }
    if !out_percent.is_null() {
        *out_percent = p as c_int;
    }
    VFS_OK as c_int
}

// ============================================================================
// 单元测试（T1/T2 验收用例）。零第三方依赖：临时目录手搓。
// ============================================================================

#[cfg(test)]
mod tests {
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
        assert_eq!(&raw[ib.offset as usize..ib.offset as usize + b.len()], &b[..]);
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
        assert!(matches!(v.shared.persist_image(1, &[]), Err(VfsError::GenChanged)));
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
        v.enumerate_read(gen, &mut names, &mut offs, &mut sizes, &mut crcs, &mut states).unwrap();
        let got: Vec<&str> = names
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| std::str::from_utf8(s).unwrap())
            .collect();
        assert!(got.contains(&"alpha") && got.contains(&"beta"));

        seq_write(&v, "gamma", &[3u8; 30]).unwrap(); // gen 变化
        assert!(matches!(
            v.enumerate_read(gen, &mut names, &mut offs, &mut sizes, &mut crcs, &mut states),
            Err(VfsError::GenChanged)
        ));
        let (gen2, count2, blen2) = v.enumerate_query().unwrap();
        assert_eq!(count2, 3);
        let mut names2 = vec![0u8; blen2 as usize];
        let mut offs2 = vec![0u64; count2 as usize];
        let mut sizes2 = vec![0u64; count2 as usize];
        let mut crcs2 = vec![0u32; count2 as usize];
        let mut states2 = vec![0u8; count2 as usize];
        v.enumerate_read(gen2, &mut names2, &mut offs2, &mut sizes2, &mut crcs2, &mut states2).unwrap();
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
        v.enumerate_read(gen, &mut names, &mut offs, &mut sizes, &mut crcs, &mut states).unwrap();
        let ia = v.lookup("f0.bin").unwrap();
        assert_eq!(ia.crc, format::crc32(&[0u8; 16]));
        assert_eq!(v.lookup("f19999.bin").unwrap().size, 16);
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
        let p = compact::build_plan(vec![("a".into(), e(4096, 4096))], 8192);
        assert_eq!(p.items[0].new_off, 0);
        assert_eq!(p.new_logical, 4096);
        // gap < size → 搬到旧逻辑末尾之后；前一条 offset==cursor 原地不动
        let p = compact::build_plan(vec![("a".into(), e(0, 4096)), ("b".into(), e(12288, 12288))], 16384);
        assert_eq!(p.items[0].new_off, 0);
        assert_eq!(p.items[1].new_off, 16384);
        assert_eq!(p.new_logical, 16384 + 12288);
        // 尾部大空洞 → 不搬
        let p = compact::build_plan(vec![("a".into(), e(0, 4096))], 1 << 20);
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
            vec![("a".into(), e(0)), ("c".into(), e(8192)), ("d".into(), e(12288))],
            16384,
        );
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
        v.compact(8192).unwrap();
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
        v.compact(0).unwrap();
        wait_idle(&v);

        assert_eq!(v.lookup("f1").unwrap().offset, 0); // 原地
        assert_eq!(v.lookup("f3").unwrap().offset, 24576);
        let s = v.stat();
        assert_eq!(s.logical, 24576 + 12288);
        assert_eq!(s.physical, s.logical);
        let raw = data_of(&td.0);
        assert_eq!(&raw[24576..24576 + 12288], &d3[..]);
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
        assert!(matches!(v.compact(0), Err(VfsError::BusyWriters)));
        w.abort();

        v.compact(0).unwrap();
        wait_moving(&v);
        assert!(matches!(v.compact(0), Err(VfsError::BusyCompacting)));
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
        let df = OpenOptions::new().write(true).open(td.0.join("files.vfs")).unwrap();
        pwrite(&df, i2.offset, &[0u8; 64]).unwrap();
        drop(df);

        v.compact(0).unwrap();
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
            v.compact(0).unwrap();
            wait_idle(&v);
        }
        {
            let v = Vfs::open(&td.0).unwrap();
            assert_eq!(v.lookup("f1").unwrap().offset, 0); // 原地
            assert_eq!(v.lookup("f3").unwrap().offset, 4096);
            let raw = data_of(&td.0);
            assert_eq!(&raw[4096..8192], &mk(3)[..]);
            v.delete("f1").unwrap();
            v.compact(0).unwrap();
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

    /// R05：更小的 region 不得复用更大的闲置槽位——死尾巴会卡死重开时的顺序扫描，
    /// 其后追加的槽位全部不可达（gen 回退、新文件消失）。
    #[test]
    fn persist_never_reuses_larger_inactive_slot() {
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
            v.enumerate_read(gen, &mut vec![0u8; blen as usize], &mut vec![0u64; 2],
                             &mut vec![0u64; 2], &mut vec![0u32; 2], &mut vec![0u8; 2]),
            Err(VfsError::GenChanged)
        ));

        // 长名 alloc 后仍按旧 gen 读 → GEN_CHANGED（修复前：blen 变大 → 越界写）
        let w2 = v.alloc(&"x".repeat(4096), 10).unwrap();
        assert!(matches!(
            v.enumerate_read(gen, &mut vec![0u8; blen as usize], &mut vec![0u64; 2],
                             &mut vec![0u64; 2], &mut vec![0u32; 2], &mut vec![0u8; 2]),
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
        v.enumerate_read(gen2, &mut names, &mut offs, &mut sizes, &mut crcs, &mut states).unwrap();
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
}
