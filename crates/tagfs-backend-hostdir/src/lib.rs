//! 目录单元：把宿主目录（Windows、Linux 这类层级文件系统）挂成一个存储单元，让上层能读写
//! 那里按路径组织的文件。
//!
//! - 挂载目录就是根 `/`。其中的每个普通文件是一个实体，带 `tagfs:hostfs_path`，值是它以挂载目录为根的
//!   路径；内容就是那个文件，读写都直接对文件进行（Tag 只负责找到文件）。目录不是实体。
//!   符号链接不映射，也不会被跟随。
//! - `tagfs:hostfs_path` 是映射出来的：加上它就在宿主上建出文件（缺的上层目录一并建出），
//!   改它就移动文件，去掉它或删除实体就删掉宿主文件。
//! - 没有路径的实体存在挂载目录下的镜像文件里（`/.tagfs/unit.img`，块格式），和别的单元一样。
//!   镜像文件同时存放全部实体的 Tag 和索引，以及目录单元自己的记录。镜像文件按需扩容。
//! - 宿主上的改动（建、删、改名、改内容）通过文件系统通知实时同步过来；挂载时还会全量对账一次。
//! - 暂存的内容放在宿主上的暂存文件里（`/.tagfs/staged/`）：新实体带路径时直接改名过去，不复制；
//!   不带路径时才搬进镜像。
//!
//! 元数据的一次修改分两步提交，中间做宿主上的文件操作：
//!
//! 1. 镜像里提交实体的修改，同时记下要做的宿主操作（意图记录）。这是提交点。
//! 2. 按意图记录操作宿主：要复制的文件在宿主上复制；再把要让出的文件挪进 `/.tagfs/tmp`，
//!    最后把各文件放到新路径。
//! 3. 镜像里再提交一次：删掉意图记录，记下文件身份。
//!
//! 之后断电，挂载时按意图记录把第 2、3 步做完：宿主操作都可以重做。
//!
//! 只有内容要换个地方存放时才会搬运整个文件：路径实体失去路径（内容进镜像）、从路径实体复制出没有路径的
//! 实体，或者镜像里的实体加上路径（内容写到宿主）。这些都是分段流式进行的，不会整个读进内存。

mod path;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use tagfs_backend_file::{ImageError, ImageUnit};
use tagfs_block::{Aux, Batch, OpenError};
use tagfs_core::{
    Change, EntityId, EntityRecord, IdSource, Key, Namespace, Probe, Snapshot, StageId, Tag, TxnId, TxnInfo, Unit,
    UnitError, UnitId, Value,
};
use tagfs_sync::Mutex;

pub use path::{HostPath, INVALID_PATH, PATH_TAKEN, RESERVED};

/// 目录单元映射的 `tagfs` 键：宿主层级文件系统（Windows、Linux 这一类按目录树组织的文件系统）里的路径。
///
/// 值是以挂载目录为根的路径，如 `/abc/bcd/1.file`，实体的内容就是那个文件。这个 Tag 不是存下来的，
/// 而是映射出来的：增、删、改它，就是在宿主上建立、删除、移动对应的文件。
pub const HOSTFS_PATH: &str = "hostfs_path";

const IMAGE: &str = "unit.img";
const TMP: &str = "tmp";
const STAGED: &str = "staged";
/// 第 2 步里“宿主上的复制”已经做完的标记。
const COPIED: &str = "copied";
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
    /// 暂存文件改名过去。
    FromStage(StageId),
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
    /// 宿主上的复制：新实体、源文件。在让出旧文件之前做。
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
        self.copies.is_empty() && self.items.is_empty()
    }

    fn encode(&self) -> Vec<u8> {
        let mut e = Enc(Vec::new());
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
            match it.place {
                Place::Nothing => e.u8(0),
                Place::Image => e.u8(1),
                Place::Staged => e.u8(2),
                Place::Parked => e.u8(3),
                Place::FromStage(s) => {
                    e.u8(4);
                    e.u64(s.0);
                }
            }
            e.u8(it.vacate as u8);
        }
        e.0
    }

    fn decode(b: &[u8]) -> Result<Intent, UnitError> {
        let mut d = Dec(b);
        let mut intent = Intent::default();
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
                4 => Place::FromStage(StageId(d.u64()?)),
                _ => return Err(UnitError::Corrupt),
            };
            intent.items.push(Item { id, old, new, place, vacate: d.u8()? != 0 });
        }
        Ok(intent)
    }
}

/// 一组修改在目录单元上的执行计划。
struct Plan {
    /// 失去路径的路径实体和它的文件：内容搬进镜像。
    hydrate: Vec<(EntityId, HostPath)>,
    /// 从路径实体复制、自己最后没有路径的新实体：第几项修改、源文件。内容从源文件搬进镜像。
    copy_in: Vec<(usize, HostPath)>,
    /// 内容要从暂存文件搬进镜像的暂存（实体最后没有路径，或者同一批里还要从它复制）。
    stage_in: Vec<StageId>,
    /// 暂存文件直接改名到新路径的暂存。
    stage_out: Vec<StageId>,
    /// 这一批用到的其余暂存：提交之后宿主上的暂存文件就没用了。
    stage_done: Vec<StageId>,
    intent: Intent,
}

// ---- 单元 ----

/// 需要对账的范围：挂载目录下的若干路径（按段），空表示整个目录。
type Scope = Vec<String>;

struct Watch {
    // 只为持有：放掉它就停止监听。
    _watcher: notify::RecommendedWatcher,
    events: Mutex<mpsc::Receiver<notify::Result<notify::Event>>>,
}

pub struct DirUnit {
    root: PathBuf,
    image: ImageUnit,
    ids: Mutex<Box<dyn IdSource + Send>>,
    /// 元数据修改和对账一次一个：它们都会搬动宿主上的文件。内容读写不用它。
    meta: Mutex<()>,
    watch: Option<Watch>,
    /// 取快照前同步失败的错误，下一个能报错的调用把它报出来。
    sync_error: Mutex<Option<UnitError>>,
    /// 写过、还没落盘的宿主文件。
    dirty: Mutex<BTreeSet<HostPath>>,
}

impl DirUnit {
    /// 在已有的目录上建目录单元，并把里面的文件映射进来。
    pub fn create(
        root: impl AsRef<Path>,
        unit: UnitId,
        ids: Box<dyn IdSource + Send>,
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
    pub fn open(root: impl AsRef<Path>, ids: Box<dyn IdSource + Send>) -> Result<(Self, ScanReport), DirError> {
        let root = fs::canonicalize(root)?;
        let img = root.join(RESERVED).join(IMAGE);
        if !img.is_file() {
            return Err(DirError::NotFound);
        }
        let image = tagfs_backend_file::open(img)?;
        Self::start(root, image, ids)
    }

    fn start(root: PathBuf, image: ImageUnit, ids: Box<dyn IdSource + Send>) -> Result<(Self, ScanReport), DirError> {
        // 先开始监听，再对账：对账期间发生的改动不会漏掉。
        let watch = start_watch(&root);
        let me = DirUnit {
            root,
            image,
            ids: Mutex::new(ids),
            meta: Mutex::new(()),
            watch,
            sync_error: Mutex::new(None),
            dirty: Mutex::new(BTreeSet::new()),
        };
        if let Some(raw) = me.image.aux_get(AUX_INTENT)? {
            let intent = Intent::decode(&raw)?;
            me.carry_out(&intent)?;
            me.finish(&intent)?;
        }
        me.clean_tmp()?;
        me.clean_staged()?;
        let report = me.reconcile(None)?;
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
    pub fn image(&self) -> &ImageUnit {
        &self.image
    }

    /// 全量对账。
    pub fn rescan(&self) -> Result<ScanReport, UnitError> {
        self.drain_events();
        let _one = self.meta.lock();
        self.reconcile(None)
    }

    /// 把收到的宿主改动通知同步进来。
    pub fn refresh(&self) -> Result<ScanReport, UnitError> {
        if let Some(e) = self.sync_error.lock().take() {
            return Err(e);
        }
        match self.drain_events() {
            None => Ok(ScanReport::default()),
            Some(scopes) => {
                let _one = self.meta.lock();
                self.reconcile(scopes.map(|s| s.into_iter().collect()))
            }
        }
    }

    /// 取出全部待处理的通知，合并成对账范围。没有通知时为 `None`；`Some(None)` 表示要全量对账。
    fn drain_events(&self) -> Option<Option<BTreeSet<Scope>>> {
        let watch = self.watch.as_ref()?;
        let events = watch.events.lock();
        let mut scopes = BTreeSet::new();
        let mut full = false;
        let mut any = false;
        loop {
            match events.try_recv() {
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

    fn tmp(&self) -> PathBuf {
        self.root.join(RESERVED).join(TMP)
    }

    fn staged_file(&self, s: StageId) -> PathBuf {
        self.root.join(RESERVED).join(STAGED).join(s.0.to_string())
    }

    fn clean_tmp(&self) -> Result<(), UnitError> {
        match fs::remove_dir_all(self.tmp()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err(e)),
        }
    }

    /// 镜像里已经没有对应暂存的暂存文件：上次没用上就断了，删掉。
    fn clean_staged(&self) -> Result<(), UnitError> {
        let live: BTreeSet<String> = self.image.stages()?.iter().map(|s| s.0.to_string()).collect();
        let entries = match fs::read_dir(self.root.join(RESERVED).join(STAGED)) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(io_err(e)),
        };
        for entry in entries.flatten() {
            if !entry.file_name().to_str().is_some_and(|n| live.contains(n)) {
                fs::remove_file(entry.path()).map_err(io_err)?;
            }
        }
        Ok(())
    }

    fn remove_staged_file(&self, s: StageId) -> Result<(), UnitError> {
        match fs::remove_file(self.staged_file(s)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err(e)),
        }
    }

    /// 暂存文件落盘（准备记录或意图记录要引用它之前）。
    fn sync_staged_file(&self, s: StageId) -> Result<(), UnitError> {
        match fs::File::open(self.staged_file(s)) {
            Ok(f) => f.sync_all().map_err(io_err),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(UnitError::UnknownStage(s)),
            Err(e) => Err(io_err(e)),
        }
    }

    /// 路径实体的宿主文件，打开来写。文件在宿主上不见了就当实体不存在（对账时它会被删掉）。
    fn open_host(&self, id: &EntityId, p: &HostPath) -> Result<fs::File, UnitError> {
        match p.inspect(&self.root)? {
            Some(m) if m.is_file() => fs::OpenOptions::new().write(true).open(p.on_host(&self.root)).map_err(io_err),
            _ => Err(UnitError::NotFound(*id)),
        }
    }

    /// 实体当前的映射路径；不存在时报错。
    fn located(&self, id: &EntityId) -> Result<Option<HostPath>, UnitError> {
        self.refresh()?;
        let rec = self.image.snapshot().get(id)?.ok_or(UnitError::NotFound(*id))?;
        path_of(&rec.tags)
    }

    /// 把 `from`（不存在就当空的）的内容分段写进镜像里的暂存。
    fn stream_in(&self, from: Option<PathBuf>, stage: StageId) -> Result<(), UnitError> {
        let Some(from) = from else { return Ok(()) };
        let f = match fs::File::open(from) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(io_err(e)),
        };
        let mut buf = vec![0u8; CHUNK];
        let mut offset = 0u64;
        loop {
            let n = read_at(&f, offset, &mut buf).map_err(io_err)?;
            if n == 0 {
                return Ok(());
            }
            self.image.write_staged(stage, offset, &buf[..n])?;
            offset += n as u64;
        }
    }

    /// 路径实体在宿主上的文件（经过符号链接检查）；不存在为 `None`。
    fn host_file(&self, p: &HostPath) -> Result<Option<PathBuf>, UnitError> {
        Ok(match p.inspect(&self.root)? {
            Some(m) if m.is_file() => Some(p.on_host(&self.root)),
            _ => None,
        })
    }

    /// 范围内的宿主文件和镜像对账。`scopes` 为 `None` 时是整个目录。调用者持有 `meta`。
    fn reconcile(&self, scopes: Option<BTreeSet<Scope>>) -> Result<ScanReport, UnitError> {
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
            let id = EntityId::generate(&mut **self.ids.lock());
            changes.push(Change::Create { id, tags: vec![path_tag(&p)], content: None });
            aux.push(Aux::Put(identity_key(&id), ident.encode()));
            report.added += 1;
        }
        if !changes.is_empty() || !aux.is_empty() {
            self.image.commit_batch(&Batch { changes: &changes, aux: &aux, ..Batch::default() })?;
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
        let mut copies: Vec<(usize, EntityId, EntityId)> = Vec::new();
        let mut created: Vec<(EntityId, StageId)> = Vec::new();
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
                Change::Create { id, tags, content } => {
                    load(id, &mut after)?;
                    after.insert(*id, Some(tags.clone()));
                    if let Some(s) = content {
                        created.push((*id, *s));
                    }
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
            }
        }
        drop(snap);

        // 最后的状态：不存在为 None，存在时为它的路径。
        let mut finals: BTreeMap<EntityId, Option<Option<HostPath>>> = BTreeMap::new();
        for (id, tags) in &after {
            finals.insert(*id, tags.as_ref().map(|t| path_of(t)).transpose()?);
        }
        let on_host = |e: &EntityId| matches!(before.get(e), Some(Some(_)));

        // 失去路径的路径实体：内容搬进镜像。其余路径实体的内容一直留在宿主。
        let hydrate: Vec<(EntityId, HostPath)> = finals
            .iter()
            .filter(|(id, fin)| on_host(id) && matches!(fin, Some(None)))
            .map(|(id, _)| (*id, before[id].clone().expect("on host")))
            .collect();
        let hydrated: BTreeSet<EntityId> = hydrate.iter().map(|(id, _)| *id).collect();
        let direct: BTreeSet<EntityId> =
            before.iter().filter(|(id, p)| p.is_some() && !hydrated.contains(id)).map(|(id, _)| *id).collect();

        // 从路径实体复制：目标有路径就在宿主上复制文件，没有路径就把文件内容搬进镜像。
        let mut copy_in = Vec::new();
        let mut staged: BTreeMap<EntityId, HostPath> = BTreeMap::new();
        for (i, f, t) in &copies {
            if !on_host(f) {
                continue;
            }
            let src = before[f].clone().expect("on host");
            match &finals[t] {
                None => {}
                Some(None) => copy_in.push((*i, src)),
                Some(Some(_)) => {
                    staged.insert(*t, src);
                }
            }
        }

        // 带暂存的新实体：最后有路径（且这一批里没人从它复制）就把暂存文件改名过去，否则内容进镜像。
        let sources: BTreeSet<EntityId> = copies.iter().map(|(_, f, _)| *f).collect();
        let mut from_stage: BTreeMap<EntityId, StageId> = BTreeMap::new();
        let (mut stage_in, mut stage_done) = (Vec::new(), Vec::new());
        for (id, s) in &created {
            match &finals[id] {
                Some(Some(_)) if !sources.contains(id) => {
                    from_stage.insert(*id, *s);
                }
                Some(_) => stage_in.push(*s),
                None => stage_done.push(*s),
            }
        }
        stage_done.extend(&stage_in);

        let mut intent =
            Intent { copies: staged.iter().map(|(t, src)| (*t, src.clone())).collect(), ..Intent::default() };
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
                Some(_) if from_stage.contains_key(id) => Place::FromStage(from_stage[id]),
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
        let stage_out = from_stage.into_values().collect();
        Ok(Plan { hydrate, copy_in, stage_in, stage_out, stage_done, intent })
    }

    /// 第 1 步到第 3 步。`finish` 是同时了结的准备记录。调用者持有 `meta`。
    fn run(&self, changes: &[Change], finish: Option<(TxnId, bool)>) -> Result<(), UnitError> {
        let plan = self.plan(changes)?;
        // 要搬进镜像的内容先分段写进镜像里的暂存（这一步不挡别人读写），提交时一起生效。
        let mut ours = Vec::new();
        let mut first = || -> Result<(), UnitError> {
            let mut adopt = Vec::new();
            for (id, p) in &plan.hydrate {
                let s = self.image.stage()?;
                ours.push(s);
                self.stream_in(self.host_file(p)?, s)?;
                adopt.push((*id, s));
            }
            let mut image_changes = changes.to_vec();
            for (i, src) in &plan.copy_in {
                let s = self.image.stage()?;
                ours.push(s);
                self.stream_in(self.host_file(src)?, s)?;
                let Change::Copy { id, tags, .. } = &changes[*i] else { unreachable!("copy_in comes from a copy") };
                image_changes[*i] = Change::Create { id: *id, tags: tags.clone(), content: Some(s) };
            }
            for s in &plan.stage_in {
                self.stream_in(Some(self.staged_file(*s)), *s)?;
            }
            for s in &plan.stage_out {
                self.sync_staged_file(*s)?;
            }
            let aux = if plan.intent.is_empty() {
                Vec::new()
            } else {
                vec![Aux::Put(AUX_INTENT.to_vec(), plan.intent.encode())]
            };
            self.image.commit_batch(&Batch { adopt: &adopt, changes: &image_changes, aux: &aux, finish })
        };
        if let Err(e) = first() {
            for s in ours {
                let _ = self.image.discard_staged(s);
            }
            return Err(e);
        }
        // 内容已经进了镜像的暂存文件不再需要。断在这里也没关系：挂载时没有对应暂存的暂存文件会被删掉。
        for s in &plan.stage_done {
            self.remove_staged_file(*s)?;
        }
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

        // 宿主上的复制。
        if !tmp.join(COPIED).exists() {
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
            fs::write(tmp.join(COPIED), b"").map_err(io_err)?;
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
                Place::FromStage(s) => self.staged_file(s),
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

    /// 第 3 步：从镜像写出的内容清出镜像（换成空的内容），记下文件身份，删掉意图记录。
    fn finish(&self, intent: &Intent) -> Result<(), UnitError> {
        let mut adopt = Vec::new();
        let mut aux = vec![Aux::Delete(AUX_INTENT.to_vec())];
        let mut paths: BTreeMap<EntityId, Option<HostPath>> = BTreeMap::new();
        for it in &intent.items {
            if it.place == Place::Image {
                adopt.push((it.id, self.image.stage()?));
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
        self.image.commit_batch(&Batch { adopt: &adopt, aux: &aux, ..Batch::default() })
    }

    /// 准备记录里引用的暂存。
    fn stages_of(changes: &[Change]) -> impl Iterator<Item = StageId> + '_ {
        changes.iter().filter_map(|c| match c {
            Change::Create { content: Some(s), .. } => Some(*s),
            _ => None,
        })
    }
}

fn start_watch(root: &Path) -> Option<Watch> {
    use notify::Watcher;
    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(tx).ok()?;
    watcher.watch(root, notify::RecursiveMode::Recursive).ok()?;
    Some(Watch { _watcher: watcher, events: Mutex::new(rx) })
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
        self.image.id()
    }

    fn mapped_keys(&self) -> &[&'static str] {
        &[HOSTFS_PATH]
    }

    fn snapshot(&self) -> Box<dyn Snapshot> {
        // 先把宿主上的改动同步进来。失败时留着错误给下一个能报错的调用。
        if let Err(e) = self.refresh() {
            *self.sync_error.lock() = Some(e);
        }
        Box::new(DirSnapshot { inner: self.image.snapshot(), root: self.root.clone() })
    }

    /// 路径实体直接写宿主文件；其余的写镜像。
    fn write(&self, id: &EntityId, offset: u64, data: &[u8]) -> Result<(), UnitError> {
        match self.located(id)? {
            None => self.image.write(id, offset, data),
            Some(p) => {
                write_at(&self.open_host(id, &p)?, offset, data).map_err(io_err)?;
                self.dirty.lock().insert(p);
                Ok(())
            }
        }
    }

    fn set_len(&self, id: &EntityId, len: u64) -> Result<(), UnitError> {
        match self.located(id)? {
            None => self.image.set_len(id, len),
            Some(p) => {
                self.open_host(id, &p)?.set_len(len).map_err(io_err)?;
                self.dirty.lock().insert(p);
                Ok(())
            }
        }
    }

    fn sync(&self) -> Result<(), UnitError> {
        self.image.sync()?;
        let dirty = std::mem::take(&mut *self.dirty.lock());
        for p in dirty {
            if let Some(f) = self.host_file(&p)? {
                fs::File::open(f).and_then(|f| f.sync_all()).map_err(io_err)?;
            }
        }
        Ok(())
    }

    /// 暂存放在宿主上的暂存文件里；镜像里同时有一份空的暂存，编号由它分配，断电后靠它认领暂存文件。
    fn stage(&self) -> Result<StageId, UnitError> {
        let s = self.image.stage()?;
        let made = fs::create_dir_all(self.root.join(RESERVED).join(STAGED))
            .and_then(|()| fs::File::create(self.staged_file(s)).map(|_| ()));
        if let Err(e) = made {
            let _ = self.image.discard_staged(s);
            return Err(io_err(e));
        }
        Ok(s)
    }

    fn write_staged(&self, stage: StageId, offset: u64, data: &[u8]) -> Result<(), UnitError> {
        let f = match fs::OpenOptions::new().write(true).open(self.staged_file(stage)) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(UnitError::UnknownStage(stage)),
            Err(e) => return Err(io_err(e)),
        };
        write_at(&f, offset, data).map_err(io_err)
    }

    fn discard_staged(&self, stage: StageId) -> Result<(), UnitError> {
        self.image.discard_staged(stage)?;
        self.remove_staged_file(stage)
    }

    fn apply(&self, changes: &[Change]) -> Result<(), UnitError> {
        self.refresh()?;
        let _one = self.meta.lock();
        self.run(changes, None)
    }

    fn prepare(&self, txn: &TxnInfo, changes: &[Change]) -> Result<(), UnitError> {
        self.refresh()?;
        let _one = self.meta.lock();
        // 先检查路径，免得提交点之后才发现这个单元做不了。
        self.plan(changes)?;
        for s in Self::stages_of(changes) {
            self.sync_staged_file(s)?;
        }
        self.image.prepare(txn, changes)
    }

    fn prepared(&self) -> Result<Vec<(TxnInfo, Vec<Change>)>, UnitError> {
        self.image.prepared()
    }

    fn commit_prepared(&self, txn: TxnId, as_primary: bool) -> Result<(), UnitError> {
        self.refresh()?;
        let _one = self.meta.lock();
        let (_, changes) =
            self.image.prepared()?.into_iter().find(|(t, _)| t.id == txn).ok_or(UnitError::UnknownTxn(txn))?;
        self.run(&changes, Some((txn, as_primary)))
    }

    fn abort_prepared(&self, txn: TxnId) -> Result<(), UnitError> {
        let _one = self.meta.lock();
        let (_, changes) =
            self.image.prepared()?.into_iter().find(|(t, _)| t.id == txn).ok_or(UnitError::UnknownTxn(txn))?;
        self.image.abort_prepared(txn)?;
        for s in Self::stages_of(&changes) {
            self.remove_staged_file(s)?;
        }
        Ok(())
    }

    fn committed(&self) -> Result<Vec<TxnInfo>, UnitError> {
        self.image.committed()
    }

    fn forget(&self, txn: TxnId) -> Result<(), UnitError> {
        self.image.forget(txn)
    }

    /// 先和宿主目录全量对账，再检查镜像。
    fn check(&self) -> Result<(), UnitError> {
        self.rescan()?;
        self.image.verify()
    }

    fn describe(&self) -> Vec<(&'static str, String)> {
        let snap = self.image.snapshot();
        let (ns, key) = hostfs_key();
        let files = snap.probe(&Probe::Has { ns: &ns, key: &key }).ok().flatten().map_or(0, |f| f.len());
        let all = snap.all().map_or(0, |a| a.len());
        let mut out = vec![
            ("root", self.root.display().to_string()),
            ("files", format!("{files} mapped from the directory, {} only in the image", all - files)),
            ("live sync", String::from(if self.is_live() { "yes" } else { "no" })),
        ];
        for (name, value) in self.image.describe() {
            out.push((if name == "size" { "image size" } else { name }, value));
        }
        out
    }
}

/// `path` 是不是目录单元的挂载目录。只看，不改。
pub fn recognizes(path: impl AsRef<Path>) -> bool {
    let path = path.as_ref();
    path.is_dir() && path.join(RESERVED).join(IMAGE).is_file()
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
