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
        /// 保存完整副本的设备，可以写多次；不写时用网络里开了 serve 的设备
        #[arg(long = "replica", value_name = "设备")]
        replicas: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// 本机作为存储设备：保存登记到这里的工作区的完整副本（锚点通常开着）
    Serve {
        /// on、off 或 status
        #[arg(value_parser = ["on", "off", "status"])]
        state: String,
        #[arg(long)]
        json: bool,
    },
    /// 和存储设备同步：取回新的提交和目录结构（文件内容用到时再取），推送本机的提交，或报告分叉
    ///
    /// 例：nexus drive sync work（新设备上第一次打开工作区也用它）
    Sync {
        ws: String,
        #[arg(long)]
        json: bool,
    },
    /// 分叉时把存储设备上的提交合并进来：两边都改了的文件保留本机的版本，对方的另存为 <文件>.conflict-<设备>
    Merge {
        ws: String,
        #[arg(long)]
        json: bool,
    },
    /// 数据在哪里：副本、是否已同步、未提交的改动、本机有多少内容
    ///
    /// 例：nexus drive where work:/data
    Where {
        path: String,
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
    /// 把工作区（或其中一个目录）挂到一个空目录上（Linux）；在前台运行，fusermount -u 或 Ctrl-C 卸载
    ///
    /// 挂载里的写入只落本机，用 nexus drive commit 提交。
    /// 例：nexus drive mount work:/ ~/work
    Mount {
        path: String,
        /// 挂载点：已经存在的空目录
        dir: String,
        /// 挂载期间记录读了哪些文件，卸载时保存成这个名字的预读清单
        #[arg(long, value_name = "名字")]
        record: Option<String>,
    },
    /// 记录这台设备读了工作区的哪些文件，得到预读清单（交给存储设备，别的设备可以按它预读）
    ///
    /// 例：nexus drive record work --name build；构建；nexus drive record work --stop
    Record {
        ws: String,
        /// 清单的名字
        #[arg(long, default_value = "default", conflicts_with = "stop")]
        name: String,
        /// 结束记录，保存清单
        #[arg(long)]
        stop: bool,
        #[arg(long)]
        json: bool,
    },
    /// 把文件内容提前取到本机：按预读清单，或者 --all 取整个工作区
    Prefetch {
        ws: String,
        /// 预读清单的名字（默认 default）
        #[arg(long, value_name = "名字", conflicts_with = "all")]
        profile: Option<String>,
        /// 取整个工作区（之后可以离线用）
        #[arg(long)]
        all: bool,
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
        DriveCmd::Mount { .. } => return fail(sink, crate::EXIT_USAGE, "usage", "挂载只能在真实设备上运行（Linux）"),
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
        DriveCmd::Create { ws, replicas, json } => {
            (ask(ctl, DriveRequest::Create { ws: ws.clone(), replicas: replicas.clone() }).await, *json)
        }
        DriveCmd::Serve { state, json } => {
            let on = match state.as_str() {
                "on" => Some(true),
                "off" => Some(false),
                _ => None,
            };
            (ask(ctl, DriveRequest::Serve { on }).await, *json)
        }
        DriveCmd::Sync { ws, json } => (ask(ctl, DriveRequest::Sync { ws: ws.clone() }).await, *json),
        DriveCmd::Merge { ws, json } => (ask(ctl, DriveRequest::Merge { ws: ws.clone() }).await, *json),
        DriveCmd::Where { path, json } => (ask(ctl, DriveRequest::Where { path: path.clone() }).await, *json),
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
        DriveCmd::Record { ws, name, stop, json } => {
            let name = (!stop).then(|| name.clone());
            (ask(ctl, DriveRequest::Record { ws: ws.clone(), name }).await, *json)
        }
        DriveCmd::Prefetch { ws, profile, all, json } => {
            (ask(ctl, DriveRequest::Prefetch { ws: ws.clone(), profile: profile.clone(), all: *all }).await, *json)
        }
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
        // 只有挂载用这两个请求。
        DriveResponse::Done { path } => {
            if !json {
                return format!("{path}（还没提交）\n");
            }
            json!({ "path": path })
        }
        DriveResponse::Stamp { tree } => json!({ "tree": nexus_drive_core::Hash(*tree).to_string() }),
        DriveResponse::Recording { name } => {
            if !json {
                return format!("开始记录预读清单 {name}；读完后 nexus drive record <工作区> --stop\n");
            }
            json!({ "recording": name })
        }
        DriveResponse::Profile { name, files, saved_to } => {
            if !json {
                let place = if saved_to.is_empty() { "只在本机（没有交给存储设备）".to_string() } else { format!("已交给 {}", saved_to.join("、")) };
                return format!("预读清单 {name}：{} 个文件，{place}\n", files.len());
            }
            json!({ "profile": name, "files": files, "saved_to": saved_to })
        }
        DriveResponse::Prefetched(p) => {
            if !json {
                return format!(
                    "{} 个文件，{} 字节；这次取到 {} 字节，现在都在本机\n",
                    p.files, p.total_bytes, p.fetched_bytes
                );
            }
            json!({ "files": p.files, "fetched_bytes": p.fetched_bytes, "total_bytes": p.total_bytes, "local_bytes": p.local_bytes })
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
        DriveResponse::Committed { commit, sync } => {
            if !json {
                return format!("已提交 {}；{}", &commit[..12], sync_text(sync));
            }
            json!({ "commit": commit, "sync": sync_json(sync) })
        }
        DriveResponse::Serving { on } => {
            if !json {
                return if *on { "本机是存储设备：保存登记到这里的工作区的完整副本\n".into() } else { "本机不是存储设备\n".into() };
            }
            json!({ "serving": on })
        }
        DriveResponse::Synced(r) => {
            if !json {
                return sync_text(r);
            }
            sync_json(r)
        }
        DriveResponse::Merged { commit, conflicts, sync } => {
            if !json {
                let mut t = format!("已合并 {}", &commit[..12]);
                if !conflicts.is_empty() {
                    t.push_str(&format!("；冲突（本机的版本保留，对方的另存为 .conflict-*）：{}", conflicts.join("、")));
                }
                return format!("{t}；{}", sync_text(sync));
            }
            json!({ "commit": commit, "conflicts": conflicts, "sync": sync_json(sync) })
        }
        DriveResponse::Where(w) => {
            if !json {
                let place = if w.replicas.is_empty() {
                    "只在本机（没有存储设备）".to_string()
                } else if w.synced {
                    format!("已同步到 {}", w.replicas.join("、"))
                } else {
                    format!("还没同步到 {}：最新的提交只在本机", w.replicas.join("、"))
                };
                let pct = (w.local_bytes * 100).checked_div(w.total_bytes).unwrap_or(100);
                return format!(
                    "{}\n  {place}\n  未提交的改动 {}\n  内容 {} 字节，本机有 {} 字节（{pct}%）\n",
                    w.path, w.uncommitted, w.total_bytes, w.local_bytes
                );
            }
            json!({
                "path": w.path, "replicas": w.replicas, "synced": w.synced, "uncommitted": w.uncommitted,
                "total_bytes": w.total_bytes, "local_bytes": w.local_bytes,
            })
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

fn sync_json(r: &nexus_wire::DriveSyncReport) -> Value {
    json!({ "state": r.state, "head": r.head, "remote": r.remote, "message": r.message })
}

fn sync_text(r: &nexus_wire::DriveSyncReport) -> String {
    let what = match r.state.as_str() {
        "local" => "只在本机（没有存储设备）",
        "same" => "和存储设备一致",
        "pulled" => "已取回存储设备上的提交",
        "pushed" => "已推送到存储设备",
        "diverged" => "分叉",
        _ => "存储设备连不上",
    };
    if r.message.is_empty() || r.state == "local" {
        format!("{what}\n")
    } else {
        format!("{what}：{}\n", r.message)
    }
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
            let exec = {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    md.permissions().mode() & 0o111 != 0
                }
                #[cfg(windows)]
                {
                    false
                }
            };
            let f = std::fs::File::open(&local)
                .map_err(|e| Failure::Other(EXIT_INVALID_INPUT, "invalid_input", format!("{}: {e}", local.display())))?;
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
