//! 在一棵树上读：快照和进行中的提交共用。

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::ops::Bound;

use tagfs_core::{
    Change, EntityId, EntityRecord, Key, Namespace, Probe, StageId, Tag, TxnId, TxnInfo, UnitError, Value,
};
use tagfs_device::BlockDevice;

use crate::codec::{self, Reader, VALUE_STR, put_escaped_open, put_ns_key, put_tag, put_u64, put_value};
use crate::keys;
use crate::tree::Tree;

/// 对象记录。内容不超过 [`INLINE_MAX`] 时直接放在记录里，否则在区段里。
#[derive(Clone, Debug)]
pub(crate) struct Object {
    pub(crate) refs: u64,
    pub(crate) size: u64,
    pub(crate) inline: Option<Vec<u8>>,
}

pub(crate) const INLINE_MAX: usize = 512;

impl Object {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut v = Vec::new();
        put_u64(&mut v, self.refs);
        put_u64(&mut v, self.size);
        match &self.inline {
            Some(data) => {
                v.push(0);
                v.extend_from_slice(data);
            }
            None => v.push(1),
        }
        v
    }

    fn decode(b: &[u8]) -> Result<Self, UnitError> {
        let mut r = Reader::new(b);
        let refs = r.u64()?;
        let size = r.u64()?;
        let inline = match r.u8()? {
            0 => Some(r.rest().to_vec()),
            1 => None,
            _ => return Err(UnitError::Corrupt),
        };
        if inline.as_ref().is_some_and(|d| d.len() as u64 != size) {
            return Err(UnitError::Corrupt);
        }
        Ok(Object { refs, size, inline })
    }
}

/// 对象里的一段：逻辑块号、物理块号、块数。
pub(crate) type Extent = (u64, u64, u64);

pub(crate) fn decode_extent(o: u64, k: &[u8], v: &[u8]) -> Result<Extent, UnitError> {
    let mut kr = Reader::new(&k[1..]);
    if kr.be_u64()? != o {
        return Err(UnitError::Corrupt);
    }
    let logical = kr.be_u64()?;
    let mut vr = Reader::new(v);
    Ok((logical, vr.u64()?, vr.u64()?))
}

pub(crate) struct View<'a, D: BlockDevice + ?Sized> {
    pub(crate) tree: &'a Tree,
    pub(crate) dev: &'a D,
    pub(crate) bs: u64,
}

impl<D: BlockDevice + ?Sized> View<'_, D> {
    pub(crate) fn entity_object(&self, id: &EntityId) -> Result<Option<u64>, UnitError> {
        match self.tree.get(self.dev, &keys::entity(id))? {
            Some(v) => Ok(Some(Reader::new(&v).u64()?)),
            None => Ok(None),
        }
    }

    pub(crate) fn object(&self, o: u64) -> Result<Object, UnitError> {
        Object::decode(&self.tree.get(self.dev, &keys::object(o))?.ok_or(UnitError::Corrupt)?)
    }

    pub(crate) fn tags(&self, id: &EntityId) -> Result<Vec<Tag>, UnitError> {
        let prefix = keys::entity_tags(id);
        let mut tags = Vec::new();
        self.tree.scan_prefix(self.dev, &prefix, &mut |k, _| {
            tags.push(Reader::new(&k[prefix.len()..]).tag()?);
            Ok(true)
        })?;
        Ok(tags)
    }

    pub(crate) fn record(&self, id: &EntityId) -> Result<Option<EntityRecord>, UnitError> {
        let Some(o) = self.entity_object(id)? else { return Ok(None) };
        Ok(Some(EntityRecord { id: *id, tags: self.tags(id)?, size: self.object(o)?.size }))
    }

    pub(crate) fn all(&self) -> Result<Vec<EntityId>, UnitError> {
        let mut out = Vec::new();
        self.tree.scan_prefix(self.dev, &[keys::ENTITY], &mut |k, _| {
            out.push(EntityId(Reader::new(&k[1..]).id16()?));
            Ok(true)
        })?;
        Ok(out)
    }

    /// 和逻辑块 `[b0, b1)` 有重叠的区段，按逻辑块号排序。
    pub(crate) fn extents_in(&self, o: u64, b0: u64, b1: u64) -> Result<Vec<Extent>, UnitError> {
        let mut out = Vec::new();
        let prefix = keys::extents(o);
        if let Some((k, v)) = self.tree.floor(self.dev, &keys::extent(o, b0))?
            && k.starts_with(&prefix)
        {
            let e = decode_extent(o, &k, &v)?;
            if e.0 < b0 && e.0 + e.2 > b0 {
                out.push(e);
            }
        }
        self.tree.scan(self.dev, &keys::extent(o, b0), &mut |k, v| {
            if !k.starts_with(&prefix) {
                return Ok(false);
            }
            let e = decode_extent(o, k, v)?;
            if e.0 >= b1 {
                return Ok(false);
            }
            out.push(e);
            Ok(true)
        })?;
        Ok(out)
    }

    /// 读对象内容。没有区段的块（空洞）读出来是零。
    pub(crate) fn read_object(&self, o: u64, offset: u64, buf: &mut [u8]) -> Result<usize, UnitError> {
        let rec = self.object(o)?;
        if offset >= rec.size {
            return Ok(0);
        }
        let n = (buf.len() as u64).min(rec.size - offset) as usize;
        let buf = &mut buf[..n];
        if let Some(data) = &rec.inline {
            buf.copy_from_slice(&data[offset as usize..offset as usize + n]);
            return Ok(n);
        }
        buf.fill(0);
        let end = offset + n as u64;
        for (l, p, len) in self.extents_in(o, offset / self.bs, end.div_ceil(self.bs))? {
            let lo = (l * self.bs).max(offset);
            let hi = ((l + len) * self.bs).min(end);
            let phys = p * self.bs + (lo - l * self.bs);
            self.dev
                .read_at(phys, &mut buf[(lo - offset) as usize..(hi - offset) as usize])
                .map_err(|_| UnitError::Device)?;
        }
        Ok(n)
    }

    /// 用 Tag 索引回答叶子条件。
    pub(crate) fn probe(&self, probe: &Probe<'_>) -> Result<Vec<EntityId>, UnitError> {
        let mut out: Vec<EntityId> = self.probe_tags(probe)?.into_iter().map(|(id, _)| id).collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// 同 [`View::probe`]，连同命中的 Tag（一个实体在同一个键上有几个值就出现几次），按索引的顺序。
    /// 只要值的时候不必再逐个取实体。
    pub(crate) fn probe_tags(&self, probe: &Probe<'_>) -> Result<Vec<(EntityId, Tag)>, UnitError> {
        if let Probe::Children { ns, key, parent } = *probe {
            return self.children(ns, key, parent);
        }
        let mut base = alloc::vec![keys::INDEX];
        let (start, stop): (Vec<u8>, Option<&Bound<_>>) = match *probe {
            Probe::Children { .. } => unreachable!(),
            Probe::Is(tag) => {
                put_tag(&mut base, tag);
                (base.clone(), None)
            }
            Probe::Has { ns, key } => {
                put_ns_key(&mut base, ns, key);
                (base.clone(), None)
            }
            Probe::Prefix { ns, key, prefix } => {
                put_ns_key(&mut base, ns, key);
                base.push(VALUE_STR);
                put_escaped_open(&mut base, prefix.as_bytes());
                (base.clone(), None)
            }
            Probe::Range { ns, key, lower, upper } => {
                put_ns_key(&mut base, ns, key);
                let mut start = base.clone();
                if let Bound::Included(v) | Bound::Excluded(v) = lower {
                    put_value(&mut start, v);
                }
                (start, Some(upper))
            }
        };
        let mut out = Vec::new();
        self.tree.scan(self.dev, &start, &mut |k, _| {
            if !k.starts_with(&base) || k.len() < 1 + 16 {
                return Ok(false);
            }
            let tag = Reader::new(&k[1..k.len() - 16]).tag()?;
            match stop {
                Some(Bound::Included(b)) if &tag.value > b => return Ok(false),
                Some(Bound::Excluded(b)) if &tag.value >= b => return Ok(false),
                _ => {}
            }
            if probe.matches(&tag) {
                out.push((EntityId(k[k.len() - 16..].try_into().unwrap()), tag));
            }
            Ok(true)
        })?;
        Ok(out)
    }

    /// `parent` 下一层的值。索引按值排序，`<parent><名字>/` 开头的值（更深的层级）都排在
    /// `<parent><名字>0` 之前（`0` 紧跟在 `/` 后面）：扫到更深一层时从那里接着扫，跳过整棵子树。
    /// 代价只和直接子项的个数有关。
    fn children(&self, ns: &Namespace, key: &Key, parent: &str) -> Result<Vec<(EntityId, Tag)>, UnitError> {
        let mut base = alloc::vec![keys::INDEX];
        put_ns_key(&mut base, ns, key);
        base.push(VALUE_STR);
        put_escaped_open(&mut base, parent.as_bytes());
        let mut out = Vec::new();
        let mut from = base.clone();
        loop {
            let mut skip_to = None;
            self.tree.scan(self.dev, &from, &mut |k, _| {
                if !k.starts_with(&base) || k.len() < 1 + 16 {
                    return Ok(false);
                }
                let tag = Reader::new(&k[1..k.len() - 16]).tag()?;
                let Value::Str(s) = &tag.value else { return Ok(false) };
                let Some(rest) = s.strip_prefix(parent) else { return Ok(false) };
                match rest.find('/') {
                    None => {
                        if !rest.is_empty() {
                            out.push((EntityId(k[k.len() - 16..].try_into().unwrap()), tag.clone()));
                        }
                        Ok(true)
                    }
                    Some(i) => {
                        let mut next = base.clone();
                        put_escaped_open(&mut next, &rest.as_bytes()[..i]);
                        next.push(b'/' + 1);
                        skip_to = Some(next);
                        Ok(false)
                    }
                }
            })?;
            match skip_to {
                Some(next) => from = next,
                None => break,
            }
        }
        Ok(out)
    }

    /// 全部准备记录和各自的属主。
    pub(crate) fn prepared(&self) -> Result<Vec<(TxnInfo, Vec<Change>, u64)>, UnitError> {
        let mut raw: BTreeMap<[u8; 16], Vec<u8>> = BTreeMap::new();
        self.tree.scan_prefix(self.dev, &[keys::PREPARED], &mut |k, v| {
            raw.entry(Reader::new(&k[1..]).id16()?).or_default().extend_from_slice(v);
            Ok(true)
        })?;
        raw.iter()
            .map(|(txn, b)| {
                let (info, changes) = decode_prepared(b)?;
                let owner = self.tree.get(self.dev, &keys::txn_owner(&TxnId(*txn)))?;
                Ok((info, changes, owner.map_or(0, |v| keys::decode_owner(&v))))
            })
            .collect()
    }

    pub(crate) fn prepared_one(&self, txn: &TxnId) -> Result<Option<(TxnInfo, Vec<Change>)>, UnitError> {
        let mut raw = Vec::new();
        let mut found = false;
        self.tree.scan_prefix(self.dev, &keys::prepared(txn), &mut |_, v| {
            found = true;
            raw.extend_from_slice(v);
            Ok(true)
        })?;
        if found { decode_prepared(&raw).map(Some) } else { Ok(None) }
    }

    /// 暂存对应的对象；不存在时报错。
    pub(crate) fn staged(&self, stage: StageId) -> Result<u64, UnitError> {
        match self.tree.get(self.dev, &keys::staged(stage.0))? {
            Some(_) => Ok(stage.0),
            None => Err(UnitError::UnknownStage(stage)),
        }
    }

    /// 全部暂存和各自的属主。
    pub(crate) fn stages(&self) -> Result<Vec<(StageId, u64)>, UnitError> {
        let mut out = Vec::new();
        self.tree.scan_prefix(self.dev, &[keys::STAGED], &mut |k, v| {
            out.push((StageId(Reader::new(&k[1..]).be_u64()?), keys::decode_owner(v)));
            Ok(true)
        })?;
        Ok(out)
    }

    /// 已提交事务的记录和各自的属主。
    pub(crate) fn committed(&self) -> Result<Vec<(TxnInfo, u64)>, UnitError> {
        let mut out = Vec::new();
        self.tree.scan_prefix(self.dev, &[keys::COMMITTED], &mut |_, v| {
            let mut r = Reader::new(v);
            let info = codec::txn_info(&mut r)?;
            out.push((info, keys::decode_owner(r.rest())));
            Ok(true)
        })?;
        Ok(out)
    }
}

pub(crate) fn encode_prepared(info: &TxnInfo, changes: &[Change]) -> Vec<u8> {
    let mut b = Vec::new();
    codec::put_txn_info(&mut b, info);
    codec::put_u32(&mut b, changes.len() as u32);
    for c in changes {
        codec::put_change(&mut b, c);
    }
    b
}

fn decode_prepared(b: &[u8]) -> Result<(TxnInfo, Vec<Change>), UnitError> {
    let mut r = Reader::new(b);
    let info = codec::txn_info(&mut r)?;
    let n = r.u32()? as usize;
    let mut changes = Vec::new();
    for _ in 0..n {
        changes.push(codec::change(&mut r)?);
    }
    if !r.is_empty() {
        return Err(UnitError::Corrupt);
    }
    Ok((info, changes))
}
