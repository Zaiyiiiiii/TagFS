//! 块格式存储单元：建在任意 [`BlockDevice`] 上（宿主机单文件、裸盘、内存）。
//!
//! 盘上布局：
//!
//! - 块 0、块 1：两份超级块，按提交号交替写。挂载时取校验通过、提交号最大的那份。
//! - 其余的块：树节点、对象内容、空闲表，全部写时复制，没有固定位置。
//!
//! 一次提交：先写树节点和新的空闲表，刷盘；再写超级块，刷盘。超级块写完就是提交点，
//! 断电后要么是旧的状态，要么是新的状态。
//!
//! 内存里有一个“当前版本”（一棵树的根）：读者拿着某个版本读，不加锁，也不挡写者。写者一次一个，
//! 在当前版本上改出新版本再公布。
//!
//! - 元数据修改（[`Unit::apply`] 等）改完就提交，提交之后才公布。
//! - 内容写入不提交：数据在锁外直接写进新分配的块，再回到锁里把这些块登记进对象、公布新版本。
//!   这些登记在下一次提交（[`Unit::sync`]、任何元数据修改）时随树一起落盘；之前断电，
//!   新写的块在盘上的空闲表里仍是空闲的，单元回到上一次提交的状态。
//!
//! 被替换下来的块要等两件事才能再分配：盘上最近一次提交不再引用它（否则断电后会读到别的内容），
//! 并且持有旧版本的读者都结束了。

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
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use tagfs_core::{Change, EntityId, EntityRecord, Probe, Snapshot, StageId, TxnId, TxnInfo, Unit, UnitError, UnitId};
use tagfs_device::BlockDevice;
use tagfs_sync::{Mutex, MutexGuard};

use crate::codec::{Reader, put_u32, put_u64};
use crate::crc::crc32c;
use crate::extent::ExtentSet;
use crate::tree::Tree;
use crate::view::{Extent, View};
use crate::work::{Work, WritePlan};

const MAGIC: &[u8; 8] = b"TAGFSBLK";
const VERSION: u32 = 2;
/// 支持的最小块大小。树的单个条目上限由它决定。
pub const MIN_BLOCK_SIZE: u32 = 4096;
const MIN_BLOCKS: u64 = 16;
/// 扩容时至少加这么多块（16 MiB）。
const GROW_MIN_BLOCKS: u64 = 4096;
/// 在锁外复制对象内容时一次搬多少块。
const COPY_CHUNK_BLOCKS: u64 = 64;

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

/// 一次提交的全部内容，见 [`BlockUnit::commit_batch`]。
#[derive(Default)]
pub struct Batch<'a> {
    /// 先做：这些实体的内容换成暂存的内容，暂存随之用掉。
    pub adopt: &'a [(EntityId, StageId)],
    /// 实体的修改，按顺序应用。
    pub changes: &'a [Change],
    /// 附加记录的改动，在修改之后应用。
    pub aux: &'a [Aux],
    /// 了结这条准备记录：删掉它（其中的修改不会再被应用，调用者自己放进 `changes`）；
    /// 第二项为 `true` 时同时记下“这个事务已提交”。
    pub finish: Option<(TxnId, bool)>,
}

/// 释放了、暂时还不能再分配的块。
#[derive(Clone, Debug)]
struct Released {
    /// 从这个版本起不再引用它们。
    epoch: u64,
    blocks: ExtentSet,
    /// 最近一次提交还引用它们：下一次提交之后才算空闲。
    after_commit: bool,
}

/// 写者的状态。一次改动失败时整个恢复成改动前的样子，所以要能便宜地复制（树只复制根）。
#[derive(Clone)]
pub(crate) struct State {
    /// 最近一次提交的超级块（块数可能因扩容而更新）。
    sb: Super,
    pub(crate) tree: Tree,
    pub(crate) next_object: u64,
    /// 可以分配的块。
    pub(crate) free: ExtentSet,
    /// 最近一次提交之后才分配的块：盘上的状态没有引用它们。
    pub(crate) fresh: ExtentSet,
    /// 已经分配、正在锁外写入、还没登记的块。
    pub(crate) inflight: ExtentSet,
    pending: Vec<Released>,
    /// 当前空闲表占用的块。
    free_list: Vec<u64>,
    /// 当前公布的版本号。
    epoch: u64,
    /// 有没提交的改动。
    dirty: bool,
    /// 写超级块出错后不知道盘上是哪个状态，不再接受写入。
    broken: bool,
}

/// 读者看到的当前版本，和持有各个版本的读者数。
struct Current {
    tree: Tree,
    epoch: u64,
    readers: BTreeMap<u64, usize>,
}

struct Shared<D> {
    dev: D,
    bs: usize,
    unit: UnitId,
    current: Mutex<Current>,
    state: Mutex<State>,
}

pub struct BlockUnit<D: BlockDevice> {
    s: Arc<Shared<D>>,
}

/// 登记一个读者持有某个版本；放掉时注销。
struct Pin<D: BlockDevice> {
    s: Arc<Shared<D>>,
    epoch: u64,
}

impl<D: BlockDevice> Drop for Pin<D> {
    fn drop(&mut self) {
        let mut cur = self.s.current.lock();
        if let Some(n) = cur.readers.get_mut(&self.epoch) {
            *n -= 1;
            if *n == 0 {
                cur.readers.remove(&self.epoch);
            }
        }
    }
}

fn device_err<T>(r: Result<T, tagfs_device::DeviceError>) -> Result<T, UnitError> {
    r.map_err(|_| UnitError::Device)
}

/// 一次大块写入在锁外写好之前分配的块，不用了就直接还回去（它们从没被登记过）。
fn unreserve(st: &mut State, blocks: &[(u64, u64)]) {
    for &(s, l) in blocks {
        st.inflight.remove_overlap(s, l);
        st.fresh.remove_overlap(s, l);
        st.free.insert(s, l);
    }
}

/// 一次改动的收尾：本次释放的块按“从下一个版本起不再引用”记下，树里被复制走的盘上节点也算释放。
fn settle<D: BlockDevice>(mut w: Work<'_, D>) {
    for b in core::mem::take(&mut w.s.tree.replaced) {
        w.release(b, 1);
    }
    let epoch = w.s.epoch + 1;
    for (blocks, after_commit) in [(w.durable, true), (w.young, false)] {
        if !blocks.is_empty() {
            w.s.pending.push(Released { epoch, blocks, after_commit });
        }
    }
}

impl<D: BlockDevice + 'static> BlockUnit<D> {
    fn new(dev: D, sb: Super, free: ExtentSet, free_list: Vec<u64>) -> Self {
        let bs = sb.block_size as usize;
        let tree = Tree::open((sb.root != 0).then_some(sb.root), bs);
        let state = State {
            tree: tree.view(),
            next_object: sb.next_object,
            free,
            fresh: ExtentSet::new(),
            inflight: ExtentSet::new(),
            pending: Vec::new(),
            free_list,
            epoch: 0,
            dirty: false,
            broken: false,
            sb: sb.clone(),
        };
        BlockUnit {
            s: Arc::new(Shared {
                dev,
                bs,
                unit: sb.unit,
                current: Mutex::new(Current { tree, epoch: 0, readers: BTreeMap::new() }),
                state: Mutex::new(state),
            }),
        }
    }

    /// 在设备上建一个空单元，覆盖原有内容。
    pub fn format(dev: D, unit: UnitId) -> Result<Self, OpenError> {
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
        let sb = Super { block_size: bs, blocks, unit, generation: 0, root: 0, free_head: 0, next_object: 1 };
        let me = Self::new(dev, sb, free, Vec::new());
        {
            let mut st = me.lock();
            st.dirty = true;
            me.commit(&mut st)?;
        }
        Ok(me)
    }

    /// 挂载已有的单元。断电前没用上的暂存在这里回收。
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
        let me = Self::new(dev, sb, free, free_list);
        me.drop_unclaimed_stages()?;
        Ok(me)
    }

    /// 没有准备记录引用的暂存：上次没用上就断了，回收。
    fn drop_unclaimed_stages(&self) -> Result<(), UnitError> {
        let claimed: BTreeSet<StageId> = self
            .prepared()?
            .iter()
            .flat_map(|(_, changes)| changes.iter())
            .filter_map(|c| match c {
                Change::Create { content: Some(s), .. } => Some(*s),
                _ => None,
            })
            .collect();
        let stale: Vec<StageId> = self.stages()?.into_iter().filter(|s| !claimed.contains(s)).collect();
        if !stale.is_empty() {
            self.transact(|w| stale.iter().try_for_each(|s| w.discard_stage(*s)))?;
        }
        Ok(())
    }

    /// 底层设备（测试里用来模拟断电）。
    pub fn device(&self) -> &D {
        &self.s.dev
    }

    /// 最近一次提交的提交号。
    pub fn generation(&self) -> u64 {
        self.lock().sb.generation
    }

    /// 空闲块数（含等待旧版本的读者结束、等待下一次提交的）。
    pub fn free_blocks(&self) -> u64 {
        let st = self.lock();
        st.free.blocks() + st.pending.iter().map(|r| r.blocks.blocks()).sum::<u64>()
    }

    /// 已经释放、暂时还不能再分配的块数。
    pub fn pinned_blocks(&self) -> u64 {
        self.lock().pending.iter().map(|r| r.blocks.blocks()).sum()
    }

    /// 全部块数（含两份超级块）。
    pub fn total_blocks(&self) -> u64 {
        self.lock().sb.blocks
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.s.state.lock()
    }

    fn bs(&self) -> u64 {
        self.s.bs as u64
    }

    fn work<'a>(&'a self, st: &'a mut State) -> Work<'a, D> {
        Work { dev: &self.s.dev, bs: self.bs(), s: st, durable: ExtentSet::new(), young: ExtentSet::new() }
    }

    /// 在当前版本上读，读的期间登记为读者。
    fn view<T>(&self, f: impl FnOnce(&View<'_, D>) -> Result<T, UnitError>) -> Result<T, UnitError> {
        let (tree, _pin) = self.pin_current();
        f(&View { tree: &tree, dev: &self.s.dev, bs: self.bs() })
    }

    fn pin_current(&self) -> (Tree, Pin<D>) {
        let mut cur = self.s.current.lock();
        let epoch = cur.epoch;
        *cur.readers.entry(epoch).or_default() += 1;
        (cur.tree.view(), Pin { s: self.s.clone(), epoch })
    }

    /// 把改好的状态公布成新版本。
    fn publish(&self, st: &mut State) {
        st.epoch += 1;
        {
            let mut cur = self.s.current.lock();
            cur.tree = st.tree.view();
            cur.epoch = st.epoch;
        }
        self.reclaim(st);
    }

    /// 不再有人引用的已释放块，放回可分配的空间。返回是否放回了什么。
    fn reclaim(&self, st: &mut State) -> bool {
        let oldest = self.s.current.lock().readers.keys().next().copied();
        let published = st.epoch;
        let (ready, waiting): (Vec<_>, Vec<_>) = core::mem::take(&mut st.pending)
            .into_iter()
            .partition(|r| !r.after_commit && r.epoch <= published && oldest.is_none_or(|o| o >= r.epoch));
        st.pending = waiting;
        let any = !ready.is_empty();
        for r in ready {
            st.free.extend(&r.blocks);
        }
        any
    }

    /// 在写者状态上做一次改动，`commit` 时随即提交，然后公布。失败时状态不变；空间不够时，
    /// 先收回能收回的块，再试着扩容，然后整个重做。
    fn run<T>(
        &self,
        st: &mut State,
        commit: bool,
        f: &mut impl FnMut(&mut Work<'_, D>) -> Result<T, UnitError>,
    ) -> Result<T, UnitError> {
        loop {
            if st.broken {
                return Err(UnitError::Device);
            }
            let saved = st.clone();
            let mut w = self.work(st);
            let r = match f(&mut w) {
                Ok(t) => {
                    settle(w);
                    st.dirty = true;
                    if commit { self.commit(st).map(|()| t) } else { Ok(t) }
                }
                Err(e) => Err(e),
            };
            match r {
                Ok(t) => {
                    self.publish(st);
                    return Ok(t);
                }
                Err(e) => {
                    let broken = st.broken;
                    *st = saved;
                    st.broken |= broken;
                    if e == UnitError::NoSpace && (self.reclaim(st) || self.grow(st)) {
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    /// 一次不提交的改动（内容写入）。
    fn update<T>(&self, mut f: impl FnMut(&mut Work<'_, D>) -> Result<T, UnitError>) -> Result<T, UnitError> {
        let mut st = self.lock();
        self.run(&mut st, false, &mut f)
    }

    /// 一次随即提交的改动（元数据修改）。
    fn transact<T>(&self, mut f: impl FnMut(&mut Work<'_, D>) -> Result<T, UnitError>) -> Result<T, UnitError> {
        let mut st = self.lock();
        self.run(&mut st, true, &mut f)
    }

    /// 把写者状态写成新的提交。失败时调用者负责恢复状态。
    fn commit(&self, st: &mut State) -> Result<(), UnitError> {
        if st.broken {
            return Err(UnitError::Device);
        }
        let dev = &self.s.dev;
        let bs = self.s.bs;
        let generation = st.sb.generation + 1;
        let root = {
            let (free, fresh) = (&mut st.free, &mut st.fresh);
            st.tree.flush(dev, &mut || alloc_one(free, fresh), generation)?
        };
        // 旧的空闲表在这次提交之后就没人引用了。
        let mut retired = ExtentSet::new();
        for &b in &st.free_list {
            retired.insert(b, 1);
        }

        // 空闲表本身也要占块：先分配，再把分配后的空闲集合写进去。正在锁外写入的块在盘上也算空闲：
        // 它们还没被登记，断电之后没人引用。
        let per = free_list_per_block(bs);
        let mut list_blocks = Vec::new();
        let persisted = loop {
            let mut all = st.free.clone();
            all.extend(&retired);
            all.extend(&st.inflight);
            for r in &st.pending {
                all.extend(&r.blocks);
            }
            if list_blocks.len() >= all.count().div_ceil(per) {
                break all;
            }
            list_blocks.push(alloc_one(&mut st.free, &mut st.fresh)?);
        };
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
            buf.resize(bs, 0);
            let crc = crc32c(&buf[4..]);
            buf[..4].copy_from_slice(&crc.to_le_bytes());
            device_err(dev.write_at(b * bs as u64, &buf))?;
        }
        device_err(dev.flush())?;

        let sb = Super {
            generation,
            root: root.unwrap_or(0),
            free_head: list_blocks.first().copied().unwrap_or(0),
            next_object: st.next_object,
            ..st.sb.clone()
        };
        let slot = generation % 2;
        if dev.write_at(slot * bs as u64, &sb.encode(bs)).and_then(|_| dev.flush()).is_err() {
            st.broken = true;
            return Err(UnitError::Device);
        }

        st.sb = sb;
        st.free_list = list_blocks;
        for r in &mut st.pending {
            r.after_commit = false;
        }
        if !retired.is_empty() {
            st.pending.push(Released { epoch: 0, blocks: retired, after_commit: false });
        }
        st.fresh = st.inflight.clone();
        st.dirty = false;
        Ok(())
    }

    /// 设备扩容（至少翻倍），新增的块进空闲空间。下一次提交把新的大小写进超级块；在那之前断电，
    /// 超级块还是旧的大小，多出来的部分没人用。返回是否扩成了。
    fn grow(&self, st: &mut State) -> bool {
        let bs = self.bs();
        let old = st.sb.blocks;
        let want = old.saturating_mul(2).max(old + GROW_MIN_BLOCKS);
        if self.s.dev.grow(want.saturating_mul(bs)).is_err() {
            return false;
        }
        let now = self.s.dev.capacity() / bs;
        if now <= old {
            return false;
        }
        st.free.insert(old, now - old);
        st.sb.blocks = now;
        true
    }

    /// 写对象的内容：小的直接在锁里改；大的先在锁里分配新块，锁外写数据，再回锁里登记。
    fn write_object(&self, o: u64, offset: u64, data: &[u8]) -> Result<(), UnitError> {
        let Some(plan) = self.update(|w| w.plan_write(o, offset, data))? else { return Ok(()) };
        let written = self.fill_blocks(&plan, offset, data);
        let mut st = self.lock();
        let r = written.and_then(|()| self.run(&mut st, false, &mut |w| w.finish_write(&plan)));
        if r.is_err() {
            unreserve(&mut st, &plan.blocks);
        }
        r
    }

    /// 锁外：把数据写进安排好的新块。只写了一部分的首尾两块，先读出原来的内容再补上。
    fn fill_blocks(&self, plan: &WritePlan, offset: u64, data: &[u8]) -> Result<(), UnitError> {
        let bs = self.bs();
        let dev = &self.s.dev;
        let end = offset + data.len() as u64;
        let (b0, b1) = (plan.b0, plan.b1);
        let read_old = |phys: Option<u64>| -> Result<Vec<u8>, UnitError> {
            let mut block = vec![0u8; bs as usize];
            if let Some(p) = phys {
                device_err(dev.read_at(p * bs, &mut block))?;
            }
            Ok(block)
        };
        // 落在第 `b` 块里的那部分数据，覆盖到这块原来的内容上。
        let overlay = |b: u64, block: &mut [u8]| {
            let (lo, hi) = (offset.max(b * bs), end.min((b + 1) * bs));
            block[(lo - b * bs) as usize..(hi - b * bs) as usize]
                .copy_from_slice(&data[(lo - offset) as usize..(hi - offset) as usize]);
        };
        let mut head = None;
        if !offset.is_multiple_of(bs) {
            let mut block = read_old(plan.head)?;
            overlay(b0, &mut block);
            head = Some(block);
        }
        let mut tail = None;
        if !end.is_multiple_of(bs) && !(b1 - 1 == b0 && head.is_some()) {
            let mut block = read_old(plan.tail)?;
            overlay(b1 - 1, &mut block);
            tail = Some(block);
        }
        let mut logical = b0;
        for &(start, len) in &plan.blocks {
            let mut j = logical;
            while j < logical + len {
                let phys = (start + (j - logical)) * bs;
                if j == b0
                    && let Some(block) = &head
                {
                    device_err(dev.write_at(phys, block))?;
                    j += 1;
                    continue;
                }
                if j == b1 - 1
                    && let Some(block) = &tail
                {
                    device_err(dev.write_at(phys, block))?;
                    j += 1;
                    continue;
                }
                // 中间整块的数据在 `data` 里是连续的一段，直接写。
                let stop = (logical + len).min(if tail.is_some() { b1 - 1 } else { b1 });
                let from = (j * bs - offset) as usize;
                let to = (stop * bs - offset) as usize;
                device_err(dev.write_at(phys, &data[from..to]))?;
                j = stop;
            }
            logical += len;
        }
        Ok(())
    }

    /// 实体要改内容，返回它独占的对象。对象被共享着就复制一份给它：大的在锁外复制数据。
    fn own(&self, id: &EntityId) -> Result<u64, UnitError> {
        type Copying = (u64, u64, Vec<Extent>, Vec<(u64, u64)>);
        let step: Result<u64, Copying> = self.update(|w| {
            Ok(match w.own_small(id)? {
                Ok(o) => Ok(o),
                Err(o) => {
                    let size = w.object(o)?.size;
                    let extents = w.extents(o)?;
                    let blocks = w.alloc(extents.iter().map(|e| e.2).sum())?;
                    for &(s, l) in &blocks {
                        w.s.inflight.insert(s, l);
                    }
                    Err((o, size, extents, blocks))
                }
            })
        })?;
        let (from, size, extents, blocks) = match step {
            Ok(o) => return Ok(o),
            Err(c) => c,
        };
        let copied = self.copy_extents(&extents, &blocks);
        let mut st = self.lock();
        let r = copied.and_then(|layout| {
            self.run(&mut st, false, &mut |w| {
                if w.entity_object(id)? != from {
                    // 调用者没有排好同一实体上的写入。
                    return Err(UnitError::Corrupt);
                }
                w.switch_to_copy(id, from, size, &layout)
            })
        });
        if r.is_err() {
            unreserve(&mut st, &blocks);
        }
        r
    }

    /// 锁外：把区段的数据依次搬进新分配的块，返回新的区段（逻辑块、物理块、块数）。
    fn copy_extents(&self, extents: &[Extent], blocks: &[(u64, u64)]) -> Result<Vec<Extent>, UnitError> {
        let bs = self.bs();
        let dev = &self.s.dev;
        let mut layout = Vec::new();
        let mut dst = blocks.iter().copied();
        let (mut at, mut room) = (0u64, 0u64);
        for &(logical, phys, n) in extents {
            let mut done = 0;
            while done < n {
                if room == 0 {
                    (at, room) = dst.next().ok_or(UnitError::Corrupt)?;
                }
                let chunk = (n - done).min(room).min(COPY_CHUNK_BLOCKS);
                let mut buf = vec![0u8; (chunk * bs) as usize];
                device_err(dev.read_at((phys + done) * bs, &mut buf))?;
                device_err(dev.write_at(at * bs, &buf))?;
                layout.push((logical + done, at, chunk));
                (at, room, done) = (at + chunk, room - chunk, done + chunk);
            }
        }
        Ok(layout)
    }

    /// 把一批修改、附加记录和准备记录的了结作为一次提交原子地完成。
    /// 建在块格式单元之上的单元（如目录单元）靠它把自己的记录和实体的修改放进同一次提交。
    pub fn commit_batch(&self, batch: &Batch<'_>) -> Result<(), UnitError> {
        self.transact(|w| {
            for (id, stage) in batch.adopt {
                w.adopt(id, *stage)?;
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

    /// 现有的全部暂存。
    pub fn stages(&self) -> Result<Vec<StageId>, UnitError> {
        self.view(|v| v.stages())
    }

    /// 读一条附加记录。
    pub fn aux_get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, UnitError> {
        self.view(|v| v.tree.get(v.dev, &keys::aux(key)))
    }

    /// 键以 `prefix` 开头的全部附加记录，按键排序。
    pub fn aux_list(&self, prefix: &[u8]) -> Result<Vec<AuxRecord>, UnitError> {
        self.view(|v| {
            let mut out = Vec::new();
            v.tree.scan_prefix(v.dev, &keys::aux(prefix), &mut |k, val| {
                out.push((k[1..].to_vec(), val.to_vec()));
                Ok(true)
            })?;
            Ok(out)
        })
    }

    /// 全面检查：每个块恰好属于超级块、树节点、对象内容、空闲表、空闲空间、待回收、正在写入中的一种；
    /// 对象引用数和引用它的实体数一致（暂存的对象没有实体引用）；实体的 Tag 和 Tag 索引一致。
    pub fn verify(&self) -> Result<(), UnitError> {
        // 在写者状态的一个瞬间上查：拿着那一刻的树和空间记账，登记为读者，之后锁外慢慢查。
        let (tree, free, pending, inflight, free_list, blocks, next_object, _pin) = {
            let st = self.lock();
            let pin = {
                let mut cur = self.s.current.lock();
                *cur.readers.entry(st.epoch).or_default() += 1;
                Pin { s: self.s.clone(), epoch: st.epoch }
            };
            let pending: Vec<ExtentSet> = st.pending.iter().map(|r| r.blocks.clone()).collect();
            (
                st.tree.view(),
                st.free.clone(),
                pending,
                st.inflight.clone(),
                st.free_list.clone(),
                st.sb.blocks,
                st.next_object,
                pin,
            )
        };
        let mut used = ExtentSet::new();
        let claim = |s: u64, l: u64, used: &mut ExtentSet| -> Result<(), UnitError> {
            if l == 0 || s + l > blocks || used.overlaps(s, l) {
                return Err(UnitError::Corrupt);
            }
            used.insert(s, l);
            Ok(())
        };
        claim(0, 2, &mut used)?;
        for &b in &free_list {
            claim(b, 1, &mut used)?;
        }
        for set in pending.iter().chain([&free, &inflight]) {
            for (s, l) in set.iter() {
                claim(s, l, &mut used)?;
            }
        }
        let dev = &self.s.dev;
        let mut nodes = Vec::new();
        tree.visit_nodes(dev, &mut |b| nodes.push(b))?;
        for b in nodes {
            claim(b, 1, &mut used)?;
        }

        let mut refs: BTreeMap<u64, u64> = BTreeMap::new();
        let mut staged = BTreeSet::new();
        let mut entity_tags = Vec::new();
        let mut index = Vec::new();
        let mut objects = Vec::new();
        let mut extents = Vec::new();
        tree.scan(dev, &[], &mut |k, v| {
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
                keys::OBJECT => objects.push(Reader::new(&k[1..]).be_u64()?),
                keys::EXTENT => {
                    let o = Reader::new(&k[1..]).be_u64()?;
                    extents.push(view::decode_extent(o, k, v)?);
                }
                keys::STAGED => {
                    staged.insert(Reader::new(&k[1..]).be_u64()?);
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
        let v = View { tree: &tree, dev, bs: self.bs() };
        if objects.len() != refs.len() + staged.len() {
            return Err(UnitError::Corrupt);
        }
        for o in &objects {
            let rec = v.object(*o)?;
            let expect = if staged.contains(o) { None } else { refs.get(o) };
            if rec.refs != expect.copied().unwrap_or(0) || *o >= next_object {
                return Err(UnitError::Corrupt);
            }
        }
        for (_, p, n) in extents {
            claim(p, n, &mut used)?;
        }
        if used.blocks() != blocks {
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
        self.s.unit
    }

    fn snapshot(&self) -> Box<dyn Snapshot> {
        let (tree, pin) = self.pin_current();
        Box::new(BlockSnapshot { tree, pin })
    }

    fn write(&self, id: &EntityId, offset: u64, data: &[u8]) -> Result<(), UnitError> {
        let o = self.own(id)?;
        self.write_object(o, offset, data)
    }

    fn set_len(&self, id: &EntityId, len: u64) -> Result<(), UnitError> {
        let o = self.own(id)?;
        self.update(|w| w.truncate_object(o, len))
    }

    fn sync(&self) -> Result<(), UnitError> {
        let mut st = self.lock();
        if !st.dirty {
            return Ok(());
        }
        self.run(&mut st, true, &mut |_| Ok(()))
    }

    fn stage(&self) -> Result<StageId, UnitError> {
        self.update(|w| w.new_stage())
    }

    fn write_staged(&self, stage: StageId, offset: u64, data: &[u8]) -> Result<(), UnitError> {
        let o = self.view(|v| v.staged(stage))?;
        self.write_object(o, offset, data)
    }

    fn discard_staged(&self, stage: StageId) -> Result<(), UnitError> {
        self.update(|w| w.discard_stage(stage))
    }

    fn apply(&self, changes: &[Change]) -> Result<(), UnitError> {
        self.transact(|w| changes.iter().try_for_each(|c| w.apply(c)))
    }

    fn prepare(&self, txn: &TxnInfo, changes: &[Change]) -> Result<(), UnitError> {
        self.transact(|w| w.put_prepared(txn, changes))
    }

    fn prepared(&self) -> Result<Vec<(TxnInfo, Vec<Change>)>, UnitError> {
        self.view(|v| v.prepared())
    }

    fn commit_prepared(&self, txn: TxnId, as_primary: bool) -> Result<(), UnitError> {
        self.transact(|w| {
            let (info, changes) = w.prepared_one(&txn)?.ok_or(UnitError::UnknownTxn(txn))?;
            for c in &changes {
                w.apply(c)?;
            }
            w.remove_prepared(&txn)?;
            if as_primary {
                w.put_committed(&info)?;
            }
            Ok(())
        })
    }

    fn abort_prepared(&self, txn: TxnId) -> Result<(), UnitError> {
        self.transact(|w| {
            let (_, changes) = w.prepared_one(&txn)?.ok_or(UnitError::UnknownTxn(txn))?;
            w.remove_prepared(&txn)?;
            // 它占着的暂存一并丢掉。
            for c in &changes {
                if let Change::Create { content: Some(stage), .. } = c {
                    w.discard_stage(*stage)?;
                }
            }
            Ok(())
        })
    }

    fn committed(&self) -> Result<Vec<TxnInfo>, UnitError> {
        self.view(|v| v.committed())
    }

    fn forget(&self, txn: TxnId) -> Result<(), UnitError> {
        self.transact(|w| w.remove_committed(&txn))
    }

    fn check(&self) -> Result<(), UnitError> {
        self.verify()
    }

    fn describe(&self) -> Vec<(&'static str, alloc::string::String)> {
        use alloc::format;
        let bs = self.bs();
        vec![
            ("size", format!("{} bytes", self.total_blocks() * bs)),
            ("free", format!("{} bytes", self.free_blocks() * bs)),
            ("generation", format!("{}", self.generation())),
        ]
    }
}

struct BlockSnapshot<D: BlockDevice> {
    tree: Tree,
    pin: Pin<D>,
}

impl<D: BlockDevice> BlockSnapshot<D> {
    fn with_view<T>(&self, f: impl FnOnce(&View<'_, D>) -> Result<T, UnitError>) -> Result<T, UnitError> {
        f(&View { tree: &self.tree, dev: &self.pin.s.dev, bs: self.pin.s.bs as u64 })
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
