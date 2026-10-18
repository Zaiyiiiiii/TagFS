//! `tagfs-file`：单文件后端自己的工具，只管建立镜像文件和把它加入运行中的卷。之后的使用走通用的 `tagfs` 工具。

use std::process::ExitCode;

use tagfs_proto::{Client, Request, Response};

const USAGE: &str = "\
用法：tagfs-file <命令> <文件>

  init <文件>   新建镜像文件（已有的会被覆盖），里面是一个空单元
  add <文件>    把镜像文件里的单元加入运行中的卷";

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
            tagfs_backend_file::create(path, tagfs_core::UnitId(id))
                .map(|_| println!("unit {}", hex(&id)))
                .map_err(|e| e.to_string())
        }
        "add" => Client::connect()
            .and_then(|mut c| c.call(&Request::Attach { backend: "file".into(), location: path.clone() }))
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
            eprintln!("tagfs-file: {e}");
            ExitCode::from(1)
        }
    }
}
