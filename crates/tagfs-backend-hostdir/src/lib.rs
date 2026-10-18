//! 目录单元：把宿主目录（Windows、Linux 这类层级文件系统）挂成一个存储单元，让上层能读写
//! 那里按路径组织的文件。
//!
//! - 挂载目录就是根 `/`。其中的每个普通文件是一个实体，带 `tagfs:hostfs_path`，值是它以挂载目录为根的
//!   路径；内容就是那个文件，读写都直接对文件进行。目录不是实体。符号链接不映射，也不会被跟随。
//! - `tagfs:hostfs_path` 是映射出来的：加上它就在宿主上建出文件（缺的上层目录一并建出），
//!   改它就移动文件，去掉它或删除实体就删掉宿主文件。
//! - 没有路径的实体存在挂载目录下的镜像文件里（`/.tagfs/unit.img`，块格式），和别的单元一样。
//!   镜像文件同时存放全部实体的 Tag 和索引，以及目录单元自己的记录。镜像文件按需扩容。
//! - 宿主上的改动（建、删、改名、改内容）通过文件系统通知实时同步过来；挂载时还会全量对账一次。
//!
//! 一次修改分两步提交，中间做宿主上的文件操作：
//!
//! 1. 镜像里提交实体的修改，同时记下要做的宿主操作（意图记录）。这是提交点。
//! 2. 按意图记录操作宿主：对路径实体的写入直接写进宿主文件；要复制的文件在宿主上复制；
//!    再把要让出的文件挪进 `/.tagfs/tmp`，最后把各文件放到新路径。
//! 3. 镜像里再提交一次：删掉意图记录，记下文件身份。
//!
//! 之后断电，挂载时按意图记录把第 2、3 步做完：宿主操作都可以重做。
//!
//! 只有内容要换个地方存放时才会搬运整个文件：路径实体失去路径（内容进镜像）、被复制成没有路径的实体，
//! 或者镜像里的实体加上路径（内容写到宿主）。这些都是分段流式进行的，不会整个读进内存。

mod path;

use std::cell::{Cell, Ref, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use tagfs_backend_file::{ImageError, ImageUnit};
use tagfs_block::{Aux, Batch, Fill, OpenError, Source};
use tagfs_core::{
    Change, EntityId, EntityRecord, IdSource, Key, Namespace, Probe, Snapshot, Tag, TxnId, TxnInfo, Unit, UnitError,
    UnitId, Value,
};

pub use path::{HostPath, INVALID_PATH, PATH_TAKEN, RESERVED};

/// 目录单元映射的 `tagfs` 键：宿主层级文件系统（Windows、Linux 这一类按目录树组织的文件系统）里的路径。
///
/// 值是以挂载目录为根的路径，如 `/abc/bcd/1.file`，实体的内容就是那个文件。这个 Tag 不是存下来的，
/// 而是映射出来的：增、删、改它，就是在宿主上建立、删除、移动对应的文件。
pub const HOSTFS_PATH: &str = "hostfs_path";

const IMAGE: &str = "unit.img";
const TMP: &str = "tmp";
/// 第 2 步里“直接写入和复制”已经做完的标记。
const EDITED: &str = "edited";
/// 第 2 步里“让出旧文件”已经做完的标记。
const PARKED: &str = "parked";
/// 流式搬运内容时一次读写多少。
const CHUNK: usize = 1 << 20;

const AUX_INTENT: &[u8] = b"intent";
const AUX_IDENTITY: &[u8] = b"id/";

#[derive(Debug, PartialEq, Eq)]
pub enum DirError {
    /// 挂载点不是目录。
    NotADirectory,
    /// 已经有目录单元了（`create` 时）。
    AlreadyExists,
    /// 还没有目录单元（`open` 时）。
    NotFound,
    Io(io::ErrorKind),
    Open(OpenError),
    Unit(UnitError),
}

impl From<io::Error> for DirError {
    fn from(e: io::Error) -> Self {
        DirError::Io(e.kind())
    }
}

impl From<ImageError> for DirError {
    fn from(e: ImageError) -> Self {
        match e {
            ImageError::Io(e) => DirError::Io(e.kind()),
            ImageError::Format(e) => DirError::Open(e),
        }
    }
}

impl From<UnitError> for DirError {
    fn from(e: UnitError) -> Self {
        DirError::Unit(e)
    }
}

impl std::fmt::Display for DirError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DirError::NotADirectory => f.write_str("mount point is not a directory"),
            DirError::AlreadyExists => f.write_str("directory unit already exists"),
            DirError::NotFound => f.write_str("no directory unit here"),
            DirError::Io(k) => write!(f, "i/o error: {k}"),
            DirError::Open(e) => write!(f, "{e}"),
            DirError::Unit(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DirError {}

/// 一次对账的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub added: usize,
    pub removed: usize,
    pub renamed: usize,
    /// 没有映射的文件：名字不是 UTF-8、在宿主上不合法、路径太长、符号链接或特殊文件。
    pub skipped: usize,
}

fn io_err(_: io::Error) -> UnitError {
    UnitError::Device
}

fn hostfs_key() -> (Namespace, Key) {
    (Namespace::tagfs(), Key::new(HOSTFS_PATH))
}

/// 实体 Tag 里的映射路径。带了不止一个时报错。
fn path_of(tags: &[Tag]) -> Result<Option<HostPath>, UnitError> {
    let (ns, key) = hostfs_key();
    let mut found = None;
    for t in tags.iter().filter(|t| t.is(&ns, &key)) {
        let Value::Str(v) = &t.value else { return Err(INVALID_PATH) };
        if found.replace(HostPath::parse(v)?).is_some() {
            return Err(INVALID_PATH);
        }
    }
    Ok(found)
}

fn path_tag(p: &HostPath) -> Tag {
    Tag::new(Namespace::TAGFS, HOSTFS_PATH, p.value())
}

fn hex(id: &EntityId) -> String {
    id.0.iter().map(|b| format!("{b:02x}")).collect()
}

fn sync_dir(dir: &Path) -> Result<(), UnitError> {
    #[cfg(unix)]
    {
        fs::File::open(dir).and_then(|f| f.sync_all()).map_err(io_err)
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

fn read_at(f: &fs::File, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        #[cfg(unix)]
        let n = std::os::unix::fs::FileExt::read_at(f, &mut buf[done..], offset + done as u64)?;
        #[cfg(windows)]
        let n = std::os::windows::fs::FileExt::seek_read(f, &mut buf[done..], offset + done as u64)?;
        if n == 0 {
            break;
        }
        done += n;
    }
    Ok(done)
}

fn write_at(f: &fs::File, offset: u64, data: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::write_all_at(f, data, offset)
    }
    #[cfg(windows)]
    {
        let mut done = 0;
        while done < data.len() {
            match std::os::windows::fs::FileExt::seek_write(f, &data[done..], offset + done as u64)? {
                0 => return Err(io::ErrorKind::WriteZero.into()),
                n => done += n,
            }
        }
        Ok(())
    }
}

// ---- 文件身份 ----

/// 宿主文件的身份，用来认出改名。仿照 git 的索引：记下文件系统给的文件号和 stat 信息（长度、修改时间）。
///
/// - 有文件号时按文件号认：POSIX 是设备号加 inode 号，宿主报告创建时间时把它也算进去（inode 会被复用）；
///   Windows 是卷序列号加文件号（NTFS 的文件号带序号，不会被新文件复用）。
/// - 拿不到文件号时（某些网络文件系统），按长度加修改时间认，而且只在这样的匹配唯一时才算改名。
#[derive(Clone, Debug, PartialEq, Eq)]
struct Ident {
    file_id: Option<Vec<u8>>,
    size: u64,
    mtime_ns: u128,
}

impl Ident {
    fn of(path: &Path, m: &fs::Metadata) -> Ident {
        let mtime_ns =
            m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
        Ident { file_id: file_id(path, m), size: m.len(), mtime_ns }
    }

    fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        match &self.file_id {
            Some(id) => {
                b.push(1);
                b.extend_from_slice(&(id.len() as u32).to_le_bytes());
                b.extend_from_slice(id);
            }
            None => b.push(0),
        }
        b.extend_from_slice(&self.size.to_le_bytes());
        b.extend_from_slice(&self.mtime_ns.to_le_bytes());
        b
    }

    fn decode(b: &[u8]) -> Option<Ident> {
        let (file_id, rest) = match *b.first()? {
            1 => {
                let n = u32::from_le_bytes(b.get(1..5)?.try_into().ok()?) as usize;
                (Some(b.get(5..5 + n)?.to_vec()), b.get(5 + n..)?)
            }
            _ => (None, b.get(1..)?),
        };
        Some(Ident {
            file_id,
            size: u64::from_le_bytes(rest.get(..8)?.try_into().ok()?),
            mtime_ns: u128::from_le_bytes(rest.get(8..24)?.try_into().ok()?),
        })
    }

    /// 是不是同一个文件。两边都有文件号就只看文件号；否则看长度和修改时间。
    fn same_file(&self, other: &Ident) -> bool {
        match (&self.file_id, &other.file_id) {
            (Some(a), Some(b)) => a == b,
            _ => self.size == other.size && self.mtime_ns == other.mtime_ns,
        }
    }

    /// 匹配是否需要唯一才能算数（按 stat 信息匹配时）。
    fn weak(&self, other: &Ident) -> bool {
        self.file_id.is_none() || other.file_id.is_none()
    }
}

fn file_id(path: &Path, m: &fs::Metadata) -> Option<Vec<u8>> {
    let mut v = match file_id::get_file_id(path).ok()? {
        file_id::FileId::Inode { device_id, inode_number } => {
            let mut v = device_id.to_le_bytes().to_vec();
            v.extend_from_slice(&inode_number.to_le_bytes());
            v
        }
        file_id::FileId::LowRes { volume_serial_number, file_index } => {
            let mut v = volume_serial_number.to_le_bytes().to_vec();
            v.extend_from_slice(&file_index.to_le_bytes());
            v
        }
        file_id::FileId::HighRes { volume_serial_number, file_id } => {
            let mut v = volume_serial_number.to_le_bytes().to_vec();
            v.extend_from_slice(&file_id.to_le_bytes());
            v
        }
    };
    if cfg!(unix)
        && let Ok(born) = m.created()
        && let Ok(d) = born.duration_since(std::time::UNIX_EPOCH)
    {
        v.extend_from_slice(&d.as_nanos().to_le_bytes());
    }
    Some(v)
}

fn identity_key(id: &EntityId) -> Vec<u8> {
    let mut k = AUX_IDENTITY.to_vec();
    k.extend_from_slice(&id.0);
    k
}

// ---- 意图记录 ----

/// 对路径实体内容的直接修改。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Edit {
    Write { offset: u64, data: Vec<u8> },
    Truncate { len: u64 },
}

/// 实体的文件最后放到新路径时，内容从哪来。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Place {
    /// 不用放（没有新路径，或者文件留在原处）。
    Nothing,
    /// 从镜像写出。
    Image,
    /// 宿主上复制好、暂存在临时目录的文件。
    Staged,
    /// 原来的文件（挪进临时目录后）改名过去。
    Parked,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Item {
    id: EntityId,
    /// 修改前的路径。
    old: Option<HostPath>,
    /// 修改后的路径。
    new: Option<HostPath>,
    place: Place,
    /// 旧文件要让出：挪进临时目录，最后删掉或改名到新路径。
    vacate: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Intent {
    /// 直接写进宿主文件的修改，按顺序：实体、它的文件、修改。
    edits: Vec<(EntityId, HostPath, Edit)>,
    /// 宿主上的复制：新实体、源文件。在全部直接修改之后做。
    copies: Vec<(EntityId, HostPath)>,
    items: Vec<Item>,
}

struct Enc(Vec<u8>);

impl Enc {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u64(b.len() as u64);
        self.0.extend_from_slice(b);
    }
    fn id(&mut self, id: &EntityId) {
        self.0.extend_from_slice(&id.0);
    }
    fn path(&mut self, p: &Option<HostPath>) {
        match p {
            Some(p) => {
                self.u8(1);
                self.bytes(p.value().as_bytes());
            }
            None => self.u8(0),
        }
    }
}

struct Dec<'a>(&'a [u8]);

impl<'a> Dec<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], UnitError> {
        if self.0.len() < n {
            return Err(UnitError::Corrupt);
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8, UnitError> {
        Ok(self.take(1)?[0])
    }
    fn u64(&mut self) -> Result<u64, UnitError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn bytes(&mut self) -> Result<&'a [u8], UnitError> {
        let n = usize::try_from(self.u64()?).map_err(|_| UnitError::Corrupt)?;
        self.take(n)
    }
    fn id(&mut self) -> Result<EntityId, UnitError> {
        Ok(EntityId(self.take(16)?.try_into().unwrap()))
    }
    fn path(&mut self) -> Result<Option<HostPath>, UnitError> {
        match self.u8()? {
            0 => Ok(None),
            _ => {
                let s = std::str::from_utf8(self.bytes()?).map_err(|_| UnitError::Corrupt)?;
                HostPath::parse(s).map(Some).map_err(|_| UnitError::Corrupt)
            }
        }
    }
}

impl Intent {
    fn is_empty(&self) -> bool {
        self.edits.is_empty() && self.copies.is_empty() && self.items.is_empty()
    }

    fn encode(&self) -> Vec<u8> {
        let mut e = Enc(Vec::new());
        e.u64(self.edits.len() as u64);
        for (id, p, edit) in &self.edits {
            e.id(id);
            e.path(&Some(p.clone()));
            match edit {
                Edit::Write { offset, data } => {
                    e.u8(0);
                    e.u64(*offset);
                    e.bytes(data);
                }
                Edit::Truncate { len } => {
                    e.u8(1);
                    e.u64(*len);
                }
            }
        }
        e.u64(self.copies.len() as u64);
        for (id, src) in &self.copies {
            e.id(id);
            e.path(&Some(src.clone()));
        }
        e.u64(self.items.len() as u64);
        for it in &self.items {
            e.id(&it.id);
            e.path(&it.old);
            e.path(&it.new);
            e.u8(match it.place {
                Place::Nothing => 0,
                Place::Image => 1,
                Place::Staged => 2,
                Place::Parked => 3,
            });
            e.u8(it.vacate as u8);
        }
        e.0
    }

    fn decode(b: &[u8]) -> Result<Intent, UnitError> {
        let mut d = Dec(b);
        let mut intent = Intent::default();
        for _ in 0..d.u64()? {
            let id = d.id()?;
            let p = d.path()?.ok_or(UnitError::Corrupt)?;
            let edit = match d.u8()? {
                0 => Edit::Write { offset: d.u64()?, data: d.bytes()?.to_vec() },
                1 => Edit::Truncate { len: d.u64()? },
                _ => return Err(UnitError::Corrupt),
            };
            intent.edits.push((id, p, edit));
        }
        for _ in 0..d.u64()? {
            let id = d.id()?;
            intent.copies.push((id, d.path()?.ok_or(UnitError::Corrupt)?));
        }
        for _ in 0..d.u64()? {
            let id = d.id()?;
            let old = d.path()?;
            let new = d.path()?;
            let place = match d.u8()? {
                0 => Place::Nothing,
                1 => Place::Image,
                2 => Place::Staged,
                3 => Place::Parked,
                _ => return Err(UnitError::Corrupt),
            };
            intent.items.push(Item { id, old, new, place, vacate: d.u8()? != 0 });
        }
        Ok(intent)
    }
}

/// 一组修改在目录单元上的执行计划。
struct Plan {
    /// 内容要搬进镜像的路径实体和它的文件。
    hydrate: Vec<(EntityId, HostPath)>,
    /// 交给镜像的修改（对直接写宿主的实体，内容修改不进镜像）。
    changes: Vec<Change>,
    intent: Intent,
}

/// 把宿主文件当作内容来源，分段读取。
struct HostSource(Option<fs::File>);

impl Source for HostSource {
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize, UnitError> {
        match &self.0 {
            Some(f) => read_at(f, offset, buf).map_err(io_err),
            None => Ok(0),
        }
    }
}

// ---- 单元 ----

/// 需要对账的范围：挂载目录下的若干路径（按段），空表示整个目录。
type Scope = Vec<String>;

struct Inner {
    root: PathBuf,
    image: ImageUnit,
    ids: Box<dyn IdSource>,
}

struct Watch {
    // 只为持有：放掉它就停止监听。
    _watcher: notify::RecommendedWatcher,
    events: mpsc::Receiver<notify::Result<notify::Event>>,
}

pub struct DirUnit {
    root: PathBuf,
    inner: RefCell<Inner>,
    watch: Option<Watch>,
    /// 取快照前同步失败的错误，下一个能报错的调用把它报出来。
    sync_error: Cell<Option<UnitError>>,
}

impl DirUnit {
    /// 在已有的目录上建目录单元，并把里面的文件映射进来。
    pub fn create(
        root: impl AsRef<Path>,
        unit: UnitId,
        ids: Box<dyn IdSource>,
    ) -> Result<(Self, ScanReport), DirError> {
        let root = fs::canonicalize(root)?;
        if !fs::metadata(&root)?.is_dir() {
            return Err(DirError::NotADirectory);
        }
        let own = root.join(RESERVED);
        match fs::create_dir(&own) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err(DirError::AlreadyExists),
            Err(e) => return Err(e.into()),
        }
        let image = tagfs_backend_file::create(own.join(IMAGE), unit)?;
        Self::start(root, image, ids)
    }

    /// 挂载已有的目录单元：完成断电前没做完的宿主操作，再和目录对账。
    pub fn open(root: impl AsRef<Path>, ids: Box<dyn IdSource>) -> Result<(Self, ScanReport), DirError> {
        let root = fs::canonicalize(root)?;
        let img = root.join(RESERVED).join(IMAGE);
        if !img.is_file() {
            return Err(DirError::NotFound);
        }
        let image = tagfs_backend_file::open(img)?;
        Self::start(root, image, ids)
    }

    fn start(root: PathBuf, image: ImageUnit, ids: Box<dyn IdSource>) -> Result<(Self, ScanReport), DirError> {
        // 先开始监听，再对账：对账期间发生的改动不会漏掉。
        let watch = start_watch(&root);
        let mut inner = Inner { root: root.clone(), image, ids };
        if let Some(raw) = inner.image.aux_get(AUX_INTENT)? {
            let intent = Intent::decode(&raw)?;
            inner.carry_out(&intent)?;
            inner.finish(&intent)?;
        }
        inner.clean_tmp()?;
        let report = inner.reconcile(None)?;
        let me = DirUnit { root, inner: RefCell::new(inner), watch, sync_error: Cell::new(None) };
        Ok((me, report))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 是否在实时监听宿主上的改动。监听建不起来时（例如系统的监听数量上限）只在挂载时和 [`DirUnit::rescan`] 时对账。
    pub fn is_live(&self) -> bool {
        self.watch.is_some()
    }

    /// 镜像（块格式单元），检查用。
    pub fn image(&self) -> Ref<'_, ImageUnit> {
        Ref::map(self.inner.borrow(), |i| &i.image)
    }

    /// 全量对账。
    pub fn rescan(&mut self) -> Result<ScanReport, UnitError> {
        self.drain_events();
        self.inner.get_mut().reconcile(None)
    }

    /// 把收到的宿主改动通知同步进来。
    pub fn sync(&self) -> Result<ScanReport, UnitError> {
        if let Some(e) = self.sync_error.take() {
            return Err(e);
        }
        match self.drain_events() {
            None => Ok(ScanReport::default()),
            Some(scopes) => self.inner.borrow_mut().reconcile(scopes.map(|s| s.into_iter().collect())),
        }
    }

    /// 取出全部待处理的通知，合并成对账范围。没有通知时为 `None`；`Some(None)` 表示要全量对账。
    fn drain_events(&self) -> Option<Option<BTreeSet<Scope>>> {
        let watch = self.watch.as_ref()?;
        let mut scopes = BTreeSet::new();
        let mut full = false;
        let mut any = false;
        loop {
            match watch.events.try_recv() {
                Ok(Ok(ev)) => {
                    any = true;
                    full |= ev.need_rescan();
                    for p in &ev.paths {
                        match scope_of(&self.root, p) {
                            Some(s) if s.first().is_some_and(|f| f == RESERVED) => {}
                            Some(s) if s.is_empty() => full = true,
                            Some(s) => {
                                scopes.insert(s);
                            }
                            None => full = true,
                        }
                    }
                }
                Ok(Err(_)) => {
                    any = true;
                    full = true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    any = true;
                    full = true;
                    break;
                }
            }
        }
        if !any {
            return None;
        }
        if full || scopes.is_empty() {
            return Some(None);
        }
        // 去掉被别的范围包含的。
        let mut kept: Vec<Scope> = Vec::new();
        for s in scopes {
            if !kept.iter().any(|k| s.starts_with(k)) {
                kept.push(s);
            }
        }
        Some(Some(kept.into_iter().collect()))
    }

    fn sync_mut(&mut self) -> Result<(), UnitError> {
        self.sync().map(|_| ())
    }
}

fn start_watch(root: &Path) -> Option<Watch> {
    use notify::Watcher;
    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(tx).ok()?;
    watcher.watch(root, notify::RecursiveMode::Recursive).ok()?;
    Some(Watch { _watcher: watcher, events: rx })
}

/// 宿主上的绝对路径在挂载目录下的各段。不在挂载目录下或名字不是 UTF-8 时为 `None`。
fn scope_of(root: &Path, p: &Path) -> Option<Scope> {
    let rel = p.strip_prefix(root).ok()?;
    rel.components()
        .map(|c| match c {
            std::path::Component::Normal(s) => s.to_str().map(String::from),
            _ => None,
        })
        .collect()
}

impl Inner {
    fn tmp(&self) -> PathBuf {
        self.root.join(RESERVED).join(TMP)
    }

    fn clean_tmp(&self) -> Result<(), UnitError> {
        match fs::remove_dir_all(self.tmp()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err(e)),
        }
    }

    /// 范围内的宿主文件和镜像对账。`scopes` 为 `None` 时是整个目录。
    fn reconcile(&mut self, scopes: Option<BTreeSet<Scope>>) -> Result<ScanReport, UnitError> {
        let mut report = ScanReport::default();
        let mut found: BTreeMap<HostPath, Ident> = BTreeMap::new();
        match &scopes {
            None => walk(&self.root, &mut Vec::new(), &mut found, &mut report)?,
            Some(scopes) => {
                for s in scopes {
                    self.collect_scope(s, &mut found, &mut report)?;
                }
            }
        }
        let in_scope = |p: &HostPath| match &scopes {
            None => true,
            Some(scopes) => {
                let segs = p.segments();
                scopes.iter().any(|s| segs.starts_with(s))
            }
        };

        let snap = self.image.snapshot();
        let (ns, key) = hostfs_key();
        let mut known: Vec<(EntityId, HostPath)> = Vec::new();
        for id in snap.probe(&Probe::Has { ns: &ns, key: &key })?.unwrap_or_default() {
            let rec = snap.get(&id)?.ok_or(UnitError::Corrupt)?;
            if let Some(p) = path_of(&rec.tags)?
                && in_scope(&p)
            {
                known.push((id, p));
            }
        }
        drop(snap);
        let identities: BTreeMap<Vec<u8>, Ident> =
            self.image.aux_list(AUX_IDENTITY)?.into_iter().filter_map(|(k, v)| Some((k, Ident::decode(&v)?))).collect();

        let mut changes = Vec::new();
        let mut aux = Vec::new();
        let mut lost = Vec::new();
        for (id, p) in known {
            match found.remove(&p) {
                Some(ident) => {
                    if identities.get(&identity_key(&id)) != Some(&ident) {
                        aux.push(Aux::Put(identity_key(&id), ident.encode()));
                    }
                }
                None => lost.push((id, p)),
            }
        }
        // 认改名：消失的实体和新出现的文件，是同一个文件就算改名。按 stat 信息认的要求两边都唯一。
        let mut pairs: Vec<(usize, HostPath, bool)> = Vec::new();
        for (i, (id, _)) in lost.iter().enumerate() {
            let Some(old) = identities.get(&identity_key(id)) else { continue };
            let hits: Vec<_> = found.iter().filter(|(_, new)| old.same_file(new)).collect();
            // 不止一个候选就认不出来，当作删除加新建。
            if let [(p, new)] = hits[..] {
                pairs.push((i, p.clone(), old.weak(new)));
            }
        }
        let mut claimed: BTreeMap<HostPath, usize> = BTreeMap::new();
        for (_, p, _) in &pairs {
            *claimed.entry(p.clone()).or_default() += 1;
        }
        let mut renamed = BTreeMap::new();
        for (i, p, weak) in pairs {
            if !weak || claimed[&p] == 1 {
                renamed.insert(i, p);
            }
        }
        for (i, (id, old)) in lost.into_iter().enumerate() {
            match renamed.remove(&i) {
                Some(new) => {
                    let ident = found.remove(&new).expect("found");
                    changes.push(Change::RemoveTag { id, tag: path_tag(&old) });
                    changes.push(Change::AddTag { id, tag: path_tag(&new) });
                    aux.push(Aux::Put(identity_key(&id), ident.encode()));
                    report.renamed += 1;
                }
                None => {
                    changes.push(Change::Delete { id });
                    aux.push(Aux::Delete(identity_key(&id)));
                    report.removed += 1;
                }
            }
        }
        for (p, ident) in found {
            let id = EntityId::generate(&mut *self.ids);
            changes.push(Change::Create { id, tags: vec![path_tag(&p)], content: Vec::new() });
            aux.push(Aux::Put(identity_key(&id), ident.encode()));
            report.added += 1;
        }
        if !changes.is_empty() || !aux.is_empty() {
            self.image.commit_batch(&mut Batch { changes: &changes, aux: &aux, ..Batch::default() })?;
        }
        Ok(report)
    }

    /// 收集一个范围里的文件：范围是目录就整个走一遍，是文件就只看它。
    fn collect_scope(
        &self,
        scope: &Scope,
        found: &mut BTreeMap<HostPath, Ident>,
        report: &mut ScanReport,
    ) -> Result<(), UnitError> {
        // 范围的上层任何一级是链接或不是目录，就没有可映射的东西。
        let mut host = self.root.clone();
        for (i, seg) in scope.iter().enumerate() {
            host.push(seg);
            let Ok(m) = fs::symlink_metadata(&host) else { return Ok(()) };
            if m.file_type().is_symlink() {
                return Ok(());
            }
            let last = i + 1 == scope.len();
            if last && m.is_file() {
                let names: Vec<&std::ffi::OsStr> = scope.iter().map(|s| std::ffi::OsStr::new(s.as_str())).collect();
                match HostPath::from_names(&names) {
                    Some(p) => {
                        found.insert(p, Ident::of(&host, &m));
                    }
                    None => report.skipped += 1,
                }
                return Ok(());
            }
            if !m.is_dir() {
                return Ok(());
            }
        }
        let mut names: Vec<std::ffi::OsString> = scope.iter().map(std::ffi::OsString::from).collect();
        walk(&host, &mut names, found, report)
    }

    /// 算出一组修改要做的宿主操作，并检查路径。
    fn plan(&self, changes: &[Change]) -> Result<Plan, UnitError> {
        let snap = self.image.snapshot();
        // 涉及的实体：修改前的路径（只对修改前就存在的实体），以及按顺序模拟下来的 Tag（`None` 是已删除）。
        let mut before: BTreeMap<EntityId, Option<HostPath>> = BTreeMap::new();
        let mut after: BTreeMap<EntityId, Option<Vec<Tag>>> = BTreeMap::new();
        let mut content_ops: BTreeMap<EntityId, Vec<usize>> = BTreeMap::new();
        let mut copies: Vec<(usize, EntityId, EntityId)> = Vec::new();
        let mut load = |id: &EntityId, after: &mut BTreeMap<EntityId, Option<Vec<Tag>>>| -> Result<(), UnitError> {
            if !after.contains_key(id) {
                let tags = snap.get(id)?.map(|r| r.tags);
                if let Some(t) = &tags {
                    before.insert(*id, path_of(t)?);
                }
                after.insert(*id, tags);
            }
            Ok(())
        };
        for (i, c) in changes.iter().enumerate() {
            match c {
                Change::Create { id, tags, .. } => {
                    load(id, &mut after)?;
                    after.insert(*id, Some(tags.clone()));
                }
                Change::Copy { from, id, tags } => {
                    load(from, &mut after)?;
                    load(id, &mut after)?;
                    copies.push((i, *from, *id));
                    after.insert(*id, Some(tags.clone()));
                }
                Change::Delete { id } => {
                    load(id, &mut after)?;
                    after.insert(*id, None);
                }
                Change::AddTag { id, tag } => {
                    load(id, &mut after)?;
                    if let Some(Some(tags)) = after.get_mut(id)
                        && !tags.contains(tag)
                    {
                        tags.push(tag.clone());
                    }
                }
                Change::RemoveTag { id, tag } => {
                    load(id, &mut after)?;
                    if let Some(Some(tags)) = after.get_mut(id) {
                        tags.retain(|t| t != tag);
                    }
                }
                Change::Write { id, .. } | Change::Truncate { id, .. } => {
                    load(id, &mut after)?;
                    if after[id].is_none() {
                        return Err(UnitError::NotFound(*id));
                    }
                    content_ops.entry(*id).or_default().push(i);
                }
            }
        }
        drop(snap);

        // 最后的状态：不存在为 None，存在时为它的路径。
        let mut finals: BTreeMap<EntityId, Option<Option<HostPath>>> = BTreeMap::new();
        for (id, tags) in &after {
            finals.insert(*id, tags.as_ref().map(|t| path_of(t)).transpose()?);
        }
        let on_host = |e: &EntityId| matches!(before.get(e), Some(Some(_)));
        let later_op = |e: &EntityId, i: usize| content_ops.get(e).is_some_and(|v| v.iter().any(|&j| j > i));
        let later_copy_from = |e: &EntityId, i: usize| copies.iter().any(|(j, f, _)| f == e && *j > i);

        // 内容要搬进镜像的路径实体。
        let mut hydrate_set = BTreeSet::new();
        for (i, f, t) in &copies {
            if !on_host(f) {
                continue;
            }
            let target_ok = match &finals[t] {
                None => true,
                Some(None) => false,
                Some(Some(_)) => !later_op(t, *i) && !later_copy_from(t, *i),
            };
            if !target_ok || later_op(f, *i) {
                hydrate_set.insert(*f);
            }
        }
        for (id, fin) in &finals {
            if on_host(id) && matches!(fin, Some(None)) {
                hydrate_set.insert(*id);
            }
        }
        // 内容一直留在宿主、直接改文件的实体。
        let direct: BTreeSet<EntityId> =
            before.iter().filter(|(id, p)| p.is_some() && !hydrate_set.contains(id)).map(|(id, _)| *id).collect();
        let mut staged: BTreeMap<EntityId, HostPath> = BTreeMap::new();
        for (_, f, t) in &copies {
            if direct.contains(f) && matches!(finals[t], Some(Some(_))) {
                staged.insert(*t, before[f].clone().expect("on host"));
            }
        }

        let mut intent = Intent::default();
        let mut image_changes = Vec::new();
        for c in changes {
            match c {
                Change::Write { id, offset, data } if direct.contains(id) => {
                    let p = before[id].clone().expect("on host");
                    intent.edits.push((*id, p, Edit::Write { offset: *offset, data: data.clone() }));
                }
                Change::Truncate { id, len } if direct.contains(id) => {
                    let p = before[id].clone().expect("on host");
                    intent.edits.push((*id, p, Edit::Truncate { len: *len }));
                }
                _ => image_changes.push(c.clone()),
            }
        }
        intent.copies = staged.iter().map(|(t, src)| (*t, src.clone())).collect();

        let mut targets: BTreeSet<HostPath> = BTreeSet::new();
        for (id, fin) in &finals {
            let old = before.get(id).cloned().flatten();
            let new = fin.clone().flatten();
            if let Some(p) = &new
                && !targets.insert(p.clone())
            {
                return Err(PATH_TAKEN);
            }
            let place = match &new {
                None => Place::Nothing,
                Some(_) if staged.contains_key(id) => Place::Staged,
                Some(_) if direct.contains(id) => {
                    if old == new {
                        Place::Nothing
                    } else {
                        Place::Parked
                    }
                }
                Some(_) => Place::Image,
            };
            let vacate = old.is_some() && (old != new || place == Place::Image);
            if place != Place::Nothing || vacate {
                intent.items.push(Item { id: *id, old, new, place, vacate });
            }
        }
        // 新路径在宿主上必须空着（或者正好被这次让出）。
        let vacated: BTreeSet<&HostPath> =
            intent.items.iter().filter(|i| i.vacate).filter_map(|i| i.old.as_ref()).collect();
        for it in &intent.items {
            if let Some(p) = &it.new
                && it.place != Place::Nothing
                && p.inspect(&self.root)?.is_some()
                && !vacated.contains(p)
            {
                return Err(PATH_TAKEN);
            }
        }
        let hydrate = hydrate_set.into_iter().map(|id| (id, before[&id].clone().expect("on host"))).collect();
        Ok(Plan { hydrate, changes: image_changes, intent })
    }

    /// 第 1 步到第 3 步。`finish` 是同时了结的准备记录。
    fn run(&mut self, changes: &[Change], finish: Option<(TxnId, bool)>) -> Result<(), UnitError> {
        let plan = self.plan(changes)?;
        let mut sources: Vec<(EntityId, HostSource)> = Vec::new();
        for (id, p) in &plan.hydrate {
            let file = match p.inspect(&self.root)? {
                Some(m) if m.is_file() => Some(fs::File::open(p.on_host(&self.root)).map_err(io_err)?),
                _ => None,
            };
            sources.push((*id, HostSource(file)));
        }
        let fills = sources.iter_mut().map(|(id, s)| Fill { id: *id, source: s }).collect();
        let aux =
            if plan.intent.is_empty() { Vec::new() } else { vec![Aux::Put(AUX_INTENT.to_vec(), plan.intent.encode())] };
        self.image.commit_batch(&mut Batch { fills, changes: &plan.changes, aux: &aux, finish })?;
        if plan.intent.is_empty() {
            return Ok(());
        }
        self.carry_out(&plan.intent)?;
        self.finish(&plan.intent)?;
        self.clean_tmp()
    }

    /// 第 2 步：按意图记录操作宿主。可以重做：每一段做完留一个标记，重做时跳过已经完成的段。
    fn carry_out(&self, intent: &Intent) -> Result<(), UnitError> {
        let tmp = self.tmp();
        fs::create_dir_all(&tmp).map_err(io_err)?;
        let staged = |id: &EntityId, ext: &str| tmp.join(format!("{}.{ext}", hex(id)));

        // 直接写入和复制。同一串写入从头重做，结果和只做一遍一样。
        if !tmp.join(EDITED).exists() {
            let mut touched: BTreeMap<&HostPath, fs::File> = BTreeMap::new();
            for (_, p, edit) in &intent.edits {
                if !touched.contains_key(p) {
                    // 文件在宿主上被删掉了就跳过，对账时实体会被删掉。
                    if !matches!(p.inspect(&self.root)?, Some(m) if m.is_file()) {
                        continue;
                    }
                    let f = fs::OpenOptions::new().write(true).open(p.on_host(&self.root)).map_err(io_err)?;
                    touched.insert(p, f);
                }
                let f = &touched[p];
                match edit {
                    Edit::Write { offset, data } => write_at(f, *offset, data).map_err(io_err)?,
                    Edit::Truncate { len } => f.set_len(*len).map_err(io_err)?,
                }
            }
            for f in touched.values() {
                f.sync_all().map_err(io_err)?;
            }
            for (id, src) in &intent.copies {
                let dest = staged(id, "new");
                match src.inspect(&self.root)? {
                    Some(m) if m.is_file() => {
                        fs::copy(src.on_host(&self.root), &dest).map_err(io_err)?;
                    }
                    _ => {
                        fs::File::create(&dest).map_err(io_err)?;
                    }
                }
                fs::File::open(&dest).and_then(|f| f.sync_all()).map_err(io_err)?;
            }
            fs::write(tmp.join(EDITED), b"").map_err(io_err)?;
            sync_dir(&tmp)?;
        }

        // 让出旧文件。
        if !tmp.join(PARKED).exists() {
            for it in intent.items.iter().filter(|i| i.vacate) {
                let old = it.old.as_ref().expect("vacate has old");
                let parked = staged(&it.id, "old");
                if matches!(old.inspect(&self.root), Ok(Some(m)) if m.is_file()) && !parked.exists() {
                    fs::rename(old.on_host(&self.root), &parked).map_err(io_err)?;
                }
            }
            fs::write(tmp.join(PARKED), b"").map_err(io_err)?;
            sync_dir(&tmp)?;
        }

        // 放到新路径。
        let snap = self.image.snapshot();
        let mut dirs = BTreeSet::new();
        for it in &intent.items {
            let Some(new) = &it.new else { continue };
            let from = match it.place {
                Place::Nothing => continue,
                Place::Image => {
                    let out = staged(&it.id, "img");
                    let mut f = fs::File::create(&out).map_err(io_err)?;
                    let mut buf = vec![0u8; CHUNK];
                    let mut offset = 0u64;
                    loop {
                        let n = snap.read(&it.id, offset, &mut buf)?;
                        if n == 0 {
                            break;
                        }
                        io::Write::write_all(&mut f, &buf[..n]).map_err(io_err)?;
                        offset += n as u64;
                    }
                    f.sync_all().map_err(io_err)?;
                    out
                }
                Place::Staged => staged(&it.id, "new"),
                Place::Parked => staged(&it.id, "old"),
            };
            if !from.exists() {
                continue; // 上一次已经放好了。
            }
            new.create_parents(&self.root)?;
            let dest = new.on_host(&self.root);
            fs::rename(&from, &dest).map_err(io_err)?;
            dirs.insert(dest.parent().expect("under root").to_path_buf());
        }
        for d in dirs {
            sync_dir(&d)?;
        }
        Ok(())
    }

    /// 第 3 步：从镜像写出的内容清出镜像，记下文件身份，删掉意图记录。
    fn finish(&mut self, intent: &Intent) -> Result<(), UnitError> {
        let mut changes = Vec::new();
        let mut aux = vec![Aux::Delete(AUX_INTENT.to_vec())];
        let mut paths: BTreeMap<EntityId, Option<HostPath>> = BTreeMap::new();
        for (id, p, _) in &intent.edits {
            paths.insert(*id, Some(p.clone()));
        }
        for it in &intent.items {
            if it.place == Place::Image {
                changes.push(Change::Truncate { id: it.id, len: 0 });
            }
            paths.insert(it.id, it.new.clone());
        }
        for (id, p) in paths {
            match p.map(|p| p.inspect(&self.root).map(|m| (p, m))).transpose()? {
                Some((p, Some(m))) => {
                    aux.push(Aux::Put(identity_key(&id), Ident::of(&p.on_host(&self.root), &m).encode()))
                }
                _ => aux.push(Aux::Delete(identity_key(&id))),
            }
        }
        self.image.commit_batch(&mut Batch { changes: &changes, aux: &aux, ..Batch::default() })
    }
}

/// 走一遍 `dir`（挂载目录下 `names` 那一级），收集可映射的普通文件。不跟随符号链接。
fn walk(
    dir: &Path,
    names: &mut Vec<std::ffi::OsString>,
    found: &mut BTreeMap<HostPath, Ident>,
    report: &mut ScanReport,
) -> Result<(), UnitError> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        // 走到一半目录被删掉了：没有东西可映射。
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_err(e)),
    };
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        if names.is_empty() && name == RESERVED {
            continue;
        }
        // DirEntry 的类型不跟随符号链接。
        let Ok(ty) = entry.file_type() else { continue };
        names.push(name);
        if ty.is_dir() {
            walk(&entry.path(), names, found, report)?;
        } else if ty.is_file() {
            let refs: Vec<&std::ffi::OsStr> = names.iter().map(|n| n.as_os_str()).collect();
            match (HostPath::from_names(&refs), entry.metadata()) {
                (Some(p), Ok(m)) => {
                    found.insert(p, Ident::of(&entry.path(), &m));
                }
                (None, _) => report.skipped += 1,
                (Some(_), Err(_)) => {}
            }
        } else {
            report.skipped += 1;
        }
        names.pop();
    }
    Ok(())
}

impl Unit for DirUnit {
    fn id(&self) -> UnitId {
        self.inner.borrow().image.id()
    }

    fn mapped_keys(&self) -> &[&'static str] {
        &[HOSTFS_PATH]
    }

    fn snapshot(&self) -> Box<dyn Snapshot> {
        // 先把宿主上的改动同步进来。失败时留着错误给下一个能报错的调用。
        if let Err(e) = self.sync() {
            self.sync_error.set(Some(e));
        }
        Box::new(DirSnapshot { inner: self.inner.borrow().image.snapshot(), root: self.root.clone() })
    }

    fn apply(&mut self, changes: &[Change]) -> Result<(), UnitError> {
        self.sync_mut()?;
        self.inner.get_mut().run(changes, None)
    }

    fn prepare(&mut self, txn: &TxnInfo, changes: &[Change]) -> Result<(), UnitError> {
        self.sync_mut()?;
        // 先检查路径，免得提交点之后才发现这个单元做不了。
        let inner = self.inner.get_mut();
        inner.plan(changes)?;
        inner.image.prepare(txn, changes)
    }

    fn prepared(&self) -> Result<Vec<(TxnInfo, Vec<Change>)>, UnitError> {
        self.inner.borrow().image.prepared()
    }

    fn commit_prepared(&mut self, txn: TxnId, as_primary: bool) -> Result<(), UnitError> {
        self.sync_mut()?;
        let inner = self.inner.get_mut();
        let (_, changes) =
            inner.image.prepared()?.into_iter().find(|(t, _)| t.id == txn).ok_or(UnitError::UnknownTxn(txn))?;
        inner.run(&changes, Some((txn, as_primary)))
    }

    fn abort_prepared(&mut self, txn: TxnId) -> Result<(), UnitError> {
        self.inner.get_mut().image.abort_prepared(txn)
    }

    fn committed(&self) -> Result<Vec<TxnInfo>, UnitError> {
        self.inner.borrow().image.committed()
    }

    fn forget(&mut self, txn: TxnId) -> Result<(), UnitError> {
        self.inner.get_mut().image.forget(txn)
    }
}

/// 路径实体的长度和内容直接取自宿主文件的当前状态。
struct DirSnapshot {
    inner: Box<dyn Snapshot>,
    root: PathBuf,
}

impl DirSnapshot {
    fn host_file(&self, tags: &[Tag]) -> Result<Option<(HostPath, Option<fs::Metadata>)>, UnitError> {
        match path_of(tags)? {
            Some(p) => {
                let m = p.inspect(&self.root)?.filter(|m| m.is_file());
                Ok(Some((p, m)))
            }
            None => Ok(None),
        }
    }
}

impl Snapshot for DirSnapshot {
    fn get(&self, id: &EntityId) -> Result<Option<EntityRecord>, UnitError> {
        let Some(mut rec) = self.inner.get(id)? else { return Ok(None) };
        if let Some((_, m)) = self.host_file(&rec.tags)? {
            rec.size = m.map_or(0, |m| m.len());
        }
        Ok(Some(rec))
    }

    fn probe(&self, probe: &Probe<'_>) -> Result<Option<Vec<EntityId>>, UnitError> {
        self.inner.probe(probe)
    }

    fn all(&self) -> Result<Vec<EntityId>, UnitError> {
        self.inner.all()
    }

    fn read(&self, id: &EntityId, offset: u64, buf: &mut [u8]) -> Result<usize, UnitError> {
        let rec = self.inner.get(id)?.ok_or(UnitError::NotFound(*id))?;
        match self.host_file(&rec.tags)? {
            None => self.inner.read(id, offset, buf),
            Some((_, None)) => Ok(0),
            Some((p, Some(_))) => {
                let f = fs::File::open(p.on_host(&self.root)).map_err(io_err)?;
                read_at(&f, offset, buf).map_err(io_err)
            }
        }
    }
}

#[cfg(test)]
mod tests;
