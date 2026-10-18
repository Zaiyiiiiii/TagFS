//! Tag：`(命名空间, 键, 值)`。
//!
//! TagFS 只认识自己的命名空间 [`Namespace::TAGFS`]，其余命名空间对它都是不透明的名字，
//! 由上层定义。

use alloc::string::String;
use core::fmt;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Namespace(String);

impl Namespace {
    /// TagFS 自用的命名空间名。只有 TagFS 自己能写。
    pub const TAGFS: &'static str = "tagfs";

    pub fn new(name: impl Into<String>) -> Self {
        Namespace(name.into())
    }

    pub fn tagfs() -> Self {
        Namespace(String::from(Self::TAGFS))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_tagfs(&self) -> bool {
        self.0 == Self::TAGFS
    }
}

impl fmt::Debug for Namespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(String);

impl Key {
    pub fn new(name: impl Into<String>) -> Self {
        Key(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Tag 的值只有三种。没有数组：要多个值就打多个同名 Tag。
///
/// 排序：无值 < 所有 uint < 所有字符串；uint 按数值，字符串按字节。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Value {
    None,
    Uint(u64),
    Str(String),
}

impl Value {
    pub fn str(s: impl Into<String>) -> Self {
        Value::Str(s.into())
    }

    pub(crate) fn same_kind(&self, other: &Value) -> bool {
        core::mem::discriminant(self) == core::mem::discriminant(other)
    }
}

impl From<u64> for Value {
    fn from(v: u64) -> Self {
        Value::Uint(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Str(String::from(v))
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Str(v)
    }
}

/// 一个实体上，(命名空间, 键, 值) 完全相同的 Tag 只算一个：重复添加没有效果。
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tag {
    pub ns: Namespace,
    pub key: Key,
    pub value: Value,
}

impl Tag {
    pub fn new(ns: &str, key: &str, value: impl Into<Value>) -> Self {
        Tag { ns: Namespace::new(ns), key: Key::new(key), value: value.into() }
    }

    /// 无值 Tag：只表示存在。
    pub fn flag(ns: &str, key: &str) -> Self {
        Tag { ns: Namespace::new(ns), key: Key::new(key), value: Value::None }
    }

    pub fn is(&self, ns: &Namespace, key: &Key) -> bool {
        &self.ns == ns && &self.key == key
    }
}

impl fmt::Debug for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.value {
            Value::None => write!(f, "{:?}:{:?}", self.ns, self.key),
            Value::Uint(v) => write!(f, "{:?}:{:?}={v}", self.ns, self.key),
            Value::Str(v) => write!(f, "{:?}:{:?}={v:?}", self.ns, self.key),
        }
    }
}

/// TagFS 自己写入的 Tag 的键。
pub mod tagfs_keys {
    /// 跨单元事务尚未在所有参与单元完成，值为事务编号。带它的实体在结果确定前只读。
    pub const TXN_PENDING: &str = "txn_pending";
}
