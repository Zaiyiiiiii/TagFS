//! 实体和事务的身份。

use core::fmt;
use core::str::FromStr;

/// 实体身份：128 位，全局唯一，永不复用。
///
/// 按 UUIDv7 的布局生成（48 位毫秒时间戳在前），同一时段创建的实体在索引里相邻。
/// 时间和随机数由宿主通过 [`IdSource`] 提供，TagFS 本身不读时钟、不取随机数。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntityId(pub [u8; 16]);

/// 跨单元事务的编号，生成方式同 [`EntityId`]。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TxnId(pub [u8; 16]);

/// 生成身份所需的时间和随机数，由宿主提供。
pub trait IdSource {
    /// 自 Unix 纪元起的毫秒数。
    fn unix_millis(&mut self) -> u64;
    /// 密码学安全的随机字节。
    fn fill_random(&mut self, buf: &mut [u8]);
}

/// 按 UUIDv7 布局拼出 16 字节。
pub fn uuid_v7(unix_millis: u64, random: [u8; 10]) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..6].copy_from_slice(&unix_millis.to_be_bytes()[2..]);
    b[6..].copy_from_slice(&random);
    b[6] = 0x70 | (b[6] & 0x0f); // 版本 7
    b[8] = 0x80 | (b[8] & 0x3f); // RFC 9562 变体
    b
}

pub(crate) fn next_v7(src: &mut dyn IdSource) -> [u8; 16] {
    let millis = src.unix_millis();
    let mut random = [0u8; 10];
    src.fill_random(&mut random);
    uuid_v7(millis, random)
}

impl EntityId {
    pub fn generate(src: &mut dyn IdSource) -> Self {
        EntityId(next_v7(src))
    }
}

impl TxnId {
    pub fn generate(src: &mut dyn IdSource) -> Self {
        TxnId(next_v7(src))
    }
}

/// 8-4-4-4-12 的小写十六进制，和 UUID 的常见写法一致。实体之间的关系就是以这个字符串为值的 Tag。
impl fmt::Display for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hyphenated(f, &self.0)
    }
}

impl fmt::Debug for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EntityId({self})")
    }
}

impl fmt::Display for TxnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hyphenated(f, &self.0)
    }
}

fn write_hyphenated(f: &mut fmt::Formatter<'_>, b: &[u8; 16]) -> fmt::Result {
    for (i, byte) in b.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            f.write_str("-")?;
        }
        write!(f, "{byte:02x}")?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseIdError;

impl fmt::Display for ParseIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not an entity id")
    }
}

impl FromStr for EntityId {
    type Err = ParseIdError;

    /// 只接受 [`fmt::Display`] 输出的那种写法（大小写均可）。
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.as_bytes();
        if s.len() != 36 {
            return Err(ParseIdError);
        }
        let mut out = [0u8; 16];
        let mut pos = 0;
        for (i, slot) in out.iter_mut().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                if s[pos] != b'-' {
                    return Err(ParseIdError);
                }
                pos += 1;
            }
            *slot = (hex(s[pos])? << 4) | hex(s[pos + 1])?;
            pos += 2;
        }
        Ok(EntityId(out))
    }
}

fn hex(c: u8) -> Result<u8, ParseIdError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(ParseIdError),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn v7_layout() {
        let b = uuid_v7(0x0123_4567_89ab, [0xff; 10]);
        assert_eq!(&b[..6], &[0x01, 0x23, 0x45, 0x67, 0x89, 0xab]);
        assert_eq!(b[6] >> 4, 7);
        assert_eq!(b[8] >> 6, 0b10);
    }

    #[test]
    fn display_round_trips() {
        let id = EntityId(uuid_v7(1_700_000_000_000, [7; 10]));
        let text = id.to_string();
        assert_eq!(text.len(), 36);
        assert_eq!(text.parse::<EntityId>(), Ok(id));
        assert_eq!(text.to_uppercase().parse::<EntityId>(), Ok(id));
        assert!("not-an-id".parse::<EntityId>().is_err());
    }

    #[test]
    fn ids_sort_by_time() {
        let a = EntityId(uuid_v7(1, [0xff; 10]));
        let b = EntityId(uuid_v7(2, [0; 10]));
        assert!(a < b);
    }
}
