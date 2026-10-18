//! 运行中的卷（`tagfs-volume`）和各工具之间的通信协议。
//!
//! 走本机套接字（POSIX 上是 Unix 域套接字，Windows 上是命名管道）。每条消息是一个帧：
//! 4 字节小端长度，后面是内容。一条请求对应一条应答。

use std::io::{self, Read, Write};
use std::ops::Bound;

use interprocess::local_socket::prelude::*;
use interprocess::local_socket::{GenericFilePath, GenericNamespaced, Name, Stream};
use tagfs_core::{EntityId, Key, Namespace, Query, Tag, UnitId, Value};

/// 单个帧的上限，防止对方发来离谱的长度。
const MAX_FRAME: u32 = 1 << 30;

/// 一个单元在卷里的登记：用哪个后端、在哪里、现在是否在线。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnitInfo {
    /// 在线时为单元身份。
    pub id: Option<UnitId>,
    pub backend: String,
    pub location: String,
    /// 不在线时，接不上的原因。
    pub offline: Option<String>,
    /// 在线时单元对自己的描述。
    pub details: Vec<(String, String)>,
}

/// 实体的 Tag 和长度。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityInfo {
    pub id: EntityId,
    pub tags: Vec<Tag>,
    pub size: u64,
}

/// 事务里的一项操作。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Create { unit: Option<UnitId>, tags: Vec<Tag>, content: Vec<u8> },
    Copy { from: EntityId, unit: Option<UnitId>, tags: Vec<Tag> },
    Delete { id: EntityId },
    AddTag { id: EntityId, tag: Tag },
    RemoveTag { id: EntityId, tag: Tag },
    Write { id: EntityId, offset: u64, data: Vec<u8> },
    Truncate { id: EntityId, len: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// 卷里登记的全部单元。
    Units,
    /// 用 `backend` 后端把 `location` 上的单元加入卷并上线。
    Attach {
        backend: String,
        location: String,
    },
    /// 把单元移出卷。
    Detach {
        unit: UnitId,
    },
    /// 单元的一致性检查。
    Check {
        unit: UnitId,
    },
    Query {
        query: Query,
        after: Option<EntityId>,
        limit: u32,
    },
    Get {
        id: EntityId,
    },
    Read {
        id: EntityId,
        offset: u64,
        len: u32,
    },
    /// 一个事务：全部生效或全部不生效。
    Apply {
        ops: Vec<Op>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Response {
    Done,
    Error(String),
    Units(Vec<UnitInfo>),
    Unit(UnitId),
    Ids(Vec<EntityId>),
    Entity(EntityInfo),
    Bytes(Vec<u8>),
    /// `Apply` 的结果：每项操作一个，新建和复制给出新实体。
    Applied(Vec<Option<EntityId>>),
}

// ---- 编码 ----

#[derive(Default)]
struct Enc(Vec<u8>);

impl Enc {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
    }
    fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
    fn id16(&mut self, b: &[u8; 16]) {
        self.0.extend_from_slice(b);
    }
    fn opt_unit(&mut self, u: &Option<UnitId>) {
        match u {
            Some(u) => {
                self.u8(1);
                self.id16(&u.0);
            }
            None => self.u8(0),
        }
    }
    fn value(&mut self, v: &Value) {
        match v {
            Value::None => self.u8(0),
            Value::Uint(n) => {
                self.u8(1);
                self.u64(*n);
            }
            Value::Str(s) => {
                self.u8(2);
                self.str(s);
            }
        }
    }
    fn ns_key(&mut self, ns: &Namespace, key: &Key) {
        self.str(ns.as_str());
        self.str(key.as_str());
    }
    fn tag(&mut self, t: &Tag) {
        self.ns_key(&t.ns, &t.key);
        self.value(&t.value);
    }
    fn tags(&mut self, tags: &[Tag]) {
        self.u32(tags.len() as u32);
        for t in tags {
            self.tag(t);
        }
    }
    fn bound(&mut self, b: &Bound<Value>) {
        match b {
            Bound::Unbounded => self.u8(0),
            Bound::Included(v) => {
                self.u8(1);
                self.value(v);
            }
            Bound::Excluded(v) => {
                self.u8(2);
                self.value(v);
            }
        }
    }
    fn query(&mut self, q: &Query) {
        match q {
            Query::All => self.u8(0),
            Query::Has { ns, key } => {
                self.u8(1);
                self.ns_key(ns, key);
            }
            Query::Is(t) => {
                self.u8(2);
                self.tag(t);
            }
            Query::Range { ns, key, lower, upper } => {
                self.u8(3);
                self.ns_key(ns, key);
                self.bound(lower);
                self.bound(upper);
            }
            Query::Prefix { ns, key, prefix } => {
                self.u8(4);
                self.ns_key(ns, key);
                self.str(prefix);
            }
            Query::And(qs) | Query::Or(qs) => {
                self.u8(if matches!(q, Query::And(_)) { 5 } else { 6 });
                self.u32(qs.len() as u32);
                for q in qs {
                    self.query(q);
                }
            }
            Query::Not(q) => {
                self.u8(7);
                self.query(q);
            }
        }
    }
}

struct Dec<'a>(&'a [u8]);

fn bad() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "malformed tagfs message")
}

impl<'a> Dec<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(bad());
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn bytes(&mut self) -> io::Result<Vec<u8>> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }
    fn str(&mut self) -> io::Result<String> {
        String::from_utf8(self.bytes()?).map_err(|_| bad())
    }
    fn id16(&mut self) -> io::Result<[u8; 16]> {
        Ok(self.take(16)?.try_into().unwrap())
    }
    fn entity(&mut self) -> io::Result<EntityId> {
        Ok(EntityId(self.id16()?))
    }
    fn unit(&mut self) -> io::Result<UnitId> {
        Ok(UnitId(self.id16()?))
    }
    fn opt_unit(&mut self) -> io::Result<Option<UnitId>> {
        Ok(if self.u8()? == 1 { Some(self.unit()?) } else { None })
    }
    fn value(&mut self) -> io::Result<Value> {
        Ok(match self.u8()? {
            0 => Value::None,
            1 => Value::Uint(self.u64()?),
            2 => Value::Str(self.str()?),
            _ => return Err(bad()),
        })
    }
    fn ns_key(&mut self) -> io::Result<(Namespace, Key)> {
        Ok((Namespace::new(self.str()?), Key::new(self.str()?)))
    }
    fn tag(&mut self) -> io::Result<Tag> {
        let (ns, key) = self.ns_key()?;
        Ok(Tag { ns, key, value: self.value()? })
    }
    fn tags(&mut self) -> io::Result<Vec<Tag>> {
        (0..self.u32()?).map(|_| self.tag()).collect()
    }
    fn bound(&mut self) -> io::Result<Bound<Value>> {
        Ok(match self.u8()? {
            0 => Bound::Unbounded,
            1 => Bound::Included(self.value()?),
            2 => Bound::Excluded(self.value()?),
            _ => return Err(bad()),
        })
    }
    fn query(&mut self, depth: u32) -> io::Result<Query> {
        if depth > 64 {
            return Err(bad());
        }
        Ok(match self.u8()? {
            0 => Query::All,
            1 => {
                let (ns, key) = self.ns_key()?;
                Query::Has { ns, key }
            }
            2 => Query::Is(self.tag()?),
            3 => {
                let (ns, key) = self.ns_key()?;
                Query::Range { ns, key, lower: self.bound()?, upper: self.bound()? }
            }
            4 => {
                let (ns, key) = self.ns_key()?;
                Query::Prefix { ns, key, prefix: self.str()? }
            }
            k @ (5 | 6) => {
                let qs = (0..self.u32()?).map(|_| self.query(depth + 1)).collect::<io::Result<Vec<_>>>()?;
                if k == 5 { Query::And(qs) } else { Query::Or(qs) }
            }
            7 => Query::Not(Box::new(self.query(depth + 1)?)),
            _ => return Err(bad()),
        })
    }
    fn end(&self) -> io::Result<()> {
        if self.0.is_empty() { Ok(()) } else { Err(bad()) }
    }
}

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::default();
        match self {
            Request::Units => e.u8(0),
            Request::Attach { backend, location } => {
                e.u8(1);
                e.str(backend);
                e.str(location);
            }
            Request::Detach { unit } => {
                e.u8(2);
                e.id16(&unit.0);
            }
            Request::Check { unit } => {
                e.u8(3);
                e.id16(&unit.0);
            }
            Request::Query { query, after, limit } => {
                e.u8(4);
                e.query(query);
                match after {
                    Some(a) => {
                        e.u8(1);
                        e.id16(&a.0);
                    }
                    None => e.u8(0),
                }
                e.u32(*limit);
            }
            Request::Get { id } => {
                e.u8(5);
                e.id16(&id.0);
            }
            Request::Read { id, offset, len } => {
                e.u8(6);
                e.id16(&id.0);
                e.u64(*offset);
                e.u32(*len);
            }
            Request::Apply { ops } => {
                e.u8(7);
                e.u32(ops.len() as u32);
                for op in ops {
                    match op {
                        Op::Create { unit, tags, content } => {
                            e.u8(0);
                            e.opt_unit(unit);
                            e.tags(tags);
                            e.bytes(content);
                        }
                        Op::Copy { from, unit, tags } => {
                            e.u8(1);
                            e.id16(&from.0);
                            e.opt_unit(unit);
                            e.tags(tags);
                        }
                        Op::Delete { id } => {
                            e.u8(2);
                            e.id16(&id.0);
                        }
                        Op::AddTag { id, tag } | Op::RemoveTag { id, tag } => {
                            e.u8(if matches!(op, Op::AddTag { .. }) { 3 } else { 4 });
                            e.id16(&id.0);
                            e.tag(tag);
                        }
                        Op::Write { id, offset, data } => {
                            e.u8(5);
                            e.id16(&id.0);
                            e.u64(*offset);
                            e.bytes(data);
                        }
                        Op::Truncate { id, len } => {
                            e.u8(6);
                            e.id16(&id.0);
                            e.u64(*len);
                        }
                    }
                }
            }
        }
        e.0
    }

    pub fn decode(b: &[u8]) -> io::Result<Request> {
        let mut d = Dec(b);
        let r = match d.u8()? {
            0 => Request::Units,
            1 => Request::Attach { backend: d.str()?, location: d.str()? },
            2 => Request::Detach { unit: d.unit()? },
            3 => Request::Check { unit: d.unit()? },
            4 => {
                let query = d.query(0)?;
                let after = if d.u8()? == 1 { Some(d.entity()?) } else { None };
                Request::Query { query, after, limit: d.u32()? }
            }
            5 => Request::Get { id: d.entity()? },
            6 => Request::Read { id: d.entity()?, offset: d.u64()?, len: d.u32()? },
            7 => {
                let mut ops = Vec::new();
                for _ in 0..d.u32()? {
                    ops.push(match d.u8()? {
                        0 => Op::Create { unit: d.opt_unit()?, tags: d.tags()?, content: d.bytes()? },
                        1 => Op::Copy { from: d.entity()?, unit: d.opt_unit()?, tags: d.tags()? },
                        2 => Op::Delete { id: d.entity()? },
                        3 => Op::AddTag { id: d.entity()?, tag: d.tag()? },
                        4 => Op::RemoveTag { id: d.entity()?, tag: d.tag()? },
                        5 => Op::Write { id: d.entity()?, offset: d.u64()?, data: d.bytes()? },
                        6 => Op::Truncate { id: d.entity()?, len: d.u64()? },
                        _ => return Err(bad()),
                    });
                }
                Request::Apply { ops }
            }
            _ => return Err(bad()),
        };
        d.end()?;
        Ok(r)
    }
}

impl Response {
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::default();
        match self {
            Response::Done => e.u8(0),
            Response::Error(m) => {
                e.u8(1);
                e.str(m);
            }
            Response::Units(units) => {
                e.u8(2);
                e.u32(units.len() as u32);
                for u in units {
                    e.opt_unit(&u.id);
                    e.str(&u.backend);
                    e.str(&u.location);
                    match &u.offline {
                        Some(why) => {
                            e.u8(1);
                            e.str(why);
                        }
                        None => e.u8(0),
                    }
                    e.u32(u.details.len() as u32);
                    for (k, v) in &u.details {
                        e.str(k);
                        e.str(v);
                    }
                }
            }
            Response::Unit(u) => {
                e.u8(3);
                e.id16(&u.0);
            }
            Response::Ids(ids) => {
                e.u8(4);
                e.u32(ids.len() as u32);
                for id in ids {
                    e.id16(&id.0);
                }
            }
            Response::Entity(info) => {
                e.u8(5);
                e.id16(&info.id.0);
                e.tags(&info.tags);
                e.u64(info.size);
            }
            Response::Bytes(b) => {
                e.u8(6);
                e.bytes(b);
            }
            Response::Applied(results) => {
                e.u8(7);
                e.u32(results.len() as u32);
                for r in results {
                    match r {
                        Some(id) => {
                            e.u8(1);
                            e.id16(&id.0);
                        }
                        None => e.u8(0),
                    }
                }
            }
        }
        e.0
    }

    pub fn decode(b: &[u8]) -> io::Result<Response> {
        let mut d = Dec(b);
        let r = match d.u8()? {
            0 => Response::Done,
            1 => Response::Error(d.str()?),
            2 => {
                let mut units = Vec::new();
                for _ in 0..d.u32()? {
                    let id = d.opt_unit()?;
                    let backend = d.str()?;
                    let location = d.str()?;
                    let offline = if d.u8()? == 1 { Some(d.str()?) } else { None };
                    let details = (0..d.u32()?).map(|_| Ok((d.str()?, d.str()?))).collect::<io::Result<_>>()?;
                    units.push(UnitInfo { id, backend, location, offline, details });
                }
                Response::Units(units)
            }
            3 => Response::Unit(d.unit()?),
            4 => Response::Ids((0..d.u32()?).map(|_| d.entity()).collect::<io::Result<_>>()?),
            5 => Response::Entity(EntityInfo { id: d.entity()?, tags: d.tags()?, size: d.u64()? }),
            6 => Response::Bytes(d.bytes()?),
            7 => Response::Applied(
                (0..d.u32()?)
                    .map(|_| Ok(if d.u8()? == 1 { Some(d.entity()?) } else { None }))
                    .collect::<io::Result<_>>()?,
            ),
            _ => return Err(bad()),
        };
        d.end()?;
        Ok(r)
    }
}

// ---- 帧 ----

pub fn write_frame(w: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

/// 读一帧；对方正常关闭连接时返回 `None`。
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len);
    if len > MAX_FRAME {
        return Err(bad());
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

// ---- 地址和客户端 ----

/// 卷的本机套接字名。环境变量 `TAGFS_VOLUME` 可以指定：含路径分隔符时是文件路径，否则是名字。
pub fn socket_name() -> io::Result<Name<'static>> {
    let configured = std::env::var("TAGFS_VOLUME").ok();
    let name = configured.unwrap_or_else(|| String::from("tagfs-volume.sock"));
    to_name(name)
}

pub fn to_name(name: String) -> io::Result<Name<'static>> {
    if name.contains('/') || name.contains('\\') {
        name.to_fs_name::<GenericFilePath>()
    } else if GenericNamespaced::is_supported() {
        name.to_ns_name::<GenericNamespaced>()
    } else {
        std::env::temp_dir().join(name).into_os_string().to_fs_name::<GenericFilePath>()
    }
}

/// 连到运行中的卷。
pub struct Client {
    conn: Stream,
}

impl Client {
    pub fn connect() -> io::Result<Client> {
        Self::connect_to(socket_name()?)
    }

    pub fn connect_to(name: Name<'_>) -> io::Result<Client> {
        Ok(Client { conn: Stream::connect(name)? })
    }

    pub fn call(&mut self, req: &Request) -> io::Result<Response> {
        write_frame(&mut self.conn, &req.encode())?;
        let frame = read_frame(&mut self.conn)?.ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
        Response::decode(&frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_responses_round_trip() {
        let id = EntityId([3; 16]);
        let unit = UnitId([4; 16]);
        let tag = Tag::new("user", "name", "a");
        let reqs = vec![
            Request::Units,
            Request::Attach { backend: "file".into(), location: "/x/u.img".into() },
            Request::Detach { unit },
            Request::Check { unit },
            Request::Query {
                query: Query::And(vec![
                    Query::has("user", "name"),
                    Query::Not(Box::new(Query::Or(vec![Query::is("app", "n", 3u64)]))),
                    Query::Range {
                        ns: Namespace::new("a"),
                        key: Key::new("b"),
                        lower: Bound::Included(Value::Uint(1)),
                        upper: Bound::Excluded(Value::str("z")),
                    },
                    Query::Prefix { ns: Namespace::new("a"), key: Key::new("b"), prefix: "p".into() },
                    Query::All,
                ]),
                after: Some(id),
                limit: 10,
            },
            Request::Get { id },
            Request::Read { id, offset: 7, len: 9 },
            Request::Apply {
                ops: vec![
                    Op::Create { unit: Some(unit), tags: vec![tag.clone(), Tag::flag("u", "f")], content: vec![1, 2] },
                    Op::Copy { from: id, unit: None, tags: vec![] },
                    Op::Delete { id },
                    Op::AddTag { id, tag: tag.clone() },
                    Op::RemoveTag { id, tag: tag.clone() },
                    Op::Write { id, offset: 1, data: vec![9] },
                    Op::Truncate { id, len: 0 },
                ],
            },
        ];
        for r in reqs {
            assert_eq!(Request::decode(&r.encode()).unwrap(), r);
        }
        let resps = vec![
            Response::Done,
            Response::Error("x".into()),
            Response::Units(vec![
                UnitInfo {
                    id: Some(unit),
                    backend: "file".into(),
                    location: "/u".into(),
                    offline: None,
                    details: vec![("size".into(), "1 bytes".into())],
                },
                UnitInfo {
                    id: None,
                    backend: "disk".into(),
                    location: "/dev/x".into(),
                    offline: Some("gone".into()),
                    details: vec![],
                },
            ]),
            Response::Unit(unit),
            Response::Ids(vec![id, id]),
            Response::Entity(EntityInfo { id, tags: vec![tag], size: 5 }),
            Response::Bytes(vec![1, 2, 3]),
            Response::Applied(vec![Some(id), None]),
        ];
        for r in resps {
            assert_eq!(Response::decode(&r.encode()).unwrap(), r);
        }
        assert!(Request::decode(&[99]).is_err());
        assert!(Request::decode(&[0, 0]).is_err(), "trailing bytes");
    }
}
