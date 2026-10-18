//! 写时复制的 B+ 树。一个单元的全部元数据都在一棵树里，按键的第一个字节分区（见 `keys`）。
//!
//! 已提交的节点永远不原地修改：要改一个节点，先把它读进内存（[`Link::Mem`]），原来的块记进
//! [`Tree::replaced`]，提交时再把内存里的节点自底向上写到新分配的块。读者拿着旧的根，
//! 看到的始终是旧的那棵树。
//!
//! 节点块的布局：
//!
//! | 偏移 | 长度 | 内容 |
//! |---|---|---|
//! | 0 | 4 | CRC-32C，覆盖 4 到块尾 |
//! | 4 | 1 | 种类：1 叶子，2 分支 |
//! | 5 | 1 | 保留 |
//! | 6 | 2 | 条目数 |
//! | 8 | 8 | 写入时的提交号 |
//! | 16 | … | 条目 |
//!
//! 叶子条目：键长 u16、值长 u16、键、值。分支条目：键长 u16、子节点块号 u64、键；
//! 键是子树里所有键的下界，第一个子树的键不起作用。
//!
//! 删除不做合并：节点变空就从父节点摘掉，根只剩一个孩子就降一层。

use alloc::borrow::Cow;
use alloc::vec;
use alloc::vec::Vec;

use tagfs_core::UnitError;
use tagfs_device::BlockDevice;

use crate::codec::{Reader, put_u16, put_u64};
use crate::crc::crc32c;

const HEADER: usize = 16;
const KIND_LEAF: u8 = 1;
const KIND_BRANCH: u8 = 2;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Link {
    Disk(u64),
    Mem(usize),
}

/// 叶子里的一个条目：键和值。
pub(crate) type Entry = (Vec<u8>, Vec<u8>);

#[derive(Clone, Debug)]
pub(crate) enum Node {
    Leaf(Vec<Entry>),
    Branch(Vec<(Vec<u8>, Link)>),
}

impl Node {
    fn body_len(&self) -> usize {
        match self {
            Node::Leaf(es) => es.iter().map(|(k, v)| 4 + k.len() + v.len()).sum(),
            Node::Branch(cs) => cs.iter().map(|(k, _)| 10 + k.len()).sum(),
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            Node::Leaf(es) => es.is_empty(),
            Node::Branch(cs) => cs.is_empty(),
        }
    }
}

/// 一个条目（键加值）的最大字节数。节点分裂后两半都放得下的前提是单个条目不超过容量的三分之一。
pub(crate) fn max_entry(block_size: usize) -> usize {
    (block_size - HEADER) / 3 - 4
}

pub(crate) fn read_block<D: BlockDevice + ?Sized>(dev: &D, block: u64, bs: usize) -> Result<Vec<u8>, UnitError> {
    let mut buf = vec![0u8; bs];
    dev.read_at(block * bs as u64, &mut buf).map_err(|_| UnitError::Device)?;
    Ok(buf)
}

fn decode(buf: &[u8]) -> Result<Node, UnitError> {
    let crc = u32::from_le_bytes(buf[..4].try_into().unwrap());
    if crc != crc32c(&buf[4..]) {
        return Err(UnitError::Corrupt);
    }
    let kind = buf[4];
    let count = u16::from_le_bytes(buf[6..8].try_into().unwrap()) as usize;
    let mut r = Reader::new(&buf[HEADER..]);
    match kind {
        KIND_LEAF => {
            let mut es = Vec::with_capacity(count);
            for _ in 0..count {
                let (kl, vl) = (r.u16()? as usize, r.u16()? as usize);
                let k = r.take(kl)?.to_vec();
                es.push((k, r.take(vl)?.to_vec()));
            }
            Ok(Node::Leaf(es))
        }
        KIND_BRANCH => {
            let mut cs = Vec::with_capacity(count);
            for _ in 0..count {
                let kl = r.u16()? as usize;
                let child = r.u64()?;
                cs.push((r.take(kl)?.to_vec(), Link::Disk(child)));
            }
            Ok(Node::Branch(cs))
        }
        _ => Err(UnitError::Corrupt),
    }
}

fn encode(node: &Node, generation: u64, bs: usize) -> Vec<u8> {
    let mut out = vec![0u8; 8];
    out[4] = match node {
        Node::Leaf(_) => KIND_LEAF,
        Node::Branch(_) => KIND_BRANCH,
    };
    let count = match node {
        Node::Leaf(es) => es.len(),
        Node::Branch(cs) => cs.len(),
    };
    out[6..8].copy_from_slice(&(count as u16).to_le_bytes());
    put_u64(&mut out, generation);
    match node {
        Node::Leaf(es) => {
            for (k, v) in es {
                put_u16(&mut out, k.len() as u16);
                put_u16(&mut out, v.len() as u16);
                out.extend_from_slice(k);
                out.extend_from_slice(v);
            }
        }
        Node::Branch(cs) => {
            for (k, link) in cs {
                let Link::Disk(child) = link else { unreachable!("children are written first") };
                put_u16(&mut out, k.len() as u16);
                put_u64(&mut out, *child);
                out.extend_from_slice(k);
            }
        }
    }
    assert!(out.len() <= bs, "node overflows its block");
    out.resize(bs, 0);
    let crc = crc32c(&out[4..]);
    out[..4].copy_from_slice(&crc.to_le_bytes());
    out
}

/// 分支里应当进入的子树：最后一个下界不大于 `key` 的。
fn child_index(cs: &[(Vec<u8>, Link)], key: &[u8]) -> usize {
    cs.partition_point(|(k, _)| k.as_slice() <= key).saturating_sub(1)
}

pub(crate) struct Tree {
    root: Option<Link>,
    arena: Vec<Node>,
    /// 改动时被复制走的已提交节点。提交之后它们不再被新根引用。
    pub(crate) replaced: Vec<u64>,
    bs: usize,
}

impl Tree {
    pub(crate) fn open(root: Option<u64>, block_size: usize) -> Self {
        Tree { root: root.map(Link::Disk), arena: Vec::new(), replaced: Vec::new(), bs: block_size }
    }

    fn load<D: BlockDevice + ?Sized>(&self, dev: &D, link: Link) -> Result<Cow<'_, Node>, UnitError> {
        match link {
            Link::Mem(i) => Ok(Cow::Borrowed(&self.arena[i])),
            Link::Disk(b) => Ok(Cow::Owned(decode(&read_block(dev, b, self.bs)?)?)),
        }
    }

    pub(crate) fn get<D: BlockDevice + ?Sized>(&self, dev: &D, key: &[u8]) -> Result<Option<Vec<u8>>, UnitError> {
        let Some(mut link) = self.root else { return Ok(None) };
        loop {
            match &*self.load(dev, link)? {
                Node::Leaf(es) => {
                    return Ok(es.binary_search_by(|(k, _)| k.as_slice().cmp(key)).ok().map(|i| es[i].1.clone()));
                }
                Node::Branch(cs) => link = cs[child_index(cs, key)].1,
            }
        }
    }

    /// 从 `from` 起按顺序访问条目，`f` 返回 `false` 时停止。
    pub(crate) fn scan<D, F>(&self, dev: &D, from: &[u8], f: &mut F) -> Result<(), UnitError>
    where
        D: BlockDevice + ?Sized,
        F: FnMut(&[u8], &[u8]) -> Result<bool, UnitError>,
    {
        if let Some(root) = self.root {
            self.scan_rec(dev, root, from, f)?;
        }
        Ok(())
    }

    fn scan_rec<D, F>(&self, dev: &D, link: Link, from: &[u8], f: &mut F) -> Result<bool, UnitError>
    where
        D: BlockDevice + ?Sized,
        F: FnMut(&[u8], &[u8]) -> Result<bool, UnitError>,
    {
        match &*self.load(dev, link)? {
            Node::Leaf(es) => {
                let start = es.partition_point(|(k, _)| k.as_slice() < from);
                for (k, v) in &es[start..] {
                    if !f(k, v)? {
                        return Ok(false);
                    }
                }
            }
            Node::Branch(cs) => {
                for (_, child) in &cs[child_index(cs, from)..] {
                    if !self.scan_rec(dev, *child, from, f)? {
                        return Ok(false);
                    }
                }
            }
        }
        Ok(true)
    }

    /// 以 `prefix` 开头的全部条目。
    pub(crate) fn scan_prefix<D, F>(&self, dev: &D, prefix: &[u8], f: &mut F) -> Result<(), UnitError>
    where
        D: BlockDevice + ?Sized,
        F: FnMut(&[u8], &[u8]) -> Result<bool, UnitError>,
    {
        self.scan(dev, prefix, &mut |k, v| if k.starts_with(prefix) { f(k, v) } else { Ok(false) })
    }

    /// 不大于 `key` 的最后一个条目。
    pub(crate) fn floor<D: BlockDevice + ?Sized>(&self, dev: &D, key: &[u8]) -> Result<Option<Entry>, UnitError> {
        match self.root {
            Some(root) => self.floor_rec(dev, root, key),
            None => Ok(None),
        }
    }

    fn floor_rec<D: BlockDevice + ?Sized>(&self, dev: &D, link: Link, key: &[u8]) -> Result<Option<Entry>, UnitError> {
        match &*self.load(dev, link)? {
            Node::Leaf(es) => {
                let n = es.partition_point(|(k, _)| k.as_slice() <= key);
                Ok(n.checked_sub(1).map(|i| es[i].clone()))
            }
            Node::Branch(cs) => {
                // 下界可能因为删除而不再是子树里实际存在的键，所以落空时往左找。
                for j in (0..=child_index(cs, key)).rev() {
                    if let Some(hit) = self.floor_rec(dev, cs[j].1, key)? {
                        return Ok(Some(hit));
                    }
                }
                Ok(None)
            }
        }
    }

    /// 访问每个盘上节点的块号（检查用）。
    pub(crate) fn visit_nodes<D, F>(&self, dev: &D, f: &mut F) -> Result<(), UnitError>
    where
        D: BlockDevice + ?Sized,
        F: FnMut(u64),
    {
        if let Some(root) = self.root {
            self.visit_rec(dev, root, f)?;
        }
        Ok(())
    }

    fn visit_rec<D, F>(&self, dev: &D, link: Link, f: &mut F) -> Result<(), UnitError>
    where
        D: BlockDevice + ?Sized,
        F: FnMut(u64),
    {
        if let Link::Disk(b) = link {
            f(b);
        }
        if let Node::Branch(cs) = &*self.load(dev, link)? {
            for (_, child) in cs {
                self.visit_rec(dev, *child, f)?;
            }
        }
        Ok(())
    }

    fn make_mem<D: BlockDevice + ?Sized>(&mut self, dev: &D, link: Link) -> Result<usize, UnitError> {
        match link {
            Link::Mem(i) => Ok(i),
            Link::Disk(b) => {
                let node = decode(&read_block(dev, b, self.bs)?)?;
                self.replaced.push(b);
                self.arena.push(node);
                Ok(self.arena.len() - 1)
            }
        }
    }

    pub(crate) fn insert<D: BlockDevice + ?Sized>(
        &mut self,
        dev: &D,
        key: Vec<u8>,
        value: Vec<u8>,
    ) -> Result<(), UnitError> {
        assert!(key.len() + value.len() <= max_entry(self.bs), "tree entry too large");
        let root = match self.root {
            Some(link) => self.make_mem(dev, link)?,
            None => {
                self.arena.push(Node::Leaf(Vec::new()));
                self.arena.len() - 1
            }
        };
        self.root = Some(Link::Mem(root));
        if let Some((sep, right)) = self.insert_rec(dev, root, key, value)? {
            self.arena.push(Node::Branch(vec![(Vec::new(), Link::Mem(root)), (sep, right)]));
            self.root = Some(Link::Mem(self.arena.len() - 1));
        }
        Ok(())
    }

    fn insert_rec<D: BlockDevice + ?Sized>(
        &mut self,
        dev: &D,
        idx: usize,
        key: Vec<u8>,
        value: Vec<u8>,
    ) -> Result<Option<(Vec<u8>, Link)>, UnitError> {
        match &mut self.arena[idx] {
            Node::Leaf(es) => match es.binary_search_by(|(k, _)| k.as_slice().cmp(&key)) {
                Ok(i) => es[i].1 = value,
                Err(i) => es.insert(i, (key, value)),
            },
            Node::Branch(cs) => {
                let i = child_index(cs, &key);
                let child = cs[i].1;
                let ci = self.make_mem(dev, child)?;
                let split = self.insert_rec(dev, ci, key, value)?;
                let Node::Branch(cs) = &mut self.arena[idx] else { unreachable!() };
                cs[i].1 = Link::Mem(ci);
                if let Some(entry) = split {
                    cs.insert(i + 1, entry);
                }
            }
        }
        Ok(self.split_if_full(idx))
    }

    /// 节点超出一块时对半分开，右半成为新节点。返回右半的下界和链接。
    fn split_if_full(&mut self, idx: usize) -> Option<(Vec<u8>, Link)> {
        let node = &mut self.arena[idx];
        let total = node.body_len();
        if HEADER + total <= self.bs {
            return None;
        }
        let right = match node {
            Node::Leaf(es) => {
                let m = split_point(es.iter().map(|(k, v)| 4 + k.len() + v.len()), total);
                Node::Leaf(es.split_off(m))
            }
            Node::Branch(cs) => {
                let m = split_point(cs.iter().map(|(k, _)| 10 + k.len()), total);
                Node::Branch(cs.split_off(m))
            }
        };
        let sep = match &right {
            Node::Leaf(es) => es[0].0.clone(),
            Node::Branch(cs) => cs[0].0.clone(),
        };
        self.arena.push(right);
        Some((sep, Link::Mem(self.arena.len() - 1)))
    }

    /// 删除一个键，返回它是否存在。
    pub(crate) fn remove<D: BlockDevice + ?Sized>(&mut self, dev: &D, key: &[u8]) -> Result<bool, UnitError> {
        if self.get(dev, key)?.is_none() {
            return Ok(false);
        }
        let root = self.make_mem(dev, self.root.expect("non-empty"))?;
        self.remove_rec(dev, root, key)?;
        self.root = Some(Link::Mem(root));
        // 根变空就成为空树；根只剩一个孩子就降一层。
        while let Some(Link::Mem(i)) = self.root {
            match &self.arena[i] {
                n if n.is_empty() => self.root = None,
                Node::Branch(cs) if cs.len() == 1 => self.root = Some(cs[0].1),
                _ => break,
            }
        }
        Ok(true)
    }

    fn remove_rec<D: BlockDevice + ?Sized>(&mut self, dev: &D, idx: usize, key: &[u8]) -> Result<(), UnitError> {
        let descend = match &mut self.arena[idx] {
            Node::Leaf(es) => {
                if let Ok(i) = es.binary_search_by(|(k, _)| k.as_slice().cmp(key)) {
                    es.remove(i);
                }
                None
            }
            Node::Branch(cs) => {
                let i = child_index(cs, key);
                Some((i, cs[i].1))
            }
        };
        if let Some((i, child)) = descend {
            let ci = self.make_mem(dev, child)?;
            self.remove_rec(dev, ci, key)?;
            let empty = self.arena[ci].is_empty();
            let Node::Branch(cs) = &mut self.arena[idx] else { unreachable!() };
            if empty {
                cs.remove(i);
            } else {
                cs[i].1 = Link::Mem(ci);
            }
        }
        Ok(())
    }

    /// 把内存里的节点写到新分配的块，返回新根（空树为 `None`）。之后树里只剩盘上节点。
    pub(crate) fn flush<D, A>(&mut self, dev: &mut D, alloc: &mut A, generation: u64) -> Result<Option<u64>, UnitError>
    where
        D: BlockDevice + ?Sized,
        A: FnMut() -> Result<u64, UnitError>,
    {
        let root = match self.root {
            None => None,
            Some(Link::Disk(b)) => Some(b),
            Some(Link::Mem(i)) => Some(self.flush_rec(dev, alloc, generation, i)?),
        };
        self.root = root.map(Link::Disk);
        self.arena.clear();
        Ok(root)
    }

    fn flush_rec<D, A>(&mut self, dev: &mut D, alloc: &mut A, generation: u64, idx: usize) -> Result<u64, UnitError>
    where
        D: BlockDevice + ?Sized,
        A: FnMut() -> Result<u64, UnitError>,
    {
        let mut node = core::mem::replace(&mut self.arena[idx], Node::Leaf(Vec::new()));
        if let Node::Branch(cs) = &mut node {
            for (_, link) in cs.iter_mut() {
                if let Link::Mem(ci) = *link {
                    *link = Link::Disk(self.flush_rec(dev, alloc, generation, ci)?);
                }
            }
        }
        let block = alloc()?;
        let buf = encode(&node, generation, self.bs);
        dev.write_at(block * self.bs as u64, &buf).map_err(|_| UnitError::Device)?;
        Ok(block)
    }
}

/// 第一个使左半不小于一半的位置，限制在 `1..len` 内。
fn split_point(sizes: impl Iterator<Item = usize>, total: usize) -> usize {
    let sizes: Vec<usize> = sizes.collect();
    let mut acc = 0;
    let mut m = sizes.len() - 1;
    for (i, s) in sizes.iter().enumerate() {
        acc += s;
        if acc * 2 >= total {
            m = i + 1;
            break;
        }
    }
    m.clamp(1, sizes.len() - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;
    use tagfs_device::MemDevice;

    const BS: usize = 4096;

    fn key(i: u32) -> Vec<u8> {
        // 打散插入顺序，键长也不一样。
        let mut k = (i.wrapping_mul(2_654_435_761)).to_be_bytes().to_vec();
        k.extend(core::iter::repeat_n(b'k', (i % 50) as usize));
        k
    }

    fn collect(t: &Tree, dev: &MemDevice) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut out = Vec::new();
        t.scan(dev, &[], &mut |k, v| {
            out.push((k.to_vec(), v.to_vec()));
            Ok(true)
        })
        .unwrap();
        out
    }

    #[test]
    fn matches_a_btreemap_through_flushes() {
        let mut dev = MemDevice::new(4096 * 4096, BS as u32);
        let mut next = 1u64;
        let mut alloc = || {
            next += 1;
            Ok(next)
        };
        let mut t = Tree::open(None, BS);
        let mut model = BTreeMap::new();
        for round in 0..4u32 {
            for i in 0..3000u32 {
                let k = key(i * 7 + round);
                let v = alloc::vec![(i % 251) as u8; (i % 300) as usize];
                t.insert(&dev, k.clone(), v.clone()).unwrap();
                model.insert(k, v);
            }
            for i in (0..3000u32).step_by(3) {
                let k = key(i * 7 + round);
                assert_eq!(t.remove(&dev, &k).unwrap(), model.remove(&k).is_some());
            }
            t.flush(&mut dev, &mut alloc, round as u64).unwrap();
            let expect: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            assert_eq!(collect(&t, &dev), expect);
        }
        for (k, v) in &model {
            assert_eq!(t.get(&dev, k).unwrap().as_ref(), Some(v));
            assert_eq!(t.floor(&dev, k).unwrap(), Some((k.clone(), v.clone())));
        }
        let mut probe = model.keys().nth(10).unwrap().clone();
        probe.push(0xff);
        assert_eq!(&t.floor(&dev, &probe).unwrap().unwrap().0, model.keys().nth(10).unwrap());
        for k in model.keys().cloned().collect::<Vec<_>>() {
            assert!(t.remove(&dev, &k).unwrap());
        }
        assert!(collect(&t, &dev).is_empty());
        assert_eq!(t.flush(&mut dev, &mut alloc, 9).unwrap(), None);
    }

    #[test]
    fn a_damaged_node_is_reported() {
        let mut dev = MemDevice::new(64 * 4096, BS as u32);
        let mut t = Tree::open(None, BS);
        t.insert(&dev, b"a".to_vec(), b"1".to_vec()).unwrap();
        let root = t.flush(&mut dev, &mut || Ok(5), 1).unwrap().unwrap();
        dev.write_at(root * BS as u64 + 100, &[1]).unwrap();
        assert_eq!(Tree::open(Some(root), BS).get(&dev, b"a"), Err(UnitError::Corrupt));
    }
}
