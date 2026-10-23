//! 单文件后端：一个块格式单元存在一个镜像文件里。
//!
//! 镜像文件可以放在宿主能访问的任何地方（本地盘、可移动盘、网络共享……），从很小开始，空间不够时自动扩容，
//! 未写过的部分不占宿主空间。宿主目录后端也用它存放没有路径的实体。
//!
//! 多个进程可以同时打开同一个镜像文件，各自直接读写，用镜像旁边锁目录（`<文件名>.locks/`）里的文件锁
//! 协调（见 `tagfs_block` 的“多个进程”）。文件锁不起作用的文件系统（某些网络盘）上打不开。
//! 每个打开的镜像有一个后台线程，把攒着的内容写入及时提交，好让别的进程能改。

use std::fmt;
use std::io;
use std::path::Path;
use std::time::Duration;

use tagfs_block::{BlockUnit, OpenError, Ticker};
use tagfs_core::UnitId;
use tagfs_device_host::FileDevice;

/// 块大小。
pub const BLOCK_SIZE: u32 = 4096;
/// 新建镜像文件的初始容量。
const INITIAL: u64 = 4 << 20;
/// 后台线程多久看一次有没有攒着的内容写入要提交。
const TICK: Duration = Duration::from_millis(25);

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

/// 在 `path` 新建镜像文件（覆盖已有文件），里面是一个空单元。调用者要保证此时没有别的进程在用这个路径。
pub fn create(path: impl AsRef<Path>, unit: UnitId) -> Result<ImageUnit, ImageError> {
    let dev = FileDevice::create_shared(path, INITIAL, BLOCK_SIZE)?;
    Ok(started(BlockUnit::format(dev, unit)?))
}

/// 在 `path` 新建镜像文件；文件已经存在时失败（`ImageError::Io`，`AlreadyExists`），不碰它。多个进程同时新建
/// 同一个镜像时只有一个成功；其余的打开它就行（新建的那个格式化完之前打开会报没有格式化，稍后再试）。
pub fn create_new(path: impl AsRef<Path>, unit: UnitId) -> Result<ImageUnit, ImageError> {
    let dev = FileDevice::create_new_shared(path, INITIAL, BLOCK_SIZE)?;
    Ok(started(BlockUnit::format(dev, unit)?))
}

/// 打开镜像文件。别的进程可以同时开着它。
pub fn open(path: impl AsRef<Path>) -> Result<ImageUnit, ImageError> {
    Ok(started(BlockUnit::open(FileDevice::open_shared(path, BLOCK_SIZE)?)?))
}

/// 起后台提交线程。单元关掉之后它自己结束。
fn started(unit: ImageUnit) -> ImageUnit {
    let ticker: Ticker<FileDevice> = unit.ticker();
    std::thread::Builder::new()
        .name("tagfs-commit".into())
        .spawn(move || {
            while ticker.tick() {
                std::thread::sleep(TICK);
            }
        })
        .expect("spawn the commit thread");
    unit
}

/// `path` 是不是镜像文件：一个普通文件，里面是块格式单元。只读，不改。
pub fn recognizes(path: impl AsRef<Path>) -> bool {
    let path = path.as_ref();
    path.is_file() && FileDevice::open(path, BLOCK_SIZE, true).is_ok_and(|d| tagfs_block::is_formatted(&d))
}

#[cfg(test)]
mod tests {
    use tagfs_core::{Change, EntityId, Tag, Unit};

    #[test]
    fn an_image_survives_reopening() {
        let path = std::env::temp_dir().join(format!("tagfs-backend-file-{}.img", std::process::id()));
        let id = EntityId([1; 16]);
        {
            let unit = super::create(&path, tagfs_core::UnitId([5; 16])).unwrap();
            let stage = unit.stage().unwrap();
            unit.write_staged(stage, 0, &vec![3; 9 << 20]).unwrap();
            unit.apply(&[Change::Create { id, tags: vec![Tag::flag("user", "x")], content: Some(stage) }]).unwrap();
        }
        let unit = super::open(&path).unwrap();
        unit.verify().unwrap();
        assert_eq!(unit.snapshot().get(&id).unwrap().unwrap().size, 9 << 20);
        std::fs::remove_file(path).unwrap();
    }
}
