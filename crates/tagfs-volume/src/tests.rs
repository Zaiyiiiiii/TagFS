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

fn attach(s: &mut Session<'_>, backend: &str, location: &std::path::Path) -> UnitId {
    match s.handle(Request::Attach { backend: backend.into(), location: location.to_string_lossy().into() }) {
        Response::Unit(u) => u,
        other => panic!("{other:?}"),
    }
}

fn apply(s: &mut Session<'_>, ops: Vec<Op>) -> Vec<Option<EntityId>> {
    match s.handle(Request::Apply { ops }) {
        Response::Applied(r) => r,
        other => panic!("{other:?}"),
    }
}

/// 在 `unit` 里建一份暂存，写进 `data`。
fn staged(s: &mut Session<'_>, unit: UnitId, data: &[u8]) -> u64 {
    let Response::Staged(stage) = s.handle(Request::Stage { unit: Some(unit) }) else { panic!() };
    s.write_staged(stage, 0, data).unwrap();
    stage
}

fn find(s: &mut Session<'_>, query: Query) -> Vec<EntityId> {
    match s.handle(Request::Query { query, after: None, limit: 1000 }) {
        Response::Ids(ids) => ids,
        other => panic!("{other:?}"),
    }
}

fn units(s: &mut Session<'_>) -> Vec<UnitInfo> {
    match s.handle(Request::Units) {
        Response::Units(u) => u,
        other => panic!("{other:?}"),
    }
}

fn read(s: &Service, id: &EntityId) -> Vec<u8> {
    let mut buf = vec![0u8; 1000];
    let n = s.volume().read(&(), id, 0, &mut buf).unwrap();
    buf.truncate(n);
    buf
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

    let s = Service::start(&state).unwrap();
    let mut c = s.session();
    assert_eq!(attach(&mut c, "file", &img), UnitId([1; 16]));
    assert_eq!(attach(&mut c, "hostdir", &dir), UnitId([2; 16]));
    let hi = staged(&mut c, UnitId([1; 16]), b"hi");
    let created = apply(
        &mut c,
        vec![
            Op::CreateFrom { stage: hi, tags: vec![Tag::new("user", "n", "x")] },
            Op::Create { unit: Some(UnitId([2; 16])), tags: vec![Tag::new("user", "n", "y")] },
        ],
    );
    let x = created[0].unwrap();
    // 查询跨单元。
    assert_eq!(find(&mut c, Query::has("user", "n")).len(), 2);
    assert_eq!(find(&mut c, Query::All).len(), 3);
    assert_eq!(read(&s, &x), b"hi");
    assert_eq!(c.handle(Request::Check { unit: UnitId([2; 16]) }), Response::Done);
    // 暂存用过就没了。
    assert!(matches!(c.handle(Request::DiscardStage { stage: hi }), Response::Error(_)));
    // 同一个位置不能加两次。
    assert!(matches!(
        c.handle(Request::Attach { backend: "file".into(), location: img.to_string_lossy().into() }),
        Response::Error(_)
    ));
    assert!(matches!(c.handle(Request::Attach { backend: "nope".into(), location: "x".into() }), Response::Error(_)));
    drop(c);
    drop(s);

    // 重启：单元按记录重新上线。
    let s = Service::start(&state).unwrap();
    let mut c = s.session();
    assert!(units(&mut c).iter().all(|u| u.offline.is_none()));
    assert_eq!(find(&mut c, Query::All).len(), 3);
    assert_eq!(read(&s, &x), b"hi");
    drop(c);
    drop(s);

    // 镜像文件暂时不在：单元离线，回来后再列单元时重新上线。
    std::fs::rename(&img, base.join("away.img")).unwrap();
    let s = Service::start(&state).unwrap();
    let mut c = s.session();
    let listed = units(&mut c);
    assert!(listed.iter().any(|u| u.backend == "file" && u.offline.is_some() && u.id.is_none()));
    assert_eq!(find(&mut c, Query::has("user", "n")).len(), 1);
    std::fs::rename(base.join("away.img"), &img).unwrap();
    assert!(units(&mut c).iter().all(|u| u.offline.is_none()));
    assert_eq!(find(&mut c, Query::has("user", "n")).len(), 2);

    // 移出卷。
    assert_eq!(c.handle(Request::Detach { unit: UnitId([1; 16]) }), Response::Done);
    assert_eq!(units(&mut c).len(), 1);
    drop(c);
    drop(s);
    let s = Service::start(&state).unwrap();
    assert_eq!(units(&mut s.session()).len(), 1);
    std::fs::remove_dir_all(base).unwrap();
}

fn serve(base: &std::path::Path) -> String {
    let state = base.join("state");
    let sock = base.join("v.sock").to_string_lossy().into_owned();
    let listener = listen(tagfs_proto::to_name(sock.clone()).unwrap()).unwrap();
    std::thread::spawn(move || {
        let s = Arc::new(Service::start(&state).unwrap());
        s.serve(&listener).unwrap();
    });
    sock
}

fn connect(sock: &str) -> Client {
    Client::connect_to(tagfs_proto::to_name(sock.to_string()).unwrap()).unwrap()
}

#[test]
fn tools_talk_to_it_over_a_local_socket() {
    let base = scratch("socket");
    let sock = serve(&base);
    let mut c = connect(&sock);
    let Response::Unit(unit) = c.call(&Request::Attach { backend: "mem".into(), location: String::new() }).unwrap()
    else {
        panic!()
    };
    let Response::Staged(stage) = c.call(&Request::Stage { unit: Some(unit) }).unwrap() else { panic!() };
    assert_eq!(c.write_staged(stage, 0, &mut &b"abc"[..]).unwrap(), Response::Done);
    let resp = c.call(&Request::Apply { ops: vec![Op::CreateFrom { stage, tags: vec![Tag::flag("user", "f")] }] });
    let Response::Applied(ids) = resp.unwrap() else { panic!() };
    let id = ids[0].unwrap();
    assert_eq!(
        c.call(&Request::Query { query: Query::has("user", "f"), after: None, limit: 10 }).unwrap(),
        Response::Ids(vec![id])
    );
    let mut out = Vec::new();
    assert_eq!(c.read(id, 1, 10, &mut out).unwrap(), Ok(2));
    assert_eq!(out, b"bc");
    assert!(matches!(c.call(&Request::Get { id: EntityId([0; 16]) }).unwrap(), Response::Error(_)));
    assert!(c.read(EntityId([0; 16]), 0, 1, &mut out).unwrap().is_err());
    // 写出错时剩下的数据照样收完，连接还能接着用。
    assert!(matches!(c.write(EntityId([0; 16]), 0, &mut &[1u8; 3 << 20][..]).unwrap(), Response::Error(_)));
    assert!(matches!(c.call(&Request::Get { id }).unwrap(), Response::Entity(_)));
    // 另一个连接看到同一个卷。
    let mut c2 = connect(&sock);
    drop(c);
    assert!(matches!(c2.call(&Request::Get { id }).unwrap(), Response::Entity(_)));
    // 同一个名字上不能再起第二个卷。
    let again = listen(tagfs_proto::to_name(sock).unwrap());
    assert_eq!(again.err().map(|e| e.kind()), Some(io::ErrorKind::AddrInUse));
}

/// 一个慢源头：一段一段地给数据，给完一段要等对方放行。
struct Gate {
    left: usize,
    go: std::sync::mpsc::Receiver<()>,
}

impl io::Read for Gate {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Ok(0);
        }
        self.go.recv().unwrap();
        let n = buf.len().min(self.left).min(1 << 20);
        buf[..n].fill(7);
        self.left -= n;
        Ok(n)
    }
}

#[test]
fn a_long_transfer_does_not_hold_up_other_connections() {
    let base = scratch("parallel");
    let img = base.join("u.img");
    tagfs::image::create(&img, UnitId([5; 16])).unwrap();
    let sock = serve(&base);
    let mut a = connect(&sock);
    let Response::Unit(unit) =
        a.call(&Request::Attach { backend: "file".into(), location: img.to_string_lossy().into() }).unwrap()
    else {
        panic!()
    };
    let Response::Applied(ids) = a
        .call(&Request::Apply { ops: vec![Op::Create { unit: Some(unit), tags: vec![Tag::flag("user", "big")] }] })
        .unwrap()
    else {
        panic!()
    };
    let big = ids[0].unwrap();

    // 连接 a 往实体里写 40 MiB（比原来单次读写的上限大），中途停住；连接 b 照常工作。
    let (go, wait) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || a.write(big, 0, &mut Gate { left: 40 << 20, go: wait }).unwrap());
    go.send(()).unwrap();
    let mut b = connect(&sock);
    let Response::Applied(ids) = b
        .call(&Request::Apply { ops: vec![Op::Create { unit: Some(unit), tags: vec![Tag::flag("user", "small")] }] })
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        b.call(&Request::Query { query: Query::has("user", "small"), after: None, limit: 10 }).unwrap(),
        Response::Ids(vec![ids[0].unwrap()])
    );
    for _ in 0..40 {
        go.send(()).unwrap();
    }
    assert_eq!(writer.join().unwrap(), Response::Done);

    // 读回来也是分段的：只数字节，不攒在内存里。
    struct Count(u64, bool);
    impl io::Write for Count {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0 += b.len() as u64;
            self.1 &= b.iter().all(|&x| x == 7);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0, true);
    assert_eq!(b.read(big, 0, u64::MAX, &mut count).unwrap(), Ok(40 << 20));
    assert_eq!((count.0, count.1), (40 << 20, true));
    assert_eq!(b.call(&Request::Flush { id: big }).unwrap(), Response::Done);
}

#[test]
fn stages_belong_to_their_connection() {
    let base = scratch("stages");
    let sock = serve(&base);
    let mut a = connect(&sock);
    let Response::Unit(unit) = a.call(&Request::Attach { backend: "mem".into(), location: String::new() }).unwrap()
    else {
        panic!()
    };
    let Response::Staged(stage) = a.call(&Request::Stage { unit: Some(unit) }).unwrap() else { panic!() };
    // 别的连接用不了它。
    let mut b = connect(&sock);
    assert!(matches!(b.write_staged(stage, 0, &mut &b"x"[..]).unwrap(), Response::Error(_)));
    let resp = b.call(&Request::Apply { ops: vec![Op::CreateFrom { stage, tags: vec![] }] }).unwrap();
    assert!(matches!(resp, Response::Error(_)));
    // 连接断开，没用上的暂存丢掉。
    drop(a);
    let mut gone = || match b.call(&Request::Units).unwrap() {
        Response::Units(u) => u[0].details.iter().any(|(k, v)| k == "stages" && v == "0"),
        _ => false,
    };
    let start = std::time::Instant::now();
    while !gone() {
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(10));
    }
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
