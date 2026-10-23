use std::io::{Cursor, Write};

use tagfs_core::Value;
use zip::write::SimpleFileOptions;

use super::*;

const UNIT: UnitId = UnitId([7; 16]);

/// 内存里的 zip：`(名字, 内容)`，名字以 `/` 结尾的是目录条目。
fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let time = zip::DateTime::from_date_and_time(2024, 3, 5, 6, 7, 8).unwrap();
    for (name, data) in entries {
        if let Some(dir) = name.strip_suffix('/') {
            w.add_directory(dir, SimpleFileOptions::default().last_modified_time(time)).unwrap();
        } else {
            let options = SimpleFileOptions::default().last_modified_time(time);
            w.start_file(*name, options).unwrap();
            w.write_all(data).unwrap();
        }
    }
    w.finish().unwrap().into_inner()
}

fn open(entries: &[(&str, &[u8])]) -> ZipUnit {
    ZipUnit::open(UNIT, Cursor::new(archive(entries))).unwrap()
}

fn value(rec: &EntityRecord, key: &str) -> Option<Value> {
    rec.tags.iter().find(|t| t.ns.is_tagfs() && t.key.as_str() == key).map(|t| t.value.clone())
}

fn find(unit: &ZipUnit, key: &str, path: &str) -> EntityRecord {
    let snap = unit.snapshot();
    let tag = Tag::new(Namespace::TAGFS, key, path);
    let ids = snap.probe(&Probe::Is(&tag)).unwrap().unwrap();
    assert_eq!(ids.len(), 1, "{key} {path}");
    snap.get(&ids[0]).unwrap().unwrap()
}

#[test]
fn entries_are_entities() {
    let big: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let unit = open(&[("manifest.yaml", b"id: x"), ("payload/web/", b""), ("payload/web/main.js", &big)]);
    let snap = unit.snapshot();
    // 三个目录（payload、payload/web 写明了，payload 是隐含的）和两个文件。
    assert_eq!(snap.all().unwrap().len(), 4);

    let manifest = find(&unit, ARCHIVE_PATH, "/manifest.yaml");
    assert_eq!(manifest.size, 5);
    let mut buf = [0u8; 16];
    assert_eq!(snap.read(&manifest.id, 0, &mut buf).unwrap(), 5);
    assert_eq!(&buf[..5], b"id: x");

    // 分段读出来和原样一致。
    let main = find(&unit, ARCHIVE_PATH, "/payload/web/main.js");
    assert_eq!(main.size, big.len() as u64);
    let mut out = Vec::new();
    let mut chunk = vec![0u8; 4096];
    loop {
        let n = snap.read(&main.id, out.len() as u64, &mut chunk).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..n]);
    }
    assert_eq!(out, big);

    let implied = find(&unit, ARCHIVE_DIR, "/payload");
    assert_eq!(value(&implied, ARCHIVE_MTIME), None, "an implied directory has no time of its own");
    let web = find(&unit, ARCHIVE_DIR, "/payload/web");
    // 2024-03-05 06:07:08 UTC
    assert_eq!(value(&web, ARCHIVE_MTIME), Some(Value::Uint(1_709_618_828_000_000_000)));
    assert_eq!(snap.read(&web.id, 0, &mut buf).unwrap(), 0);

    // 前缀查询找到一个目录下的东西。
    let prefix =
        Probe::Prefix { ns: &Namespace::tagfs(), key: &tagfs_core::Key::new(ARCHIVE_PATH), prefix: "/payload/" };
    assert_eq!(snap.probe(&prefix).unwrap().unwrap(), vec![main.id]);
}

#[test]
fn ids_depend_on_the_unit_and_the_path_only() {
    let entries: &[(&str, &[u8])] = &[("a/b.txt", b"b"), ("c.txt", b"c")];
    let a = open(entries);
    let b = open(entries);
    assert_eq!(a.snapshot().all().unwrap(), b.snapshot().all().unwrap());
    let other = ZipUnit::open(UnitId([8; 16]), Cursor::new(archive(entries))).unwrap();
    assert_ne!(find(&a, ARCHIVE_PATH, "/c.txt").id, find(&other, ARCHIVE_PATH, "/c.txt").id);
    // 同一路径的文件和目录编号不同。
    assert_ne!(entity_id(UNIT, 0, "/a"), entity_id(UNIT, 1, "/a"));
}

#[test]
fn nothing_changes_it() {
    let unit = open(&[("a.txt", b"a")]);
    let id = find(&unit, ARCHIVE_PATH, "/a.txt").id;
    assert_eq!(unit.write(&id, 0, b"x"), Err(READ_ONLY));
    assert_eq!(unit.set_len(&id, 0), Err(READ_ONLY));
    assert_eq!(unit.stage(), Err(READ_ONLY));
    assert_eq!(unit.apply(&[Change::Delete { id }]), Err(READ_ONLY));
    assert_eq!(unit.apply(&[]), Ok(()));
    assert!(unit.prepared().unwrap().is_empty());
    assert!(unit.committed().unwrap().is_empty());
    unit.check().unwrap();
}

#[test]
fn paths_must_stay_inside() {
    for bad in ["../x", "a/../../x", "a//b", "./a", "a\\b"] {
        let bytes = archive(&[(bad, b"x")]);
        assert!(matches!(ZipUnit::open(UNIT, Cursor::new(bytes)), Err(ZipError::InvalidPath(_))), "{bad}");
    }
}

#[test]
fn not_a_zip() {
    assert!(matches!(ZipUnit::open(UNIT, Cursor::new(b"hello".to_vec())), Err(ZipError::Format(_))));
}

#[test]
fn corruption_is_reported() {
    // 不压缩，内容紧跟在本地文件头（30 字节）和名字之后。
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    w.start_file("a.txt", SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)).unwrap();
    w.write_all(&[42u8; 1000]).unwrap();
    let mut bytes = w.finish().unwrap().into_inner();
    // 改坏内容中间的一个字节，CRC 对不上。
    bytes[500] ^= 0xff;
    let unit = ZipUnit::open(UNIT, Cursor::new(bytes)).unwrap();
    let id = find(&unit, ARCHIVE_PATH, "/a.txt").id;
    assert_eq!(unit.snapshot().read(&id, 0, &mut [0u8; 10]), Err(UnitError::Corrupt));
    assert_eq!(unit.check(), Err(UnitError::Corrupt));
}

#[test]
fn days_since_1970() {
    assert_eq!(days_from_civil(1970, 1, 1), 0);
    assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    assert_eq!(days_from_civil(2024, 3, 5), 19_787);
}
