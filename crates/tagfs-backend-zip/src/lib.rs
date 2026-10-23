//! zip 单元：把一个 zip 归档只读地挂成存储单元。
//!
//! 软件包、离线分发的资料这类东西，交换格式就是 zip：压缩、通用、几十年不变，各种工具都能打开和检查。
//! 不必为了在 TagFS 里用它先解开成文件，挂上就是一个单元，条目就是实体：
//!
//! - 每个文件条目是一个实体，带 `tagfs:archive_path`，值是它在归档里的路径，以 `/` 开头，
//!   如 `/payload/web/main.js`；内容就是解压后的条目。
//! - 每个目录（归档里写明的，以及文件路径上隐含的）也是一个实体，带 `tagfs:archive_dir`，没有内容。
//!   归档的根不是实体。
//! - 条目记着修改时间时，带 `tagfs:archive_mtime`（自 1970 年起的纳秒；zip 的时间不带时区，按 UTC 算）。
//!
//! 归档从任何能读、能定位的来源打开（[`Source`]），不一定是宿主上的文件：比如另一个单元里的实体。
//!
//! 单元是只读的：元数据修改、内容写入、暂存都被拒绝（[`READ_ONLY`]）。没有事务，也就没有要恢复的。
//!
//! 实体编号由单元编号和路径算出来，同一个归档用同一个单元编号打开，编号总是一样的。
//!
//! 压缩的条目没法从中间读起，读一个条目时整个解压出来，最近读过的几个留在内存里，分段读就不用反复解压。

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read, Seek};
use std::path::Path;
use std::sync::Arc;

use tagfs_core::tag::MAX_STR_LEN;
use tagfs_core::{
    Change, EntityId, EntityRecord, Namespace, Probe, Snapshot, StageId, Tag, TxnId, TxnInfo, Unit, UnitError, UnitId,
};
use tagfs_sync::Mutex;
use zip::ZipArchive;

/// zip 单元映射的 `tagfs` 键：文件条目在归档里的路径。
pub const ARCHIVE_PATH: &str = "archive_path";
/// zip 单元映射的 `tagfs` 键：归档里的一个目录。这样的实体没有内容。
pub const ARCHIVE_DIR: &str = "archive_dir";
/// zip 单元映射的 `tagfs` 键：条目的修改时间，uint，自 1970 年起的纳秒。
pub const ARCHIVE_MTIME: &str = "archive_mtime";

/// zip 单元不能改。
pub const READ_ONLY: UnitError = UnitError::Rejected("an archive unit is read-only");

/// 解压后留在内存里的条目数。
const CACHED: usize = 4;

/// 归档的来源。
pub trait Source: Read + Seek + Send {}

impl<T: Read + Seek + Send> Source for T {}

/// 打开归档时的错误。
#[derive(Debug)]
pub enum ZipError {
    Io(io::Error),
    /// 不是 zip，或者 zip 本身坏了。
    Format(String),
    /// 有条目的路径不合法：空段、`.`、`..`、反斜杠、NUL，或者太长。这样的归档整个不挂。
    InvalidPath(String),
}

impl fmt::Display for ZipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ZipError::Io(e) => write!(f, "{e}"),
            ZipError::Format(e) => write!(f, "not a usable zip archive: {e}"),
            ZipError::InvalidPath(p) => write!(f, "invalid path in zip archive: {p:?}"),
        }
    }
}

impl std::error::Error for ZipError {}

impl From<zip::result::ZipError> for ZipError {
    fn from(e: zip::result::ZipError) -> Self {
        match e {
            zip::result::ZipError::Io(e) => ZipError::Io(e),
            e => ZipError::Format(e.to_string()),
        }
    }
}

/// 一个实体：文件条目（`index` 是它在归档里的序号）或目录。
struct Item {
    tags: Vec<Tag>,
    size: u64,
    index: Option<usize>,
}

struct Inner {
    items: BTreeMap<EntityId, Item>,
    archive: Mutex<ZipArchive<Box<dyn Source>>>,
    /// 最近解压过的条目，最近的在前。
    cache: Mutex<Vec<(EntityId, Arc<Vec<u8>>)>>,
}

pub struct ZipUnit {
    id: UnitId,
    inner: Arc<Inner>,
}

/// 归档里的路径换成 Tag 的值：`a/b/c` → `/a/b/c`。目录条目末尾的 `/` 去掉。
fn path_value(name: &str) -> Result<(String, Vec<String>), ZipError> {
    let invalid = || ZipError::InvalidPath(name.to_string());
    let trimmed = name.strip_suffix('/').unwrap_or(name);
    if trimmed.is_empty() || trimmed.contains(['\\', '\0']) {
        return Err(invalid());
    }
    let segments: Vec<String> = trimmed.split('/').map(String::from).collect();
    if segments.iter().any(|s| s.is_empty() || s == "." || s == "..") {
        return Err(invalid());
    }
    let value = format!("/{trimmed}");
    if value.len() > MAX_STR_LEN {
        return Err(invalid());
    }
    Ok((value, segments))
}

/// 由单元编号、种类和路径算出的实体编号（FNV-1a，128 位），按 UUID 第 8 版标好版本位。
fn entity_id(unit: UnitId, kind: u8, value: &str) -> EntityId {
    let mut h: u128 = 0x6c62272e07bb014262b821756295c58d;
    for b in unit.0.iter().chain([kind].iter()).chain(value.as_bytes()) {
        h ^= u128::from(*b);
        h = h.wrapping_mul(0x0000000001000000000000000000013B);
    }
    let mut id = h.to_be_bytes();
    id[6] = (id[6] & 0x0f) | 0x80;
    id[8] = (id[8] & 0x3f) | 0x80;
    EntityId(id)
}

/// 公历日期到自 1970-01-01 起的天数。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn mtime_of(t: zip::DateTime) -> Option<u64> {
    let days = days_from_civil(i64::from(t.year()), i64::from(t.month()), i64::from(t.day()));
    let secs = days * 86400 + i64::from(t.hour()) * 3600 + i64::from(t.minute()) * 60 + i64::from(t.second());
    u64::try_from(secs).ok()?.checked_mul(1_000_000_000)
}

impl ZipUnit {
    /// 以 `id` 为单元编号挂上 `source` 里的归档。
    pub fn open(id: UnitId, source: impl Source + 'static) -> Result<Self, ZipError> {
        let mut archive = ZipArchive::new(Box::new(source) as Box<dyn Source>)?;
        let ns = Namespace::TAGFS;
        let mut items = BTreeMap::new();
        // 目录：路径值 → 修改时间（隐含的目录没有）。
        let mut dirs: BTreeMap<String, Option<u64>> = BTreeMap::new();
        for index in 0..archive.len() {
            let entry = archive.by_index_raw(index)?;
            let (value, segments) = path_value(entry.name())?;
            let mtime = entry.last_modified().and_then(mtime_of);
            let is_dir = entry.is_dir();
            let size = entry.size();
            drop(entry);
            for n in 1..segments.len() {
                dirs.entry(format!("/{}", segments[..n].join("/"))).or_insert(None);
            }
            if is_dir {
                let slot = dirs.entry(value).or_insert(None);
                *slot = slot.or(mtime);
                continue;
            }
            let id_of = entity_id(id, 0, &value);
            // 同名的条目只认第一个。
            if items.contains_key(&id_of) {
                continue;
            }
            let mut tags = vec![Tag::new(ns, ARCHIVE_PATH, value)];
            if let Some(t) = mtime {
                tags.push(Tag::new(ns, ARCHIVE_MTIME, t));
            }
            items.insert(id_of, Item { tags: tagfs_core::tag::normalized(&tags), size, index: Some(index) });
        }
        for (value, mtime) in dirs {
            let mut tags = vec![Tag::new(ns, ARCHIVE_DIR, value.clone())];
            if let Some(t) = mtime {
                tags.push(Tag::new(ns, ARCHIVE_MTIME, t));
            }
            items.insert(
                entity_id(id, 1, &value),
                Item { tags: tagfs_core::tag::normalized(&tags), size: 0, index: None },
            );
        }
        Ok(ZipUnit {
            id,
            inner: Arc::new(Inner { items, archive: Mutex::new(archive), cache: Mutex::new(Vec::new()) }),
        })
    }

    /// 挂上宿主上的 zip 文件。
    pub fn open_file(id: UnitId, path: impl AsRef<Path>) -> Result<Self, ZipError> {
        let file = std::fs::File::open(path).map_err(ZipError::Io)?;
        Self::open(id, io::BufReader::new(file))
    }
}

impl Inner {
    /// 解压后的整个条目。
    fn content(&self, id: &EntityId, index: usize, size: u64) -> Result<Arc<Vec<u8>>, UnitError> {
        {
            let mut cache = self.cache.lock();
            if let Some(pos) = cache.iter().position(|(c, _)| c == id) {
                let hit = cache.remove(pos);
                let data = hit.1.clone();
                cache.insert(0, hit);
                return Ok(data);
            }
        }
        let mut data = Vec::with_capacity(usize::try_from(size).unwrap_or(0).min(1 << 26));
        {
            let mut archive = self.archive.lock();
            let mut entry = archive.by_index(index).map_err(|e| match e {
                zip::result::ZipError::Io(_) => UnitError::Device,
                _ => UnitError::Corrupt,
            })?;
            // 读完整个条目时 zip 会核对 CRC，对不上就报错。
            entry.read_to_end(&mut data).map_err(|e| match e.kind() {
                io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput | io::ErrorKind::UnexpectedEof => {
                    UnitError::Corrupt
                }
                _ => UnitError::Device,
            })?;
        }
        let data = Arc::new(data);
        let mut cache = self.cache.lock();
        cache.insert(0, (*id, data.clone()));
        cache.truncate(CACHED);
        Ok(data)
    }
}

impl Unit for ZipUnit {
    fn id(&self) -> UnitId {
        self.id
    }

    fn mapped_keys(&self) -> &[&'static str] {
        &[ARCHIVE_PATH, ARCHIVE_DIR, ARCHIVE_MTIME]
    }

    fn snapshot(&self) -> Box<dyn Snapshot> {
        Box::new(ZipSnapshot(self.inner.clone()))
    }

    fn write(&self, _: &EntityId, _: u64, _: &[u8]) -> Result<(), UnitError> {
        Err(READ_ONLY)
    }

    fn set_len(&self, _: &EntityId, _: u64) -> Result<(), UnitError> {
        Err(READ_ONLY)
    }

    fn sync(&self) -> Result<(), UnitError> {
        Ok(())
    }

    fn stage(&self) -> Result<StageId, UnitError> {
        Err(READ_ONLY)
    }

    fn write_staged(&self, stage: StageId, _: u64, _: &[u8]) -> Result<(), UnitError> {
        Err(UnitError::UnknownStage(stage))
    }

    fn discard_staged(&self, stage: StageId) -> Result<(), UnitError> {
        Err(UnitError::UnknownStage(stage))
    }

    fn apply(&self, changes: &[Change]) -> Result<(), UnitError> {
        if changes.is_empty() { Ok(()) } else { Err(READ_ONLY) }
    }

    fn prepare(&self, _: &TxnInfo, changes: &[Change]) -> Result<(), UnitError> {
        self.apply(changes)
    }

    fn prepared(&self) -> Result<Vec<(TxnInfo, Vec<Change>)>, UnitError> {
        Ok(Vec::new())
    }

    fn commit_prepared(&self, txn: TxnId, _: bool) -> Result<(), UnitError> {
        Err(UnitError::UnknownTxn(txn))
    }

    fn abort_prepared(&self, txn: TxnId) -> Result<(), UnitError> {
        Err(UnitError::UnknownTxn(txn))
    }

    fn committed(&self) -> Result<Vec<TxnInfo>, UnitError> {
        Ok(Vec::new())
    }

    fn forget(&self, _: TxnId) -> Result<(), UnitError> {
        Ok(())
    }

    /// 每个文件条目都能完整解压，CRC 对得上。
    fn check(&self) -> Result<(), UnitError> {
        let mut archive = self.inner.archive.lock();
        for item in self.inner.items.values() {
            let Some(index) = item.index else { continue };
            let mut entry = archive.by_index(index).map_err(|_| UnitError::Corrupt)?;
            io::copy(&mut entry, &mut io::sink()).map_err(|_| UnitError::Corrupt)?;
        }
        Ok(())
    }

    fn describe(&self) -> Vec<(&'static str, String)> {
        let files = self.inner.items.values().filter(|i| i.index.is_some()).count();
        let bytes: u64 = self.inner.items.values().map(|i| i.size).sum();
        vec![
            ("files", files.to_string()),
            ("directories", (self.inner.items.len() - files).to_string()),
            ("uncompressed bytes", bytes.to_string()),
        ]
    }
}

struct ZipSnapshot(Arc<Inner>);

impl Snapshot for ZipSnapshot {
    fn get(&self, id: &EntityId) -> Result<Option<EntityRecord>, UnitError> {
        Ok(self.0.items.get(id).map(|i| EntityRecord { id: *id, tags: i.tags.clone(), size: i.size }))
    }

    fn probe(&self, probe: &Probe<'_>) -> Result<Option<Vec<EntityId>>, UnitError> {
        // 归档里的条目不多，逐个比对。
        Ok(Some(
            self.0.items.iter().filter(|(_, i)| i.tags.iter().any(|t| probe.matches(t))).map(|(id, _)| *id).collect(),
        ))
    }

    fn all(&self) -> Result<Vec<EntityId>, UnitError> {
        Ok(self.0.items.keys().copied().collect())
    }

    fn read(&self, id: &EntityId, offset: u64, buf: &mut [u8]) -> Result<usize, UnitError> {
        let item = self.0.items.get(id).ok_or(UnitError::NotFound(*id))?;
        let Some(index) = item.index else { return Ok(0) };
        if offset >= item.size || buf.is_empty() {
            return Ok(0);
        }
        let content = self.0.content(id, index, item.size)?;
        let start = (offset as usize).min(content.len());
        let n = buf.len().min(content.len() - start);
        buf[..n].copy_from_slice(&content[start..start + n]);
        Ok(n)
    }
}

#[cfg(test)]
mod tests;
