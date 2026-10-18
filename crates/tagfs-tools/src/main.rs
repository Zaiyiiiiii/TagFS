//! `tagfs` 命令行工具：管理员视角操作单个存储单元，不设可见性和权限限制。

mod syntax;

use std::fmt::Display;
use std::io::{Read, Write};
use std::path::Path;
use std::process::ExitCode;

use tagfs::{DirUnit, EntityId, ImageUnit, Query, SystemIds, Tag, Unit, Unrestricted, Volume};

const USAGE: &str = "\
用法：tagfs <命令> <单元> [参数...]

<单元> 是单文件单元（镜像文件或裸盘）或目录单元的目录。

  info <单元>                    单元信息
  check <单元>                   一致性检查（目录单元同时和目录对账）
  find <单元> [条件...]          查找满足条件的实体，列出 ID、长度、Tag
  show <单元> <ID>               实体的 Tag 和长度
  cat <单元> <ID>                实体内容写到标准输出
  put <单元> <文件|-> [Tag...]   新建实体，内容取自文件（- 为标准输入），返回 ID
  tag <单元> <ID> [+Tag|-Tag...] 增删 Tag
  cp <单元> <ID> [Tag...]        复制实体，新实体带给出的 Tag，返回 ID
  rm <单元> <ID>...              删除实体

Tag：ns:key（无值）、ns:key=文本、ns:key=#123（uint）。
条件：ns:key（带这个键）、ns:key=值、ns:key^=前缀、ns:key>=值（还有 > <= <），前加 ! 取反，多个条件取“且”。
单元由各后端自己的工具建立：单文件单元用 tagfs-file，目录单元用 tagfs-hostdir。
在目录单元里，tagfs:hostfs_path=/路径 就是文件在目录里的位置，增删改它就是建、删、移动文件。";

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

enum Opened {
    Image(ImageUnit),
    Dir(DirUnit),
}

fn open(path: &str) -> Result<Opened, String> {
    if Path::new(path).is_dir() {
        let (unit, _) = DirUnit::open(path, Box::new(SystemIds)).map_err(err)?;
        Ok(Opened::Dir(unit))
    } else {
        Ok(Opened::Image(tagfs::image::open(path).map_err(err)?))
    }
}

fn volume(path: &str) -> Result<Volume<Unrestricted>, String> {
    let unit: Box<dyn Unit> = match open(path)? {
        Opened::Image(u) => Box::new(u),
        Opened::Dir(u) => Box::new(u),
    };
    let mut vol = Volume::new(Unrestricted, Box::new(SystemIds));
    vol.attach(unit).map_err(err)?;
    Ok(vol)
}

fn entity_id(s: &str) -> Result<EntityId, String> {
    s.parse().map_err(|_| format!("not an entity id: {s:?}"))
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
    let path = arg(args, 1, "unit")?;
    let rest = &args[2..];
    match cmd {
        "info" => info(path)?,
        "check" => check(path)?,
        "find" => {
            let vol = volume(path)?;
            let query = syntax::query(rest)?;
            let mut cursor = tagfs::Cursor::new(query, 1000);
            let out = std::io::stdout();
            let mut out = out.lock();
            loop {
                let page = cursor.next_page(&vol, &()).map_err(err)?;
                if page.is_empty() {
                    break;
                }
                for id in page {
                    let e = vol.get(&(), &id).map_err(err)?;
                    writeln!(out, "{}", line(&id, e.size, &e.tags)).map_err(io_out)?;
                }
            }
        }
        "show" => {
            let vol = volume(path)?;
            let id = entity_id(arg(args, 2, "entity id")?)?;
            let e = vol.get(&(), &id).map_err(err)?;
            say!("id    {id}");
            say!("size  {}", e.size);
            for t in &e.tags {
                say!("tag   {}", syntax::show_tag(t));
            }
        }
        "cat" => {
            let vol = volume(path)?;
            let id = entity_id(arg(args, 2, "entity id")?)?;
            let out = std::io::stdout();
            let mut out = out.lock();
            let mut buf = vec![0u8; 1 << 20];
            let mut offset = 0u64;
            loop {
                let n = vol.read(&(), &id, offset, &mut buf).map_err(err)?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n]).map_err(io_out)?;
                offset += n as u64;
            }
        }
        "put" => {
            let source = arg(args, 2, "file")?;
            let mut content = Vec::new();
            if source == "-" {
                std::io::stdin().read_to_end(&mut content).map_err(err)?;
            } else {
                content = std::fs::read(source).map_err(err)?;
            }
            let tags = tags(&args[3..])?;
            let mut vol = volume(path)?;
            let mut tx = vol.begin(&());
            let id = tx.create(None, tags, content).map_err(err)?;
            tx.commit().map_err(err)?;
            say!("{id}");
        }
        "tag" => {
            let id = entity_id(arg(args, 2, "entity id")?)?;
            let mut vol = volume(path)?;
            let mut tx = vol.begin(&());
            for op in &args[3..] {
                match op.split_at_checked(1) {
                    Some(("+", t)) => tx.add_tag(&id, syntax::tag(t)?).map_err(err)?,
                    Some(("-", t)) => tx.remove_tag(&id, syntax::tag(t)?).map_err(err)?,
                    _ => return Err(format!("expected +tag or -tag, got {op:?}")),
                }
            }
            tx.commit().map_err(err)?;
        }
        "cp" => {
            let id = entity_id(arg(args, 2, "entity id")?)?;
            let tags = tags(&args[3..])?;
            let mut vol = volume(path)?;
            let mut tx = vol.begin(&());
            let copy = tx.copy(&id, None, tags).map_err(err)?;
            tx.commit().map_err(err)?;
            say!("{copy}");
        }
        "rm" => {
            let mut vol = volume(path)?;
            let mut tx = vol.begin(&());
            for a in rest {
                tx.delete(&entity_id(a)?).map_err(err)?;
            }
            tx.commit().map_err(err)?;
        }
        other => return Err(format!("unknown command {other:?}\n\n{USAGE}")),
    }
    Ok(())
}

fn count(unit: &dyn Unit, query: &Query) -> Result<usize, String> {
    let snap = unit.snapshot();
    let all = snap.all().map_err(err)?;
    let mut n = 0;
    for id in all {
        if let Some(r) = snap.get(&id).map_err(err)?
            && query.matches(&r.tags)
        {
            n += 1;
        }
    }
    Ok(n)
}

fn print_image(image: &ImageUnit) -> Result<(), String> {
    let bs = tagfs::BLOCK_SIZE as u64;
    say!("generation  {}", image.generation());
    say!("size        {} bytes ({} blocks)", image.total_blocks() * bs, image.total_blocks());
    say!("free        {} bytes", image.free_blocks() * bs);
    Ok(())
}

fn info(path: &str) -> Result<(), String> {
    let has_path = Query::has("tagfs", tagfs::hostdir::HOSTFS_PATH);
    match open(path)? {
        Opened::Image(unit) => {
            say!("kind        single-file unit");
            say!("unit        {}", hex(&unit.id().0));
            say!("entities    {}", count(&unit, &Query::All)?);
            print_image(&unit)?;
        }
        Opened::Dir(unit) => {
            say!("kind        directory unit");
            say!("root        {}", unit.root().display());
            say!("unit        {}", hex(&unit.id().0));
            say!("entities    {} ({} files in the directory)", count(&unit, &Query::All)?, count(&unit, &has_path)?);
            say!("live sync   {}", if unit.is_live() { "yes" } else { "no" });
            print_image(&unit.image())?;
        }
    }
    Ok(())
}

fn check(path: &str) -> Result<(), String> {
    match open(path)? {
        Opened::Image(unit) => unit.verify().map_err(err)?,
        Opened::Dir(mut unit) => {
            let report = unit.rescan().map_err(err)?;
            say!(
                "reconciled: {} added, {} removed, {} renamed, {} skipped",
                report.added,
                report.removed,
                report.renamed,
                report.skipped
            );
            unit.image().verify().map_err(err)?;
        }
    }
    say!("ok");
    Ok(())
}
