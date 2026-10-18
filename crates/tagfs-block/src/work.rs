//! 一次提交的工作区：在已提交的树上做改动，分配和释放块。
//!
//! 工作区出错就整个丢弃，单元的状态不变；写下去的数据块和节点都在空闲空间里，没有被任何根引用。

use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use tagfs_core::{Change, EntityId, Tag, TxnId, TxnInfo, UnitError};
use tagfs_device::BlockDevice;

use crate::codec::put_u64;
use crate::extent::ExtentSet;
use crate::keys;
use crate::tree::{Tree, max_entry};
use crate::view::{INLINE_MAX, Object, View, encode_prepared};

/// 一次复制对象内容时最多搬多少块。
const COPY_CHUNK_BLOCKS: u64 = 64;

pub(crate) struct Work<D: BlockDevice> {
    pub(crate) dev: Rc<RefCell<D>>,
    pub(crate) bs: u64,
    pub(crate) tree: Tree,
    /// 可以分配的块。
    pub(crate) free: ExtentSet,
    /// 本次提交新分配的块。再释放时直接回到 `free`。
    pub(crate) fresh: ExtentSet,
    /// 本次提交释放的、已提交状态还在用的块。提交之后才能再分配。
    pub(crate) released: ExtentSet,
    pub(crate) next_object: u64,
}

impl<D: BlockDevice> Work<D> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, UnitError> {
        self.tree.get(&*self.dev.borrow(), key)
    }

    fn put(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<(), UnitError> {
        self.tree.insert(&*self.dev.borrow(), key, value)
    }

    fn del(&mut self, key: &[u8]) -> Result<bool, UnitError> {
        self.tree.remove(&*self.dev.borrow(), key)
    }

    fn keys_with_prefix(&self, prefix: &[u8]) -> Result<Vec<Vec<u8>>, UnitError> {
        let mut out = Vec::new();
        self.tree.scan_prefix(&*self.dev.borrow(), prefix, &mut |k, _| {
            out.push(k.to_vec());
            Ok(true)
        })?;
        Ok(out)
    }

    fn with_view<T>(&self, f: impl FnOnce(&View<'_, D>) -> Result<T, UnitError>) -> Result<T, UnitError> {
        let dev = self.dev.borrow();
        f(&View { tree: &self.tree, dev: &*dev, bs: self.bs })
    }

    /// 分配 `n` 个块，尽量连续。
    fn alloc(&mut self, mut n: u64) -> Result<Vec<(u64, u64)>, UnitError> {
        let mut out = Vec::new();
        while n > 0 {
            let (s, l) = self.free.take(n).ok_or(UnitError::NoSpace)?;
            self.fresh.insert(s, l);
            out.push((s, l));
            n -= l;
        }
        Ok(out)
    }

    /// 释放一段块：本次新分配的直接回到空闲，已提交在用的等提交后。
    pub(crate) fn release(&mut self, start: u64, len: u64) {
        let mut cursor = start;
        for (s, l) in self.fresh.remove_overlap(start, len) {
            self.released.insert(cursor, s - cursor);
            self.free.insert(s, l);
            cursor = s + l;
        }
        self.released.insert(cursor, start + len - cursor);
    }

    fn write_blocks(&mut self, block: u64, data: &[u8]) -> Result<(), UnitError> {
        self.dev.borrow_mut().write_at(block * self.bs, data).map_err(|_| UnitError::Device)
    }

    // ---- 对象 ----

    fn object(&self, o: u64) -> Result<Object, UnitError> {
        self.with_view(|v| v.object(o))
    }

    fn put_object(&mut self, o: u64, rec: &Object) -> Result<(), UnitError> {
        self.put(keys::object(o), rec.encode())
    }

    fn put_extent(&mut self, o: u64, logical: u64, phys: u64, len: u64) -> Result<(), UnitError> {
        let mut v = Vec::new();
        put_u64(&mut v, phys);
        put_u64(&mut v, len);
        self.put(keys::extent(o, logical), v)
    }

    fn new_object(&mut self, content: &[u8]) -> Result<u64, UnitError> {
        let o = self.next_object;
        self.next_object += 1;
        if content.len() <= INLINE_MAX {
            self.put_object(o, &Object { refs: 1, size: content.len() as u64, inline: Some(content.to_vec()) })?;
        } else {
            self.put_object(o, &Object { refs: 1, size: 0, inline: None })?;
            self.write_object(o, 0, content)?;
        }
        Ok(o)
    }

    /// 引用数减一，减到零就回收对象和它的块。
    fn unref(&mut self, o: u64) -> Result<(), UnitError> {
        let mut rec = self.object(o)?;
        rec.refs -= 1;
        if rec.refs > 0 {
            return self.put_object(o, &rec);
        }
        self.unmap(o, 0, u64::MAX)?;
        self.del(&keys::object(o))?;
        Ok(())
    }

    /// 去掉逻辑块 `[b0, b1)` 的映射并释放对应的物理块；跨边界的区段只去掉重叠的部分。
    fn unmap(&mut self, o: u64, b0: u64, b1: u64) -> Result<(), UnitError> {
        for (l, p, n) in self.with_view(|v| v.extents_in(o, b0, b1))? {
            self.del(&keys::extent(o, l))?;
            let (lo, hi) = (l.max(b0), (l + n).min(b1));
            self.release(p + (lo - l), hi - lo);
            if l < lo {
                self.put_extent(o, l, p, lo - l)?;
            }
            if hi < l + n {
                self.put_extent(o, hi, p + (hi - l), l + n - hi)?;
            }
        }
        Ok(())
    }

    /// 读一个逻辑块的当前内容（空洞或超出长度的部分是零）。
    fn read_block(&self, o: u64, logical: u64, out: &mut [u8]) -> Result<(), UnitError> {
        out.fill(0);
        self.with_view(|v| v.read_object(o, logical * self.bs, out)).map(|_| ())
    }

    /// 写对象内容。对象改由区段存放时，新内容写进新分配的块，原来的块释放：已提交的状态不受影响。
    fn write_object(&mut self, o: u64, offset: u64, data: &[u8]) -> Result<(), UnitError> {
        let mut rec = self.object(o)?;
        let end = offset.checked_add(data.len() as u64).ok_or(UnitError::NoSpace)?;
        let new_size = rec.size.max(end);
        if let Some(inline) = &mut rec.inline {
            if new_size as usize <= INLINE_MAX {
                inline.resize(new_size as usize, 0);
                inline[offset as usize..end as usize].copy_from_slice(data);
                rec.size = new_size;
                return self.put_object(o, &rec);
            }
            // 放不下了，改成区段：先把原来的内容写成第一段。
            let old = core::mem::take(inline);
            self.put_object(o, &Object { refs: rec.refs, size: 0, inline: None })?;
            if !old.is_empty() {
                self.write_object(o, 0, &old)?;
            }
            return self.write_object(o, offset, data);
        }
        if !data.is_empty() {
            let bs = self.bs;
            let (b0, b1) = (offset / bs, end.div_ceil(bs));
            let mut buf = vec![0u8; ((b1 - b0) * bs) as usize];
            let head_partial = !offset.is_multiple_of(bs);
            if head_partial {
                self.read_block(o, b0, &mut buf[..bs as usize])?;
            }
            if !end.is_multiple_of(bs) && (b1 - 1 > b0 || !head_partial) {
                let at = ((b1 - 1 - b0) * bs) as usize;
                self.read_block(o, b1 - 1, &mut buf[at..at + bs as usize])?;
            }
            let at = (offset - b0 * bs) as usize;
            buf[at..at + data.len()].copy_from_slice(data);
            self.unmap(o, b0, b1)?;
            let mut logical = b0;
            for (s, l) in self.alloc(b1 - b0)? {
                let from = ((logical - b0) * bs) as usize;
                self.write_blocks(s, &buf[from..from + (l * bs) as usize])?;
                self.put_extent(o, logical, s, l)?;
                logical += l;
            }
        }
        rec.size = new_size;
        self.put_object(o, &rec)
    }

    fn truncate_object(&mut self, o: u64, len: u64) -> Result<(), UnitError> {
        let mut rec = self.object(o)?;
        if let Some(inline) = &mut rec.inline {
            if len as usize <= INLINE_MAX {
                inline.resize(len as usize, 0);
                rec.size = len;
                return self.put_object(o, &rec);
            }
            // 变长到放不下：转成区段，多出来的部分是空洞。
            let old = core::mem::take(inline);
            self.put_object(o, &Object { refs: rec.refs, size: 0, inline: None })?;
            self.write_object(o, 0, &old)?;
            let mut rec = self.object(o)?;
            rec.size = len;
            return self.put_object(o, &rec);
        }
        if len as usize <= INLINE_MAX {
            let mut content = vec![0u8; len as usize];
            self.with_view(|v| v.read_object(o, 0, &mut content))?;
            self.unmap(o, 0, u64::MAX)?;
            return self.put_object(o, &Object { refs: rec.refs, size: len, inline: Some(content) });
        }
        if len < rec.size {
            let bs = self.bs;
            self.unmap(o, len.div_ceil(bs), u64::MAX)?;
            // 最后一块的尾巴清零，之后再变长时读出来才是零。
            if !len.is_multiple_of(bs) {
                let last = len / bs;
                let mapped = !self.with_view(|v| v.extents_in(o, last, last + 1))?.is_empty();
                if mapped {
                    let mut block = vec![0u8; bs as usize];
                    self.read_block(o, last, &mut block)?;
                    block[(len % bs) as usize..].fill(0);
                    self.unmap(o, last, last + 1)?;
                    let (s, _) = self.alloc(1)?[0];
                    self.write_blocks(s, &block)?;
                    self.put_extent(o, last, s, 1)?;
                }
            }
        }
        rec.size = len;
        self.put_object(o, &rec)
    }

    /// 复制一份对象，引用数为一。
    fn duplicate(&mut self, o: u64) -> Result<u64, UnitError> {
        let rec = self.object(o)?;
        let copy = self.next_object;
        self.next_object += 1;
        self.put_object(copy, &Object { refs: 1, size: rec.size, inline: rec.inline.clone() })?;
        if rec.inline.is_none() {
            let bs = self.bs;
            for (l, p, n) in self.with_view(|v| v.extents_in(o, 0, u64::MAX))? {
                let mut done = 0;
                while done < n {
                    let chunk = (n - done).min(COPY_CHUNK_BLOCKS);
                    let mut buf = vec![0u8; (chunk * bs) as usize];
                    self.dev.borrow().read_at((p + done) * bs, &mut buf).map_err(|_| UnitError::Device)?;
                    let mut at = 0;
                    for (s, len) in self.alloc(chunk)? {
                        self.write_blocks(s, &buf[(at * bs) as usize..((at + len) * bs) as usize])?;
                        self.put_extent(copy, l + done + at, s, len)?;
                        at += len;
                    }
                    done += chunk;
                }
            }
        }
        Ok(copy)
    }

    // ---- 实体 ----

    fn entity_object(&self, id: &EntityId) -> Result<u64, UnitError> {
        self.with_view(|v| v.entity_object(id))?.ok_or(UnitError::NotFound(*id))
    }

    fn set_entity_object(&mut self, id: &EntityId, o: u64) -> Result<(), UnitError> {
        let mut v = Vec::new();
        put_u64(&mut v, o);
        self.put(keys::entity(id), v)
    }

    fn add_tag(&mut self, id: &EntityId, tag: &Tag) -> Result<(), UnitError> {
        self.put(keys::entity_tag(id, tag), Vec::new())?;
        self.put(keys::index(tag, id), Vec::new())
    }

    fn remove_tag(&mut self, id: &EntityId, tag: &Tag) -> Result<(), UnitError> {
        self.del(&keys::entity_tag(id, tag))?;
        self.del(&keys::index(tag, id))?;
        Ok(())
    }

    fn insert_entity(&mut self, id: &EntityId, o: u64, tags: &[Tag]) -> Result<(), UnitError> {
        if self.get(&keys::entity(id))?.is_some() {
            return Err(UnitError::Exists(*id));
        }
        self.set_entity_object(id, o)?;
        for t in tags {
            self.add_tag(id, t)?;
        }
        Ok(())
    }

    /// 实体要改内容：对象被共享时先复制一份给它。
    fn own_object(&mut self, id: &EntityId) -> Result<u64, UnitError> {
        let o = self.entity_object(id)?;
        if self.object(o)?.refs == 1 {
            return Ok(o);
        }
        let copy = self.duplicate(o)?;
        self.unref(o)?;
        self.set_entity_object(id, copy)?;
        Ok(copy)
    }

    pub(crate) fn apply(&mut self, change: &Change) -> Result<(), UnitError> {
        match change {
            Change::Create { id, tags, content } => {
                if self.get(&keys::entity(id))?.is_some() {
                    return Err(UnitError::Exists(*id));
                }
                let o = self.new_object(content)?;
                self.insert_entity(id, o, tags)
            }
            Change::Copy { from, id, tags } => {
                let o = self.entity_object(from)?;
                if self.get(&keys::entity(id))?.is_some() {
                    return Err(UnitError::Exists(*id));
                }
                let mut rec = self.object(o)?;
                rec.refs += 1;
                self.put_object(o, &rec)?;
                self.insert_entity(id, o, tags)
            }
            Change::Delete { id } => {
                let o = self.entity_object(id)?;
                for t in self.with_view(|v| v.tags(id))? {
                    self.remove_tag(id, &t)?;
                }
                self.del(&keys::entity(id))?;
                self.unref(o)
            }
            Change::AddTag { id, tag } => {
                self.entity_object(id)?;
                self.add_tag(id, tag)
            }
            Change::RemoveTag { id, tag } => {
                self.entity_object(id)?;
                self.remove_tag(id, tag)
            }
            Change::Write { id, offset, data } => {
                let o = self.own_object(id)?;
                self.write_object(o, *offset, data)
            }
            Change::Truncate { id, len } => {
                let o = self.own_object(id)?;
                self.truncate_object(o, *len)
            }
        }
    }

    // ---- 事务记录 ----

    fn chunk_size(&self) -> usize {
        // 键是 1 + 16 + 4 字节。
        (max_entry(self.bs as usize) - 21).min(1024)
    }

    pub(crate) fn put_prepared(&mut self, info: &TxnInfo, changes: &[Change]) -> Result<(), UnitError> {
        self.remove_prepared(&info.id)?;
        let payload = encode_prepared(info, changes);
        let chunk = self.chunk_size();
        for (i, part) in payload.chunks(chunk).enumerate() {
            self.put(keys::prepared_chunk(&info.id, i as u32), part.to_vec())?;
        }
        Ok(())
    }

    /// 删掉准备记录，返回它是否存在。
    pub(crate) fn remove_prepared(&mut self, txn: &TxnId) -> Result<bool, UnitError> {
        let ks = self.keys_with_prefix(&keys::prepared(txn))?;
        for k in &ks {
            self.del(k)?;
        }
        Ok(!ks.is_empty())
    }

    pub(crate) fn prepared_one(&self, txn: &TxnId) -> Result<Option<(TxnInfo, Vec<Change>)>, UnitError> {
        self.with_view(|v| v.prepared_one(txn))
    }

    pub(crate) fn put_committed(&mut self, info: &TxnInfo) -> Result<(), UnitError> {
        let mut v = Vec::new();
        crate::codec::put_txn_info(&mut v, info);
        self.put(keys::committed(&info.id), v)
    }

    pub(crate) fn remove_committed(&mut self, txn: &TxnId) -> Result<(), UnitError> {
        self.del(&keys::committed(txn)).map(|_| ())
    }
}
