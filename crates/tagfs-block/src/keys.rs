//! 树里的键。第一个字节区分种类，后面的字段都用保序编码，所以同一种类、同一前缀的条目相邻。
//!
//! | 种类 | 键 | 值 |
//! |---|---|---|
//! | 实体 | `01` 实体 | 对象编号 |
//! | 实体的 Tag | `02` 实体 Tag | 空 |
//! | Tag 索引 | `03` Tag 实体 | 空 |
//! | 对象 | `04` 对象编号 | 引用数、长度、内联内容 |
//! | 对象区段 | `05` 对象编号 逻辑块号 | 物理块号、块数 |
//! | 准备记录 | `06` 事务 片号 | 记录的一片 |
//! | 已提交事务 | `07` 事务 | 事务描述 |
//! | 附加记录 | `08` 任意 | 任意（上层单元自用） |

use alloc::vec;
use alloc::vec::Vec;

use tagfs_core::{EntityId, Tag, TxnId};

use crate::codec::put_tag;

pub(crate) const ENTITY: u8 = 1;
pub(crate) const ENTITY_TAG: u8 = 2;
pub(crate) const INDEX: u8 = 3;
pub(crate) const OBJECT: u8 = 4;
pub(crate) const EXTENT: u8 = 5;
pub(crate) const PREPARED: u8 = 6;
pub(crate) const COMMITTED: u8 = 7;
/// 建在块格式单元之上的单元（如目录单元）自己的记录，键和值由它决定。
pub(crate) const AUX: u8 = 8;

pub(crate) fn aux(key: &[u8]) -> Vec<u8> {
    let mut k = vec![AUX];
    k.extend_from_slice(key);
    k
}

pub(crate) fn entity(id: &EntityId) -> Vec<u8> {
    let mut k = vec![ENTITY];
    k.extend_from_slice(&id.0);
    k
}

pub(crate) fn entity_tags(id: &EntityId) -> Vec<u8> {
    let mut k = vec![ENTITY_TAG];
    k.extend_from_slice(&id.0);
    k
}

pub(crate) fn entity_tag(id: &EntityId, tag: &Tag) -> Vec<u8> {
    let mut k = entity_tags(id);
    put_tag(&mut k, tag);
    k
}

pub(crate) fn index(tag: &Tag, id: &EntityId) -> Vec<u8> {
    let mut k = vec![INDEX];
    put_tag(&mut k, tag);
    k.extend_from_slice(&id.0);
    k
}

pub(crate) fn object(o: u64) -> Vec<u8> {
    let mut k = vec![OBJECT];
    k.extend_from_slice(&o.to_be_bytes());
    k
}

pub(crate) fn extents(o: u64) -> Vec<u8> {
    let mut k = vec![EXTENT];
    k.extend_from_slice(&o.to_be_bytes());
    k
}

pub(crate) fn extent(o: u64, logical: u64) -> Vec<u8> {
    let mut k = extents(o);
    k.extend_from_slice(&logical.to_be_bytes());
    k
}

pub(crate) fn prepared(txn: &TxnId) -> Vec<u8> {
    let mut k = vec![PREPARED];
    k.extend_from_slice(&txn.0);
    k
}

pub(crate) fn prepared_chunk(txn: &TxnId, chunk: u32) -> Vec<u8> {
    let mut k = prepared(txn);
    k.extend_from_slice(&chunk.to_be_bytes());
    k
}

pub(crate) fn committed(txn: &TxnId) -> Vec<u8> {
    let mut k = vec![COMMITTED];
    k.extend_from_slice(&txn.0);
    k
}
