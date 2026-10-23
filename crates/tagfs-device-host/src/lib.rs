//! 宿主机块设备：把一个普通文件或一块裸盘（如 `/dev/sdb`）当作 [`BlockDevice`]。
//!
//! 两者用同一个实现：都是按偏移读写的文件句柄，区别只在容量从哪里来。普通文件的容量在创建时
//! 指定（文件按需扩展，未写的部分不占空间）；裸盘的容量就是设备大小。
//!
//! 普通文件可以由多个进程同时打开（[`FileDevice::create_shared`]、[`FileDevice::open_shared`]）：
//! 进程之间用文件锁协调（[`Share`]）。锁放在文件旁边的锁目录里（`<文件名>.locks/`），不锁文件本身：
//! Windows 上的文件锁是强制的，锁住的范围别人读写不了。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use tagfs_device::{BlockDevice, DeviceError, Share, in_range};

pub struct FileDevice {
    file: File,
    block_size: u32,
    /// 扩容时改变；读写时按当时的值检查范围。
    capacity: AtomicU64,
    read_only: bool,
    /// 普通文件可以扩容；裸盘不行。
    growable: bool,
    /// 扩容一次只做一个。
    growing: tagfs_sync::Mutex<()>,
    /// 和别的进程共用时的锁。
    share: Option<FileShare>,
}

impl FileDevice {
    /// 新建（或截断已有的）文件作为设备，初始容量 `capacity` 字节。之后可以扩容。只给一个进程用。
    pub fn create(path: impl AsRef<Path>, capacity: u64, block_size: u32) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path)?;
        file.set_len(capacity)?;
        Ok(FileDevice {
            file,
            block_size,
            capacity: AtomicU64::new(round_down(capacity, block_size)),
            read_only: false,
            growable: true,
            growing: tagfs_sync::Mutex::new(()),
            share: None,
        })
    }

    /// 打开已有的文件或裸盘。容量取它当前的大小（向下取整到块）。只给一个进程用。
    pub fn open(path: impl AsRef<Path>, block_size: u32, read_only: bool) -> io::Result<Self> {
        let mut file = OpenOptions::new().read(true).write(!read_only).open(path)?;
        // 裸盘的 metadata().len() 是 0，用 seek 到末尾取大小，对普通文件也成立。
        let len = file.seek(SeekFrom::End(0))?;
        let growable = !read_only && file.metadata()?.is_file();
        Ok(FileDevice {
            file,
            block_size,
            capacity: AtomicU64::new(round_down(len, block_size)),
            read_only,
            growable,
            growing: tagfs_sync::Mutex::new(()),
            share: None,
        })
    }

    /// 新建可以由多个进程同时使用的文件设备。调用者要保证此时没有别的进程在用这个路径。
    pub fn create_shared(path: impl AsRef<Path>, capacity: u64, block_size: u32) -> io::Result<Self> {
        let path = path.as_ref();
        let share = FileShare::open(&lock_dir(path))?;
        Ok(FileDevice { share: Some(share), ..Self::create(path, capacity, block_size)? })
    }

    /// 新建可以由多个进程同时使用的文件设备；文件已经存在时失败（`AlreadyExists`），不碰它。
    /// 多个进程同时新建同一个文件时只有一个成功，其余的转而打开它。
    pub fn create_new_shared(path: impl AsRef<Path>, capacity: u64, block_size: u32) -> io::Result<Self> {
        let path = path.as_ref();
        let file = OpenOptions::new().read(true).write(true).create_new(true).open(path)?;
        let share = FileShare::open(&lock_dir(path))?;
        file.set_len(capacity)?;
        Ok(FileDevice {
            file,
            block_size,
            capacity: AtomicU64::new(round_down(capacity, block_size)),
            read_only: false,
            growable: true,
            growing: tagfs_sync::Mutex::new(()),
            share: Some(share),
        })
    }

    /// 打开已有的文件，和别的进程共用。只用于普通文件。
    pub fn open_shared(path: impl AsRef<Path>, block_size: u32) -> io::Result<Self> {
        let path = path.as_ref();
        let share = FileShare::open(&lock_dir(path))?;
        Ok(FileDevice { share: Some(share), ..Self::open(path, block_size, false)? })
    }
}

/// 文件 `path` 的锁目录。
pub fn lock_dir(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".locks");
    path.with_file_name(name)
}

/// 用锁目录里的文件锁实现的 [`Share`]。
///
/// - `writer`：写者锁，独占。
/// - `readers`：读者锁，读的时候共享持有。
/// - `owners/<身份>`：每个打开设备的进程一个，独占持有到关闭。锁还被持有，那个进程就还在；
///   进程崩溃时系统替它放掉锁。
struct FileShare {
    writer: File,
    readers: File,
    /// 本进程持有读者锁的次数。
    held: std::sync::Mutex<usize>,
    owners: PathBuf,
    owner: u64,
    /// 本进程的属主锁，持有到关闭。
    owner_file: Option<File>,
}

fn random_owner() -> u64 {
    loop {
        let mut b = [0u8; 8];
        getrandom::fill(&mut b).expect("system random number generator");
        let v = u64::from_le_bytes(b);
        if v != 0 {
            return v;
        }
    }
}

fn lock_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)
}

fn would_block(e: &fs::TryLockError) -> bool {
    matches!(e, fs::TryLockError::WouldBlock)
}

impl FileShare {
    fn open(dir: &Path) -> io::Result<Self> {
        let owners = dir.join("owners");
        fs::create_dir_all(&owners)?;
        let writer = lock_file(&dir.join("writer"))?;
        let readers = lock_file(&dir.join("readers"))?;
        probe_locks(&dir.join("probe"))?;
        // 先在临时名字下拿到锁再改名，免得别人在拿锁之前看到它、当它已经不在了。
        let owner = random_owner();
        let name = format!("{owner:016x}");
        let tmp = owners.join(format!("{name}.new"));
        let owner_file = lock_file(&tmp)?;
        owner_file.lock()?;
        fs::rename(&tmp, owners.join(&name))?;
        let me =
            FileShare { writer, readers, held: std::sync::Mutex::new(0), owners, owner, owner_file: Some(owner_file) };
        me.sweep();
        Ok(me)
    }

    fn owner_path(&self, owner: u64) -> PathBuf {
        self.owners.join(format!("{owner:016x}"))
    }

    /// 删掉已经不在的进程留下的属主文件。还没改名的（`.new`）不碰：它可能正要被拿锁。
    fn sweep(&self) {
        let Ok(entries) = fs::read_dir(&self.owners) else { return };
        let mine = format!("{:016x}", self.owner);
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            if name == mine || name.ends_with(".new") {
                continue;
            }
            if let Ok(f) = OpenOptions::new().read(true).write(true).open(&path)
                && f.try_lock().is_ok()
            {
                let _ = f.unlock();
                drop(f);
                let _ = fs::remove_file(&path);
            }
        }
    }
}

/// 确认这里的文件锁真的起作用：同一个文件开两次，一边锁住时另一边必须拿不到。
/// 有的文件系统（某些网络盘、FUSE）上锁是空操作，多个进程同时写会把单元写坏，这时拒绝打开。
fn probe_locks(path: &Path) -> io::Result<()> {
    let a = lock_file(path)?;
    let b = lock_file(path)?;
    a.lock()?;
    let r = b.try_lock();
    let _ = a.unlock();
    match r {
        Err(e) if would_block(&e) => Ok(()),
        Err(fs::TryLockError::Error(e)) => Err(e),
        _ => Err(io::Error::new(io::ErrorKind::Unsupported, "file locks have no effect on this file system")),
    }
}

impl Drop for FileShare {
    fn drop(&mut self) {
        // 先关掉再删：Windows 上删一个还开着的文件会让它处于“删除中”，别人打不开也认不出它已经不在。
        drop(self.owner_file.take());
        let _ = fs::remove_file(self.owner_path(self.owner));
    }
}

impl Share for FileShare {
    fn lock_writer(&self) -> Result<(), DeviceError> {
        self.writer.lock().map_err(io_err)
    }

    fn unlock_writer(&self) {
        let _ = self.writer.unlock();
    }

    fn hold_readers(&self) -> Result<(), DeviceError> {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        if *held == 0 {
            self.readers.lock_shared().map_err(io_err)?;
        }
        *held += 1;
        Ok(())
    }

    fn release_readers(&self) {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        *held -= 1;
        if *held == 0 {
            let _ = self.readers.unlock();
        }
    }

    fn readers_absent(&self) -> Result<bool, DeviceError> {
        // 本进程自己的读者不算：先放掉自己的共享锁，试着独占，再拿回来。别的进程只有写者才会独占它，
        // 而写者就是我们，所以拿回来不会等。
        let held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        if *held > 0 {
            let _ = self.readers.unlock();
        }
        let absent = match self.readers.try_lock() {
            Ok(()) => {
                let _ = self.readers.unlock();
                Ok(true)
            }
            Err(e) if would_block(&e) => Ok(false),
            Err(_) => Err(DeviceError::Io),
        };
        if *held > 0 {
            self.readers.lock_shared().map_err(io_err)?;
        }
        absent
    }

    fn owner(&self) -> u64 {
        self.owner
    }

    fn alive(&self, owner: u64) -> Result<bool, DeviceError> {
        if owner == self.owner {
            return Ok(true);
        }
        if owner == 0 {
            return Ok(false);
        }
        let f = match OpenOptions::new().read(true).write(true).open(self.owner_path(owner)) {
            Ok(f) => f,
            // 没有这个文件，或者它正被删掉（Windows）：那个进程已经不在了。
            Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied) => {
                return Ok(false);
            }
            Err(_) => return Err(DeviceError::Io),
        };
        match f.try_lock() {
            Ok(()) => {
                let _ = f.unlock();
                Ok(false)
            }
            Err(e) if would_block(&e) => Ok(true),
            Err(_) => Err(DeviceError::Io),
        }
    }
}

fn round_down(len: u64, block_size: u32) -> u64 {
    assert!(block_size > 0, "block size must be positive");
    len - len % block_size as u64
}

fn io_err(_: io::Error) -> DeviceError {
    DeviceError::Io
}

#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(unix)]
fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset)? {
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_write(buf, offset)? {
            0 => return Err(io::ErrorKind::WriteZero.into()),
            n => {
                buf = &buf[n..];
                offset += n as u64;
            }
        }
    }
    Ok(())
}

impl BlockDevice for FileDevice {
    fn block_size(&self) -> u32 {
        self.block_size
    }

    fn capacity(&self) -> u64 {
        self.capacity.load(Ordering::Acquire)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), DeviceError> {
        if !in_range(self.capacity(), offset, buf.len()) {
            return Err(DeviceError::OutOfRange);
        }
        read_exact_at(&self.file, buf, offset).map_err(io_err)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), DeviceError> {
        if self.read_only {
            return Err(DeviceError::ReadOnly);
        }
        if !in_range(self.capacity(), offset, buf.len()) {
            return Err(DeviceError::OutOfRange);
        }
        write_all_at(&self.file, buf, offset).map_err(io_err)
    }

    fn flush(&self) -> Result<(), DeviceError> {
        if self.read_only {
            return Ok(());
        }
        self.file.sync_data().map_err(io_err)
    }

    fn trim(&self, offset: u64, len: u64) -> Result<(), DeviceError> {
        // 只是提示，宿主机文件上不做任何事。
        if !in_range(self.capacity(), offset, len as usize) {
            return Err(DeviceError::OutOfRange);
        }
        Ok(())
    }

    fn grow(&self, capacity: u64) -> Result<(), DeviceError> {
        if !self.growable {
            return Err(DeviceError::Unsupported);
        }
        let _one_at_a_time = self.growing.lock();
        let capacity = round_down(capacity, self.block_size);
        if capacity > self.capacity() {
            // 文件变长，新增部分是空洞：不占空间，读出来是零。先变长再公布新容量，
            // 这样按新容量检查通过的读写一定落在文件里。
            self.file.set_len(capacity).map_err(io_err)?;
            self.capacity.store(capacity, Ordering::Release);
        }
        Ok(())
    }

    fn refresh(&self) -> Result<(), DeviceError> {
        if self.share.is_none() {
            return Ok(());
        }
        let len = round_down(self.file.metadata().map_err(io_err)?.len(), self.block_size);
        self.capacity.fetch_max(len, Ordering::AcqRel);
        Ok(())
    }

    fn share(&self) -> Option<&dyn Share> {
        self.share.as_ref().map(|s| s as &dyn Share)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("tagfs-device-host-{}-{name}", std::process::id()))
    }

    #[test]
    fn writes_survive_reopening() {
        let path = temp_path("reopen");
        let dev = FileDevice::create(&path, 64 * 1024 + 100, 4096).unwrap();
        assert_eq!(dev.capacity(), 64 * 1024);
        dev.write_at(5000, b"hello").unwrap();
        dev.flush().unwrap();
        drop(dev);

        let dev = FileDevice::open(&path, 4096, true).unwrap();
        let mut buf = [0u8; 7];
        dev.read_at(4999, &mut buf).unwrap();
        assert_eq!(&buf, b"\0hello\0");
        assert_eq!(dev.write_at(0, b"x"), Err(DeviceError::ReadOnly));
        assert_eq!(dev.read_at(64 * 1024 - 1, &mut [0u8; 2]), Err(DeviceError::OutOfRange));
        assert_eq!(dev.grow(1 << 20), Err(DeviceError::Unsupported));
        drop(dev);

        let dev = FileDevice::open(&path, 4096, false).unwrap();
        dev.grow(1 << 20).unwrap();
        assert_eq!(dev.capacity(), 1 << 20);
        dev.write_at((1 << 20) - 1, b"z").unwrap();
        let mut buf = [1u8; 2];
        dev.read_at(64 * 1024, &mut buf).unwrap();
        assert_eq!(buf, [0, 0]);
        std::fs::remove_file(path).unwrap();
    }
}
