//! `tagfs-volume`：运行中的卷。启动后常驻，各工具经本机套接字和它通信。
//!
//! 用法：tagfs-volume [--state <目录>]
//!
//! 状态目录默认取环境变量 `TAGFS_STATE`，没有时按系统惯例（`$XDG_STATE_HOME/tagfs`、`~/.local/state/tagfs`、
//! `%LOCALAPPDATA%\tagfs`）。套接字名取环境变量 `TAGFS_VOLUME`，默认 `tagfs-volume.sock`。

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let state = match args.as_slice() {
        [] => tagfs_volume::default_state_dir(),
        [flag, dir] if flag == "--state" => dir.into(),
        _ => {
            eprintln!("usage: tagfs-volume [--state <dir>]");
            return ExitCode::from(2);
        }
    };
    let run = || -> std::io::Result<()> {
        let mut service = tagfs_volume::Service::start(&state)?;
        let listener = tagfs_volume::listen(tagfs_proto::socket_name()?)?;
        eprintln!("tagfs-volume: serving, state in {}", state.display());
        service.serve(&listener)
    };
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tagfs-volume: {e}");
            ExitCode::from(1)
        }
    }
}
