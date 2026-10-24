//! 查询表达式。核心里没有字符串语法，上层自己决定怎么书写。
//!
//! 调用者只写条件。可见性由卷在执行时强制并入，查询里既不需要、也没办法写权限条件。

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Bound;

use crate::tag::{Key, Namespace, Tag, Value};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Query {
    /// 所有（可见的）实体。
    All,
    /// 带某个键，不论值是什么。
    Has {
        ns: Namespace,
        key: Key,
    },
    /// 带某个 Tag（命名空间、键、值都相同）。
    Is(Tag),
    /// 某个键的值落在范围里。只和同类的值比较：uint 范围不匹配字符串值，反之亦然。
    Range {
        ns: Namespace,
        key: Key,
        lower: Bound<Value>,
        upper: Bound<Value>,
    },
    /// 某个键的字符串值以 `prefix` 开头。
    Prefix {
        ns: Namespace,
        key: Key,
        prefix: String,
    },
    /// 某个键的字符串值是 `parent` 下一层：以 `parent` 开头，余下的部分不空、不含 `/`。
    /// 路径形式的值（`tagfs:hostfs_path` 这一类）用它列出一个目录的直接子项，`parent` 是
    /// 目录的值加上 `/`。索引跳过更深的层级，代价只和直接子项的个数有关。
    Children {
        ns: Namespace,
        key: Key,
        parent: String,
    },
    And(Vec<Query>),
    Or(Vec<Query>),
    Not(Box<Query>),
}

/// 存储单元的索引能直接回答的叶子条件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe<'a> {
    Has { ns: &'a Namespace, key: &'a Key },
    Is(&'a Tag),
    Range { ns: &'a Namespace, key: &'a Key, lower: &'a Bound<Value>, upper: &'a Bound<Value> },
    Prefix { ns: &'a Namespace, key: &'a Key, prefix: &'a str },
    Children { ns: &'a Namespace, key: &'a Key, parent: &'a str },
}

impl Query {
    pub fn has(ns: &str, key: &str) -> Self {
        Query::Has { ns: Namespace::new(ns), key: Key::new(key) }
    }

    pub fn is(ns: &str, key: &str, value: impl Into<Value>) -> Self {
        Query::Is(Tag::new(ns, key, value))
    }

    /// 叶子条件对应的索引探测；组合条件没有。
    pub fn probe(&self) -> Option<Probe<'_>> {
        Some(match self {
            Query::Has { ns, key } => Probe::Has { ns, key },
            Query::Is(tag) => Probe::Is(tag),
            Query::Range { ns, key, lower, upper } => Probe::Range { ns, key, lower, upper },
            Query::Prefix { ns, key, prefix } => Probe::Prefix { ns, key, prefix },
            Query::Children { ns, key, parent } => Probe::Children { ns, key, parent },
            _ => return None,
        })
    }

    /// 一个实体的 Tag 集合是否满足这个条件。
    pub fn matches(&self, tags: &[Tag]) -> bool {
        match self {
            Query::All => true,
            Query::And(qs) => qs.iter().all(|q| q.matches(tags)),
            Query::Or(qs) => qs.iter().any(|q| q.matches(tags)),
            Query::Not(q) => !q.matches(tags),
            leaf => {
                let probe = leaf.probe().expect("leaf");
                tags.iter().any(|t| probe.matches(t))
            }
        }
    }
}

impl Probe<'_> {
    /// 单个 Tag 是否满足这个叶子条件。存储单元的索引实现必须和它的结果一致。
    pub fn matches(&self, t: &Tag) -> bool {
        match *self {
            Probe::Has { ns, key } => t.is(ns, key),
            Probe::Is(tag) => t == tag,
            Probe::Range { ns, key, lower, upper } => t.is(ns, key) && in_bounds(&t.value, lower, upper),
            Probe::Prefix { ns, key, prefix } => {
                t.is(ns, key) && matches!(&t.value, Value::Str(s) if s.starts_with(prefix))
            }
            Probe::Children { ns, key, parent } => {
                t.is(ns, key) && matches!(&t.value, Value::Str(s) if is_child(s, parent))
            }
        }
    }
}

/// `value` 是否在 `parent` 下一层。
pub fn is_child(value: &str, parent: &str) -> bool {
    value.strip_prefix(parent).is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
}

fn in_bounds(v: &Value, lower: &Bound<Value>, upper: &Bound<Value>) -> bool {
    let lower_ok = match lower {
        Bound::Unbounded => true,
        Bound::Included(b) => b.same_kind(v) && v >= b,
        Bound::Excluded(b) => b.same_kind(v) && v > b,
    };
    let upper_ok = match upper {
        Bound::Unbounded => true,
        Bound::Included(b) => b.same_kind(v) && v <= b,
        Bound::Excluded(b) => b.same_kind(v) && v < b,
    };
    // 两端都不限时，只要求这个键存在。
    lower_ok && upper_ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn tags() -> Vec<Tag> {
        vec![Tag::new("user", "name", "song.mp3"), Tag::new("app", "plays", 12u64), Tag::flag("user", "starred")]
    }

    #[test]
    fn leaves() {
        let t = tags();
        assert!(Query::has("user", "starred").matches(&t));
        assert!(Query::is("user", "name", "song.mp3").matches(&t));
        assert!(!Query::is("user", "name", "song").matches(&t));
        let range = |lo: u64, hi: u64| Query::Range {
            ns: Namespace::new("app"),
            key: Key::new("plays"),
            lower: Bound::Included(Value::Uint(lo)),
            upper: Bound::Excluded(Value::Uint(hi)),
        };
        assert!(range(10, 13).matches(&t));
        assert!(!range(13, 20).matches(&t));
        let prefix = Query::Prefix { ns: Namespace::new("user"), key: Key::new("name"), prefix: "song".into() };
        assert!(prefix.matches(&t));
    }

    #[test]
    fn children_are_one_level_down() {
        let children = |v: &str, parent: &str| {
            Query::Children { ns: Namespace::new("u"), key: Key::new("p"), parent: parent.into() }
                .matches(&[Tag::new("u", "p", v)])
        };
        assert!(children("/a/b", "/a/"));
        assert!(!children("/a/b/c", "/a/"), "deeper");
        assert!(!children("/a/", "/a/"), "nothing below the parent");
        assert!(!children("/ab", "/a/"));
        assert!(children("/x", "/"));
        assert!(!children("/x/y", "/"));
    }

    #[test]
    fn range_does_not_mix_kinds() {
        let t = vec![Tag::new("user", "v", "5")];
        let q = Query::Range {
            ns: Namespace::new("user"),
            key: Key::new("v"),
            lower: Bound::Included(Value::Uint(0)),
            upper: Bound::Unbounded,
        };
        assert!(!q.matches(&t));
    }

    #[test]
    fn combinators() {
        let t = tags();
        let q = Query::And(vec![
            Query::has("user", "starred"),
            Query::Not(Box::new(Query::is("user", "name", "other"))),
            Query::Or(vec![Query::has("x", "y"), Query::has("app", "plays")]),
        ]);
        assert!(q.matches(&t));
    }
}
