//! 块区段的集合：空闲空间、本次提交新分配的块、本次提交释放的块都用它表示。

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// 互不重叠、互不相邻的区段：起始块 → 块数。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ExtentSet(BTreeMap<u64, u64>);

impl ExtentSet {
    pub(crate) fn new() -> Self {
        ExtentSet(BTreeMap::new())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// 区段个数。
    pub(crate) fn count(&self) -> usize {
        self.0.len()
    }

    /// 块总数。
    pub(crate) fn blocks(&self) -> u64 {
        self.0.values().sum()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.0.iter().map(|(&s, &l)| (s, l))
    }

    /// 加入一段，和相邻的合并。调用者保证它和已有的不重叠。
    pub(crate) fn insert(&mut self, start: u64, len: u64) {
        if len == 0 {
            return;
        }
        let (mut start, mut len) = (start, len);
        if let Some((&s, &l)) = self.0.range(..start).next_back() {
            debug_assert!(s + l <= start, "overlapping extents");
            if s + l == start {
                self.0.remove(&s);
                start = s;
                len += l;
            }
        }
        if let Some(&l) = self.0.get(&(start + len)) {
            self.0.remove(&(start + len));
            len += l;
        }
        debug_assert!(self.0.range(start..start + len).next().is_none(), "overlapping extents");
        self.0.insert(start, len);
    }

    pub(crate) fn extend(&mut self, other: &ExtentSet) {
        for (s, l) in other.iter() {
            self.insert(s, l);
        }
    }

    /// 是否和 `[start, start+len)` 有重叠。
    pub(crate) fn overlaps(&self, start: u64, len: u64) -> bool {
        let end = start + len;
        if let Some((&s, &l)) = self.0.range(..start).next_back()
            && s + l > start
        {
            return true;
        }
        self.0.range(start..end).next().is_some()
    }

    /// 取走和 `[start, start+len)` 重叠的部分，按顺序返回。
    pub(crate) fn remove_overlap(&mut self, start: u64, len: u64) -> Vec<(u64, u64)> {
        let end = start + len;
        let mut hits = Vec::new();
        let first = self.0.range(..start).next_back().map(|(&s, _)| s).unwrap_or(start);
        let keys: Vec<u64> = self.0.range(first..end).map(|(&s, _)| s).collect();
        for s in keys {
            let l = self.0[&s];
            let (lo, hi) = (s.max(start), (s + l).min(end));
            if lo >= hi {
                continue;
            }
            self.0.remove(&s);
            if s < lo {
                self.0.insert(s, lo - s);
            }
            if hi < s + l {
                self.0.insert(hi, s + l - hi);
            }
            hits.push((lo, hi - lo));
        }
        hits
    }

    /// 分配至多 `want` 个连续块：先找够长的第一段，没有就取最长的一段。
    /// 返回 `(起始块, 块数)`，集合为空时返回 `None`。
    pub(crate) fn take(&mut self, want: u64) -> Option<(u64, u64)> {
        let pick = self
            .0
            .iter()
            .find(|&(_, &l)| l >= want)
            .or_else(|| self.0.iter().max_by_key(|&(_, &l)| l))
            .map(|(&s, &l)| (s, l))?;
        let (s, l) = pick;
        let n = l.min(want);
        self.0.remove(&s);
        if n < l {
            self.0.insert(s + n, l - n);
        }
        Some((s, n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn merges_and_splits() {
        let mut s = ExtentSet::new();
        s.insert(10, 5);
        s.insert(20, 5);
        s.insert(15, 5);
        assert_eq!(s.iter().collect::<Vec<_>>(), vec![(10, 15)]);
        assert_eq!(s.remove_overlap(12, 3), vec![(12, 3)]);
        assert_eq!(s.iter().collect::<Vec<_>>(), vec![(10, 2), (15, 10)]);
        assert_eq!(s.remove_overlap(0, 100), vec![(10, 2), (15, 10)]);
        assert!(s.is_empty());
    }

    #[test]
    fn take_prefers_a_run_that_fits() {
        let mut s = ExtentSet::new();
        s.insert(0, 2);
        s.insert(10, 8);
        assert_eq!(s.take(4), Some((10, 4)));
        assert_eq!(s.take(9), Some((14, 4)));
        assert_eq!(s.take(9), Some((0, 2)));
        assert_eq!(s.take(1), None);
    }
}
