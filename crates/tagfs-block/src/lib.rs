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
//!
//! ## 多个进程
//!
//! 设备带着 [`Share`] 时（宿主上的镜像文件），多个进程可以同时打开同一个单元，没有服务进程：
//!
//! - **改**要先拿写者锁。拿到时盘上的提交号和自己记得的不一样，说明别的进程提交过，整个写者状态
//!   （树根、空闲空间）从盘上重新读。写者锁一直持有到改动都提交了：元数据修改提交完就放；内容写入
//!   攒着，由 [`Ticker`] 在空闲时（或攒了太久时）提交再放。
//! - **读**要共享持有读者锁，读之前看一眼超级块，别的进程提交过就换成新的树根。读不等写。
//! - **释放的块**除了上面两件事，还要等别的进程不再读它：写者能独占读者锁的那一刻，别的进程都没在读，
//!   之后它们读到的只会是新的提交。盘上因此有两张空闲表：一张是谁都能用的，一张是“刚释放、
//!   别的进程可能还在读”的，后者要等这样的一刻之后才能用。
//! - 属于某个进程的东西（暂存、它正在进行的跨单元事务）记着它的身份。别的进程只替已经不在的
//!   进程收拾它们。

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
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};

use tagfs_core::{
    Change, EntityId, EntityRecord, Probe, Snapshot, StageId, Tag, TxnId, TxnInfo, Unit, UnitError, UnitId,
};
use tagfs_device::{BlockDevice, Share};
use tagfs_sync::{Mutex, MutexGuard};

use crate::codec::{Reader, put_u32, put_u64};
use crate::crc::crc32c;
use crate::extent::ExtentSet;
use crate::tree::Tree;
use crate::view::{Extent, View};
use crate::work::{Work, WritePlan};

const MAGIC: &[u8; 8] = b"TAGFSBLK";
/// 3 比 2 多了第二张空闲表（[`Super::recent_head`]）。2 仍然能读。
const VERSION: u32 = 3;
/// 支持的最小块大小。树的单个条目上限由它决定。
pub const MIN_BLOCK_SIZE: u32 = 4096;
const MIN_BLOCKS: u64 = 16;
/// 扩容时至少加这么多块（16 MiB）。
const GROW_MIN_BLOCKS: u64 = 4096;
/// 在锁外复制对象内容时一次搬多少块。
const COPY_CHUNK_BLOCKS: u64 = 64;
/// 读超级块时两份都没通过校验（正赶上别的进程连着提交两次）时，重读几次。
const SUPER_RETRIES: usize = 64;
/// 内容写入一直不停时，最多攒几次 [`BlockUnit::tick`] 就提交一次。
const MAX_DIRTY_TICKS: u32 = 40;

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
    /// 第二张空闲表的第一块：刚释放、别的进程可能还在读的块。只有一个进程用的设备上，打开时
    /// 它们直接算空闲。
    recent_head: u64,
}

/// 版本 2 的超级块正文长度（到 CRC 之前）；版本 3 多一个 `recent_head`。
const SUPER_LEN_V2: usize = 8 + 4 + 4 + 8 + 16 + 8 * 4;
const SUPER_LEN_V3: usize = SUPER_LEN_V2 + 8;

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
        put_u64(&mut b, self.recent_head);
        let crc = crc32c(&b);
        put_u32(&mut b, crc);
        b.resize(bs, 0);
        b
    }

    fn decode(b: &[u8]) -> Result<Super, OpenError> {
        if b[..8] != MAGIC[..] {
            return Err(OpenError::NotFormatted);
        }
        let crc_ok = |len: usize| u32::from_le_bytes(b[len..len + 4].try_into().unwrap()) == crc32c(&b[..len]);
        let len = [SUPER_LEN_V3, SUPER_LEN_V2].into_iter().find(|&l| crc_ok(l)).ok_or(OpenError::NotFormatted)?;
        let mut r = Reader::new(&b[8..len]);
        let version = r.u32()?;
        match (version, len) {
            (VERSION, SUPER_LEN_V3) | (2, SUPER_LEN_V2) => {}
            _ => return Err(OpenError::UnsupportedVersion(version)),
        }
        Ok(Super {
            block_size: r.u32()?,
            blocks: r.u64()?,
            unit: UnitId(r.id16()?),
            generation: r.u64()?,
            root: r.u64()?,
            free_head: r.u64()?,
            next_object: r.u64()?,
            recent_head: if r.is_empty() { 0 } else { r.u64()? },
        })
    }
}

/// 两份超级块里校验通过、提交号最大的那份。
fn read_super<D: BlockDevice + ?Sized>(dev: &D, bs: usize) -> Result<Super, OpenError> {
    let mut best: Option<Super> = None;
    let mut first_err = OpenError::NotFormatted;
    for slot in 0..2 {
        let buf = tree::read_block(dev, slot, bs)?;
        match Super::decode(&buf) {
            Ok(sb) if best.as_ref().is_none_or(|b| sb.generation > b.generation) => best = Some(sb),
            Ok(_) => {}
            Err(e @ OpenError::UnsupportedVersion(_)) => first_err = e,
            Err(_) => {}
        }
    }
    best.ok_or(first_err)
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
    /// 别的进程可能还在读它们（它们出现在某次提交里）：要等别的进程都不在读的一刻之后才算空闲。
    drain: bool,
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
    /// 当前空闲表（两张）占用的块。
    free_list: Vec<u64>,
    /// 当前公布的版本号。
    epoch: u64,
    /// 有没提交的改动。
    dirty: bool,
    /// 写超级块出错后不知道盘上是哪个状态，不再接受写入。
    broken: bool,
    /// 本进程的身份，记进属于它的记录；只有一个进程用的设备上是 0。
    pub(crate) owner: u64,
    /// 持有写者锁。只有一个进程用的设备上一直为真。
    writer: bool,
    /// 读者换过树根，空闲空间的记账已经过时：拿到写者锁时要从盘上重读。
    space_stale: bool,
    /// [`Hold`] 的个数：有它时不放写者锁。
    holds: usize,
    /// 上一次 [`BlockUnit::tick`] 之后有过内容写入。
    active: bool,
    /// 没提交的内容写入已经攒了几次 tick。
    dirty_ticks: u32,
    /// 上一次没能等到别的进程都不在读，是在哪个提交号上：同一个提交号上不再反复试。
    drain_tried: Option<u64>,
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
    /// 本进程里一次只有一个线程去等写者锁。
    acquiring: Mutex<()>,
    /// 和 `State::writer` 相同，读者不拿状态锁就能看。
    is_writer: AtomicBool,
}

pub struct BlockUnit<D: BlockDevice> {
    s: Arc<Shared<D>>,
}

/// 登记一个读者持有某个版本；放掉时注销。
struct Pin<D: BlockDevice> {
    s: Arc<Shared<D>>,
    epoch: u64,
    /// 持有着读者锁，放掉时一起放。
    held: bool,
}

impl<D: BlockDevice> Drop for Pin<D> {
    fn drop(&mut self) {
        {
            let mut cur = self.s.current.lock();
            if let Some(n) = cur.readers.get_mut(&self.epoch) {
                *n -= 1;
                if *n == 0 {
                    cur.readers.remove(&self.epoch);
                }
            }
        }
        if self.held
            && let Some(share) = self.s.dev.share()
        {
            share.release_readers();
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
/// 最近一次提交还引用的块出现在盘上，别的进程可能在读；之后才分配的没有。
fn settle<D: BlockDevice>(mut w: Work<'_, D>) {
    for b in core::mem::take(&mut w.s.tree.replaced) {
        w.release(b, 1);
    }
    let epoch = w.s.epoch + 1;
    for (blocks, durable) in [(w.durable, true), (w.young, false)] {
        if !blocks.is_empty() {
            w.s.pending.push(Released { epoch, blocks, after_commit: durable, drain: durable });
        }
    }
}

/// 让一个线程一直持有写者锁的凭据，见 [`BlockUnit::hold`]。
pub struct Hold<'a, D: BlockDevice + 'static> {
    unit: &'a BlockUnit<D>,
}

impl<D: BlockDevice + 'static> Drop for Hold<'_, D> {
    fn drop(&mut self) {
        let mut st = self.unit.lock();
        st.holds -= 1;
        self.unit.release(&mut st);
    }
}

/// 给后台线程用的单元句柄：不让单元活着，单元关掉之后 [`Ticker::tick`] 返回 `false`。
pub struct Ticker<D: BlockDevice> {
    s: Weak<Shared<D>>,
}

impl<D: BlockDevice + 'static> Ticker<D> {
    /// 见 [`BlockUnit::tick`]。单元已经关掉时返回 `false`。
    pub fn tick(&self) -> bool {
        match self.s.upgrade() {
            Some(s) => {
                let _ = BlockUnit { s }.tick();
                true
            }
            None => false,
        }
    }
}

impl<D: BlockDevice + 'static> BlockUnit<D> {
    fn new(dev: D, sb: Super, free: ExtentSet, free_list: Vec<u64>, pending: Vec<Released>) -> Self {
        let bs = sb.block_size as usize;
        let tree = Tree::open((sb.root != 0).then_some(sb.root), bs);
        let owner = dev.share().map_or(0, |s| s.owner());
        let writer = dev.share().is_none();
        let state = State {
            tree: tree.view(),
            next_object: sb.next_object,
            free,
            fresh: ExtentSet::new(),
            inflight: ExtentSet::new(),
            pending,
            free_list,
            epoch: 0,
            dirty: false,
            broken: false,
            owner,
            writer,
            space_stale: false,
            holds: 0,
            active: false,
            dirty_ticks: 0,
            drain_tried: None,
            sb: sb.clone(),
        };
        BlockUnit {
            s: Arc::new(Shared {
                dev,
                bs,
                unit: sb.unit,
                current: Mutex::new(Current { tree, epoch: 0, readers: BTreeMap::new() }),
                state: Mutex::new(state),
                acquiring: Mutex::new(()),
                is_writer: AtomicBool::new(writer),
            }),
        }
    }

    /// 在设备上建一个空单元，覆盖原有内容。设备和别的进程共用时，调用者要保证此时没有别的进程在用它。
    pub fn format(dev: D, unit: UnitId) -> Result<Self, OpenError> {
        let bs = dev.block_size();
        if bs < MIN_BLOCK_SIZE || !bs.is_power_of_two() {
            return Err(OpenError::BadBlockSize);
        }
        let blocks = dev.capacity() / bs as u64;
        if blocks < MIN_BLOCKS {
            return Err(OpenError::TooSmall);
        }
        if let Some(share) = dev.share() {
            device_err(share.lock_writer())?;
        }
        let zero = vec![0u8; bs as usize];
        for slot in 0..2 {
            device_err(dev.write_at(slot * bs as u64, &zero))?;
        }
        let mut free = ExtentSet::new();
        free.insert(2, blocks - 2);
        let sb = Super {
            block_size: bs,
            blocks,
            unit,
            generation: 0,
            root: 0,
            free_head: 0,
            next_object: 1,
            recent_head: 0,
        };
        let me = Self::new(dev, sb, free, Vec::new(), Vec::new());
        {
            let mut st = me.lock();
            me.became_writer(&mut st);
            st.dirty = true;
            me.commit(&mut st)?;
            me.release(&mut st);
        }
        Ok(me)
    }

    /// 挂载已有的单元。已经不在的进程（只有一个进程用的设备上：断电前）没用上的暂存在这里回收。
    pub fn open(dev: D) -> Result<Self, OpenError> {
        let bs = dev.block_size();
        if bs < MIN_BLOCK_SIZE || !bs.is_power_of_two() {
            return Err(OpenError::BadBlockSize);
        }
        // 和别的进程共用时，在写者锁里读：这样读到的空闲空间不会正被别人改。
        if let Some(share) = dev.share() {
            device_err(share.lock_writer())?;
        }
        let loaded = (|| {
            device_err(dev.refresh())?;
            let sb = read_super(&dev, bs as usize)?;
            if sb.block_size != bs {
                return Err(OpenError::BadBlockSize);
            }
            if sb.blocks > dev.capacity() / bs as u64 {
                return Err(OpenError::TooSmall);
            }
            let space = read_space(&dev, bs as usize, &sb)?;
            Ok((sb, space))
        })();
        let (sb, (mut free, recent, free_list)) = match loaded {
            Ok(x) => x,
            Err(e) => {
                if let Some(share) = dev.share() {
                    share.unlock_writer();
                }
                return Err(e);
            }
        };
        let mut pending = Vec::new();
        if dev.share().is_some() {
            if !recent.is_empty() {
                pending.push(Released { epoch: 0, blocks: recent, after_commit: false, drain: true });
            }
        } else {
            free.extend(&recent);
        }
        let me = Self::new(dev, sb, free, free_list, pending);
        me.became_writer(&mut me.lock());
        me.drop_unclaimed_stages()?;
        Ok(me)
    }

    /// 没有准备记录引用、属主也已经不在的暂存：没用上就断了，回收。
    fn drop_unclaimed_stages(&self) -> Result<(), UnitError> {
        let (stages, prepared) = self.view(|v| Ok((v.stages()?, v.prepared()?)))?;
        let claimed: BTreeSet<StageId> = prepared
            .iter()
            .flat_map(|(_, changes, _)| changes.iter())
            .filter_map(|c| match c {
                Change::Create { content: Some(s), .. } => Some(*s),
                _ => None,
            })
            .collect();
        let mut stale = Vec::new();
        for (s, owner) in stages {
            if !claimed.contains(&s) && self.abandoned(owner) {
                stale.push(s);
            }
        }
        if stale.is_empty() {
            // 打开时拿着的写者锁在这里放掉。
            self.release(&mut self.lock());
            return Ok(());
        }
        self.transact(|w| stale.iter().try_for_each(|s| w.discard_stage(*s)))
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

    /// 给后台线程用的句柄。和别的进程共用设备时，要有人定期调 [`Ticker::tick`]：
    /// 没提交的内容写入会一直占着写者锁。
    pub fn ticker(&self) -> Ticker<D> {
        Ticker { s: Arc::downgrade(&self.s) }
    }

    /// 后台定期调用（例如每 25 毫秒）：上一次之后没有新的内容写入（或者已经攒了太久），就把攒着的
    /// 内容写入提交掉，并放掉写者锁，让别的进程能改。只有一个进程用的设备上什么都不做。
    pub fn tick(&self) -> Result<(), UnitError> {
        if self.s.dev.share().is_none() {
            return Ok(());
        }
        let mut st = self.lock();
        if !st.writer {
            return Ok(());
        }
        if st.dirty {
            if core::mem::take(&mut st.active) && st.dirty_ticks < MAX_DIRTY_TICKS {
                st.dirty_ticks += 1;
                return Ok(());
            }
            self.run(&mut st, true, &mut |_| Ok(()))?;
        }
        self.release(&mut st);
        Ok(())
    }

    /// 拿住写者锁直到返回的凭据放掉。期间别的进程改不了这个单元，本进程也不会放掉写者锁。
    /// 建在块格式单元之上的单元要让自己的一组操作在进程之间不被打断时用它。
    pub fn hold(&self) -> Result<Hold<'_, D>, UnitError> {
        let mut st = self.writer()?;
        st.holds += 1;
        Ok(Hold { unit: self })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.s.state.lock()
    }

    fn bs(&self) -> u64 {
        self.s.bs as u64
    }

    fn share(&self) -> Option<&dyn Share> {
        self.s.dev.share()
    }

    /// 属主已经不在了：它留下的东西没人管，可以替它收拾。只有一个进程用的设备上，单元里的记录都是
    /// 以前（断电前）留下的。
    fn abandoned(&self, owner: u64) -> bool {
        match self.share() {
            None => true,
            Some(share) => owner != share.owner() && !share.alive(owner).unwrap_or(true),
        }
    }

    /// 这条记录归本进程处理：是本进程的，或者属主已经不在了。
    fn ours(&self, owner: u64) -> bool {
        match self.share() {
            None => true,
            Some(share) => owner == share.owner() || self.abandoned(owner),
        }
    }

    fn work<'a>(&'a self, st: &'a mut State) -> Work<'a, D> {
        Work { dev: &self.s.dev, bs: self.bs(), s: st, durable: ExtentSet::new(), young: ExtentSet::new() }
    }

    /// 拿到写者锁之后（或打开、格式化时已经拿着）记下来。
    fn became_writer(&self, st: &mut State) {
        st.writer = true;
        self.s.is_writer.store(true, Ordering::Release);
    }

    /// 写者状态，拿着写者锁。和别的进程共用设备时，没拿着就先去拿（等别的进程放掉），
    /// 别的进程提交过就从盘上重读写者状态。
    fn writer(&self) -> Result<MutexGuard<'_, State>, UnitError> {
        let Some(share) = self.share() else { return Ok(self.lock()) };
        {
            let st = self.lock();
            if st.writer {
                return Ok(st);
            }
        }
        let _one = self.s.acquiring.lock();
        {
            let st = self.lock();
            if st.writer {
                return Ok(st);
            }
        }
        // 等锁时不拿着状态锁：本进程的读者照常读。
        device_err(share.lock_writer())?;
        let mut st = self.lock();
        if let Err(e) = self.refresh(&mut st, true) {
            share.unlock_writer();
            return Err(e);
        }
        self.became_writer(&mut st);
        Ok(st)
    }

    /// 能放就放掉写者锁：改动都提交了、没有正在锁外写的块、没人要求拿住。
    fn release(&self, st: &mut State) {
        let Some(share) = self.share() else { return };
        if st.writer && !st.dirty && st.inflight.is_empty() && st.holds == 0 {
            st.writer = false;
            // 先让读者知道自己不再是写者，再放锁：之后开始的读者都会去看盘上的超级块。
            self.s.is_writer.store(false, Ordering::Release);
            share.unlock_writer();
        }
    }

    /// 别的进程提交过就跟上：换成盘上最新的树根并公布。`full` 时（拿到写者锁时）空闲空间的记账也从盘上重读，
    /// 否则只记下它过时了。调用者持有读者锁或写者锁。
    fn refresh(&self, st: &mut State, full: bool) -> Result<(), UnitError> {
        let dev = &self.s.dev;
        let bs = self.s.bs;
        let mut sb = None;
        for _ in 0..SUPER_RETRIES {
            match read_super(dev, bs) {
                Ok(s) => {
                    sb = Some(s);
                    break;
                }
                // 两份都在被重写（别的进程连着提交了两次），再读一次。
                Err(OpenError::NotFormatted) => core::hint::spin_loop(),
                Err(_) => return Err(UnitError::Corrupt),
            }
        }
        let sb = sb.ok_or(UnitError::Corrupt)?;
        let moved = sb.generation != st.sb.generation;
        if !moved && !(full && st.space_stale) {
            return Ok(());
        }
        if sb.blocks.saturating_mul(bs as u64) > dev.capacity() {
            device_err(dev.refresh())?;
            if sb.blocks.saturating_mul(bs as u64) > dev.capacity() {
                return Err(UnitError::Corrupt);
            }
        }
        if full {
            let (free, recent, free_list) = read_space(dev, bs, &sb).map_err(|e| match e {
                OpenError::Unit(e) => e,
                _ => UnitError::Corrupt,
            })?;
            st.free = free;
            st.free_list = free_list;
            st.fresh = ExtentSet::new();
            st.inflight = ExtentSet::new();
            // 本进程里还在读旧版本的读者也可能读着它们：等这些读者结束（新的版本号），
            // 也等别的进程都不在读。
            st.pending.clear();
            if !recent.is_empty() {
                st.pending.push(Released { epoch: st.epoch + 1, blocks: recent, after_commit: false, drain: true });
            }
            st.space_stale = false;
            st.dirty = false;
        } else {
            st.space_stale = true;
        }
        if moved {
            st.tree = Tree::open((sb.root != 0).then_some(sb.root), bs);
            st.next_object = sb.next_object;
            st.sb = sb;
        }
        self.publish(st);
        Ok(())
    }

    /// 在当前版本上读，读的期间登记为读者。
    fn view<T>(&self, f: impl FnOnce(&View<'_, D>) -> Result<T, UnitError>) -> Result<T, UnitError> {
        let (tree, _pin) = self.pin_current()?;
        f(&View { tree: &tree, dev: &self.s.dev, bs: self.bs() })
    }

    /// 登记为当前版本的读者。和别的进程共用设备时先共享持有读者锁；不是写者就先跟上别的进程的提交。
    fn pin_current(&self) -> Result<(Tree, Pin<D>), UnitError> {
        let held = match self.share() {
            None => false,
            Some(share) => {
                device_err(share.hold_readers())?;
                if !self.s.is_writer.load(Ordering::Acquire) {
                    let mut st = self.lock();
                    if !st.writer
                        && let Err(e) = self.refresh(&mut st, false)
                    {
                        drop(st);
                        share.release_readers();
                        return Err(e);
                    }
                }
                true
            }
        };
        let mut cur = self.s.current.lock();
        let epoch = cur.epoch;
        *cur.readers.entry(epoch).or_default() += 1;
        Ok((cur.tree.view(), Pin { s: self.s.clone(), epoch, held }))
    }

    /// 把改好的状态公布成新版本。
    fn publish(&self, st: &mut State) {
        st.epoch += 1;
        {
            let mut cur = self.s.current.lock();
            cur.tree = st.tree.view();
            cur.epoch = st.epoch;
        }
        self.reclaim(st, false);
    }

    /// 不再有人引用的已释放块，放回可分配的空间。返回是否放回了什么。
    ///
    /// 等别的进程不在读的块，在每个提交号上只试一次（试要好几次系统调用），`force` 时（空间不够）再试。
    fn reclaim(&self, st: &mut State, force: bool) -> bool {
        match self.share() {
            None => {
                // 没有别的进程。
                for r in &mut st.pending {
                    r.drain = false;
                }
            }
            Some(share) => {
                if !st.writer {
                    return false;
                }
                let waiting = st.pending.iter().any(|r| r.drain && !r.after_commit);
                if waiting && (force || st.drain_tried != Some(st.sb.generation)) {
                    if share.readers_absent().unwrap_or(false) {
                        st.drain_tried = None;
                        for r in st.pending.iter_mut().filter(|r| !r.after_commit) {
                            r.drain = false;
                        }
                    } else {
                        st.drain_tried = Some(st.sb.generation);
                    }
                }
            }
        }
        let oldest = self.s.current.lock().readers.keys().next().copied();
        let published = st.epoch;
        let (ready, waiting): (Vec<_>, Vec<_>) = core::mem::take(&mut st.pending)
            .into_iter()
            .partition(|r| !r.after_commit && !r.drain && r.epoch <= published && oldest.is_none_or(|o| o >= r.epoch));
        st.pending = waiting;
        let any = !ready.is_empty();
        for r in ready {
            st.free.extend(&r.blocks);
        }
        any
    }

    /// 在写者状态上做一次改动，`commit` 时随即提交，然后公布。失败时状态不变；空间不够时，
    /// 先收回能收回的块，再试着扩容，然后整个重做。调用者持有写者锁。
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
                    if e == UnitError::NoSpace && (self.reclaim(st, true) || self.grow(st)) {
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    /// 一次不提交的改动（内容写入）。
    fn update<T>(&self, mut f: impl FnMut(&mut Work<'_, D>) -> Result<T, UnitError>) -> Result<T, UnitError> {
        let mut st = self.writer()?;
        st.active = true;
        let r = self.run(&mut st, false, &mut f);
        self.release(&mut st);
        r
    }

    /// 一次随即提交的改动（元数据修改）。
    fn transact<T>(&self, mut f: impl FnMut(&mut Work<'_, D>) -> Result<T, UnitError>) -> Result<T, UnitError> {
        let mut st = self.writer()?;
        let r = self.run(&mut st, true, &mut f);
        self.release(&mut st);
        r
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
        // 旧的空闲表在这次提交之后就没人引用了。只有拿着写者锁的进程读空闲表，所以它们不用等别的进程。
        let mut retired = ExtentSet::new();
        for &b in &st.free_list {
            retired.insert(b, 1);
        }

        // 空闲表本身也要占块：先分配，再把分配后的空闲集合写进去。正在锁外写入的块在盘上也算空闲：
        // 它们还没被登记，断电之后没人引用。还没能再分配的已释放块进第二张表：下一个写者
        // （也许是别的进程）要等别的进程都不在读了才能用它们。只有一个进程用的设备上，重新挂载时
        // 没有任何读者，它们直接进第一张表。
        let shared = self.share().is_some();
        let per = free_list_per_block(bs);
        let mut list_blocks = Vec::new();
        let (free, recent) = loop {
            let mut free = st.free.clone();
            free.extend(&retired);
            free.extend(&st.inflight);
            let mut recent = ExtentSet::new();
            for r in &st.pending {
                if shared { recent.extend(&r.blocks) } else { free.extend(&r.blocks) }
            }
            if list_blocks.len() >= free.count().div_ceil(per) + recent.count().div_ceil(per) {
                break (free, recent);
            }
            list_blocks.push(alloc_one(&mut st.free, &mut st.fresh)?);
        };
        let (free_blocks, recent_blocks) = list_blocks.split_at(list_blocks.len() - recent.count().div_ceil(per));
        write_chain(dev, bs, free_blocks, &free)?;
        write_chain(dev, bs, recent_blocks, &recent)?;
        device_err(dev.flush())?;

        let sb = Super {
            generation,
            root: root.unwrap_or(0),
            free_head: free_blocks.first().copied().unwrap_or(0),
            recent_head: recent_blocks.first().copied().unwrap_or(0),
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
            st.pending.push(Released { epoch: 0, blocks: retired, after_commit: false, drain: false });
        }
        st.fresh = st.inflight.clone();
        st.dirty = false;
        st.dirty_ticks = 0;
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
        // 锁外写着的块还没登记，写者锁不会被放掉。
        let mut st = self.lock();
        let r = written.and_then(|()| self.run(&mut st, false, &mut |w| w.finish_write(&plan)));
        if r.is_err() {
            unreserve(&mut st, &plan.blocks);
        }
        self.release(&mut st);
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
        self.release(&mut st);
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

    /// 直接读 Tag 索引：满足叶子条件的实体和命中的 Tag，按索引的顺序（一个实体在同一个键上有几个值
    /// 就出现几次）。只要值的时候（例如一个键下全部的路径）不必再逐个取实体。
    pub fn index(&self, probe: &Probe<'_>) -> Result<Vec<(EntityId, Tag)>, UnitError> {
        self.view(|v| v.probe_tags(probe))
    }

    /// 现有的全部暂存，含别的进程的。
    pub fn stages(&self) -> Result<Vec<StageId>, UnitError> {
        self.view(|v| v.stages()).map(|s| s.into_iter().map(|(s, _)| s).collect())
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
            let mut st = self.writer()?;
            let held = match self.share() {
                Some(share) => {
                    if share.hold_readers().is_err() {
                        self.release(&mut st);
                        return Err(UnitError::Device);
                    }
                    true
                }
                None => false,
            };
            let pin = {
                let mut cur = self.s.current.lock();
                *cur.readers.entry(st.epoch).or_default() += 1;
                Pin { s: self.s.clone(), epoch: st.epoch, held }
            };
            let pending: Vec<ExtentSet> = st.pending.iter().map(|r| r.blocks.clone()).collect();
            let snapshot = (
                st.tree.view(),
                st.free.clone(),
                pending,
                st.inflight.clone(),
                st.free_list.clone(),
                st.sb.blocks,
                st.next_object,
                pin,
            );
            self.release(&mut st);
            snapshot
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
                keys::PREPARED | keys::COMMITTED | keys::AUX | keys::TXN_OWNER => {}
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

/// 把 `set` 写成一张空闲表，占 `blocks` 这些块（按顺序串起来；多出来的块写成空的）。
fn write_chain<D: BlockDevice + ?Sized>(dev: &D, bs: usize, blocks: &[u64], set: &ExtentSet) -> Result<(), UnitError> {
    let extents: Vec<(u64, u64)> = set.iter().collect();
    let mut parts = extents.chunks(free_list_per_block(bs));
    for (i, &b) in blocks.iter().enumerate() {
        let part = parts.next().unwrap_or(&[]);
        let next = blocks.get(i + 1).copied().unwrap_or(0);
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
    Ok(())
}

/// 读超级块记着的两张空闲表：谁都能用的、等别的进程不在读的，以及它们占用的块。
fn read_space<D: BlockDevice + ?Sized>(
    dev: &D,
    bs: usize,
    sb: &Super,
) -> Result<(ExtentSet, ExtentSet, Vec<u64>), OpenError> {
    let mut list = Vec::new();
    let mut seen = ExtentSet::new();
    let free = read_chain(dev, bs, sb.free_head, sb.blocks, &mut list, &mut seen)?;
    let recent = read_chain(dev, bs, sb.recent_head, sb.blocks, &mut list, &mut seen)?;
    Ok((free, recent, list))
}

/// 读一张空闲表。`list` 收集表占用的块，`seen` 是已经读到的全部区段（两张表不能重叠）。
fn read_chain<D: BlockDevice + ?Sized>(
    dev: &D,
    bs: usize,
    head: u64,
    blocks: u64,
    list: &mut Vec<u64>,
    seen: &mut ExtentSet,
) -> Result<ExtentSet, UnitError> {
    let mut out = ExtentSet::new();
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
            if s < 2 || l == 0 || s + l > blocks || seen.overlaps(s, l) {
                return Err(UnitError::Corrupt);
            }
            seen.insert(s, l);
            out.insert(s, l);
        }
    }
    Ok(out)
}

impl<D: BlockDevice + 'static> Unit for BlockUnit<D> {
    fn id(&self) -> UnitId {
        self.s.unit
    }

    fn snapshot(&self) -> Box<dyn Snapshot> {
        Box::new(BlockSnapshot { inner: self.pin_current() })
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
        // 不是写者就没有没提交的东西。
        if !self.lock().writer {
            return Ok(());
        }
        let mut st = self.writer()?;
        let r = if st.dirty { self.run(&mut st, true, &mut |_| Ok(())) } else { Ok(()) };
        self.release(&mut st);
        r
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

    /// 本进程的，和已经不在的进程留下的。别的进程正在进行的事务由它自己了结。
    fn prepared(&self) -> Result<Vec<(TxnInfo, Vec<Change>)>, UnitError> {
        let all = self.view(|v| v.prepared())?;
        Ok(all.into_iter().filter(|(_, _, owner)| self.ours(*owner)).map(|(t, c, _)| (t, c)).collect())
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

    /// 同 [`Unit::prepared`]：别的进程正在进行的事务不在其中。
    fn committed(&self) -> Result<Vec<TxnInfo>, UnitError> {
        let all = self.view(|v| v.committed())?;
        Ok(all.into_iter().filter(|(_, owner)| self.ours(*owner)).map(|(t, _)| t).collect())
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

/// 拿不到版本（和别的进程共用设备时读超级块失败）时，每次读都报这个错。
struct BlockSnapshot<D: BlockDevice> {
    inner: Result<(Tree, Pin<D>), UnitError>,
}

impl<D: BlockDevice> BlockSnapshot<D> {
    fn with_view<T>(&self, f: impl FnOnce(&View<'_, D>) -> Result<T, UnitError>) -> Result<T, UnitError> {
        let (tree, pin) = self.inner.as_ref().map_err(|e| *e)?;
        f(&View { tree, dev: &pin.s.dev, bs: pin.s.bs as u64 })
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
