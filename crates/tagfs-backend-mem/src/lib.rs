//! 内存后端：不持久化的存储单元。
//!
//! 它也是存储单元语义的参考实现：别的后端的行为（修改的效果、出错的条件、复制后的共享）都应当和它一致，
//! 各后端的测试拿它做对照。

#![no_std]

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use tagfs_core::tag::normalized;
use tagfs_core::{
    Change, EntityId, EntityRecord, Probe, Snapshot, StageId, Tag, TxnId, TxnInfo, Unit, UnitError, UnitId,
};
use tagfs_sync::RwLock;

#[derive(Clone, Default)]
struct State {
    /// 实体 → (Tag，对象编号)
    entities: BTreeMap<EntityId, (Vec<Tag>, u64)>,
    /// 对象编号 → (内容, 引用它的实体数)。内容和快照共享，改的时候才复制。
    objects: BTreeMap<u64, (Arc<Vec<u8>>, u64)>,
    next_object: u64,
    stages: BTreeMap<StageId, Arc<Vec<u8>>>,
    next_stage: u64,
    prepared: BTreeMap<TxnId, (TxnInfo, Vec<Change>)>,
    committed: BTreeMap<TxnId, TxnInfo>,
}

pub struct MemUnit {
    id: UnitId,
    /// 当前状态。快照拿走一份 `Arc`，修改时状态被快照共享着就先复制（只复制表，内容按需复制）。
    state: RwLock<Arc<State>>,
    mapped: &'static [&'static str],
}

impl MemUnit {
    pub fn new(id: UnitId) -> Self {
        MemUnit { id, state: RwLock::new(Arc::new(State::default())), mapped: &[] }
    }

    /// 声明映射的 `tagfs` 键（见 [`Unit::mapped_keys`]）。内存单元里它们只是普通的 Tag，
    /// 用来测试卷对映射键的处理。
    pub fn mapping(mut self, keys: &'static [&'static str]) -> Self {
        self.mapped = keys;
        self
    }

    /// 当前对象个数（测试回收用）。
    pub fn object_count(&self) -> usize {
        self.state.read().objects.len()
    }

    /// 当前暂存个数（测试回收用）。
    pub fn stage_count(&self) -> usize {
        self.state.read().stages.len()
    }

    /// 在当前状态上改。`f` 出错时什么都不变。
    fn update<T>(&self, f: impl FnOnce(&mut State) -> Result<T, UnitError>) -> Result<T, UnitError> {
        let mut current = self.state.write();
        let mut next = (**current).clone();
        let out = f(&mut next)?;
        *current = Arc::new(next);
        Ok(out)
    }
}

fn write_into(content: &mut Arc<Vec<u8>>, offset: u64, data: &[u8]) {
    let content = Arc::make_mut(content);
    let end = offset as usize + data.len();
    if content.len() < end {
        content.resize(end, 0);
    }
    content[offset as usize..end].copy_from_slice(data);
}

impl State {
    fn new_object(&mut self, content: Arc<Vec<u8>>) -> u64 {
        let n = self.next_object;
        self.next_object += 1;
        self.objects.insert(n, (content, 1));
        n
    }

    fn release(&mut self, object: u64) {
        let (_, refs) = self.objects.get_mut(&object).expect("object of a live entity");
        *refs -= 1;
        if *refs == 0 {
            self.objects.remove(&object);
        }
    }

    /// 实体要改内容：对象被共享时先复制一份给它。返回它独占的对象编号。
    fn own_object(&mut self, id: &EntityId) -> Result<u64, UnitError> {
        let object = self.entities.get(id).ok_or(UnitError::NotFound(*id))?.1;
        let (content, refs) = &self.objects[&object];
        if *refs == 1 {
            return Ok(object);
        }
        let copy = Arc::new((**content).clone());
        self.release(object);
        let fresh = self.new_object(copy);
        self.entities.get_mut(id).unwrap().1 = fresh;
        Ok(fresh)
    }

    fn apply(&mut self, change: &Change) -> Result<(), UnitError> {
        let exists = |s: &State, id: &EntityId| {
            if s.entities.contains_key(id) { Ok(()) } else { Err(UnitError::NotFound(*id)) }
        };
        match change {
            Change::Create { id, tags, content } => {
                if self.entities.contains_key(id) {
                    return Err(UnitError::Exists(*id));
                }
                let content = match content {
                    Some(s) => self.stages.remove(s).ok_or(UnitError::UnknownStage(*s))?,
                    None => Arc::default(),
                };
                let object = self.new_object(content);
                self.entities.insert(*id, (normalized(tags), object));
            }
            Change::Copy { from, id, tags } => {
                exists(self, from)?;
                if self.entities.contains_key(id) {
                    return Err(UnitError::Exists(*id));
                }
                let object = self.entities[from].1;
                self.objects.get_mut(&object).unwrap().1 += 1;
                self.entities.insert(*id, (normalized(tags), object));
            }
            Change::Delete { id } => {
                exists(self, id)?;
                let (_, object) = self.entities.remove(id).unwrap();
                self.release(object);
            }
            Change::AddTag { id, tag } => {
                exists(self, id)?;
                let tags = &mut self.entities.get_mut(id).unwrap().0;
                if let Err(pos) = tags.binary_search(tag) {
                    tags.insert(pos, tag.clone());
                }
            }
            Change::RemoveTag { id, tag } => {
                exists(self, id)?;
                self.entities.get_mut(id).unwrap().0.retain(|t| t != tag);
            }
        }
        Ok(())
    }
}

impl Unit for MemUnit {
    fn id(&self) -> UnitId {
        self.id
    }

    fn mapped_keys(&self) -> &[&'static str] {
        self.mapped
    }

    fn snapshot(&self) -> Box<dyn Snapshot> {
        Box::new(MemSnapshot(self.state.read().clone()))
    }

    fn write(&self, id: &EntityId, offset: u64, data: &[u8]) -> Result<(), UnitError> {
        self.update(|s| {
            let object = s.own_object(id)?;
            write_into(&mut s.objects.get_mut(&object).unwrap().0, offset, data);
            Ok(())
        })
    }

    fn set_len(&self, id: &EntityId, len: u64) -> Result<(), UnitError> {
        self.update(|s| {
            let object = s.own_object(id)?;
            Arc::make_mut(&mut s.objects.get_mut(&object).unwrap().0).resize(len as usize, 0);
            Ok(())
        })
    }

    fn sync(&self) -> Result<(), UnitError> {
        Ok(())
    }

    fn stage(&self) -> Result<StageId, UnitError> {
        self.update(|s| {
            let id = StageId(s.next_stage);
            s.next_stage += 1;
            s.stages.insert(id, Arc::default());
            Ok(id)
        })
    }

    fn write_staged(&self, stage: StageId, offset: u64, data: &[u8]) -> Result<(), UnitError> {
        self.update(|s| {
            write_into(s.stages.get_mut(&stage).ok_or(UnitError::UnknownStage(stage))?, offset, data);
            Ok(())
        })
    }

    fn discard_staged(&self, stage: StageId) -> Result<(), UnitError> {
        self.update(|s| s.stages.remove(&stage).map(|_| ()).ok_or(UnitError::UnknownStage(stage)))
    }

    fn apply(&self, changes: &[Change]) -> Result<(), UnitError> {
        self.update(|s| changes.iter().try_for_each(|c| s.apply(c)))
    }

    fn prepare(&self, txn: &TxnInfo, changes: &[Change]) -> Result<(), UnitError> {
        self.update(|s| {
            for c in changes {
                if let Change::Create { content: Some(stage), .. } = c
                    && !s.stages.contains_key(stage)
                {
                    return Err(UnitError::UnknownStage(*stage));
                }
            }
            s.prepared.insert(txn.id, (txn.clone(), changes.to_vec()));
            Ok(())
        })
    }

    fn prepared(&self) -> Result<Vec<(TxnInfo, Vec<Change>)>, UnitError> {
        Ok(self.state.read().prepared.values().cloned().collect())
    }

    fn commit_prepared(&self, txn: TxnId, as_primary: bool) -> Result<(), UnitError> {
        self.update(|s| {
            let (info, changes) = s.prepared.remove(&txn).ok_or(UnitError::UnknownTxn(txn))?;
            changes.iter().try_for_each(|c| s.apply(c))?;
            if as_primary {
                s.committed.insert(txn, info);
            }
            Ok(())
        })
    }

    fn abort_prepared(&self, txn: TxnId) -> Result<(), UnitError> {
        self.update(|s| {
            let (_, changes) = s.prepared.remove(&txn).ok_or(UnitError::UnknownTxn(txn))?;
            for c in changes {
                if let Change::Create { content: Some(stage), .. } = c {
                    s.stages.remove(&stage);
                }
            }
            Ok(())
        })
    }

    fn committed(&self) -> Result<Vec<TxnInfo>, UnitError> {
        Ok(self.state.read().committed.values().cloned().collect())
    }

    fn forget(&self, txn: TxnId) -> Result<(), UnitError> {
        self.update(|s| {
            s.committed.remove(&txn);
            Ok(())
        })
    }

    fn describe(&self) -> Vec<(&'static str, alloc::string::String)> {
        use alloc::string::ToString;
        let s = self.state.read();
        alloc::vec![
            ("entities", s.entities.len().to_string()),
            ("objects", s.objects.len().to_string()),
            ("stages", s.stages.len().to_string()),
        ]
    }

    /// 引用数和引用它的实体数一致，没有悬空的对象。
    fn check(&self) -> Result<(), UnitError> {
        let s = self.state.read().clone();
        let mut refs: BTreeMap<u64, u64> = BTreeMap::new();
        for (_, object) in s.entities.values() {
            *refs.entry(*object).or_default() += 1;
        }
        let consistent = refs.len() == s.objects.len() && s.objects.iter().all(|(o, (_, n))| refs.get(o) == Some(n));
        if !consistent {
            return Err(UnitError::Corrupt);
        }
        Ok(())
    }
}

struct MemSnapshot(Arc<State>);

impl Snapshot for MemSnapshot {
    fn get(&self, id: &EntityId) -> Result<Option<EntityRecord>, UnitError> {
        Ok(self.0.entities.get(id).map(|(tags, object)| EntityRecord {
            id: *id,
            tags: tags.clone(),
            size: self.0.objects[object].0.len() as u64,
        }))
    }

    fn probe(&self, probe: &Probe<'_>) -> Result<Option<Vec<EntityId>>, UnitError> {
        // 参考实现没有索引，直接按条件逐个比对；结果和有索引的实现必须一致。
        Ok(Some(
            self.0
                .entities
                .iter()
                .filter(|(_, (tags, _))| tags.iter().any(|t| probe.matches(t)))
                .map(|(id, _)| *id)
                .collect(),
        ))
    }

    fn all(&self) -> Result<Vec<EntityId>, UnitError> {
        Ok(self.0.entities.keys().copied().collect())
    }

    fn read(&self, id: &EntityId, offset: u64, buf: &mut [u8]) -> Result<usize, UnitError> {
        let (_, object) = self.0.entities.get(id).ok_or(UnitError::NotFound(*id))?;
        let content = &self.0.objects[object].0;
        let start = (offset as usize).min(content.len());
        let n = buf.len().min(content.len() - start);
        buf[..n].copy_from_slice(&content[start..start + n]);
        Ok(n)
    }
}
