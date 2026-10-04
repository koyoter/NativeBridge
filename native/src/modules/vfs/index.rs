// vfs/index.rs — VFS 内存索引（设计 §2.2）：
// Vec<Entry> + 名字 blob（StringArea 镜像）+ HashMap<String, u32>，generation 计数。
//
// 名字 blob 只增不减（alloc 追加、delete/abort 留下死字节），
// 每次 flush/整理序列化时由 format::build_region 重建紧凑 blob。

use std::collections::HashMap;

use super::format::Entry;

pub(crate) struct Index {
    pub(crate) entries: Vec<Entry>,
    /// UTF-8 名字 blob：每个 entry 的名字按 [name_off, name_off+name_len) 定位，后跟 NUL。
    pub(crate) names: Vec<u8>,
    pub(crate) map: HashMap<String, u32>,
    pub(crate) generation: u64,
    /// 数据区逻辑大小（追加游标）。
    pub(crate) logical: u64,
}

impl Index {
    pub(crate) fn new(generation: u64, logical: u64, entries: Vec<Entry>, names: Vec<u8>) -> Index {
        let mut idx = Index { entries, names, map: HashMap::new(), generation, logical };
        idx.rebuild_map();
        idx
    }

    fn rebuild_map(&mut self) {
        self.map.clear();
        self.map.reserve(self.entries.len());
        for (i, e) in self.entries.iter().enumerate() {
            if let Some(n) = self.name_of(e) {
                self.map.insert(n.to_string(), i as u32);
            }
        }
    }

    /// 从名字 blob 取 entry 的名字。
    pub(crate) fn name_of(&self, e: &Entry) -> Option<&str> {
        let s = e.name_off as usize;
        let l = e.name_len as usize;
        std::str::from_utf8(self.names.get(s..s + l)?).ok()
    }

    /// 紧凑名字 blob 的字节数（每名 name_len + 1 个 NUL）。
    /// enumerate 对外只给紧凑布局：内存 blob 只增不减，死字节外漏会让
    /// C# 按 NUL 切分后的名字与数组按下标配对错位。
    pub(crate) fn compact_blob_len(&self) -> u32 {
        self.entries.iter().map(|e| e.name_len as u32 + 1).sum()
    }

    pub(crate) fn find(&self, name: &str) -> Option<&Entry> {
        let &i = self.map.get(name)?;
        self.entries.get(i as usize)
    }

    pub(crate) fn find_mut(&mut self, name: &str) -> Option<&mut Entry> {
        let &i = self.map.get(name)?;
        self.entries.get_mut(i as usize)
    }

    /// 追加 entry（登记新名字；调用方负责查重）。
    pub(crate) fn insert(&mut self, name: &str, mut e: Entry) {
        e.name_off = self.names.len() as u32;
        e.name_len = name.len() as u16;
        self.names.extend_from_slice(name.as_bytes());
        self.names.push(0);
        self.map.insert(name.to_string(), self.entries.len() as u32);
        self.entries.push(e);
    }

    /// 删除 entry（swap_remove，O(1)；被顶替者的 map 下标随之修正）。
    /// 名字 blob 里的死字节留待下次序列化回收。
    pub(crate) fn remove(&mut self, name: &str) -> bool {
        let Some(&i) = self.map.get(name) else { return false };
        let i = i as usize;
        self.map.remove(name);
        let last = self.entries.len() - 1;
        self.entries.swap_remove(i);
        if i != last {
            if let Some(moved) = self.entries.get(i) {
                if let Some(n) = self.name_of(moved) {
                    let n = n.to_string();
                    self.map.insert(n, i as u32);
                }
            }
        }
        true
    }
}
