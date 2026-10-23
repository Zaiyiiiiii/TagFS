//! 在写者状态上做一次改动：改树，分配和释放块。
//!
//! 一次改动出错时，调用者把写者状态恢复成改动前的样子（见 `BlockUnit::update`）；改动中写下的块
//! 都在空闲空间里，没有被任何版本引用。

use alloc::vec;
use alloc::vec::Vec;

use tagfs_core::{Change, EntityId, StageId, Tag, TxnId, TxnInfo, UnitError};
use tagfs_device::BlockDevice;

use crate::State;
use crate::codec::put_u64;
use crate::extent::ExtentSet;
use crate::keys;
use crate::view::{Extent, INLINE_MAX, Object, View, encode_prepared};

/// 准备记录切成这么大的片存进树里，每片一个条目。
const PREPARED_CHUNK: usize = 1024;

pub(crate) struct Work<'a, D: BlockDevice> {
    pub(crate) dev: &'a D,
    pub(crate) bs: u64,
    pub(crate) s: &'a mut State,
    /// 本次释放的、最近一次提交还在用的块：下一次提交之后才能再分配。
    pub(crate) durable: ExtentSet,
    /// 本次释放的、最近一次提交之后才分配的块：没有读者在读就能再分配。
    pub(crate) young: ExtentSet,
}

/// 一次大块写入的安排：在锁外把数据写进新分配的块，再回到锁里登记。
pub(crate) struct WritePlan {
    pub(crate) object: u64,
    /// 涉及的逻辑块 `[b0, b1)`。
    pub(crate) b0: u64,
    pub(crate) b1: u64,
    /// 首、尾两块如果只写一部分，原来的内容在哪个物理块（没有映射为 `None`，读出来是零）。
    pub(crate) head: Option<u64>,
    pub(crate) tail: Option<u64>,
    /// 新分配的块：起始、块数，按逻辑顺序覆盖 `[b0, b1)`。
    pub(crate) blocks: Vec<(u64, u64)>,
    /// 写完之后的长度。
    pub(crate) size: u64,
}

impl<D: BlockDevice> Work<'_, D> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, UnitError> {
        self.s.tree.get(self.dev, key)
    }

    fn put(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<(), UnitError> {
        self.s.tree.insert(self.dev, key, value)
    }

    fn del(&mut self, key: &[u8]) -> Result<bool, UnitError> {
        self.s.tree.remove(self.dev, key)
    }

    fn keys_with_prefix(&self, prefix: &[u8]) -> Result<Vec<Vec<u8>>, UnitError> {
        let mut out = Vec::new();
        self.s.tree.scan_prefix(self.dev, prefix, &mut |k, _| {
            out.push(k.to_vec());
            Ok(true)
        })?;
        Ok(out)
    }

    pub(crate) fn with_view<T>(&self, f: impl FnOnce(&View<'_, D>) -> Result<T, UnitError>) -> Result<T, UnitError> {
        f(&View { tree: &self.s.tree, dev: self.dev, bs: self.bs })
    }

    /// 分配 `n` 个块，尽量连续。
    pub(crate) fn alloc(&mut self, mut n: u64) -> Result<Vec<(u64, u64)>, UnitError> {
        let mut out = Vec::new();
        while n > 0 {
            let (s, l) = self.s.free.take(n).ok_or(UnitError::NoSpace)?;
            self.s.fresh.insert(s, l);
            out.push((s, l));
            n -= l;
        }
        Ok(out)
    }

    /// 释放一段块。
    pub(crate) fn release(&mut self, start: u64, len: u64) {
        let mut cursor = start;
        for (s, l) in self.s.fresh.remove_overlap(start, len) {
            self.durable.insert(cursor, s - cursor);
            self.young.insert(s, l);
            cursor = s + l;
        }
        self.durable.insert(cursor, start + len - cursor);
    }

    fn write_blocks(&self, block: u64, data: &[u8]) -> Result<(), UnitError> {
        self.dev.write_at(block * self.bs, data).map_err(|_| UnitError::Device)
    }

    // ---- 对象 ----

    pub(crate) fn object(&self, o: u64) -> Result<Object, UnitError> {
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

    /// 新的空对象，引用数为 `refs`。
    fn new_object(&mut self, refs: u64) -> Result<u64, UnitError> {
        let o = self.s.next_object;
        self.s.next_object += 1;
        self.put_object(o, &Object { refs, size: 0, inline: Some(Vec::new()) })?;
        Ok(o)
    }

    /// 回收对象和它的块。
    fn free_object(&mut self, o: u64) -> Result<(), UnitError> {
        self.unmap(o, 0, u64::MAX)?;
        self.del(&keys::object(o))?;
        Ok(())
    }

    /// 引用数减一，减到零就回收对象和它的块。
    fn unref(&mut self, o: u64) -> Result<(), UnitError> {
        let mut rec = self.object(o)?;
        rec.refs -= 1;
        if rec.refs > 0 {
            return self.put_object(o, &rec);
        }
        self.free_object(o)
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

    /// 内联的内容放不下了：改由区段存放，原来的内容写成第一块。
    fn uninline(&mut self, o: u64) -> Result<(), UnitError> {
        let mut rec = self.object(o)?;
        let Some(old) = rec.inline.take() else { return Ok(()) };
        if !old.is_empty() {
            let (s, _) = self.alloc(1)?[0];
            let mut block = vec![0u8; self.bs as usize];
            block[..old.len()].copy_from_slice(&old);
            self.write_blocks(s, &block)?;
            self.put_extent(o, 0, s, 1)?;
        }
        self.put_object(o, &rec)
    }

    /// 写对象内容。内联放得下就直接改内联的内容，否则返回大块写入的安排（[`Work::finish_write`] 登记）。
    pub(crate) fn plan_write(&mut self, o: u64, offset: u64, data: &[u8]) -> Result<Option<WritePlan>, UnitError> {
        let mut rec = self.object(o)?;
        let end = offset.checked_add(data.len() as u64).ok_or(UnitError::NoSpace)?;
        let size = rec.size.max(end);
        if let Some(inline) = &mut rec.inline
            && size as usize <= INLINE_MAX
        {
            inline.resize(size as usize, 0);
            inline[offset as usize..end as usize].copy_from_slice(data);
            rec.size = size;
            self.put_object(o, &rec)?;
            return Ok(None);
        }
        self.uninline(o)?;
        if data.is_empty() {
            // 只是变长：多出来的部分是空洞。
            let mut rec = self.object(o)?;
            rec.size = size;
            self.put_object(o, &rec)?;
            return Ok(None);
        }
        let bs = self.bs;
        let (b0, b1) = (offset / bs, end.div_ceil(bs));
        let phys = |w: &Self, b: u64| -> Result<Option<u64>, UnitError> {
            let hit = w.with_view(|v| v.extents_in(o, b, b + 1))?;
            Ok(hit.first().map(|&(l, p, _)| p + (b - l)))
        };
        let head = if offset.is_multiple_of(bs) { None } else { phys(self, b0)? };
        let tail = if end.is_multiple_of(bs) { None } else { phys(self, b1 - 1)? };
        let blocks = self.alloc(b1 - b0)?;
        for &(s, l) in &blocks {
            self.s.inflight.insert(s, l);
        }
        Ok(Some(WritePlan { object: o, b0, b1, head, tail, blocks, size }))
    }

    /// 在锁外写好的块登记成对象的内容，原来的块释放。
    pub(crate) fn finish_write(&mut self, plan: &WritePlan) -> Result<(), UnitError> {
        let o = plan.object;
        self.unmap(o, plan.b0, plan.b1)?;
        let mut logical = plan.b0;
        for &(s, l) in &plan.blocks {
            self.s.inflight.remove_overlap(s, l);
            self.put_extent(o, logical, s, l)?;
            logical += l;
        }
        let mut rec = self.object(o)?;
        rec.size = plan.size;
        self.put_object(o, &rec)
    }

    pub(crate) fn truncate_object(&mut self, o: u64, len: u64) -> Result<(), UnitError> {
        let mut rec = self.object(o)?;
        if let Some(inline) = &mut rec.inline {
            if len as usize <= INLINE_MAX {
                inline.resize(len as usize, 0);
                rec.size = len;
                return self.put_object(o, &rec);
            }
            // 变长到放不下：转成区段，多出来的部分是空洞。
            self.uninline(o)?;
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

    /// 对象的全部区段（复制用）。
    pub(crate) fn extents(&self, o: u64) -> Result<Vec<Extent>, UnitError> {
        self.with_view(|v| v.extents_in(o, 0, u64::MAX))
    }

    /// 实体改用一份新对象：内容是在锁外复制好的区段（逻辑块、物理块、块数），原来的对象引用数减一。
    /// 返回新对象。
    pub(crate) fn switch_to_copy(
        &mut self,
        id: &EntityId,
        from: u64,
        size: u64,
        layout: &[Extent],
    ) -> Result<u64, UnitError> {
        let copy = self.new_object(1)?;
        self.put_object(copy, &Object { refs: 1, size, inline: None })?;
        for &(logical, phys, n) in layout {
            self.s.inflight.remove_overlap(phys, n);
            self.put_extent(copy, logical, phys, n)?;
        }
        self.unref(from)?;
        self.set_entity_object(id, copy)?;
        Ok(copy)
    }

    /// 实体要改内容，对象被共享着：内联的对象当场复制一份给它（很小）。返回它独占的对象；
    /// 对象用区段存放时返回 `Err(对象)`，由调用者在锁外复制。
    pub(crate) fn own_small(&mut self, id: &EntityId) -> Result<Result<u64, u64>, UnitError> {
        let o = self.entity_object(id)?;
        let rec = self.object(o)?;
        if rec.refs == 1 {
            return Ok(Ok(o));
        }
        if rec.inline.is_none() {
            return Ok(Err(o));
        }
        let copy = self.new_object(1)?;
        self.put_object(copy, &Object { refs: 1, ..rec })?;
        self.unref(o)?;
        self.set_entity_object(id, copy)?;
        Ok(Ok(copy))
    }

    // ---- 暂存 ----

    pub(crate) fn new_stage(&mut self) -> Result<StageId, UnitError> {
        let o = self.new_object(0)?;
        self.put(keys::staged(o), self.s.owner.to_le_bytes().to_vec())?;
        Ok(StageId(o))
    }

    /// 暂存对应的对象；不存在时报错。
    pub(crate) fn staged_object(&self, stage: StageId) -> Result<u64, UnitError> {
        self.with_view(|v| v.staged(stage))
    }

    pub(crate) fn discard_stage(&mut self, stage: StageId) -> Result<(), UnitError> {
        let o = self.staged_object(stage)?;
        self.del(&keys::staged(o))?;
        self.free_object(o)
    }

    /// 用掉暂存：对象归一个实体，引用数为一。
    fn take_stage(&mut self, stage: StageId) -> Result<u64, UnitError> {
        let o = self.staged_object(stage)?;
        self.del(&keys::staged(o))?;
        let mut rec = self.object(o)?;
        rec.refs = 1;
        self.put_object(o, &rec)?;
        Ok(o)
    }

    /// 实体的内容换成暂存的内容，原来的对象引用数减一。
    pub(crate) fn adopt(&mut self, id: &EntityId, stage: StageId) -> Result<(), UnitError> {
        let old = self.entity_object(id)?;
        let o = self.take_stage(stage)?;
        self.unref(old)?;
        self.set_entity_object(id, o)
    }

    // ---- 实体 ----

    pub(crate) fn entity_object(&self, id: &EntityId) -> Result<u64, UnitError> {
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
        self.set_entity_object(id, o)?;
        for t in tags {
            self.add_tag(id, t)?;
        }
        Ok(())
    }

    pub(crate) fn apply(&mut self, change: &Change) -> Result<(), UnitError> {
        match change {
            Change::Create { id, tags, content } => {
                if self.get(&keys::entity(id))?.is_some() {
                    return Err(UnitError::Exists(*id));
                }
                let o = match content {
                    Some(stage) => self.take_stage(*stage)?,
                    None => self.new_object(1)?,
                };
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
        }
    }

    // ---- 事务记录 ----

    /// 记下准备记录。其中引用的暂存必须存在：它们从此归这条记录。
    pub(crate) fn put_prepared(&mut self, info: &TxnInfo, changes: &[Change]) -> Result<(), UnitError> {
        for c in changes {
            if let Change::Create { content: Some(stage), .. } = c {
                self.staged_object(*stage)?;
            }
        }
        self.remove_prepared(&info.id)?;
        let payload = encode_prepared(info, changes);
        for (i, part) in payload.chunks(PREPARED_CHUNK).enumerate() {
            self.put(keys::prepared_chunk(&info.id, i as u32), part.to_vec())?;
        }
        self.put(keys::txn_owner(&info.id), self.s.owner.to_le_bytes().to_vec())
    }

    /// 删掉准备记录，返回它是否存在。
    pub(crate) fn remove_prepared(&mut self, txn: &TxnId) -> Result<bool, UnitError> {
        let ks = self.keys_with_prefix(&keys::prepared(txn))?;
        for k in &ks {
            self.del(k)?;
        }
        self.del(&keys::txn_owner(txn))?;
        Ok(!ks.is_empty())
    }

    pub(crate) fn prepared_one(&self, txn: &TxnId) -> Result<Option<(TxnInfo, Vec<Change>)>, UnitError> {
        self.with_view(|v| v.prepared_one(txn))
    }

    pub(crate) fn put_committed(&mut self, info: &TxnInfo) -> Result<(), UnitError> {
        let mut v = Vec::new();
        crate::codec::put_txn_info(&mut v, info);
        put_u64(&mut v, self.s.owner);
        self.put(keys::committed(&info.id), v)
    }

    pub(crate) fn put_aux(&mut self, key: &[u8], value: Vec<u8>) -> Result<(), UnitError> {
        self.put(keys::aux(key), value)
    }

    pub(crate) fn remove_aux(&mut self, key: &[u8]) -> Result<(), UnitError> {
        self.del(&keys::aux(key)).map(|_| ())
    }

    pub(crate) fn remove_committed(&mut self, txn: &TxnId) -> Result<(), UnitError> {
        self.del(&keys::committed(txn)).map(|_| ())
    }
}
