//! 块设备接口：存储单元的块格式实现（`tagfs-block`）通过它读写设备。
//!
//! 接口按字节偏移读写，任意偏移、任意长度；设备自身的对齐要求由各实现内部消化。
//! 设备不知道任何文件系统概念。

#![no_std]

extern crate alloc;

mod mem;

pub use mem::MemDevice;

/// 设备错误。不携带宿主的错误码，宿主实现把自己的错误映射到这里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceError {
    /// 读写超出设备末尾。
    OutOfRange,
    /// 设备报告的读写失败。
    Io,
    /// 设备只读。
    ReadOnly,
    /// 设备不支持这个操作（例如裸盘不能扩容）。
    Unsupported,
}

impl core::fmt::Display for DeviceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            DeviceError::OutOfRange => "out of device range",
            DeviceError::Io => "device i/o error",
            DeviceError::ReadOnly => "device is read-only",
            DeviceError::Unsupported => "operation not supported by the device",
        })
    }
}

impl core::error::Error for DeviceError {}

pub trait BlockDevice {
    /// 设备的物理块大小（字节）；分配按它对齐。
    fn block_size(&self) -> u32;

    /// 设备容量（字节）。
    fn capacity(&self) -> u64;

    /// 从 `offset` 读满 `buf`。
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), DeviceError>;

    /// 把 `buf` 写到 `offset`。返回时不保证已持久化，见 [`BlockDevice::flush`]。
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), DeviceError>;

    /// 返回时，之前所有写入都已持久化。
    fn flush(&mut self) -> Result<(), DeviceError>;

    /// 告诉设备这段范围不再使用。内容随后读出来是什么不作保证。
    fn trim(&mut self, offset: u64, len: u64) -> Result<(), DeviceError>;

    /// 把容量扩到至少 `capacity` 字节，新增部分读出来是零。能扩容的设备（如宿主机上的单文件）实现它；
    /// 默认不支持。
    fn grow(&mut self, capacity: u64) -> Result<(), DeviceError> {
        let _ = capacity;
        Err(DeviceError::Unsupported)
    }
}

/// `offset..offset+len` 是否落在容量为 `capacity` 的设备内。
pub fn in_range(capacity: u64, offset: u64, len: usize) -> bool {
    offset.checked_add(len as u64).is_some_and(|end| end <= capacity)
}
