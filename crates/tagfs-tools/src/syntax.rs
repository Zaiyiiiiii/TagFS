//! 命令行上 Tag 和查询条件的写法。这是工具自己的约定，TagFS 本身没有字符串语法。
//!
//! Tag：`ns:key`（无值）、`ns:key=文本`（字符串）、`ns:key=#123`（uint）。
//!
//! 查询条件（多个之间是“且”）：
//!
//! - `ns:key` 带这个键；`ns:key=值` 等于；`ns:key^=前缀` 字符串前缀；
//! - `ns:key>=值`、`ns:key>值`、`ns:key<=值`、`ns:key<值` 范围；
//! - 前面加 `!` 取反。
//!
//! 值以 `#` 开头且后面全是数字时是 uint，否则是字符串。

use std::ops::Bound;

use tagfs::{Key, Namespace, Query, Tag, Value};

fn value(s: &str) -> Value {
    match s.strip_prefix('#') {
        Some(n) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => match n.parse() {
            Ok(v) => Value::Uint(v),
            Err(_) => Value::str(s),
        },
        _ => Value::str(s),
    }
}

fn split_ns(s: &str) -> Result<(&str, &str), String> {
    match s.split_once(':') {
        Some((ns, rest)) if !ns.is_empty() && !rest.is_empty() => Ok((ns, rest)),
        _ => Err(format!("expected ns:key, got {s:?}")),
    }
}

pub fn tag(s: &str) -> Result<Tag, String> {
    let (ns, rest) = split_ns(s)?;
    Ok(match rest.split_once('=') {
        Some((key, v)) if !key.is_empty() => Tag::new(ns, key, value(v)),
        Some(_) => return Err(format!("missing key in {s:?}")),
        None => Tag::flag(ns, rest),
    })
}

/// 一个查询条件。
fn term(s: &str) -> Result<Query, String> {
    if let Some(inner) = s.strip_prefix('!') {
        return Ok(Query::Not(Box::new(term(inner)?)));
    }
    let (ns_name, rest) = split_ns(s)?;
    // 取最先出现的运算符（同一位置两个字符的优先），值里再出现运算符也不受影响。
    let found = (0..rest.len()).filter(|&i| rest.is_char_boundary(i)).find_map(|i| {
        ["^=", ">=", "<=", "=", ">", "<"].into_iter().find(|op| rest[i..].starts_with(op)).map(|op| (i, op))
    });
    if let Some((at, op)) = found {
        let (key, v) = (&rest[..at], &rest[at + op.len()..]);
        if key.is_empty() {
            return Err(format!("missing key in {s:?}"));
        }
        let (ns, key) = (Namespace::new(ns_name), Key::new(key));
        let range = |lower, upper| Query::Range { ns: ns.clone(), key: key.clone(), lower, upper };
        return Ok(match op {
            "^=" => Query::Prefix { ns, key, prefix: v.to_string() },
            "=" => Query::Is(Tag { ns, key, value: value(v) }),
            ">=" => range(Bound::Included(value(v)), Bound::Unbounded),
            ">" => range(Bound::Excluded(value(v)), Bound::Unbounded),
            "<=" => range(Bound::Unbounded, Bound::Included(value(v))),
            _ => range(Bound::Unbounded, Bound::Excluded(value(v))),
        });
    }
    Ok(Query::Has { ns: Namespace::new(ns_name), key: Key::new(rest) })
}

/// 多个条件取“且”；没有条件就是全部。
pub fn query(terms: &[String]) -> Result<Query, String> {
    let mut qs = terms.iter().map(|t| term(t)).collect::<Result<Vec<_>, _>>()?;
    Ok(match qs.len() {
        0 => Query::All,
        1 => qs.pop().unwrap(),
        _ => Query::And(qs),
    })
}

/// 和 [`tag`] 相反，用于显示。
pub fn show_tag(t: &Tag) -> String {
    let base = format!("{}:{}", t.ns.as_str(), t.key.as_str());
    match &t.value {
        Value::None => base,
        Value::Uint(v) => format!("{base}=#{v}"),
        Value::Str(v) => format!("{base}={v}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags() {
        assert_eq!(tag("user:starred").unwrap(), Tag::flag("user", "starred"));
        assert_eq!(tag("user:name=a=b").unwrap(), Tag::new("user", "name", "a=b"));
        assert_eq!(tag("app:plays=#12").unwrap(), Tag::new("app", "plays", 12u64));
        assert_eq!(tag("app:code=#x1").unwrap(), Tag::new("app", "code", "#x1"));
        assert_eq!(tag("tagfs:hostfs_path=/a/b.txt").unwrap(), Tag::new("tagfs", "hostfs_path", "/a/b.txt"));
        assert!(tag("nonamespace").is_err());
        assert!(tag(":k").is_err());
        for t in ["user:starred", "user:name=x", "app:plays=#3"] {
            assert_eq!(show_tag(&tag(t).unwrap()), t);
        }
    }

    #[test]
    fn queries() {
        let q = |s: &[&str]| query(&s.iter().map(|x| x.to_string()).collect::<Vec<_>>()).unwrap();
        assert_eq!(q(&[]), Query::All);
        assert_eq!(q(&["user:name"]), Query::has("user", "name"));
        assert_eq!(q(&["user:name=a"]), Query::is("user", "name", "a"));
        assert_eq!(q(&["user:name=a>=b"]), Query::is("user", "name", "a>=b"));
        assert_eq!(
            q(&["app:plays>=#3"]),
            Query::Range {
                ns: Namespace::new("app"),
                key: Key::new("plays"),
                lower: Bound::Included(Value::Uint(3)),
                upper: Bound::Unbounded
            }
        );
        assert_eq!(
            q(&["user:name^=so", "!user:hidden"]),
            Query::And(vec![
                Query::Prefix { ns: Namespace::new("user"), key: Key::new("name"), prefix: "so".into() },
                Query::Not(Box::new(Query::has("user", "hidden"))),
            ])
        );
    }
}
