//! TagFS 各层共用的锁。
//!
//! 逻辑层、块格式和设备接口不依赖 `std`，将来要能跑在裸机上，那里只有自旋锁；在宿主操作系统上，
//! 等锁时应当让出线程，不能空转（锁可能跨着一次设备读写）。两种锁的接口相同（都是 `lock_api` 的），
//! 由 `std` 特性选择：任何一个依赖方打开它，整个程序就都用宿主的锁。

#![no_std]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

#[cfg(feature = "std")]
pub use parking_lot::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
#[cfg(not(feature = "std"))]
pub use spin::lock_api::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

#[cfg(feature = "std")]
type RawRwLock = parking_lot::RawRwLock;
#[cfg(not(feature = "std"))]
type RawRwLock = spin::RwLock<()>;

type Slot = Arc<lock_api::RwLock<RawRwLock, ()>>;

/// 按键加的读写锁：同一个键上读者共享、写者独占，不同的键互不相干。只为正在用的键占地方。
pub struct Keyed<K: Ord + Clone> {
    slots: Mutex<BTreeMap<K, Slot>>,
}

enum Held {
    Read(#[allow(dead_code)] lock_api::ArcRwLockReadGuard<RawRwLock, ()>),
    Write(#[allow(dead_code)] lock_api::ArcRwLockWriteGuard<RawRwLock, ()>),
}

/// 持有的锁；放掉它就解锁。
pub struct KeyedGuard<'a, K: Ord + Clone> {
    table: &'a Keyed<K>,
    held: Vec<(K, Held)>,
}

impl<K: Ord + Clone> Default for Keyed<K> {
    fn default() -> Self {
        Keyed { slots: Mutex::new(BTreeMap::new()) }
    }
}

impl<K: Ord + Clone> Keyed<K> {
    pub fn new() -> Self {
        Self::default()
    }

    fn slot(&self, key: &K) -> Slot {
        self.slots.lock().entry(key.clone()).or_default().clone()
    }

    /// 共享地锁住一个键。
    pub fn read(&self, key: &K) -> KeyedGuard<'_, K> {
        let held = Held::Read(self.slot(key).read_arc());
        KeyedGuard { table: self, held: alloc::vec![(key.clone(), held)] }
    }

    /// 独占地锁住一组键。按键的顺序加锁，所以同时锁多个键的人之间不会互相卡死。
    pub fn write_all(&self, keys: impl IntoIterator<Item = K>) -> KeyedGuard<'_, K> {
        let mut keys: Vec<K> = keys.into_iter().collect();
        keys.sort();
        keys.dedup();
        let held = keys
            .into_iter()
            .map(|k| {
                let h = Held::Write(self.slot(&k).write_arc());
                (k, h)
            })
            .collect();
        KeyedGuard { table: self, held }
    }

    /// 独占地锁住一个键。
    pub fn write(&self, key: &K) -> KeyedGuard<'_, K> {
        self.write_all([key.clone()])
    }
}

impl<K: Ord + Clone> Drop for KeyedGuard<'_, K> {
    fn drop(&mut self) {
        for (key, held) in self.held.drain(..) {
            drop(held);
            // 没人再用这个键就把它的锁收掉。拿锁的人都要先经过这张表，所以在表的锁里数得准。
            let mut slots = self.table.slots.lock();
            if slots.get(&key).is_some_and(|s| Arc::strong_count(s) == 1) {
                slots.remove(&key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_are_dropped_when_unused() {
        let t: Keyed<u32> = Keyed::new();
        {
            let _a = t.read(&1);
            let _b = t.read(&1);
            let _c = t.write_all([3, 2, 3]);
            assert_eq!(t.slots.lock().len(), 3);
        }
        assert!(t.slots.lock().is_empty());
    }
}
