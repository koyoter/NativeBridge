// vfs/format.rs — VFS 磁盘格式（设计 §2.1）的小端序列化与校验。
//
// header.vfs = SuperBlock(64B) + Region A + Region B（双缓冲，单 active）：
//   SuperBlock: magic[8]="1KVFSHDR" | format_version u32 | active_gen u64 | sb_crc u32 | reserved
//               （sb_crc 覆盖 magic+version+active_gen 共 20 字节）
//   Region 头(32B): gen u64 | entry_count u32 | string_area_len u32 | region_crc u32
//                   | logical_size u64 | reserved u32
//               （region_crc 覆盖 EntryTable + StringArea）
//   Entry(32B): name_off u32 | name_len u16 | state u8 | reserved u8
//               | offset u64 | size u64 | crc32 u32 | entry_crc u32
//               （entry_crc 覆盖前 28 字节）
//   StringArea: UTF-8 文件名连续存放，每名后跟一个 NUL。
//
// files.vfs = 纯数据区，文件按 4K 对齐连续摆放，无内联头。

// crc32 共享实现（modules/util.rs）。
pub(crate) use crate::modules::util::{crc32, update_raw as crc32_update};

pub(crate) const MAGIC: &[u8; 8] = b"1KVFSHDR";
pub(crate) const FORMAT_VERSION: u32 = 1;
pub(crate) const SB_SIZE: u64 = 64;
/// SuperBlock 内 active_gen 的偏移（sb_patch 写 [SB_GEN_OFF, SB_GEN_OFF+12)）。
pub(crate) const SB_GEN_OFF: u64 = 12;
pub(crate) const REGION_HDR_SIZE: usize = 32;
pub(crate) const ENTRY_SIZE: usize = 32;
pub(crate) const ALIGN: u64 = 4096;

// 文件状态（§2.4）
pub(crate) const ST_ACTIVE: u8 = 0;
pub(crate) const ST_DELETED: u8 = 1;
pub(crate) const ST_DOWNLOADING: u8 = 2;
pub(crate) const ST_BAD: u8 = 3;

/// 4K 对齐（0 → 0）。
pub(crate) fn align4k(n: u64) -> u64 {
    n.div_ceil(ALIGN) * ALIGN
}

// --- Entry ---

#[derive(Clone, Copy, Debug)]
pub(crate) struct Entry {
    pub(crate) name_off: u32,
    pub(crate) name_len: u16,
    pub(crate) state: u8,
    pub(crate) offset: u64,
    pub(crate) size: u64,
    pub(crate) crc: u32,
}

impl Entry {
    pub(crate) fn encode(&self, out: &mut [u8; ENTRY_SIZE]) {
        out[0..4].copy_from_slice(&self.name_off.to_le_bytes());
        out[4..6].copy_from_slice(&self.name_len.to_le_bytes());
        out[6] = self.state;
        out[7] = 0;
        out[8..16].copy_from_slice(&self.offset.to_le_bytes());
        out[16..24].copy_from_slice(&self.size.to_le_bytes());
        out[24..28].copy_from_slice(&self.crc.to_le_bytes());
        let crc = crc32(&out[0..28]);
        out[28..32].copy_from_slice(&crc.to_le_bytes());
    }

    pub(crate) fn decode(b: &[u8]) -> Option<Entry> {
        if b.len() < ENTRY_SIZE {
            return None;
        }
        let stored = u32::from_le_bytes(b[28..32].try_into().ok()?);
        if crc32(&b[0..28]) != stored {
            return None;
        }
        let state = b[6];
        if state > ST_BAD {
            return None;
        }
        Some(Entry {
            name_off: u32::from_le_bytes(b[0..4].try_into().ok()?),
            name_len: u16::from_le_bytes(b[4..6].try_into().ok()?),
            state,
            offset: u64::from_le_bytes(b[8..16].try_into().ok()?),
            size: u64::from_le_bytes(b[16..24].try_into().ok()?),
            crc: u32::from_le_bytes(b[24..28].try_into().ok()?),
        })
    }
}

// --- SuperBlock ---

/// 全新 SuperBlock 镜像（64B）。
pub(crate) fn sb_image(active_gen: u64) -> [u8; 64] {
    let mut sb = [0u8; 64];
    sb[0..8].copy_from_slice(MAGIC);
    sb[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    sb[12..20].copy_from_slice(&active_gen.to_le_bytes());
    let crc = crc32(&sb[0..20]);
    sb[20..24].copy_from_slice(&crc.to_le_bytes());
    sb
}

/// 原地更新 active_gen 时写入 [SB_GEN_OFF, SB_GEN_OFF+12) 的 12 字节（gen + 重算的 sb_crc）。
pub(crate) fn sb_patch(active_gen: u64) -> [u8; 12] {
    let mut prefix = [0u8; 20];
    prefix[0..8].copy_from_slice(MAGIC);
    prefix[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    prefix[12..20].copy_from_slice(&active_gen.to_le_bytes());
    let mut out = [0u8; 12];
    out[0..8].copy_from_slice(&active_gen.to_le_bytes());
    out[8..12].copy_from_slice(&crc32(&prefix).to_le_bytes());
    out
}

/// 读 SuperBlock：合法则返回 active_gen。
pub(crate) fn read_sb(bytes: &[u8]) -> Option<u64> {
    if bytes.len() < SB_SIZE as usize || &bytes[0..8] != MAGIC {
        return None;
    }
    if u32::from_le_bytes(bytes[8..12].try_into().ok()?) != FORMAT_VERSION {
        return None;
    }
    let crc = u32::from_le_bytes(bytes[20..24].try_into().ok()?);
    if crc32(&bytes[0..20]) != crc {
        return None;
    }
    Some(u64::from_le_bytes(bytes[12..20].try_into().ok()?))
}

// --- Region ---

pub(crate) struct RegionImage {
    pub(crate) bytes: Vec<u8>,
    /// StringArea 在 bytes 中的起始偏移（重建 Index 时切出名字 blob 用）。
    pub(crate) blob_off: usize,
}

/// 从旧名字 blob 重建紧凑 StringArea 并序列化整个 region（就地修正 entries 的 name_off）。
pub(crate) fn build_region(gen: u64, logical: u64, entries: &mut [Entry], old_blob: &[u8]) -> RegionImage {
    let mut blob = Vec::with_capacity(old_blob.len());
    for e in entries.iter_mut() {
        // 先取旧 blob 里的偏移再覆写 name_off：name_off 指向 old_blob，
        // 先写后读会把条目挂到别的（含已删除死字节的）名字下。
        let (s, l) = (e.name_off as usize, e.name_len as usize);
        e.name_off = blob.len() as u32;
        blob.extend_from_slice(&old_blob[s..s + l]);
        blob.push(0);
    }
    let table_len = entries.len() * ENTRY_SIZE;
    let mut bytes = Vec::with_capacity(REGION_HDR_SIZE + table_len + blob.len());
    bytes.extend_from_slice(&gen.to_le_bytes());
    bytes.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&(blob.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes()); // region_crc 占位
    bytes.extend_from_slice(&logical.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes()); // reserved
    for e in entries.iter() {
        let mut cell = [0u8; ENTRY_SIZE];
        e.encode(&mut cell);
        bytes.extend_from_slice(&cell);
    }
    let blob_off = bytes.len();
    bytes.extend_from_slice(&blob);
    let crc = crc32(&bytes[REGION_HDR_SIZE..]);
    bytes[16..20].copy_from_slice(&crc.to_le_bytes());
    RegionImage { bytes, blob_off }
}

pub(crate) struct ParsedRegion {
    pub(crate) gen: u64,
    pub(crate) logical: u64,
    pub(crate) entries: Vec<Entry>,
    pub(crate) names: Vec<u8>,
}

/// 扫描 header.vfs 中 off 处的一个 region 槽位。
/// 返回 None = 头部不可读（后续槽位无法定位，扫描终止）；
/// parsed = None = 头部可读但数据校验失败（槽位仍可作为复用空间）。
pub(crate) struct SlotScan {
    pub(crate) off: u64,
    pub(crate) total: u64,
    pub(crate) parsed: Option<ParsedRegion>,
}

pub(crate) fn scan_slot(bytes: &[u8], off: usize) -> Option<SlotScan> {
    if off + REGION_HDR_SIZE > bytes.len() {
        return None;
    }
    let h = &bytes[off..off + REGION_HDR_SIZE];
    let gen = u64::from_le_bytes(h[0..8].try_into().ok()?);
    let count = u32::from_le_bytes(h[8..12].try_into().ok()?) as usize;
    let str_len = u32::from_le_bytes(h[12..16].try_into().ok()?) as usize;
    let region_crc = u32::from_le_bytes(h[16..20].try_into().ok()?);
    let logical = u64::from_le_bytes(h[20..28].try_into().ok()?);
    // 头部合理性：防止把垃圾字节当成巨大 region。
    if count > 1 << 24 || str_len > 1 << 30 {
        return None;
    }
    let total = (REGION_HDR_SIZE + count * ENTRY_SIZE + str_len) as u64;
    if off as u64 + total > bytes.len() as u64 {
        return None;
    }
    let body_off = off + REGION_HDR_SIZE;
    let table = &bytes[body_off..body_off + count * ENTRY_SIZE];
    let names = &bytes[body_off + count * ENTRY_SIZE..off + total as usize];
    let mut parsed = None;
    if crc32(&bytes[body_off..off + total as usize]) == region_crc {
        let mut entries = Vec::with_capacity(count);
        let mut ok = true;
        for cell in table.chunks_exact(ENTRY_SIZE) {
            match Entry::decode(cell) {
                Some(e) => {
                    let s = e.name_off as usize;
                    let l = e.name_len as usize;
                    if s + l + 1 > names.len() || std::str::from_utf8(&names[s..s + l]).is_err() {
                        ok = false;
                        break;
                    }
                    entries.push(e);
                }
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            parsed = Some(ParsedRegion { gen, logical, entries, names: names.to_vec() });
        }
    }
    Some(SlotScan { off: off as u64, total, parsed })
}

#[cfg(test)]
mod tests {
    use super::*;

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
            Entry { name_off: 0, name_len: 3, state: ST_ACTIVE, offset: 0, size: 4, crc: 0 },
            Entry { name_off: 8, name_len: 3, state: ST_ACTIVE, offset: 4096, size: 4, crc: 0 },
        ];
        let img = build_region(1, 4096, &mut entries, &old_blob);
        let names = &img.bytes[img.blob_off..];
        assert_eq!(names, b"AAA\0CCC\0"); // 修复前：C 会读到死字节 "BBB"
        assert_eq!(entries[1].name_off, 4);
    }
}
