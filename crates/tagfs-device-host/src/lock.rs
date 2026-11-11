//! 跨进程锁。
//!
//! 锁所在目录的文件系统上系统文件锁起作用，就用系统的（[`Mode::System`]）：锁住那里的一个锁文件。
//! 不起作用时（Android 的共享存储、某些 FUSE、网络盘）换成这里自己实现的（[`Mode::Own`]），锁文件仍放在
//! 原处，只是不靠它来锁：
//!
//! - 每把锁由锁文件的规范路径算出一个抽象命名空间里的 Unix 套接字名字，绑上这个名字就是拿到了锁，绑不上
//!   就是别的进程拿着。进程退出或崩溃时内核替它关掉套接字、放掉锁，不会留下没人放的锁，也不用在别处
//!   写任何东西。
//! - 共享持有（[`Shared`]）时每个持有者绑一个自己的名字，并在锁旁边的 `<锁文件名>.own/` 里放一个同名的
//!   标记文件，好让别人数得出还有谁；标记对应的名字绑得上，说明那个持有者已经不在了，标记顺手删掉。
//!
//! 用哪一种由锁所在目录的文件系统决定（[`mode`]），同一个文件系统上对每个进程都一样，所以本机的进程对
//! 同一把锁总是用同一种。抽象套接字只在本机内有效：不同机器上的进程经网络盘共用一个单元时，自己实现的锁
//! 协调不了它们（系统文件锁在这种盘上本来也靠不住）。抽象套接字只有 Linux 和 Android 有，其他系统上
//! 系统文件锁不起作用就报错。

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// 一个目录里的锁用哪一种实现。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// 系统文件锁。
    System,
    /// 自己实现的锁（见模块说明）。
    Own,
}

/// 目录 `dir`（没有就建）里的锁用哪一种：在那里试一下系统文件锁，起作用就用系统的，不然用自己实现的。
pub fn mode(dir: &Path) -> io::Result<Mode> {
    fs::create_dir_all(dir)?;
    let works = match probe(&dir.join("probe")) {
        Ok(works) => works,
        Err(e) if e.kind() == io::ErrorKind::Unsupported => false,
        Err(e) => return Err(e),
    };
    if works {
        Ok(Mode::System)
    } else if own::AVAILABLE {
        Ok(Mode::Own)
    } else {
        Err(io::Error::new(io::ErrorKind::Unsupported, format!("file locks have no effect in {}", dir.display())))
    }
}

/// 系统文件锁在这里是否起作用：同一个文件开两次，一边锁住时另一边必须拿不到。有的文件系统上锁是空操作，
/// 当它起作用的话，多个进程同时写会把单元写坏。
fn probe(path: &Path) -> io::Result<bool> {
    let a = open_file(path)?;
    let b = open_file(path)?;
    a.lock()?;
    let r = b.try_lock();
    let _ = a.unlock();
    match r {
        Err(fs::TryLockError::WouldBlock) => Ok(true),
        Err(fs::TryLockError::Error(e)) => Err(e),
        Ok(()) => Ok(false),
    }
}

fn open_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)
}

/// 一把互斥锁。放掉 `Lock` 也就放掉了锁。
pub struct Lock {
    imp: Imp,
}

enum Imp {
    System(File),
    Own { name: Vec<u8>, held: Mutex<Option<own::Held>> },
}

impl Lock {
    /// `path` 上的锁（锁文件不存在就建）。`mode` 是 `path` 所在目录的 [`mode`]。
    pub fn open(path: &Path, mode: Mode) -> io::Result<Lock> {
        let file = open_file(path)?;
        let imp = match mode {
            Mode::System => Imp::System(file),
            Mode::Own => Imp::Own { name: key(path)?, held: Mutex::new(None) },
        };
        Ok(Lock { imp })
    }

    /// `path` 上的锁，在锁文件出现之前就已经拿到：别人看到这个文件时它一定是锁着的，不会被当成已经不在的
    /// 持有者留下的。`path` 要是还没人用过的名字。
    pub fn create_held(path: &Path, mode: Mode) -> io::Result<Lock> {
        match mode {
            Mode::System => {
                let mut tmp = path.as_os_str().to_os_string();
                tmp.push(".new");
                let tmp = PathBuf::from(tmp);
                let file = open_file(&tmp)?;
                file.lock()?;
                fs::rename(&tmp, path)?;
                Ok(Lock { imp: Imp::System(file) })
            }
            Mode::Own => {
                let name = key(path)?;
                let held = own::try_bind(&name)?.ok_or_else(|| io::Error::from(io::ErrorKind::AddrInUse))?;
                open_file(path)?;
                Ok(Lock { imp: Imp::Own { name, held: Mutex::new(Some(held)) } })
            }
        }
    }

    /// 拿锁，拿不到就等。
    pub fn lock(&self) -> io::Result<()> {
        match &self.imp {
            Imp::System(file) => file.lock(),
            Imp::Own { .. } => {
                let mut wait = 1;
                while !self.try_lock()? {
                    std::thread::sleep(Duration::from_millis(wait));
                    wait = (wait * 2).min(20);
                }
                Ok(())
            }
        }
    }

    /// 拿锁，拿不到不等，返回 `false`。
    pub fn try_lock(&self) -> io::Result<bool> {
        match &self.imp {
            Imp::System(file) => match file.try_lock() {
                Ok(()) => Ok(true),
                Err(fs::TryLockError::WouldBlock) => Ok(false),
                Err(fs::TryLockError::Error(e)) => Err(e),
            },
            Imp::Own { name, held } => {
                let mut held = held.lock().unwrap_or_else(|e| e.into_inner());
                if held.is_some() {
                    return Ok(true);
                }
                *held = own::try_bind(name)?;
                Ok(held.is_some())
            }
        }
    }

    pub fn unlock(&self) {
        match &self.imp {
            Imp::System(file) => {
                let _ = file.unlock();
            }
            Imp::Own { held, .. } => {
                held.lock().unwrap_or_else(|e| e.into_inner()).take();
            }
        }
    }
}

/// `path` 上的锁此刻是不是被别的 [`Lock`] 拿着（不等，也不拿）。锁文件不在就当没人拿着。
pub fn held_elsewhere(path: &Path, mode: Mode) -> io::Result<bool> {
    match mode {
        Mode::System => {
            let file = match OpenOptions::new().read(true).write(true).open(path) {
                Ok(f) => f,
                // 没有这个文件，或者它正被删掉（Windows）。
                Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied) => {
                    return Ok(false);
                }
                Err(e) => return Err(e),
            };
            match file.try_lock() {
                Ok(()) => {
                    let _ = file.unlock();
                    Ok(false)
                }
                Err(fs::TryLockError::WouldBlock) => Ok(true),
                Err(fs::TryLockError::Error(e)) => Err(e),
            }
        }
        Mode::Own => {
            if !path.exists() {
                return Ok(false);
            }
            Ok(own::try_bind(&key(path)?)?.is_none())
        }
    }
}

/// 可以由多个进程共享持有的锁；持有的进程可以问此刻还有没有别的进程持有。
pub struct Shared {
    imp: SharedImp,
}

enum SharedImp {
    System(File),
    Own { dir: PathBuf, me: String, held: Mutex<Option<own::Held>> },
}

impl Shared {
    /// `path` 上的共享锁。`mode` 是 `path` 所在目录的 [`mode`]。
    pub fn open(path: &Path, mode: Mode) -> io::Result<Shared> {
        let imp = match mode {
            Mode::System => SharedImp::System(open_file(path)?),
            Mode::Own => {
                let mut dir = path.as_os_str().to_os_string();
                dir.push(".own");
                let dir = PathBuf::from(dir);
                fs::create_dir_all(&dir)?;
                let mut b = [0u8; 8];
                getrandom::fill(&mut b).map_err(|_| io::Error::other("system random number generator"))?;
                SharedImp::Own { dir, me: format!("{:016x}", u64::from_le_bytes(b)), held: Mutex::new(None) }
            }
        };
        Ok(Shared { imp })
    }

    /// 共享持有。和 [`Shared::release`] 一一对应，同一个 `Shared` 不重复持有。
    pub fn hold(&self) -> io::Result<()> {
        match &self.imp {
            SharedImp::System(file) => file.lock_shared(),
            SharedImp::Own { dir, me, held } => {
                let mark = dir.join(me);
                // 先绑名字再放标记：别人看到标记时名字一定已经绑着。
                let bound = own::try_bind(&key(&mark)?)?.ok_or_else(|| io::Error::from(io::ErrorKind::AddrInUse))?;
                open_file(&mark)?;
                *held.lock().unwrap_or_else(|e| e.into_inner()) = Some(bound);
                Ok(())
            }
        }
    }

    pub fn release(&self) {
        match &self.imp {
            SharedImp::System(file) => {
                let _ = file.unlock();
            }
            SharedImp::Own { dir, me, held } => {
                let _ = fs::remove_file(dir.join(me));
                held.lock().unwrap_or_else(|e| e.into_inner()).take();
            }
        }
    }

    /// 此刻是否没有别的进程持有（不等）。`holding`：本进程此刻是否持有它；本进程自己的不算。
    pub fn others_absent(&self, holding: bool) -> io::Result<bool> {
        match &self.imp {
            SharedImp::System(file) => {
                // 先放掉自己的共享锁，试着独占，再拿回来。
                if holding {
                    let _ = file.unlock();
                }
                let absent = match file.try_lock() {
                    Ok(()) => {
                        let _ = file.unlock();
                        Ok(true)
                    }
                    Err(fs::TryLockError::WouldBlock) => Ok(false),
                    Err(fs::TryLockError::Error(e)) => Err(e),
                };
                if holding {
                    file.lock_shared()?;
                }
                absent
            }
            SharedImp::Own { dir, me, .. } => {
                for entry in fs::read_dir(dir)? {
                    let mark = entry?.path();
                    if mark.file_name().is_some_and(|n| n == me.as_str()) {
                        continue;
                    }
                    match own::try_bind(&key(&mark)?)? {
                        // 绑得上：那个持有者已经不在了。
                        Some(_) => {
                            let _ = fs::remove_file(&mark);
                        }
                        None => return Ok(false),
                    }
                }
                Ok(true)
            }
        }
    }
}

/// 锁 `path` 对应的抽象套接字名字：由 `path` 的规范路径算出，本机的进程对同一个锁文件得到同一个名字。
fn key(path: &Path) -> io::Result<Vec<u8>> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = path.file_name().ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let full = fs::canonicalize(parent)?.join(name);
    Ok(format!("tagfs.{:032x}", fnv1a(full.as_os_str().as_encoded_bytes())).into_bytes())
}

/// 128 位 FNV-1a：只要求稳定、不容易撞。
fn fnv1a(bytes: &[u8]) -> u128 {
    bytes.iter().fold(0x6c62272e07bb014262b821756295c58d, |h, &b| {
        (h ^ b as u128).wrapping_mul(0x0000000001000000000000000000013b)
    })
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod own {
    use std::io;
    #[cfg(target_os = "android")]
    use std::os::android::net::SocketAddrExt;
    #[cfg(target_os = "linux")]
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixListener};

    pub const AVAILABLE: bool = true;

    /// 拿着它就拿着名字；关掉（放掉它，或者进程不在了）名字就空出来。
    pub type Held = UnixListener;

    /// 绑上抽象名字 `name`：绑上了返回 `Some`，别人绑着返回 `None`。
    pub fn try_bind(name: &[u8]) -> io::Result<Option<Held>> {
        let addr = SocketAddr::from_abstract_name(name)?;
        match UnixListener::bind_addr(&addr) {
            Ok(l) => Ok(Some(l)),
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
mod own {
    use std::io;

    pub const AVAILABLE: bool = false;

    pub enum Held {}

    pub fn try_bind(_name: &[u8]) -> io::Result<Option<Held>> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tagfs-lock-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn own_locks_exclude_each_other_and_go_with_their_holder() {
        let dir = temp_dir("own");
        let path = dir.join("writer");
        let a = Lock::open(&path, Mode::Own).unwrap();
        let b = Lock::open(&path, Mode::Own).unwrap();
        assert!(a.try_lock().unwrap());
        assert!(!b.try_lock().unwrap());
        assert!(held_elsewhere(&path, Mode::Own).unwrap());
        drop(a);
        assert!(!held_elsewhere(&path, Mode::Own).unwrap());
        assert!(b.try_lock().unwrap());
        b.unlock();
        let owner = Lock::create_held(&dir.join("owner"), Mode::Own).unwrap();
        assert!(held_elsewhere(&dir.join("owner"), Mode::Own).unwrap());
        drop(owner);
        assert!(!held_elsewhere(&dir.join("owner"), Mode::Own).unwrap());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn own_shared_locks_count_other_holders() {
        let dir = temp_dir("shared");
        let path = dir.join("readers");
        let a = Shared::open(&path, Mode::Own).unwrap();
        let b = Shared::open(&path, Mode::Own).unwrap();
        a.hold().unwrap();
        assert!(a.others_absent(true).unwrap());
        b.hold().unwrap();
        assert!(!a.others_absent(true).unwrap());
        b.release();
        assert!(a.others_absent(true).unwrap());
        // A holder that's gone without releasing: its mark stays, its name is free.
        b.hold().unwrap();
        if let SharedImp::Own { held, .. } = &b.imp {
            held.lock().unwrap().take();
        }
        assert!(a.others_absent(true).unwrap());
        assert_eq!(fs::read_dir(dir.join("readers.own")).unwrap().count(), 1, "the gone holder's mark is cleaned up");
        fs::remove_dir_all(dir).unwrap();
    }
}
