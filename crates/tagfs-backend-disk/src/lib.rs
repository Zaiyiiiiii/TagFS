//! 裸盘后端：一个块格式单元直接放在整块盘或分区上（如 `/dev/sdb`、`\\.\PhysicalDrive1`）。
//!
//! 容量就是盘的大小，不能扩容。格式化会抹掉盘上原有的全部内容。

use std::fmt;
use std::io;
use std::path::Path;

use tagfs_block::{BlockUnit, OpenError};
use tagfs_core::UnitId;
use tagfs_device_host::FileDevice;

/// 块大小。
pub const BLOCK_SIZE: u32 = 4096;

/// 裸盘单元。
pub type DiskUnit = BlockUnit<FileDevice>;

#[derive(Debug)]
pub enum DiskError {
    Io(io::Error),
    Format(OpenError),
}

impl From<io::Error> for DiskError {
    fn from(e: io::Error) -> Self {
        DiskError::Io(e)
    }
}

impl From<OpenError> for DiskError {
    fn from(e: OpenError) -> Self {
        DiskError::Format(e)
    }
}

impl fmt::Display for DiskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiskError::Io(e) => write!(f, "{e}"),
            DiskError::Format(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DiskError {}

/// 在盘上建一个空单元，原有内容全部丢失。
pub fn format(device: impl AsRef<Path>, unit: UnitId) -> Result<DiskUnit, DiskError> {
    Ok(BlockUnit::format(FileDevice::open(device, BLOCK_SIZE, false)?, unit)?)
}

/// 打开盘上的单元。
pub fn open(device: impl AsRef<Path>) -> Result<DiskUnit, DiskError> {
    Ok(BlockUnit::open(FileDevice::open(device, BLOCK_SIZE, false)?)?)
}

/// `device` 是不是格式化过的盘：一个设备（不是普通文件或目录），上面是块格式单元。只读，不改。
pub fn recognizes(device: impl AsRef<Path>) -> bool {
    let device = device.as_ref();
    std::fs::metadata(device).is_ok_and(|m| !m.is_file() && !m.is_dir())
        && FileDevice::open(device, BLOCK_SIZE, true).is_ok_and(|d| tagfs_block::is_formatted(&d))
}

#[cfg(test)]
mod tests {
    use tagfs_core::{Change, EntityId, Unit, UnitId};

    /// 没有真盘可用，用一个固定大小的文件代替：裸盘后端对它和对盘的处理一样。
    #[test]
    fn format_then_open() {
        let path = std::env::temp_dir().join(format!("tagfs-backend-disk-{}.bin", std::process::id()));
        std::fs::File::create(&path).unwrap().set_len(1 << 20).unwrap();
        let unit = super::format(&path, UnitId([2; 16])).unwrap();
        let stage = unit.stage().unwrap();
        unit.write_staged(stage, 0, b"on disk").unwrap();
        unit.apply(&[Change::Create { id: EntityId([1; 16]), tags: vec![], content: Some(stage) }]).unwrap();
        drop(unit);
        let unit = super::open(&path).unwrap();
        unit.verify().unwrap();
        assert_eq!(unit.id(), UnitId([2; 16]));
        std::fs::remove_file(path).unwrap();
    }
}
