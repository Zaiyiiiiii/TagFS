use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tagfs_core::{Change, EntityId, IdSource, Tag, Unit, UnitError, UnitId};

use super::*;

struct Counter(u64);

impl IdSource for Counter {
    fn unix_millis(&mut self) -> u64 {
        1
    }
    fn fill_random(&mut self, buf: &mut [u8]) {
        self.0 += 1;
        buf.fill(0);
        buf[..8].copy_from_slice(&self.0.to_be_bytes());
    }
}

fn ids() -> Box<dyn IdSource> {
    Box::new(Counter(1000))
}

/// 每个测试自己的空目录。
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tagfs-hostdir-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::canonicalize(dir).unwrap()
}

fn eid(n: u64) -> EntityId {
    let mut b = [0u8; 16];
    b[8..].copy_from_slice(&n.to_be_bytes());
    EntityId(b)
}

fn p(v: &str) -> Tag {
    Tag::new("tagfs", "hostfs_path", v)
}

fn fresh(name: &str) -> (PathBuf, DirUnit) {
    let root = scratch(name);
    let (unit, _) = DirUnit::create(&root, UnitId([3; 16]), ids()).unwrap();
    (root, unit)
}

fn content(u: &dyn Unit, id: &EntityId) -> Vec<u8> {
    let s = u.snapshot();
    let size = s.get(id).unwrap().unwrap().size;
    let mut buf = vec![0u8; size as usize];
    assert_eq!(s.read(id, 0, &mut buf).unwrap(), size as usize);
    buf
}

/// 带某个路径的实体。
fn by_path(u: &dyn Unit, path: &str) -> Option<EntityId> {
    let s = u.snapshot();
    let tag = p(path);
    let hits = s.probe(&tagfs_core::Probe::Is(&tag)).unwrap().unwrap();
    assert!(hits.len() <= 1);
    hits.first().copied()
}

/// 镜像里存的内容长度：路径实体的内容不应留在镜像里。
fn image_size(u: &DirUnit, id: &EntityId) -> u64 {
    u.image().snapshot().get(id).unwrap().unwrap().size
}

/// 等宿主改动的通知到达并同步进来。
fn eventually(mut ok: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ok() {
        assert!(start.elapsed() < Duration::from_secs(5), "host change was not picked up");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn existing_files_become_entities_with_their_paths() {
    let root = scratch("scan");
    fs::create_dir_all(root.join("music/live")).unwrap();
    fs::write(root.join("a.txt"), b"hello").unwrap();
    fs::write(root.join("music/live/song.mp3"), b"la la").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc", root.join("escape")).unwrap();

    let (unit, report) = DirUnit::create(&root, UnitId([1; 16]), ids()).unwrap();
    assert_eq!(report.added, 2);
    #[cfg(unix)]
    assert_eq!(report.skipped, 1, "the symlink is neither mapped nor followed");
    let song = by_path(&unit, "/music/live/song.mp3").unwrap();
    assert_eq!(content(&unit, &song), b"la la");
    assert_eq!(unit.snapshot().get(&song).unwrap().unwrap().tags, vec![p("/music/live/song.mp3")]);
    assert_eq!(image_size(&unit, &song), 0, "content is not copied into the image");
    assert_eq!(unit.snapshot().all().unwrap().len(), 2);
}

#[test]
fn a_path_creates_the_file_and_missing_directories() {
    let (root, mut unit) = fresh("create");
    unit.apply(&[Change::Create {
        id: eid(1),
        tags: vec![p("/abc/bcd/1.file"), Tag::new("user", "n", "x")],
        content: b"data".to_vec(),
    }])
    .unwrap();
    assert_eq!(fs::read(root.join("abc/bcd/1.file")).unwrap(), b"data");
    assert_eq!(content(&unit, &eid(1)), b"data");
    assert_eq!(image_size(&unit, &eid(1)), 0);

    // 没有路径的实体存在镜像里，宿主上看不到。
    unit.apply(&[Change::Create { id: eid(2), tags: vec![], content: b"inside".to_vec() }]).unwrap();
    assert_eq!(content(&unit, &eid(2)), b"inside");
    let mut names: Vec<_> = fs::read_dir(&root).unwrap().map(|e| e.unwrap().file_name()).collect();
    names.sort();
    assert_eq!(names, [".tagfs", "abc"].map(std::ffi::OsString::from));
    assert!(!root.join(".tagfs/tmp").exists());
}

#[test]
fn writes_go_straight_to_the_host_file() {
    let (root, mut unit) = fresh("direct");
    // 一个 16 MiB 的宿主文件，写一个字节、截短、再写。
    let big = vec![7u8; 16 << 20];
    fs::write(root.join("big.bin"), &big).unwrap();
    unit.rescan().unwrap();
    let id = by_path(&unit, "/big.bin").unwrap();
    let image_before = unit.image().total_blocks();
    unit.apply(&[
        Change::Write { id, offset: 5, data: b"X".to_vec() },
        Change::Truncate { id, len: 10 << 20 },
        Change::Write { id, offset: (12 << 20) - 1, data: b"Y".to_vec() },
    ])
    .unwrap();
    let now = fs::read(root.join("big.bin")).unwrap();
    assert_eq!(now.len(), 12 << 20);
    assert_eq!(now[5], b'X');
    assert_eq!(now[(10 << 20) - 1], 7);
    assert!(now[10 << 20..(12 << 20) - 1].iter().all(|&b| b == 0));
    assert_eq!(now[(12 << 20) - 1], b'Y');
    assert_eq!(image_size(&unit, &id), 0, "nothing went through the image");
    assert_eq!(unit.image().total_blocks(), image_before, "the image did not have to grow");
}

#[test]
fn changing_the_path_tag_moves_removes_or_materialises_the_file() {
    let (root, mut unit) = fresh("move");
    unit.apply(&[
        Change::Create { id: eid(1), tags: vec![p("/a.txt")], content: b"one".to_vec() },
        Change::Create { id: eid(2), tags: vec![p("/b.txt")], content: b"two".to_vec() },
        Change::Create { id: eid(3), tags: vec![], content: b"three".to_vec() },
    ])
    .unwrap();

    // 改名：同一个实体，文件移过去，内容不进镜像。
    unit.apply(&[
        Change::RemoveTag { id: eid(1), tag: p("/a.txt") },
        Change::AddTag { id: eid(1), tag: p("/d/e/a.txt") },
    ])
    .unwrap();
    assert!(!root.join("a.txt").exists());
    assert_eq!(fs::read(root.join("d/e/a.txt")).unwrap(), b"one");
    assert_eq!(image_size(&unit, &eid(1)), 0);

    // 一次里对调两个路径。
    unit.apply(&[
        Change::RemoveTag { id: eid(1), tag: p("/d/e/a.txt") },
        Change::RemoveTag { id: eid(2), tag: p("/b.txt") },
        Change::AddTag { id: eid(1), tag: p("/b.txt") },
        Change::AddTag { id: eid(2), tag: p("/d/e/a.txt") },
    ])
    .unwrap();
    assert_eq!(fs::read(root.join("b.txt")).unwrap(), b"one");
    assert_eq!(fs::read(root.join("d/e/a.txt")).unwrap(), b"two");

    // 去掉路径：宿主文件删掉，内容进镜像。
    unit.apply(&[Change::RemoveTag { id: eid(2), tag: p("/d/e/a.txt") }]).unwrap();
    assert!(!root.join("d/e/a.txt").exists());
    assert_eq!(content(&unit, &eid(2)), b"two");

    // 给镜像里的实体加上路径：内容写到宿主，镜像里清掉。
    unit.apply(&[Change::AddTag { id: eid(3), tag: p("/c.txt") }]).unwrap();
    assert_eq!(fs::read(root.join("c.txt")).unwrap(), b"three");
    assert_eq!(image_size(&unit, &eid(3)), 0);

    // 写、改名在同一次里：写进原文件，再移过去。
    unit.apply(&[
        Change::Write { id: eid(1), offset: 3, data: b"!".to_vec() },
        Change::RemoveTag { id: eid(1), tag: p("/b.txt") },
        Change::AddTag { id: eid(1), tag: p("/f.txt") },
    ])
    .unwrap();
    assert_eq!(fs::read(root.join("f.txt")).unwrap(), b"one!");
    unit.apply(&[Change::Delete { id: eid(3) }]).unwrap();
    assert!(!root.join("c.txt").exists());
    unit.image().verify().unwrap();
}

#[test]
fn copies_follow_the_target_path() {
    let (root, mut unit) = fresh("copy");
    unit.apply(&[Change::Create { id: eid(1), tags: vec![p("/src")], content: b"payload".to_vec() }]).unwrap();
    unit.apply(&[
        Change::Write { id: eid(1), offset: 0, data: b"P".to_vec() },
        Change::Copy { from: eid(1), id: eid(2), tags: vec![p("/dst")] },
        Change::Copy { from: eid(1), id: eid(3), tags: vec![] },
    ])
    .unwrap();
    assert_eq!(fs::read(root.join("dst")).unwrap(), b"Payload");
    assert_eq!(fs::read(root.join("src")).unwrap(), b"Payload");
    assert_eq!(content(&unit, &eid(3)), b"Payload");
    assert_eq!(image_size(&unit, &eid(1)), 0);
    assert_eq!(image_size(&unit, &eid(2)), 0);

    // 复制之后再改源：复制品不受影响。
    unit.apply(&[
        Change::Copy { from: eid(1), id: eid(4), tags: vec![p("/dst2")] },
        Change::Write { id: eid(1), offset: 0, data: b"Q".to_vec() },
    ])
    .unwrap();
    assert_eq!(fs::read(root.join("dst2")).unwrap(), b"Payload");
    assert_eq!(fs::read(root.join("src")).unwrap(), b"Qayload");
    unit.image().verify().unwrap();
}

#[test]
fn bad_or_taken_paths_change_nothing() {
    let (root, mut unit) = fresh("guard");
    fs::write(root.join("outside-tagfs.txt"), b"x").unwrap();
    fs::write(root.join("file"), b"x").unwrap();
    unit.rescan().unwrap();
    let before = unit.snapshot().all().unwrap();
    let try_path = |unit: &mut DirUnit, v: &str| {
        unit.apply(&[
            Change::Create { id: eid(50), tags: vec![], content: vec![] },
            Change::Create { id: eid(51), tags: vec![p(v)], content: b"y".to_vec() },
        ])
    };
    for bad in ["/../x", "/a/../../x", "relative", "/.tagfs/unit.img", "/a//b", "/a/"] {
        assert_eq!(try_path(&mut unit, bad), Err(UnitError::InvalidPath), "{bad}");
    }
    assert_eq!(try_path(&mut unit, "/outside-tagfs.txt"), Err(UnitError::PathTaken));
    assert_eq!(try_path(&mut unit, "/file/below"), Err(UnitError::PathTaken));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(std::env::temp_dir(), root.join("link")).unwrap();
        assert_eq!(try_path(&mut unit, "/link/x"), Err(UnitError::InvalidPath));
    }
    // 一个实体两个路径，两个实体同一路径。
    assert_eq!(
        unit.apply(&[Change::Create { id: eid(52), tags: vec![p("/m"), p("/n")], content: vec![] }]),
        Err(UnitError::InvalidPath)
    );
    assert_eq!(
        unit.apply(&[
            Change::Create { id: eid(53), tags: vec![p("/same")], content: vec![] },
            Change::Create { id: eid(54), tags: vec![p("/same")], content: vec![] },
        ]),
        Err(UnitError::PathTaken)
    );
    // 删掉之后再写，和别的单元一样报“不存在”。
    let file = by_path(&unit, "/file").unwrap();
    assert_eq!(
        unit.apply(&[Change::Delete { id: file }, Change::Write { id: file, offset: 0, data: b"z".to_vec() }]),
        Err(UnitError::NotFound(file))
    );
    assert_eq!(unit.snapshot().all().unwrap(), before);
    assert!(!root.join("same").exists());
    assert!(root.join("file").exists());
}

#[test]
fn host_changes_show_up_right_away() {
    let (root, unit) = fresh("live");
    // Linux、Windows、macOS 都有文件系统通知；别的宿主可能没有。
    if cfg!(any(target_os = "linux", windows, target_os = "macos")) {
        assert!(unit.is_live());
    } else if !unit.is_live() {
        return;
    }
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::write(root.join("docs/new.txt"), b"hi").unwrap();
    eventually(|| by_path(&unit, "/docs/new.txt").is_some());
    let id = by_path(&unit, "/docs/new.txt").unwrap();
    assert_eq!(content(&unit, &id), b"hi");

    fs::rename(root.join("docs"), root.join("papers")).unwrap();
    eventually(|| by_path(&unit, "/papers/new.txt").is_some());
    assert_eq!(by_path(&unit, "/papers/new.txt"), Some(id), "renaming the directory keeps the entity");
    assert_eq!(by_path(&unit, "/docs/new.txt"), None);

    fs::remove_file(root.join("papers/new.txt")).unwrap();
    eventually(|| unit.snapshot().all().unwrap().is_empty());
}

#[test]
fn reopening_catches_up_with_changes_made_on_the_host() {
    let root = scratch("rescan");
    fs::write(root.join("keep"), b"k").unwrap();
    fs::write(root.join("gone"), b"g").unwrap();
    fs::write(root.join("old-name"), b"r").unwrap();
    let (mut unit, _) = DirUnit::create(&root, UnitId([1; 16]), ids()).unwrap();
    unit.apply(&[Change::Create { id: eid(9), tags: vec![], content: b"image only".to_vec() }]).unwrap();
    let keep = by_path(&unit, "/keep").unwrap();
    let renamed = by_path(&unit, "/old-name").unwrap();
    drop(unit);

    fs::remove_file(root.join("gone")).unwrap();
    fs::rename(root.join("old-name"), root.join("new-name")).unwrap();
    fs::write(root.join("fresh"), b"f").unwrap();
    let (unit, report) = DirUnit::open(&root, ids()).unwrap();
    assert_eq!(by_path(&unit, "/keep"), Some(keep));
    assert_eq!(by_path(&unit, "/gone"), None);
    assert!(by_path(&unit, "/fresh").is_some());
    assert_eq!(content(&unit, &eid(9)), b"image only");
    assert_eq!(by_path(&unit, "/new-name"), Some(renamed), "same entity after a rename on the host");
    assert_eq!(report, ScanReport { added: 1, removed: 1, renamed: 1, skipped: 0 });
}

#[test]
fn identities_follow_gits_rules() {
    let with_id = |id: u8, size, mtime_ns| Ident { file_id: Some(vec![id]), size, mtime_ns };
    let stat_only = |size, mtime_ns| Ident { file_id: None, size, mtime_ns };
    // 有文件号就只看文件号：改了内容的改名也认得出，号不同就不是同一个。
    assert!(with_id(1, 10, 5).same_file(&with_id(1, 99, 6)));
    assert!(!with_id(1, 10, 5).same_file(&with_id(2, 10, 5)));
    // 没有文件号时看 stat 信息，这种匹配要唯一才算。
    assert!(stat_only(10, 5).same_file(&with_id(1, 10, 5)));
    assert!(stat_only(10, 5).weak(&with_id(1, 10, 5)));
    assert!(!stat_only(10, 5).same_file(&stat_only(10, 6)));
    for i in [with_id(3, 1, 2), stat_only(4, 5)] {
        assert_eq!(Ident::decode(&i.encode()), Some(i));
    }
}

/// 第 1 步提交之后、宿主操作做完之前断电：挂载时把剩下的做完。
#[test]
fn an_interrupted_change_is_finished_when_reopened() {
    for stage in 0..3 {
        let (root, mut unit) = fresh(&format!("recover-{stage}"));
        unit.apply(&[
            Change::Create { id: eid(1), tags: vec![p("/a")], content: b"A".to_vec() },
            Change::Create { id: eid(2), tags: vec![p("/b")], content: b"B".to_vec() },
        ])
        .unwrap();
        let changes = [
            Change::Write { id: eid(1), offset: 1, data: b"1".to_vec() },
            Change::RemoveTag { id: eid(1), tag: p("/a") },
            Change::RemoveTag { id: eid(2), tag: p("/b") },
            Change::AddTag { id: eid(1), tag: p("/b") },
            Change::AddTag { id: eid(2), tag: p("/x/a") },
            Change::Create { id: eid(3), tags: vec![p("/c")], content: b"C".to_vec() },
            Change::Copy { from: eid(2), id: eid(4), tags: vec![p("/d")] },
        ];
        // 照 run 的第 1 步做，再按阶段做一部分宿主操作，然后“断电”。
        {
            let inner = unit.inner.get_mut();
            let plan = inner.plan(&changes).unwrap();
            assert!(plan.hydrate.is_empty());
            let aux = [Aux::Put(AUX_INTENT.to_vec(), plan.intent.encode())];
            inner.image.commit_batch(&mut Batch { changes: &plan.changes, aux: &aux, ..Batch::default() }).unwrap();
            if stage >= 1 {
                inner.carry_out(&plan.intent).unwrap();
            }
            if stage >= 2 {
                // 第 2 步做完以后再重做一遍，结果也一样。
                fs::create_dir_all(inner.tmp()).unwrap();
                inner.carry_out(&plan.intent).unwrap();
            }
        }
        drop(unit);

        let (unit, report) = DirUnit::open(&root, ids()).unwrap();
        assert_eq!(report, ScanReport::default(), "stage {stage}: nothing left for the scan to fix");
        assert_eq!(fs::read(root.join("b")).unwrap(), b"A1");
        assert_eq!(fs::read(root.join("x/a")).unwrap(), b"B");
        assert_eq!(fs::read(root.join("c")).unwrap(), b"C");
        assert_eq!(fs::read(root.join("d")).unwrap(), b"B");
        assert!(!root.join("a").exists());
        assert_eq!(image_size(&unit, &eid(3)), 0);
        assert!(unit.image().aux_get(AUX_INTENT).unwrap().is_none());
        assert!(!root.join(".tagfs/tmp").exists());
        unit.image().verify().unwrap();
    }
}

#[test]
fn works_inside_a_volume() {
    use tagfs_core::mem_unit::MemUnit;
    use tagfs_core::{Query, Rules, Volume};

    let root = scratch("volume");
    let (dir, _) = DirUnit::create(&root, UnitId([1; 16]), ids()).unwrap();
    let rules = Rules::<()>::new()
        .reader("user", |_| true)
        .reader("tagfs", |_| true)
        .writer("user", |_| true)
        .modify(|_, _| true)
        .create(|_| Some(vec![]));
    let mut v = Volume::new(rules, ids());
    v.attach(Box::new(MemUnit::new(UnitId([2; 16])))).unwrap();
    v.attach(Box::new(dir)).unwrap();

    let mut tx = v.begin(&());
    let a = tx.create(Some(UnitId([2; 16])), vec![Tag::new("user", "n", "a")], b"from memory".to_vec()).unwrap();
    tx.commit().unwrap();
    // 跨单元复制到目录单元里的一个路径上（两阶段提交）。
    let mut tx = v.begin(&());
    let b = tx.copy(&a, Some(UnitId([1; 16])), vec![p("/exports/a.txt")]).unwrap();
    tx.add_tag(&a, Tag::new("user", "copied_to", b.to_string())).unwrap();
    tx.commit().unwrap();
    assert_eq!(fs::read(root.join("exports/a.txt")).unwrap(), b"from memory");
    assert_eq!(v.query(&(), &Query::has("tagfs", "txn_pending")).unwrap(), vec![]);

    // 经卷改路径。
    let mut tx = v.begin(&());
    tx.remove_tag(&b, p("/exports/a.txt")).unwrap();
    tx.add_tag(&b, p("/renamed.txt")).unwrap();
    tx.commit().unwrap();
    assert!(root.join("renamed.txt").is_file());
    assert_eq!(v.query(&(), &Query::is("tagfs", "hostfs_path", "/renamed.txt")).unwrap(), vec![b]);
}
