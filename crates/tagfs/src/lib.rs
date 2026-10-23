//! TagFS 的对外接口。
//!
//! 操作系统用它建卷、接入存储单元、在事务里读写实体和 Tag、查询：
//!
//! ```no_run
//! use std::sync::Arc;
//! use tagfs::{Query, Tag, Unrestricted, Volume};
//!
//! let unit = tagfs::image::create("/tmp/demo.img", tagfs::new_unit_id()).unwrap();
//! let vol = Volume::new(Unrestricted, Box::new(tagfs::SystemIds));
//! vol.attach(Arc::new(unit)).unwrap();
//!
//! // 内容先写进暂存，随新实体一起出现。
//! let stage = vol.stage(None).unwrap();
//! stage.write(0, b"hello").unwrap();
//! let mut tx = vol.begin(&());
//! let id = tx.create_from(stage, vec![Tag::new("user", "name", "hello.txt")]).unwrap();
//! tx.commit().unwrap();
//! assert_eq!(vol.query(&(), &Query::is("user", "name", "hello.txt")).unwrap(), vec![id]);
//!
//! // 之后的读写直接对内容进行，不经过事务。
//! vol.write(&(), &id, 5, b", world").unwrap();
//! vol.flush(&(), &id).unwrap();
//! ```
//!
//! 存储单元由各后端提供，逻辑层不知道有哪些后端：
//!
//! - [`image`]：单文件后端。单元存在一个镜像文件里，放在宿主能访问的任何地方，按需扩容。
//! - [`disk`]：裸盘后端。单元直接放在整块盘或分区上。
//! - [`hostdir`]：宿主目录后端（路径目录兼容层），把宿主目录里按路径组织的文件映射成实体。
//! - [`mem`]：内存后端，不持久化。
//! - [`archive`]：zip 后端，把一个 zip 归档只读地挂成单元（软件包、离线分发的资料）。归档里没有单元编号，
//!   由调用者给，所以不在 [`open_unit`] 认的范围里。

use std::io;
use std::path::Path;

pub use tagfs_backend_disk as disk;
pub use tagfs_backend_disk::DiskUnit;
pub use tagfs_backend_file as image;
pub use tagfs_backend_file::{ImageError, ImageUnit};
pub use tagfs_backend_hostdir as hostdir;
pub use tagfs_backend_hostdir::{DirError, DirUnit};
pub use tagfs_backend_mem as mem;
pub use tagfs_backend_mem::MemUnit;
pub use tagfs_backend_zip as archive;
pub use tagfs_backend_zip::{ZipError, ZipUnit};
pub use tagfs_block::{BlockUnit, Hold, OpenError, Ticker};
pub use tagfs_core::{
    Change, Entity, EntityId, EntityRecord, Error, IdSource, Key, Namespace, Policy, Probe, Query, Rules, Snapshot,
    Stage, StageId, Tag, Txn, TxnId, TxnInfo, Unit, UnitError, UnitId, Value, Volume,
};
pub use tagfs_device::{BlockDevice, DeviceError, MemDevice};
pub use tagfs_device_host::{FileDevice, lock_dir};

/// 块大小。
pub const BLOCK_SIZE: u32 = tagfs_backend_file::BLOCK_SIZE;

/// 打开或新建存储单元时的错误。
#[derive(Debug)]
pub enum UnitOpenError {
    /// 没有哪个后端认得这个路径上的东西。
    Unrecognized,
    Io(io::Error),
    Block(OpenError),
    Dir(DirError),
}

impl From<disk::DiskError> for UnitOpenError {
    fn from(e: disk::DiskError) -> Self {
        match e {
            disk::DiskError::Io(e) => UnitOpenError::Io(e),
            disk::DiskError::Format(e) => UnitOpenError::Block(e),
        }
    }
}

impl From<ImageError> for UnitOpenError {
    fn from(e: ImageError) -> Self {
        match e {
            ImageError::Io(e) => UnitOpenError::Io(e),
            ImageError::Format(e) => UnitOpenError::Block(e),
        }
    }
}

impl From<io::Error> for UnitOpenError {
    fn from(e: io::Error) -> Self {
        UnitOpenError::Io(e)
    }
}

impl From<OpenError> for UnitOpenError {
    fn from(e: OpenError) -> Self {
        UnitOpenError::Block(e)
    }
}

impl From<DirError> for UnitOpenError {
    fn from(e: DirError) -> Self {
        UnitOpenError::Dir(e)
    }
}

impl std::fmt::Display for UnitOpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnitOpenError::Unrecognized => f.write_str("not a TagFS unit"),
            UnitOpenError::Io(e) => write!(f, "{e}"),
            UnitOpenError::Block(e) => write!(f, "{e}"),
            UnitOpenError::Dir(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for UnitOpenError {}

/// 用系统时钟和系统随机数生成身份。
pub struct SystemIds;

impl IdSource for SystemIds {
    fn unix_millis(&mut self) -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
    }

    fn fill_random(&mut self, buf: &mut [u8]) {
        getrandom::fill(buf).expect("system random number generator");
    }
}

/// 新的单元身份（随机）。
pub fn new_unit_id() -> UnitId {
    let mut b = [0u8; 16];
    SystemIds.fill_random(&mut b);
    UnitId(b)
}

/// 打开 `path` 上已有的单元：依次问各后端是否认得它，由认得的后端打开。调用者拿到的都是 [`Unit`]，
/// 之后的使用和单元种类无关。
pub fn open_unit(path: impl AsRef<Path>) -> Result<Box<dyn Unit>, UnitOpenError> {
    let path = path.as_ref();
    if hostdir::recognizes(path) {
        let (unit, _) = DirUnit::open(path, Box::new(SystemIds))?;
        Ok(Box::new(unit))
    } else if image::recognizes(path) {
        Ok(Box::new(image::open(path)?))
    } else if disk::recognizes(path) {
        Ok(Box::new(disk::open(path)?))
    } else {
        Err(UnitOpenError::Unrecognized)
    }
}

/// 不设任何限制的策略：没有边界，什么都能读、写、改、建。给管理工具和测试用；操作系统应当提供自己的策略。
pub struct Unrestricted;

impl Policy for Unrestricted {
    type Context = ();

    fn is_boundary(&self, _: &Namespace, _: &Key) -> bool {
        false
    }

    fn crosses(&self, _: &(), _: &Namespace, _: &Key, _: &[&Value]) -> bool {
        true
    }

    fn can_read(&self, _: &(), _: &Namespace) -> bool {
        true
    }

    fn can_write(&self, _: &(), _: &Namespace) -> bool {
        true
    }

    fn can_modify(&self, _: &(), _: &[Tag]) -> bool {
        true
    }

    fn on_create(&self, _: &()) -> Option<Vec<Tag>> {
        Some(Vec::new())
    }
}

/// 查询游标：按 EntityId 顺序一页一页取结果。游标不持有卷，两页之间可以照常修改卷。
pub struct Cursor {
    query: Query,
    after: Option<EntityId>,
    page: usize,
    done: bool,
}

impl Cursor {
    pub fn new(query: Query, page: usize) -> Self {
        Cursor { query, after: None, page: page.max(1), done: false }
    }

    /// 下一页；没有更多结果时返回空。
    pub fn next_page<P: Policy>(&mut self, vol: &Volume<P>, ctx: &P::Context) -> Result<Vec<EntityId>, Error> {
        if self.done {
            return Ok(Vec::new());
        }
        let ids = vol.query_page(ctx, &self.query, self.after.as_ref(), self.page)?;
        self.done = ids.len() < self.page;
        self.after = ids.last().copied().or(self.after);
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume_with(unit: Box<dyn Unit>) -> Volume<Unrestricted> {
        let v = Volume::new(Unrestricted, Box::new(SystemIds));
        v.attach(unit.into()).unwrap();
        v
    }

    #[test]
    fn an_image_unit_round_trips_through_reopening() {
        let path = std::env::temp_dir().join(format!("tagfs-facade-{}.img", std::process::id()));
        let unit = image::create(&path, new_unit_id()).unwrap();
        let v = volume_with(Box::new(unit));
        let stage = v.stage(None).unwrap();
        stage.write(0, &[5; 10_000]).unwrap();
        let mut tx = v.begin(&());
        let id = tx.create_from(stage, vec![Tag::new("user", "name", "a")]).unwrap();
        tx.commit().unwrap();
        v.write(&(), &id, 10_000, &[6; 5_000]).unwrap();
        v.flush(&(), &id).unwrap();
        drop(v);

        let v = volume_with(open_unit(&path).unwrap());
        assert_eq!(v.get(&(), &id).unwrap().size, 15_000);
        assert_eq!(v.query(&(), &Query::has("user", "name")).unwrap(), vec![id]);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_cursor_walks_all_results() {
        let v = volume_with(Box::new(MemUnit::new(new_unit_id())));
        let mut tx = v.begin(&());
        let mut ids: Vec<_> = (0..10u64).map(|i| tx.create(None, vec![Tag::new("u", "n", i)]).unwrap()).collect();
        tx.commit().unwrap();
        ids.sort();
        let mut c = Cursor::new(Query::has("u", "n"), 4);
        let mut seen = Vec::new();
        loop {
            let page = c.next_page(&v, &()).unwrap();
            if page.is_empty() {
                break;
            }
            seen.extend(page);
        }
        assert_eq!(seen, ids);
    }

    #[test]
    fn units_are_opened_by_the_backend_that_recognises_them() {
        let base = std::env::temp_dir().join(format!("tagfs-facade-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("plain")).unwrap();
        std::fs::create_dir_all(base.join("mapped")).unwrap();
        std::fs::write(base.join("not-a-unit"), b"hello").unwrap();
        image::create(base.join("u.img"), UnitId([1; 16])).unwrap();
        DirUnit::create(base.join("mapped"), UnitId([2; 16]), Box::new(SystemIds)).unwrap();

        assert_eq!(open_unit(base.join("u.img")).unwrap().id(), UnitId([1; 16]));
        assert_eq!(open_unit(base.join("mapped")).unwrap().id(), UnitId([2; 16]));
        for nothing in ["plain", "not-a-unit", "missing"] {
            assert!(matches!(open_unit(base.join(nothing)), Err(UnitOpenError::Unrecognized)), "{nothing}");
        }
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn a_zip_archive_is_a_read_only_unit() {
        let path = std::env::temp_dir().join(format!("tagfs-facade-{}.zip", std::process::id()));
        {
            use std::io::Write;
            let mut w = zip_writer(&path);
            w.start_file("payload/web/main.js", zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(b"console.log(1)").unwrap();
            w.finish().unwrap();
        }
        let v = volume_with(Box::new(ZipUnit::open_file(UnitId([3; 16]), &path).unwrap()));
        let q =
            Query::Prefix { ns: Namespace::tagfs(), key: Key::new(archive::ARCHIVE_PATH), prefix: "/payload/".into() };
        let ids = v.query(&(), &q).unwrap();
        assert_eq!(ids.len(), 1);
        let mut buf = [0u8; 64];
        let n = v.read(&(), &ids[0], 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"console.log(1)");
        // 卷照常拒绝改它。
        let mut tx = v.begin(&());
        assert!(tx.add_tag(&ids[0], Tag::new("user", "x", "y")).is_err() || tx.commit().is_err());
        assert!(v.write(&(), &ids[0], 0, b"x").is_err());
        drop(v);
        std::fs::remove_file(path).unwrap();
    }

    fn zip_writer(path: &Path) -> zip::ZipWriter<std::fs::File> {
        zip::ZipWriter::new(std::fs::File::create(path).unwrap())
    }

    #[test]
    fn ids_are_unique() {
        let mut ids = SystemIds;
        let a = EntityId::generate(&mut ids);
        let b = EntityId::generate(&mut ids);
        assert_ne!(a, b);
        assert_ne!(new_unit_id(), new_unit_id());
    }
}
