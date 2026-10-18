//! TagFS 的对外接口。
//!
//! 操作系统用它建卷、接入存储单元、在事务里读写实体和 Tag、查询：
//!
//! ```no_run
//! use tagfs::{Query, Tag, Unrestricted, Volume};
//!
//! let unit = tagfs::create_image("/tmp/demo.img", tagfs::new_unit_id()).unwrap();
//! let mut vol = Volume::new(Unrestricted, Box::new(tagfs::SystemIds));
//! vol.attach(Box::new(unit)).unwrap();
//!
//! let mut tx = vol.begin(&());
//! let id = tx.create(None, vec![Tag::new("user", "name", "hello.txt")], b"hello".to_vec()).unwrap();
//! tx.commit().unwrap();
//! assert_eq!(vol.query(&(), &Query::is("user", "name", "hello.txt")).unwrap(), vec![id]);
//! ```
//!
//! 存储单元的种类：
//!
//! - 单文件（镜像文件）：[`create_image`]、[`open_image`]。放在宿主能访问的任何地方，按需扩容。
//! - 裸盘：[`open_disk`]，先用 [`format_disk`] 格式化。
//! - 目录：[`DirUnit`]，把宿主目录里按路径组织的文件映射成实体。
//! - 内存：[`memory_unit`]，测试和临时卷用。

use std::io;
use std::path::Path;

pub use tagfs_backend_hostdir::{DirError, DirUnit, HostPath, ScanReport};
pub use tagfs_block::{BlockUnit, OpenError};
pub use tagfs_core::tag::tagfs_keys;
pub use tagfs_core::{
    Change, Entity, EntityId, EntityRecord, Error, IdSource, Key, Namespace, Policy, Probe, Query, Rules, Snapshot,
    Tag, Txn, TxnId, TxnInfo, Unit, UnitError, UnitId, Value, Volume,
};
pub use tagfs_device::{BlockDevice, DeviceError, MemDevice};
pub use tagfs_device_host::FileDevice;

/// 块大小。
pub const BLOCK_SIZE: u32 = 4096;
/// 新建镜像文件的初始容量；不够时自动扩容。
const INITIAL_IMAGE: u64 = 4 << 20;

/// 打开或新建存储单元时的错误。
#[derive(Debug)]
pub enum UnitOpenError {
    Io(io::Error),
    Block(OpenError),
    Dir(DirError),
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

/// 在 `path` 新建一个单文件单元（覆盖已有文件）。
pub fn create_image(path: impl AsRef<Path>, unit: UnitId) -> Result<BlockUnit<FileDevice>, UnitOpenError> {
    let dev = FileDevice::create(path, INITIAL_IMAGE, BLOCK_SIZE)?;
    Ok(BlockUnit::format(dev, unit)?)
}

/// 打开单文件单元。
pub fn open_image(path: impl AsRef<Path>) -> Result<BlockUnit<FileDevice>, UnitOpenError> {
    Ok(BlockUnit::open(FileDevice::open(path, BLOCK_SIZE, false)?)?)
}

/// 格式化裸盘（整块盘或分区），原有内容全部丢失。
pub fn format_disk(path: impl AsRef<Path>, unit: UnitId) -> Result<BlockUnit<FileDevice>, UnitOpenError> {
    Ok(BlockUnit::format(FileDevice::open(path, BLOCK_SIZE, false)?, unit)?)
}

/// 打开裸盘上的单元。
pub fn open_disk(path: impl AsRef<Path>) -> Result<BlockUnit<FileDevice>, UnitOpenError> {
    open_image(path)
}

/// 内存单元，最大 `max_bytes` 字节，按需增长。
pub fn memory_unit(unit: UnitId, max_bytes: u64) -> BlockUnit<MemDevice> {
    let dev = MemDevice::growable(64 * BLOCK_SIZE as u64, max_bytes, BLOCK_SIZE);
    BlockUnit::format(dev, unit).expect("a fresh memory device formats")
}

/// 打开 `path` 上已有的单元：目录按目录单元打开，文件或设备按块格式打开。
pub fn open_unit(path: impl AsRef<Path>) -> Result<Box<dyn Unit>, UnitOpenError> {
    let path = path.as_ref();
    if std::fs::metadata(path)?.is_dir() {
        let (unit, _) = DirUnit::open(path, Box::new(SystemIds))?;
        Ok(Box::new(unit))
    } else {
        Ok(Box::new(open_image(path)?))
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
        let mut v = Volume::new(Unrestricted, Box::new(SystemIds));
        v.attach(unit).unwrap();
        v
    }

    #[test]
    fn an_image_unit_round_trips_through_reopening() {
        let path = std::env::temp_dir().join(format!("tagfs-facade-{}.img", std::process::id()));
        let unit = create_image(&path, new_unit_id()).unwrap();
        let mut v = volume_with(Box::new(unit));
        let mut tx = v.begin(&());
        let id = tx.create(None, vec![Tag::new("user", "name", "a")], vec![5; 10_000]).unwrap();
        tx.commit().unwrap();
        drop(v);

        let v = volume_with(open_unit(&path).unwrap());
        assert_eq!(v.get(&(), &id).unwrap().size, 10_000);
        assert_eq!(v.query(&(), &Query::has("user", "name")).unwrap(), vec![id]);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_cursor_walks_all_results() {
        let mut v = volume_with(Box::new(memory_unit(new_unit_id(), 64 << 20)));
        let mut tx = v.begin(&());
        let mut ids: Vec<_> =
            (0..10u64).map(|i| tx.create(None, vec![Tag::new("u", "n", i)], vec![]).unwrap()).collect();
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
    fn ids_are_unique() {
        let mut ids = SystemIds;
        let a = EntityId::generate(&mut ids);
        let b = EntityId::generate(&mut ids);
        assert_ne!(a, b);
        assert_ne!(new_unit_id(), new_unit_id());
    }
}
