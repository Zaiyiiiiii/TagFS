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

/// 块设备。可以被多个线程同时读写：不重叠的范围互不影响，和宿主上按偏移读写文件（`pread`/`pwrite`）
/// 一样；同一范围上的并发写由使用者自己避免。
pub trait BlockDevice: Send + Sync {
    /// 设备的物理块大小（字节）；分配按它对齐。
    fn block_size(&self) -> u32;

    /// 设备容量（字节）。
    fn capacity(&self) -> u64;

    /// 从 `offset` 读满 `buf`。
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), DeviceError>;

    /// 把 `buf` 写到 `offset`。返回时不保证已持久化，见 [`BlockDevice::flush`]。
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), DeviceError>;

    /// 返回时，之前所有写入都已持久化。
    fn flush(&self) -> Result<(), DeviceError>;

    /// 告诉设备这段范围不再使用。内容随后读出来是什么不作保证。
    fn trim(&self, offset: u64, len: u64) -> Result<(), DeviceError>;

    /// 把容量扩到至少 `capacity` 字节，新增部分读出来是零。能扩容的设备（如宿主机上的单文件）实现它；
    /// 默认不支持。
    fn grow(&self, capacity: u64) -> Result<(), DeviceError> {
        let _ = capacity;
        Err(DeviceError::Unsupported)
    }

    /// 重新取容量：别的进程可能扩了容。只会变大。
    fn refresh(&self) -> Result<(), DeviceError> {
        Ok(())
    }

    /// 多个进程同时使用这个设备时的协调。只给一个进程用的设备（内存、裸盘）没有它。
    fn share(&self) -> Option<&dyn Share> {
        None
    }
}

/// 多个进程同时打开同一个设备时的协调，由宿主实现（例如文件锁）。没有服务进程：每个进程自己打开设备，
/// 靠这里的锁排开彼此。
///
/// - **写者锁**：同一时刻只有一个进程改单元。
/// - **读者锁**：进程读单元时共享持有它。写者能独占它的那一刻，说明没有别的进程在读旧的状态，
///   别的进程读过的块才能重新分配。
/// - **属主**：每个打开设备的进程有一个身份。单元里属于某个进程的东西（暂存、进行中的跨单元事务）
///   记着它的身份；身份不在了（进程退出或崩溃），别的进程才能替它收拾。
pub trait Share: Send + Sync {
    /// 拿写者锁，拿不到就等。
    fn lock_writer(&self) -> Result<(), DeviceError>;

    fn unlock_writer(&self);

    /// 共享持有读者锁。同一个进程里可以重复持有，每次对应一次 [`Share::release_readers`]。
    fn hold_readers(&self) -> Result<(), DeviceError>;

    fn release_readers(&self);

    /// 此刻是否没有别的进程持有读者锁（不等）。只有持有写者锁的进程调用。
    fn readers_absent(&self) -> Result<bool, DeviceError>;

    /// 本进程的身份，不为 0。
    fn owner(&self) -> u64;

    /// 这个身份的进程是否还开着这个设备。本进程自己的身份算开着。
    fn alive(&self, owner: u64) -> Result<bool, DeviceError>;
}

/// `offset..offset+len` 是否落在容量为 `capacity` 的设备内。
pub fn in_range(capacity: u64, offset: u64, len: usize) -> bool {
    offset.checked_add(len as u64).is_some_and(|end| end <= capacity)
}
