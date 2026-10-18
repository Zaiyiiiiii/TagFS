//! 目录单元：把宿主目录（Windows、Linux 这类层级文件系统）挂成一个存储单元，让上层能读写
//! 那里按路径组织的文件。
//!
//! - 挂载目录就是根 `/`。其中的每个普通文件是一个实体，带 `tagfs:hostfs_path`，值是它以挂载目录为根的
//!   路径；内容就是那个文件，不复制。目录不是实体。符号链接不映射，也不会被跟随。
//! - `tagfs:hostfs_path` 是映射出来的：加上它就在宿主上建出文件（缺的上层目录一并建出），
//!   改它就移动文件，去掉它或删除实体就删掉宿主文件。
//! - 没有路径的实体存在挂载目录下的镜像文件里（`/.tagfs/unit.img`，块格式），和别的单元一样。
//!   镜像文件同时存放全部实体的 Tag 和索引，以及目录单元自己的记录。
//!
//! 一次修改分两步提交，中间做宿主上的文件操作：
//!
//! 1. 镜像里提交实体的修改，同时记下要做的宿主操作（意图记录）。要读写内容的路径实体先把宿主文件的内容
//!    搬进镜像，修改照常在镜像里进行。
//! 2. 按意图记录操作宿主：先把要让出的文件挪进 `/.tagfs/tmp`，再把新内容写到临时文件、改名到位，或者把挪走的
//!    文件改名到新路径。
//! 3. 镜像里再提交一次：路径实体的内容清出镜像，删掉意图记录。
//!
//! 第 1 步是提交点。之后断电，挂载时按意图记录把第 2、3 步做完。宿主操作都可以重做。
//!
//! 挂载时扫描整个目录，和镜像里的记录对账：新出现的文件建成实体，消失的删掉，改了名的（按 inode 识别，
//! 仅 POSIX）更新路径、保持实体不变。
//!
//! 路径实体的内容是宿主文件的当前内容，不受快照保护：宿主上的程序随时可能改它。

mod path;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use tagfs_block::{Aux, Batch, BlockUnit, OpenError};
use tagfs_core::tag::tagfs_keys::HOSTFS_PATH;
use tagfs_core::{
    Change, EntityId, EntityRecord, IdSource, Key, Namespace, Probe, Snapshot, Tag, TxnId, TxnInfo, Unit, UnitError,
    UnitId, Value,
};
use tagfs_device_host::FileDevice;

pub use path::{HostPath, RESERVED};

const IMAGE: &str = "unit.img";
const TMP: &str = "tmp";
/// 第 2 步里“让出旧文件”已经做完的标记。
const PHASE2: &str = "phase2";
const BLOCK_SIZE: u32 = 4096;

const AUX_INTENT: &[u8] = b"intent";
const AUX_IDENTITY: &[u8] = b"id/";

#[derive(Debug, PartialEq, Eq)]
pub enum DirError {
    /// 挂载点不是目录。
    NotADirectory,
    /// 已经有目录单元了（`create` 时）或者还没有（`open` 时）。
    AlreadyExists,
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

impl From<OpenError> for DirError {
    fn from(e: OpenError) -> Self {
        DirError::Open(e)
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

/// 一次扫描对账的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub added: usize,
    pub removed: usize,
    pub renamed: usize,
    /// 没有映射的文件：名字不是 UTF-8、在宿主上不合法、路径太长、符号链接或特殊文件。
    pub skipped: usize,
}

pub struct DirUnit {
    root: PathBuf,
    image: BlockUnit<FileDevice>,
    ids: Box<dyn IdSource>,
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
        let Value::Str(v) = &t.value else { return Err(UnitError::InvalidPath) };
        if found.replace(HostPath::parse(v)?).is_some() {
            return Err(UnitError::InvalidPath);
        }
    }
    Ok(found)
}

fn path_tag(p: &HostPath) -> Tag {
    Tag::new(Namespace::TAGFS, HOSTFS_PATH, p.value())
}

/// 文件在宿主上的身份，用来认出改名。拿不到时为空，这时不做改名识别。
///
/// inode 号会被复用：删掉一个文件再建一个，新文件可能拿到同一个号。所以宿主能报告创建时间时把它也算进去。
fn identity(m: &fs::Metadata) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mut v = m.dev().to_le_bytes().to_vec();
        v.extend_from_slice(&m.ino().to_le_bytes());
        if let Ok(born) = m.created()
            && let Ok(d) = born.duration_since(std::time::UNIX_EPOCH)
        {
            v.extend_from_slice(&d.as_nanos().to_le_bytes());
        }
        v
    }
    #[cfg(not(unix))]
    {
        let _ = m;
        Vec::new()
    }
}

fn identity_key(id: &EntityId) -> Vec<u8> {
    let mut k = AUX_IDENTITY.to_vec();
    k.extend_from_slice(&id.0);
    k
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

fn hex(id: &EntityId) -> String {
    id.0.iter().map(|b| format!("{b:02x}")).collect()
}

/// 一项宿主操作，见模块说明。
#[derive(Clone, Debug, PartialEq, Eq)]
struct Item {
    id: EntityId,
    /// 修改前的路径；有 `vacate` 时要让出。
    old: Option<HostPath>,
    /// 修改后的路径。
    new: Option<HostPath>,
    /// 新路径的内容从镜像写出（否则是把旧文件改名过去）。
    materialize: bool,
    /// 旧文件要让出：挪进临时目录，最后删掉或改名到新路径。
    vacate: bool,
}

fn encode_intent(items: &[Item]) -> Vec<u8> {
    let mut b = (items.len() as u32).to_le_bytes().to_vec();
    for it in items {
        b.extend_from_slice(&it.id.0);
        let flags = it.old.is_some() as u8
            | (it.new.is_some() as u8) << 1
            | (it.materialize as u8) << 2
            | (it.vacate as u8) << 3;
        b.push(flags);
        for p in [&it.old, &it.new].into_iter().flatten() {
            let v = p.value();
            b.extend_from_slice(&(v.len() as u32).to_le_bytes());
            b.extend_from_slice(v.as_bytes());
        }
    }
    b
}

fn decode_intent(b: &[u8]) -> Result<Vec<Item>, UnitError> {
    let mut pos = 0;
    let mut take = |n: usize| -> Result<&[u8], UnitError> {
        let s = b.get(pos..pos + n).ok_or(UnitError::Corrupt)?;
        pos += n;
        Ok(s)
    };
    let count = u32::from_le_bytes(take(4)?.try_into().unwrap());
    let mut items = Vec::new();
    for _ in 0..count {
        let id = EntityId(take(16)?.try_into().unwrap());
        let flags = take(1)?[0];
        let mut read_path = |present: bool| -> Result<Option<HostPath>, UnitError> {
            if !present {
                return Ok(None);
            }
            let n = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
            let s = std::str::from_utf8(take(n)?).map_err(|_| UnitError::Corrupt)?;
            HostPath::parse(s).map(Some).map_err(|_| UnitError::Corrupt)
        };
        let old = read_path(flags & 1 != 0)?;
        let new = read_path(flags & 2 != 0)?;
        items.push(Item { id, old, new, materialize: flags & 4 != 0, vacate: flags & 8 != 0 });
    }
    Ok(items)
}

/// 一组修改在目录单元上的执行计划。
struct Plan {
    /// 先搬进镜像的路径实体内容。
    hydrate: Vec<Change>,
    items: Vec<Item>,
}

impl DirUnit {
    /// 在已有的目录上建目录单元，并把里面的文件映射进来。`image_capacity` 是镜像文件的容量（字节）；
    /// 镜像文件按需占用空间。
    pub fn create(
        root: impl AsRef<Path>,
        unit: UnitId,
        image_capacity: u64,
        ids: Box<dyn IdSource>,
    ) -> Result<(Self, ScanReport), DirError> {
        let root = root.as_ref().to_path_buf();
        if !fs::metadata(&root)?.is_dir() {
            return Err(DirError::NotADirectory);
        }
        let own = root.join(RESERVED);
        match fs::create_dir(&own) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err(DirError::AlreadyExists),
            Err(e) => return Err(e.into()),
        }
        let dev = FileDevice::create(own.join(IMAGE), image_capacity, BLOCK_SIZE)?;
        let image = BlockUnit::format(dev, unit)?;
        let mut me = DirUnit { root, image, ids };
        let report = me.rescan()?;
        Ok((me, report))
    }

    /// 挂载已有的目录单元：完成断电前没做完的宿主操作，再和目录对账。
    pub fn open(root: impl AsRef<Path>, ids: Box<dyn IdSource>) -> Result<(Self, ScanReport), DirError> {
        let root = root.as_ref().to_path_buf();
        let img = root.join(RESERVED).join(IMAGE);
        if !img.is_file() {
            return Err(DirError::NotFound);
        }
        let image = BlockUnit::open(FileDevice::open(img, BLOCK_SIZE, false)?)?;
        let mut me = DirUnit { root, image, ids };
        if let Some(raw) = me.image.aux_get(AUX_INTENT)? {
            let items = decode_intent(&raw)?;
            me.carry_out(&items)?;
            me.finish(&items)?;
        }
        me.clean_tmp()?;
        let report = me.rescan()?;
        Ok((me, report))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 镜像（块格式单元），检查用。
    pub fn image(&self) -> &BlockUnit<FileDevice> {
        &self.image
    }

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

    /// 重新扫描挂载目录，和镜像里的路径实体对账。
    pub fn rescan(&mut self) -> Result<ScanReport, UnitError> {
        let mut report = ScanReport::default();
        let mut found: BTreeMap<HostPath, Vec<u8>> = BTreeMap::new();
        self.walk(&self.root.clone(), &mut Vec::new(), &mut found, &mut report)?;

        let snap = self.image.snapshot();
        let (ns, key) = hostfs_key();
        let mut known: Vec<(EntityId, HostPath)> = Vec::new();
        for id in snap.probe(&Probe::Has { ns: &ns, key: &key })?.unwrap_or_default() {
            let rec = snap.get(&id)?.ok_or(UnitError::Corrupt)?;
            if let Some(p) = path_of(&rec.tags)? {
                known.push((id, p));
            }
        }
        let identities: BTreeMap<Vec<u8>, Vec<u8>> = self.image.aux_list(AUX_IDENTITY)?.into_iter().collect();

        let mut changes = Vec::new();
        let mut aux = Vec::new();
        let mut lost = Vec::new();
        for (id, p) in known {
            match found.remove(&p) {
                Some(ident) => {
                    if identities.get(&identity_key(&id)) != Some(&ident) {
                        aux.push(Aux::Put(identity_key(&id), ident));
                    }
                }
                None => lost.push((id, p)),
            }
        }
        for (id, old) in lost {
            let ident = identities.get(&identity_key(&id)).filter(|i| !i.is_empty());
            let moved = ident.and_then(|i| found.iter().find(|(_, v)| *v == i).map(|(p, _)| p.clone()));
            match moved {
                Some(new) => {
                    found.remove(&new);
                    changes.push(Change::RemoveTag { id, tag: path_tag(&old) });
                    changes.push(Change::AddTag { id, tag: path_tag(&new) });
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
            aux.push(Aux::Put(identity_key(&id), ident));
            report.added += 1;
        }
        drop(snap);
        if !changes.is_empty() || !aux.is_empty() {
            self.image.commit_batch(&Batch { changes: &changes, aux: &aux, ..Batch::default() })?;
        }
        Ok(report)
    }

    fn walk(
        &self,
        dir: &Path,
        names: &mut Vec<std::ffi::OsString>,
        found: &mut BTreeMap<HostPath, Vec<u8>>,
        report: &mut ScanReport,
    ) -> Result<(), UnitError> {
        for entry in fs::read_dir(dir).map_err(io_err)? {
            let entry = entry.map_err(io_err)?;
            let name = entry.file_name();
            if names.is_empty() && name == RESERVED {
                continue;
            }
            // DirEntry 的类型不跟随符号链接。
            let ty = entry.file_type().map_err(io_err)?;
            names.push(name);
            if ty.is_dir() {
                self.walk(&entry.path(), names, found, report)?;
            } else if ty.is_file() {
                let refs: Vec<&std::ffi::OsStr> = names.iter().map(|n| n.as_os_str()).collect();
                match HostPath::from_names(&refs) {
                    Some(p) => {
                        let m = entry.metadata().map_err(io_err)?;
                        found.insert(p, identity(&m));
                    }
                    None => report.skipped += 1,
                }
            } else {
                report.skipped += 1;
            }
            names.pop();
        }
        Ok(())
    }

    /// 读宿主文件的全部内容。
    fn read_host(&self, p: &HostPath) -> Result<Vec<u8>, UnitError> {
        match p.inspect(&self.root)? {
            Some(m) if m.is_file() => fs::read(p.on_host(&self.root)).map_err(io_err),
            // 文件在宿主上被删了：内容按空算，下次扫描时实体会被删掉。
            _ => Ok(Vec::new()),
        }
    }

    /// 算出一组修改要做的宿主操作，并检查路径。
    fn plan(&self, changes: &[Change]) -> Result<Plan, UnitError> {
        let snap = self.image.snapshot();
        // 涉及的实体：修改前的路径（只对修改前就存在的实体），以及模拟到最后的 Tag。
        let mut before: BTreeMap<EntityId, Option<HostPath>> = BTreeMap::new();
        let mut after: BTreeMap<EntityId, Option<Vec<Tag>>> = BTreeMap::new();
        let mut written = BTreeSet::new();
        let mut needs_content = BTreeSet::new();
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
        for c in changes {
            match c {
                Change::Create { id, tags, .. } => {
                    load(id, &mut after)?;
                    after.insert(*id, Some(tags.clone()));
                    written.insert(*id);
                }
                Change::Copy { from, id, tags } => {
                    load(from, &mut after)?;
                    load(id, &mut after)?;
                    needs_content.insert(*from);
                    after.insert(*id, Some(tags.clone()));
                    written.insert(*id);
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
                    needs_content.insert(*id);
                    written.insert(*id);
                }
            }
        }

        let mut items = Vec::new();
        let mut hydrate_ids = BTreeSet::new();
        let mut targets: BTreeMap<HostPath, EntityId> = BTreeMap::new();
        for (id, tags) in &after {
            let old = before.get(id).cloned().flatten();
            let new = match tags {
                Some(t) => path_of(t)?,
                None => None,
            };
            // 内容要进镜像：要读写它，或者实体还在、但不再有宿主文件。
            if old.is_some() && (needs_content.contains(id) || (tags.is_some() && new.is_none())) {
                hydrate_ids.insert(*id);
            }
            let materialize = new.is_some() && (written.contains(id) || hydrate_ids.contains(id) || old.is_none());
            let vacate = old.is_some() && (old != new || materialize);
            if let Some(p) = &new
                && targets.insert(p.clone(), *id).is_some()
            {
                return Err(UnitError::PathTaken);
            }
            if materialize || vacate || old != new {
                items.push(Item { id: *id, old, new, materialize, vacate });
            }
        }
        // 新路径在宿主上必须空着（或者正好被这次让出）。
        let vacated: BTreeSet<&HostPath> = items.iter().filter(|i| i.vacate).filter_map(|i| i.old.as_ref()).collect();
        for it in &items {
            if let Some(p) = &it.new
                && p.inspect(&self.root)?.is_some()
                && !vacated.contains(p)
            {
                return Err(UnitError::PathTaken);
            }
        }
        let mut hydrate = Vec::new();
        for id in hydrate_ids {
            let p = before[&id].as_ref().expect("has a host file");
            hydrate.push(Change::Write { id, offset: 0, data: self.read_host(p)? });
        }
        Ok(Plan { hydrate, items })
    }

    /// 第 1 步到第 3 步。`finish` 是同时了结的准备记录。
    fn run(&mut self, changes: &[Change], finish: Option<(TxnId, bool)>) -> Result<(), UnitError> {
        let plan = self.plan(changes)?;
        let mut all = plan.hydrate;
        all.extend_from_slice(changes);
        let aux = if plan.items.is_empty() {
            Vec::new()
        } else {
            vec![Aux::Put(AUX_INTENT.to_vec(), encode_intent(&plan.items))]
        };
        self.image.commit_batch(&Batch { changes: &all, aux: &aux, finish })?;
        if plan.items.is_empty() {
            return Ok(());
        }
        self.carry_out(&plan.items)?;
        self.finish(&plan.items)?;
        self.clean_tmp()
    }

    /// 第 2 步：按意图记录操作宿主。可以重做。
    fn carry_out(&self, items: &[Item]) -> Result<(), UnitError> {
        let tmp = self.tmp();
        fs::create_dir_all(&tmp).map_err(io_err)?;
        let marker = tmp.join(PHASE2);
        if !marker.exists() {
            for it in items.iter().filter(|i| i.vacate) {
                let old = it.old.as_ref().expect("vacate has old");
                let parked = tmp.join(format!("{}.old", hex(&it.id)));
                if matches!(old.inspect(&self.root), Ok(Some(m)) if m.is_file()) && !parked.exists() {
                    fs::rename(old.on_host(&self.root), &parked).map_err(io_err)?;
                }
            }
            fs::write(&marker, b"").map_err(io_err)?;
            sync_dir(&tmp)?;
        }
        let snap = self.image.snapshot();
        let mut dirs = BTreeSet::new();
        for it in items {
            let Some(new) = &it.new else { continue };
            new.create_parents(&self.root)?;
            let dest = new.on_host(&self.root);
            if it.materialize {
                let size = snap.get(&it.id)?.ok_or(UnitError::Corrupt)?.size;
                let mut content = vec![0u8; size as usize];
                snap.read(&it.id, 0, &mut content)?;
                let staged = tmp.join(format!("{}.new", hex(&it.id)));
                let mut f = fs::File::create(&staged).map_err(io_err)?;
                io::Write::write_all(&mut f, &content).map_err(io_err)?;
                f.sync_all().map_err(io_err)?;
                fs::rename(&staged, &dest).map_err(io_err)?;
            } else {
                let parked = tmp.join(format!("{}.old", hex(&it.id)));
                if parked.exists() {
                    fs::rename(&parked, &dest).map_err(io_err)?;
                }
            }
            dirs.insert(dest.parent().expect("under root").to_path_buf());
        }
        for d in dirs {
            sync_dir(&d)?;
        }
        Ok(())
    }

    /// 第 3 步：路径实体的内容清出镜像，记下文件身份，删掉意图记录。
    fn finish(&mut self, items: &[Item]) -> Result<(), UnitError> {
        let mut changes = Vec::new();
        let mut aux = vec![Aux::Delete(AUX_INTENT.to_vec())];
        for it in items {
            match &it.new {
                Some(p) => {
                    if it.materialize {
                        changes.push(Change::Truncate { id: it.id, len: 0 });
                    }
                    if let Some(m) = p.inspect(&self.root)? {
                        aux.push(Aux::Put(identity_key(&it.id), identity(&m)));
                    }
                }
                None => aux.push(Aux::Delete(identity_key(&it.id))),
            }
        }
        self.image.commit_batch(&Batch { changes: &changes, aux: &aux, ..Batch::default() })
    }
}

impl Unit for DirUnit {
    fn id(&self) -> UnitId {
        self.image.id()
    }

    fn snapshot(&self) -> Box<dyn Snapshot> {
        Box::new(DirSnapshot { inner: self.image.snapshot(), root: self.root.clone() })
    }

    fn apply(&mut self, changes: &[Change]) -> Result<(), UnitError> {
        self.run(changes, None)
    }

    fn prepare(&mut self, txn: &TxnInfo, changes: &[Change]) -> Result<(), UnitError> {
        // 先检查路径，免得提交点之后才发现这个单元做不了。
        self.plan(changes)?;
        self.image.prepare(txn, changes)
    }

    fn prepared(&self) -> Result<Vec<(TxnInfo, Vec<Change>)>, UnitError> {
        self.image.prepared()
    }

    fn commit_prepared(&mut self, txn: TxnId, as_primary: bool) -> Result<(), UnitError> {
        let (_, changes) =
            self.image.prepared()?.into_iter().find(|(t, _)| t.id == txn).ok_or(UnitError::UnknownTxn(txn))?;
        self.run(&changes, Some((txn, as_primary)))
    }

    fn abort_prepared(&mut self, txn: TxnId) -> Result<(), UnitError> {
        self.image.abort_prepared(txn)
    }

    fn committed(&self) -> Result<Vec<TxnInfo>, UnitError> {
        self.image.committed()
    }

    fn forget(&mut self, txn: TxnId) -> Result<(), UnitError> {
        self.image.forget(txn)
    }
}

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

/// 从 `offset` 读，读到文件末尾为止。
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

#[cfg(test)]
mod tests;
