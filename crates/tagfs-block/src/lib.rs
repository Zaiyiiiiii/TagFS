//! 块格式存储单元：建在任意 [`BlockDevice`] 上（宿主机单文件、裸盘、内存）。
//!
//! 盘上布局：
//!
//! - 块 0、块 1：两份超级块，按提交号交替写。挂载时取校验通过、提交号最大的那份。
//! - 其余的块：树节点、对象内容、空闲表，全部写时复制，没有固定位置。
//!
//! 一次提交：先写数据块和新的树节点、新的空闲表，刷盘；再写超级块，刷盘。超级块写完就是提交点，
//! 断电后要么是旧的状态，要么是新的状态。
//!
//! 被替换下来的块在提交之后才能重新分配，并且要等持有旧快照的读者都结束。

#![no_std]

extern crate alloc;

mod codec;
mod crc;
mod extent;
mod keys;
mod tree;
mod view;
mod work;

#[cfg(test)]
mod tests;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::{Ref, RefCell};
use core::fmt;

use tagfs_core::{Change, EntityId, EntityRecord, Probe, Snapshot, TxnId, TxnInfo, Unit, UnitError, UnitId};
use tagfs_device::BlockDevice;

use crate::codec::{Reader, put_u32, put_u64};
use crate::crc::crc32c;
use crate::extent::ExtentSet;
use crate::tree::Tree;
use crate::view::View;
use crate::work::Work;

const MAGIC: &[u8; 8] = b"TAGFSBLK";
const VERSION: u32 = 1;
/// 支持的最小块大小。树的单个条目上限由它决定。
pub const MIN_BLOCK_SIZE: u32 = 4096;
const MIN_BLOCKS: u64 = 16;
/// 扩容时至少加这么多块（16 MiB）。
const GROW_MIN_BLOCKS: u64 = 4096;

/// 打开或格式化失败的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenError {
    /// 两份超级块都无效：没有格式化过，或者都损坏了。
    NotFormatted,
    /// 格式版本不认识。
    UnsupportedVersion(u32),
    /// 块大小不是 2 的幂或小于 [`MIN_BLOCK_SIZE`]，或和超级块里记录的不一致。
    BadBlockSize,
    /// 设备太小，或比超级块里记录的小。
    TooSmall,
    /// 元数据损坏或读写失败。
    Unit(UnitError),
}

impl From<UnitError> for OpenError {
    fn from(e: UnitError) -> Self {
        OpenError::Unit(e)
    }
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpenError::NotFormatted => f.write_str("no valid superblock"),
            OpenError::UnsupportedVersion(v) => write!(f, "unsupported format version {v}"),
            OpenError::BadBlockSize => f.write_str("unsupported block size"),
            OpenError::TooSmall => f.write_str("device too small"),
            OpenError::Unit(e) => write!(f, "{e}"),
        }
    }
}

impl core::error::Error for OpenError {}

#[derive(Clone, Debug)]
struct Super {
    block_size: u32,
    blocks: u64,
    unit: UnitId,
    generation: u64,
    /// 树根；0 表示空树（块 0 是超级块，不会是节点）。
    root: u64,
    /// 空闲表的第一块；0 表示没有空闲块。
    free_head: u64,
    next_object: u64,
}

impl Super {
    fn encode(&self, bs: usize) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(MAGIC);
        put_u32(&mut b, VERSION);
        put_u32(&mut b, self.block_size);
        put_u64(&mut b, self.blocks);
        b.extend_from_slice(&self.unit.0);
        put_u64(&mut b, self.generation);
        put_u64(&mut b, self.root);
        put_u64(&mut b, self.free_head);
        put_u64(&mut b, self.next_object);
        let crc = crc32c(&b);
        put_u32(&mut b, crc);
        b.resize(bs, 0);
        b
    }

    fn decode(b: &[u8]) -> Result<Super, OpenError> {
        const LEN: usize = 8 + 4 + 4 + 8 + 16 + 8 * 4;
        if b[..8] != MAGIC[..] || u32::from_le_bytes(b[LEN..LEN + 4].try_into().unwrap()) != crc32c(&b[..LEN]) {
            return Err(OpenError::NotFormatted);
        }
        let mut r = Reader::new(&b[8..LEN]);
        let version = r.u32()?;
        if version != VERSION {
            return Err(OpenError::UnsupportedVersion(version));
        }
        Ok(Super {
            block_size: r.u32()?,
            blocks: r.u64()?,
            unit: UnitId(r.id16()?),
            generation: r.u64()?,
            root: r.u64()?,
            free_head: r.u64()?,
            next_object: r.u64()?,
        })
    }
}

/// 空闲表块：CRC（覆盖 4 到块尾）、本块区段数 u32、下一块 u64、区段（起始 u64、块数 u64）。
fn free_list_per_block(bs: usize) -> usize {
    (bs - 16) / 16
}

/// 附加记录的一项改动。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Aux {
    Put(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
}

/// 一条附加记录：键和值。
pub type AuxRecord = (Vec<u8>, Vec<u8>);

/// 内容的来源，按偏移分段读取，不必整个放进内存。可能被从头再读一遍（提交因扩容重做时）。
pub trait Source {
    /// 从 `offset` 读到 `buf`，返回读到的字节数；0 表示结束。
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize, UnitError>;
}

/// 用 `source` 的全部内容替换实体 `id` 的内容。
pub struct Fill<'a> {
    pub id: EntityId,
    pub source: &'a mut dyn Source,
}

/// 一次提交的全部内容，见 [`BlockUnit::commit_batch`]。
#[derive(Default)]
pub struct Batch<'a> {
    /// 先做：替换这些实体的内容。
    pub fills: Vec<Fill<'a>>,
    /// 实体的修改，按顺序应用。
    pub changes: &'a [Change],
    /// 附加记录的改动，在修改之后应用。
    pub aux: &'a [Aux],
    /// 了结这条准备记录：删掉它（其中的修改不会再被应用，调用者自己放进 `changes`）；
    /// 第二项为 `true` 时同时记下“这个事务已提交”。
    pub finish: Option<(TxnId, bool)>,
}

/// 快照登记：提交号 → 持有它的读者数。
type Readers = Rc<RefCell<BTreeMap<u64, usize>>>;

pub struct BlockUnit<D: BlockDevice> {
    dev: Rc<RefCell<D>>,
    bs: usize,
    sb: Super,
    /// 可以分配的块。
    free: ExtentSet,
    /// 当前空闲表占用的块。
    free_list: Vec<u64>,
    /// 已经释放、但可能还有旧快照在读的块：释放时的提交号 → 块。
    quarantine: Vec<(u64, ExtentSet)>,
    readers: Readers,
    /// 写超级块出错后不知道盘上是哪个状态，不再接受写入。
    broken: bool,
}

fn device_err<T>(r: Result<T, tagfs_device::DeviceError>) -> Result<T, UnitError> {
    r.map_err(|_| UnitError::Device)
}

impl<D: BlockDevice + 'static> BlockUnit<D> {
    /// 在设备上建一个空单元，覆盖原有内容。
    pub fn format(mut dev: D, unit: UnitId) -> Result<Self, OpenError> {
        let bs = dev.block_size();
        if bs < MIN_BLOCK_SIZE || !bs.is_power_of_two() {
            return Err(OpenError::BadBlockSize);
        }
        let blocks = dev.capacity() / bs as u64;
        if blocks < MIN_BLOCKS {
            return Err(OpenError::TooSmall);
        }
        let zero = vec![0u8; bs as usize];
        for slot in 0..2 {
            device_err(dev.write_at(slot * bs as u64, &zero))?;
        }
        let mut free = ExtentSet::new();
        free.insert(2, blocks - 2);
        let mut me = BlockUnit {
            dev: Rc::new(RefCell::new(dev)),
            bs: bs as usize,
            sb: Super { block_size: bs, blocks, unit, generation: 0, root: 0, free_head: 0, next_object: 1 },
            free,
            free_list: Vec::new(),
            quarantine: Vec::new(),
            readers: Rc::default(),
            broken: false,
        };
        let w = me.work();
        me.commit(w)?;
        Ok(me)
    }

    /// 挂载已有的单元。
    pub fn open(dev: D) -> Result<Self, OpenError> {
        let bs = dev.block_size();
        if bs < MIN_BLOCK_SIZE || !bs.is_power_of_two() {
            return Err(OpenError::BadBlockSize);
        }
        let mut best: Option<Super> = None;
        let mut first_err = OpenError::NotFormatted;
        for slot in 0..2 {
            let buf = tree::read_block(&dev, slot, bs as usize)?;
            match Super::decode(&buf) {
                Ok(sb) if best.as_ref().is_none_or(|b| sb.generation > b.generation) => best = Some(sb),
                Ok(_) => {}
                Err(e @ OpenError::UnsupportedVersion(_)) => first_err = e,
                Err(_) => {}
            }
        }
        let sb = best.ok_or(first_err)?;
        if sb.block_size != bs {
            return Err(OpenError::BadBlockSize);
        }
        if sb.blocks > dev.capacity() / bs as u64 {
            return Err(OpenError::TooSmall);
        }
        let (free, free_list) = read_free_list(&dev, bs as usize, sb.free_head, sb.blocks)?;
        Ok(BlockUnit {
            dev: Rc::new(RefCell::new(dev)),
            bs: bs as usize,
            sb,
            free,
            free_list,
            quarantine: Vec::new(),
            readers: Rc::default(),
            broken: false,
        })
    }

    /// 底层设备（测试里用来模拟断电）。
    pub fn device(&self) -> Ref<'_, D> {
        self.dev.borrow()
    }

    /// 当前提交号。
    pub fn generation(&self) -> u64 {
        self.sb.generation
    }

    /// 空闲块数（含等待旧快照结束的）。
    pub fn free_blocks(&self) -> u64 {
        self.free.blocks() + self.quarantine.iter().map(|(_, s)| s.blocks()).sum::<u64>()
    }

    /// 已经释放、但还有旧快照在读、暂时不能再分配的块数。
    pub fn pinned_blocks(&self) -> u64 {
        self.quarantine.iter().map(|(_, s)| s.blocks()).sum()
    }

    /// 全部块数（含两份超级块）。
    pub fn total_blocks(&self) -> u64 {
        self.sb.blocks
    }

    fn root(&self) -> Option<u64> {
        (self.sb.root != 0).then_some(self.sb.root)
    }

    fn work(&self) -> Work<D> {
        Work {
            dev: self.dev.clone(),
            bs: self.bs as u64,
            tree: Tree::open(self.root(), self.bs),
            free: self.free.clone(),
            fresh: ExtentSet::new(),
            released: ExtentSet::new(),
            next_object: self.sb.next_object,
        }
    }

    fn with_view<T>(&self, f: impl FnOnce(&View<'_, D>) -> Result<T, UnitError>) -> Result<T, UnitError> {
        let tree = Tree::open(self.root(), self.bs);
        let dev = self.dev.borrow();
        f(&View { tree: &tree, dev: &*dev, bs: self.bs as u64 })
    }

    /// 把工作区写成新的提交。失败时单元状态不变。
    fn commit(&mut self, mut w: Work<D>) -> Result<(), UnitError> {
        if self.broken {
            return Err(UnitError::Device);
        }
        let generation = self.sb.generation + 1;
        let root = {
            let mut dev = self.dev.borrow_mut();
            let (free, fresh) = (&mut w.free, &mut w.fresh);
            w.tree.flush(&mut *dev, &mut || alloc_one(free, fresh), generation)?
        };
        for b in core::mem::take(&mut w.tree.replaced) {
            w.release(b, 1);
        }
        for &b in &self.free_list {
            w.release(b, 1);
        }

        // 空闲表本身也要占块：先分配，再把分配后的空闲集合写进去。
        let per = free_list_per_block(self.bs);
        let mut list_blocks = Vec::new();
        let persisted = loop {
            let mut all = w.free.clone();
            all.extend(&w.released);
            for (_, q) in &self.quarantine {
                all.extend(q);
            }
            if list_blocks.len() >= all.count().div_ceil(per) {
                break all;
            }
            list_blocks.push(alloc_one(&mut w.free, &mut w.fresh)?);
        };
        {
            let mut dev = self.dev.borrow_mut();
            let extents: Vec<(u64, u64)> = persisted.iter().collect();
            let mut parts = extents.chunks(per);
            for (i, &b) in list_blocks.iter().enumerate() {
                let part = parts.next().unwrap_or(&[]);
                let next = list_blocks.get(i + 1).copied().unwrap_or(0);
                let mut buf = vec![0u8; 4];
                put_u32(&mut buf, part.len() as u32);
                put_u64(&mut buf, next);
                for &(s, l) in part {
                    put_u64(&mut buf, s);
                    put_u64(&mut buf, l);
                }
                buf.resize(self.bs, 0);
                let crc = crc32c(&buf[4..]);
                buf[..4].copy_from_slice(&crc.to_le_bytes());
                device_err(dev.write_at(b * self.bs as u64, &buf))?;
            }
            device_err(dev.flush())?;
        }

        let sb = Super {
            generation,
            root: root.unwrap_or(0),
            free_head: list_blocks.first().copied().unwrap_or(0),
            next_object: w.next_object,
            ..self.sb.clone()
        };
        {
            let mut dev = self.dev.borrow_mut();
            let slot = generation % 2;
            let written = dev.write_at(slot * self.bs as u64, &sb.encode(self.bs)).and_then(|_| dev.flush());
            if written.is_err() {
                self.broken = true;
                return Err(UnitError::Device);
            }
        }

        self.sb = sb;
        self.free = w.free;
        self.free_list = list_blocks;
        if !w.released.is_empty() {
            self.quarantine.push((generation, w.released));
        }
        self.reclaim();
        Ok(())
    }

    /// 没有读者再看得到的已释放块，放回可分配的空闲空间。
    fn reclaim(&mut self) {
        let oldest = self.readers.borrow().keys().next().copied();
        let (ready, waiting): (Vec<_>, Vec<_>) =
            core::mem::take(&mut self.quarantine).into_iter().partition(|(g, _)| oldest.is_none_or(|o| o >= *g));
        self.quarantine = waiting;
        for (_, set) in ready {
            self.free.extend(&set);
        }
    }

    /// 在工作区里做改动并提交。空间不够时，设备能扩容就扩容后整个重做。
    fn update(&mut self, mut f: impl FnMut(&mut Work<D>) -> Result<(), UnitError>) -> Result<(), UnitError> {
        loop {
            let mut w = self.work();
            match f(&mut w).and_then(|()| self.commit(w)) {
                Err(UnitError::NoSpace) if self.grow() => continue,
                r => return r,
            }
        }
    }

    /// 设备扩容（至少翻倍），新增的块进空闲空间。下一次提交把新的大小写进超级块；在那之前断电，
    /// 超级块还是旧的大小，多出来的部分没人用。返回是否扩成了。
    fn grow(&mut self) -> bool {
        let bs = self.bs as u64;
        let old = self.sb.blocks;
        let want = old.saturating_mul(2).max(old + GROW_MIN_BLOCKS);
        let mut dev = self.dev.borrow_mut();
        if dev.grow(want.saturating_mul(bs)).is_err() {
            return false;
        }
        let now = dev.capacity() / bs;
        if now <= old {
            return false;
        }
        self.free.insert(old, now - old);
        self.sb.blocks = now;
        true
    }

    /// 把一批修改、附加记录和准备记录的了结作为一次提交原子地完成。
    /// 建在块格式单元之上的单元（如目录单元）靠它把自己的记录和实体的修改放进同一次提交。
    pub fn commit_batch(&mut self, batch: &mut Batch<'_>) -> Result<(), UnitError> {
        self.update(|w| {
            for fill in batch.fills.iter_mut() {
                w.fill(&fill.id, &mut *fill.source)?;
            }
            for c in batch.changes {
                w.apply(c)?;
            }
            for a in batch.aux {
                match a {
                    Aux::Put(k, v) => w.put_aux(k, v.clone())?,
                    Aux::Delete(k) => w.remove_aux(k)?,
                }
            }
            if let Some((txn, as_primary)) = batch.finish {
                let (info, _) = w.prepared_one(&txn)?.ok_or(UnitError::UnknownTxn(txn))?;
                w.remove_prepared(&txn)?;
                if as_primary {
                    w.put_committed(&info)?;
                }
            }
            Ok(())
        })
    }

    /// 读一条附加记录。
    pub fn aux_get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, UnitError> {
        let tree = Tree::open(self.root(), self.bs);
        tree.get(&*self.dev.borrow(), &keys::aux(key))
    }

    /// 键以 `prefix` 开头的全部附加记录，按键排序。
    pub fn aux_list(&self, prefix: &[u8]) -> Result<Vec<AuxRecord>, UnitError> {
        let tree = Tree::open(self.root(), self.bs);
        let mut out = Vec::new();
        tree.scan_prefix(&*self.dev.borrow(), &keys::aux(prefix), &mut |k, v| {
            out.push((k[1..].to_vec(), v.to_vec()));
            Ok(true)
        })?;
        Ok(out)
    }

    /// 全面检查：每个块恰好属于超级块、树节点、对象内容、空闲表、空闲空间中的一种；
    /// 对象引用数和引用它的实体数一致；实体的 Tag 和 Tag 索引一致。
    pub fn verify(&self) -> Result<(), UnitError> {
        let mut used = ExtentSet::new();
        let claim = |s: u64, l: u64, used: &mut ExtentSet| -> Result<(), UnitError> {
            if l == 0 || s + l > self.sb.blocks || used.overlaps(s, l) {
                return Err(UnitError::Corrupt);
            }
            used.insert(s, l);
            Ok(())
        };
        claim(0, 2, &mut used)?;
        for &b in &self.free_list {
            claim(b, 1, &mut used)?;
        }
        for (s, l) in self.free.iter() {
            claim(s, l, &mut used)?;
        }
        for (_, q) in &self.quarantine {
            for (s, l) in q.iter() {
                claim(s, l, &mut used)?;
            }
        }
        let tree = Tree::open(self.root(), self.bs);
        let dev = self.dev.borrow();
        let mut nodes = Vec::new();
        tree.visit_nodes(&*dev, &mut |b| nodes.push(b))?;
        for b in nodes {
            claim(b, 1, &mut used)?;
        }

        let mut refs: BTreeMap<u64, u64> = BTreeMap::new();
        let mut entity_tags = Vec::new();
        let mut index = Vec::new();
        let mut objects = Vec::new();
        let mut extents = Vec::new();
        tree.scan(&*dev, &[], &mut |k, v| {
            match k[0] {
                keys::ENTITY => *refs.entry(Reader::new(v).u64()?).or_default() += 1,
                keys::ENTITY_TAG => {
                    let id = EntityId(Reader::new(&k[1..17]).id16()?);
                    entity_tags.push((Reader::new(&k[17..]).tag()?, id));
                }
                keys::INDEX => {
                    let id = EntityId(k[k.len() - 16..].try_into().unwrap());
                    index.push((Reader::new(&k[1..k.len() - 16]).tag()?, id));
                }
                keys::OBJECT => objects.push((Reader::new(&k[1..]).be_u64()?, v.to_vec())),
                keys::EXTENT => {
                    let o = Reader::new(&k[1..]).be_u64()?;
                    extents.push(view::decode_extent(o, k, v)?);
                }
                keys::PREPARED | keys::COMMITTED | keys::AUX => {}
                _ => return Err(UnitError::Corrupt),
            }
            Ok(true)
        })?;
        entity_tags.sort();
        index.sort();
        if entity_tags != index {
            return Err(UnitError::Corrupt);
        }
        let v = View { tree: &tree, dev: &*dev, bs: self.bs as u64 };
        if objects.len() != refs.len() {
            return Err(UnitError::Corrupt);
        }
        for (o, _) in &objects {
            let rec = v.object(*o)?;
            if refs.get(o) != Some(&rec.refs) || *o >= self.sb.next_object {
                return Err(UnitError::Corrupt);
            }
        }
        for (_, p, n) in extents {
            claim(p, n, &mut used)?;
        }
        if used.blocks() != self.sb.blocks {
            return Err(UnitError::Corrupt);
        }
        Ok(())
    }
}

/// 设备上是不是块格式单元（至少有一份超级块校验通过，格式版本不论）。只读，不改设备。
pub fn is_formatted<D: BlockDevice + ?Sized>(dev: &D) -> bool {
    let bs = dev.block_size();
    if bs < MIN_BLOCK_SIZE || !bs.is_power_of_two() {
        return false;
    }
    (0..2).any(|slot| {
        tree::read_block(dev, slot, bs as usize)
            .is_ok_and(|buf| !matches!(Super::decode(&buf), Err(OpenError::NotFormatted)))
    })
}

fn alloc_one(free: &mut ExtentSet, fresh: &mut ExtentSet) -> Result<u64, UnitError> {
    let (b, _) = free.take(1).ok_or(UnitError::NoSpace)?;
    fresh.insert(b, 1);
    Ok(b)
}

fn read_free_list<D: BlockDevice + ?Sized>(
    dev: &D,
    bs: usize,
    head: u64,
    blocks: u64,
) -> Result<(ExtentSet, Vec<u64>), UnitError> {
    let mut free = ExtentSet::new();
    let mut list = Vec::new();
    let mut next = head;
    while next != 0 {
        if next >= blocks || list.contains(&next) {
            return Err(UnitError::Corrupt);
        }
        list.push(next);
        let buf = tree::read_block(dev, next, bs)?;
        if u32::from_le_bytes(buf[..4].try_into().unwrap()) != crc32c(&buf[4..]) {
            return Err(UnitError::Corrupt);
        }
        let mut r = Reader::new(&buf[4..]);
        let count = r.u32()? as usize;
        next = r.u64()?;
        if count > free_list_per_block(bs) {
            return Err(UnitError::Corrupt);
        }
        for _ in 0..count {
            let (s, l) = (r.u64()?, r.u64()?);
            if s < 2 || l == 0 || s + l > blocks || free.overlaps(s, l) {
                return Err(UnitError::Corrupt);
            }
            free.insert(s, l);
        }
    }
    Ok((free, list))
}

impl<D: BlockDevice + 'static> Unit for BlockUnit<D> {
    fn id(&self) -> UnitId {
        self.sb.unit
    }

    fn snapshot(&self) -> Box<dyn Snapshot> {
        let generation = self.sb.generation;
        *self.readers.borrow_mut().entry(generation).or_default() += 1;
        Box::new(BlockSnapshot {
            dev: self.dev.clone(),
            tree: Tree::open(self.root(), self.bs),
            bs: self.bs as u64,
            generation,
            readers: self.readers.clone(),
        })
    }

    fn apply(&mut self, changes: &[Change]) -> Result<(), UnitError> {
        self.update(|w| changes.iter().try_for_each(|c| w.apply(c)))
    }

    fn prepare(&mut self, txn: &TxnInfo, changes: &[Change]) -> Result<(), UnitError> {
        self.update(|w| w.put_prepared(txn, changes))
    }

    fn prepared(&self) -> Result<Vec<(TxnInfo, Vec<Change>)>, UnitError> {
        self.with_view(|v| v.prepared())
    }

    fn commit_prepared(&mut self, txn: TxnId, as_primary: bool) -> Result<(), UnitError> {
        let (_, changes) = self.with_view(|v| v.prepared_one(&txn))?.ok_or(UnitError::UnknownTxn(txn))?;
        self.commit_batch(&mut Batch { changes: &changes, finish: Some((txn, as_primary)), ..Batch::default() })
    }

    fn abort_prepared(&mut self, txn: TxnId) -> Result<(), UnitError> {
        self.update(|w| if w.remove_prepared(&txn)? { Ok(()) } else { Err(UnitError::UnknownTxn(txn)) })
    }

    fn committed(&self) -> Result<Vec<TxnInfo>, UnitError> {
        self.with_view(|v| v.committed())
    }

    fn forget(&mut self, txn: TxnId) -> Result<(), UnitError> {
        self.update(|w| w.remove_committed(&txn))
    }

    fn check(&mut self) -> Result<(), UnitError> {
        self.verify()
    }
}

struct BlockSnapshot<D: BlockDevice> {
    dev: Rc<RefCell<D>>,
    tree: Tree,
    bs: u64,
    generation: u64,
    readers: Readers,
}

impl<D: BlockDevice> BlockSnapshot<D> {
    fn with_view<T>(&self, f: impl FnOnce(&View<'_, D>) -> Result<T, UnitError>) -> Result<T, UnitError> {
        let dev = self.dev.borrow();
        f(&View { tree: &self.tree, dev: &*dev, bs: self.bs })
    }
}

impl<D: BlockDevice> Drop for BlockSnapshot<D> {
    fn drop(&mut self) {
        let mut readers = self.readers.borrow_mut();
        if let Some(n) = readers.get_mut(&self.generation) {
            *n -= 1;
            if *n == 0 {
                readers.remove(&self.generation);
            }
        }
    }
}

impl<D: BlockDevice> Snapshot for BlockSnapshot<D> {
    fn get(&self, id: &EntityId) -> Result<Option<EntityRecord>, UnitError> {
        self.with_view(|v| v.record(id))
    }

    fn probe(&self, probe: &Probe<'_>) -> Result<Option<Vec<EntityId>>, UnitError> {
        self.with_view(|v| v.probe(probe)).map(Some)
    }

    fn all(&self) -> Result<Vec<EntityId>, UnitError> {
        self.with_view(|v| v.all())
    }

    fn read(&self, id: &EntityId, offset: u64, buf: &mut [u8]) -> Result<usize, UnitError> {
        self.with_view(|v| {
            let o = v.entity_object(id)?.ok_or(UnitError::NotFound(*id))?;
            v.read_object(o, offset, buf)
        })
    }
}
