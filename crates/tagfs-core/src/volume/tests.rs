use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use super::*;
use crate::mem_unit::MemUnit;
use crate::policy::Rules;

/// 测试用的调用上下文：当前用户、当前应用、是否有“读取所有用户文件”的能力，
/// 以及这次创建的实体是否应用专属（操作系统据此附加 owner_app 边界）。
struct Ctx {
    user: &'static str,
    app: &'static str,
    all_users: bool,
    app_private: bool,
}

const ALICE: Ctx = Ctx { user: "alice", app: "notes", all_users: false, app_private: false };
const ALICE_PRIVATE: Ctx = Ctx { user: "alice", app: "notes", all_users: false, app_private: true };
const ALICE_IN_PLAYER: Ctx = Ctx { user: "alice", app: "player", all_users: false, app_private: false };
const BOB: Ctx = Ctx { user: "bob", app: "notes", all_users: false, app_private: false };
const ADMIN_IN_PLAYER: Ctx = Ctx { user: "root", app: "player", all_users: true, app_private: false };

fn is(v: &Value, s: &str) -> bool {
    matches!(v, Value::Str(x) if x == s)
}

/// 操作系统一侧的规则：归属用户和归属应用两道边界，`user` 命名空间由实体的主人写。
fn rules() -> Rules<Ctx> {
    Rules::new()
        .boundary("system", "owner_user", |c: &Ctx, vs| c.all_users || vs.iter().any(|v| is(v, c.user)))
        // “读取所有用户文件”的能力只放宽用户边界，不放宽应用边界。
        .boundary("system", "owner_app", |c: &Ctx, vs| vs.iter().any(|v| is(v, c.app)))
        .writer("user", |_| true)
        .modify(|c: &Ctx, tags| tags.iter().any(|t| t.key.as_str() == "owner_user" && is(&t.value, c.user)))
        .create(|c: &Ctx| {
            let mut tags = vec![Tag::new("system", "owner_user", c.user)];
            if c.app_private {
                tags.push(Tag::new("system", "owner_app", c.app));
            }
            Some(tags)
        })
}

struct Counter(u64);

impl IdSource for Counter {
    fn unix_millis(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }
    fn fill_random(&mut self, buf: &mut [u8]) {
        buf.fill(self.0 as u8);
    }
}

fn uid(n: u8) -> UnitId {
    UnitId([n; 16])
}

fn volume(units: &[u8]) -> Volume<Rules<Ctx>> {
    let mut v = Volume::new(rules(), Box::new(Counter(0)));
    for n in units {
        v.attach(Box::new(MemUnit::new(uid(*n)))).unwrap();
    }
    v
}

fn name(n: &str) -> Tag {
    Tag::new("user", "name", n)
}

fn create(v: &mut Volume<Rules<Ctx>>, ctx: &Ctx, unit: Option<u8>, tags: Vec<Tag>, content: &[u8]) -> EntityId {
    let mut tx = v.begin(ctx);
    let id = tx.create(unit.map(uid), tags, content.to_vec()).unwrap();
    tx.commit().unwrap();
    id
}

fn read_all(v: &Volume<Rules<Ctx>>, ctx: &Ctx, id: &EntityId) -> Vec<u8> {
    let size = v.get(ctx, id).unwrap().size as usize;
    let mut buf = vec![0u8; size];
    v.read(ctx, id, 0, &mut buf).unwrap();
    buf
}

#[test]
fn names_are_plain_tags_and_may_repeat() {
    let mut v = volume(&[1]);
    let a = create(&mut v, &ALICE, None, vec![name("report.pdf"), Tag::new("user", "project", "alpha")], b"A");
    let b = create(&mut v, &ALICE, None, vec![name("report.pdf"), Tag::new("user", "project", "beta")], b"B");
    assert_eq!(v.query(&ALICE, &Query::is("user", "name", "report.pdf")).unwrap(), {
        let mut ids = vec![a, b];
        ids.sort();
        ids
    });
    let both = Query::And(vec![Query::is("user", "name", "report.pdf"), Query::is("user", "project", "alpha")]);
    assert_eq!(v.query(&ALICE, &both).unwrap(), vec![a]);
    assert_eq!(read_all(&v, &ALICE, &b), b"B");
}

#[test]
fn invisible_is_the_same_as_missing() {
    let mut v = volume(&[1]);
    let a = create(&mut v, &ALICE, None, vec![name("secret")], b"x");
    assert_eq!(v.get(&BOB, &a), Err(Error::NotFound));
    assert_eq!(v.get(&BOB, &EntityId([9; 16])), Err(Error::NotFound));
    assert!(v.query(&BOB, &Query::All).unwrap().is_empty());
    assert_eq!(v.read(&BOB, &a, 0, &mut [0u8; 1]), Err(Error::NotFound));
    let mut tx = v.begin(&BOB);
    assert_eq!(tx.add_tag(&a, name("mine")), Err(Error::NotFound));
}

#[test]
fn a_capability_relaxes_only_its_own_boundary() {
    let mut v = volume(&[1]);
    let plain = create(&mut v, &ALICE, None, vec![name("plain")], b"");
    let app_only = create(&mut v, &ALICE_PRIVATE, None, vec![name("private")], b"");
    // 不带 owner_app 的实体，有全部用户权限的主体能看见。
    assert!(v.get(&ADMIN_IN_PLAYER, &plain).is_ok());
    // 带 owner_app=notes 的实体：即使能读所有用户的文件，不是 notes 也看不见。
    assert_eq!(v.get(&ADMIN_IN_PLAYER, &app_only), Err(Error::NotFound));
    // 同一个用户换个应用也看不见；回到 notes 才看得见。
    assert_eq!(v.get(&ALICE_IN_PLAYER, &app_only), Err(Error::NotFound));
    assert!(v.get(&ALICE, &app_only).is_ok());
    assert_eq!(v.query(&ALICE_IN_PLAYER, &Query::has("user", "name")).unwrap(), vec![plain]);
}

#[test]
fn namespace_write_rules_default_to_deny() {
    let mut v = volume(&[1]);
    let a = create(&mut v, &ALICE, None, vec![], b"");
    let mut tx = v.begin(&ALICE);
    assert_eq!(tx.add_tag(&a, Tag::new("system", "owner_app", "x")), Err(Error::NotWritable));
    assert_eq!(tx.add_tag(&a, Tag::new("other", "k", "v")), Err(Error::NotWritable));
    assert_eq!(tx.add_tag(&a, Tag::new("tagfs", "txn_pending", "x")), Err(Error::NotWritable));
    assert_eq!(tx.create(None, vec![Tag::flag("tagfs", "anything")], vec![]), Err(Error::NotWritable));
    assert!(tx.add_tag(&a, name("fine")).is_ok());
    assert_eq!(tx.add_tag(&a, name(&"x".repeat(4097))), Err(Error::TagTooLong));
    assert!(tx.add_tag(&a, name(&"x".repeat(4096))).is_ok());
}

#[test]
fn mapped_tagfs_keys_follow_the_modify_rule() {
    let mut v = volume(&[1]);
    let a = create(&mut v, &ALICE, None, vec![], b"");
    let path = Tag::new("tagfs", "hostfs_path", "/a.txt");
    let mut tx = v.begin(&BOB);
    assert_eq!(tx.add_tag(&a, path.clone()), Err(Error::NotFound), "invisible to bob");
    drop(tx);
    let mut tx = v.begin(&ADMIN_IN_PLAYER);
    assert_eq!(tx.add_tag(&a, path.clone()), Err(Error::NotWritable), "visible but not his");
    drop(tx);
    let mut tx = v.begin(&ALICE);
    assert!(tx.add_tag(&a, path.clone()).is_ok());
    assert!(tx.create(None, vec![Tag::new("tagfs", "hostfs_path", "/b.txt")], vec![]).is_ok());
}

#[test]
fn copy_shares_content_until_written() {
    let mut v = volume(&[1]);
    let a = create(&mut v, &ALICE, None, vec![name("a")], b"hello");
    let mut tx = v.begin(&ALICE);
    let b = tx.copy(&a, None, vec![name("b")]).unwrap();
    tx.commit().unwrap();
    assert_eq!(read_all(&v, &ALICE, &b), b"hello");

    let mut tx = v.begin(&ALICE);
    tx.write(&b, 0, b"J".to_vec()).unwrap();
    tx.commit().unwrap();
    assert_eq!(read_all(&v, &ALICE, &a), b"hello");
    assert_eq!(read_all(&v, &ALICE, &b), b"Jello");

    let mut tx = v.begin(&ALICE);
    tx.delete(&a).unwrap();
    tx.commit().unwrap();
    assert_eq!(v.get(&ALICE, &a), Err(Error::NotFound));
    assert_eq!(read_all(&v, &ALICE, &b), b"Jello");
}

#[test]
fn deleting_entities_releases_objects() {
    let mut unit = MemUnit::new(uid(1));
    let a = EntityId([1; 16]);
    let b = EntityId([2; 16]);
    unit.apply(&[
        Change::Create { id: a, tags: vec![], content: b"x".to_vec() },
        Change::Copy { from: a, id: b, tags: vec![] },
    ])
    .unwrap();
    assert_eq!(unit.object_count(), 1);
    unit.apply(&[Change::Delete { id: a }]).unwrap();
    assert_eq!(unit.object_count(), 1);
    unit.apply(&[Change::Delete { id: b }]).unwrap();
    assert_eq!(unit.object_count(), 0);
}

#[test]
fn a_failed_change_leaves_the_unit_untouched() {
    let mut v = volume(&[1]);
    let a = create(&mut v, &ALICE, None, vec![name("a")], b"");
    let mut tx = v.begin(&ALICE);
    tx.add_tag(&a, name("renamed")).unwrap();
    tx.delete(&a).unwrap();
    tx.commit().unwrap();
    assert_eq!(v.get(&ALICE, &a), Err(Error::NotFound));

    let mut unit = MemUnit::new(uid(2));
    let x = EntityId([1; 16]);
    let err = unit
        .apply(&[Change::Create { id: x, tags: vec![], content: vec![] }, Change::Delete { id: EntityId([2; 16]) }]);
    assert!(err.is_err());
    assert!(unit.snapshot().get(&x).unwrap().is_none());
}

#[test]
fn units_are_one_volume_and_leave_with_their_entities() {
    let mut v = volume(&[1, 2]);
    let a = create(&mut v, &ALICE, Some(1), vec![name("on-1")], b"");
    let b = create(&mut v, &ALICE, Some(2), vec![name("on-2")], b"");
    assert_eq!(v.query(&ALICE, &Query::has("user", "name")).unwrap().len(), 2);

    let two = v.detach(uid(2)).unwrap();
    assert_eq!(v.get(&ALICE, &b), Err(Error::NotFound));
    assert_eq!(v.query(&ALICE, &Query::has("user", "name")).unwrap(), vec![a]);
    let mut tx = v.begin(&ALICE);
    assert_eq!(tx.create(Some(uid(2)), vec![], vec![]), Err(Error::NoUnit));
    drop(tx);

    v.attach(two).unwrap();
    assert!(v.get(&ALICE, &b).is_ok());
}

fn pending_tags(v: &Volume<Rules<Ctx>>, ctx: &Ctx) -> usize {
    v.query(ctx, &Query::has("tagfs", "txn_pending")).unwrap().len()
}

#[test]
fn a_cross_unit_transaction_is_atomic() {
    let mut v = volume(&[1, 2]);
    let a = create(&mut v, &ALICE, Some(1), vec![name("a")], b"aaa");
    let mut tx = v.begin(&ALICE);
    let b = tx.copy(&a, Some(uid(2)), vec![name("b")]).unwrap();
    tx.add_tag(&a, Tag::new("user", "copied_to", b.to_string())).unwrap();
    tx.commit().unwrap();

    assert_eq!(read_all(&v, &ALICE, &b), b"aaa");
    assert_eq!(v.query(&ALICE, &Query::is("user", "copied_to", b.to_string())).unwrap(), vec![a]);
    // 全部完成后不留标记，主单元也清掉了记录。
    assert_eq!(pending_tags(&v, &ALICE), 0);
    assert!(v.unit(uid(1)).unwrap().committed().unwrap().is_empty());
    assert!(v.unit(uid(2)).unwrap().committed().unwrap().is_empty());
}

/// 手工搭出“主单元已提交、参与单元还没完成就断电”的现场。
/// 单元 1 是主单元，新建 x；单元 2 给已有实体 e 加 Tag、新建 y。
fn half_done(primary_committed: bool) -> (MemUnit, MemUnit, EntityId, EntityId, EntityId, String) {
    let mut one = MemUnit::new(uid(1));
    let mut two = MemUnit::new(uid(2));
    let owner = Tag::new("system", "owner_user", "alice");
    let e = EntityId([0xe0; 16]);
    two.apply(&[Change::Create { id: e, tags: vec![owner.clone(), name("e")], content: vec![] }]).unwrap();

    let txn = TxnId([0x77; 16]);
    let pending = pending_tag(txn);
    let info = TxnInfo { id: txn, primary: uid(1), participants: vec![uid(1), uid(2)] };
    let x = EntityId([0x10; 16]);
    let y = EntityId([0x20; 16]);
    one.prepare(&info, &[Change::Create { id: x, tags: vec![owner.clone(), pending.clone()], content: vec![] }])
        .unwrap();
    two.prepare(
        &info,
        &[
            Change::AddTag { id: e, tag: name("touched") },
            Change::Create { id: y, tags: vec![owner, pending.clone()], content: vec![] },
            Change::AddTag { id: e, tag: pending.clone() },
        ],
    )
    .unwrap();
    if primary_committed {
        one.commit_prepared(txn, true).unwrap();
    }
    let Value::Str(value) = pending.value else { unreachable!() };
    (one, two, e, x, y, value)
}

#[test]
fn an_undecided_unit_stays_online_with_pending_tags() {
    let (one, two, e, x, y, txn) = half_done(true);
    let mut v = Volume::new(rules(), Box::new(Counter(100)));
    v.attach(Box::new(two)).unwrap();

    // 主单元不在：单元 2 照常在线，e 带上未完成标记且只读，y 还没出现。
    let got = v.get(&ALICE, &e).unwrap();
    assert!(got.tags.contains(&Tag::new("tagfs", "txn_pending", txn.as_str())));
    assert!(!got.tags.contains(&name("touched")));
    assert_eq!(v.get(&ALICE, &y), Err(Error::NotFound));
    let mut tx = v.begin(&ALICE);
    assert_eq!(tx.add_tag(&e, name("other")), Err(Error::Pending));
    drop(tx);

    // 主单元上线：事务已提交，补完，去掉标记。
    v.attach(Box::new(one)).unwrap();
    assert!(v.get(&ALICE, &e).unwrap().tags.contains(&name("touched")));
    assert!(v.get(&ALICE, &x).is_ok());
    assert!(v.get(&ALICE, &y).is_ok());
    assert_eq!(pending_tags(&v, &ALICE), 0);
    assert!(v.unit(uid(1)).unwrap().committed().unwrap().is_empty());
    let mut tx = v.begin(&ALICE);
    assert!(tx.add_tag(&e, name("other")).is_ok());
}

#[test]
fn a_transaction_the_primary_never_committed_is_dropped() {
    let (one, two, e, x, y, _) = half_done(false);
    let mut v = Volume::new(rules(), Box::new(Counter(100)));
    v.attach(Box::new(two)).unwrap();
    assert_eq!(pending_tags(&v, &ALICE), 1);

    v.attach(Box::new(one)).unwrap();
    assert!(!v.get(&ALICE, &e).unwrap().tags.contains(&name("touched")));
    assert_eq!(v.get(&ALICE, &x), Err(Error::NotFound));
    assert_eq!(v.get(&ALICE, &y), Err(Error::NotFound));
    assert_eq!(pending_tags(&v, &ALICE), 0);
    assert!(v.unit(uid(1)).unwrap().prepared().unwrap().is_empty());
    assert!(v.unit(uid(2)).unwrap().prepared().unwrap().is_empty());
}

#[test]
fn committed_primary_keeps_its_record_until_every_participant_is_done() {
    let (one, two, _, x, _, _) = half_done(true);
    let mut v = Volume::new(rules(), Box::new(Counter(100)));
    v.attach(Box::new(one)).unwrap();
    // 单元 2 还没上线：x 已经可见，但带着标记；主单元留着记录。
    assert!(v.get(&ALICE, &x).unwrap().tags.iter().any(|t| t.ns.is_tagfs()));
    assert_eq!(v.unit(uid(1)).unwrap().committed().unwrap().len(), 1);
    v.attach(Box::new(two)).unwrap();
    assert_eq!(pending_tags(&v, &ALICE), 0);
    assert!(v.unit(uid(1)).unwrap().committed().unwrap().is_empty());
}
