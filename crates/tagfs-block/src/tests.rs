extern crate std;

use alloc::collections::BTreeMap;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::ops::Bound;
use core::sync::atomic::{AtomicU64, Ordering};

use tagfs_backend_mem::MemUnit;
use tagfs_core::{
    Change, EntityId, Key, Namespace, Probe, Snapshot, StageId, Tag, TxnId, TxnInfo, Unit, UnitError, UnitId, Value,
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

/// 对单元的一次操作：一组元数据修改，或者一次内容写入、截断。
#[derive(Debug)]
enum Op {
    /// 修改里 `Create` 的 `content` 是 `contents` 的下标，用的时候换成这个单元里写好的暂存。
    Changes(Vec<Change>, Vec<Vec<u8>>),
    Write(EntityId, u64, Vec<u8>),
    SetLen(EntityId, u64),
}

/// 新建实体的修改，内容先写进暂存。
fn create(unit: &dyn Unit, id: EntityId, tags: Vec<Tag>, content: &[u8]) -> Result<(), UnitError> {
    let op = Op::Changes(vec![Change::Create { id, tags, content: Some(StageId(0)) }], vec![content.to_vec()]);
    run(unit, &op)
}

fn run(unit: &dyn Unit, op: &Op) -> Result<(), UnitError> {
    match op {
        Op::Changes(changes, contents) => {
            let mut stages = Vec::new();
            for c in contents {
                let s = unit.stage()?;
                stages.push(s);
                unit.write_staged(s, 0, c)?;
            }
            let changes: Vec<Change> = changes
                .iter()
                .map(|c| match c {
                    Change::Create { id, tags, content: Some(i) } => {
                        Change::Create { id: *id, tags: tags.clone(), content: Some(stages[i.0 as usize]) }
                    }
                    other => other.clone(),
                })
                .collect();
            let r = unit.apply(&changes);
            if r.is_err() {
                // 设备坏了的时候丢也丢不掉；重新挂载时单元自己回收。
                for s in stages {
                    let _ = unit.discard_staged(s);
                }
            }
            r
        }
        Op::Write(id, offset, data) => unit.write(id, *offset, data),
        Op::SetLen(id, len) => unit.set_len(id, *len),
    }
}

/// 一次随机操作，引用的实体多半存在，偶尔不存在（应当整组失败）。
fn random_op(rng: &mut Rng, live: &[EntityId], next_id: &mut u64) -> Op {
    let pick = |rng: &mut Rng, live: &[EntityId]| {
        if live.is_empty() || rng.below(30) == 0 { eid(999_999) } else { live[rng.below(live.len() as u64) as usize] }
    };
    match rng.below(10) {
        0..=2 => {
            let n = random_len(rng);
            Op::Write(pick(rng, live), rng.below(12 * BS as u64), rng.bytes(n))
        }
        3 => Op::SetLen(pick(rng, live), random_len(rng) as u64),
        _ => {
            let mut out = Vec::new();
            let mut contents = Vec::new();
            let mut live = live.to_vec();
            for _ in 0..1 + rng.below(4) {
                let c = match rng.below(7) {
                    0 | 1 => {
                        *next_id += 1;
                        let id = eid(*next_id);
                        live.push(id);
                        let tags = (0..rng.below(4)).map(|_| random_tag(rng)).collect();
                        let content = if rng.below(4) == 0 {
                            None
                        } else {
                            let n = random_len(rng);
                            contents.push(rng.bytes(n));
                            Some(StageId(contents.len() as u64 - 1))
                        };
                        Change::Create { id, tags, content }
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
                    _ => Change::RemoveTag { id: pick(rng, &live), tag: random_tag(rng) },
                };
                out.push(c);
            }
            Op::Changes(out, contents)
        }
    }
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
    assert!(!crate::is_formatted(&mem_device(64)));
    assert!(crate::is_formatted(fresh(64).device()));
    assert_eq!(BlockUnit::open(mem_device(64)).err(), Some(OpenError::NotFormatted));
    assert_eq!(BlockUnit::format(MemDevice::new(64 * 512, 512), UNIT).err(), Some(OpenError::BadBlockSize));
    assert_eq!(BlockUnit::format(mem_device(4), UNIT).err(), Some(OpenError::TooSmall));
}

#[test]
fn behaves_like_the_reference_unit() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let block = fresh(2048);
    let reference = MemUnit::new(UNIT);
    let mut next_id = 0;
    for round in 0..400 {
        let live = reference.snapshot().all().unwrap();
        let op = random_op(&mut rng, &live, &mut next_id);
        let expect = run(&reference, &op);
        let got = run(&block, &op);
        assert_eq!(got, expect, "round {round}: {op:?}");
        let (a, b) = (block.snapshot(), reference.snapshot());
        assert_eq!(dump(&*a), dump(&*b), "round {round}");
        if round % 25 == 0 {
            same_probes(&*a, &*b);
            block.verify().unwrap();
        }
    }
    block.verify().unwrap();
    // 落盘之后重新挂载，内容不变。
    block.sync().unwrap();
    let reopened = BlockUnit::open(block.device().crash(usize::MAX)).unwrap();
    reopened.verify().unwrap();
    assert_eq!(dump(&*reopened.snapshot()), dump(&*reference.snapshot()));
    same_probes(&*reopened.snapshot(), &*reference.snapshot());
}

#[test]
fn tags_up_to_the_length_limit_work() {
    let block = fresh(2048);
    let reference = MemUnit::new(UNIT);
    // 最坏情况：全是 NUL，转义后翻倍。
    let longest = |c: char| Tag::new("user", "path", c.to_string().repeat(tagfs_core::tag::MAX_STR_LEN));
    let mut changes = Vec::new();
    for i in 0..60u64 {
        let c = ['\0', 'a', 'b'][i as usize % 3];
        let mut tag = longest(c);
        if let Value::Str(s) = &mut tag.value {
            s.truncate(s.len() - i as usize);
        }
        changes.push(Change::Create { id: eid(i), tags: vec![tag, Tag::new("user", "n", i)], content: None });
    }
    for unit in [&block as &dyn Unit, &reference] {
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
    let unit = fresh(512);
    let empty = unit.free_blocks();
    let mut rng = Rng(42);
    let mut ids = Vec::new();
    for i in 0..40 {
        let id = eid(i);
        ids.push(id);
        let n = random_len(&mut rng);
        let content = rng.bytes(n);
        create(&unit, id, vec![random_tag(&mut rng)], &content).unwrap();
        unit.apply(&[Change::Copy { from: id, id: eid(1000 + i), tags: vec![] }]).unwrap();
        unit.write(&eid(1000 + i), 5000, &[1; 9000]).unwrap();
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
    let unit = fresh(2048);
    let id = eid(1);
    let old: Vec<u8> = (0..40_000u32).map(|i| i as u8).collect();
    create(&unit, id, vec![Tag::flag("user", "old")], &old).unwrap();
    let snap = unit.snapshot();
    for i in 0..30u8 {
        unit.write(&id, 0, &[i; 40_000]).unwrap();
        unit.apply(&[Change::RemoveTag { id, tag: Tag::flag("user", "old") }]).unwrap();
        create(&unit, eid(100 + i as u64), vec![], &[i; 9000]).unwrap();
        unit.apply(&[Change::Delete { id: eid(100 + i as u64) }]).unwrap();
    }
    assert_eq!(dump(&*snap)[&id], (vec![Tag::flag("user", "old")], old));
    unit.verify().unwrap();
    // 旧快照在读的块不能再分配；放掉之后，下一次改动时回到可分配的空间。
    assert!(unit.pinned_blocks() > 30 * 10);
    drop(snap);
    unit.apply(&[Change::AddTag { id, tag: Tag::flag("user", "x") }]).unwrap();
    unit.verify().unwrap();
    assert_eq!(unit.pinned_blocks(), 0);
}

#[test]
fn running_out_of_space_changes_nothing() {
    let unit = fresh(64);
    let id = eid(1);
    create(&unit, id, vec![], &[1; 100_000]).unwrap();
    let before = dump(&*unit.snapshot());
    // 暂存写不下：报空间不足，单元不变。
    assert_eq!(create(&unit, eid(2), vec![], &[2; 200_000]), Err(UnitError::NoSpace));
    assert_eq!(unit.write(&id, 100_000, &[3; 200_000]), Err(UnitError::NoSpace));
    assert_eq!(dump(&*unit.snapshot()), before);
    unit.verify().unwrap();
    unit.apply(&[Change::Delete { id }]).unwrap();
    create(&unit, eid(2), vec![], &[2; 200_000]).unwrap();
    unit.verify().unwrap();
}

#[test]
fn writes_are_visible_at_once_and_durable_after_sync() {
    let unit = fresh(512);
    let id = eid(1);
    create(&unit, id, vec![], b"committed").unwrap();
    unit.write(&id, 0, &[7; 30_000]).unwrap();
    assert_eq!(dump(&*unit.snapshot())[&id].1, vec![7; 30_000]);
    unit.verify().unwrap();
    // 没落盘就断电：回到上一次提交的内容，空间也都在。
    let lost = BlockUnit::open(unit.device().crash(usize::MAX)).unwrap();
    lost.verify().unwrap();
    assert_eq!(dump(&*lost.snapshot())[&id].1, b"committed");
    unit.sync().unwrap();
    let kept = BlockUnit::open(unit.device().crash(usize::MAX)).unwrap();
    kept.verify().unwrap();
    assert_eq!(dump(&*kept.snapshot())[&id].1, vec![7; 30_000]);
}

#[test]
fn unused_stages_are_reclaimed() {
    let unit = fresh(512);
    let empty = unit.free_blocks();
    let a = unit.stage().unwrap();
    unit.write_staged(a, 0, &[1; 50_000]).unwrap();
    let b = unit.stage().unwrap();
    unit.write_staged(b, 0, &[2; 50_000]).unwrap();
    unit.discard_staged(b).unwrap();
    assert_eq!(unit.write_staged(b, 0, b"x"), Err(UnitError::UnknownStage(b)));
    unit.verify().unwrap();
    // 落了盘的暂存没被用上：重新挂载时回收。
    unit.sync().unwrap();
    let back = BlockUnit::open(unit.device().crash(usize::MAX)).unwrap();
    back.verify().unwrap();
    assert_eq!(back.write_staged(a, 0, b"x"), Err(UnitError::UnknownStage(a)));
    assert_eq!(back.free_blocks(), empty);
}

#[test]
fn prepared_transactions_survive_a_restart() {
    let unit = fresh(256);
    let a = eid(1);
    create(&unit, a, vec![], b"a").unwrap();
    let info = TxnInfo { id: TxnId([3; 16]), primary: UNIT, participants: vec![UNIT, UnitId([8; 16])] };
    let stage = unit.stage().unwrap();
    unit.write_staged(stage, 0, &[5; 5000]).unwrap();
    let changes = vec![
        Change::AddTag { id: a, tag: Tag::new("tagfs", "txn_pending", "t") },
        Change::Create { id: eid(2), tags: vec![], content: Some(stage) },
    ];
    assert_eq!(
        unit.prepare(&info, &[Change::Create { id: eid(3), tags: vec![], content: Some(StageId(999)) }]),
        Err(UnitError::UnknownStage(StageId(999)))
    );
    unit.prepare(&info, &changes).unwrap();
    assert_eq!(unit.snapshot().all().unwrap(), vec![a], "prepared changes are not visible yet");

    // 准备记录引用的暂存随它一起保留下来。
    let unit = BlockUnit::open(unit.device().crash(usize::MAX)).unwrap();
    assert_eq!(unit.prepared().unwrap(), vec![(info.clone(), changes)]);
    unit.commit_prepared(info.id, true).unwrap();
    assert!(unit.prepared().unwrap().is_empty());
    assert_eq!(unit.committed().unwrap(), vec![info.clone()]);
    assert_eq!(dump(&*unit.snapshot())[&eid(2)].1, vec![5; 5000]);
    unit.forget(info.id).unwrap();
    assert!(unit.committed().unwrap().is_empty());
    assert_eq!(unit.abort_prepared(info.id), Err(UnitError::UnknownTxn(info.id)));
    unit.verify().unwrap();

    // 放弃的事务把它占着的暂存一并丢掉。
    let empty = unit.free_blocks();
    let stage = unit.stage().unwrap();
    unit.write_staged(stage, 0, &[6; 20_000]).unwrap();
    unit.prepare(&info, &[Change::Create { id: eid(4), tags: vec![], content: Some(stage) }]).unwrap();
    unit.abort_prepared(info.id).unwrap();
    unit.verify().unwrap();
    assert_eq!(unit.free_blocks(), empty);
}

#[test]
fn aux_records_commit_together_with_changes() {
    use crate::{Aux, Batch};
    let unit = fresh(128);
    let changes = [Change::Create { id: eid(1), tags: vec![], content: None }];
    let aux = [Aux::Put(b"log/1".to_vec(), b"a".to_vec()), Aux::Put(b"log/2".to_vec(), b"b".to_vec())];
    unit.commit_batch(&Batch { changes: &changes, aux: &aux, ..Batch::default() }).unwrap();
    // 修改失败时附加记录也不生效。
    let bad = [Change::Delete { id: eid(9) }];
    let more = [Aux::Delete(b"log/1".to_vec())];
    assert!(unit.commit_batch(&Batch { changes: &bad, aux: &more, ..Batch::default() }).is_err());
    let unit = BlockUnit::open(unit.device().crash(usize::MAX)).unwrap();
    assert_eq!(unit.aux_get(b"log/1").unwrap(), Some(b"a".to_vec()));
    assert_eq!(unit.aux_list(b"log/").unwrap().len(), 2);
    assert_eq!(unit.snapshot().all().unwrap(), vec![eid(1)]);
    unit.verify().unwrap();
}

#[test]
fn adopting_a_stage_replaces_the_content() {
    use crate::Batch;
    let unit = fresh(2048);
    create(&unit, eid(1), vec![], b"old").unwrap();
    let stage = unit.stage().unwrap();
    // 分段写进暂存，不必整个放进内存。
    let chunk: Vec<u8> = (0..1_000_000u32).map(|i| (i % 253) as u8).collect();
    for i in 0..3u64 {
        unit.write_staged(stage, i * chunk.len() as u64, &chunk).unwrap();
    }
    unit.commit_batch(&Batch { adopt: &[(eid(1), stage)], ..Batch::default() }).unwrap();
    let got = &dump(&*unit.snapshot())[&eid(1)].1;
    assert_eq!(got.len(), 3_000_000);
    assert!(got.chunks(chunk.len()).all(|c| c == chunk.as_slice()));
    unit.verify().unwrap();
}

#[test]
fn a_growable_device_grows_instead_of_running_out() {
    let dev = MemDevice::growable(16 * BS as u64, 64 << 20, BS);
    let unit = BlockUnit::format(dev, UNIT).unwrap();
    let mut rng = Rng(3);
    let big = rng.bytes(3 << 20);
    create(&unit, eid(1), vec![], &big).unwrap();
    assert!(unit.total_blocks() > 16);
    unit.verify().unwrap();
    let back = BlockUnit::open(unit.device().crash(usize::MAX)).unwrap();
    back.verify().unwrap();
    assert_eq!(dump(&*back.snapshot())[&eid(1)].1, big);
    // 扩到设备上限还不够，才报空间不足。
    assert_eq!(create(&back, eid(2), vec![], &vec![1; 80 << 20]), Err(UnitError::NoSpace));
    back.verify().unwrap();
}

#[test]
fn readers_and_writers_on_different_entities_do_not_wait_for_each_other() {
    let unit = Arc::new(fresh(8192));
    let ids: Vec<EntityId> = (0..4).map(eid).collect();
    for id in &ids {
        create(&*unit, *id, vec![], b"").unwrap();
    }
    std::thread::scope(|s| {
        for (n, id) in ids.iter().enumerate() {
            let unit = &unit;
            s.spawn(move || {
                for i in 0..40u64 {
                    unit.write(id, i * 10_000, &vec![n as u8 + 1; 10_000]).unwrap();
                    // 同时有人在读整个单元。
                    let _ = dump(&*unit.snapshot());
                }
            });
        }
    });
    unit.verify().unwrap();
    unit.sync().unwrap();
    let back = BlockUnit::open(unit.device().crash(usize::MAX)).unwrap();
    back.verify().unwrap();
    let all = dump(&*back.snapshot());
    for (n, id) in ids.iter().enumerate() {
        assert_eq!(all[id].1, vec![n as u8 + 1; 400_000]);
    }
}

/// 写到第 `budget` 次就“断电”的设备：之后的写入和刷盘都失败。
struct Budget {
    inner: MemDevice,
    budget: AtomicU64,
}

impl Budget {
    fn spend(&self) -> Result<(), DeviceError> {
        self.budget
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |b| b.checked_sub(1))
            .map(|_| ())
            .map_err(|_| DeviceError::Io)
    }
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

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), DeviceError> {
        self.spend()?;
        self.inner.write_at(offset, buf)
    }

    fn flush(&self) -> Result<(), DeviceError> {
        if self.budget.load(Ordering::SeqCst) == 0 {
            return Err(DeviceError::Io);
        }
        self.inner.flush()
    }

    fn trim(&self, offset: u64, len: u64) -> Result<(), DeviceError> {
        self.inner.trim(offset, len)
    }
}

/// 在一次操作（随后落盘）的每一次写入处断电，并且未刷盘的写入落下任意前缀：重新挂载后，
/// 要么是操作前的状态，要么是操作后的状态，而且空间记账没有错。
#[test]
fn power_loss_at_any_write_leaves_old_or_new_state() {
    let mut rng = Rng(7);
    let base = fresh(512);
    let mut next_id = 0;
    let reference = MemUnit::new(UNIT);
    for _ in 0..12 {
        let live = reference.snapshot().all().unwrap();
        let op = random_op(&mut rng, &live, &mut next_id);
        if run(&reference, &op).is_ok() {
            run(&base, &op).unwrap();
        }
    }
    base.sync().unwrap();
    for round in 0..20 {
        let before = dump(&*reference.snapshot());
        let live = reference.snapshot().all().unwrap();
        let op = random_op(&mut rng, &live, &mut next_id);
        if run(&reference, &op).is_err() {
            continue;
        }
        let after = dump(&*reference.snapshot());
        let image = base.device().crash(usize::MAX);
        for budget in 0.. {
            let dev = Budget { inner: image.crash(usize::MAX), budget: AtomicU64::new(budget) };
            let unit = BlockUnit::open(dev).unwrap();
            let done = run(&unit, &op).and_then(|()| unit.sync()).is_ok();
            let pending = unit.device().inner.pending_writes();
            for keep in 0..=pending {
                let crashed = unit.device().inner.crash(keep);
                let back = BlockUnit::open(crashed).unwrap();
                back.verify().unwrap();
                let state = dump(&*back.snapshot());
                assert!(state == before || state == after, "round {round} budget {budget} keep {keep}: {op:?}");
                if done {
                    assert!(state == after, "a finished sync must survive");
                }
            }
            if done {
                break;
            }
        }
        run(&base, &op).unwrap();
        base.sync().unwrap();
    }
}

#[test]
fn a_damaged_superblock_falls_back_to_the_other() {
    let unit = fresh(128);
    create(&unit, eid(1), vec![], b"one").unwrap();
    let one = dump(&*unit.snapshot());
    create(&unit, eid(2), vec![], b"two").unwrap();
    let dev = unit.device().crash(usize::MAX);
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
    let v = Volume::new(rules(), alloc::boxed::Box::new(Ids(0)));
    v.attach(Arc::new(one)).unwrap();
    v.attach(Arc::new(two)).unwrap();

    let stage = v.stage(Some(UnitId([1; 16]))).unwrap();
    stage.write(0, &[9; 10_000]).unwrap();
    let mut tx = v.begin(&());
    let a = tx.create_from(stage, vec![Tag::new("user", "name", "a")]).unwrap();
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
