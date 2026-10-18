//! `tagfs-file`：单文件后端自己的工具。建好之后，单元上的日常操作用通用的 `tagfs` 工具。

use std::process::ExitCode;

const USAGE: &str = "\
用法：tagfs-file <命令> <文件>

  init <文件>   新建镜像文件（已有的会被覆盖），里面是一个空单元
  info <文件>   镜像文件的空间情况";

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(cmd), Some(path)) = (args.first().map(String::as_str), args.get(1)) else {
        println!("{USAGE}");
        return ExitCode::from(2);
    };
    let result = match cmd {
        "init" => {
            let mut id = [0u8; 16];
            getrandom::fill(&mut id).expect("system random number generator");
            tagfs_backend_file::create(path, tagfs_core::UnitId(id)).map(|_| println!("unit {}", hex(&id)))
        }
        "info" => tagfs_backend_file::open(path).map(|unit| {
            let bs = tagfs_backend_file::BLOCK_SIZE as u64;
            println!("unit        {}", hex(&tagfs_core::Unit::id(&unit).0));
            println!("generation  {}", unit.generation());
            println!("size        {} bytes (grows on demand)", unit.total_blocks() * bs);
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
            eprintln!("tagfs-file: {e}");
            ExitCode::from(1)
        }
    }
}
