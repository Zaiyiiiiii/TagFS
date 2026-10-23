//! 多个进程同时用一个单元。这里的“进程”是同一个测试里的几个单元实例：各有各的设备句柄，共用一块内存设备，
//! 用模拟的锁协调（和宿主上的文件锁一样：进程退出时它持有的锁都放掉）。“进程退出”就是放掉它的单元。

use std::sync::atomic::AtomicBool;
use std::sync::{Condvar, Mutex as StdMutex, MutexGuard as StdGuard};
use std::time::Duration;

use tagfs_device::Share;

use super::*;

struct Locks {
    writer: Option<u64>,
    /// 各进程持有读者锁的次数。
    readers: BTreeMap<u64, usize>,
    alive: alloc::collections::BTreeSet<u64>,
    next: u64,
}

struct World {
    mem: MemDevice,
    locks: StdMutex<Locks>,
    changed: Condvar,
    /// 还能写几次，用完就“断电”：之后的写入和刷盘都失败。
    budget: AtomicU64,
}

impl World {
    fn new(blocks: u64) -> Arc<World> {
        Self::with(MemDevice::growable(blocks * BS as u64, 256 << 20, BS))
    }

    fn with(mem: MemDevice) -> Arc<World> {
        Arc::new(World {
            mem,
            locks: StdMutex::new(Locks { writer: None, readers: BTreeMap::new(), alive: Default::default(), next: 1 }),
            changed: Condvar::new(),
            budget: AtomicU64::new(u64::MAX),
        })
    }

    fn locks(&self) -> StdGuard<'_, Locks> {
        self.locks.lock().unwrap()
    }

    /// 一个新进程打开设备。
    fn join(self: &Arc<Self>) -> Proc {
        let mut l = self.locks();
        let id = l.next;
        l.next += 1;
        l.alive.insert(id);
        Proc { world: self.clone(), id }
    }
}

/// 一个进程手里的设备，同时也是它的锁。
struct Proc {
    world: Arc<World>,
    id: u64,
}

impl Drop for Proc {
    fn drop(&mut self) {
        let mut l = self.world.locks();
        l.alive.remove(&self.id);
        if l.writer == Some(self.id) {
            l.writer = None;
        }
        l.readers.remove(&self.id);
        self.world.changed.notify_all();
    }
}

impl Share for Proc {
    fn lock_writer(&self) -> Result<(), DeviceError> {
        let mut l = self.world.locks();
        while l.writer.is_some() {
            l = self.world.changed.wait(l).unwrap();
        }
        l.writer = Some(self.id);
        Ok(())
    }

    fn unlock_writer(&self) {
        let mut l = self.world.locks();
        assert_eq!(l.writer, Some(self.id), "unlocking a writer lock it does not hold");
        l.writer = None;
        self.world.changed.notify_all();
    }

    fn hold_readers(&self) -> Result<(), DeviceError> {
        *self.world.locks().readers.entry(self.id).or_default() += 1;
        Ok(())
    }

    fn release_readers(&self) {
        let mut l = self.world.locks();
        let n = l.readers.get_mut(&self.id).expect("held");
        *n -= 1;
        if *n == 0 {
            l.readers.remove(&self.id);
        }
    }

    fn readers_absent(&self) -> Result<bool, DeviceError> {
        Ok(self.world.locks().readers.keys().all(|&p| p == self.id))
    }

    fn owner(&self) -> u64 {
        self.id
    }

    fn alive(&self, owner: u64) -> Result<bool, DeviceError> {
        Ok(self.world.locks().alive.contains(&owner))
    }
}

impl BlockDevice for Proc {
    fn block_size(&self) -> u32 {
        self.world.mem.block_size()
    }

    fn capacity(&self) -> u64 {
        self.world.mem.capacity()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), DeviceError> {
        self.world.mem.read_at(offset, buf)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), DeviceError> {
        self.world
            .budget
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |b| b.checked_sub(1))
            .map_err(|_| DeviceError::Io)?;
        self.world.mem.write_at(offset, buf)
    }

    fn flush(&self) -> Result<(), DeviceError> {
        if self.world.budget.load(Ordering::SeqCst) == 0 {
            return Err(DeviceError::Io);
        }
        self.world.mem.flush()
    }

    fn trim(&self, offset: u64, len: u64) -> Result<(), DeviceError> {
        self.world.mem.trim(offset, len)
    }

    fn grow(&self, capacity: u64) -> Result<(), DeviceError> {
        self.world.mem.grow(capacity)
    }

    fn share(&self) -> Option<&dyn Share> {
        Some(self)
    }
}

type Shared = BlockUnit<Proc>;

fn first(world: &Arc<World>) -> Shared {
    BlockUnit::format(world.join(), UNIT).unwrap()
}

fn another(world: &Arc<World>) -> Shared {
    BlockUnit::open(world.join()).unwrap()
}

fn holds_writer(world: &World, unit: &Shared) -> bool {
    world.locks().writer == Some(unit.device().id)
}

#[test]
fn processes_see_each_others_changes() {
    let w = World::new(1024);
    let a = first(&w);
    let b = another(&w);
    create(&a, eid(1), vec![Tag::flag("user", "a")], b"hello").unwrap();
    assert!(!holds_writer(&w, &a), "a metadata change is committed and the writer lock released");
    assert_eq!(dump(&*b.snapshot())[&eid(1)], (vec![Tag::flag("user", "a")], b"hello".to_vec()));
    create(&b, eid(2), vec![], b"world").unwrap();
    b.apply(&[Change::AddTag { id: eid(1), tag: Tag::flag("user", "b") }]).unwrap();
    let seen = dump(&*a.snapshot());
    assert_eq!(seen[&eid(2)].1, b"world");
    assert_eq!(seen[&eid(1)].0, vec![Tag::flag("user", "a"), Tag::flag("user", "b")]);
    assert_eq!(dump(&*a.snapshot()), dump(&*b.snapshot()));
    a.verify().unwrap();
    b.verify().unwrap();
}

#[test]
fn content_writes_hold_the_writer_until_a_tick() {
    let w = World::new(1024);
    let a = first(&w);
    let b = another(&w);
    create(&a, eid(1), vec![], b"hello").unwrap();
    a.write(&eid(1), 0, b"HELLO").unwrap();
    assert!(holds_writer(&w, &a), "uncommitted content keeps the writer lock");
    // 没提交的写入别的进程看不到。
    assert_eq!(dump(&*b.snapshot())[&eid(1)].1, b"hello");

    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            b.apply(&[Change::AddTag { id: eid(1), tag: Tag::flag("user", "b") }]).unwrap();
            done.store(true, Ordering::SeqCst);
        });
        std::thread::sleep(Duration::from_millis(50));
        assert!(!done.load(Ordering::SeqCst), "the other process waits for the writer lock");
        // 第一次 tick 看到刚有过写入，再等一次；第二次提交并放锁。
        a.tick().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert!(!done.load(Ordering::SeqCst));
        a.tick().unwrap();
    });
    assert!(done.load(Ordering::SeqCst));
    let seen = dump(&*a.snapshot());
    assert_eq!(seen[&eid(1)], (vec![Tag::flag("user", "b")], b"HELLO".to_vec()));
    assert_eq!(seen, dump(&*b.snapshot()));
    b.verify().unwrap();
}

#[test]
fn a_reader_in_another_process_keeps_its_blocks() {
    let w = World::new(2048);
    let a = first(&w);
    let b = another(&w);
    let id = eid(1);
    let old: Vec<u8> = (0..40_000u32).map(|i| i as u8).collect();
    create(&a, id, vec![Tag::flag("user", "old")], &old).unwrap();
    let snap = b.snapshot();
    for i in 0..30u8 {
        a.write(&id, 0, &[i; 40_000]).unwrap();
        a.sync().unwrap();
        a.apply(&[Change::RemoveTag { id, tag: Tag::flag("user", "old") }]).unwrap();
        create(&a, eid(100 + i as u64), vec![], &[i; 9000]).unwrap();
        a.apply(&[Change::Delete { id: eid(100 + i as u64) }]).unwrap();
    }
    // 别的进程还在读的块没有被重新分配。
    assert_eq!(dump(&*snap)[&id], (vec![Tag::flag("user", "old")], old));
    assert!(a.pinned_blocks() > 30 * 10, "blocks the other process may be reading stay out of the free space");
    a.verify().unwrap();

    // 读完之后空间又能重用：同样的改动一轮一轮做下去，单元不再变大。
    drop(snap);
    let round = |n: u64| {
        for i in 0..30u8 {
            a.write(&id, 0, &[i; 40_000]).unwrap();
            a.sync().unwrap();
            create(&a, eid(n + i as u64), vec![], &[i; 9000]).unwrap();
            a.apply(&[Change::Delete { id: eid(n + i as u64) }]).unwrap();
        }
    };
    round(200);
    let settled = a.total_blocks();
    round(300);
    round(400);
    assert_eq!(a.total_blocks(), settled);
    a.verify().unwrap();
    assert_eq!(dump(&*b.snapshot())[&id].1, vec![29; 40_000]);
}

#[test]
fn stages_belong_to_their_process() {
    let w = World::new(1024);
    let a = first(&w);
    let s = a.stage().unwrap();
    a.write_staged(s, 0, &[1; 50_000]).unwrap();
    a.sync().unwrap();
    // 别的进程打开单元时不动还活着的进程的暂存。
    let b = another(&w);
    assert_eq!(b.stages().unwrap(), vec![s]);
    a.write_staged(s, 0, b"x").unwrap();
    a.sync().unwrap();
    // 进程不在了，下一个打开的进程替它收拾。
    drop(a);
    let c = another(&w);
    assert!(c.stages().unwrap().is_empty());
    c.verify().unwrap();
    b.verify().unwrap();
}

#[test]
fn another_process_leaves_a_running_transaction_alone() {
    let w = World::new(1024);
    let a = first(&w);
    let b = another(&w);
    create(&a, eid(1), vec![], b"a").unwrap();
    let info = TxnInfo { id: TxnId([3; 16]), primary: UNIT, participants: vec![UNIT, UnitId([8; 16])] };
    let changes = vec![Change::AddTag { id: eid(1), tag: Tag::new("tagfs", "txn_pending", "t") }];
    a.prepare(&info, &changes).unwrap();
    a.commit_prepared(info.id, true).unwrap();
    a.prepare(&info, &changes[..0]).unwrap();
    assert_eq!(a.prepared().unwrap(), vec![(info.clone(), vec![])]);
    assert_eq!(a.committed().unwrap(), vec![info.clone()]);
    // 另一个进程看不到：事务由它的进程自己了结。
    assert!(b.prepared().unwrap().is_empty());
    assert!(b.committed().unwrap().is_empty());
    // 那个进程不在了，就轮到别人。
    drop(a);
    assert_eq!(b.prepared().unwrap(), vec![(info.clone(), vec![])]);
    assert_eq!(b.committed().unwrap(), vec![info]);
}

#[test]
fn random_operations_from_several_processes() {
    let w = World::new(2048);
    let mut procs = vec![first(&w), another(&w), another(&w)];
    let reference = MemUnit::new(UNIT);
    let mut rng = Rng(0x5eed);
    let mut next_id = 0;
    // 某个进程手里一直拿着的旧快照，和它当时的内容。
    let mut held: Option<(usize, alloc::boxed::Box<dyn Snapshot>, Dump)> = None;
    for round in 0..300 {
        let p = rng.below(procs.len() as u64) as usize;
        let live = reference.snapshot().all().unwrap();
        let op = random_op(&mut rng, &live, &mut next_id);
        let expect = run(&reference, &op);
        assert_eq!(run(&procs[p], &op), expect, "round {round}: {op:?}");
        // 提交并放锁，别的进程才能改。
        procs[p].sync().unwrap();
        let want = dump(&*reference.snapshot());
        for (i, u) in procs.iter().enumerate() {
            assert_eq!(dump(&*u.snapshot()), want, "round {round}, process {i}");
        }
        match rng.below(12) {
            0 => {
                let q = rng.below(procs.len() as u64) as usize;
                let s = procs[q].snapshot();
                let d = dump(&*s);
                held = Some((q, s, d));
            }
            1 => held = None,
            // 一个进程退出，另一个打开。
            2 if held.as_ref().is_none_or(|(q, _, _)| *q != p) => {
                procs.remove(p);
                procs.push(another(&w));
            }
            _ => {}
        }
        if let Some((_, s, d)) = &held {
            assert_eq!(&dump(&**s), d, "round {round}: a held snapshot changed under its reader");
        }
        if round % 50 == 0 {
            for u in &procs {
                u.verify().unwrap();
            }
        }
    }
    drop(held);
    for u in &procs {
        u.verify().unwrap();
    }
}

/// 别的进程拿着旧快照（释放的块进盘上的第二张空闲表）时，在一次改动及其提交的任意一次写入处断电，
/// 未刷盘的写入落下任意前缀：重新打开后要么是改动前、要么是改动后的状态，空间记账也对。
#[test]
fn power_loss_while_another_process_reads() {
    let mut rng = Rng(11);
    let reference = MemUnit::new(UNIT);
    let mut next_id = 0;
    let w = World::new(512);
    let a = first(&w);
    for _ in 0..10 {
        let live = reference.snapshot().all().unwrap();
        let op = random_op(&mut rng, &live, &mut next_id);
        if run(&reference, &op).is_ok() {
            run(&a, &op).unwrap();
        }
    }
    a.sync().unwrap();
    for round in 0..12 {
        let before = dump(&*reference.snapshot());
        let live = reference.snapshot().all().unwrap();
        let op = random_op(&mut rng, &live, &mut next_id);
        if run(&reference, &op).is_err() {
            continue;
        }
        let after = dump(&*reference.snapshot());
        let image = w.mem.crash(usize::MAX);
        for budget in 0.. {
            let replay = World::with(image.crash(usize::MAX));
            let reader = another(&replay);
            let snap = reader.snapshot();
            let u = another(&replay);
            replay.budget.store(budget, Ordering::SeqCst);
            let done = run(&u, &op).and_then(|()| u.sync()).is_ok();
            let pending = replay.mem.pending_writes();
            for keep in 0..=pending {
                let back = another(&World::with(replay.mem.crash(keep)));
                back.verify().unwrap();
                let state = dump(&*back.snapshot());
                assert!(state == before || state == after, "round {round} budget {budget} keep {keep}: {op:?}");
                if done {
                    assert!(state == after, "a finished sync must survive");
                }
            }
            drop((snap, reader, u));
            if done {
                break;
            }
        }
        run(&a, &op).unwrap();
        a.sync().unwrap();
    }
}

#[test]
fn concurrent_processes_with_background_commits() {
    let w = World::new(4096);
    let procs = [first(&w), another(&w)];
    for (n, u) in procs.iter().enumerate() {
        create(u, eid(n as u64), vec![], b"").unwrap();
    }
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        // 每个进程一个后台提交线程，和镜像文件后端一样。
        for u in &procs {
            let stop = &stop;
            s.spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    u.tick().unwrap();
                    std::thread::sleep(Duration::from_millis(2));
                }
            });
        }
        let workers: Vec<_> = procs
            .iter()
            .enumerate()
            .map(|(n, u)| {
                s.spawn(move || {
                    let other = eid(1 - n as u64);
                    for i in 0..60u64 {
                        u.write(&eid(n as u64), i * 5_000, &vec![n as u8 + 1; 5_000]).unwrap();
                        if i % 7 == 0 {
                            u.apply(&[Change::AddTag { id: eid(n as u64), tag: Tag::new("user", "i", i) }]).unwrap();
                        }
                        // 读另一个进程的实体：看到的总是它某次提交时的样子。
                        let seen = dump(&*u.snapshot())[&other].1.clone();
                        assert!(seen.len().is_multiple_of(5_000) && seen.iter().all(|&b| b == 2 - n as u8));
                    }
                })
            })
            .collect();
        for h in workers {
            h.join().unwrap();
        }
        stop.store(true, Ordering::SeqCst);
    });
    for u in &procs {
        u.sync().unwrap();
    }
    for u in &procs {
        let all = dump(&*u.snapshot());
        for n in 0..2u64 {
            assert_eq!(all[&eid(n)].1, vec![n as u8 + 1; 300_000]);
        }
        u.verify().unwrap();
    }
}
