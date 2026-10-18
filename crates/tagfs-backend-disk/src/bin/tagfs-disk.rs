//! `tagfs-disk`：裸盘后端自己的工具。格式化之后，单元上的日常操作用通用的 `tagfs` 工具。

use std::process::ExitCode;

const USAGE: &str = "\
用法：tagfs-disk format <设备> --yes

  format <设备> --yes   在整块盘或分区上建一个空单元。盘上原有的全部内容都会丢失，
                        所以必须加 --yes 确认。";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some("format"), Some(device)) = (args.first().map(String::as_str), args.get(1)) else {
        println!("{USAGE}");
        return ExitCode::from(2);
    };
    if args.get(2).map(String::as_str) != Some("--yes") {
        eprintln!("tagfs-disk: formatting erases everything on {device}; add --yes to confirm");
        return ExitCode::from(2);
    }
    let mut id = [0u8; 16];
    getrandom::fill(&mut id).expect("system random number generator");
    match tagfs_backend_disk::format(device, tagfs_core::UnitId(id)) {
        Ok(unit) => {
            println!("unit {}", id.iter().map(|b| format!("{b:02x}")).collect::<String>());
            println!("size {} bytes", unit.total_blocks() * tagfs_backend_disk::BLOCK_SIZE as u64);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("tagfs-disk: {e}");
            ExitCode::from(1)
        }
    }
}
