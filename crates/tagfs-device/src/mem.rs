//! 内存设备：用于测试和临时卷。
//!
//! 它区分“已持久化”和“尚未刷盘”的写入，可以模拟断电：[`MemDevice::crash`] 只保留
//! 未刷盘写入中的前若干个，其余丢弃。故障注入测试靠它检查任意一次写入之后断电的结果。

use alloc::vec;
use alloc::vec::Vec;

use crate::{BlockDevice, DeviceError, in_range};

pub struct MemDevice {
    block_size: u32,
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
        Self { block_size, durable: vec![0; len as usize], pending: Vec::new(), writes: 0 }
    }

    /// 至今发生过的写入次数。
    pub fn write_count(&self) -> u64 {
        self.writes
    }

    /// 尚未刷盘的写入个数。
    pub fn pending_writes(&self) -> usize {
        self.pending.len()
    }

    /// 模拟断电：未刷盘的写入只有前 `keep` 个落盘，其余丢失。返回断电后的设备。
    pub fn crash(&self, keep: usize) -> MemDevice {
        let mut durable = self.durable.clone();
        for (offset, data) in self.pending.iter().take(keep) {
            let start = *offset as usize;
            durable[start..start + data.len()].copy_from_slice(data);
        }
        MemDevice { block_size: self.block_size, durable, pending: Vec::new(), writes: 0 }
    }
}

impl BlockDevice for MemDevice {
    fn block_size(&self) -> u32 {
        self.block_size
    }

    fn capacity(&self) -> u64 {
        self.durable.len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), DeviceError> {
        if !in_range(self.capacity(), offset, buf.len()) {
            return Err(DeviceError::OutOfRange);
        }
        let start = offset as usize;
        buf.copy_from_slice(&self.durable[start..start + buf.len()]);
        // 未刷盘的写入对读取可见，按顺序覆盖上去。
        let end = offset + buf.len() as u64;
        for (w_off, data) in &self.pending {
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

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), DeviceError> {
        if !in_range(self.capacity(), offset, buf.len()) {
            return Err(DeviceError::OutOfRange);
        }
        self.writes += 1;
        self.pending.push((offset, buf.to_vec()));
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DeviceError> {
        for (offset, data) in self.pending.drain(..) {
            let start = offset as usize;
            self.durable[start..start + data.len()].copy_from_slice(&data);
        }
        Ok(())
    }

    fn trim(&mut self, offset: u64, len: u64) -> Result<(), DeviceError> {
        if !in_range(self.capacity(), offset, len as usize) {
            return Err(DeviceError::OutOfRange);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_see_unflushed_writes() {
        let mut dev = MemDevice::new(64, 16);
        dev.write_at(4, b"hello").unwrap();
        dev.write_at(6, b"XY").unwrap();
        let mut buf = [0u8; 8];
        dev.read_at(2, &mut buf).unwrap();
        assert_eq!(&buf, b"\0\0heXYo\0");
    }

    #[test]
    fn crash_keeps_only_flushed_and_the_first_pending() {
        let mut dev = MemDevice::new(16, 16);
        dev.write_at(0, b"a").unwrap();
        dev.flush().unwrap();
        dev.write_at(1, b"b").unwrap();
        dev.write_at(2, b"c").unwrap();
        let after = dev.crash(1);
        let mut buf = [0u8; 3];
        after.read_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"ab\0");
        assert_eq!(dev.crash(0).durable[..3], *b"a\0\0");
    }

    #[test]
    fn out_of_range_is_rejected() {
        let mut dev = MemDevice::new(8, 8);
        assert_eq!(dev.write_at(6, b"abc"), Err(DeviceError::OutOfRange));
        assert_eq!(dev.read_at(u64::MAX, &mut [0u8; 1]), Err(DeviceError::OutOfRange));
    }
}
