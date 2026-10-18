//! 单文件后端：一个块格式单元存在一个镜像文件里。
//!
//! 镜像文件可以放在宿主能访问的任何地方（本地盘、可移动盘、网络共享……），从很小开始，空间不够时自动扩容，
//! 未写过的部分不占宿主空间。宿主目录后端也用它存放没有路径的实体。

use std::fmt;
use std::io;
use std::path::Path;

use tagfs_block::{BlockUnit, OpenError};
use tagfs_core::UnitId;
use tagfs_device_host::FileDevice;

/// 块大小。
pub const BLOCK_SIZE: u32 = 4096;
/// 新建镜像文件的初始容量。
const INITIAL: u64 = 4 << 20;

/// 单文件单元。
pub type ImageUnit = BlockUnit<FileDevice>;

#[derive(Debug)]
pub enum ImageError {
    Io(io::Error),
    Format(OpenError),
}

impl From<io::Error> for ImageError {
    fn from(e: io::Error) -> Self {
        ImageError::Io(e)
    }
}

impl From<OpenError> for ImageError {
    fn from(e: OpenError) -> Self {
        ImageError::Format(e)
    }
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::Io(e) => write!(f, "{e}"),
            ImageError::Format(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ImageError {}

/// 在 `path` 新建镜像文件（覆盖已有文件），里面是一个空单元。
pub fn create(path: impl AsRef<Path>, unit: UnitId) -> Result<ImageUnit, ImageError> {
    let dev = FileDevice::create(path, INITIAL, BLOCK_SIZE)?;
    Ok(BlockUnit::format(dev, unit)?)
}

/// 打开镜像文件。
pub fn open(path: impl AsRef<Path>) -> Result<ImageUnit, ImageError> {
    Ok(BlockUnit::open(FileDevice::open(path, BLOCK_SIZE, false)?)?)
}

#[cfg(test)]
mod tests {
    use tagfs_core::{Change, EntityId, Tag, Unit};

    #[test]
    fn an_image_survives_reopening() {
        let path = std::env::temp_dir().join(format!("tagfs-backend-file-{}.img", std::process::id()));
        let id = EntityId([1; 16]);
        {
            let mut unit = super::create(&path, tagfs_core::UnitId([5; 16])).unwrap();
            unit.apply(&[Change::Create { id, tags: vec![Tag::flag("user", "x")], content: vec![3; 9 << 20] }])
                .unwrap();
        }
        let unit = super::open(&path).unwrap();
        unit.verify().unwrap();
        assert_eq!(unit.snapshot().get(&id).unwrap().unwrap().size, 9 << 20);
        std::fs::remove_file(path).unwrap();
    }
}
