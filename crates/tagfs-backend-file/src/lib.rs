//! 单文件后端：一个块格式单元存在一个镜像文件里。
//!
//! 镜像文件可以放在宿主能访问的任何地方（本地盘、可移动盘、网络共享……），从很小开始，空间不够时自动扩容，
//! 未写过的部分不占宿主空间。宿主目录后端也用它存放没有路径的实体。
//!
//! 多个进程可以同时打开同一个镜像文件，各自直接读写，用镜像旁边锁目录（`<文件名>.locks/`）里的锁
//! 协调（见 `tagfs_block` 的“多个进程”）。系统文件锁在那里不起作用时用自己实现的锁（`tagfs_device_host::lock`）。
//! 每个打开的镜像有一个后台线程，把攒着的内容写入及时提交，好让别的进程能改。

use std::fmt;
use std::io;
use std::path::Path;
use std::time::Duration;

use tagfs_block::{BlockUnit, OpenError, Ticker};
use tagfs_core::UnitId;
use tagfs_device_host::{FileDevice, lock};

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

/// 打开 `path` 的镜像文件，还没有就新建一个空单元（编号 `unit`）。多个进程可以同时调用：只有一个新建，
/// 其余的等它建完再打开。
///
/// 新建的一方从开始到格式化完一直持有锁目录里的 `create` 锁，并在锁目录里留下“正在新建”的标记（`creating`）。
/// 拿到这把锁时只有两种情况：镜像已经建好；或者新建的进程已经不在了（锁随它放掉了），留下一个没格式化完的
/// 文件。有标记的半成品由拿到锁的一方重新建好；没有标记的文件不是这里建的（比如别的什么文件），不碰它，报错。
/// 所以不需要等一会儿再试，也不会把建到一半的单元一直当成“别人正在建”。
pub fn open_or_create(path: impl AsRef<Path>, unit: UnitId) -> Result<ImageUnit, ImageError> {
    open_or_create_at(path.as_ref(), unit, false)
}

/// 同 [`open_or_create`]，但 `path` 是 TagFS 自己的位置（比如宿主目录单元的 `.tagfs/unit.img`），那里不会有
/// 别的文件：没格式化完的文件即使没有标记（标记出现之前的版本留下的），也当作半成品重新建好。
pub fn open_or_create_own(path: impl AsRef<Path>, unit: UnitId) -> Result<ImageUnit, ImageError> {
    open_or_create_at(path.as_ref(), unit, true)
}

fn open_or_create_at(path: &Path, unit: UnitId, own: bool) -> Result<ImageUnit, ImageError> {
    if recognizes(path) {
        return open(path);
    }
    let dir = tagfs_device_host::lock_dir(path);
    let mode = lock::mode(&dir)?;
    let creating = lock::Lock::open(&dir.join("create"), mode)?;
    creating.lock()?;
    if recognizes(path) {
        return open(path);
    }
    let mark = dir.join("creating");
    if !own && path.exists() && !mark.exists() {
        return Err(ImageError::Format(OpenError::NotFormatted));
    }
    // 标记：这里有一个正在新建的镜像。格式化完才去掉。
    std::fs::File::create(&mark)?.sync_all()?;
    let created = create(path, unit)?;
    std::fs::remove_file(&mark)?;
    Ok(created)
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

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("tagfs-backend-file-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn open_or_create_creates_once() {
        let dir = temp_dir("once");
        let path = dir.join("a.img");
        let first = super::open_or_create(&path, tagfs_core::UnitId([1; 16])).unwrap().id();
        let again = super::open_or_create(&path, tagfs_core::UnitId([2; 16])).unwrap().id();
        assert_eq!(first, again);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn open_or_create_finishes_an_abandoned_image() {
        // A creator that died after marking and truncating, before formatting.
        let dir = temp_dir("abandoned");
        let path = dir.join("a.img");
        let locks = tagfs_device_host::lock_dir(&path);
        std::fs::create_dir_all(&locks).unwrap();
        std::fs::write(locks.join("creating"), b"").unwrap();
        std::fs::write(&path, vec![0u8; 8192]).unwrap();
        let unit = super::open_or_create(&path, tagfs_core::UnitId([3; 16])).unwrap();
        assert_eq!(unit.id(), tagfs_core::UnitId([3; 16]));
        drop(unit);
        assert!(!locks.join("creating").exists(), "the mark is gone once it's done");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn open_or_create_leaves_other_files_alone() {
        let dir = temp_dir("foreign");
        let path = dir.join("notes.txt");
        std::fs::write(&path, b"not an image").unwrap();
        assert!(matches!(
            super::open_or_create(&path, tagfs_core::UnitId([4; 16])),
            Err(super::ImageError::Format(super::OpenError::NotFormatted))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"not an image");
        // TagFS's own location holds nothing else: an unmarked half-made image there is redone.
        let unit = super::open_or_create_own(&path, tagfs_core::UnitId([4; 16])).unwrap();
        assert_eq!(unit.id(), tagfs_core::UnitId([4; 16]));
        drop(unit);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
