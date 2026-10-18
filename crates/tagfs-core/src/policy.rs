//! 可见性和写入规则。
//!
//! TagFS 不定义主体是什么、边界有哪些、谁能写哪个命名空间，这些都由操作系统提供：
//! 操作系统实现 [`Policy`]（或用 [`Rules`] 逐条注册），TagFS 只负责执行。调用上下文的类型
//! 也由操作系统决定（[`Policy::Context`]），TagFS 不看它的内容，只把它交回给规则。
//!
//! `tagfs` 命名空间不受写入规则管辖：其中由 TagFS 自己维护的键任何调用者都不能写，这一点由卷强制；
//! 映射的键（见 [`crate::tag::tagfs_keys::is_mapped`]）写它等于修改实体，按 [`Policy::can_modify`] 授权。

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::tag::{Key, Namespace, Tag, Value};

pub trait Policy {
    /// 可信调用上下文，由操作系统构造并随每次调用传入。
    type Context;

    /// 实体上 `(ns, key)` 这类 Tag 是否构成一道边界。
    fn is_boundary(&self, ns: &Namespace, key: &Key) -> bool;

    /// 主体能否越过实体上的这道边界。`values` 是实体在这个键上的全部值（至少一个）。
    /// 实体可见，当且仅当它身上的每一道边界都能越过。
    fn crosses(&self, ctx: &Self::Context, ns: &Namespace, key: &Key, values: &[&Value]) -> bool;

    /// 主体能否在一个对它可见的实体上增删这个命名空间的 Tag。
    fn can_write(&self, ctx: &Self::Context, ns: &Namespace) -> bool;

    /// 主体能否修改一个对它可见的实体本身：写内容、截断、删除。`tags` 是实体当前的 Tag。
    fn can_modify(&self, ctx: &Self::Context, tags: &[Tag]) -> bool;

    /// 主体创建实体时，操作系统要附加的 Tag（例如归属类的边界）。`None` 表示不允许创建。
    fn on_create(&self, ctx: &Self::Context) -> Option<Vec<Tag>>;
}

type BoundaryRule<C> = Box<dyn Fn(&C, &[&Value]) -> bool>;
type WriteRule<C> = Box<dyn Fn(&C) -> bool>;
type ModifyRule<C> = Box<dyn Fn(&C, &[Tag]) -> bool>;
type CreateRule<C> = Box<dyn Fn(&C) -> Option<Vec<Tag>>>;

/// 逐条注册的规则。没有注册的一律拒绝：没有写入规则的命名空间谁都不能写，
/// 没有修改规则就谁都不能改实体，没有创建规则就谁都不能创建。
pub struct Rules<C> {
    boundaries: BTreeMap<(Namespace, Key), BoundaryRule<C>>,
    writers: BTreeMap<Namespace, WriteRule<C>>,
    modify: Option<ModifyRule<C>>,
    create: Option<CreateRule<C>>,
}

impl<C> Default for Rules<C> {
    fn default() -> Self {
        Rules { boundaries: BTreeMap::new(), writers: BTreeMap::new(), modify: None, create: None }
    }
}

impl<C> Rules<C> {
    pub fn new() -> Self {
        Self::default()
    }

    /// 把实体上的 `ns:key` 注册为边界，`rule` 判断主体能否越过。
    pub fn boundary(mut self, ns: &str, key: &str, rule: impl Fn(&C, &[&Value]) -> bool + 'static) -> Self {
        self.boundaries.insert((Namespace::new(ns), Key::new(key)), Box::new(rule));
        self
    }

    /// 注册命名空间 `ns` 的写入规则。`tagfs` 命名空间注册了也不生效。
    pub fn writer(mut self, ns: &str, rule: impl Fn(&C) -> bool + 'static) -> Self {
        self.writers.insert(Namespace::new(ns), Box::new(rule));
        self
    }

    pub fn modify(mut self, rule: impl Fn(&C, &[Tag]) -> bool + 'static) -> Self {
        self.modify = Some(Box::new(rule));
        self
    }

    pub fn create(mut self, rule: impl Fn(&C) -> Option<Vec<Tag>> + 'static) -> Self {
        self.create = Some(Box::new(rule));
        self
    }
}

impl<C> Policy for Rules<C> {
    type Context = C;

    fn is_boundary(&self, ns: &Namespace, key: &Key) -> bool {
        self.boundaries.keys().any(|(n, k)| n == ns && k == key)
    }

    fn crosses(&self, ctx: &C, ns: &Namespace, key: &Key, values: &[&Value]) -> bool {
        self.boundaries.iter().find(|((n, k), _)| n == ns && k == key).is_some_and(|(_, rule)| rule(ctx, values))
    }

    fn can_write(&self, ctx: &C, ns: &Namespace) -> bool {
        !ns.is_tagfs() && self.writers.get(ns).is_some_and(|rule| rule(ctx))
    }

    fn can_modify(&self, ctx: &C, tags: &[Tag]) -> bool {
        self.modify.as_ref().is_some_and(|rule| rule(ctx, tags))
    }

    fn on_create(&self, ctx: &C) -> Option<Vec<Tag>> {
        self.create.as_ref().and_then(|rule| rule(ctx))
    }
}
