//! 块格式单元放在宿主机文件上：写入、关闭、重新打开。

use tagfs_block::BlockUnit;
use tagfs_core::{Change, EntityId, Tag, Unit, UnitId};
use tagfs_device_host::FileDevice;

#[test]
fn a_unit_in_a_file_survives_reopening() {
    let path = std::env::temp_dir().join(format!("tagfs-unit-{}.img", std::process::id()));
    let id = EntityId([1; 16]);
    let content: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    {
        let dev = FileDevice::create(&path, 8 << 20, 4096).unwrap();
        let mut unit = BlockUnit::format(dev, UnitId([5; 16])).unwrap();
        unit.apply(&[Change::Create { id, tags: vec![Tag::new("user", "name", "big")], content: content.clone() }])
            .unwrap();
    }
    let unit = BlockUnit::open(FileDevice::open(&path, 4096, false).unwrap()).unwrap();
    unit.verify().unwrap();
    assert_eq!(unit.id(), UnitId([5; 16]));
    let snap = unit.snapshot();
    let rec = snap.get(&id).unwrap().unwrap();
    assert_eq!(rec.tags, vec![Tag::new("user", "name", "big")]);
    let mut buf = vec![0u8; rec.size as usize];
    snap.read(&id, 0, &mut buf).unwrap();
    assert_eq!(buf, content);
    drop(snap);
    drop(unit);
    std::fs::remove_file(path).unwrap();
}
