//! 运行中的卷。
//!
//! - 卷由哪些单元组成记在状态目录的 `units` 文件里（每行：后端名、制表符、位置）。启动时逐个让它们上线；
//!   接不上的（例如移动盘没插）记为离线，之后列出单元时会再试。
//! - 后端都编译在这个进程里，按名字找：`file`、`disk`、`hostdir`、`mem`。后端特有的只有“怎样从一个位置
//!   打开单元”，之后的读写、查询、检查都走统一的单元接口。
//! - 权限属于操作系统层；操作系统接入之前，卷用不设限的策略。
//! - 请求一条一条处理（单元不是线程安全的）。

use std::io;
use std::path::{Path, PathBuf};

use interprocess::local_socket::Listener;
use interprocess::local_socket::prelude::*;
use tagfs::{EntityId, SystemIds, Unit, UnitId, Unrestricted, Volume};
use tagfs_proto::{EntityInfo, Op, Request, Response, UnitInfo, read_frame, write_frame};

/// 状态目录里记录单元的文件。
const UNITS_FILE: &str = "units";
/// 单次读取的上限。
const MAX_READ: u32 = 16 << 20;

struct Entry {
    backend: String,
    location: String,
    unit: Option<UnitId>,
    offline: Option<String>,
}

pub struct Service {
    state: PathBuf,
    vol: Volume<Unrestricted>,
    entries: Vec<Entry>,
}

/// 用名字为 `backend` 的后端打开 `location` 上的单元。后端特有的只有这一步。
fn open_unit(backend: &str, location: &str) -> Result<Box<dyn Unit>, String> {
    let e = |e: &dyn std::fmt::Display| e.to_string();
    Ok(match backend {
        "file" => Box::new(tagfs::image::open(location).map_err(|x| e(&x))?),
        "disk" => Box::new(tagfs::disk::open(location).map_err(|x| e(&x))?),
        "hostdir" => Box::new(tagfs::DirUnit::open(location, Box::new(SystemIds)).map_err(|x| e(&x))?.0),
        "mem" => Box::new(tagfs::MemUnit::new(tagfs::new_unit_id())),
        other => return Err(format!("unknown backend {other:?}")),
    })
}

/// 位置的规范写法：文件和目录用绝对路径，这样从哪里调用都指同一个东西。
fn normalize(backend: &str, location: &str) -> Result<String, String> {
    match backend {
        "file" | "hostdir" => std::fs::canonicalize(location)
            .map(|p| p.to_string_lossy().into_owned())
            .map_err(|e| format!("{location}: {e}")),
        _ => Ok(location.to_string()),
    }
}

impl Service {
    /// 用状态目录 `state` 启动：读出卷的单元并让它们上线。
    pub fn start(state: impl AsRef<Path>) -> io::Result<Service> {
        let state = state.as_ref().to_path_buf();
        std::fs::create_dir_all(&state)?;
        let mut me = Service { state, vol: Volume::new(Unrestricted, Box::new(SystemIds)), entries: Vec::new() };
        let listed = match std::fs::read_to_string(me.state.join(UNITS_FILE)) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        for line in listed.lines() {
            if let Some((backend, location)) = line.split_once('\t') {
                me.entries.push(Entry {
                    backend: backend.to_string(),
                    location: location.to_string(),
                    unit: None,
                    offline: None,
                });
            }
        }
        me.bring_online();
        Ok(me)
    }

    /// 让离线的单元再试着上线。
    fn bring_online(&mut self) {
        for i in 0..self.entries.len() {
            if self.entries[i].unit.is_some() {
                continue;
            }
            let entry = &self.entries[i];
            let result = open_unit(&entry.backend, &entry.location).and_then(|unit| {
                let id = unit.id();
                if self.vol.unit(id).is_some() {
                    return Err(format!("unit {} is already online", hex(&id.0)));
                }
                self.vol.attach(unit).map_err(|e| e.to_string())?;
                Ok(id)
            });
            let entry = &mut self.entries[i];
            match result {
                Ok(id) => (entry.unit, entry.offline) = (Some(id), None),
                Err(why) => entry.offline = Some(why),
            }
        }
    }

    /// 把单元清单写回状态目录（先写临时文件再改名，不会写坏）。内存单元不记：重启后它本来就没了。
    fn save(&self) -> io::Result<()> {
        let mut text = String::new();
        for e in self.entries.iter().filter(|e| e.backend != "mem") {
            text.push_str(&e.backend);
            text.push('\t');
            text.push_str(&e.location);
            text.push('\n');
        }
        let tmp = self.state.join(format!("{UNITS_FILE}.tmp"));
        std::fs::write(&tmp, text)?;
        std::fs::rename(tmp, self.state.join(UNITS_FILE))
    }

    pub fn handle(&mut self, req: Request) -> Response {
        match self.dispatch(req) {
            Ok(r) => r,
            Err(e) => Response::Error(e),
        }
    }

    fn dispatch(&mut self, req: Request) -> Result<Response, String> {
        let err = |e: tagfs::Error| e.to_string();
        Ok(match req {
            Request::Units => {
                self.bring_online();
                Response::Units(
                    self.entries
                        .iter()
                        .map(|e| UnitInfo {
                            id: e.unit,
                            backend: e.backend.clone(),
                            location: e.location.clone(),
                            offline: e.offline.clone(),
                            details: e
                                .unit
                                .and_then(|id| self.vol.unit(id))
                                .map(|u| u.describe().into_iter().map(|(k, v)| (k.to_string(), v)).collect())
                                .unwrap_or_default(),
                        })
                        .collect(),
                )
            }
            Request::Attach { backend, location } => {
                let location = normalize(&backend, &location)?;
                if backend != "mem" && self.entries.iter().any(|e| e.backend == backend && e.location == location) {
                    return Err(format!("{location} is already part of the volume"));
                }
                let unit = open_unit(&backend, &location)?;
                let id = unit.id();
                if self.vol.unit(id).is_some() {
                    return Err(format!("unit {} is already online", hex(&id.0)));
                }
                self.vol.attach(unit).map_err(err)?;
                self.entries.push(Entry { backend, location, unit: Some(id), offline: None });
                self.save().map_err(|e| e.to_string())?;
                Response::Unit(id)
            }
            Request::Detach { unit } => {
                let i = self.entries.iter().position(|e| e.unit == Some(unit)).ok_or("no such unit")?;
                self.vol.detach(unit);
                self.entries.remove(i);
                self.save().map_err(|e| e.to_string())?;
                Response::Done
            }
            Request::Check { unit } => {
                let mut u = self.vol.detach(unit).ok_or("no such unit online")?;
                let checked = u.check();
                self.vol.attach(u).map_err(err)?;
                checked.map_err(|e| e.to_string())?;
                Response::Done
            }
            Request::Query { query, after, limit } => {
                Response::Ids(self.vol.query_page(&(), &query, after.as_ref(), limit as usize).map_err(err)?)
            }
            Request::Get { id } => {
                let e = self.vol.get(&(), &id).map_err(err)?;
                Response::Entity(EntityInfo { id: e.id, tags: e.tags, size: e.size })
            }
            Request::Read { id, offset, len } => {
                let mut buf = vec![0u8; len.min(MAX_READ) as usize];
                let n = self.vol.read(&(), &id, offset, &mut buf).map_err(err)?;
                buf.truncate(n);
                Response::Bytes(buf)
            }
            Request::Apply { ops } => {
                let mut tx = self.vol.begin(&());
                let mut results: Vec<Option<EntityId>> = Vec::new();
                for op in ops {
                    results.push(match op {
                        Op::Create { unit, tags, content } => Some(tx.create(unit, tags, content).map_err(err)?),
                        Op::Copy { from, unit, tags } => Some(tx.copy(&from, unit, tags).map_err(err)?),
                        Op::Delete { id } => tx.delete(&id).map(|_| None).map_err(err)?,
                        Op::AddTag { id, tag } => tx.add_tag(&id, tag).map(|_| None).map_err(err)?,
                        Op::RemoveTag { id, tag } => tx.remove_tag(&id, tag).map(|_| None).map_err(err)?,
                        Op::Write { id, offset, data } => tx.write(&id, offset, data).map(|_| None).map_err(err)?,
                        Op::Truncate { id, len } => tx.truncate(&id, len).map(|_| None).map_err(err)?,
                    });
                }
                tx.commit().map_err(err)?;
                Response::Applied(results)
            }
        })
    }

    /// 在 `listener` 上一个连接一个连接地服务，直到出错。
    pub fn serve(&mut self, listener: &Listener) -> io::Result<()> {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            // 一个连接出错只断开它自己。
            let _ = self.serve_conn(&mut conn);
        }
        Ok(())
    }

    fn serve_conn(&mut self, conn: &mut interprocess::local_socket::Stream) -> io::Result<()> {
        while let Some(frame) = read_frame(conn)? {
            let resp = match Request::decode(&frame) {
                Ok(req) => self.handle(req),
                Err(e) => Response::Error(e.to_string()),
            };
            write_frame(conn, &resp.encode())?;
        }
        Ok(())
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// 默认的状态目录。环境变量 `TAGFS_STATE` 可以指定。
pub fn default_state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("TAGFS_STATE") {
        return PathBuf::from(dir);
    }
    #[cfg(windows)]
    if let Some(dir) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(dir).join("tagfs");
    }
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(dir).join("tagfs");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/state/tagfs");
    }
    std::env::temp_dir().join("tagfs")
}

/// 在 `name` 上开始监听。已经有卷在这个名字上应答时报错；上一个卷没正常退出、留下了套接字文件时覆盖它。
pub fn listen(name: interprocess::local_socket::Name<'_>) -> io::Result<Listener> {
    if interprocess::local_socket::Stream::connect(name.borrow()).is_ok() {
        return Err(io::Error::new(io::ErrorKind::AddrInUse, "another tagfs-volume is already running"));
    }
    interprocess::local_socket::ListenerOptions::new().name(name).try_overwrite(true).create_sync()
}

#[cfg(test)]
mod tests;
