//! 真的多个进程同时用一个镜像文件：测试程序把自己作为子进程再启动一次（`TAGFS_CHILD` 说明它扮演什么）。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tagfs_core::{Change, EntityId, Snapshot, Tag, Unit, UnitId};

fn eid(process: u8, n: u64) -> EntityId {
    let mut b = [0u8; 16];
    b[0] = process;
    b[8..].copy_from_slice(&n.to_be_bytes());
    EntityId(b)
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tagfs-processes-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn content(s: &dyn Snapshot, id: &EntityId) -> Option<Vec<u8>> {
    let size = s.get(id).unwrap()?.size;
    let mut buf = vec![0u8; size as usize];
    assert_eq!(s.read(id, 0, &mut buf).unwrap(), size as usize);
    Some(buf)
}

/// 一个进程的工作：建实体（1 号起）、改自己的 0 号实体的内容、读另一个进程的实体。
fn work(unit: &dyn Unit, me: u8, other: u8, rounds: u64) {
    for i in 1..rounds {
        let stage = unit.stage().unwrap();
        unit.write_staged(stage, 0, &vec![me; 3000 + i as usize]).unwrap();
        unit.apply(&[Change::Create {
            id: eid(me, i),
            tags: vec![Tag::new("test", "by", me as u64)],
            content: Some(stage),
        }])
        .unwrap();
        unit.write(&eid(me, 0), (i - 1) * 100, &[me; 100]).unwrap();
        // 另一个进程的实体：看到的总是完整提交过的内容。
        let s = unit.snapshot();
        for j in 1..rounds {
            match content(&*s, &eid(other, j)) {
                Some(c) => assert_eq!(c, vec![other; 3000 + j as usize]),
                None => break,
            }
        }
    }
    unit.sync().unwrap();
}

fn child(role: &str, image: &Path) {
    let unit = tagfs_backend_file::open(image).unwrap();
    match role {
        "work" => work(&unit, 2, 1, 60),
        "crash" => {
            create(&unit, eid(2, 1), b"committed");
            let stage = unit.stage().unwrap();
            unit.write_staged(stage, 0, &[9; 50_000]).unwrap();
            unit.sync().unwrap();
            // 没提交的内容写入，拿着写者锁直接退出：不放锁、不删属主文件，和崩溃一样。
            unit.write(&eid(2, 1), 0, b"lost").unwrap();
            std::process::exit(0);
        }
        _ => unreachable!("unknown role {role}"),
    }
}

fn create(unit: &dyn Unit, id: EntityId, data: &[u8]) {
    let stage = unit.stage().unwrap();
    unit.write_staged(stage, 0, data).unwrap();
    unit.apply(&[Change::Create { id, tags: vec![], content: Some(stage) }]).unwrap();
}

/// 以 `role` 启动子进程，跑同一个测试。
fn spawn(test: &str, role: &str, image: &Path) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env("TAGFS_CHILD", role)
        .env("TAGFS_IMAGE", image)
        .stdout(Stdio::null())
        .spawn()
        .unwrap()
}

/// 在子进程里时扮演子进程，返回 `true`。
fn as_child() -> bool {
    match std::env::var("TAGFS_CHILD") {
        Ok(role) => {
            child(&role, Path::new(&std::env::var("TAGFS_IMAGE").unwrap()));
            true
        }
        Err(_) => false,
    }
}

#[test]
fn two_processes_work_on_one_image() {
    if as_child() {
        return;
    }
    let dir = scratch("work");
    let image = dir.join("u.img");
    let unit = tagfs_backend_file::create(&image, UnitId([1; 16])).unwrap();
    create(&unit, eid(1, 0), b"");
    create(&unit, eid(2, 0), b"");
    let mut other = spawn("two_processes_work_on_one_image", "work", &image);
    work(&unit, 1, 2, 60);
    assert!(other.wait().unwrap().success(), "the other process failed");

    let s = unit.snapshot();
    for p in [1u8, 2] {
        for i in 1..60 {
            assert_eq!(content(&*s, &eid(p, i)).unwrap(), vec![p; 3000 + i as usize]);
        }
        assert_eq!(content(&*s, &eid(p, 0)).unwrap(), vec![p; 5900]);
    }
    drop(s);
    unit.verify().unwrap();
    drop(unit);
    // 都关掉之后再打开：落盘的就是最后的样子。
    let again = tagfs_backend_file::open(&image).unwrap();
    again.verify().unwrap();
    assert_eq!(content(&*again.snapshot(), &eid(2, 59)).unwrap(), vec![2; 3059]);
    drop(again);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_process_that_dies_holding_the_writer_lock_does_not_block_others() {
    if as_child() {
        return;
    }
    let dir = scratch("crash");
    let image = dir.join("u.img");
    let unit = tagfs_backend_file::create(&image, UnitId([1; 16])).unwrap();
    let mut other = spawn("a_process_that_dies_holding_the_writer_lock_does_not_block_others", "crash", &image);
    assert!(other.wait().unwrap().success());
    // 它的锁随进程一起没了：这里照样能改。没提交的写入丢了，提交过的还在。
    create(&unit, eid(1, 1), b"after");
    let s = unit.snapshot();
    assert_eq!(content(&*s, &eid(2, 1)).unwrap(), b"committed");
    assert_eq!(content(&*s, &eid(1, 1)).unwrap(), b"after");
    drop(s);
    // 它落了盘的暂存，下一个打开的进程替它收拾。
    assert_eq!(unit.stages().unwrap().len(), 1);
    let next = tagfs_backend_file::open(&image).unwrap();
    assert!(next.stages().unwrap().is_empty());
    next.verify().unwrap();
    unit.verify().unwrap();
    drop((unit, next));
    std::fs::remove_dir_all(dir).unwrap();
}
