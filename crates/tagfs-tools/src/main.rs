//! `tagfs` 命令行工具：连到运行中的卷，面向整个卷工作。
//!
//! 它不区分单元是哪种后端：各后端只在建立单元、把单元加入卷时不同，那由各自的工具负责。
//! 权限属于操作系统层，这里不涉及。

mod syntax;

use std::fmt::Display;
use std::io::Write;
use std::process::ExitCode;

use tagfs_core::{EntityId, Tag, UnitId};
use tagfs_proto::{Client, EntityInfo, Op, Request, Response};

const USAGE: &str = "\
用法：tagfs <命令> [参数...]

连到运行中的卷（tagfs-volume）。卷由若干单元组成，查询和读写面向整个卷。

  units                          卷里的单元：身份、后端、位置、是否在线、单元自己的描述
  check <单元>                   单元的一致性检查
  detach <单元>                  把单元移出卷
  find [条件...]                 查找满足条件的实体，列出 ID、长度、Tag
  show <ID>                      实体的 Tag 和长度
  cat <ID>                       实体内容写到标准输出
  put <文件|-> [--unit <单元>] [Tag...]
                                 新建实体，内容取自文件（- 为标准输入），返回 ID；不指定单元时由卷决定
  tag <ID> [+Tag|-Tag...]        增删 Tag
  cp <ID> [Tag...]               复制实体，新实体带给出的 Tag，返回 ID
  rm <ID>...                     删除实体

Tag：ns:key（无值）、ns:key=文本、ns:key=#123（uint）。
条件：ns:key（带这个键）、ns:key=值、ns:key^=前缀、ns:key>=值（还有 > <= <），前加 ! 取反，多个条件取“且”。
单元由各后端自己的工具建立并加入卷：tagfs-file、tagfs-disk、tagfs-hostdir。";

/// 写一行到标准输出。写不进去时返回错误（见 [`io_out`]），不像 `println!` 那样崩溃。
macro_rules! say {
    ($($t:tt)*) => {
        writeln!(std::io::stdout(), $($t)*).map_err(io_out)?
    };
}

/// 输出出错。对方先关了管道（例如接到 `head`）不算错误：返回空串，`main` 静默结束。
fn io_out(e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::BrokenPipe { String::new() } else { e.to_string() }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if e.is_empty() => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tagfs: {e}");
            ExitCode::from(1)
        }
    }
}

fn err(e: impl Display) -> String {
    e.to_string()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// 和卷的一次请求。卷报告的错误转成本工具的错误。
struct Volume(Client);

impl Volume {
    fn connect() -> Result<Volume, String> {
        Client::connect().map(Volume).map_err(|e| format!("cannot reach the volume (is tagfs-volume running?): {e}"))
    }

    fn call(&mut self, req: Request) -> Result<Response, String> {
        match self.0.call(&req).map_err(err)? {
            Response::Error(e) => Err(e),
            r => Ok(r),
        }
    }

    fn get(&mut self, id: EntityId) -> Result<EntityInfo, String> {
        match self.call(Request::Get { id })? {
            Response::Entity(e) => Ok(e),
            other => Err(format!("unexpected answer {other:?}")),
        }
    }

    fn apply(&mut self, ops: Vec<Op>) -> Result<Vec<Option<EntityId>>, String> {
        match self.call(Request::Apply { ops })? {
            Response::Applied(r) => Ok(r),
            other => Err(format!("unexpected answer {other:?}")),
        }
    }
}

fn entity_id(s: &str) -> Result<EntityId, String> {
    s.parse().map_err(|_| format!("not an entity id: {s:?}"))
}

fn unit_id(s: &str) -> Result<UnitId, String> {
    let bad = || format!("not a unit id: {s:?}");
    if s.len() != 32 {
        return Err(bad());
    }
    let mut b = [0u8; 16];
    for (i, byte) in b.iter_mut().enumerate() {
        *byte = u8::from_str_radix(s.get(2 * i..2 * i + 2).ok_or_else(bad)?, 16).map_err(|_| bad())?;
    }
    Ok(UnitId(b))
}

fn tags(args: &[String]) -> Result<Vec<Tag>, String> {
    args.iter().map(|a| syntax::tag(a)).collect()
}

fn line(id: &EntityId, size: u64, tags: &[Tag]) -> String {
    let tags: Vec<String> = tags.iter().map(syntax::show_tag).collect();
    format!("{id}  {size}  {}", tags.join(" "))
}

fn arg<'a>(args: &'a [String], i: usize, what: &str) -> Result<&'a str, String> {
    args.get(i).map(String::as_str).ok_or_else(|| format!("missing {what}\n\n{USAGE}"))
}

fn run(args: &[String]) -> Result<(), String> {
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    if matches!(cmd, "help" | "-h" | "--help") {
        say!("{USAGE}");
        return Ok(());
    }
    let rest = &args[1..];
    let mut vol = Volume::connect()?;
    match cmd {
        "units" => {
            let Response::Units(units) = vol.call(Request::Units)? else { return Err("unexpected answer".into()) };
            for u in units {
                let id = u.id.map_or_else(|| String::from("-"), |id| hex(&id.0));
                say!("{id}  {}  {}", u.backend, u.location);
                match &u.offline {
                    Some(why) => say!("    offline: {why}"),
                    None => {
                        for (k, v) in &u.details {
                            say!("    {k}: {v}");
                        }
                    }
                }
            }
        }
        "check" => {
            vol.call(Request::Check { unit: unit_id(arg(args, 1, "unit")?)? })?;
            say!("ok");
        }
        "detach" => {
            vol.call(Request::Detach { unit: unit_id(arg(args, 1, "unit")?)? })?;
        }
        "find" => {
            let query = syntax::query(rest)?;
            let mut after = None;
            let out = std::io::stdout();
            let mut out = out.lock();
            loop {
                let Response::Ids(page) = vol.call(Request::Query { query: query.clone(), after, limit: 1000 })? else {
                    return Err("unexpected answer".into());
                };
                if page.is_empty() {
                    break;
                }
                after = page.last().copied();
                for id in page {
                    let e = vol.get(id)?;
                    writeln!(out, "{}", line(&id, e.size, &e.tags)).map_err(io_out)?;
                }
            }
        }
        "show" => {
            let id = entity_id(arg(args, 1, "entity id")?)?;
            let e = vol.get(id)?;
            say!("id    {id}");
            say!("size  {}", e.size);
            for t in &e.tags {
                say!("tag   {}", syntax::show_tag(t));
            }
        }
        "cat" => {
            // 卷一段一段地发，收到一段写一段。
            let id = entity_id(arg(args, 1, "entity id")?)?;
            let out = std::io::stdout();
            vol.0.read(id, 0, u64::MAX, &mut out.lock()).map_err(io_out)??;
        }
        "put" => {
            let source = arg(args, 1, "file")?;
            let mut rest = &args[2..];
            let mut unit = None;
            if rest.first().map(String::as_str) == Some("--unit") {
                unit = Some(unit_id(arg(rest, 1, "unit")?)?);
                rest = &rest[2..];
            }
            let tags = tags(rest)?;
            // 内容边读边发，写进暂存；实体随事务出现，内容随它一起出现。
            let Response::Staged(stage) = vol.call(Request::Stage { unit })? else {
                return Err("unexpected answer".into());
            };
            let sent = if source == "-" {
                vol.0.write_staged(stage, 0, &mut std::io::stdin().lock())
            } else {
                let mut file = std::fs::File::open(source).map_err(|e| format!("{source}: {e}"))?;
                vol.0.write_staged(stage, 0, &mut file)
            };
            if let Response::Error(e) = sent.map_err(err)? {
                return Err(e);
            }
            let ids = vol.apply(vec![Op::CreateFrom { stage, tags }])?;
            say!("{}", ids[0].ok_or("no id returned")?);
        }
        "tag" => {
            let id = entity_id(arg(args, 1, "entity id")?)?;
            let mut ops = Vec::new();
            for op in &args[2..] {
                ops.push(match op.split_at_checked(1) {
                    Some(("+", t)) => Op::AddTag { id, tag: syntax::tag(t)? },
                    Some(("-", t)) => Op::RemoveTag { id, tag: syntax::tag(t)? },
                    _ => return Err(format!("expected +tag or -tag, got {op:?}")),
                });
            }
            vol.apply(ops)?;
        }
        "cp" => {
            let id = entity_id(arg(args, 1, "entity id")?)?;
            let ids = vol.apply(vec![Op::Copy { from: id, unit: None, tags: tags(&args[2..])? }])?;
            say!("{}", ids[0].ok_or("no id returned")?);
        }
        "rm" => {
            let ops = rest.iter().map(|a| entity_id(a).map(|id| Op::Delete { id })).collect::<Result<_, _>>()?;
            vol.apply(ops)?;
        }
        other => return Err(format!("unknown command {other:?}\n\n{USAGE}")),
    }
    Ok(())
}
