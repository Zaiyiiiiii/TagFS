//! TagFS 的逻辑层：实体、Tag、可见性、查询，以及由存储单元组成的卷和它上面的事务。
//!
//! 这里不碰任何存储：存储单元由各后端实现 [`unit::Unit`] 接入，逻辑层不知道有哪些后端。

#![no_std]

extern crate alloc;

pub mod id;
pub mod policy;
pub mod query;
pub mod tag;
pub mod unit;
pub mod volume;

pub use id::{EntityId, IdSource, TxnId};
pub use policy::{Policy, Rules};
pub use query::{Probe, Query};
pub use tag::{Key, Namespace, Tag, Value};
pub use unit::{Change, EntityRecord, Snapshot, TxnInfo, Unit, UnitError, UnitId};
pub use volume::{Entity, Error, Txn, Volume};
