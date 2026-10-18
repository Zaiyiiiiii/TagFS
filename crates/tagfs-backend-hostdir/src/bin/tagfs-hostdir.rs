//! `tagfs-hostdir`：宿主目录后端自己的工具，只管在宿主目录上建立目录单元和把它加入运行中的卷。
//! 之后的使用走通用的 `tagfs` 工具。

use std::process::ExitCode;

use tagfs_backend_hostdir::DirUnit;
use tagfs_core::{IdSource, UnitId};
use tagfs_proto::{Client, Request, Response};

const USAGE: &str = "\
用法：tagfs-hostdir <命令> <目录>

  init <目录>   在已有的宿主目录上建目录单元，把里面的文件映射成实体
  add <目录>    把目录单元加入运行中的卷";

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

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(cmd), Some(dir)) = (args.first().map(String::as_str), args.get(1)) else {
        println!("{USAGE}");
        return ExitCode::from(2);
    };
    let result = match cmd {
        "init" => {
            let mut id = [0u8; 16];
            SystemIds.fill_random(&mut id);
            DirUnit::create(dir, UnitId(id), Box::new(SystemIds))
                .map(|(_, report)| {
                    println!("unit {}", hex(&id));
                    println!("mapped {} files, skipped {}", report.added, report.skipped);
                })
                .map_err(|e| e.to_string())
        }
        "add" => Client::connect()
            .and_then(|mut c| c.call(&Request::Attach { backend: "hostdir".into(), location: dir.clone() }))
            .map_err(|e| format!("cannot reach the volume: {e}"))
            .and_then(|r| match r {
                Response::Unit(id) => {
                    println!("unit {} is online", hex(&id.0));
                    Ok(())
                }
                Response::Error(e) => Err(e),
                other => Err(format!("unexpected answer {other:?}")),
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
