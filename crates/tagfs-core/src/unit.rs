//! 存储单元接口。
//!
//! 一个单元只管自己的实体、Tag、对象和索引，不保存其他单元的任何数据。单元由各后端实现，
//! 逻辑层不知道有哪些后端。实体和它引用的对象总在同一个单元。卷由若干单元组成，
//! 单元离线时它的实体一起从卷里消失。
//!
//! 单元不做可见性和权限判断，那是卷的事；单元只保证自己的数据一致：一次 [`Unit::apply`]
//! 或 [`Unit::commit_prepared`] 要么全部生效，要么全部不生效，断电后也一样。

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::id::{EntityId, TxnId};
use crate::query::Probe;
use crate::tag::Tag;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct UnitId(pub [u8; 16]);

/// 单元里的一个实体。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityRecord {
    pub id: EntityId,
    /// 按 Tag 的顺序排好、去重。
    pub tags: Vec<Tag>,
    /// 内容长度（字节）。
    pub size: u64,
}

/// 对单元的一项修改。卷已经做完可见性和权限检查，单元只负责照做并保持一致。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// 新建实体和它的对象。
    Create {
        id: EntityId,
        tags: Vec<Tag>,
        content: Vec<u8>,
    },
    /// 新建实体 `id`，和同一单元里的 `from` 引用同一个对象。
    Copy {
        from: EntityId,
        id: EntityId,
        tags: Vec<Tag>,
    },
    /// 删除实体；对象没有别的实体引用时一并回收。
    Delete {
        id: EntityId,
    },
    AddTag {
        id: EntityId,
        tag: Tag,
    },
    RemoveTag {
        id: EntityId,
        tag: Tag,
    },
    /// 从 `offset` 写入；超过当前长度的部分补零扩展。对象被别的实体共享时先复制一份。
    Write {
        id: EntityId,
        offset: u64,
        data: Vec<u8>,
    },
    Truncate {
        id: EntityId,
        len: u64,
    },
}

impl Change {
    /// 这项修改涉及的实体。
    pub fn entity(&self) -> EntityId {
        match self {
            Change::Create { id, .. }
            | Change::Copy { id, .. }
            | Change::Delete { id }
            | Change::AddTag { id, .. }
            | Change::RemoveTag { id, .. }
            | Change::Write { id, .. }
            | Change::Truncate { id, .. } => *id,
        }
    }
}

/// 跨单元事务的描述，随准备记录持久化在每个参与单元里。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxnInfo {
    pub id: TxnId,
    /// 主单元：它的提交就是整个事务的提交点。
    pub primary: UnitId,
    /// 全部参与单元，含主单元。
    pub participants: Vec<UnitId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnitError {
    /// 修改引用了单元里不存在的实体。
    NotFound(EntityId),
    /// 新建的实体已经存在。
    Exists(EntityId),
    /// 空间不够。
    NoSpace,
    /// 读到的数据校验不通过。
    Corrupt,
    /// 底层设备报错。
    Device,
    /// 没有这个事务的准备记录。
    UnknownTxn(TxnId),
    /// 单元按自己的规则拒绝了这组修改（例如映射的 Tag 取值不合法）。原因由单元给出。
    Rejected(&'static str),
}

impl fmt::Display for UnitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnitError::NotFound(id) => write!(f, "entity {id} not found in unit"),
            UnitError::Exists(id) => write!(f, "entity {id} already exists"),
            UnitError::NoSpace => f.write_str("no space left in unit"),
            UnitError::Corrupt => f.write_str("unit data is corrupt"),
            UnitError::Device => f.write_str("device error"),
            UnitError::UnknownTxn(t) => write!(f, "no prepared transaction {t}"),
            UnitError::Rejected(why) => write!(f, "rejected by the unit: {why}"),
        }
    }
}

impl core::error::Error for UnitError {}

pub trait Unit {
    fn id(&self) -> UnitId;

    /// 这个单元映射的 `tagfs` 键：它们反映单元里的实际状态（例如实体对应的宿主文件），
    /// 上层写它就是让单元去改那个状态，所以卷按“修改实体”授权上层写它们。其余 `tagfs` 键只有逻辑层自己写。
    fn mapped_keys(&self) -> &[&'static str] {
        &[]
    }

    /// 当前已提交状态的只读快照。之后的提交不影响它。
    fn snapshot(&self) -> Box<dyn Snapshot>;

    /// 原子地应用一组修改。
    fn apply(&mut self, changes: &[Change]) -> Result<(), UnitError>;

    /// 两阶段提交的第一阶段：持久化事务描述和修改，但不改变已提交状态。
    fn prepare(&mut self, txn: &TxnInfo, changes: &[Change]) -> Result<(), UnitError>;

    /// 所有还没有了结的准备记录。卷在单元上线时据此恢复事务。
    fn prepared(&self) -> Result<Vec<(TxnInfo, Vec<Change>)>, UnitError>;

    /// 应用准备好的修改并删除准备记录，原子地完成。`as_primary` 时同时记下
    /// “这个事务已提交”，这就是整个事务的提交点。
    fn commit_prepared(&mut self, txn: TxnId, as_primary: bool) -> Result<(), UnitError>;

    /// 丢弃准备记录，不应用修改。
    fn abort_prepared(&mut self, txn: TxnId) -> Result<(), UnitError>;

    /// 作为主单元记下的、还没清掉的已提交事务。
    fn committed(&self) -> Result<Vec<TxnInfo>, UnitError>;

    /// 清掉“已提交”记录。卷确认所有参与单元都完成后调用。
    fn forget(&mut self, txn: TxnId) -> Result<(), UnitError>;

    /// 一致性检查，怎么查由单元自己决定。发现问题返回 [`UnitError::Corrupt`] 或别的错误。
    fn check(&mut self) -> Result<(), UnitError>;

    /// 单元对自己的描述（如容量、空闲空间），给人看的名称和值。各后端自己决定说什么。
    fn describe(&self) -> Vec<(&'static str, String)> {
        Vec::new()
    }
}

/// 已提交状态的只读视图。存储单元的读取可能失败（设备出错、校验不通过），
/// 所以每个方法都可能返回错误；这和“不存在”是两回事。
pub trait Snapshot {
    fn get(&self, id: &EntityId) -> Result<Option<EntityRecord>, UnitError>;

    /// 用索引回答叶子条件：满足条件的实体（只看这个条件）。没有相应索引时返回 `None`，
    /// 卷会改为扫描。
    fn probe(&self, probe: &Probe<'_>) -> Result<Option<Vec<EntityId>>, UnitError>;

    /// 单元里的全部实体。
    fn all(&self) -> Result<Vec<EntityId>, UnitError>;

    /// 读实体内容，返回读到的字节数（到达末尾时少于 `buf.len()`）。
    fn read(&self, id: &EntityId, offset: u64, buf: &mut [u8]) -> Result<usize, UnitError>;
}
