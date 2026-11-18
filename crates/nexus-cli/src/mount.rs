//! `nexus drive mount`（M4.3）：把工作区挂成一个目录（Linux FUSE）。
//!
//! 挂载进程是本机常驻程序的普通客户端：读目录和文件用 `drive ls` / `drive cat` 同样的请求，
//! 写入先放在本地的临时文件里（覆盖层），文件关闭时写进工作树。不提交就不会复制到任何设备。
//!
//! 和常驻程序说话的是一个单独的线程（自己的运行时）：内核的回调在 FUSE 线程上同步等待回应，
//! 主线程卸载时也在等，两者都不能负责驱动网络。
//!
//! 缓存：属性和目录项让内核缓存 [`TTL`]；每次打开文件都丢弃页缓存；工作树的根变了（同步进来、
//! 或者别的程序改了工作树）就丢掉目录缓存。所以同步进来的修改最多 [`TTL`] 加 [`STAMP_EVERY`] 后可见。

use crate::UnixCtl;
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyWrite, Request, TimeOrNow,
};
use nexus_cli::{fail, fail_wire, Ctl, Sink, EXIT_INTERNAL, EXIT_INVALID_INPUT};
use nexus_drive_core::path::DrivePath;
use nexus_wire::{CtlRequest, CtlResponse, DriveEntryInfo, DriveRequest, DriveResponse, ErrorCode, Input, WireError};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc as smpsc;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::mpsc;

const TTL: Duration = Duration::from_secs(1);
const STAMP_EVERY: Duration = Duration::from_millis(500);
const ROOT: u64 = 1;

/// 挂载并在前台运行，直到被卸载或收到 SIGINT / SIGTERM。
pub async fn run<K: Sink>(path: &str, dir: &str, record: Option<&str>, data_dir: &Path, ctl: UnixCtl, sink: &mut K) -> i32 {
    let base: DrivePath = match path.parse() {
        Ok(p) => p,
        Err(m) => return fail(sink, EXIT_INVALID_INPUT, "invalid_input", &m),
    };
    // 先检查：路径是工作区里的一个目录，挂载点是空目录。
    if let Err(e) = check_dir(&ctl, &base).await {
        return fail_wire(sink, &e);
    }
    match std::fs::read_dir(dir).map(|mut d| d.next().is_none()) {
        Ok(true) => {}
        Ok(false) => return fail(sink, EXIT_INVALID_INPUT, "invalid_input", &format!("挂载点 {dir} 不是空目录")),
        Err(e) => return fail(sink, EXIT_INVALID_INPUT, "invalid_input", &format!("挂载点 {dir} 不能用：{e}")),
    }
    let overlay = data_dir.join("mount-overlay").join(std::process::id().to_string());
    if let Err(e) = std::fs::create_dir_all(&overlay) {
        return fail(sink, EXIT_INTERNAL, "internal", &format!("不能创建覆盖层目录 {}：{e}", overlay.display()));
    }

    // 记录预读清单：挂载前开始，卸载后结束（结束时清单交给存储设备）。
    if let Some(name) = record {
        if let Err(e) = ask(&ctl, DriveRequest::Record { ws: base.ws.clone(), name: Some(name.to_string()) }).await {
            return fail_wire(sink, &e);
        }
    }
    let client = Client::start(ctl.clone());
    let stamp = match client.ask(DriveRequest::Stamp { ws: base.ws.clone() }) {
        Ok(DriveResponse::Stamp { tree }) => tree,
        Ok(_) => return fail(sink, EXIT_INTERNAL, "internal", "常驻程序的回应无法理解"),
        Err(e) => return fail_wire(sink, &e),
    };
    let fs = Fs::new(client, base.clone(), overlay.clone(), stamp);
    let opts = [MountOption::FSName("nexus".into()), MountOption::Subtype(base.ws.clone()), MountOption::DefaultPermissions, MountOption::NoAtime];
    let session = match fuser::spawn_mount2(fs, dir, &opts) {
        Ok(s) => s,
        Err(e) => return fail(sink, EXIT_INTERNAL, "internal", &format!("挂载失败：{e}")),
    };
    sink.err(format!("已把 {base} 挂到 {dir}；写入只落本机（nexus drive commit 提交），fusermount -u {dir} 卸载\n").as_bytes());

    use tokio::signal::unix::{signal, SignalKind};
    let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else {
        return fail(sink, EXIT_INTERNAL, "internal", "不能监听信号");
    };
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            _ = term.recv() => break,
            _ = int.recv() => break,
            _ = tick.tick() => if session.guard.is_finished() { break },
        }
    }
    // 卸载（已经被 fusermount -u 卸载时什么也不做），等 FUSE 线程结束：文件系统收尾时写回没写完的文件。
    drop(session);
    if record.is_some() {
        match ask(&ctl, DriveRequest::Record { ws: base.ws.clone(), name: None }).await {
            Ok(DriveResponse::Profile { name, files, saved_to }) => {
                let place = if saved_to.is_empty() { "只在本机".to_string() } else { format!("已交给 {}", saved_to.join("、")) };
                sink.err(format!("预读清单 {name}：{} 个文件，{place}\n", files.len()).as_bytes());
            }
            Ok(_) => {}
            Err(e) => sink.err(format!("保存预读清单失败：{}\n", e.message).as_bytes()),
        }
    }
    // 卸载偶尔没有生效时（挂载点还在），用 fusermount 强制卸掉，不留下断开的挂载点。
    if still_mounted(dir) {
        for bin in ["fusermount3", "fusermount"] {
            if std::process::Command::new(bin).args(["-u", "-z", dir]).status().is_ok_and(|s| s.success()) {
                break;
            }
        }
    }
    let _ = std::fs::remove_dir_all(&overlay);
    0
}

fn still_mounted(dir: &str) -> bool {
    // 断开的挂载点 canonicalize 会失败（ENOTCONN），这时按原样比较。
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| PathBuf::from(dir));
    let target = dir.to_string_lossy().replace(' ', "\\040");
    std::fs::read_to_string("/proc/self/mounts").is_ok_and(|m| m.lines().any(|l| l.split(' ').nth(1) == Some(target.as_str())))
}

async fn check_dir(ctl: &UnixCtl, base: &DrivePath) -> Result<(), WireError> {
    let Some((name, parent)) = base.parts.split_last() else {
        return ask(ctl, DriveRequest::Ls { path: base.to_string(), at: None }).await.map(|_| ());
    };
    let parent = DrivePath { ws: base.ws.clone(), parts: parent.to_vec() };
    match ask(ctl, DriveRequest::Ls { path: parent.to_string(), at: None }).await? {
        DriveResponse::Entries { entries, .. } => match entries.iter().find(|e| &e.name == name) {
            Some(e) if e.kind == "dir" => Ok(()),
            Some(_) => Err(WireError::new(ErrorCode::InvalidInput, format!("{base} 不是目录"))),
            None => Err(WireError::new(ErrorCode::NotFound, format!("{base} 不存在"))),
        },
        _ => Err(WireError::new(ErrorCode::Internal, "常驻程序的回应无法理解")),
    }
}

async fn ask<C: Ctl>(ctl: &C, req: DriveRequest) -> Result<DriveResponse, WireError> {
    let mut s = ctl.open(CtlRequest::Drive(req)).await.map_err(unreachable)?;
    match s.rx.recv().await {
        Some(CtlResponse::Drive(r)) => Ok(r),
        Some(CtlResponse::Error(e)) => Err(e),
        _ => Err(WireError::new(ErrorCode::Internal, "常驻程序的回应无法理解")),
    }
}

fn unreachable(e: std::io::Error) -> WireError {
    WireError::new(ErrorCode::Internal, format!("连不上本机的常驻程序（{e}）"))
}

async fn read_range<C: Ctl>(ctl: &C, path: String, offset: u64, length: u64) -> Result<Vec<u8>, WireError> {
    let mut s = ctl.open(CtlRequest::Drive(DriveRequest::Cat { path, at: None, offset, length: Some(length) })).await.map_err(unreachable)?;
    let mut out = Vec::new();
    loop {
        match s.rx.recv().await {
            Some(CtlResponse::Ok) => {}
            Some(CtlResponse::PipeData(b)) => out.extend_from_slice(&b),
            Some(CtlResponse::PipeEnd) => return Ok(out),
            Some(CtlResponse::Error(e)) => return Err(e),
            _ => return Err(WireError::new(ErrorCode::Internal, "常驻程序的回应无法理解")),
        }
    }
}

async fn put_file<C: Ctl>(ctl: &C, path: String, exec: bool, file: PathBuf) -> Result<(), WireError> {
    let mut f = File::open(&file).map_err(|e| WireError::new(ErrorCode::Internal, format!("读不了覆盖层文件：{e}")))?;
    let mut s = ctl.open(CtlRequest::Drive(DriveRequest::Put { path, exec })).await.map_err(unreachable)?;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| WireError::new(ErrorCode::Internal, format!("读不了覆盖层文件：{e}")))?;
        let msg = if n == 0 { Input::CloseStdin } else { Input::Stdin(buf[..n].to_vec()) };
        if s.tx.send(msg).await.is_err() || n == 0 {
            break;
        }
    }
    match s.rx.recv().await {
        Some(CtlResponse::Drive(_)) => Ok(()),
        Some(CtlResponse::Error(e)) => Err(e),
        _ => Err(WireError::new(ErrorCode::Internal, "常驻程序的回应无法理解")),
    }
}

/// 发给客户端线程的一次调用。
enum Call {
    Ask(DriveRequest, smpsc::Sender<Result<DriveResponse, WireError>>),
    Read(String, u64, u64, smpsc::Sender<Result<Vec<u8>, WireError>>),
    Put(String, bool, PathBuf, smpsc::Sender<Result<(), WireError>>),
}

/// 和常驻程序说话的线程。
struct Client {
    tx: mpsc::UnboundedSender<Call>,
}

impl Client {
    fn start(ctl: UnixCtl) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Call>();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("无法启动运行时");
            let local = tokio::task::LocalSet::new();
            let ctl = std::rc::Rc::new(ctl);
            local.block_on(&rt, async move {
                while let Some(call) = rx.recv().await {
                    let ctl = ctl.clone();
                    tokio::task::spawn_local(async move {
                        match call {
                            Call::Ask(req, reply) => drop(reply.send(ask(&*ctl, req).await)),
                            Call::Read(path, off, len, reply) => drop(reply.send(read_range(&*ctl, path, off, len).await)),
                            Call::Put(path, exec, file, reply) => drop(reply.send(put_file(&*ctl, path, exec, file).await)),
                        }
                    });
                }
            });
        });
        Self { tx }
    }

    fn call<T>(&self, make: impl FnOnce(smpsc::Sender<Result<T, WireError>>) -> Call) -> Result<T, WireError> {
        let (tx, rx) = smpsc::channel();
        if self.tx.send(make(tx)).is_err() {
            return Err(WireError::new(ErrorCode::Internal, "客户端线程已经结束"));
        }
        rx.recv().unwrap_or_else(|_| Err(WireError::new(ErrorCode::Internal, "客户端线程已经结束")))
    }

    fn ask(&self, req: DriveRequest) -> Result<DriveResponse, WireError> {
        self.call(|tx| Call::Ask(req, tx))
    }
}

fn errno(e: &WireError) -> i32 {
    match e.code {
        ErrorCode::NotFound => libc::ENOENT,
        ErrorCode::Denied => libc::EACCES,
        ErrorCode::InvalidInput => libc::EINVAL,
        _ => libc::EIO,
    }
}

/// 一个正在写的文件：内容在本地临时文件里，关闭时写进工作树。
struct Overlay {
    file: File,
    path: PathBuf,
    dirty: bool,
    exec: bool,
    refs: usize,
    mtime: SystemTime,
}

struct Fs {
    client: Client,
    base: DrivePath,
    overlay_dir: PathBuf,
    /// 相对挂载根的路径（"" 是根）和 inode 号，挂载期间稳定。
    inos: HashMap<String, u64>,
    paths: HashMap<u64, String>,
    next_ino: u64,
    dirs: HashMap<String, Vec<DriveEntryInfo>>,
    /// 每个文件上次看到的内容和那时定下的修改时间。
    seen: HashMap<String, (Option<[u8; 32]>, SystemTime)>,
    /// 程序用 touch 设的修改时间。
    touched: HashMap<String, SystemTime>,
    overlays: HashMap<String, Overlay>,
    handles: HashMap<u64, String>,
    next_fh: u64,
    start: SystemTime,
    first_stamp: [u8; 32],
    stamp: [u8; 32],
    checked: Instant,
    uid: u32,
    gid: u32,
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

fn split(rel: &str) -> (&str, &str) {
    rel.rsplit_once('/').unwrap_or(("", rel))
}

impl Fs {
    fn new(client: Client, base: DrivePath, overlay_dir: PathBuf, stamp: [u8; 32]) -> Self {
        let mut inos = HashMap::new();
        let mut paths = HashMap::new();
        inos.insert(String::new(), ROOT);
        paths.insert(ROOT, String::new());
        // SAFETY: getuid / getgid 没有前置条件。
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Self {
            client,
            base,
            overlay_dir,
            inos,
            paths,
            next_ino: ROOT + 1,
            dirs: HashMap::new(),
            seen: HashMap::new(),
            touched: HashMap::new(),
            overlays: HashMap::new(),
            handles: HashMap::new(),
            next_fh: 1,
            start: SystemTime::now(),
            first_stamp: stamp,
            stamp,
            checked: Instant::now(),
            uid,
            gid,
        }
    }

    /// 工作区里的路径。
    fn dpath(&self, rel: &str) -> String {
        let mut p = self.base.clone();
        p.parts.extend(rel.split('/').filter(|s| !s.is_empty()).map(str::to_string));
        p.to_string()
    }

    fn ino(&mut self, rel: &str) -> u64 {
        if let Some(i) = self.inos.get(rel) {
            return *i;
        }
        let i = self.next_ino;
        self.next_ino += 1;
        self.inos.insert(rel.to_string(), i);
        self.paths.insert(i, rel.to_string());
        i
    }

    fn path_of(&self, ino: u64) -> Result<String, i32> {
        self.paths.get(&ino).cloned().ok_or(libc::ENOENT)
    }

    /// 工作树的根变了就丢掉目录缓存。
    fn refresh(&mut self) {
        if self.checked.elapsed() < STAMP_EVERY {
            return;
        }
        self.checked = Instant::now();
        if let Ok(DriveResponse::Stamp { tree }) = self.client.ask(DriveRequest::Stamp { ws: self.base.ws.clone() }) {
            if tree != self.stamp {
                self.stamp = tree;
                self.dirs.clear();
            }
        }
    }

    /// 自己改了工作树：目录缓存作废，下次立刻重新检查。
    fn changed(&mut self) {
        self.dirs.clear();
        self.checked = Instant::now() - STAMP_EVERY;
    }

    fn list(&mut self, rel: &str) -> Result<&Vec<DriveEntryInfo>, i32> {
        self.refresh();
        if !self.dirs.contains_key(rel) {
            match self.client.ask(DriveRequest::Ls { path: self.dpath(rel), at: None }) {
                Ok(DriveResponse::Entries { entries, .. }) => {
                    self.dirs.insert(rel.to_string(), entries);
                }
                Ok(_) => return Err(libc::EIO),
                Err(e) => return Err(errno(&e)),
            }
        }
        Ok(&self.dirs[rel])
    }

    fn entry(&mut self, rel: &str) -> Result<Option<DriveEntryInfo>, i32> {
        if rel.is_empty() {
            return Ok(Some(DriveEntryInfo { name: String::new(), kind: "dir".into(), size: 0, exec: false, target: None, content: None }));
        }
        let (dir, name) = split(rel);
        match self.list(dir) {
            Ok(list) => Ok(list.iter().find(|e| e.name == name).cloned()),
            Err(e) if e == libc::ENOENT || e == libc::EINVAL => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn mtime(&mut self, rel: &str, content: Option<[u8; 32]>) -> SystemTime {
        if let Some(o) = self.overlays.get(rel).filter(|o| o.dirty) {
            return o.mtime;
        }
        if let Some(t) = self.touched.get(rel) {
            return *t;
        }
        let fresh = if self.stamp == self.first_stamp { self.start } else { SystemTime::now() };
        match self.seen.get(rel) {
            Some((c, t)) if *c == content => *t,
            Some(_) => {
                let now = SystemTime::now();
                self.seen.insert(rel.to_string(), (content, now));
                now
            }
            None => {
                self.seen.insert(rel.to_string(), (content, fresh));
                fresh
            }
        }
    }

    fn attr(&mut self, rel: &str) -> Result<FileAttr, i32> {
        let e = self.entry(rel)?.ok_or(libc::ENOENT)?;
        let ino = self.ino(rel);
        let (kind, perm, mut size) = match e.kind.as_str() {
            "dir" => (FileType::Directory, 0o755, 0),
            "symlink" => (FileType::Symlink, 0o777, e.size),
            _ => (FileType::RegularFile, if e.exec { 0o755 } else { 0o644 }, e.size),
        };
        let mut perm = perm;
        if let Some(o) = self.overlays.get(rel) {
            size = o.file.metadata().map(|m| m.len()).unwrap_or(size);
            perm = if o.exec { 0o755 } else { 0o644 };
        }
        let mtime = if kind == FileType::Directory { self.start } else { self.mtime(rel, e.content) };
        Ok(FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: mtime,
            kind,
            perm,
            nlink: if kind == FileType::Directory { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 64 * 1024,
            flags: 0,
        })
    }

    fn child(&mut self, parent: u64, name: &OsStr) -> Result<String, i32> {
        let dir = self.path_of(parent)?;
        let name = name.to_str().ok_or(libc::EINVAL)?;
        Ok(join(&dir, name))
    }

    fn ask(&mut self, req: DriveRequest) -> Result<DriveResponse, i32> {
        let r = self.client.ask(req).map_err(|e| errno(&e));
        self.changed();
        r
    }

    /// 把覆盖层里的内容写进工作树。
    fn upload(&mut self, rel: &str) -> Result<(), i32> {
        let Some(o) = self.overlays.get(rel) else { return Ok(()) };
        if !o.dirty {
            return Ok(());
        }
        let (path, exec, file) = (self.dpath(rel), o.exec, o.path.clone());
        let r = self.client.call(|tx| Call::Put(path, exec, file, tx)).map_err(|e| errno(&e));
        self.changed();
        if r.is_ok() {
            // 修改时间保持写入时的；记下新内容的标识，以后内容再变才再变。
            let o = self.overlays.get_mut(rel).unwrap();
            o.dirty = false;
            let t = o.mtime;
            self.touched.remove(rel);
            if let Ok(Some(e)) = self.entry(rel) {
                self.seen.insert(rel.to_string(), (e.content, t));
            }
        }
        r
    }

    /// 打开文件准备写：建立覆盖层（已有内容复制进来，`truncate` 时从空开始）。
    fn open_write(&mut self, rel: &str, truncate: bool, exec: bool) -> Result<(), i32> {
        if let Some(o) = self.overlays.get_mut(rel) {
            o.refs += 1;
            if truncate {
                o.file.set_len(0).map_err(|_| libc::EIO)?;
                o.dirty = true;
                o.mtime = SystemTime::now();
            }
            return Ok(());
        }
        let n = self.next_fh;
        let path = self.overlay_dir.join(format!("o{n}"));
        let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).map_err(|_| libc::EIO)?;
        let existing = self.entry(rel)?;
        if let Some(e) = existing.as_ref().filter(|e| e.kind == "file") {
            // 打开时就确认能写（把可执行位设成它现在的值，工作树不变）：关闭时才失败的话，多数程序不会报告。
            self.client.ask(DriveRequest::Chmod { path: self.dpath(rel), exec: e.exec }).map_err(|e| errno(&e))?;
        }
        let size = existing.map(|e| e.size).unwrap_or(0);
        if !truncate && size > 0 {
            let mut off = 0u64;
            while off < size {
                let len = (size - off).min(4 * 1024 * 1024);
                let data = self.client.call(|tx| Call::Read(self.dpath(rel), off, len, tx)).map_err(|e| errno(&e))?;
                if data.is_empty() {
                    break;
                }
                file.write_all_at(&data, off).map_err(|_| libc::EIO)?;
                off += data.len() as u64;
            }
        }
        self.overlays.insert(rel.to_string(), Overlay { file, path, dirty: truncate, exec, refs: 1, mtime: SystemTime::now() });
        if truncate {
            // 截断立刻生效（新文件也立刻出现在工作树里）。
            if let Err(e) = self.upload(rel) {
                let o = self.overlays.remove(rel).unwrap();
                let _ = std::fs::remove_file(&o.path);
                return Err(e);
            }
        }
        Ok(())
    }

    fn new_handle(&mut self, rel: &str) -> u64 {
        let fh = self.next_fh;
        self.next_fh += 1;
        self.handles.insert(fh, rel.to_string());
        fh
    }

    fn close(&mut self, fh: u64) -> Result<(), i32> {
        let Some(rel) = self.handles.remove(&fh) else { return Ok(()) };
        let r = self.upload(&rel);
        if let Some(o) = self.overlays.get_mut(&rel) {
            o.refs = o.refs.saturating_sub(1);
            if o.refs == 0 {
                let o = self.overlays.remove(&rel).unwrap();
                let _ = std::fs::remove_file(&o.path);
            }
        }
        r
    }

    /// 改名后，旧路径（和它下面的路径）的 inode、覆盖层、打开的句柄都跟着走。
    fn moved(&mut self, from: &str, to: &str) {
        let under = |p: &str| p == from || p.starts_with(&format!("{from}/"));
        let renamed = |p: &str| format!("{to}{}", &p[from.len()..]);
        if let Some(old) = self.inos.remove(to) {
            self.paths.remove(&old);
        }
        let moving: Vec<(String, u64)> = self.inos.iter().filter(|(p, _)| under(p)).map(|(p, i)| (p.clone(), *i)).collect();
        for (p, i) in moving {
            self.inos.remove(&p);
            let q = renamed(&p);
            self.inos.insert(q.clone(), i);
            self.paths.insert(i, q);
        }
        for map in [&mut self.seen] {
            let keys: Vec<String> = map.keys().filter(|p| under(p)).cloned().collect();
            for k in keys {
                let v = map.remove(&k).unwrap();
                map.insert(renamed(&k), v);
            }
        }
        let keys: Vec<String> = self.overlays.keys().filter(|p| under(p)).cloned().collect();
        for k in keys {
            let v = self.overlays.remove(&k).unwrap();
            self.overlays.insert(renamed(&k), v);
        }
        for p in self.handles.values_mut() {
            if under(p) {
                *p = renamed(p);
            }
        }
    }
}

impl Drop for Fs {
    fn drop(&mut self) {
        // 卸载时还没写回的内容（一般没有：文件关闭时已经写回）。
        let dirty: Vec<String> = self.overlays.iter().filter(|(_, o)| o.dirty).map(|(p, _)| p.clone()).collect();
        for p in dirty {
            let _ = self.upload(&p);
        }
    }
}

impl Filesystem for Fs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        match self.child(parent, name).and_then(|rel| self.attr(&rel)) {
            Ok(a) => reply.entry(&TTL, &a, 0),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        match self.path_of(ino).and_then(|rel| self.attr(&rel)) {
            Ok(a) => reply.attr(&TTL, &a),
            Err(e) => reply.error(e),
        }
    }

    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let r = (|| {
            let rel = self.path_of(ino)?;
            let e = self.entry(&rel)?.ok_or(libc::ENOENT)?;
            if let Some(mode) = mode {
                let exec = mode & 0o111 != 0;
                if e.kind == "file" && exec != e.exec {
                    self.ask(DriveRequest::Chmod { path: self.dpath(&rel), exec })?;
                }
                if let Some(o) = self.overlays.get_mut(&rel) {
                    o.exec = exec;
                }
            }
            if let Some(size) = size {
                if e.kind == "dir" {
                    return Err(libc::EISDIR);
                }
                let opened = self.overlays.contains_key(&rel);
                if !opened {
                    self.open_write(&rel, size == 0, e.exec)?;
                }
                let o = self.overlays.get_mut(&rel).unwrap();
                o.file.set_len(size).map_err(|_| libc::EIO)?;
                o.dirty = true;
                o.mtime = SystemTime::now();
                if !opened {
                    let fh = self.new_handle(&rel);
                    self.close(fh)?;
                }
            }
            if let Some(t) = mtime {
                let t = match t {
                    TimeOrNow::SpecificTime(t) => t,
                    TimeOrNow::Now => SystemTime::now(),
                };
                if let Some(o) = self.overlays.get_mut(&rel) {
                    o.mtime = t;
                }
                self.touched.insert(rel.clone(), t);
            }
            self.attr(&rel)
        })();
        match r {
            Ok(a) => reply.attr(&TTL, &a),
            Err(e) => reply.error(e),
        }
    }

    fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
        match self.path_of(ino).and_then(|rel| self.entry(&rel)) {
            Ok(Some(DriveEntryInfo { target: Some(t), .. })) => reply.data(t.as_bytes()),
            Ok(Some(_)) => reply.error(libc::EINVAL),
            Ok(None) => reply.error(libc::ENOENT),
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        let r = (|| {
            let rel = self.child(parent, name)?;
            if self.entry(&rel)?.is_some() {
                return Err(libc::EEXIST);
            }
            self.ask(DriveRequest::Mkdir { path: self.dpath(&rel) })?;
            self.attr(&rel)
        })();
        match r {
            Ok(a) => reply.entry(&TTL, &a, 0),
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let r = (|| {
            let rel = self.child(parent, name)?;
            match self.entry(&rel)? {
                None => return Err(libc::ENOENT),
                Some(e) if e.kind == "dir" => return Err(libc::EISDIR),
                Some(_) => {}
            }
            self.ask(DriveRequest::Rm { path: self.dpath(&rel), recursive: false })?;
            if let Some(i) = self.inos.remove(&rel) {
                self.paths.remove(&i);
            }
            self.seen.remove(&rel);
            Ok(())
        })();
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let r = (|| {
            let rel = self.child(parent, name)?;
            match self.entry(&rel)? {
                None => return Err(libc::ENOENT),
                Some(e) if e.kind != "dir" => return Err(libc::ENOTDIR),
                Some(_) => {}
            }
            if !self.list(&rel)?.is_empty() {
                return Err(libc::ENOTEMPTY);
            }
            self.ask(DriveRequest::Rm { path: self.dpath(&rel), recursive: true })?;
            if let Some(i) = self.inos.remove(&rel) {
                self.paths.remove(&i);
            }
            Ok(())
        })();
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn symlink(&mut self, _req: &Request<'_>, parent: u64, link_name: &OsStr, target: &Path, reply: ReplyEntry) {
        let r = (|| {
            let rel = self.child(parent, link_name)?;
            if self.entry(&rel)?.is_some() {
                return Err(libc::EEXIST);
            }
            let target = target.to_str().ok_or(libc::EINVAL)?.to_string();
            self.ask(DriveRequest::Symlink { path: self.dpath(&rel), target })?;
            self.attr(&rel)
        })();
        match r {
            Ok(a) => reply.entry(&TTL, &a, 0),
            Err(e) => reply.error(e),
        }
    }

    fn rename(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, newparent: u64, newname: &OsStr, flags: u32, reply: ReplyEmpty) {
        let r = (|| {
            if flags != 0 {
                return Err(libc::EINVAL);
            }
            let from = self.child(parent, name)?;
            let to = self.child(newparent, newname)?;
            let src = self.entry(&from)?.ok_or(libc::ENOENT)?;
            if let Some(dst) = self.entry(&to)? {
                match (src.kind == "dir", dst.kind == "dir") {
                    (false, true) => return Err(libc::EISDIR),
                    (true, false) => return Err(libc::ENOTDIR),
                    (true, true) if !self.list(&to)?.is_empty() => return Err(libc::ENOTEMPTY),
                    _ => {}
                }
            }
            // 正在写的文件先写回，改名搬的是最新内容。
            let dirty: Vec<String> = self.overlays.keys().filter(|p| **p == from || p.starts_with(&format!("{from}/"))).cloned().collect();
            for p in dirty {
                self.upload(&p)?;
            }
            self.ask(DriveRequest::Rename { from: self.dpath(&from), to: self.dpath(&to) })?;
            self.moved(&from, &to);
            Ok(())
        })();
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        let r = (|| {
            let rel = self.path_of(ino)?;
            let e = self.entry(&rel)?.ok_or(libc::ENOENT)?;
            if e.kind == "dir" {
                return Err(libc::EISDIR);
            }
            if flags & libc::O_ACCMODE != libc::O_RDONLY {
                self.open_write(&rel, flags & libc::O_TRUNC != 0, e.exec)?;
            }
            Ok(self.new_handle(&rel))
        })();
        match r {
            // 不保留页缓存：每次打开都读到工作树的当前内容。
            Ok(fh) => reply.opened(fh, 0),
            Err(e) => reply.error(e),
        }
    }

    fn read(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, offset: i64, size: u32, _flags: i32, _lock: Option<u64>, reply: ReplyData) {
        let Some(rel) = self.handles.get(&fh).cloned() else { return reply.error(libc::EBADF) };
        if let Some(o) = self.overlays.get(&rel) {
            let mut buf = vec![0u8; size as usize];
            return match o.file.read_at(&mut buf, offset as u64) {
                Ok(n) => reply.data(&buf[..n]),
                Err(_) => reply.error(libc::EIO),
            };
        }
        match self.client.call(|tx| Call::Read(self.dpath(&rel), offset as u64, size as u64, tx)) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyWrite,
    ) {
        let Some(rel) = self.handles.get(&fh) else { return reply.error(libc::EBADF) };
        let Some(o) = self.overlays.get_mut(rel) else { return reply.error(libc::EBADF) };
        match o.file.write_all_at(data, offset as u64) {
            Ok(()) => {
                o.dirty = true;
                o.mtime = SystemTime::now();
                reply.written(data.len() as u32)
            }
            Err(_) => reply.error(libc::EIO),
        }
    }

    fn flush(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        let Some(rel) = self.handles.get(&fh).cloned() else { return reply.ok() };
        match self.upload(&rel) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn fsync(&mut self, req: &Request<'_>, ino: u64, fh: u64, _datasync: bool, reply: ReplyEmpty) {
        self.flush(req, ino, fh, 0, reply)
    }

    fn release(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, _flags: i32, _lock: Option<u64>, _flush: bool, reply: ReplyEmpty) {
        match self.close(fh) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn readdir(&mut self, _req: &Request<'_>, ino: u64, _fh: u64, offset: i64, mut reply: ReplyDirectory) {
        let r = (|| {
            let rel = self.path_of(ino)?;
            let list = self.list(&rel)?.clone();
            let parent = if rel.is_empty() { ROOT } else { self.ino(split(&rel).0) };
            let mut all = vec![(ino, FileType::Directory, ".".to_string()), (parent, FileType::Directory, "..".to_string())];
            for e in list {
                let kind = match e.kind.as_str() {
                    "dir" => FileType::Directory,
                    "symlink" => FileType::Symlink,
                    _ => FileType::RegularFile,
                };
                all.push((self.ino(&join(&rel, &e.name)), kind, e.name));
            }
            Ok(all)
        })();
        match r {
            Ok(all) => {
                for (i, (ino, kind, name)) in all.into_iter().enumerate().skip(offset as usize) {
                    if reply.add(ino, (i + 1) as i64, kind, name) {
                        break;
                    }
                }
                reply.ok()
            }
            Err(e) => reply.error(e),
        }
    }

    fn create(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, mode: u32, _umask: u32, _flags: i32, reply: ReplyCreate) {
        let r = (|| {
            let rel = self.child(parent, name)?;
            if let Some(e) = self.entry(&rel)? {
                if e.kind == "dir" {
                    return Err(libc::EISDIR);
                }
            }
            self.open_write(&rel, true, mode & 0o111 != 0)?;
            let fh = self.new_handle(&rel);
            Ok((self.attr(&rel)?, fh))
        })();
        match r {
            Ok((a, fh)) => reply.created(&TTL, &a, 0, fh, 0),
            Err(e) => reply.error(e),
        }
    }
}
