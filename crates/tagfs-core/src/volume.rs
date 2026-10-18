//! 卷：多个存储单元组成的整体，也是 TagFS 的逻辑层。
//!
//! - 可见性：实体身上每道边界都能越过才可见；不可见的实体在所有读接口上都和不存在一样。
//! - 查询：分发给每个在线单元，结果合并。
//! - 事务：只涉及一个单元时直接提交；涉及多个单元时做两阶段提交，仍是一个原子操作。
//!   事务没在所有参与单元上完成之前，涉及的实体带 `tagfs:txn_pending`，结果确定前只读。

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use crate::id::{EntityId, IdSource, TxnId};
use crate::policy::Policy;
use crate::query::Query;
use crate::tag::{Key, Namespace, Tag, Value, tagfs_keys};
use crate::unit::{Change, EntityRecord, Snapshot, TxnInfo, Unit, UnitError, UnitId};

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

pub struct Volume<P: Policy> {
    policy: P,
    ids: Box<dyn IdSource>,
    /// 在线单元，按加入顺序。没指定单元时新实体放进第一个。
    units: Vec<Box<dyn Unit>>,
}

fn pending_tag(txn: TxnId) -> Tag {
    Tag { ns: Namespace::tagfs(), key: Key::new(tagfs_keys::TXN_PENDING), value: Value::Str(txn.to_string()) }
}

fn is_pending(tags: &[Tag]) -> bool {
    tags.iter().any(|t| t.ns.is_tagfs() && t.key.as_str() == tagfs_keys::TXN_PENDING)
}

impl<P: Policy> Volume<P> {
    pub fn new(policy: P, ids: Box<dyn IdSource>) -> Self {
        Volume { policy, ids, units: Vec::new() }
    }

    /// 单元上线。它带着的未了结事务在这里恢复：能确定结果的照结果完成，不能确定的
    /// 给涉及的实体打上 `tagfs:txn_pending`，等主单元上线再定。
    pub fn attach(&mut self, unit: Box<dyn Unit>) -> Result<(), Error> {
        if self.position(unit.id()).is_none() {
            self.units.push(unit);
        }
        self.resolve()
    }

    /// 单元离线，它的实体随之从卷里消失。
    pub fn detach(&mut self, id: UnitId) -> Option<Box<dyn Unit>> {
        let i = self.position(id)?;
        Some(self.units.remove(i))
    }

    pub fn unit_ids(&self) -> Vec<UnitId> {
        self.units.iter().map(|u| u.id()).collect()
    }

    pub fn unit(&self, id: UnitId) -> Option<&dyn Unit> {
        self.position(id).map(|i| &*self.units[i])
    }

    fn position(&self, id: UnitId) -> Option<usize> {
        self.units.iter().position(|u| u.id() == id)
    }

    fn snapshots(&self) -> Vec<(UnitId, Box<dyn Snapshot>)> {
        self.units.iter().map(|u| (u.id(), u.snapshot())).collect()
    }

    /// 找实体所在的单元，不看可见性。
    fn locate(&self, id: &EntityId) -> Option<(UnitId, EntityRecord)> {
        self.units.iter().find_map(|u| u.snapshot().get(id).map(|r| (u.id(), r)))
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

    fn visible_record(&self, ctx: &P::Context, id: &EntityId) -> Option<(UnitId, EntityRecord)> {
        self.locate(id).filter(|(_, r)| self.visible(ctx, &r.tags))
    }

    pub fn get(&self, ctx: &P::Context, id: &EntityId) -> Result<Entity, Error> {
        let (_, r) = self.visible_record(ctx, id).ok_or(Error::NotFound)?;
        Ok(Entity { id: r.id, tags: r.tags, size: r.size })
    }

    pub fn read(&self, ctx: &P::Context, id: &EntityId, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        let (unit, _) = self.visible_record(ctx, id).ok_or(Error::NotFound)?;
        let i = self.position(unit).ok_or(Error::NotFound)?;
        Ok(self.units[i].snapshot().read(id, offset, buf)?)
    }

    /// 满足条件、且对调用者可见的实体，按 EntityId 排序。
    pub fn query(&self, ctx: &P::Context, query: &Query) -> Vec<EntityId> {
        let mut out = Vec::new();
        for (_, snap) in self.snapshots() {
            let candidates = candidates(&*snap, query).unwrap_or_else(|| snap.all());
            for id in candidates {
                if let Some(r) = snap.get(&id)
                    && query.matches(&r.tags)
                    && self.visible(ctx, &r.tags)
                {
                    out.push(id);
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    pub fn begin<'a>(&'a mut self, ctx: &'a P::Context) -> Txn<'a, P> {
        Txn { vol: self, ctx, changes: BTreeMap::new(), created: BTreeMap::new(), deleted: BTreeSet::new() }
    }

    /// 依据在线单元的状态推进所有未了结的跨单元事务。
    fn resolve(&mut self) -> Result<(), Error> {
        // 带准备记录的单元：问主单元。
        let mut work = Vec::new();
        for u in &self.units {
            for (info, changes) in u.prepared() {
                work.push((u.id(), info, changes));
            }
        }
        for (unit, info, changes) in work {
            let Some(primary) = self.position(info.primary) else {
                // 主单元不在线，结果不能确定：给这个单元里已存在的、涉及的实体打标记。
                self.mark_in_doubt(unit, info.id, &changes)?;
                continue;
            };
            let committed = self.units[primary].committed().iter().any(|t| t.id == info.id);
            let i = self.position(unit).expect("online");
            if committed {
                self.units[i].commit_prepared(info.id, false)?;
            } else if self.units[i].prepared().iter().any(|(t, _)| t.id == info.id) {
                // 主单元没有提交：要么它自己还停在准备阶段（提交点之前就断了），
                // 要么它已经放弃。两种情况都放弃这个事务。
                self.abort_everywhere(info.id)?;
            }
        }
        // 主单元记下的已提交事务：全部参与单元都完成后，去掉标记并清掉记录。
        let mut done = Vec::new();
        for u in &self.units {
            for info in u.committed() {
                let finished = info.participants.iter().all(|p| {
                    self.position(*p).is_some_and(|i| !self.units[i].prepared().iter().any(|(t, _)| t.id == info.id))
                });
                if finished {
                    done.push((u.id(), info));
                }
            }
        }
        for (primary, info) in done {
            for p in &info.participants {
                self.clear_pending(*p, info.id)?;
            }
            let i = self.position(primary).expect("online");
            self.units[i].forget(info.id)?;
        }
        Ok(())
    }

    fn abort_everywhere(&mut self, txn: TxnId) -> Result<(), Error> {
        for i in 0..self.units.len() {
            if self.units[i].prepared().iter().any(|(t, _)| t.id == txn) {
                self.units[i].abort_prepared(txn)?;
                let id = self.units[i].id();
                self.clear_pending(id, txn)?;
            }
        }
        Ok(())
    }

    fn mark_in_doubt(&mut self, unit: UnitId, txn: TxnId, changes: &[Change]) -> Result<(), Error> {
        let i = self.position(unit).expect("online");
        let snap = self.units[i].snapshot();
        let tag = pending_tag(txn);
        let marks: Vec<Change> = changes
            .iter()
            .map(Change::entity)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|id| snap.get(id).is_some_and(|r| !r.tags.contains(&tag)))
            .map(|id| Change::AddTag { id, tag: tag.clone() })
            .collect();
        if !marks.is_empty() {
            self.units[i].apply(&marks)?;
        }
        Ok(())
    }

    fn clear_pending(&mut self, unit: UnitId, txn: TxnId) -> Result<(), Error> {
        let Some(i) = self.position(unit) else { return Ok(()) };
        let tag = pending_tag(txn);
        let snap = self.units[i].snapshot();
        let probe = crate::query::Probe::Is(&tag);
        let ids = snap.probe(&probe).unwrap_or_else(|| {
            snap.all().into_iter().filter(|id| snap.get(id).is_some_and(|r| r.tags.contains(&tag))).collect()
        });
        let clears: Vec<Change> = ids.into_iter().map(|id| Change::RemoveTag { id, tag: tag.clone() }).collect();
        if !clears.is_empty() {
            self.units[i].apply(&clears)?;
        }
        Ok(())
    }
}

/// 用索引算候选集；算不出时返回 `None`（改为扫描）。候选集只是上界，最后还要逐个核对。
fn candidates(snap: &dyn Snapshot, query: &Query) -> Option<Vec<EntityId>> {
    match query {
        Query::And(qs) => {
            let mut sets = qs.iter().filter_map(|q| candidates(snap, q));
            let first: BTreeSet<EntityId> = sets.next()?.into_iter().collect();
            Some(sets.fold(first, |acc, s| s.into_iter().filter(|id| acc.contains(id)).collect()).into_iter().collect())
        }
        Query::Or(qs) => {
            let mut all = BTreeSet::new();
            for q in qs {
                all.extend(candidates(snap, q)?);
            }
            Some(all.into_iter().collect())
        }
        Query::All | Query::Not(_) => None,
        leaf => snap.probe(&leaf.probe().expect("leaf")),
    }
}

/// 一组修改。提交前不影响卷；不提交就放弃。
pub struct Txn<'a, P: Policy> {
    vol: &'a mut Volume<P>,
    ctx: &'a P::Context,
    changes: BTreeMap<UnitId, Vec<Change>>,
    /// 本事务新建的实体：所在单元和建时的 Tag。
    created: BTreeMap<EntityId, (UnitId, Vec<Tag>)>,
    deleted: BTreeSet<EntityId>,
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
        let (unit, r) = self.vol.visible_record(self.ctx, id).ok_or(Error::NotFound)?;
        if is_pending(&r.tags) {
            return Err(Error::Pending);
        }
        Ok((unit, r.tags))
    }

    fn check_tag_writable(&self, tag: &Tag) -> Result<(), Error> {
        if !tag.within_limits() {
            return Err(Error::TagTooLong);
        }
        if tag.ns.is_tagfs() || !self.vol.policy.can_write(self.ctx, &tag.ns) {
            return Err(Error::NotWritable);
        }
        Ok(())
    }

    fn placement(&self, unit: Option<UnitId>) -> Result<UnitId, Error> {
        match unit {
            Some(u) => self.vol.position(u).map(|_| u).ok_or(Error::NoUnit),
            None => self.vol.units.first().map(|u| u.id()).ok_or(Error::NoUnit),
        }
    }

    /// 新实体的 Tag：调用者给的（须可写）加上操作系统在创建时附加的。
    fn creation_tags(&self, tags: Vec<Tag>) -> Result<Vec<Tag>, Error> {
        for t in &tags {
            self.check_tag_writable(t)?;
        }
        let mut extra = self.vol.policy.on_create(self.ctx).ok_or(Error::NotWritable)?;
        if extra.iter().any(|t| t.ns.is_tagfs()) {
            return Err(Error::NotWritable);
        }
        if !extra.iter().all(Tag::within_limits) {
            return Err(Error::TagTooLong);
        }
        extra.extend(tags);
        Ok(crate::mem_unit::normalized(&extra))
    }

    fn push(&mut self, unit: UnitId, change: Change) {
        self.changes.entry(unit).or_default().push(change);
    }

    /// 新建实体。`unit` 为空时由卷决定放在哪个单元。
    pub fn create(&mut self, unit: Option<UnitId>, tags: Vec<Tag>, content: Vec<u8>) -> Result<EntityId, Error> {
        let unit = self.placement(unit)?;
        let tags = self.creation_tags(tags)?;
        let id = EntityId::generate(&mut *self.vol.ids);
        self.created.insert(id, (unit, tags.clone()));
        self.push(unit, Change::Create { id, tags, content });
        Ok(id)
    }

    /// 复制：新建实体，带 `tags`，内容和 `from` 相同。同一单元里两者共享对象，直到任一方修改。
    pub fn copy(&mut self, from: &EntityId, unit: Option<UnitId>, tags: Vec<Tag>) -> Result<EntityId, Error> {
        let (src_unit, _) = self.target_readable(from)?;
        let unit = match unit {
            Some(_) => self.placement(unit)?,
            None => src_unit,
        };
        let tags = self.creation_tags(tags)?;
        let id = EntityId::generate(&mut *self.vol.ids);
        if unit == src_unit {
            self.push(unit, Change::Copy { from: *from, id, tags: tags.clone() });
        } else {
            if self.created.contains_key(from) {
                return Err(Error::Uncommitted);
            }
            let content = self.read_all(src_unit, from)?;
            self.push(unit, Change::Create { id, tags: tags.clone(), content });
        }
        self.created.insert(id, (unit, tags));
        Ok(id)
    }

    /// 可读即可（复制源不要求可写，也不受未完成事务的只读限制）。
    fn target_readable(&self, id: &EntityId) -> Result<(UnitId, Vec<Tag>), Error> {
        if self.deleted.contains(id) {
            return Err(Error::NotFound);
        }
        if let Some(created) = self.created.get(id) {
            return Ok(created.clone());
        }
        let (unit, r) = self.vol.visible_record(self.ctx, id).ok_or(Error::NotFound)?;
        Ok((unit, r.tags))
    }

    fn read_all(&self, unit: UnitId, id: &EntityId) -> Result<Vec<u8>, Error> {
        let i = self.vol.position(unit).ok_or(Error::NoUnit)?;
        let snap = self.vol.units[i].snapshot();
        let size = snap.get(id).ok_or(Error::NotFound)?.size;
        let mut buf = vec![0u8; size as usize];
        let n = snap.read(id, 0, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
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
        let (unit, _) = self.target(id)?;
        self.check_tag_writable(&tag)?;
        if let Some((_, tags)) = self.created.get_mut(id) {
            tags.push(tag.clone());
        }
        self.push(unit, Change::AddTag { id: *id, tag });
        Ok(())
    }

    pub fn remove_tag(&mut self, id: &EntityId, tag: Tag) -> Result<(), Error> {
        let (unit, _) = self.target(id)?;
        self.check_tag_writable(&tag)?;
        if let Some((_, tags)) = self.created.get_mut(id) {
            tags.retain(|t| t != &tag);
        }
        self.push(unit, Change::RemoveTag { id: *id, tag });
        Ok(())
    }

    pub fn write(&mut self, id: &EntityId, offset: u64, data: Vec<u8>) -> Result<(), Error> {
        let (unit, tags) = self.target(id)?;
        if !self.vol.policy.can_modify(self.ctx, &tags) {
            return Err(Error::NotWritable);
        }
        self.push(unit, Change::Write { id: *id, offset, data });
        Ok(())
    }

    pub fn truncate(&mut self, id: &EntityId, len: u64) -> Result<(), Error> {
        let (unit, tags) = self.target(id)?;
        if !self.vol.policy.can_modify(self.ctx, &tags) {
            return Err(Error::NotWritable);
        }
        self.push(unit, Change::Truncate { id: *id, len });
        Ok(())
    }

    /// 提交。要么全部生效，要么全部不生效。
    pub fn commit(self) -> Result<(), Error> {
        let Txn { vol, changes, .. } = self;
        match changes.len() {
            0 => Ok(()),
            1 => {
                let (unit, changes) = changes.into_iter().next().expect("one");
                let i = vol.position(unit).ok_or(Error::NoUnit)?;
                Ok(vol.units[i].apply(&changes)?)
            }
            _ => commit_across(vol, changes),
        }
    }
}

/// 跨单元两阶段提交。第一个参与单元是主单元，它的提交就是整个事务的提交点。
fn commit_across<P: Policy>(vol: &mut Volume<P>, changes: BTreeMap<UnitId, Vec<Change>>) -> Result<(), Error> {
    let txn = TxnId::generate(&mut *vol.ids);
    let participants: Vec<UnitId> = changes.keys().copied().collect();
    let info = TxnInfo { id: txn, primary: participants[0], participants: participants.clone() };
    let tag = pending_tag(txn);

    // 每个涉及的实体都带上未完成标记：新建的直接带在 Tag 里，已有的追加一条 AddTag。
    let mut prepared_units = Vec::new();
    for (unit, mut list) in changes {
        let mut born = BTreeSet::new();
        let mut gone = BTreeSet::new();
        for c in &mut list {
            match c {
                Change::Create { id, tags, .. } | Change::Copy { id, tags, .. } => {
                    tags.push(tag.clone());
                    *tags = crate::mem_unit::normalized(tags);
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
        let i = vol.position(unit).ok_or(Error::NoUnit)?;
        if let Err(e) = vol.units[i].prepare(&info, &list) {
            for u in prepared_units {
                if let Some(j) = vol.position(u) {
                    let _ = vol.units[j].abort_prepared(txn);
                }
            }
            return Err(e.into());
        }
        prepared_units.push(unit);
    }

    let primary = vol.position(info.primary).ok_or(Error::NoUnit)?;
    if let Err(e) = vol.units[primary].commit_prepared(txn, true) {
        vol.abort_everywhere(txn)?;
        return Err(e.into());
    }
    // 提交点已过。其余单元失败的，留着准备记录，等 resolve 再完成。
    for unit in &participants[1..] {
        if let Some(i) = vol.position(*unit) {
            let _ = vol.units[i].commit_prepared(txn, false);
        }
    }
    // 收尾（去掉标记、清掉主单元的记录）失败不影响结果：事务已经提交，收尾在之后的
    // resolve 里重做。
    let _ = vol.resolve();
    Ok(())
}

#[cfg(test)]
mod tests;
