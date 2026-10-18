//! 映射路径：`tagfs:hostfs_path` 的值和宿主路径之间的转换。
//!
//! 值以挂载目录为根，形如 `/abc/bcd/1.file`：以 `/` 开头，用 `/` 分隔，没有空段、`.`、`..`，
//! 不含 NUL。挂载目录之上的路径不会出现在值里。每一段还要是宿主上合法的文件名（Windows 的
//! 限制更多）。满足这些条件的值拼到挂载目录下，结果一定还在挂载目录里。
//!
//! 第一段不能是 [`RESERVED`]：那是目录单元自己的镜像文件和临时文件所在的地方。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use tagfs_core::UnitError;
use tagfs_core::tag::MAX_STR_LEN;

/// 挂载目录下由目录单元自己使用的目录名。
pub const RESERVED: &str = ".tagfs";

/// 映射路径不合法：格式不对、越出挂载目录、经过符号链接、在宿主上不能用作文件名，
/// 或者一个实体带了不止一个路径。
pub const INVALID_PATH: UnitError = UnitError::Rejected("invalid host path");
/// 映射路径在宿主上已经被别的文件占用（或者路径上某一级是文件）。
pub const PATH_TAKEN: UnitError = UnitError::Rejected("host path already taken");

/// 校验过的映射路径，按段存放。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct HostPath(Vec<String>);

impl HostPath {
    pub fn parse(value: &str) -> Result<Self, UnitError> {
        if value.len() > MAX_STR_LEN {
            return Err(INVALID_PATH);
        }
        let rest = value.strip_prefix('/').ok_or(INVALID_PATH)?;
        let parts: Vec<String> = rest.split('/').map(String::from).collect();
        if parts.iter().any(|p| !valid_name(p)) || parts[0] == RESERVED {
            return Err(INVALID_PATH);
        }
        Ok(HostPath(parts))
    }

    /// 由宿主上的各级文件名组成；不合法（含非 UTF-8 的名字）时返回 `None`，这样的文件不映射。
    pub fn from_names(names: &[&std::ffi::OsStr]) -> Option<Self> {
        let mut value = String::new();
        for n in names {
            value.push('/');
            value.push_str(n.to_str()?);
        }
        HostPath::parse(&value).ok()
    }

    /// 各段。
    pub fn segments(&self) -> &[String] {
        &self.0
    }

    pub fn value(&self) -> String {
        let mut s = String::new();
        for p in &self.0 {
            s.push('/');
            s.push_str(p);
        }
        s
    }

    /// 宿主上的完整路径。
    pub fn on_host(&self, root: &Path) -> PathBuf {
        let mut p = root.to_path_buf();
        p.extend(&self.0);
        p
    }

    /// 各级上层目录在宿主上的路径，由浅到深。
    fn parents(&self, root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut p = root.to_path_buf();
        for part in &self.0[..self.0.len() - 1] {
            p.push(part);
            out.push(p.clone());
        }
        out
    }

    /// 检查路径上没有符号链接（含目标本身），防止经由链接越出挂载目录。
    /// 返回目标当前的状态：不存在为 `None`。
    pub fn inspect(&self, root: &Path) -> Result<Option<fs::Metadata>, UnitError> {
        for dir in self.parents(root) {
            match fs::symlink_metadata(&dir) {
                Ok(m) if m.file_type().is_symlink() => return Err(INVALID_PATH),
                Ok(m) if !m.is_dir() => return Err(PATH_TAKEN),
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(_) => return Err(UnitError::Device),
            }
        }
        match fs::symlink_metadata(self.on_host(root)) {
            Ok(m) if m.file_type().is_symlink() => Err(INVALID_PATH),
            Ok(m) => Ok(Some(m)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(UnitError::Device),
        }
    }

    /// 建出缺少的上层目录。已有的每一级都必须是真目录，不能是文件或链接。
    pub fn create_parents(&self, root: &Path) -> Result<(), UnitError> {
        for dir in self.parents(root) {
            match fs::symlink_metadata(&dir) {
                Ok(m) if m.file_type().is_symlink() => return Err(INVALID_PATH),
                Ok(m) if !m.is_dir() => return Err(PATH_TAKEN),
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => match fs::create_dir(&dir) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists && dir.is_dir() => {}
                    Err(_) => return Err(UnitError::Device),
                },
                Err(_) => return Err(UnitError::Device),
            }
        }
        Ok(())
    }
}

/// 一段文件名在任何宿主上都必须满足的条件，加上当前宿主自己的限制。
fn valid_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('\0') && host_allows(name)
}

#[cfg(windows)]
fn host_allows(name: &str) -> bool {
    const RESERVED_NAMES: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1",
        "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if name.chars().any(|c| (c as u32) < 0x20 || "<>:\"\\|?*".contains(c)) || name.ends_with(['.', ' ']) {
        return false;
    }
    let stem = name.split('.').next().unwrap_or("").trim_end();
    !RESERVED_NAMES.iter().any(|r| r.eq_ignore_ascii_case(stem))
}

#[cfg(not(windows))]
fn host_allows(_: &str) -> bool {
    // POSIX 文件名里只禁止 `/` 和 NUL，前面已经排除。
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_rooted_at_the_mount() {
        let p = HostPath::parse("/abc/bcd/1.file").unwrap();
        assert_eq!(p.value(), "/abc/bcd/1.file");
        assert_eq!(p.on_host(Path::new("/mnt/x")), Path::new("/mnt/x/abc/bcd/1.file"));
    }

    #[test]
    fn rejects_anything_that_could_leave_the_mount() {
        for bad in [
            "",
            "abc",
            "/",
            "//abc",
            "/abc/",
            "/abc//d",
            "/..",
            "/a/../../etc/passwd",
            "/./a",
            "/a\0b",
            "/.tagfs/unit.img",
            "/.tagfs",
        ] {
            assert_eq!(HostPath::parse(bad), Err(INVALID_PATH), "{bad:?}");
        }
        let long = format!("/{}", "a".repeat(MAX_STR_LEN));
        assert_eq!(HostPath::parse(&long), Err(INVALID_PATH));
        assert!(HostPath::parse("/x/.tagfs").is_ok(), "only the top-level name is reserved");
    }
}
