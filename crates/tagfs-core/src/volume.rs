//! 卷：多个存储单元组成的整体，也是 TagFS 的逻辑层。
//!
//! - 可见性：实体身上每道边界都能越过才可见；不可见的实体在所有读接口上都和不存在一样。
//! - 查询：分发给每个在线单元，结果合并。
//! - 元数据事务：只涉及一个单元时直接提交；涉及多个单元时做两阶段提交，仍是一个原子操作。
//!   事务没在所有参与单元上完成之前，涉及的实体带 `tagfs:txn_pending`，结果确定前只读。
//! - 内容：按偏移直接读写所在单元，不经过事务（和 NTFS、ext4 一样）。要和实体一起原子出现的内容
//!   先写进暂存（[`Volume::stage`]），建实体时引用它。
//! - 并发：卷的方法都可以被多个线程同时调用。读不加锁；修改按实体加锁，同一个实体上的写入和修改
//!   排队，不同实体互不相干。

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use tagfs_sync::{Keyed, Mutex, RwLock};

use crate::id::{EntityId, IdSource, TxnId};
use crate::policy::Policy;
use crate::query::Query;
use crate::tag::{Key, Namespace, Tag, Value, tagfs_keys};
use crate::unit::{Change, EntityRecord, Snapshot, StageId, TxnInfo, Unit, UnitError, UnitId};

/// 跨单元复制时一次搬多少。
const COPY_CHUNK: usize = 1 << 20;

/// 调用者看到的实体。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entity {
    pub id: EntityId,
    pub tags: Vec<Tag>,
    pub size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// 实体不存在，或对调用者不可见（两者不作区分）。
    NotFound,
    /// 实体可见，但调用者不能做这项修改。
    NotWritable,
    /// 实体涉及一个未完成的跨单元事务，结果确定前只读。
    Pending,
    /// 指定的单元不在线，或卷里没有在线单元。
    NoUnit,
    /// 跨单元复制的源实体是本事务里新建、尚未提交的。
    Uncommitted,
    /// Tag 的命名空间、键或字符串值超过长度上限。
    TagTooLong,
    Unit(UnitError),
}

impl From<UnitError> for Error {
    fn from(e: UnitError) -> Self {
        Error::Unit(e)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotFound => f.write_str("entity not found"),
            Error::NotWritable => f.write_str("not writable"),
            Error::Pending => f.write_str("entity is part of an unfinished transaction"),
            Error::NoUnit => f.write_str("storage unit not online"),
            Error::Uncommitted => f.write_str("source entity is not committed yet"),
            Error::TagTooLong => f.write_str("tag exceeds the length limits"),
            Error::Unit(e) => write!(f, "{e}"),
        }
    }
}

impl core::error::Error for Error {}

type Units = Vec<Arc<dyn Unit>>;
/// 实体所在的单元和它的记录。
type Located = (Arc<dyn Unit>, EntityRecord);

pub struct Volume<P: Policy> {
    policy: P,
    ids: Mutex<Box<dyn IdSource + Send>>,
    /// 在线单元，按加入顺序。没指定单元时新实体放进第一个。
    units: RwLock<Units>,
    /// 按实体加的锁：读内容共享，写内容和元数据修改独占。
    locks: Keyed<EntityId>,
    /// 正在进行的跨单元事务。恢复时不碰它们：它们的准备记录还没到该了结的时候。
    in_flight: Mutex<BTreeSet<TxnId>>,
    /// 恢复一次只做一个。
    resolving: Mutex<()>,
}

fn pending_tag(txn: TxnId) -> Tag {
    Tag { ns: Namespace::tagfs(), key: Key::new(tagfs_keys::TXN_PENDING), value: Value::Str(txn.to_string()) }
}

fn is_pending(tags: &[Tag]) -> bool {
    tags.iter().any(|t| t.ns.is_tagfs() && t.key.as_str() == tagfs_keys::TXN_PENDING)
}

fn find(units: &Units, id: UnitId) -> Option<&Arc<dyn Unit>> {
    units.iter().find(|u| u.id() == id)
}

/// 一份暂存的内容：写好之后交给 [`Txn::create_from`]，随实体一起出现。没用上就在放掉时丢掉。
pub struct Stage {
    unit: Arc<dyn Unit>,
    id: StageId,
    used: bool,
}

impl Stage {
    /// 暂存所在的单元，也就是用它建出的实体所在的单元。
    pub fn unit(&self) -> UnitId {
        self.unit.id()
    }

    /// 从 `offset` 起写入，超过当前长度的部分补零扩展。
    pub fn write(&self, offset: u64, data: &[u8]) -> Result<(), Error> {
        Ok(self.unit.write_staged(self.id, offset, data)?)
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        if !self.used {
            let _ = self.unit.discard_staged(self.id);
        }
    }
}

impl<P: Policy> Volume<P> {
    pub fn new(policy: P, ids: Box<dyn IdSource + Send>) -> Self {
        Volume {
            policy,
            ids: Mutex::new(ids),
            units: RwLock::new(Vec::new()),
            locks: Keyed::new(),
            in_flight: Mutex::new(BTreeSet::new()),
            resolving: Mutex::new(()),
        }
    }

    /// 单元上线。它带着的未了结事务在这里恢复：能确定结果的照结果完成，不能确定的
    /// 给涉及的实体打上 `tagfs:txn_pending`，等主单元上线再定。
    pub fn attach(&self, unit: Arc<dyn Unit>) -> Result<(), Error> {
        {
            let mut units = self.units.write();
            if find(&units, unit.id()).is_none() {
                units.push(unit);
            }
        }
        self.resolve()
    }

    /// 单元离线，它的实体随之从卷里消失。正在进行的读写还拿着它的，做完为止。
    pub fn detach(&self, id: UnitId) -> Option<Arc<dyn Unit>> {
        let mut units = self.units.write();
        let i = units.iter().position(|u| u.id() == id)?;
        Some(units.remove(i))
    }

    pub fn unit_ids(&self) -> Vec<UnitId> {
        self.units.read().iter().map(|u| u.id()).collect()
    }

    pub fn unit(&self, id: UnitId) -> Option<Arc<dyn Unit>> {
        find(&self.units.read(), id).cloned()
    }

    fn units(&self) -> Units {
        self.units.read().clone()
    }

    fn new_id(&self) -> EntityId {
        EntityId::generate(&mut **self.ids.lock())
    }

    /// 找实体所在的单元，不看可见性。
    fn locate(&self, id: &EntityId) -> Result<Option<Located>, Error> {
        for u in self.units() {
            if let Some(r) = u.snapshot().get(id)? {
                return Ok(Some((u, r)));
            }
        }
        Ok(None)
    }

    fn visible(&self, ctx: &P::Context, tags: &[Tag]) -> bool {
        // tags 已按 (命名空间, 键, 值) 排好，同一个键的值相邻。
        let mut i = 0;
        while i < tags.len() {
            let (ns, key) = (&tags[i].ns, &tags[i].key);
            let mut j = i;
            while j < tags.len() && tags[j].is(ns, key) {
                j += 1;
            }
            if self.policy.is_boundary(ns, key) {
                let values: Vec<&Value> = tags[i..j].iter().map(|t| &t.value).collect();
                if !self.policy.crosses(ctx, ns, key, &values) {
                    return false;
                }
            }
            i = j;
        }
        true
    }

    /// 实体所在的单元和记录；不可见当作不存在，返回 `NotFound`。
    fn visible_record(&self, ctx: &P::Context, id: &EntityId) -> Result<Located, Error> {
        self.locate(id)?.filter(|(_, r)| self.visible(ctx, &r.tags)).ok_or(Error::NotFound)
    }

    /// 调用者能修改的实体所在的单元：可见、不在未完成事务里、策略允许修改。
    fn modifiable(&self, ctx: &P::Context, id: &EntityId) -> Result<Arc<dyn Unit>, Error> {
        let (unit, r) = self.visible_record(ctx, id)?;
        if is_pending(&r.tags) {
            return Err(Error::Pending);
        }
        if !self.policy.can_modify(ctx, &r.tags) {
            return Err(Error::NotWritable);
        }
        Ok(unit)
    }

    /// 调用者看得到的那部分 Tag。
    fn readable(&self, ctx: &P::Context, tags: Vec<Tag>) -> Vec<Tag> {
        tags.into_iter().filter(|t| self.policy.can_read(ctx, &t.ns)).collect()
    }

    /// 实体的 Tag（只含调用者看得到的命名空间）和长度。
    pub fn get(&self, ctx: &P::Context, id: &EntityId) -> Result<Entity, Error> {
        let (_, r) = self.visible_record(ctx, id)?;
        Ok(Entity { id: r.id, tags: self.readable(ctx, r.tags), size: r.size })
    }

    /// 从 `offset` 起读内容，返回读到的字节数（到末尾时少于 `buf.len()`）。读多少由调用者决定。
    pub fn read(&self, ctx: &P::Context, id: &EntityId, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        let _lock = self.locks.read(id);
        let (unit, _) = self.visible_record(ctx, id)?;
        Ok(unit.snapshot().read(id, offset, buf)?)
    }

    /// 从 `offset` 起写内容，超过当前长度的部分补零扩展。直接写进实体所在的单元，立刻对读者可见；
    /// [`Volume::flush`] 之后保证落盘。
    pub fn write(&self, ctx: &P::Context, id: &EntityId, offset: u64, data: &[u8]) -> Result<(), Error> {
        let _lock = self.locks.write(id);
        Ok(self.modifiable(ctx, id)?.write(id, offset, data)?)
    }

    /// 把内容截短或补零加长到 `len`。
    pub fn set_len(&self, ctx: &P::Context, id: &EntityId, len: u64) -> Result<(), Error> {
        let _lock = self.locks.write(id);
        Ok(self.modifiable(ctx, id)?.set_len(id, len)?)
    }

    /// 让实体所在单元之前的写入都落盘。
    pub fn flush(&self, ctx: &P::Context, id: &EntityId) -> Result<(), Error> {
        let (unit, _) = self.visible_record(ctx, id)?;
        Ok(unit.sync()?)
    }

    /// 让所有在线单元之前的写入都落盘。
    pub fn sync(&self) -> Result<(), Error> {
        let mut first = Ok(());
        for u in self.units() {
            if let Err(e) = u.sync() {
                first = first.and(Err(e.into()));
            }
        }
        first
    }

    /// 新的一份暂存。`unit` 为空时由卷决定放在哪个单元。
    pub fn stage(&self, unit: Option<UnitId>) -> Result<Stage, Error> {
        let unit = self.placement(unit)?;
        let id = unit.stage()?;
        Ok(Stage { unit, id, used: false })
    }

    fn placement(&self, unit: Option<UnitId>) -> Result<Arc<dyn Unit>, Error> {
        let units = self.units.read();
        match unit {
            Some(u) => find(&units, u).cloned(),
            None => units.first().cloned(),
        }
        .ok_or(Error::NoUnit)
    }

    /// 满足条件、且对调用者可见的实体，按 EntityId 排序。条件只对调用者看得到的 Tag 判断。
    pub fn query(&self, ctx: &P::Context, query: &Query) -> Result<Vec<EntityId>, Error> {
        self.query_page(ctx, query, None, usize::MAX)
    }

    /// 分页查询：EntityId 大于 `after` 的结果中最前面的至多 `limit` 个。拿上一页的最后一个作为下一页的 `after`。
    pub fn query_page(
        &self,
        ctx: &P::Context,
        query: &Query,
        after: Option<&EntityId>,
        limit: usize,
    ) -> Result<Vec<EntityId>, Error> {
        let mut out = Vec::new();
        for u in self.units() {
            let snap = u.snapshot();
            let candidates = match candidates(&*snap, query)? {
                Some(c) => c,
                None => snap.all()?,
            };
            for id in candidates {
                if after.is_some_and(|a| id <= *a) {
                    continue;
                }
                if let Some(r) = snap.get(&id)?
                    && self.visible(ctx, &r.tags)
                    && query.matches(&self.readable(ctx, r.tags))
                {
                    out.push(id);
                }
            }
        }
        out.sort();
        out.dedup();
        out.truncate(limit);
        Ok(out)
    }

    /// 同 [`Volume::query`]，连同实体一起返回（Tag 只含调用者看得到的命名空间），按 EntityId 排序。
    /// 判断可见性时已经取过记录，调用者要用 Tag 时不必再逐个 [`Volume::get`]。
    pub fn query_entities(&self, ctx: &P::Context, query: &Query) -> Result<Vec<Entity>, Error> {
        let mut out: BTreeMap<EntityId, Entity> = BTreeMap::new();
        for u in self.units() {
            let snap = u.snapshot();
            let candidates = match candidates(&*snap, query)? {
                Some(c) => c,
                None => snap.all()?,
            };
            for id in candidates {
                if out.contains_key(&id) {
                    continue;
                }
                if let Some(r) = snap.get(&id)?
                    && self.visible(ctx, &r.tags)
                {
                    let tags = self.readable(ctx, r.tags);
                    if query.matches(&tags) {
                        out.insert(id, Entity { id, tags, size: r.size });
                    }
                }
            }
        }
        Ok(out.into_values().collect())
    }

    pub fn begin<'a>(&'a self, ctx: &'a P::Context) -> Txn<'a, P> {
        Txn {
            vol: self,
            ctx,
            changes: BTreeMap::new(),
            created: BTreeMap::new(),
            deleted: BTreeSet::new(),
            stages: Vec::new(),
        }
    }

    /// 依据在线单元的状态推进所有未了结的跨单元事务（正在进行的除外）。
    fn resolve(&self) -> Result<(), Error> {
        let _one = self.resolving.lock();
        let units = self.units();
        let in_flight = self.in_flight.lock().clone();
        // 带准备记录的单元：问主单元。
        let mut work = Vec::new();
        for u in &units {
            for (info, changes) in u.prepared()? {
                if !in_flight.contains(&info.id) {
                    work.push((u.clone(), info, changes));
                }
            }
        }
        for (unit, info, changes) in work {
            let Some(primary) = find(&units, info.primary) else {
                // 主单元不在线，结果不能确定：给这个单元里已存在的、涉及的实体打标记。
                mark_in_doubt(&*unit, info.id, &changes)?;
                continue;
            };
            let committed = primary.committed()?.iter().any(|t| t.id == info.id);
            if committed {
                unit.commit_prepared(info.id, false)?;
            } else if has_prepared(&*unit, info.id)? {
                // 主单元没有提交：要么它自己还停在准备阶段（提交点之前就断了），
                // 要么它已经放弃。两种情况都放弃这个事务。
                abort_everywhere(&units, info.id)?;
            }
        }
        // 主单元记下的已提交事务：全部参与单元都完成后，去掉标记并清掉记录。
        let mut done = Vec::new();
        for u in &units {
            for info in u.committed()? {
                if in_flight.contains(&info.id) {
                    continue;
                }
                let mut finished = true;
                for p in &info.participants {
                    finished &= match find(&units, *p) {
                        Some(pu) => !has_prepared(&**pu, info.id)?,
                        None => false,
                    };
                }
                if finished {
                    done.push((u.clone(), info));
                }
            }
        }
        for (primary, info) in done {
            for p in &info.participants {
                if let Some(pu) = find(&units, *p) {
                    clear_pending(&**pu, info.id)?;
                }
            }
            primary.forget(info.id)?;
        }
        Ok(())
    }
}

fn has_prepared(unit: &dyn Unit, txn: TxnId) -> Result<bool, Error> {
    Ok(unit.prepared()?.iter().any(|(t, _)| t.id == txn))
}

fn abort_everywhere(units: &Units, txn: TxnId) -> Result<(), Error> {
    for u in units {
        if has_prepared(&**u, txn)? {
            u.abort_prepared(txn)?;
            clear_pending(&**u, txn)?;
        }
    }
    Ok(())
}

fn mark_in_doubt(unit: &dyn Unit, txn: TxnId, changes: &[Change]) -> Result<(), Error> {
    let snap = unit.snapshot();
    let tag = pending_tag(txn);
    let mut marks = Vec::new();
    for id in changes.iter().map(Change::entity).collect::<BTreeSet<_>>() {
        if snap.get(&id)?.is_some_and(|r| !r.tags.contains(&tag)) {
            marks.push(Change::AddTag { id, tag: tag.clone() });
        }
    }
    if !marks.is_empty() {
        unit.apply(&marks)?;
    }
    Ok(())
}

fn clear_pending(unit: &dyn Unit, txn: TxnId) -> Result<(), Error> {
    let tag = pending_tag(txn);
    let snap = unit.snapshot();
    let probe = crate::query::Probe::Is(&tag);
    let ids = match snap.probe(&probe)? {
        Some(ids) => ids,
        None => {
            let mut ids = Vec::new();
            for id in snap.all()? {
                if snap.get(&id)?.is_some_and(|r| r.tags.contains(&tag)) {
                    ids.push(id);
                }
            }
            ids
        }
    };
    let clears: Vec<Change> = ids.into_iter().map(|id| Change::RemoveTag { id, tag: tag.clone() }).collect();
    if !clears.is_empty() {
        unit.apply(&clears)?;
    }
    Ok(())
}

/// 用索引算候选集；算不出时返回 `None`（改为扫描）。候选集只是上界，最后还要逐个核对。
fn candidates(snap: &dyn Snapshot, query: &Query) -> Result<Option<Vec<EntityId>>, UnitError> {
    Ok(match query {
        Query::And(qs) => {
            let mut acc: Option<BTreeSet<EntityId>> = None;
            for q in qs {
                if let Some(s) = candidates(snap, q)? {
                    acc = Some(match acc {
                        None => s.into_iter().collect(),
                        Some(acc) => s.into_iter().filter(|id| acc.contains(id)).collect(),
                    });
                }
            }
            acc.map(|s| s.into_iter().collect())
        }
        Query::Or(qs) => {
            let mut all = BTreeSet::new();
            for q in qs {
                match candidates(snap, q)? {
                    Some(s) => all.extend(s),
                    None => return Ok(None),
                }
            }
            Some(all.into_iter().collect())
        }
        Query::All | Query::Not(_) => None,
        leaf => snap.probe(&leaf.probe().expect("leaf"))?,
    })
}

/// 一组元数据修改。提交前不影响卷；不提交就放弃。
pub struct Txn<'a, P: Policy> {
    vol: &'a Volume<P>,
    ctx: &'a P::Context,
    changes: BTreeMap<UnitId, Vec<Change>>,
    /// 本事务新建的实体：所在单元和建时的 Tag。
    created: BTreeMap<EntityId, (UnitId, Vec<Tag>)>,
    deleted: BTreeSet<EntityId>,
    /// 新建实体引用的暂存。提交时交给单元用掉；没提交就随事务丢掉。
    stages: Vec<Stage>,
}

impl<P: Policy> Txn<'_, P> {
    /// 要修改的实体：所在单元和当前 Tag。不可见当作不存在；在未完成事务里则只读。
    fn target(&self, id: &EntityId) -> Result<(UnitId, Vec<Tag>), Error> {
        if self.deleted.contains(id) {
            return Err(Error::NotFound);
        }
        if let Some(created) = self.created.get(id) {
            return Ok(created.clone());
        }
        let (unit, r) = self.vol.visible_record(self.ctx, id)?;
        if is_pending(&r.tags) {
            return Err(Error::Pending);
        }
        Ok((unit.id(), r.tags))
    }

    /// 能否在 `unit` 里的实体上增删这个 Tag。`entity` 是实体当前的 Tag；新建实体时为 `None`。
    fn check_tag_writable(&self, tag: &Tag, unit: UnitId, entity: Option<&[Tag]>) -> Result<(), Error> {
        if !tag.within_limits() {
            return Err(Error::TagTooLong);
        }
        let allowed = if tag.ns.is_tagfs() {
            // 单元映射的键按修改实体授权；新建实体时，能建就能带。
            let mapped = self.vol.unit(unit).is_some_and(|u| u.mapped_keys().contains(&tag.key.as_str()));
            mapped && entity.is_none_or(|tags| self.vol.policy.can_modify(self.ctx, tags))
        } else {
            self.vol.policy.can_write(self.ctx, &tag.ns)
        };
        if allowed { Ok(()) } else { Err(Error::NotWritable) }
    }

    /// 新实体（放在 `unit` 里）的 Tag：调用者给的（须可写）加上操作系统在创建时附加的。
    fn creation_tags(&self, unit: UnitId, tags: Vec<Tag>) -> Result<Vec<Tag>, Error> {
        for t in &tags {
            self.check_tag_writable(t, unit, None)?;
        }
        let mut extra = self.vol.policy.on_create(self.ctx).ok_or(Error::NotWritable)?;
        if extra.iter().any(|t| t.ns.is_tagfs()) {
            return Err(Error::NotWritable);
        }
        if !extra.iter().all(Tag::within_limits) {
            return Err(Error::TagTooLong);
        }
        extra.extend(tags);
        Ok(crate::tag::normalized(&extra))
    }

    fn push(&mut self, unit: UnitId, change: Change) {
        self.changes.entry(unit).or_default().push(change);
    }

    /// 新建内容为空的实体。`unit` 为空时由卷决定放在哪个单元。
    pub fn create(&mut self, unit: Option<UnitId>, tags: Vec<Tag>) -> Result<EntityId, Error> {
        let unit = self.vol.placement(unit)?.id();
        self.insert(unit, tags, None)
    }

    /// 新建实体，内容是暂存里写好的；实体放在暂存所在的单元。
    pub fn create_from(&mut self, stage: Stage, tags: Vec<Tag>) -> Result<EntityId, Error> {
        let unit = stage.unit();
        if self.vol.unit(unit).is_none() {
            return Err(Error::NoUnit);
        }
        self.insert(unit, tags, Some(stage))
    }

    fn insert(&mut self, unit: UnitId, tags: Vec<Tag>, stage: Option<Stage>) -> Result<EntityId, Error> {
        let tags = self.creation_tags(unit, tags)?;
        let id = self.vol.new_id();
        self.created.insert(id, (unit, tags.clone()));
        self.push(unit, Change::Create { id, tags, content: stage.as_ref().map(|s| s.id) });
        self.stages.extend(stage);
        Ok(id)
    }

    /// 复制：新建实体，带 `tags`，内容和 `from` 相同。同一单元里两者共享对象，直到任一方修改；
    /// 跨单元时内容在这里搬进目标单元的暂存，提交时随新实体一起出现。
    pub fn copy(&mut self, from: &EntityId, unit: Option<UnitId>, tags: Vec<Tag>) -> Result<EntityId, Error> {
        let (src_unit, _) = self.target_readable(from)?;
        let unit = match unit {
            Some(_) => self.vol.placement(unit)?.id(),
            None => src_unit,
        };
        if unit == src_unit {
            let tags = self.creation_tags(unit, tags)?;
            let id = self.vol.new_id();
            self.push(unit, Change::Copy { from: *from, id, tags: tags.clone() });
            self.created.insert(id, (unit, tags));
            return Ok(id);
        }
        if self.created.contains_key(from) {
            return Err(Error::Uncommitted);
        }
        let stage = self.vol.stage(Some(unit))?;
        self.copy_content(src_unit, from, &stage)?;
        self.insert(unit, tags, Some(stage))
    }

    /// 把 `from` 的内容分段搬进暂存。
    fn copy_content(&self, src: UnitId, from: &EntityId, stage: &Stage) -> Result<(), Error> {
        let _lock = self.vol.locks.read(from);
        let snap = self.vol.unit(src).ok_or(Error::NoUnit)?.snapshot();
        let mut buf = vec![0u8; COPY_CHUNK];
        let mut offset = 0u64;
        loop {
            let n = snap.read(from, offset, &mut buf)?;
            if n == 0 {
                return Ok(());
            }
            stage.write(offset, &buf[..n])?;
            offset += n as u64;
        }
    }

    /// 可读即可（复制源不要求可写，也不受未完成事务的只读限制）。
    fn target_readable(&self, id: &EntityId) -> Result<(UnitId, Vec<Tag>), Error> {
        if self.deleted.contains(id) {
            return Err(Error::NotFound);
        }
        if let Some(created) = self.created.get(id) {
            return Ok(created.clone());
        }
        let (unit, r) = self.vol.visible_record(self.ctx, id)?;
        Ok((unit.id(), r.tags))
    }

    pub fn delete(&mut self, id: &EntityId) -> Result<(), Error> {
        let (unit, tags) = self.target(id)?;
        if !self.vol.policy.can_modify(self.ctx, &tags) {
            return Err(Error::NotWritable);
        }
        self.created.remove(id);
        self.deleted.insert(*id);
        self.push(unit, Change::Delete { id: *id });
        Ok(())
    }

    pub fn add_tag(&mut self, id: &EntityId, tag: Tag) -> Result<(), Error> {
        let (unit, tags) = self.target(id)?;
        self.check_tag_writable(&tag, unit, Some(&tags))?;
        if let Some((_, tags)) = self.created.get_mut(id) {
            tags.push(tag.clone());
        }
        self.push(unit, Change::AddTag { id: *id, tag });
        Ok(())
    }

    pub fn remove_tag(&mut self, id: &EntityId, tag: Tag) -> Result<(), Error> {
        let (unit, tags) = self.target(id)?;
        self.check_tag_writable(&tag, unit, Some(&tags))?;
        if let Some((_, tags)) = self.created.get_mut(id) {
            tags.retain(|t| t != &tag);
        }
        self.push(unit, Change::RemoveTag { id: *id, tag });
        Ok(())
    }

    /// 放弃。什么都不生效（和直接丢掉事务一样，写出来更清楚）。
    pub fn abort(self) {}

    /// 提交。要么全部生效，要么全部不生效。
    pub fn commit(self) -> Result<(), Error> {
        let Txn { vol, changes, mut stages, .. } = self;
        // 锁住涉及的实体（复制的源也锁住，免得它在提交前被删掉），再确认它们还能改：
        // 加锁之前别人可能已经删掉了它们，或者把它们卷进了别的未完成事务。
        let mut touched = BTreeSet::new();
        let mut born = BTreeSet::new();
        for c in changes.values().flatten() {
            touched.insert(c.entity());
            match c {
                Change::Create { id, .. } => {
                    born.insert(*id);
                }
                Change::Copy { from, id, .. } => {
                    touched.insert(*from);
                    born.insert(*id);
                }
                _ => {}
            }
        }
        let _locks = vol.locks.write_all(touched.iter().copied());
        for id in touched.difference(&born) {
            if let Some((_, r)) = vol.locate(id)?
                && is_pending(&r.tags)
            {
                return Err(Error::Pending);
            }
        }
        match changes.len() {
            0 => Ok(()),
            1 => {
                let (unit, changes) = changes.into_iter().next().expect("one");
                vol.unit(unit).ok_or(Error::NoUnit)?.apply(&changes)?;
                stages.iter_mut().for_each(|s| s.used = true);
                Ok(())
            }
            _ => {
                let txn = TxnId::generate(&mut **vol.ids.lock());
                vol.in_flight.lock().insert(txn);
                let r = commit_across(vol, txn, changes, &mut stages);
                vol.in_flight.lock().remove(&txn);
                // 收尾（去掉标记、清掉主单元的记录）失败不影响结果：事务已经提交，收尾在之后的
                // resolve 里重做。
                if r.is_ok() {
                    let _ = vol.resolve();
                }
                r
            }
        }
    }
}

/// 跨单元两阶段提交。第一个参与单元是主单元，它的提交就是整个事务的提交点。
fn commit_across<P: Policy>(
    vol: &Volume<P>,
    txn: TxnId,
    changes: BTreeMap<UnitId, Vec<Change>>,
    stages: &mut [Stage],
) -> Result<(), Error> {
    let participants: Vec<UnitId> = changes.keys().copied().collect();
    let info = TxnInfo { id: txn, primary: participants[0], participants: participants.clone() };
    let tag = pending_tag(txn);
    let units = vol.units();

    // 每个涉及的实体都带上未完成标记：新建的直接带在 Tag 里，已有的追加一条 AddTag。
    let mut prepared_units: Vec<&Arc<dyn Unit>> = Vec::new();
    for (unit_id, mut list) in changes {
        let mut born = BTreeSet::new();
        let mut gone = BTreeSet::new();
        for c in &mut list {
            match c {
                Change::Create { id, tags, .. } | Change::Copy { id, tags, .. } => {
                    tags.push(tag.clone());
                    *tags = crate::tag::normalized(tags);
                    born.insert(*id);
                }
                Change::Delete { id } => {
                    gone.insert(*id);
                }
                _ => {}
            }
        }
        let touched: BTreeSet<EntityId> = list.iter().map(Change::entity).collect();
        for id in touched {
            if !born.contains(&id) && !gone.contains(&id) {
                list.push(Change::AddTag { id, tag: tag.clone() });
            }
        }
        let prepared = find(&units, unit_id).ok_or(Error::NoUnit).and_then(|u| Ok(u.prepare(&info, &list)?));
        if let Err(e) = prepared {
            for u in prepared_units {
                let _ = u.abort_prepared(txn);
            }
            return Err(e);
        }
        // 暂存从此归这个单元的准备记录。
        stages.iter_mut().filter(|s| s.unit() == unit_id).for_each(|s| s.used = true);
        prepared_units.push(find(&units, unit_id).expect("just prepared"));
    }

    let primary = find(&units, info.primary).ok_or(Error::NoUnit)?;
    if let Err(e) = primary.commit_prepared(txn, true) {
        abort_everywhere(&units, txn)?;
        return Err(e.into());
    }
    // 提交点已过。其余单元失败的，留着准备记录，等 resolve 再完成。
    for unit in &participants[1..] {
        if let Some(u) = find(&units, *unit) {
            let _ = u.commit_prepared(txn, false);
        }
    }
    Ok(())
}
