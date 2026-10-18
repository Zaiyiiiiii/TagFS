//! `tagfs-disk`：裸盘后端自己的工具。格式化之后，单元上的日常操作用通用的 `tagfs` 工具。

use std::process::ExitCode;

const USAGE: &str = "\
用法：tagfs-disk <命令> <设备>

  format <设备> --yes   在整块盘或分区上建一个空单元。盘上原有的全部内容都会丢失，
                        所以必须加 --yes 确认。
  info <设备>           盘上单元的空间情况";

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(cmd), Some(device)) = (args.first().map(String::as_str), args.get(1)) else {
        println!("{USAGE}");
        return ExitCode::from(2);
    };
    let bs = tagfs_backend_disk::BLOCK_SIZE as u64;
    let result = match cmd {
        "format" => {
            if args.get(2).map(String::as_str) != Some("--yes") {
                eprintln!("tagfs-disk: formatting erases everything on {device}; add --yes to confirm");
                return ExitCode::from(2);
            }
            let mut id = [0u8; 16];
            getrandom::fill(&mut id).expect("system random number generator");
            tagfs_backend_disk::format(device, tagfs_core::UnitId(id)).map(|unit| {
                println!("unit {}", hex(&id));
                println!("size {} bytes", unit.total_blocks() * bs);
            })
        }
        "info" => tagfs_backend_disk::open(device).map(|unit| {
            println!("unit        {}", hex(&tagfs_core::Unit::id(&unit).0));
            println!("generation  {}", unit.generation());
            println!("size        {} bytes", unit.total_blocks() * bs);
            println!("free        {} bytes", unit.free_blocks() * bs);
        }),
        _ => {
            println!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tagfs-disk: {e}");
            ExitCode::from(1)
        }
    }
}
