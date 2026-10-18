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

/// 命名空间名的最大长度（字节）。
pub const MAX_NAMESPACE_LEN: usize = 64;
/// 键的最大长度（字节）。
pub const MAX_KEY_LEN: usize = 64;
/// 字符串值的最大长度（字节），和 Linux 的 `PATH_MAX` 一样。
pub const MAX_STR_LEN: usize = 4096;

impl Tag {
    /// 长度是否在限制之内。存储单元要把 Tag 放进索引键，键的长度必须有上限。
    pub fn within_limits(&self) -> bool {
        self.ns.as_str().len() <= MAX_NAMESPACE_LEN
            && self.key.as_str().len() <= MAX_KEY_LEN
            && match &self.value {
                Value::Str(s) => s.len() <= MAX_STR_LEN,
                _ => true,
            }
    }
}

/// TagFS 自己写入的 Tag 的键。
pub mod tagfs_keys {
    /// 跨单元事务尚未在所有参与单元完成，值为事务编号。带它的实体在结果确定前只读。
    pub const TXN_PENDING: &str = "txn_pending";

    /// 宿主层级文件系统（Windows、Linux 这一类按目录树组织的文件系统）里的路径。
    ///
    /// 只在目录单元里有意义：值是以挂载目录为根的路径，如 `/abc/bcd/1.file`，实体的内容就是那个文件。
    /// 这个 Tag 不是存下来的，而是映射出来的：增、删、改它，就是在宿主上建立、删除、移动对应的文件。
    /// 其他单元里它只是普通的 Tag。
    pub const HOSTFS_PATH: &str = "hostfs_path";

    /// 上层可以写的 `tagfs` 键。它们映射到存储单元里的实际状态，写它就是改那个状态，
    /// 所以按“修改实体”来授权（[`crate::Policy::can_modify`]）。其余 `tagfs` 键只有 TagFS 自己写。
    pub fn is_mapped(key: &str) -> bool {
        key == HOSTFS_PATH
    }
}
