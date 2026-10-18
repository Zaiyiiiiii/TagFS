//! 写时复制的 B+ 树。一个单元的全部元数据都在一棵树里，按键的第一个字节分区（见 `keys`）。
//!
//! 已提交的节点永远不原地修改：要改一个节点，先把它读进内存（[`Link::Mem`]），原来的块记进
//! [`Tree::replaced`]，提交时再把内存里的节点自底向上写到新分配的块。
//!
//! 内存里的节点也是共享的、不可变的：一棵树的拷贝只是多拿一份根，改动时沿路径复制被别人共享着的节点，
//! 没人共享的就地改。所以读者拿着某个时刻的根读，写者同时改，互不干扰。
//!
//! 节点通常占一个块。单个条目本身就超过一块时（例如很长的 Tag），它独占一个节点，
//! 这个节点跨多个块存放；这些块不必连续，第一块里记着其余块的位置。节点的布局（多块时
//! 按块的顺序拼起来看）：
//!
//! | 偏移 | 长度 | 内容 |
//! |---|---|---|
//! | 0 | 4 | CRC-32C，覆盖 4 到节点末尾 |
//! | 4 | 1 | 种类：1 叶子，2 分支 |
//! | 5 | 1 | 保留 |
//! | 6 | 2 | 条目数 |
//! | 8 | 8 | 写入时的提交号 |
//! | 16 | 2 | 额外块数 e |
//! | 18 | 2 | 保留 |
//! | 20 | 8e | 额外块的块号 |
//! | 20+8e | … | 条目 |
//!
//! 叶子条目：键长 u16、值长 u16、键、值。分支条目：键长 u16、子节点块号 u64、键；
//! 键是子树里所有键的下界，第一个子树的键不起作用。
//!
//! 删除不做合并：节点变空就从父节点摘掉，根只剩一个孩子就降一层。

use alloc::borrow::Cow;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use tagfs_core::UnitError;
use tagfs_device::BlockDevice;

use crate::codec::{Reader, put_u16, put_u64};
use crate::crc::crc32c;

const HEADER: usize = 20;
const KIND_LEAF: u8 = 1;
const KIND_BRANCH: u8 = 2;

#[derive(Clone, Debug)]
pub(crate) enum Link {
    Disk(u64),
    Mem(Arc<Node>),
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

    fn len(&self) -> usize {
        match self {
            Node::Leaf(es) => es.len(),
            Node::Branch(cs) => cs.len(),
        }
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 各条目编码后的字节数，按顺序。
    fn entry_sizes(&self) -> Vec<usize> {
        match self {
            Node::Leaf(es) => es.iter().map(|(k, v)| 4 + k.len() + v.len()).collect(),
            Node::Branch(cs) => cs.iter().map(|(k, _)| 10 + k.len()).collect(),
        }
    }
}

/// 正文为 `body` 字节的节点要占几个块（额外块的块号也要占地方）。
fn blocks_for(body: usize, bs: usize) -> usize {
    let mut k = 1;
    while HEADER + 8 * (k - 1) + body > k * bs {
        k += 1;
    }
    k
}

pub(crate) fn read_block<D: BlockDevice + ?Sized>(dev: &D, block: u64, bs: usize) -> Result<Vec<u8>, UnitError> {
    let mut buf = vec![0u8; bs];
    dev.read_at(block * bs as u64, &mut buf).map_err(|_| UnitError::Device)?;
    Ok(buf)
}

/// 读一个节点，返回节点和它占用的全部块（第一个是它自己的块号）。
fn read_node<D: BlockDevice + ?Sized>(dev: &D, block: u64, bs: usize) -> Result<(Node, Vec<u64>), UnitError> {
    let mut buf = read_block(dev, block, bs)?;
    let extra = u16::from_le_bytes(buf[16..18].try_into().unwrap()) as usize;
    if HEADER + 8 * extra > bs {
        return Err(UnitError::Corrupt);
    }
    let mut blocks = vec![block];
    for i in 0..extra {
        let at = HEADER + 8 * i;
        blocks.push(u64::from_le_bytes(buf[at..at + 8].try_into().unwrap()));
    }
    for &b in &blocks[1..] {
        buf.extend_from_slice(&read_block(dev, b, bs)?);
    }
    Ok((decode(&buf, extra)?, blocks))
}

fn decode(buf: &[u8], extra: usize) -> Result<Node, UnitError> {
    let crc = u32::from_le_bytes(buf[..4].try_into().unwrap());
    if crc != crc32c(&buf[4..]) {
        return Err(UnitError::Corrupt);
    }
    let kind = buf[4];
    let count = u16::from_le_bytes(buf[6..8].try_into().unwrap()) as usize;
    let mut r = Reader::new(&buf[HEADER + 8 * extra..]);
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

/// 编码成 `blocks.len()` 个块那么长的字节串；`blocks` 是节点要写入的块，第一个是它自己。
fn encode(node: &Node, generation: u64, blocks: &[u64], bs: usize) -> Vec<u8> {
    let mut out = vec![0u8; 8];
    out[4] = match node {
        Node::Leaf(_) => KIND_LEAF,
        Node::Branch(_) => KIND_BRANCH,
    };
    out[6..8].copy_from_slice(&(node.len() as u16).to_le_bytes());
    put_u64(&mut out, generation);
    put_u16(&mut out, (blocks.len() - 1) as u16);
    put_u16(&mut out, 0);
    for &b in &blocks[1..] {
        put_u64(&mut out, b);
    }
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
                let &Link::Disk(child) = link else { unreachable!("children are written first") };
                put_u16(&mut out, k.len() as u16);
                put_u64(&mut out, child);
                out.extend_from_slice(k);
            }
        }
    }
    assert!(out.len() <= blocks.len() * bs, "node overflows its blocks");
    out.resize(blocks.len() * bs, 0);
    let crc = crc32c(&out[4..]);
    out[..4].copy_from_slice(&crc.to_le_bytes());
    out
}

/// 第一个使左半不小于一半的位置，限制在 `1..len` 内。
fn half(sizes: &[usize]) -> usize {
    let total: usize = sizes.iter().sum();
    let mut acc = 0;
    let at = sizes.iter().position(|s| {
        acc += s;
        acc * 2 >= total
    });
    (at.unwrap_or(0) + 1).clamp(1, sizes.len() - 1)
}

/// 大于 `left`、不大于 `right` 的最短键（`right` 的一个前缀）。分支里只存它，键很长时分支也不会跟着变大。
fn separator(left: &[u8], right: &[u8]) -> Vec<u8> {
    debug_assert!(left < right);
    let common = left.iter().zip(right).take_while(|(a, b)| a == b).count();
    right[..common + 1].to_vec()
}

/// 分支里应当进入的子树：最后一个下界不大于 `key` 的。
fn child_index(cs: &[(Vec<u8>, Link)], key: &[u8]) -> usize {
    cs.partition_point(|(k, _)| k.as_slice() <= key).saturating_sub(1)
}

#[derive(Clone)]
pub(crate) struct Tree {
    root: Option<Link>,
    /// 改动时被复制走的已提交节点。提交之后它们不再被新根引用。
    pub(crate) replaced: Vec<u64>,
    bs: usize,
}

impl Tree {
    pub(crate) fn open(root: Option<u64>, block_size: usize) -> Self {
        Tree { root: root.map(Link::Disk), replaced: Vec::new(), bs: block_size }
    }

    /// 同一时刻的树，给读者用：共享全部节点，以后两边各改各的。
    pub(crate) fn view(&self) -> Tree {
        Tree { root: self.root.clone(), replaced: Vec::new(), bs: self.bs }
    }

    fn load<D: BlockDevice + ?Sized>(&self, dev: &D, link: &Link) -> Result<Arc<Node>, UnitError> {
        match link {
            Link::Mem(n) => Ok(n.clone()),
            Link::Disk(b) => Ok(Arc::new(read_node(dev, *b, self.bs)?.0)),
        }
    }

    pub(crate) fn get<D: BlockDevice + ?Sized>(&self, dev: &D, key: &[u8]) -> Result<Option<Vec<u8>>, UnitError> {
        let Some(root) = &self.root else { return Ok(None) };
        let mut node = self.load(dev, root)?;
        loop {
            let next = match &*node {
                Node::Leaf(es) => {
                    return Ok(es.binary_search_by(|(k, _)| k.as_slice().cmp(key)).ok().map(|i| es[i].1.clone()));
                }
                Node::Branch(cs) => self.load(dev, &cs[child_index(cs, key)].1)?,
            };
            node = next;
        }
    }

    /// 从 `from` 起按顺序访问条目，`f` 返回 `false` 时停止。
    pub(crate) fn scan<D, F>(&self, dev: &D, from: &[u8], f: &mut F) -> Result<(), UnitError>
    where
        D: BlockDevice + ?Sized,
        F: FnMut(&[u8], &[u8]) -> Result<bool, UnitError>,
    {
        if let Some(root) = &self.root {
            self.scan_rec(dev, root, from, f)?;
        }
        Ok(())
    }

    fn scan_rec<D, F>(&self, dev: &D, link: &Link, from: &[u8], f: &mut F) -> Result<bool, UnitError>
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
                    if !self.scan_rec(dev, child, from, f)? {
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
        match &self.root {
            Some(root) => self.floor_rec(dev, root, key),
            None => Ok(None),
        }
    }

    fn floor_rec<D: BlockDevice + ?Sized>(&self, dev: &D, link: &Link, key: &[u8]) -> Result<Option<Entry>, UnitError> {
        match &*self.load(dev, link)? {
            Node::Leaf(es) => {
                let n = es.partition_point(|(k, _)| k.as_slice() <= key);
                Ok(n.checked_sub(1).map(|i| es[i].clone()))
            }
            Node::Branch(cs) => {
                // 下界可能因为删除而不再是子树里实际存在的键，所以落空时往左找。
                for j in (0..=child_index(cs, key)).rev() {
                    if let Some(hit) = self.floor_rec(dev, &cs[j].1, key)? {
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
        if let Some(root) = &self.root {
            self.visit_rec(dev, root, f)?;
        }
        Ok(())
    }

    fn visit_rec<D, F>(&self, dev: &D, link: &Link, f: &mut F) -> Result<(), UnitError>
    where
        D: BlockDevice + ?Sized,
        F: FnMut(u64),
    {
        let node = match link {
            Link::Mem(n) => Cow::Borrowed(&**n),
            Link::Disk(b) => {
                let (node, blocks) = read_node(dev, *b, self.bs)?;
                blocks.into_iter().for_each(&mut *f);
                Cow::Owned(node)
            }
        };
        if let Node::Branch(cs) = &*node {
            for (_, child) in cs {
                self.visit_rec(dev, child, f)?;
            }
        }
        Ok(())
    }

    pub(crate) fn insert<D: BlockDevice + ?Sized>(
        &mut self,
        dev: &D,
        key: Vec<u8>,
        value: Vec<u8>,
    ) -> Result<(), UnitError> {
        assert!(key.len() <= u16::MAX as usize && value.len() <= u16::MAX as usize, "tree entry too large");
        let bs = self.bs;
        let mut root = self.root.take().unwrap_or_else(|| Link::Mem(Arc::new(Node::Leaf(Vec::new()))));
        let split = insert_rec(dev, bs, &mut self.replaced, &mut root, key, value);
        let mut split = match split {
            Ok(s) => s,
            Err(e) => {
                self.root = Some(root);
                return Err(e);
            }
        };
        // 根分裂了就长高一层；新根本身也可能放不下，继续分。
        while !split.is_empty() {
            let mut children = vec![(Vec::new(), root)];
            children.extend(split);
            let mut node = Node::Branch(children);
            split = split_if_full(&mut node, bs);
            root = Link::Mem(Arc::new(node));
        }
        self.root = Some(root);
        Ok(())
    }

    /// 删除一个键，返回它是否存在。
    pub(crate) fn remove<D: BlockDevice + ?Sized>(&mut self, dev: &D, key: &[u8]) -> Result<bool, UnitError> {
        if self.get(dev, key)?.is_none() {
            return Ok(false);
        }
        let mut root = self.root.take().expect("non-empty");
        if let Err(e) = remove_rec(dev, self.bs, &mut self.replaced, &mut root, key) {
            self.root = Some(root);
            return Err(e);
        }
        // 根变空就成为空树；根只剩一个孩子就降一层。
        let mut root = Some(root);
        while let Some(Link::Mem(n)) = &root {
            root = match &**n {
                n if n.is_empty() => None,
                Node::Branch(cs) if cs.len() == 1 => Some(cs[0].1.clone()),
                _ => break,
            };
        }
        self.root = root;
        Ok(true)
    }

    /// 把内存里的节点写到新分配的块，返回新根（空树为 `None`）。之后树里只剩盘上节点。
    /// 失败时树不变（已经分配、写下的块由调用者收回）。
    pub(crate) fn flush<D, A>(&mut self, dev: &D, alloc: &mut A, generation: u64) -> Result<Option<u64>, UnitError>
    where
        D: BlockDevice + ?Sized,
        A: FnMut() -> Result<u64, UnitError>,
    {
        let root = match &self.root {
            None => None,
            Some(Link::Disk(b)) => Some(*b),
            Some(Link::Mem(n)) => Some(flush_rec(dev, self.bs, alloc, generation, n)?),
        };
        self.root = root.map(Link::Disk);
        Ok(root)
    }
}

/// 要改的节点：盘上的先读进内存（原来的块记进 `replaced`），被别人共享着的先复制一份。
fn node_mut<'a, D: BlockDevice + ?Sized>(
    dev: &D,
    bs: usize,
    replaced: &mut Vec<u64>,
    link: &'a mut Link,
) -> Result<&'a mut Node, UnitError> {
    if let Link::Disk(b) = *link {
        let (node, blocks) = read_node(dev, b, bs)?;
        replaced.extend(blocks);
        *link = Link::Mem(Arc::new(node));
    }
    match link {
        Link::Mem(n) => Ok(Arc::make_mut(n)),
        Link::Disk(_) => unreachable!("just loaded"),
    }
}

fn insert_rec<D: BlockDevice + ?Sized>(
    dev: &D,
    bs: usize,
    replaced: &mut Vec<u64>,
    link: &mut Link,
    key: Vec<u8>,
    value: Vec<u8>,
) -> Result<Vec<(Vec<u8>, Link)>, UnitError> {
    let node = node_mut(dev, bs, replaced, link)?;
    match node {
        Node::Leaf(es) => match es.binary_search_by(|(k, _)| k.as_slice().cmp(&key)) {
            Ok(i) => es[i].1 = value,
            Err(i) => es.insert(i, (key, value)),
        },
        Node::Branch(cs) => {
            let i = child_index(cs, &key);
            let split = insert_rec(dev, bs, replaced, &mut cs[i].1, key, value)?;
            for (j, entry) in split.into_iter().enumerate() {
                cs.insert(i + 1 + j, entry);
            }
        }
    }
    Ok(split_if_full(node, bs))
}

/// 节点超出一块时，从左到右尽量装满一块地切成几段，后面各段成为新节点。返回新节点的下界和链接。
///
/// 叶子里单独就超过一块的条目自成一段（这样的节点跨多个块存放）。分支每段至少留两个孩子，
/// 否则分隔键很长时，根每长高一层还是同样多的长键，永远分不完。
fn split_if_full(node: &mut Node, bs: usize) -> Vec<(Vec<u8>, Link)> {
    let cap = bs - HEADER;
    let min = match node {
        Node::Leaf(_) => 1,
        Node::Branch(_) => 2,
    };
    if node.body_len() <= cap || node.len() < 2 * min {
        return Vec::new();
    }
    let sizes = node.entry_sizes();
    let mut starts = Vec::new();
    if sizes.iter().all(|&s| s * 3 <= cap) {
        // 常见情况：对半分。条目都不超过容量的三分之一，两半一定都放得下，
        // 而且之后的插入不会立刻又把满的一半撑破。
        starts.push(half(&sizes));
    } else {
        // 有大条目：从左到右尽量装满一块。
        let (mut used, mut count) = (0, 0);
        for (i, &s) in sizes.iter().enumerate() {
            if count >= min && used + s > cap {
                starts.push(i);
                (used, count) = (0, 0);
            }
            used += s;
            count += 1;
        }
        if count < min {
            starts.pop();
        }
    }
    let mut pieces = Vec::new();
    for &at in starts.iter().rev() {
        let (piece, sep) = match node {
            Node::Leaf(es) => {
                let right = es.split_off(at);
                let sep = separator(&es[at - 1].0, &right[0].0);
                (Node::Leaf(right), sep)
            }
            Node::Branch(cs) => {
                let right = cs.split_off(at);
                let sep = right[0].0.clone();
                (Node::Branch(right), sep)
            }
        };
        pieces.push((sep, Link::Mem(Arc::new(piece))));
    }
    pieces.reverse();
    pieces
}

fn remove_rec<D: BlockDevice + ?Sized>(
    dev: &D,
    bs: usize,
    replaced: &mut Vec<u64>,
    link: &mut Link,
    key: &[u8],
) -> Result<(), UnitError> {
    match node_mut(dev, bs, replaced, link)? {
        Node::Leaf(es) => {
            if let Ok(i) = es.binary_search_by(|(k, _)| k.as_slice().cmp(key)) {
                es.remove(i);
            }
        }
        Node::Branch(cs) => {
            let i = child_index(cs, key);
            remove_rec(dev, bs, replaced, &mut cs[i].1, key)?;
            let empty = matches!(&cs[i].1, Link::Mem(n) if n.is_empty());
            if empty {
                cs.remove(i);
            }
        }
    }
    Ok(())
}

/// 把内存里的节点（连同它下面的内存节点）写到新分配的块，返回它的块号。节点本身不变：
/// 别的读者可能还拿着它。
fn flush_rec<D, A>(dev: &D, bs: usize, alloc: &mut A, generation: u64, node: &Node) -> Result<u64, UnitError>
where
    D: BlockDevice + ?Sized,
    A: FnMut() -> Result<u64, UnitError>,
{
    let written;
    let node = match node {
        Node::Branch(cs) => {
            let mut out = Vec::with_capacity(cs.len());
            for (k, link) in cs {
                let child = match link {
                    Link::Disk(b) => *b,
                    Link::Mem(n) => flush_rec(dev, bs, alloc, generation, n)?,
                };
                out.push((k.clone(), Link::Disk(child)));
            }
            written = Node::Branch(out);
            &written
        }
        leaf => leaf,
    };
    let mut blocks = Vec::new();
    for _ in 0..blocks_for(node.body_len(), bs) {
        blocks.push(alloc()?);
    }
    let buf = encode(node, generation, &blocks, bs);
    for (b, part) in blocks.iter().zip(buf.chunks(bs)) {
        dev.write_at(b * bs as u64, part).map_err(|_| UnitError::Device)?;
    }
    Ok(blocks[0])
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
        let dev = MemDevice::new(4096 * 4096, BS as u32);
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
            t.flush(&dev, &mut alloc, round as u64).unwrap();
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
        assert_eq!(t.flush(&dev, &mut alloc, 9).unwrap(), None);
    }

    #[test]
    fn entries_larger_than_a_block_get_their_own_nodes() {
        let dev = MemDevice::new(4096 * 4096, BS as u32);
        let mut next = 1u64;
        // 故意隔一个块分配，确认多块节点不需要连续的块。
        let mut alloc = || {
            next += 2;
            Ok(next)
        };
        let mut t = Tree::open(None, BS);
        let mut model = BTreeMap::new();
        for i in 0..400u32 {
            let mut k = key(i);
            if i % 3 == 0 {
                k.extend(core::iter::repeat_n(0u8, 8000 + (i as usize % 700)));
            }
            let v = alloc::vec![(i % 7) as u8; if i % 5 == 0 { 9000 } else { 20 }];
            t.insert(&dev, k.clone(), v.clone()).unwrap();
            model.insert(k, v);
            if i % 50 == 49 {
                t.flush(&dev, &mut alloc, i as u64).unwrap();
            }
        }
        for k in model.keys().step_by(4).cloned().collect::<Vec<_>>() {
            assert!(t.remove(&dev, &k).unwrap());
            model.remove(&k);
        }
        t.flush(&dev, &mut alloc, 999).unwrap();
        let expect: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(collect(&t, &dev), expect);
        let mut blocks = Vec::new();
        t.visit_nodes(&dev, &mut |b| blocks.push(b)).unwrap();
        let unique: alloc::collections::BTreeSet<_> = blocks.iter().collect();
        assert_eq!(unique.len(), blocks.len());
    }

    #[test]
    fn a_damaged_node_is_reported() {
        let dev = MemDevice::new(64 * 4096, BS as u32);
        let mut t = Tree::open(None, BS);
        t.insert(&dev, b"a".to_vec(), b"1".to_vec()).unwrap();
        let root = t.flush(&dev, &mut || Ok(5), 1).unwrap().unwrap();
        dev.write_at(root * BS as u64 + 100, &[1]).unwrap();
        assert_eq!(Tree::open(Some(root), BS).get(&dev, b"a"), Err(UnitError::Corrupt));
    }
}
