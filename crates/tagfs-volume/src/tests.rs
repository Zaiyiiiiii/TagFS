use std::path::PathBuf;

use tagfs::{Query, Tag, UnitId};
use tagfs_proto::{Client, Op, Request, Response, UnitInfo};

use super::*;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tagfs-volume-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn attach(s: &mut Service, backend: &str, location: &std::path::Path) -> UnitId {
    match s.handle(Request::Attach { backend: backend.into(), location: location.to_string_lossy().into() }) {
        Response::Unit(u) => u,
        other => panic!("{other:?}"),
    }
}

fn apply(s: &mut Service, ops: Vec<Op>) -> Vec<Option<EntityId>> {
    match s.handle(Request::Apply { ops }) {
        Response::Applied(r) => r,
        other => panic!("{other:?}"),
    }
}

fn find(s: &mut Service, query: Query) -> Vec<EntityId> {
    match s.handle(Request::Query { query, after: None, limit: 1000 }) {
        Response::Ids(ids) => ids,
        other => panic!("{other:?}"),
    }
}

fn units(s: &mut Service) -> Vec<UnitInfo> {
    match s.handle(Request::Units) {
        Response::Units(u) => u,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_volume_of_several_units_survives_a_restart() {
    let base = scratch("restart");
    let state = base.join("state");
    let img = base.join("u.img");
    let dir = base.join("docs");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), b"from the host").unwrap();
    tagfs::image::create(&img, UnitId([1; 16])).unwrap();
    tagfs::DirUnit::create(&dir, UnitId([2; 16]), Box::new(SystemIds)).unwrap();

    let mut s = Service::start(&state).unwrap();
    assert_eq!(attach(&mut s, "file", &img), UnitId([1; 16]));
    assert_eq!(attach(&mut s, "hostdir", &dir), UnitId([2; 16]));
    let created = apply(
        &mut s,
        vec![
            Op::Create { unit: Some(UnitId([1; 16])), tags: vec![Tag::new("user", "n", "x")], content: b"hi".to_vec() },
            Op::Create { unit: Some(UnitId([2; 16])), tags: vec![Tag::new("user", "n", "y")], content: vec![] },
        ],
    );
    let x = created[0].unwrap();
    // 查询跨单元。
    assert_eq!(find(&mut s, Query::has("user", "n")).len(), 2);
    assert_eq!(find(&mut s, Query::All).len(), 3);
    assert_eq!(s.handle(Request::Read { id: x, offset: 0, len: 100 }), Response::Bytes(b"hi".to_vec()));
    assert_eq!(s.handle(Request::Check { unit: UnitId([2; 16]) }), Response::Done);
    // 同一个位置不能加两次。
    assert!(matches!(
        s.handle(Request::Attach { backend: "file".into(), location: img.to_string_lossy().into() }),
        Response::Error(_)
    ));
    assert!(matches!(s.handle(Request::Attach { backend: "nope".into(), location: "x".into() }), Response::Error(_)));
    drop(s);

    // 重启：单元按记录重新上线。
    let mut s = Service::start(&state).unwrap();
    assert!(units(&mut s).iter().all(|u| u.offline.is_none()));
    assert_eq!(find(&mut s, Query::All).len(), 3);
    assert_eq!(s.handle(Request::Read { id: x, offset: 0, len: 100 }), Response::Bytes(b"hi".to_vec()));
    drop(s);

    // 镜像文件暂时不在：单元离线，回来后再列单元时重新上线。
    std::fs::rename(&img, base.join("away.img")).unwrap();
    let mut s = Service::start(&state).unwrap();
    let listed = units(&mut s);
    assert!(listed.iter().any(|u| u.backend == "file" && u.offline.is_some() && u.id.is_none()));
    assert_eq!(find(&mut s, Query::has("user", "n")).len(), 1);
    std::fs::rename(base.join("away.img"), &img).unwrap();
    assert!(units(&mut s).iter().all(|u| u.offline.is_none()));
    assert_eq!(find(&mut s, Query::has("user", "n")).len(), 2);

    // 移出卷。
    assert_eq!(s.handle(Request::Detach { unit: UnitId([1; 16]) }), Response::Done);
    assert_eq!(units(&mut s).len(), 1);
    drop(s);
    let mut s = Service::start(&state).unwrap();
    assert_eq!(units(&mut s).len(), 1);
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn tools_talk_to_it_over_a_local_socket() {
    let base = scratch("socket");
    let state = base.join("state");
    let sock = base.join("v.sock").to_string_lossy().into_owned();
    let name = tagfs_proto::to_name(sock.clone()).unwrap();
    let listener = listen(name).unwrap();
    std::thread::spawn(move || {
        let mut s = Service::start(&state).unwrap();
        s.serve(&listener).unwrap();
    });

    let mut c = Client::connect_to(tagfs_proto::to_name(sock).unwrap()).unwrap();
    let Response::Unit(unit) = c.call(&Request::Attach { backend: "mem".into(), location: String::new() }).unwrap()
    else {
        panic!()
    };
    let resp = c
        .call(&Request::Apply {
            ops: vec![Op::Create { unit: Some(unit), tags: vec![Tag::flag("user", "f")], content: b"abc".to_vec() }],
        })
        .unwrap();
    let Response::Applied(ids) = resp else { panic!("{resp:?}") };
    let id = ids[0].unwrap();
    assert_eq!(
        c.call(&Request::Query { query: Query::has("user", "f"), after: None, limit: 10 }).unwrap(),
        Response::Ids(vec![id])
    );
    assert_eq!(c.call(&Request::Read { id, offset: 1, len: 10 }).unwrap(), Response::Bytes(b"bc".to_vec()));
    assert!(matches!(c.call(&Request::Get { id: EntityId([0; 16]) }).unwrap(), Response::Error(_)));
    // 另一个连接看到同一个卷。
    let mut c2 =
        Client::connect_to(tagfs_proto::to_name(base.join("v.sock").to_string_lossy().into()).unwrap()).unwrap();
    drop(c);
    assert!(matches!(c2.call(&Request::Get { id }).unwrap(), Response::Entity(_)));
    // 同一个名字上不能再起第二个卷。
    let again = listen(tagfs_proto::to_name(base.join("v.sock").to_string_lossy().into()).unwrap());
    assert_eq!(again.err().map(|e| e.kind()), Some(io::ErrorKind::AddrInUse));
}

#[cfg(unix)]
#[test]
fn a_socket_left_behind_by_a_dead_volume_is_reused() {
    let base = scratch("stale");
    let sock = base.join("v.sock");
    // 一个没人监听的套接字文件，像上一个卷被强行结束后留下的那样。
    drop(std::os::unix::net::UnixListener::bind(&sock).unwrap());
    assert!(sock.exists());
    let listener = listen(tagfs_proto::to_name(sock.to_string_lossy().into()).unwrap()).unwrap();
    drop(listener);
}
