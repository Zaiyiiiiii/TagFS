use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
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

fn ids() -> Box<dyn IdSource + Send> {
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

/// 新建实体的修改：内容先写进暂存。
fn new_entity(u: &dyn Unit, id: EntityId, tags: Vec<Tag>, data: &[u8]) -> Change {
    let stage = u.stage().unwrap();
    u.write_staged(stage, 0, data).unwrap();
    Change::Create { id, tags, content: Some(stage) }
}

fn create(u: &dyn Unit, id: EntityId, tags: Vec<Tag>, data: &[u8]) {
    let c = new_entity(u, id, tags, data);
    u.apply(&[c]).unwrap();
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

fn d(v: &str) -> Tag {
    Tag::new("tagfs", "hostfs_dir", v)
}

/// 带某个目录路径的实体。
fn by_dir(u: &dyn Unit, path: &str) -> Option<EntityId> {
    let s = u.snapshot();
    let tag = d(path);
    let hits = s.probe(&tagfs_core::Probe::Is(&tag)).unwrap().unwrap();
    assert!(hits.len() <= 1, "one entity per directory");
    hits.first().copied()
}

/// 去掉读的时候现取的修改时间，剩下存着的 Tag。
fn stored(tags: &[Tag]) -> Vec<Tag> {
    tags.iter().filter(|t| t.key.as_str() != HOSTFS_MTIME).cloned().collect()
}

/// 镜像里存的内容长度：路径实体的内容不应留在镜像里。
fn image_size(u: &DirUnit, id: &EntityId) -> u64 {
    u.image().snapshot().get(id).unwrap().unwrap().size
}

fn staged_files(root: &std::path::Path) -> usize {
    fs::read_dir(root.join(".tagfs/staged")).map_or(0, |d| d.count())
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
    assert_eq!(report.added, 4, "two files and two directories");
    #[cfg(unix)]
    assert_eq!(report.skipped, 1, "the symlink is neither mapped nor followed");
    let song = by_path(&unit, "/music/live/song.mp3").unwrap();
    assert_eq!(content(&unit, &song), b"la la");
    let tags = unit.snapshot().get(&song).unwrap().unwrap().tags;
    assert_eq!(stored(&tags), vec![p("/music/live/song.mp3")]);
    assert_eq!(image_size(&unit, &song), 0, "content is not copied into the image");
    assert!(by_dir(&unit, "/music").is_some() && by_dir(&unit, "/music/live").is_some());
    assert_eq!(unit.snapshot().all().unwrap().len(), 4);
}

#[test]
fn a_path_creates_the_file_and_missing_directories() {
    let (root, unit) = fresh("create");
    let c = new_entity(&unit, eid(1), vec![p("/abc/bcd/1.file"), Tag::new("user", "n", "x")], b"data");
    let Change::Create { content: Some(_stage), .. } = c else { unreachable!() };
    #[cfg(unix)]
    let staged_inode = std::os::unix::fs::MetadataExt::ino(&fs::metadata(unit.staged_file(_stage)).unwrap());
    unit.apply(&[c]).unwrap();
    assert_eq!(fs::read(root.join("abc/bcd/1.file")).unwrap(), b"data");
    // 暂存文件直接改名成了目标文件，没有复制。
    #[cfg(unix)]
    assert_eq!(std::os::unix::fs::MetadataExt::ino(&fs::metadata(root.join("abc/bcd/1.file")).unwrap()), staged_inode);
    assert_eq!(content(&unit, &eid(1)), b"data");
    assert_eq!(image_size(&unit, &eid(1)), 0);

    // 没有路径的实体存在镜像里，宿主上看不到。
    create(&unit, eid(2), vec![], b"inside");
    assert_eq!(content(&unit, &eid(2)), b"inside");
    let mut names: Vec<_> = fs::read_dir(&root).unwrap().map(|e| e.unwrap().file_name()).collect();
    names.sort();
    assert_eq!(names, [".tagfs", "abc"].map(std::ffi::OsString::from));
    assert!(!root.join(".tagfs/tmp").exists());
    assert_eq!(staged_files(&root), 0);
}

#[test]
fn writes_go_straight_to_the_host_file() {
    let (root, unit) = fresh("direct");
    // 一个 16 MiB 的宿主文件，写一个字节、截短、再写。
    let big = vec![7u8; 16 << 20];
    fs::write(root.join("big.bin"), &big).unwrap();
    unit.rescan().unwrap();
    let id = by_path(&unit, "/big.bin").unwrap();
    let image_before = unit.image().total_blocks();
    unit.write(&id, 5, b"X").unwrap();
    unit.set_len(&id, 10 << 20).unwrap();
    unit.write(&id, (12 << 20) - 1, b"Y").unwrap();
    unit.sync().unwrap();
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
    let (root, unit) = fresh("move");
    let changes = [
        new_entity(&unit, eid(1), vec![p("/a.txt")], b"one"),
        new_entity(&unit, eid(2), vec![p("/b.txt")], b"two"),
        new_entity(&unit, eid(3), vec![], b"three"),
    ];
    unit.apply(&changes).unwrap();

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

    // 写过再改名：写进原文件，移过去的是同一个文件。
    unit.write(&eid(1), 3, b"!").unwrap();
    unit.apply(&[Change::RemoveTag { id: eid(1), tag: p("/b.txt") }, Change::AddTag { id: eid(1), tag: p("/f.txt") }])
        .unwrap();
    assert_eq!(fs::read(root.join("f.txt")).unwrap(), b"one!");
    unit.apply(&[Change::Delete { id: eid(3) }]).unwrap();
    assert!(!root.join("c.txt").exists());
    unit.image().verify().unwrap();
}

#[test]
fn copies_follow_the_target_path() {
    let (root, unit) = fresh("copy");
    create(&unit, eid(1), vec![p("/src")], b"payload");
    unit.write(&eid(1), 0, b"P").unwrap();
    unit.apply(&[
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
    unit.apply(&[Change::Copy { from: eid(1), id: eid(4), tags: vec![p("/dst2")] }]).unwrap();
    unit.write(&eid(1), 0, b"Q").unwrap();
    assert_eq!(fs::read(root.join("dst2")).unwrap(), b"Payload");
    assert_eq!(fs::read(root.join("src")).unwrap(), b"Qayload");
    assert_eq!(content(&unit, &eid(3)), b"Payload");
    unit.image().verify().unwrap();
}

#[test]
fn bad_or_taken_paths_change_nothing() {
    let (root, unit) = fresh("guard");
    fs::write(root.join("outside-tagfs.txt"), b"x").unwrap();
    fs::write(root.join("file"), b"x").unwrap();
    unit.rescan().unwrap();
    let before = unit.snapshot().all().unwrap();
    let try_path = |unit: &DirUnit, v: &str| {
        let made = new_entity(unit, eid(51), vec![p(v)], b"y");
        let r = unit.apply(&[Change::Create { id: eid(50), tags: vec![], content: None }, made.clone()]);
        // 没用上的暂存原样留着，由调用者丢掉。
        let Change::Create { content: Some(stage), .. } = made else { unreachable!() };
        unit.discard_staged(stage).unwrap();
        r
    };
    for bad in ["/../x", "/a/../../x", "relative", "/.tagfs/unit.img", "/a//b", "/a/"] {
        assert_eq!(try_path(&unit, bad), Err(INVALID_PATH), "{bad}");
    }
    assert_eq!(try_path(&unit, "/outside-tagfs.txt"), Err(PATH_TAKEN));
    assert_eq!(try_path(&unit, "/file/below"), Err(PATH_TAKEN));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(std::env::temp_dir(), root.join("link")).unwrap();
        assert_eq!(try_path(&unit, "/link/x"), Err(INVALID_PATH));
    }
    // 一个实体两个路径，两个实体同一路径。
    assert_eq!(
        unit.apply(&[Change::Create { id: eid(52), tags: vec![p("/m"), p("/n")], content: None }]),
        Err(INVALID_PATH)
    );
    assert_eq!(
        unit.apply(&[
            Change::Create { id: eid(53), tags: vec![p("/same")], content: None },
            Change::Create { id: eid(54), tags: vec![p("/same")], content: None },
        ]),
        Err(PATH_TAKEN)
    );
    assert_eq!(unit.snapshot().all().unwrap(), before);
    assert!(!root.join("same").exists());
    assert_eq!(staged_files(&root), 0);
    // 删掉之后再写，和别的单元一样报“不存在”。
    let file = by_path(&unit, "/file").unwrap();
    unit.apply(&[Change::Delete { id: file }]).unwrap();
    assert!(!root.join("file").exists());
    assert_eq!(unit.write(&file, 0, b"z"), Err(UnitError::NotFound(file)));
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
    // 目录还在，它的实体也还在。
    eventually(|| unit.snapshot().all().unwrap().len() == 1);
    assert!(by_dir(&unit, "/papers").is_some());
    assert!(by_dir(&unit, "/docs").is_none());
}

#[test]
fn reopening_catches_up_with_changes_made_on_the_host() {
    let root = scratch("rescan");
    fs::write(root.join("keep"), b"k").unwrap();
    fs::write(root.join("gone"), b"g").unwrap();
    fs::write(root.join("old-name"), b"r").unwrap();
    let (unit, _) = DirUnit::create(&root, UnitId([1; 16]), ids()).unwrap();
    create(&unit, eid(9), vec![], b"image only");
    // 一份没用上的暂存：重新挂载时连同宿主上的暂存文件一起回收。
    let leftover = unit.stage().unwrap();
    unit.write_staged(leftover, 0, b"never used").unwrap();
    let keep = by_path(&unit, "/keep").unwrap();
    let renamed = by_path(&unit, "/old-name").unwrap();
    unit.sync().unwrap();
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
    assert_eq!(staged_files(&root), 0);
    assert_eq!(unit.write_staged(leftover, 0, b"x"), Err(UnitError::UnknownStage(leftover)));
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
        let (root, unit) = fresh(&format!("recover-{stage}"));
        let first = [new_entity(&unit, eid(1), vec![p("/a")], b"A"), new_entity(&unit, eid(2), vec![p("/b")], b"B")];
        unit.apply(&first).unwrap();
        unit.write(&eid(1), 1, b"1").unwrap();
        let changes = [
            Change::RemoveTag { id: eid(1), tag: p("/a") },
            Change::RemoveTag { id: eid(2), tag: p("/b") },
            Change::AddTag { id: eid(1), tag: p("/b") },
            Change::AddTag { id: eid(2), tag: p("/x/a") },
            new_entity(&unit, eid(3), vec![p("/c")], b"C"),
            Change::Copy { from: eid(2), id: eid(4), tags: vec![p("/d")] },
        ];
        // 照 run 的第 1 步做，再按阶段做一部分宿主操作，然后“断电”。
        {
            let plan = unit.plan(&changes).unwrap();
            assert!(plan.hydrate.is_empty() && plan.copy_in.is_empty() && plan.stage_in.is_empty());
            let aux = [Aux::Put(AUX_INTENT.to_vec(), plan.intent.encode())];
            unit.image.commit_batch(&Batch { changes: &changes, aux: &aux, ..Batch::default() }).unwrap();
            if stage >= 1 {
                unit.carry_out(&plan.intent).unwrap();
            }
            if stage >= 2 {
                // 第 2 步做完以后再重做一遍，结果也一样。
                fs::create_dir_all(unit.tmp()).unwrap();
                unit.carry_out(&plan.intent).unwrap();
            }
        }
        unit.sync().unwrap();
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
        assert_eq!(staged_files(&root), 0);
        unit.image().verify().unwrap();
    }
}

/// 同一个目录挂两次：各有各的锁句柄，和两个进程一样。
#[test]
fn two_mounts_of_one_directory_stay_consistent() {
    let (root, a) = fresh("two-mounts");
    fs::write(root.join("seen.txt"), b"by both").unwrap();
    // 两个“进程”的身份来源不能给出同样的编号。
    let (b, report) = DirUnit::open(&root, Box::new(Counter(5000))).unwrap();
    assert_eq!(report.added, 1);
    // a 自己也对账到这个文件：认得出 b 已经建了实体，不再建一个。
    a.rescan().unwrap();
    assert_eq!(a.snapshot().all().unwrap().len(), 1);
    let seen = by_path(&a, "/seen.txt").unwrap();
    assert_eq!(by_path(&b, "/seen.txt"), Some(seen));

    // 一边经路径建、改名，另一边看得到。
    create(&a, eid(1), vec![p("/dir/new.txt")], b"from a");
    assert_eq!(content(&b, &eid(1)), b"from a");
    b.apply(&[
        Change::RemoveTag { id: eid(1), tag: p("/dir/new.txt") },
        Change::AddTag { id: eid(1), tag: p("/moved.txt") },
    ])
    .unwrap();
    assert_eq!(fs::read(root.join("moved.txt")).unwrap(), b"from a");
    assert_eq!(by_path(&a, "/moved.txt"), Some(eid(1)));
    b.write(&eid(1), 0, b"FROM").unwrap();
    assert_eq!(content(&a, &eid(1)), b"FROM a");
    // 没有路径的实体在镜像里，两边共用。
    create(&b, eid(2), vec![], b"inside");
    assert_eq!(content(&a, &eid(2)), b"inside");

    // 等两边都收到这些改动的通知并对完账：没有多出来的实体（三个文件，一个空下来的目录 /dir）。
    eventually(|| {
        a.refresh().unwrap();
        b.refresh().unwrap();
        a.snapshot().all().unwrap().len() == 4 && b.snapshot().all().unwrap().len() == 4
    });
    assert!(by_dir(&a, "/dir").is_some());
    a.image().verify().unwrap();
}

#[test]
fn directories_are_entities_too() {
    let (root, unit) = fresh("dirs");
    // 建一个空目录，它留得住。
    unit.apply(&[Change::Create { id: eid(1), tags: vec![d("/empty/inner")], content: None }]).unwrap();
    assert!(root.join("empty/inner").is_dir());
    assert!(by_dir(&unit, "/empty").is_some(), "the parent made along the way is an entity too");
    assert_eq!(by_dir(&unit, "/empty/inner"), Some(eid(1)));
    unit.rescan().unwrap();
    assert_eq!(by_dir(&unit, "/empty/inner"), Some(eid(1)), "a rescan keeps it");

    // 目录没有内容，也不能同时是文件，也不能和别的目录、文件撞路径。
    assert_eq!(unit.write(&eid(1), 0, b"x"), Err(NOT_A_FILE));
    let both = Change::Create { id: eid(2), tags: vec![d("/x"), p("/y")], content: None };
    assert_eq!(unit.apply(&[both]), Err(INVALID_PATH));
    let again = Change::Create { id: eid(2), tags: vec![d("/empty/inner")], content: None };
    assert_eq!(unit.apply(&[again]), Err(PATH_TAKEN));
    create(&unit, eid(3), vec![p("/file")], b"f");
    let over = Change::Create { id: eid(2), tags: vec![d("/file")], content: None };
    assert_eq!(unit.apply(&[over]), Err(PATH_TAKEN));

    // 改目录的路径，连同里面的文件一起改：整个目录搬过去，旧目录删掉。
    create(&unit, eid(4), vec![p("/empty/inner/a.txt")], b"A");
    unit.apply(&[
        Change::RemoveTag { id: eid(1), tag: d("/empty/inner") },
        Change::AddTag { id: eid(1), tag: d("/moved") },
        Change::RemoveTag { id: eid(4), tag: p("/empty/inner/a.txt") },
        Change::AddTag { id: eid(4), tag: p("/moved/a.txt") },
    ])
    .unwrap();
    assert!(!root.join("empty/inner").exists());
    assert_eq!(fs::read(root.join("moved/a.txt")).unwrap(), b"A");
    assert_eq!(by_dir(&unit, "/moved"), Some(eid(1)));

    // 删目录：里面还有文件时目录留着，实体随即补回来。
    unit.apply(&[Change::Delete { id: eid(1) }]).unwrap();
    assert!(root.join("moved").is_dir());
    assert!(by_dir(&unit, "/moved").is_some());
    let moved = by_dir(&unit, "/moved").unwrap();
    unit.apply(&[Change::Delete { id: eid(4) }, Change::Delete { id: moved }]).unwrap();
    assert!(!root.join("moved").exists());
    assert_eq!(by_dir(&unit, "/moved"), None);
    unit.image().verify().unwrap();
}

#[test]
fn modification_times_come_from_the_host() {
    let (root, unit) = fresh("mtime");
    create(&unit, eid(1), vec![p("/a.txt")], b"a");
    let mtime = |id: &EntityId| {
        unit.snapshot().get(id).unwrap().unwrap().tags.iter().find_map(|t| match (t.key.as_str(), &t.value) {
            (HOSTFS_MTIME, tagfs_core::Value::Uint(ns)) => Some(*ns),
            _ => None,
        })
    };
    let host = |path: &str| {
        let m = fs::metadata(root.join(path)).unwrap().modified().unwrap();
        m.duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64
    };
    assert_eq!(mtime(&eid(1)), Some(host("a.txt")));
    // 写它就是改宿主上的修改时间；它不进镜像。
    let when = 1_600_000_000_000_000_000u64;
    unit.apply(&[Change::AddTag { id: eid(1), tag: Tag::new("tagfs", HOSTFS_MTIME, when) }]).unwrap();
    assert_eq!(host("a.txt"), when);
    assert_eq!(mtime(&eid(1)), Some(when));
    assert!(unit.image().snapshot().get(&eid(1)).unwrap().unwrap().tags.iter().all(|t| t.key.as_str() != HOSTFS_MTIME));
    // 目录也一样。
    unit.apply(&[Change::Create {
        id: eid(2),
        tags: vec![d("/dir"), Tag::new("tagfs", HOSTFS_MTIME, when)],
        content: None,
    }])
    .unwrap();
    assert_eq!(host("dir"), when);
    assert_eq!(mtime(&eid(2)), Some(when));
    // 没有路径的实体没有修改时间。
    create(&unit, eid(3), vec![], b"inside");
    assert_eq!(mtime(&eid(3)), None);
}

/// 一个挂载做到一半就不在了（意图记录还在）：另一个挂载下一次改动时把它做完。
#[test]
fn another_mount_finishes_an_interrupted_change() {
    let (root, a) = fresh("finish-for-other");
    create(&a, eid(1), vec![p("/a")], b"A");
    let (b, _) = DirUnit::open(&root, Box::new(Counter(5000))).unwrap();
    let changes = [Change::RemoveTag { id: eid(1), tag: p("/a") }, Change::AddTag { id: eid(1), tag: p("/z/a") }];
    // a 只做了第 1 步。
    let plan = a.plan(&changes).unwrap();
    let aux = [Aux::Put(AUX_INTENT.to_vec(), plan.intent.encode())];
    a.image.commit_batch(&Batch { changes: &changes, aux: &aux, ..Batch::default() }).unwrap();
    drop(a);
    create(&b, eid(2), vec![p("/b")], b"B");
    assert_eq!(fs::read(root.join("z/a")).unwrap(), b"A");
    assert!(!root.join("a").exists());
    assert!(b.image().aux_get(AUX_INTENT).unwrap().is_none());
    assert_eq!(fs::read(root.join("b")).unwrap(), b"B");
    b.image().verify().unwrap();
}

#[test]
fn works_inside_a_volume() {
    use tagfs_backend_mem::MemUnit;
    use tagfs_core::{Query, Rules, Volume};

    let root = scratch("volume");
    let (dir, _) = DirUnit::create(&root, UnitId([1; 16]), ids()).unwrap();
    let rules = Rules::<()>::new()
        .reader("user", |_| true)
        .reader("tagfs", |_| true)
        .writer("user", |_| true)
        .modify(|_, _| true)
        .create(|_| Some(vec![]));
    let v = Volume::new(rules, ids());
    v.attach(Arc::new(MemUnit::new(UnitId([2; 16])))).unwrap();
    v.attach(Arc::new(dir)).unwrap();

    let stage = v.stage(Some(UnitId([2; 16]))).unwrap();
    stage.write(0, b"from memory").unwrap();
    let mut tx = v.begin(&());
    let a = tx.create_from(stage, vec![Tag::new("user", "n", "a")]).unwrap();
    tx.commit().unwrap();
    // 跨单元复制到目录单元里的一个路径上（两阶段提交）。
    let mut tx = v.begin(&());
    let b = tx.copy(&a, Some(UnitId([1; 16])), vec![p("/exports/a.txt")]).unwrap();
    tx.add_tag(&a, Tag::new("user", "copied_to", b.to_string())).unwrap();
    tx.commit().unwrap();
    assert_eq!(fs::read(root.join("exports/a.txt")).unwrap(), b"from memory");
    assert_eq!(v.query(&(), &Query::has("tagfs", "txn_pending")).unwrap(), vec![]);
    assert_eq!(staged_files(&root), 0);

    // 经卷写内容、改路径。
    v.write(&(), &b, 0, b"FROM").unwrap();
    let mut tx = v.begin(&());
    tx.remove_tag(&b, p("/exports/a.txt")).unwrap();
    tx.add_tag(&b, p("/renamed.txt")).unwrap();
    tx.commit().unwrap();
    assert_eq!(fs::read(root.join("renamed.txt")).unwrap(), b"FROM memory");
    assert_eq!(v.query(&(), &Query::is("tagfs", "hostfs_path", "/renamed.txt")).unwrap(), vec![b]);
}
