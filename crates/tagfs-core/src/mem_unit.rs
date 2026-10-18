//! 内存参考单元：不持久化，用来测试逻辑层，也是存储单元语义的参考实现。
//! 块格式单元的行为必须和它一致。

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::id::{EntityId, TxnId};
use crate::query::Probe;
use crate::tag::Tag;
use crate::unit::{Change, EntityRecord, Snapshot, TxnInfo, Unit, UnitError, UnitId};

#[derive(Clone, Default)]
struct State {
    /// 实体 → (Tag，对象编号)
    entities: BTreeMap<EntityId, (Vec<Tag>, u64)>,
    /// 对象编号 → (内容, 引用它的实体数)
    objects: BTreeMap<u64, (Vec<u8>, u64)>,
    next_object: u64,
    prepared: BTreeMap<TxnId, (TxnInfo, Vec<Change>)>,
    committed: BTreeMap<TxnId, TxnInfo>,
}

pub struct MemUnit {
    id: UnitId,
    state: Arc<State>,
}

impl MemUnit {
    pub fn new(id: UnitId) -> Self {
        MemUnit { id, state: Arc::new(State::default()) }
    }

    /// 当前对象个数（测试回收用）。
    pub fn object_count(&self) -> usize {
        self.state.objects.len()
    }
}

impl State {
    fn new_object(&mut self, content: Vec<u8>) -> u64 {
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
    fn own_object(&mut self, id: EntityId) -> u64 {
        let object = self.entities[&id].1;
        let (content, refs) = &self.objects[&object];
        if *refs == 1 {
            return object;
        }
        let copy = content.clone();
        self.release(object);
        let fresh = self.new_object(copy);
        self.entities.get_mut(&id).unwrap().1 = fresh;
        fresh
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
                let object = self.new_object(content.clone());
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
            Change::Write { id, offset, data } => {
                exists(self, id)?;
                let object = self.own_object(*id);
                let content = &mut self.objects.get_mut(&object).unwrap().0;
                let end = *offset as usize + data.len();
                if content.len() < end {
                    content.resize(end, 0);
                }
                content[*offset as usize..end].copy_from_slice(data);
            }
            Change::Truncate { id, len } => {
                exists(self, id)?;
                let object = self.own_object(*id);
                self.objects.get_mut(&object).unwrap().0.resize(*len as usize, 0);
            }
        }
        Ok(())
    }

    fn apply_all(&self, changes: &[Change]) -> Result<State, UnitError> {
        let mut next = self.clone();
        for c in changes {
            next.apply(c)?;
        }
        Ok(next)
    }
}

/// 排序去重后的 Tag。
pub fn normalized(tags: &[Tag]) -> Vec<Tag> {
    let mut v = tags.to_vec();
    v.sort();
    v.dedup();
    v
}

impl Unit for MemUnit {
    fn id(&self) -> UnitId {
        self.id
    }

    fn snapshot(&self) -> Box<dyn Snapshot> {
        Box::new(MemSnapshot(self.state.clone()))
    }

    fn apply(&mut self, changes: &[Change]) -> Result<(), UnitError> {
        self.state = Arc::new(self.state.apply_all(changes)?);
        Ok(())
    }

    fn prepare(&mut self, txn: &TxnInfo, changes: &[Change]) -> Result<(), UnitError> {
        let mut next = (*self.state).clone();
        next.prepared.insert(txn.id, (txn.clone(), changes.to_vec()));
        self.state = Arc::new(next);
        Ok(())
    }

    fn prepared(&self) -> Vec<(TxnInfo, Vec<Change>)> {
        self.state.prepared.values().cloned().collect()
    }

    fn commit_prepared(&mut self, txn: TxnId, as_primary: bool) -> Result<(), UnitError> {
        let (info, changes) = self.state.prepared.get(&txn).cloned().ok_or(UnitError::UnknownTxn(txn))?;
        let mut next = self.state.apply_all(&changes)?;
        next.prepared.remove(&txn);
        if as_primary {
            next.committed.insert(txn, info);
        }
        self.state = Arc::new(next);
        Ok(())
    }

    fn abort_prepared(&mut self, txn: TxnId) -> Result<(), UnitError> {
        let mut next = (*self.state).clone();
        next.prepared.remove(&txn).ok_or(UnitError::UnknownTxn(txn))?;
        self.state = Arc::new(next);
        Ok(())
    }

    fn committed(&self) -> Vec<TxnInfo> {
        self.state.committed.values().cloned().collect()
    }

    fn forget(&mut self, txn: TxnId) -> Result<(), UnitError> {
        let mut next = (*self.state).clone();
        next.committed.remove(&txn);
        self.state = Arc::new(next);
        Ok(())
    }
}

struct MemSnapshot(Arc<State>);

impl Snapshot for MemSnapshot {
    fn get(&self, id: &EntityId) -> Option<EntityRecord> {
        let (tags, object) = self.0.entities.get(id)?;
        Some(EntityRecord { id: *id, tags: tags.clone(), size: self.0.objects[object].0.len() as u64 })
    }

    fn probe(&self, probe: &Probe<'_>) -> Option<Vec<EntityId>> {
        // 参考实现没有索引，直接按条件逐个比对；结果和有索引的实现必须一致。
        Some(
            self.0
                .entities
                .iter()
                .filter(|(_, (tags, _))| tags.iter().any(|t| probe.matches(t)))
                .map(|(id, _)| *id)
                .collect(),
        )
    }

    fn all(&self) -> Vec<EntityId> {
        self.0.entities.keys().copied().collect()
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
