//! 运行中的卷。
//!
//! - 卷由哪些单元组成记在状态目录的 `units` 文件里（每行：后端名、制表符、位置）。启动时逐个让它们上线；
//!   接不上的（例如移动盘没插）记为离线，之后列出单元时会再试。
//! - 后端都编译在这个进程里，按名字找：`file`、`disk`、`hostdir`、`mem`。后端特有的只有“怎样从一个位置
//!   打开单元”，之后的读写、查询、检查都走统一的单元接口。
//! - 权限属于操作系统层；操作系统接入之前，卷用不设限的策略。
//! - 每个连接一个线程，各自独立处理。卷本身可以同时处理多个请求：读不等写，修改按实体排队。
//! - 内容写入不立刻落盘（和宿主的文件系统一样），每隔 [`FLUSH_INTERVAL`] 统一落盘一次；
//!   工具要求时（[`Request::Flush`]）立刻落盘。

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use interprocess::local_socket::Listener;
use interprocess::local_socket::Stream;
use interprocess::local_socket::prelude::*;
use tagfs::{EntityId, Stage, SystemIds, Unit, UnitId, Unrestricted, Volume};
use tagfs_proto::{CHUNK, EntityInfo, Op, Request, Response, UnitInfo, read_frame, write_frame};

/// 状态目录里记录单元的文件。
const UNITS_FILE: &str = "units";
/// 多久统一落盘一次。
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

struct Entry {
    backend: String,
    location: String,
    unit: Option<UnitId>,
    offline: Option<String>,
}

pub struct Service {
    state: PathBuf,
    vol: Volume<Unrestricted>,
    entries: Mutex<Vec<Entry>>,
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
        let listed = match std::fs::read_to_string(state.join(UNITS_FILE)) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        let entries = listed
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .map(|(backend, location)| Entry {
                backend: backend.to_string(),
                location: location.to_string(),
                unit: None,
                offline: None,
            })
            .collect();
        let me = Service { state, vol: Volume::new(Unrestricted, Box::new(SystemIds)), entries: Mutex::new(entries) };
        me.bring_online(&mut me.entries.lock().unwrap());
        Ok(me)
    }

    /// 卷本身。
    pub fn volume(&self) -> &Volume<Unrestricted> {
        &self.vol
    }

    /// 一个新会话（一个连接一个）。
    pub fn session(&self) -> Session<'_> {
        Session { service: self, stages: BTreeMap::new(), next: 0 }
    }

    /// 让离线的单元再试着上线。
    fn bring_online(&self, entries: &mut [Entry]) {
        for entry in entries.iter_mut().filter(|e| e.unit.is_none()) {
            let result = open_unit(&entry.backend, &entry.location).and_then(|unit| {
                let id = unit.id();
                if self.vol.unit(id).is_some() {
                    return Err(format!("unit {} is already online", hex(&id.0)));
                }
                self.vol.attach(unit.into()).map_err(|e| e.to_string())?;
                Ok(id)
            });
            match result {
                Ok(id) => (entry.unit, entry.offline) = (Some(id), None),
                Err(why) => entry.offline = Some(why),
            }
        }
    }

    /// 把单元清单写回状态目录（先写临时文件再改名，不会写坏）。内存单元不记：重启后它本来就没了。
    fn save(&self, entries: &[Entry]) -> io::Result<()> {
        let mut text = String::new();
        for e in entries.iter().filter(|e| e.backend != "mem") {
            text.push_str(&e.backend);
            text.push('\t');
            text.push_str(&e.location);
            text.push('\n');
        }
        let tmp = self.state.join(format!("{UNITS_FILE}.tmp"));
        std::fs::write(&tmp, text)?;
        std::fs::rename(tmp, self.state.join(UNITS_FILE))
    }

    /// 在 `listener` 上服务，每个连接一个线程；另有一个线程定时把写入落盘。
    pub fn serve(self: &Arc<Self>, listener: &Listener) -> io::Result<()> {
        let flusher = self.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(FLUSH_INTERVAL);
                if let Err(e) = flusher.vol.sync() {
                    eprintln!("tagfs-volume: flushing failed: {e}");
                }
            }
        });
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let me = self.clone();
            // 一个连接出错只断开它自己。
            std::thread::spawn(move || {
                let _ = me.serve_conn(conn);
            });
        }
        Ok(())
    }

    fn serve_conn(&self, mut conn: Stream) -> io::Result<()> {
        let mut session = self.session();
        while let Some(frame) = read_frame(&mut conn)? {
            let req = match Request::decode(&frame) {
                Ok(req) => req,
                Err(e) => {
                    write_frame(&mut conn, &Response::Error(e.to_string()).encode())?;
                    continue;
                }
            };
            match req {
                Request::Read { id, offset, len } => self.send_content(&mut conn, &id, offset, len)?,
                Request::Write { id, offset } => {
                    let resp = receive(&mut conn, offset, |at, data| {
                        self.vol.write(&(), &id, at, data).map_err(|e| e.to_string())
                    })?;
                    write_frame(&mut conn, &resp.encode())?;
                }
                Request::WriteStaged { stage, offset } => {
                    let resp = receive(&mut conn, offset, |at, data| session.write_staged(stage, at, data))?;
                    write_frame(&mut conn, &resp.encode())?;
                }
                other => write_frame(&mut conn, &session.handle(other).encode())?,
            }
        }
        Ok(())
    }

    /// 流式应答一次读：一段一段读出来就发，缓冲区只有一段大。
    fn send_content(&self, conn: &mut Stream, id: &EntityId, offset: u64, len: u64) -> io::Result<()> {
        let mut buf = vec![0u8; CHUNK];
        let (mut at, mut left) = (offset, len);
        while left > 0 {
            let want = (left.min(CHUNK as u64)) as usize;
            let n = match self.vol.read(&(), id, at, &mut buf[..want]) {
                Ok(n) => n,
                Err(e) => return write_frame(conn, &Response::Error(e.to_string()).encode()),
            };
            if n == 0 {
                break;
            }
            write_frame(conn, &Response::Bytes(buf[..n].to_vec()).encode())?;
            at += n as u64;
            left -= n as u64;
        }
        write_frame(conn, &Response::Done.encode())
    }
}

/// 收一次流式写入的数据帧（直到空帧），每段交给 `f`。出错之后剩下的数据照样收完（连接才能接着用），
/// 但不再写。
fn receive(
    conn: &mut Stream,
    offset: u64,
    mut f: impl FnMut(u64, &[u8]) -> Result<(), String>,
) -> io::Result<Response> {
    let mut at = offset;
    let mut failed = None;
    loop {
        let chunk = read_frame(conn)?.ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
        if chunk.is_empty() {
            break;
        }
        if failed.is_none()
            && let Err(e) = f(at, &chunk)
        {
            failed = Some(e);
        }
        at += chunk.len() as u64;
    }
    Ok(failed.map_or(Response::Done, Response::Error))
}

/// 一个连接上的会话。它发起的暂存归它：连接断开（会话结束）时没用上的都丢掉。
pub struct Session<'a> {
    service: &'a Service,
    stages: BTreeMap<u64, Stage>,
    next: u64,
}

impl Session<'_> {
    /// 写这个会话的一份暂存。
    pub fn write_staged(&self, stage: u64, offset: u64, data: &[u8]) -> Result<(), String> {
        let stage = self.stages.get(&stage).ok_or("no such stage on this connection")?;
        stage.write(offset, data).map_err(|e| e.to_string())
    }

    /// 处理一条非流式的请求。
    pub fn handle(&mut self, req: Request) -> Response {
        match self.dispatch(req) {
            Ok(r) => r,
            Err(e) => Response::Error(e),
        }
    }

    fn dispatch(&mut self, req: Request) -> Result<Response, String> {
        let s = self.service;
        let vol = &s.vol;
        let err = |e: tagfs::Error| e.to_string();
        Ok(match req {
            Request::Units => {
                let mut entries = s.entries.lock().unwrap();
                s.bring_online(&mut entries);
                Response::Units(
                    entries
                        .iter()
                        .map(|e| UnitInfo {
                            id: e.unit,
                            backend: e.backend.clone(),
                            location: e.location.clone(),
                            offline: e.offline.clone(),
                            details: e
                                .unit
                                .and_then(|id| vol.unit(id))
                                .map(|u| u.describe().into_iter().map(|(k, v)| (k.to_string(), v)).collect())
                                .unwrap_or_default(),
                        })
                        .collect(),
                )
            }
            Request::Attach { backend, location } => {
                let location = normalize(&backend, &location)?;
                let mut entries = s.entries.lock().unwrap();
                if backend != "mem" && entries.iter().any(|e| e.backend == backend && e.location == location) {
                    return Err(format!("{location} is already part of the volume"));
                }
                let unit = open_unit(&backend, &location)?;
                let id = unit.id();
                if vol.unit(id).is_some() {
                    return Err(format!("unit {} is already online", hex(&id.0)));
                }
                vol.attach(unit.into()).map_err(err)?;
                entries.push(Entry { backend, location, unit: Some(id), offline: None });
                s.save(&entries).map_err(|e| e.to_string())?;
                Response::Unit(id)
            }
            Request::Detach { unit } => {
                let mut entries = s.entries.lock().unwrap();
                let i = entries.iter().position(|e| e.unit == Some(unit)).ok_or("no such unit")?;
                vol.detach(unit);
                entries.remove(i);
                s.save(&entries).map_err(|e| e.to_string())?;
                Response::Done
            }
            Request::Check { unit } => {
                vol.unit(unit).ok_or("no such unit online")?.check().map_err(|e| e.to_string())?;
                Response::Done
            }
            Request::Query { query, after, limit } => {
                Response::Ids(vol.query_page(&(), &query, after.as_ref(), limit as usize).map_err(err)?)
            }
            Request::Get { id } => {
                let e = vol.get(&(), &id).map_err(err)?;
                Response::Entity(EntityInfo { id: e.id, tags: e.tags, size: e.size })
            }
            Request::SetLen { id, len } => {
                vol.set_len(&(), &id, len).map_err(err)?;
                Response::Done
            }
            Request::Flush { id } => {
                vol.flush(&(), &id).map_err(err)?;
                Response::Done
            }
            Request::Stage { unit } => {
                let stage = vol.stage(unit).map_err(err)?;
                self.next += 1;
                self.stages.insert(self.next, stage);
                Response::Staged(self.next)
            }
            Request::DiscardStage { stage } => {
                self.stages.remove(&stage).ok_or("no such stage on this connection")?;
                Response::Done
            }
            Request::Apply { ops } => {
                let mut tx = vol.begin(&());
                let mut results: Vec<Option<EntityId>> = Vec::new();
                for op in ops {
                    results.push(match op {
                        Op::Create { unit, tags } => Some(tx.create(unit, tags).map_err(err)?),
                        // 用到的暂存交给事务：成功就成了实体的内容，失败就随事务丢掉。
                        Op::CreateFrom { stage, tags } => {
                            let stage = self.stages.remove(&stage).ok_or("no such stage on this connection")?;
                            Some(tx.create_from(stage, tags).map_err(err)?)
                        }
                        Op::Copy { from, unit, tags } => Some(tx.copy(&from, unit, tags).map_err(err)?),
                        Op::Delete { id } => tx.delete(&id).map(|_| None).map_err(err)?,
                        Op::AddTag { id, tag } => tx.add_tag(&id, tag).map(|_| None).map_err(err)?,
                        Op::RemoveTag { id, tag } => tx.remove_tag(&id, tag).map(|_| None).map_err(err)?,
                    });
                }
                tx.commit().map_err(err)?;
                Response::Applied(results)
            }
            Request::Read { .. } | Request::Write { .. } | Request::WriteStaged { .. } => {
                return Err(String::from("content is streamed over a connection"));
            }
        })
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
    if Stream::connect(name.borrow()).is_ok() {
        return Err(io::Error::new(io::ErrorKind::AddrInUse, "another tagfs-volume is already running"));
    }
    interprocess::local_socket::ListenerOptions::new().name(name).try_overwrite(true).create_sync()
}

#[cfg(test)]
mod tests;
