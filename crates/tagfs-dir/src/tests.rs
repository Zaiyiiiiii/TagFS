use std::fs;
use std::path::PathBuf;

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
    let dir = std::env::temp_dir().join(format!("tagfs-dir-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
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
    let (unit, _) = DirUnit::create(&root, UnitId([3; 16]), 64 << 20, ids()).unwrap();
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

#[test]
fn existing_files_become_entities_with_their_paths() {
    let root = scratch("scan");
    fs::create_dir_all(root.join("music/live")).unwrap();
    fs::write(root.join("a.txt"), b"hello").unwrap();
    fs::write(root.join("music/live/song.mp3"), b"la la").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc", root.join("escape")).unwrap();

    let (unit, report) = DirUnit::create(&root, UnitId([1; 16]), 64 << 20, ids()).unwrap();
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

    // 写内容、删除。
    unit.apply(&[Change::Write { id: eid(1), offset: 3, data: b"!".to_vec() }]).unwrap();
    assert_eq!(fs::read(root.join("b.txt")).unwrap(), b"one!");
    unit.apply(&[Change::Delete { id: eid(3) }]).unwrap();
    assert!(!root.join("c.txt").exists());
    unit.image().verify().unwrap();
}

#[test]
fn copies_follow_the_target_path() {
    let (root, mut unit) = fresh("copy");
    unit.apply(&[Change::Create { id: eid(1), tags: vec![p("/src")], content: b"payload".to_vec() }]).unwrap();
    unit.apply(&[
        Change::Copy { from: eid(1), id: eid(2), tags: vec![p("/dst")] },
        Change::Copy { from: eid(1), id: eid(3), tags: vec![] },
    ])
    .unwrap();
    assert_eq!(fs::read(root.join("dst")).unwrap(), b"payload");
    assert_eq!(fs::read(root.join("src")).unwrap(), b"payload");
    assert_eq!(content(&unit, &eid(3)), b"payload");
    assert_eq!(image_size(&unit, &eid(1)), 0);
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
    assert_eq!(unit.snapshot().all().unwrap(), before);
    assert!(!root.join("same").exists());
}

#[test]
fn reopening_catches_up_with_changes_made_on_the_host() {
    let root = scratch("rescan");
    fs::write(root.join("keep"), b"k").unwrap();
    fs::write(root.join("gone"), b"g").unwrap();
    fs::write(root.join("old-name"), b"r").unwrap();
    let (mut unit, _) = DirUnit::create(&root, UnitId([1; 16]), 64 << 20, ids()).unwrap();
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
    #[cfg(unix)]
    {
        assert_eq!(by_path(&unit, "/new-name"), Some(renamed), "same entity after a rename on the host");
        assert_eq!(report, ScanReport { added: 1, removed: 1, renamed: 1, skipped: 0 });
    }
    #[cfg(not(unix))]
    let _ = (renamed, report);
}

/// 第 1 步提交之后、宿主操作做完之前断电：挂载时把剩下的做完。
#[test]
fn an_interrupted_change_is_finished_when_reopened() {
    for stop_after_host_ops in [false, true] {
        let (root, mut unit) = fresh(&format!("recover-{stop_after_host_ops}"));
        unit.apply(&[
            Change::Create { id: eid(1), tags: vec![p("/a")], content: b"A".to_vec() },
            Change::Create { id: eid(2), tags: vec![p("/b")], content: b"B".to_vec() },
        ])
        .unwrap();
        let changes = [
            Change::RemoveTag { id: eid(1), tag: p("/a") },
            Change::RemoveTag { id: eid(2), tag: p("/b") },
            Change::AddTag { id: eid(1), tag: p("/b") },
            Change::AddTag { id: eid(2), tag: p("/x/a") },
            Change::Create { id: eid(3), tags: vec![p("/c")], content: b"C".to_vec() },
        ];
        // 照 run 的第 1 步做，然后“断电”。
        let plan = unit.plan(&changes).unwrap();
        let mut all = plan.hydrate.clone();
        all.extend_from_slice(&changes);
        let aux = [Aux::Put(AUX_INTENT.to_vec(), encode_intent(&plan.items))];
        unit.image.commit_batch(&Batch { changes: &all, aux: &aux, ..Batch::default() }).unwrap();
        if stop_after_host_ops {
            unit.carry_out(&plan.items).unwrap();
        }
        drop(unit);

        let (unit, report) = DirUnit::open(&root, ids()).unwrap();
        assert_eq!(report, ScanReport::default(), "nothing left for the scan to fix");
        assert_eq!(fs::read(root.join("b")).unwrap(), b"A");
        assert_eq!(fs::read(root.join("x/a")).unwrap(), b"B");
        assert_eq!(fs::read(root.join("c")).unwrap(), b"C");
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
    let (dir, _) = DirUnit::create(&root, UnitId([1; 16]), 64 << 20, ids()).unwrap();
    let rules = Rules::<()>::new().writer("user", |_| true).modify(|_, _| true).create(|_| Some(vec![]));
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
