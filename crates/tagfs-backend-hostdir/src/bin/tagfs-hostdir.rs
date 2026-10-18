//! `tagfs-hostdir`：宿主目录后端自己的工具。建好之后，单元上的日常操作用通用的 `tagfs` 工具。

use std::process::ExitCode;

use tagfs_backend_hostdir::DirUnit;
use tagfs_core::{IdSource, UnitId};

const USAGE: &str = "\
用法：tagfs-hostdir <命令> <目录>

  init <目录>   在已有的宿主目录上建目录单元，把里面的文件映射成实体
  scan <目录>   和宿主目录全量对账，报告新增、删除、改名的文件
  info <目录>   映射了多少文件、是否实时同步、镜像文件的空间情况";

/// 系统时钟加系统随机数。
struct SystemIds;

impl IdSource for SystemIds {
    fn unix_millis(&mut self) -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
    }

    fn fill_random(&mut self, buf: &mut [u8]) {
        getrandom::fill(buf).expect("system random number generator");
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(cmd), Some(dir)) = (args.first(), args.get(1)) else {
        println!("{USAGE}");
        return ExitCode::from(2);
    };
    let result = match cmd.as_str() {
        "init" => {
            let mut id = [0u8; 16];
            SystemIds.fill_random(&mut id);
            DirUnit::create(dir, UnitId(id), Box::new(SystemIds)).map(|(_, report)| {
                println!("unit {}", id.iter().map(|b| format!("{b:02x}")).collect::<String>());
                println!("mapped {} files, skipped {}", report.added, report.skipped);
            })
        }
        "scan" => DirUnit::open(dir, Box::new(SystemIds)).map(|(_, report)| {
            println!(
                "{} added, {} removed, {} renamed, {} skipped",
                report.added, report.removed, report.renamed, report.skipped
            );
        }),
        "info" => DirUnit::open(dir, Box::new(SystemIds)).map(|(unit, _)| {
            use tagfs_core::{Key, Namespace, Probe, Unit};
            let (ns, key) = (Namespace::tagfs(), Key::new(tagfs_backend_hostdir::HOSTFS_PATH));
            let snap = unit.snapshot();
            let all = snap.all().map(|a| a.len()).unwrap_or(0);
            let files = snap.probe(&Probe::Has { ns: &ns, key: &key }).ok().flatten().map_or(0, |f| f.len());
            let image = unit.image();
            println!("root        {}", unit.root().display());
            println!("unit        {}", unit.id().0.iter().map(|b| format!("{b:02x}")).collect::<String>());
            println!("entities    {all} ({files} files in the directory, {} in the image)", all - files);
            println!("live sync   {}", if unit.is_live() { "yes" } else { "no" });
            println!("image       {} bytes, {} free", image.total_blocks() * 4096, image.free_blocks() * 4096);
        }),
        _ => {
            println!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tagfs-hostdir: {e}");
            ExitCode::from(1)
        }
    }
}
