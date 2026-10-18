//! `tagfs-file`：单文件后端自己的工具。建好之后，单元上的日常操作用通用的 `tagfs` 工具。

use std::process::ExitCode;

const USAGE: &str = "\
用法：tagfs-file init <文件>

  init <文件>   新建镜像文件（已有的会被覆盖），里面是一个空单元";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some("init"), Some(path)) = (args.first().map(String::as_str), args.get(1)) else {
        println!("{USAGE}");
        return ExitCode::from(2);
    };
    let mut id = [0u8; 16];
    getrandom::fill(&mut id).expect("system random number generator");
    match tagfs_backend_file::create(path, tagfs_core::UnitId(id)) {
        Ok(_) => {
            println!("unit {}", id.iter().map(|b| format!("{b:02x}")).collect::<String>());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("tagfs-file: {e}");
            ExitCode::from(1)
        }
    }
}
