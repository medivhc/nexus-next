//! `nexus drive`：工作区（M4.1）。路径写作 `<工作区>:/<路径>`。

use crate::{doc, fail, fail_wire, Ctl, Sink, EXIT_DAEMON_UNREACHABLE, EXIT_INTERNAL, EXIT_INVALID_INPUT, JSON_SCHEMA};
use clap::Subcommand;
use nexus_wire::{CtlRequest, CtlResponse, DriveRequest, DriveResponse, Input};
use serde_json::{json, Value};
use std::path::Path;
use tokio::io::{AsyncRead, AsyncReadExt};

#[derive(Subcommand, Debug)]
pub enum DriveCmd {
    /// 创建一个空的工作区
    ///
    /// 例：nexus drive create work
    Create {
        /// 工作区名：小写字母、数字、连字符
        ws: String,
        #[arg(long)]
        json: bool,
    },
    /// 列出工作区，或者工作区里的一个目录
    ///
    /// 例：nexus drive ls；nexus drive ls work:/src
    Ls {
        /// <工作区>:/<路径>；不写时列出工作区
        path: Option<String>,
        /// 看某次提交（提交号或至少 8 位的前缀），默认看还没提交的工作树
        #[arg(long, value_name = "提交号")]
        at: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 输出文件内容（可以只读一段）
    ///
    /// 例：nexus drive cat work:/src/main.rs；nexus drive cat work:/big.bin --offset 1048576 --length 4096
    Cat {
        path: String,
        #[arg(long, value_name = "提交号")]
        at: Option<String>,
        /// 从第几个字节开始
        #[arg(long, default_value_t = 0)]
        offset: u64,
        /// 读多少字节，默认读到末尾
        #[arg(long)]
        length: Option<u64>,
    },
    /// 把标准输入写成工作区里的一个文件（新建或覆盖，中间的目录自动建立）
    ///
    /// 例：echo hi | nexus drive put work:/notes/hi.txt
    Put {
        path: String,
        /// 设可执行位
        #[arg(long)]
        exec: bool,
        #[arg(long)]
        json: bool,
    },
    /// 把本地目录导入工作区（文件、子目录、符号链接，保留可执行位）
    ///
    /// 例：nexus drive import ~/src/nexus-next work:/
    Import {
        /// 本地目录
        dir: String,
        /// 放到工作区的哪里，例如 work:/ 或 work:/vendor/x
        dest: String,
        #[arg(long)]
        json: bool,
    },
    /// 删除文件（目录要加 -r）
    Rm {
        path: String,
        #[arg(short = 'r', long)]
        recursive: bool,
        #[arg(long)]
        json: bool,
    },
    /// 列出还没提交的改动
    Status {
        ws: String,
        #[arg(long)]
        json: bool,
    },
    /// 把还没提交的改动生成一次提交
    ///
    /// 例：nexus drive commit work -m "整理目录"
    Commit {
        ws: String,
        #[arg(short = 'm', long, default_value = "")]
        message: String,
        #[arg(long)]
        json: bool,
    },
    /// 提交历史，新的在前
    Log {
        ws: String,
        #[arg(long)]
        json: bool,
    },
}

/// 一次请求、一次回应。
async fn ask<C: Ctl>(ctl: &C, req: DriveRequest) -> Result<DriveResponse, Failure> {
    let mut s = ctl.open(CtlRequest::Drive(req)).await.map_err(unreachable)?;
    match s.rx.recv().await {
        Some(CtlResponse::Drive(r)) => Ok(r),
        Some(CtlResponse::Error(e)) => Err(Failure::Wire(e)),
        _ => Err(Failure::Other(EXIT_INTERNAL, "internal", "常驻程序的回应无法理解".into())),
    }
}

enum Failure {
    Wire(nexus_wire::WireError),
    Other(i32, &'static str, String),
}

fn unreachable(e: std::io::Error) -> Failure {
    Failure::Other(EXIT_DAEMON_UNREACHABLE, "daemon_unreachable", format!("连不上本机的常驻程序（{e}）"))
}

impl Failure {
    fn report<K: Sink>(self, sink: &mut K) -> i32 {
        match self {
            Failure::Wire(e) => fail_wire(sink, &e),
            Failure::Other(code, kind, msg) => fail(sink, code, kind, &msg),
        }
    }
}

/// 写一个文件的内容从哪里来。本地文件同步读：导入时逐个文件读，不需要后台线程（仿真里也是确定的）。
enum Source<'a, R> {
    Stream(&'a mut R),
    File(std::fs::File),
}

impl<R: AsyncRead + Unpin> Source<'_, R> {
    async fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Source::Stream(r) => r.read(buf).await,
            Source::File(f) => std::io::Read::read(f, buf),
        }
    }
}

/// 写一个文件：内容来自 `input`。
async fn put<C: Ctl, R: AsyncRead + Unpin>(ctl: &C, path: &str, exec: bool, mut input: Source<'_, R>) -> Result<DriveResponse, Failure> {
    let mut s = ctl.open(CtlRequest::Drive(DriveRequest::Put { path: path.to_string(), exec })).await.map_err(unreachable)?;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = input.read(&mut buf).await.map_err(|e| Failure::Other(EXIT_INVALID_INPUT, "invalid_input", format!("读不了输入：{e}")))?;
        let msg = if n == 0 { Input::CloseStdin } else { Input::Stdin(buf[..n].to_vec()) };
        if s.tx.send(msg).await.is_err() || n == 0 {
            break;
        }
    }
    match s.rx.recv().await {
        Some(CtlResponse::Drive(r)) => Ok(r),
        Some(CtlResponse::Error(e)) => Err(Failure::Wire(e)),
        _ => Err(Failure::Other(EXIT_INTERNAL, "internal", "常驻程序的回应无法理解".into())),
    }
}

pub(crate) async fn run<C: Ctl, R: AsyncRead + Unpin, K: Sink>(cmd: &DriveCmd, ctl: &C, stdin: &mut R, sink: &mut K) -> i32 {
    let (result, json) = match cmd {
        DriveCmd::Cat { path, at, offset, length } => return cat(ctl, path, at.clone(), *offset, *length, sink).await,
        DriveCmd::Import { dir, dest, json } => match import(ctl, dir, dest).await {
            Ok((files, bytes)) => {
                let text = if *json {
                    doc(json!({ "schema": JSON_SCHEMA, "files": files, "bytes": bytes }))
                } else {
                    format!("导入了 {files} 个文件，{bytes} 字节（还没提交：nexus drive commit）\n")
                };
                sink.out(text.as_bytes());
                return 0;
            }
            Err(f) => return f.report(sink),
        },
        DriveCmd::Put { path, exec, json } => (put(ctl, path, *exec, Source::Stream(stdin)).await, *json),
        DriveCmd::Create { ws, json } => (ask(ctl, DriveRequest::Create { ws: ws.clone() }).await, *json),
        DriveCmd::Ls { path: None, json, .. } => (ask(ctl, DriveRequest::List).await, *json),
        DriveCmd::Ls { path: Some(path), at, json } => {
            (ask(ctl, DriveRequest::Ls { path: path.clone(), at: at.clone() }).await, *json)
        }
        DriveCmd::Rm { path, recursive, json } => {
            (ask(ctl, DriveRequest::Rm { path: path.clone(), recursive: *recursive }).await, *json)
        }
        DriveCmd::Status { ws, json } => (ask(ctl, DriveRequest::Status { ws: ws.clone() }).await, *json),
        DriveCmd::Commit { ws, message, json } => {
            (ask(ctl, DriveRequest::Commit { ws: ws.clone(), message: message.clone() }).await, *json)
        }
        DriveCmd::Log { ws, json } => (ask(ctl, DriveRequest::Log { ws: ws.clone() }).await, *json),
    };
    match result {
        Ok(r) => {
            sink.out(render(&r, json).as_bytes());
            0
        }
        Err(f) => f.report(sink),
    }
}

fn render(r: &DriveResponse, json: bool) -> String {
    let v: Value = match r {
        DriveResponse::Created { ws } => {
            if !json {
                return format!("已创建工作区 {ws}\n");
            }
            json!({ "workspace": ws })
        }
        DriveResponse::Workspaces(list) => {
            if !json {
                if list.is_empty() {
                    return "没有工作区（nexus drive create <名字>）\n".into();
                }
                return list
                    .iter()
                    .map(|w| {
                        let head = w.head.as_deref().map_or("（没有提交）".to_string(), |h| h[..12].to_string());
                        format!("{:<20} {:<14} 未提交的改动 {}\n", w.name, head, w.changes)
                    })
                    .collect();
            }
            json!({ "workspaces": list.iter().map(|w| json!({ "name": w.name, "head": w.head, "changes": w.changes })).collect::<Vec<_>>() })
        }
        DriveResponse::Entries { path, entries } => {
            if !json {
                return entries
                    .iter()
                    .map(|e| match e.kind.as_str() {
                        "dir" => format!("{}/\n", e.name),
                        "symlink" => format!("{} -> {}\n", e.name, e.target.as_deref().unwrap_or_default()),
                        _ => format!("{:<40} {:>12}{}\n", e.name, e.size, if e.exec { "  可执行" } else { "" }),
                    })
                    .collect();
            }
            let entries: Vec<Value> = entries
                .iter()
                .map(|e| {
                    let mut v = json!({ "name": e.name, "kind": e.kind, "size": e.size, "exec": e.exec });
                    if let Some(t) = &e.target {
                        v["target"] = json!(t);
                    }
                    v
                })
                .collect();
            json!({ "path": path, "entries": entries })
        }
        DriveResponse::Written { path, size } => {
            if !json {
                return format!("已写入 {path}（{size} 字节，还没提交）\n");
            }
            json!({ "path": path, "size": size })
        }
        DriveResponse::Removed { path } => {
            if !json {
                return format!("已删除 {path}（还没提交）\n");
            }
            json!({ "removed": path })
        }
        DriveResponse::Changes(list) => {
            if !json {
                if list.is_empty() {
                    return "没有未提交的改动\n".into();
                }
                let mark = |k: &str| match k {
                    "added" => "新增",
                    "modified" => "修改",
                    _ => "删除",
                };
                return list.iter().map(|c| format!("{}  {}\n", mark(&c.kind), c.path)).collect();
            }
            json!({ "changes": list.iter().map(|c| json!({ "kind": c.kind, "path": c.path })).collect::<Vec<_>>() })
        }
        DriveResponse::Committed { commit } => {
            if !json {
                return format!("已提交 {}\n", &commit[..12]);
            }
            json!({ "commit": commit })
        }
        DriveResponse::Log(list) => {
            if !json {
                return list.iter().map(|c| format!("{}  {:<12} {}\n", &c.commit[..12], c.device, c.message)).collect();
            }
            json!({ "commits": list.iter().map(|c| json!({ "commit": c.commit, "message": c.message, "device": c.device, "time_ms": c.time_ms })).collect::<Vec<_>>() })
        }
    };
    let mut v = v;
    v["schema"] = json!(JSON_SCHEMA);
    doc(v)
}

async fn cat<C: Ctl, K: Sink>(ctl: &C, path: &str, at: Option<String>, offset: u64, length: Option<u64>, sink: &mut K) -> i32 {
    let req = DriveRequest::Cat { path: path.to_string(), at, offset, length };
    let mut s = match ctl.open(CtlRequest::Drive(req)).await {
        Ok(s) => s,
        Err(e) => return unreachable(e).report(sink),
    };
    loop {
        match s.rx.recv().await {
            Some(CtlResponse::Ok) => {}
            Some(CtlResponse::PipeData(b)) => sink.out(&b),
            Some(CtlResponse::PipeEnd) => return 0,
            Some(CtlResponse::Error(e)) => return fail_wire(sink, &e),
            _ => return fail(sink, EXIT_INTERNAL, "internal", "常驻程序的回应无法理解"),
        }
    }
}

/// 把本地目录导入：先确认工作区存在，再逐个写入文件和符号链接。返回（文件数，字节数）。
async fn import<C: Ctl>(ctl: &C, dir: &str, dest: &str) -> Result<(u64, u64), Failure> {
    let base: nexus_drive_core::path::DrivePath =
        dest.parse().map_err(|m: String| Failure::Other(EXIT_INVALID_INPUT, "invalid_input", m))?;
    ask(ctl, DriveRequest::Ls { path: format!("{}:/", base.ws), at: None }).await?;
    let root = Path::new(dir);
    if !root.is_dir() {
        return Err(Failure::Other(EXIT_INVALID_INPUT, "invalid_input", format!("不是本地目录：{dir}")));
    }
    let mut files = Vec::new();
    walk(root, root, &mut files).map_err(|e| Failure::Other(EXIT_INVALID_INPUT, "invalid_input", format!("读不了 {dir}：{e}")))?;
    let (mut count, mut bytes) = (0u64, 0u64);
    for rel in files {
        let local = root.join(&rel);
        let mut target = base.clone();
        target.parts.extend(rel.iter().map(|c| c.to_string_lossy().into_owned()));
        let path = target.to_string();
        let md = std::fs::symlink_metadata(&local).map_err(|e| Failure::Other(EXIT_INVALID_INPUT, "invalid_input", format!("{}：{e}", local.display())))?;
        if md.file_type().is_symlink() {
            let to = std::fs::read_link(&local).map_err(|e| Failure::Other(EXIT_INVALID_INPUT, "invalid_input", e.to_string()))?;
            ask(ctl, DriveRequest::Symlink { path, target: to.to_string_lossy().into_owned() }).await?;
        } else {
            use std::os::unix::fs::PermissionsExt;
            let exec = md.permissions().mode() & 0o111 != 0;
            let f = std::fs::File::open(&local)
                .map_err(|e| Failure::Other(EXIT_INVALID_INPUT, "invalid_input", format!("{}：{e}", local.display())))?;
            put::<C, tokio::io::Empty>(ctl, &path, exec, Source::File(f)).await?;
            bytes += md.len();
        }
        count += 1;
    }
    Ok((count, bytes))
}

/// 目录里的文件和符号链接（相对路径），按名字排序；不跟随符号链接。
fn walk(root: &Path, dir: &Path, out: &mut Vec<std::path::PathBuf>) -> std::io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let ft = e.file_type()?;
        let p = e.path();
        if ft.is_dir() {
            walk(root, &p, out)?;
        } else if ft.is_file() || ft.is_symlink() {
            out.push(p.strip_prefix(root).expect("在根下面").to_path_buf());
        }
    }
    Ok(())
}
