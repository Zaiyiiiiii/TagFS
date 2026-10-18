//! 盘上的编码。
//!
//! 树的键要保持顺序：按字节比较两个键的结果，必须和比较它们代表的值的结果一致。
//! 所以键里的整数用大端，字符串转义后以 `00 00` 结尾（内容里的 `00` 写成 `00 FF`）。
//! 值不参与比较，整数用小端，字符串带长度前缀。

use alloc::string::String;
use alloc::vec::Vec;

use tagfs_core::{Change, EntityId, Key, Namespace, Tag, TxnId, TxnInfo, UnitError, UnitId, Value};

pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.pos == self.buf.len()
    }

    pub(crate) fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }

    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], UnitError> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.buf.len()).ok_or(UnitError::Corrupt)?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, UnitError> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16, UnitError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, UnitError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, UnitError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub(crate) fn be_u64(&mut self) -> Result<u64, UnitError> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub(crate) fn id16(&mut self) -> Result<[u8; 16], UnitError> {
        Ok(self.take(16)?.try_into().unwrap())
    }

    /// 带 u32 长度前缀的字节串。
    pub(crate) fn bytes(&mut self) -> Result<&'a [u8], UnitError> {
        let n = self.u32()? as usize;
        self.take(n)
    }

    /// 转义编码的字节串（键里用）。
    fn escaped(&mut self) -> Result<Vec<u8>, UnitError> {
        let mut out = Vec::new();
        loop {
            match self.u8()? {
                0 => match self.u8()? {
                    0 => return Ok(out),
                    0xff => out.push(0),
                    _ => return Err(UnitError::Corrupt),
                },
                b => out.push(b),
            }
        }
    }

    fn escaped_str(&mut self) -> Result<String, UnitError> {
        String::from_utf8(self.escaped()?).map_err(|_| UnitError::Corrupt)
    }

    /// 键里的 Tag，见 [`put_tag`]。
    pub(crate) fn tag(&mut self) -> Result<Tag, UnitError> {
        let ns = Namespace::new(self.escaped_str()?);
        let key = Key::new(self.escaped_str()?);
        let value = match self.u8()? {
            0 => Value::None,
            1 => Value::Uint(self.be_u64()?),
            2 => Value::Str(self.escaped_str()?),
            _ => return Err(UnitError::Corrupt),
        };
        Ok(Tag { ns, key, value })
    }
}

pub(crate) fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub(crate) fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub(crate) fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub(crate) fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_u32(out, b.len() as u32);
    out.extend_from_slice(b);
}

/// 转义但不加结尾：前缀查询用它，结果是所有以该内容开头的编码的公共前缀。
pub(crate) fn put_escaped_open(out: &mut Vec<u8>, b: &[u8]) {
    for &c in b {
        if c == 0 {
            out.extend_from_slice(&[0, 0xff]);
        } else {
            out.push(c);
        }
    }
}

fn put_escaped(out: &mut Vec<u8>, b: &[u8]) {
    put_escaped_open(out, b);
    out.extend_from_slice(&[0, 0]);
}

/// 命名空间和键，作为同一个键下所有 Tag 的公共前缀。
pub(crate) fn put_ns_key(out: &mut Vec<u8>, ns: &Namespace, key: &Key) {
    put_escaped(out, ns.as_str().as_bytes());
    put_escaped(out, key.as_str().as_bytes());
}

/// 值的类型标记，和 [`Value`] 的排序一致：无值 < uint < 字符串。
pub(crate) const VALUE_NONE: u8 = 0;
pub(crate) const VALUE_UINT: u8 = 1;
pub(crate) const VALUE_STR: u8 = 2;

pub(crate) fn put_value(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::None => out.push(VALUE_NONE),
        Value::Uint(n) => {
            out.push(VALUE_UINT);
            out.extend_from_slice(&n.to_be_bytes());
        }
        Value::Str(s) => {
            out.push(VALUE_STR);
            put_escaped(out, s.as_bytes());
        }
    }
}

/// 保序的 Tag 编码，自带边界，后面可以直接接别的字段。
pub(crate) fn put_tag(out: &mut Vec<u8>, t: &Tag) {
    put_ns_key(out, &t.ns, &t.key);
    put_value(out, &t.value);
}

pub(crate) fn put_txn_info(out: &mut Vec<u8>, info: &TxnInfo) {
    out.extend_from_slice(&info.id.0);
    out.extend_from_slice(&info.primary.0);
    put_u32(out, info.participants.len() as u32);
    for p in &info.participants {
        out.extend_from_slice(&p.0);
    }
}

pub(crate) fn txn_info(r: &mut Reader<'_>) -> Result<TxnInfo, UnitError> {
    let id = TxnId(r.id16()?);
    let primary = UnitId(r.id16()?);
    let n = r.u32()? as usize;
    let mut participants = Vec::new();
    for _ in 0..n {
        participants.push(UnitId(r.id16()?));
    }
    Ok(TxnInfo { id, primary, participants })
}

fn put_tags(out: &mut Vec<u8>, tags: &[Tag]) {
    put_u32(out, tags.len() as u32);
    for t in tags {
        put_tag(out, t);
    }
}

fn tags(r: &mut Reader<'_>) -> Result<Vec<Tag>, UnitError> {
    let n = r.u32()? as usize;
    let mut out = Vec::new();
    for _ in 0..n {
        out.push(r.tag()?);
    }
    Ok(out)
}

pub(crate) fn put_change(out: &mut Vec<u8>, c: &Change) {
    match c {
        Change::Create { id, tags, content } => {
            out.push(1);
            out.extend_from_slice(&id.0);
            put_tags(out, tags);
            put_bytes(out, content);
        }
        Change::Copy { from, id, tags } => {
            out.push(2);
            out.extend_from_slice(&from.0);
            out.extend_from_slice(&id.0);
            put_tags(out, tags);
        }
        Change::Delete { id } => {
            out.push(3);
            out.extend_from_slice(&id.0);
        }
        Change::AddTag { id, tag } => {
            out.push(4);
            out.extend_from_slice(&id.0);
            put_tag(out, tag);
        }
        Change::RemoveTag { id, tag } => {
            out.push(5);
            out.extend_from_slice(&id.0);
            put_tag(out, tag);
        }
        Change::Write { id, offset, data } => {
            out.push(6);
            out.extend_from_slice(&id.0);
            put_u64(out, *offset);
            put_bytes(out, data);
        }
        Change::Truncate { id, len } => {
            out.push(7);
            out.extend_from_slice(&id.0);
            put_u64(out, *len);
        }
    }
}

pub(crate) fn change(r: &mut Reader<'_>) -> Result<Change, UnitError> {
    let kind = r.u8()?;
    let id = EntityId(r.id16()?);
    Ok(match kind {
        1 => Change::Create { id, tags: tags(r)?, content: r.bytes()?.to_vec() },
        2 => Change::Copy { from: id, id: EntityId(r.id16()?), tags: tags(r)? },
        3 => Change::Delete { id },
        4 => Change::AddTag { id, tag: r.tag()? },
        5 => Change::RemoveTag { id, tag: r.tag()? },
        6 => Change::Write { id, offset: r.u64()?, data: r.bytes()?.to_vec() },
        7 => Change::Truncate { id, len: r.u64()? },
        _ => return Err(UnitError::Corrupt),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn enc(t: &Tag) -> Vec<u8> {
        let mut v = Vec::new();
        put_tag(&mut v, t);
        v
    }

    #[test]
    fn tag_encoding_keeps_order_and_round_trips() {
        let mut tags = vec![
            Tag::flag("a", "k"),
            Tag::new("a", "k", 0u64),
            Tag::new("a", "k", 7u64),
            Tag::new("a", "k", 300u64),
            Tag::new("a", "k", ""),
            Tag::new("a", "k", "a"),
            Tag::new("a", "k", "a\0"),
            Tag::new("a", "k", "a\0b"),
            Tag::new("a", "k", "a\u{1}"),
            Tag::new("a", "k\0", ""),
            Tag::new("a", "kk", ""),
            Tag::new("a\0", "", ""),
            Tag::new("b", "", ""),
        ];
        let sorted = tags.clone();
        tags.reverse();
        tags.sort_by_key(enc);
        assert_eq!(tags, sorted);
        for t in &sorted {
            assert_eq!(&Reader::new(&enc(t)).tag().unwrap(), t);
        }
    }

    #[test]
    fn changes_round_trip() {
        let id = EntityId([1; 16]);
        let all = vec![
            Change::Create { id, tags: vec![Tag::new("u", "n", "x")], content: vec![1, 2, 3] },
            Change::Copy { from: id, id: EntityId([2; 16]), tags: vec![] },
            Change::Delete { id },
            Change::AddTag { id, tag: Tag::new("u", "n", 5u64) },
            Change::RemoveTag { id, tag: Tag::flag("u", "s") },
            Change::Write { id, offset: 9, data: vec![0; 4] },
            Change::Truncate { id, len: 3 },
        ];
        let mut buf = Vec::new();
        for c in &all {
            put_change(&mut buf, c);
        }
        let mut r = Reader::new(&buf);
        for c in &all {
            assert_eq!(&change(&mut r).unwrap(), c);
        }
        assert!(r.is_empty());
    }
}
