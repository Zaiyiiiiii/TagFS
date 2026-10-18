//! 宿主机块设备：把一个普通文件或一块裸盘（如 `/dev/sdb`）当作 [`BlockDevice`]。
//!
//! 两者用同一个实现：都是按偏移读写的文件句柄，区别只在容量从哪里来。普通文件的容量在创建时
//! 指定（文件按需扩展，未写的部分不占空间）；裸盘的容量就是设备大小。

use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom};
use std::path::Path;

use tagfs_device::{BlockDevice, DeviceError, in_range};

pub struct FileDevice {
    file: File,
    block_size: u32,
    capacity: u64,
    read_only: bool,
}

impl FileDevice {
    /// 新建（或截断已有的）文件作为设备，容量 `capacity` 字节。
    pub fn create(path: impl AsRef<Path>, capacity: u64, block_size: u32) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path)?;
        file.set_len(capacity)?;
        Ok(FileDevice { file, block_size, capacity: round_down(capacity, block_size), read_only: false })
    }

    /// 打开已有的文件或裸盘。容量取它当前的大小（向下取整到块）。
    pub fn open(path: impl AsRef<Path>, block_size: u32, read_only: bool) -> io::Result<Self> {
        let mut file = OpenOptions::new().read(true).write(!read_only).open(path)?;
        // 裸盘的 metadata().len() 是 0，用 seek 到末尾取大小，对普通文件也成立。
        let len = file.seek(SeekFrom::End(0))?;
        Ok(FileDevice { file, block_size, capacity: round_down(len, block_size), read_only })
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
        self.capacity
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), DeviceError> {
        if !in_range(self.capacity, offset, buf.len()) {
            return Err(DeviceError::OutOfRange);
        }
        read_exact_at(&self.file, buf, offset).map_err(io_err)
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), DeviceError> {
        if self.read_only {
            return Err(DeviceError::ReadOnly);
        }
        if !in_range(self.capacity, offset, buf.len()) {
            return Err(DeviceError::OutOfRange);
        }
        write_all_at(&self.file, buf, offset).map_err(io_err)
    }

    fn flush(&mut self) -> Result<(), DeviceError> {
        if self.read_only {
            return Ok(());
        }
        self.file.sync_data().map_err(io_err)
    }

    fn trim(&mut self, offset: u64, len: u64) -> Result<(), DeviceError> {
        // 只是提示，宿主机文件上不做任何事。
        if !in_range(self.capacity, offset, len as usize) {
            return Err(DeviceError::OutOfRange);
        }
        Ok(())
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
        let mut dev = FileDevice::create(&path, 64 * 1024 + 100, 4096).unwrap();
        assert_eq!(dev.capacity(), 64 * 1024);
        dev.write_at(5000, b"hello").unwrap();
        dev.flush().unwrap();
        drop(dev);

        let dev = FileDevice::open(&path, 4096, true).unwrap();
        let mut buf = [0u8; 7];
        dev.read_at(4999, &mut buf).unwrap();
        assert_eq!(&buf, b"\0hello\0");
        let mut ro = dev;
        assert_eq!(ro.write_at(0, b"x"), Err(DeviceError::ReadOnly));
        assert_eq!(ro.read_at(64 * 1024 - 1, &mut [0u8; 2]), Err(DeviceError::OutOfRange));
        std::fs::remove_file(path).unwrap();
    }
}
