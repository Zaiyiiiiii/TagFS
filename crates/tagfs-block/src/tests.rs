use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;
use core::ops::Bound;

use tagfs_backend_mem::MemUnit;
use tagfs_core::{
    Change, EntityId, Key, Namespace, Probe, Snapshot, Tag, TxnId, TxnInfo, Unit, UnitError, UnitId, Value,
};
use tagfs_device::{BlockDevice, DeviceError, MemDevice};

use crate::{BlockUnit, OpenError};

const BS: u32 = 4096;
const UNIT: UnitId = UnitId([7; 16]);

fn mem_device(blocks: u64) -> MemDevice {
    MemDevice::new(blocks * BS as u64, BS)
}

fn fresh(blocks: u64) -> BlockUnit<MemDevice> {
    BlockUnit::format(mem_device(blocks), UNIT).unwrap()
}

/// 一个单元的全部内容：实体 → (Tag, 内容)。
type Dump = BTreeMap<EntityId, (Vec<Tag>, Vec<u8>)>;

fn dump(s: &dyn Snapshot) -> Dump {
    let mut out = BTreeMap::new();
    for id in s.all().unwrap() {
        let r = s.get(&id).unwrap().unwrap();
        let mut content = vec![0u8; r.size as usize];
        assert_eq!(s.read(&id, 0, &mut content).unwrap(), r.size as usize);
        out.insert(id, (r.tags, content));
    }
    out
}

/// 确定性的伪随机数，测试可重放。
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

fn eid(n: u64) -> EntityId {
    let mut b = [0u8; 16];
    b[8..].copy_from_slice(&n.to_be_bytes());
    EntityId(b)
}

fn random_tag(rng: &mut Rng) -> Tag {
    let ns = ["user", "app", "system"][rng.below(3) as usize];
    let key = ["name", "rating", "flag", "zero\0key"][rng.below(4) as usize];
    let value = match rng.below(3) {
        0 => Value::None,
        1 => Value::Uint(rng.below(20)),
        _ => Value::Str(["a", "ab", "abc", "b", "", "a\0b"][rng.below(6) as usize].to_string()),
    };
    Tag { ns: Namespace::new(ns), key: Key::new(key), value }
}

fn random_len(rng: &mut Rng) -> usize {
    match rng.below(4) {
        0 => rng.below(64) as usize,
        1 => rng.below(600) as usize,
        2 => rng.below(3 * BS as u64) as usize,
        _ => rng.below(20 * BS as u64) as usize,
    }
}

/// 一组修改，引用的实体多半存在，偶尔不存在（应当整组失败）。
fn random_changes(rng: &mut Rng, live: &[EntityId], next_id: &mut u64) -> Vec<Change> {
    let mut out = Vec::new();
    let mut live = live.to_vec();
    for _ in 0..1 + rng.below(4) {
        let pick = |rng: &mut Rng, live: &[EntityId]| {
            if live.is_empty() || rng.below(30) == 0 {
                eid(999_999)
            } else {
                live[rng.below(live.len() as u64) as usize]
            }
        };
        let c = match rng.below(10) {
            0 | 1 => {
                *next_id += 1;
                let id = eid(*next_id);
                live.push(id);
                let n = random_len(rng);
                Change::Create { id, tags: (0..rng.below(4)).map(|_| random_tag(rng)).collect(), content: rng.bytes(n) }
            }
            2 => {
                *next_id += 1;
                let id = eid(*next_id);
                let from = pick(rng, &live);
                live.push(id);
                Change::Copy { from, id, tags: vec![random_tag(rng)] }
            }
            3 => {
                let id = pick(rng, &live);
                live.retain(|x| *x != id);
                Change::Delete { id }
            }
            4 | 5 => Change::AddTag { id: pick(rng, &live), tag: random_tag(rng) },
            6 => Change::RemoveTag { id: pick(rng, &live), tag: random_tag(rng) },
            7 | 8 => {
                let n = random_len(rng);
                Change::Write { id: pick(rng, &live), offset: rng.below(12 * BS as u64), data: rng.bytes(n) }
            }
            _ => Change::Truncate { id: pick(rng, &live), len: random_len(rng) as u64 },
        };
        out.push(c);
    }
    out
}

/// 一组索引查询的参数：同一个键上的等值、范围、前缀。
struct ProbeCase {
    ns: Namespace,
    key: Key,
    tag: Tag,
    lower: Bound<Value>,
    upper: Bound<Value>,
    prefix: &'static str,
}

fn probes() -> Vec<ProbeCase> {
    let mut v = Vec::new();
    for ns in ["user", "app"] {
        for key in ["name", "rating", "zero\0key"] {
            let case = |tag, lower, upper, prefix| ProbeCase {
                ns: Namespace::new(ns),
                key: Key::new(key),
                tag,
                lower,
                upper,
                prefix,
            };
            v.push(case(
                Tag::new(ns, key, 3u64),
                Bound::Included(Value::Uint(2)),
                Bound::Excluded(Value::Uint(9)),
                "a",
            ));
            v.push(case(Tag::new(ns, key, "ab"), Bound::Excluded(Value::str("a")), Bound::Unbounded, ""));
            v.push(case(Tag::flag(ns, key), Bound::Unbounded, Bound::Included(Value::Uint(5)), "a\0"));
        }
    }
    v
}

/// 两边的索引查询结果一致。
fn same_probes(a: &dyn Snapshot, b: &dyn Snapshot) {
    for c in probes() {
        let all = [
            Probe::Has { ns: &c.ns, key: &c.key },
            Probe::Is(&c.tag),
            Probe::Range { ns: &c.ns, key: &c.key, lower: &c.lower, upper: &c.upper },
            Probe::Prefix { ns: &c.ns, key: &c.key, prefix: c.prefix },
        ];
        for p in &all {
            assert_eq!(a.probe(p).unwrap(), b.probe(p).unwrap(), "{p:?}");
        }
    }
}

#[test]
fn format_and_reopen_an_empty_unit() {
    let unit = fresh(64);
    unit.verify().unwrap();
    assert_eq!(unit.free_blocks() + 2 + 1, unit.total_blocks(), "only the free list block is in use");
    let dev = unit.device().crash(usize::MAX);
    let again = BlockUnit::open(dev).unwrap();
    assert_eq!(again.id(), UNIT);
    assert!(again.snapshot().all().unwrap().is_empty());
    again.verify().unwrap();
}

#[test]
fn rejects_what_it_cannot_use() {
    assert_eq!(BlockUnit::open(mem_device(64)).err(), Some(OpenError::NotFormatted));
    assert_eq!(BlockUnit::format(MemDevice::new(64 * 512, 512), UNIT).err(), Some(OpenError::BadBlockSize));
    assert_eq!(BlockUnit::format(mem_device(4), UNIT).err(), Some(OpenError::TooSmall));
}

#[test]
fn behaves_like_the_reference_unit() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut block = fresh(2048);
    let mut reference = MemUnit::new(UNIT);
    let mut next_id = 0;
    for round in 0..300 {
        let live = reference.snapshot().all().unwrap();
        let changes = random_changes(&mut rng, &live, &mut next_id);
        let expect = reference.apply(&changes);
        let got = block.apply(&changes);
        assert_eq!(got, expect, "round {round}: {changes:?}");
        let (a, b) = (block.snapshot(), reference.snapshot());
        assert_eq!(dump(&*a), dump(&*b), "round {round}");
        if round % 25 == 0 {
            same_probes(&*a, &*b);
            block.verify().unwrap();
        }
    }
    block.verify().unwrap();
    // 重新挂载后内容不变。
    let reopened = BlockUnit::open(block.device().crash(usize::MAX)).unwrap();
    reopened.verify().unwrap();
    assert_eq!(dump(&*reopened.snapshot()), dump(&*reference.snapshot()));
    same_probes(&*reopened.snapshot(), &*reference.snapshot());
}

#[test]
fn tags_up_to_the_length_limit_work() {
    let mut block = fresh(2048);
    let mut reference = MemUnit::new(UNIT);
    // 最坏情况：全是 NUL，转义后翻倍。
    let longest = |c: char| Tag::new("user", "path", c.to_string().repeat(tagfs_core::tag::MAX_STR_LEN));
    let mut changes = Vec::new();
    for i in 0..60u64 {
        let c = ['\0', 'a', 'b'][i as usize % 3];
        let mut tag = longest(c);
        if let Value::Str(s) = &mut tag.value {
            s.truncate(s.len() - i as usize);
        }
        changes.push(Change::Create { id: eid(i), tags: vec![tag, Tag::new("user", "n", i)], content: vec![] });
    }
    for unit in [&mut block as &mut dyn Unit, &mut reference] {
        unit.apply(&changes).unwrap();
        unit.apply(&[Change::Delete { id: eid(7) }, Change::RemoveTag { id: eid(8), tag: longest('\0') }]).unwrap();
    }
    block.verify().unwrap();
    let reopened = BlockUnit::open(block.device().crash(usize::MAX)).unwrap();
    reopened.verify().unwrap();
    assert_eq!(dump(&*reopened.snapshot()), dump(&*reference.snapshot()));
    let (ns, key) = (Namespace::new("user"), Key::new("path"));
    for p in [
        Probe::Prefix { ns: &ns, key: &key, prefix: "aaa" },
        Probe::Has { ns: &ns, key: &key },
        Probe::Range { ns: &ns, key: &key, lower: &Bound::Excluded(Value::str("a")), upper: &Bound::Unbounded },
    ] {
        assert_eq!(reopened.snapshot().probe(&p).unwrap(), reference.snapshot().probe(&p).unwrap());
    }
}

#[test]
fn deleting_everything_gives_all_space_back() {
    let mut unit = fresh(512);
    let empty = unit.free_blocks();
    let mut rng = Rng(42);
    let mut ids = Vec::new();
    for i in 0..40 {
        let id = eid(i);
        ids.push(id);
        let n = random_len(&mut rng);
        let content = rng.bytes(n);
        unit.apply(&[Change::Create { id, tags: vec![random_tag(&mut rng)], content }]).unwrap();
        unit.apply(&[Change::Copy { from: id, id: eid(1000 + i), tags: vec![] }]).unwrap();
        unit.apply(&[Change::Write { id: eid(1000 + i), offset: 5000, data: vec![1; 9000] }]).unwrap();
    }
    unit.verify().unwrap();
    let deletes: Vec<Change> =
        ids.iter().flat_map(|&id| [id, eid(1000 + id.0[15] as u64)]).map(|id| Change::Delete { id }).collect();
    unit.apply(&deletes).unwrap();
    unit.verify().unwrap();
    assert!(unit.snapshot().all().unwrap().is_empty());
    assert_eq!(unit.free_blocks(), empty);
}

#[test]
fn a_snapshot_keeps_seeing_its_version() {
    let mut unit = fresh(2048);
    let id = eid(1);
    let old: Vec<u8> = (0..40_000u32).map(|i| i as u8).collect();
    unit.apply(&[Change::Create { id, tags: vec![Tag::flag("user", "old")], content: old.clone() }]).unwrap();
    let snap = unit.snapshot();
    for i in 0..30u8 {
        unit.apply(&[
            Change::Write { id, offset: 0, data: vec![i; 40_000] },
            Change::RemoveTag { id, tag: Tag::flag("user", "old") },
            Change::Create { id: eid(100 + i as u64), tags: vec![], content: vec![i; 9000] },
        ])
        .unwrap();
        unit.apply(&[Change::Delete { id: eid(100 + i as u64) }]).unwrap();
    }
    assert_eq!(dump(&*snap)[&id], (vec![Tag::flag("user", "old")], old));
    unit.verify().unwrap();
    // 旧快照在读的块不能再分配；放掉之后，下一次提交时回到可分配的空间。
    assert!(unit.pinned_blocks() > 30 * 10);
    drop(snap);
    unit.apply(&[Change::AddTag { id, tag: Tag::flag("user", "x") }]).unwrap();
    unit.verify().unwrap();
    assert_eq!(unit.pinned_blocks(), 0);
}

#[test]
fn running_out_of_space_changes_nothing() {
    let mut unit = fresh(64);
    let id = eid(1);
    unit.apply(&[Change::Create { id, tags: vec![], content: vec![1; 100_000] }]).unwrap();
    let before = dump(&*unit.snapshot());
    let big = Change::Create { id: eid(2), tags: vec![], content: vec![2; 200_000] };
    assert_eq!(unit.apply(&[big]), Err(UnitError::NoSpace));
    assert_eq!(dump(&*unit.snapshot()), before);
    unit.verify().unwrap();
    unit.apply(&[Change::Delete { id }]).unwrap();
    unit.apply(&[Change::Create { id: eid(2), tags: vec![], content: vec![2; 200_000] }]).unwrap();
    unit.verify().unwrap();
}

#[test]
fn prepared_transactions_survive_a_restart() {
    let mut unit = fresh(256);
    let a = eid(1);
    unit.apply(&[Change::Create { id: a, tags: vec![], content: b"a".to_vec() }]).unwrap();
    let info = TxnInfo { id: TxnId([3; 16]), primary: UNIT, participants: vec![UNIT, UnitId([8; 16])] };
    let changes = vec![
        Change::AddTag { id: a, tag: Tag::new("tagfs", "txn_pending", "t") },
        Change::Create { id: eid(2), tags: vec![], content: vec![5; 5000] },
    ];
    unit.prepare(&info, &changes).unwrap();
    assert_eq!(unit.snapshot().all().unwrap(), vec![a], "prepared changes are not visible yet");

    let mut unit = BlockUnit::open(unit.device().crash(usize::MAX)).unwrap();
    assert_eq!(unit.prepared().unwrap(), vec![(info.clone(), changes)]);
    unit.commit_prepared(info.id, true).unwrap();
    assert!(unit.prepared().unwrap().is_empty());
    assert_eq!(unit.committed().unwrap(), vec![info.clone()]);
    assert_eq!(dump(&*unit.snapshot())[&eid(2)].1, vec![5; 5000]);
    unit.forget(info.id).unwrap();
    assert!(unit.committed().unwrap().is_empty());
    assert_eq!(unit.abort_prepared(info.id), Err(UnitError::UnknownTxn(info.id)));
    unit.verify().unwrap();
}

#[test]
fn aux_records_commit_together_with_changes() {
    use crate::{Aux, Batch};
    let mut unit = fresh(128);
    let changes = [Change::Create { id: eid(1), tags: vec![], content: b"x".to_vec() }];
    let aux = [Aux::Put(b"log/1".to_vec(), b"a".to_vec()), Aux::Put(b"log/2".to_vec(), b"b".to_vec())];
    unit.commit_batch(&mut Batch { changes: &changes, aux: &aux, ..Batch::default() }).unwrap();
    // 修改失败时附加记录也不生效。
    let bad = [Change::Delete { id: eid(9) }];
    let more = [Aux::Delete(b"log/1".to_vec())];
    assert!(unit.commit_batch(&mut Batch { changes: &bad, aux: &more, ..Batch::default() }).is_err());
    let unit = BlockUnit::open(unit.device().crash(usize::MAX)).unwrap();
    assert_eq!(unit.aux_get(b"log/1").unwrap(), Some(b"a".to_vec()));
    assert_eq!(unit.aux_list(b"log/").unwrap().len(), 2);
    assert_eq!(unit.snapshot().all().unwrap(), vec![eid(1)]);
    unit.verify().unwrap();
}

#[test]
fn a_growable_device_grows_instead_of_running_out() {
    let dev = MemDevice::growable(16 * BS as u64, 64 << 20, BS);
    let mut unit = BlockUnit::format(dev, UNIT).unwrap();
    let mut rng = Rng(3);
    let big = rng.bytes(3 << 20);
    unit.apply(&[Change::Create { id: eid(1), tags: vec![], content: big.clone() }]).unwrap();
    assert!(unit.total_blocks() > 16);
    unit.verify().unwrap();
    let back = BlockUnit::open(unit.device().crash(usize::MAX)).unwrap();
    back.verify().unwrap();
    assert_eq!(dump(&*back.snapshot())[&eid(1)].1, big);
    // 扩到设备上限还不够，才报空间不足。
    let mut unit = back;
    let huge = Change::Create { id: eid(2), tags: vec![], content: vec![1; 80 << 20] };
    assert_eq!(unit.apply(&[huge]), Err(UnitError::NoSpace));
    unit.verify().unwrap();
}

#[test]
fn fill_streams_content_from_a_source() {
    use crate::{Batch, Fill, Source};
    struct Counting(u64);
    impl Source for Counting {
        fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize, UnitError> {
            let n = (self.0.saturating_sub(offset) as usize).min(buf.len());
            for (i, b) in buf[..n].iter_mut().enumerate() {
                *b = ((offset as usize + i) % 253) as u8;
            }
            Ok(n)
        }
    }
    let mut unit = fresh(2048);
    unit.apply(&[Change::Create { id: eid(1), tags: vec![], content: b"old".to_vec() }]).unwrap();
    let mut src = Counting(2_500_000);
    unit.commit_batch(&mut Batch { fills: vec![Fill { id: eid(1), source: &mut src }], ..Batch::default() }).unwrap();
    let got = &dump(&*unit.snapshot())[&eid(1)].1;
    assert_eq!(got.len(), 2_500_000);
    assert!(got.iter().enumerate().all(|(i, b)| *b == (i % 253) as u8));
    unit.verify().unwrap();
}

/// 写到第 `budget` 次就“断电”的设备：之后的写入和刷盘都失败。
struct Budget {
    inner: MemDevice,
    budget: u64,
}

impl BlockDevice for Budget {
    fn block_size(&self) -> u32 {
        self.inner.block_size()
    }

    fn capacity(&self) -> u64 {
        self.inner.capacity()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), DeviceError> {
        self.inner.read_at(offset, buf)
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), DeviceError> {
        if self.budget == 0 {
            return Err(DeviceError::Io);
        }
        self.budget -= 1;
        self.inner.write_at(offset, buf)
    }

    fn flush(&mut self) -> Result<(), DeviceError> {
        if self.budget == 0 {
            return Err(DeviceError::Io);
        }
        self.inner.flush()
    }

    fn trim(&mut self, offset: u64, len: u64) -> Result<(), DeviceError> {
        self.inner.trim(offset, len)
    }
}

/// 在一次提交的每一次写入处断电，并且未刷盘的写入落下任意前缀：重新挂载后，
/// 要么是提交前的状态，要么是提交后的状态，而且空间记账没有错。
#[test]
fn power_loss_at_any_write_leaves_old_or_new_state() {
    let mut rng = Rng(7);
    let mut base = fresh(512);
    let mut next_id = 0;
    let mut reference = MemUnit::new(UNIT);
    for _ in 0..12 {
        let live = reference.snapshot().all().unwrap();
        let changes = random_changes(&mut rng, &live, &mut next_id);
        if reference.apply(&changes).is_ok() {
            base.apply(&changes).unwrap();
        }
    }
    for round in 0..15 {
        let before = dump(&*reference.snapshot());
        let live = reference.snapshot().all().unwrap();
        let changes = random_changes(&mut rng, &live, &mut next_id);
        if reference.apply(&changes).is_err() {
            continue;
        }
        let after = dump(&*reference.snapshot());
        let image = base.device().crash(usize::MAX);
        for budget in 0.. {
            let dev = Budget { inner: image.crash(usize::MAX), budget };
            let mut unit = BlockUnit::open(dev).unwrap();
            let done = unit.apply(&changes).is_ok();
            let pending = unit.device().inner.pending_writes();
            for keep in 0..=pending {
                let crashed = unit.device().inner.crash(keep);
                let back = BlockUnit::open(crashed).unwrap();
                back.verify().unwrap();
                let state = dump(&*back.snapshot());
                assert!(state == before || state == after, "round {round} budget {budget} keep {keep}");
                if done {
                    assert!(state == after, "a finished commit must survive");
                }
            }
            if done {
                break;
            }
        }
        base.apply(&changes).unwrap();
    }
}

#[test]
fn a_damaged_superblock_falls_back_to_the_other() {
    let mut unit = fresh(128);
    unit.apply(&[Change::Create { id: eid(1), tags: vec![], content: b"one".to_vec() }]).unwrap();
    let one = dump(&*unit.snapshot());
    unit.apply(&[Change::Create { id: eid(2), tags: vec![], content: b"two".to_vec() }]).unwrap();
    let mut dev = unit.device().crash(usize::MAX);
    // 最新的超级块写坏了一个字节。
    let slot = unit.generation() % 2;
    dev.write_at(slot * BS as u64 + 30, &[0xaa]).unwrap();
    let back = BlockUnit::open(dev).unwrap();
    assert_eq!(dump(&*back.snapshot()), one);
    back.verify().unwrap();
}

#[test]
fn works_as_a_volume_unit() {
    use tagfs_core::{IdSource, Query, Rules, Volume};

    struct Ids(u64);
    impl IdSource for Ids {
        fn unix_millis(&mut self) -> u64 {
            1
        }
        fn fill_random(&mut self, buf: &mut [u8]) {
            self.0 += 1;
            buf.fill(0);
            buf[..8].copy_from_slice(&self.0.to_be_bytes());
        }
    }
    let rules = || {
        Rules::<()>::new()
            .reader("user", |_| true)
            .reader("tagfs", |_| true)
            .writer("user", |_| true)
            .modify(|_, _| true)
            .create(|_| Some(vec![]))
    };
    let one = BlockUnit::format(mem_device(256), UnitId([1; 16])).unwrap();
    let two = BlockUnit::format(mem_device(256), UnitId([2; 16])).unwrap();
    let mut v = Volume::new(rules(), Box::new(Ids(0)));
    v.attach(Box::new(one)).unwrap();
    v.attach(Box::new(two)).unwrap();

    let mut tx = v.begin(&());
    let a = tx.create(Some(UnitId([1; 16])), vec![Tag::new("user", "name", "a")], vec![9; 10_000]).unwrap();
    tx.commit().unwrap();
    let mut tx = v.begin(&());
    let b = tx.copy(&a, Some(UnitId([2; 16])), vec![Tag::new("user", "name", "b")]).unwrap();
    tx.add_tag(&a, Tag::new("user", "copied_to", b.to_string())).unwrap();
    tx.commit().unwrap();

    assert_eq!(v.query(&(), &Query::has("user", "name")).unwrap().len(), 2);
    assert_eq!(v.query(&(), &Query::has("tagfs", "txn_pending")).unwrap(), vec![]);
    let mut buf = vec![0u8; 10_000];
    assert_eq!(v.read(&(), &b, 0, &mut buf).unwrap(), 10_000);
    assert_eq!(buf, vec![9; 10_000]);
}
