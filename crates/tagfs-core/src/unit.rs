//! 存储单元接口。
//!
//! 一个单元只管自己的实体、Tag、对象和索引，不保存其他单元的任何数据。单元由各后端实现，
//! 逻辑层不知道有哪些后端。实体和它引用的对象总在同一个单元。卷由若干单元组成，
//! 单元离线时它的实体一起从卷里消失。
//!
//! 单元不做可见性和权限判断，那是卷的事；单元只保证自己的数据一致。
//!
//! 和 NTFS、ext4 这类文件系统一样，元数据和内容分开处理：
//!
//! - **元数据**（实体、Tag、实体引用哪个对象）按事务修改：一次 [`Unit::apply`] 或 [`Unit::commit_prepared`]
//!   要么全部生效，要么全部不生效，返回时已经落盘，断电后也一样。修改里不带内容，所以总是很小。
//! - **内容**按偏移直接读写存储（[`Unit::write`]、[`Unit::set_len`]），不经过事务，也不在内存里攒着；
//!   写下去立刻对读者可见，[`Unit::sync`] 之后（或下一次元数据提交之后）才保证落盘。断电时丢失的只是
//!   尚未落盘的写入，单元本身始终一致。
//! - 需要和元数据一起原子出现的内容（新建实体时的内容、跨单元复制），先写进**暂存**（[`Unit::stage`]）：
//!   暂存的内容放在单元的空闲空间里，不属于任何实体；建实体时引用它，随那次提交一起生效。
//!   没用上的暂存丢掉即可，断电后单元自己回收。
//!
//! 单元可以被多个线程同时使用：读不等写；互不相干的写各做各的。同一个实体的内容写入由调用者
//! 排好先后（卷按实体加锁），单元不替调用者排。

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::id::{EntityId, TxnId};
use crate::query::Probe;
use crate::tag::Tag;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct UnitId(pub [u8; 16]);

/// 单元里的一份暂存内容，见 [`Unit::stage`]。编号由单元分配，只在这个单元里有意义。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct StageId(pub u64);

/// 单元里的一个实体。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityRecord {
    pub id: EntityId,
    /// 按 Tag 的顺序排好、去重。
    pub tags: Vec<Tag>,
    /// 内容长度（字节）。
    pub size: u64,
}

/// 对单元元数据的一项修改。卷已经做完可见性和权限检查，单元只负责照做并保持一致。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// 新建实体和它的对象。`content` 是暂存的内容，用上之后暂存就不在了；为空时内容为空。
    Create {
        id: EntityId,
        tags: Vec<Tag>,
        content: Option<StageId>,
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
}

impl Change {
    /// 这项修改涉及的实体。
    pub fn entity(&self) -> EntityId {
        match self {
            Change::Create { id, .. }
            | Change::Copy { id, .. }
            | Change::Delete { id }
            | Change::AddTag { id, .. }
            | Change::RemoveTag { id, .. } => *id,
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
    /// 没有这份暂存（已经用掉、丢掉，或从来没有）。
    UnknownStage(StageId),
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
            UnitError::UnknownStage(s) => write!(f, "no staged content {}", s.0),
            UnitError::Rejected(why) => write!(f, "rejected by the unit: {why}"),
        }
    }
}

impl core::error::Error for UnitError {}

pub trait Unit: Send + Sync {
    fn id(&self) -> UnitId;

    /// 这个单元映射的 `tagfs` 键：它们反映单元里的实际状态（例如实体对应的宿主文件），
    /// 上层写它就是让单元去改那个状态，所以卷按“修改实体”授权上层写它们。其余 `tagfs` 键只有逻辑层自己写。
    fn mapped_keys(&self) -> &[&'static str] {
        &[]
    }

    /// 当前状态的只读视图，含已经写入、尚未落盘的内容。之后的修改不影响它。
    fn snapshot(&self) -> Box<dyn Snapshot>;

    // ---- 内容 ----

    /// 从 `offset` 起写实体的内容，超过当前长度的部分补零扩展。对象被别的实体共享时先复制一份给它。
    fn write(&self, id: &EntityId, offset: u64, data: &[u8]) -> Result<(), UnitError>;

    /// 把实体的内容截短或补零加长到 `len`。
    fn set_len(&self, id: &EntityId, len: u64) -> Result<(), UnitError>;

    /// 返回时，之前的内容写入都已落盘。
    fn sync(&self) -> Result<(), UnitError>;

    // ---- 暂存 ----

    /// 新的一份空暂存。
    fn stage(&self) -> Result<StageId, UnitError>;

    /// 写暂存的内容，规则同 [`Unit::write`]。
    fn write_staged(&self, stage: StageId, offset: u64, data: &[u8]) -> Result<(), UnitError>;

    /// 丢掉一份没用上的暂存。
    fn discard_staged(&self, stage: StageId) -> Result<(), UnitError>;

    // ---- 元数据 ----

    /// 原子地应用一组修改。修改引用的暂存随之用掉；失败时暂存原样留着。
    fn apply(&self, changes: &[Change]) -> Result<(), UnitError>;

    /// 两阶段提交的第一阶段：持久化事务描述和修改，但不改变实体。修改引用的暂存从此归这条准备记录，
    /// 随它一起落盘，提交时用掉，放弃时丢掉。
    fn prepare(&self, txn: &TxnInfo, changes: &[Change]) -> Result<(), UnitError>;

    /// 所有还没有了结的准备记录。卷在单元上线时据此恢复事务。
    fn prepared(&self) -> Result<Vec<(TxnInfo, Vec<Change>)>, UnitError>;

    /// 应用准备好的修改并删除准备记录，原子地完成。`as_primary` 时同时记下
    /// “这个事务已提交”，这就是整个事务的提交点。
    fn commit_prepared(&self, txn: TxnId, as_primary: bool) -> Result<(), UnitError>;

    /// 丢弃准备记录，不应用修改。
    fn abort_prepared(&self, txn: TxnId) -> Result<(), UnitError>;

    /// 作为主单元记下的、还没清掉的已提交事务。
    fn committed(&self) -> Result<Vec<TxnInfo>, UnitError>;

    /// 清掉“已提交”记录。卷确认所有参与单元都完成后调用。
    fn forget(&self, txn: TxnId) -> Result<(), UnitError>;

    /// 一致性检查，怎么查由单元自己决定。发现问题返回 [`UnitError::Corrupt`] 或别的错误。
    fn check(&self) -> Result<(), UnitError>;

    /// 单元对自己的描述（如容量、空闲空间），给人看的名称和值。各后端自己决定说什么。
    fn describe(&self) -> Vec<(&'static str, String)> {
        Vec::new()
    }
}

/// 某一时刻的只读视图。存储单元的读取可能失败（设备出错、校验不通过），
/// 所以每个方法都可能返回错误；这和“不存在”是两回事。
pub trait Snapshot: Send + Sync {
    fn get(&self, id: &EntityId) -> Result<Option<EntityRecord>, UnitError>;

    /// 用索引回答叶子条件：满足条件的实体（只看这个条件）。没有相应索引时返回 `None`，
    /// 卷会改为扫描。
    fn probe(&self, probe: &Probe<'_>) -> Result<Option<Vec<EntityId>>, UnitError>;

    /// 单元里的全部实体。
    fn all(&self) -> Result<Vec<EntityId>, UnitError>;

    /// 读实体内容，返回读到的字节数（到达末尾时少于 `buf.len()`）。
    fn read(&self, id: &EntityId, offset: u64, buf: &mut [u8]) -> Result<usize, UnitError>;
}
