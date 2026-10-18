//! 内存设备：用于测试和临时卷。
//!
//! 它区分“已持久化”和“尚未刷盘”的写入，可以模拟断电：[`MemDevice::crash`] 只保留
//! 未刷盘写入中的前若干个，其余丢弃。故障注入测试靠它检查任意一次写入之后断电的结果。

use alloc::vec;
use alloc::vec::Vec;

use tagfs_sync::RwLock;

use crate::{BlockDevice, DeviceError, in_range};

pub struct MemDevice {
    block_size: u32,
    /// 能扩容到的最大容量；`None` 表示不能扩容。
    max_len: Option<u64>,
    state: RwLock<State>,
}

struct State {
    /// 已持久化的内容。
    durable: Vec<u8>,
    /// 尚未刷盘的写入，按发生顺序。
    pending: Vec<(u64, Vec<u8>)>,
    /// 设备生命周期内一共发生过的写入次数（含已刷盘的），故障注入用它定位断电点。
    writes: u64,
}

impl MemDevice {
    pub fn new(len: u64, block_size: u32) -> Self {
        assert!(block_size > 0, "block size must be positive");
        Self {
            block_size,
            max_len: None,
            state: RwLock::new(State { durable: vec![0; len as usize], pending: Vec::new(), writes: 0 }),
        }
    }

    /// 能扩容的内存设备，最大到 `max_len` 字节。
    pub fn growable(len: u64, max_len: u64, block_size: u32) -> Self {
        Self { max_len: Some(max_len), ..Self::new(len, block_size) }
    }

    /// 至今发生过的写入次数。
    pub fn write_count(&self) -> u64 {
        self.state.read().writes
    }

    /// 尚未刷盘的写入个数。
    pub fn pending_writes(&self) -> usize {
        self.state.read().pending.len()
    }

    /// 模拟断电：未刷盘的写入只有前 `keep` 个落盘，其余丢失。返回断电后的设备。
    pub fn crash(&self, keep: usize) -> MemDevice {
        let s = self.state.read();
        let mut durable = s.durable.clone();
        for (offset, data) in s.pending.iter().take(keep) {
            let start = *offset as usize;
            durable[start..start + data.len()].copy_from_slice(data);
        }
        MemDevice {
            block_size: self.block_size,
            max_len: self.max_len,
            state: RwLock::new(State { durable, pending: Vec::new(), writes: 0 }),
        }
    }
}

impl BlockDevice for MemDevice {
    fn block_size(&self) -> u32 {
        self.block_size
    }

    fn capacity(&self) -> u64 {
        self.state.read().durable.len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), DeviceError> {
        let s = self.state.read();
        if !in_range(s.durable.len() as u64, offset, buf.len()) {
            return Err(DeviceError::OutOfRange);
        }
        let start = offset as usize;
        buf.copy_from_slice(&s.durable[start..start + buf.len()]);
        // 未刷盘的写入对读取可见，按顺序覆盖上去。
        let end = offset + buf.len() as u64;
        for (w_off, data) in &s.pending {
            let w_end = w_off + data.len() as u64;
            if *w_off >= end || w_end <= offset {
                continue;
            }
            let lo = (*w_off).max(offset);
            let hi = w_end.min(end);
            buf[(lo - offset) as usize..(hi - offset) as usize]
                .copy_from_slice(&data[(lo - w_off) as usize..(hi - w_off) as usize]);
        }
        Ok(())
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), DeviceError> {
        let mut s = self.state.write();
        if !in_range(s.durable.len() as u64, offset, buf.len()) {
            return Err(DeviceError::OutOfRange);
        }
        s.writes += 1;
        s.pending.push((offset, buf.to_vec()));
        Ok(())
    }

    fn flush(&self) -> Result<(), DeviceError> {
        let mut s = self.state.write();
        let State { durable, pending, .. } = &mut *s;
        for (offset, data) in pending.drain(..) {
            let start = offset as usize;
            durable[start..start + data.len()].copy_from_slice(&data);
        }
        Ok(())
    }

    fn trim(&self, offset: u64, len: u64) -> Result<(), DeviceError> {
        if !in_range(self.capacity(), offset, len as usize) {
            return Err(DeviceError::OutOfRange);
        }
        Ok(())
    }

    fn grow(&self, capacity: u64) -> Result<(), DeviceError> {
        let max = self.max_len.ok_or(DeviceError::Unsupported)?;
        if capacity > max {
            return Err(DeviceError::OutOfRange);
        }
        let mut s = self.state.write();
        if capacity > s.durable.len() as u64 {
            s.durable.resize(capacity as usize, 0);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_see_unflushed_writes() {
        let dev = MemDevice::new(64, 16);
        dev.write_at(4, b"hello").unwrap();
        dev.write_at(6, b"XY").unwrap();
        let mut buf = [0u8; 8];
        dev.read_at(2, &mut buf).unwrap();
        assert_eq!(&buf, b"\0\0heXYo\0");
    }

    #[test]
    fn crash_keeps_only_flushed_and_the_first_pending() {
        let dev = MemDevice::new(16, 16);
        dev.write_at(0, b"a").unwrap();
        dev.flush().unwrap();
        dev.write_at(1, b"b").unwrap();
        dev.write_at(2, b"c").unwrap();
        let after = dev.crash(1);
        let mut buf = [0u8; 3];
        after.read_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"ab\0");
        dev.crash(0).read_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"a\0\0");
    }

    #[test]
    fn out_of_range_is_rejected() {
        let dev = MemDevice::new(8, 8);
        assert_eq!(dev.write_at(6, b"abc"), Err(DeviceError::OutOfRange));
        assert_eq!(dev.read_at(u64::MAX, &mut [0u8; 1]), Err(DeviceError::OutOfRange));
    }
}
