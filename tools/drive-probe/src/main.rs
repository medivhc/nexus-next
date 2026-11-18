//! M4.0 技术验证：
//!   drive-probe chunk <路径>...          内容定义分块（FastCDC）+ BLAKE3 的吞吐
//!   drive-probe mount <源目录> <挂载点>   只读的 FUSE 透传，用来测在挂载目录里构建的开销
use fuser::{FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyOpen, Request};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("chunk") => chunk(&args[2..]),
        Some("mount") => mount(Path::new(&args[2]), Path::new(&args[3])),
        _ => eprintln!("用法：drive-probe chunk <路径>... | mount <源目录> <挂载点>"),
    }
}

fn walk(p: &Path, out: &mut Vec<PathBuf>) {
    let Ok(md) = std::fs::symlink_metadata(p) else { return };
    if md.is_dir() {
        if p.file_name().is_some_and(|n| n == "target" || n == ".git" || n == "node_modules") {
            return;
        }
        for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
            walk(&e.path(), out);
        }
    } else if md.is_file() {
        out.push(p.to_path_buf());
    }
}

fn chunk(paths: &[String]) {
    let mut files = Vec::new();
    for p in paths {
        walk(Path::new(p), &mut files);
    }
    let (min, avg, max) = (16 * 1024, 64 * 1024, 256 * 1024);
    let t = Instant::now();
    let (mut bytes, mut chunks, mut unique) = (0u64, 0u64, std::collections::HashSet::new());
    for f in &files {
        let data = std::fs::read(f).unwrap_or_default();
        bytes += data.len() as u64;
        if data.is_empty() {
            continue;
        }
        for c in fastcdc::v2020::FastCDC::new(&data, min, avg, max) {
            let h = blake3::hash(&data[c.offset..c.offset + c.length]);
            chunks += 1;
            unique.insert(*h.as_bytes());
        }
    }
    let s = t.elapsed().as_secs_f64();
    println!(
        "{} 个文件，{:.1} MB，{} 块（不同的 {}），分块+哈希 {:.2} 秒，{:.0} MB/s（单线程，含读文件）",
        files.len(),
        bytes as f64 / 1e6,
        chunks,
        unique.len(),
        s,
        bytes as f64 / 1e6 / s
    );
}

// ---------------------------------------------------------------- 只读透传

struct Passthrough {
    paths: HashMap<u64, PathBuf>,
    inos: HashMap<PathBuf, u64>,
    next: u64,
    files: HashMap<u64, File>,
    next_fh: u64,
}

const TTL: Duration = Duration::from_secs(60);

impl Passthrough {
    fn ino(&mut self, p: PathBuf) -> u64 {
        if let Some(&i) = self.inos.get(&p) {
            return i;
        }
        self.next += 1;
        self.paths.insert(self.next, p.clone());
        self.inos.insert(p, self.next);
        self.next
    }

    fn attr(ino: u64, p: &Path) -> Option<FileAttr> {
        let m = std::fs::symlink_metadata(p).ok()?;
        let kind = if m.is_dir() {
            FileType::Directory
        } else if m.file_type().is_symlink() {
            FileType::Symlink
        } else {
            FileType::RegularFile
        };
        let t = |s: i64, ns: i64| UNIX_EPOCH + Duration::new(s.max(0) as u64, ns as u32);
        Some(FileAttr {
            ino,
            size: m.len(),
            blocks: m.blocks(),
            atime: t(m.atime(), m.atime_nsec()),
            mtime: t(m.mtime(), m.mtime_nsec()),
            ctime: t(m.ctime(), m.ctime_nsec()),
            crtime: t(m.mtime(), m.mtime_nsec()),
            kind,
            perm: (m.mode() & 0o7777) as u16,
            nlink: m.nlink() as u32,
            uid: m.uid(),
            gid: m.gid(),
            rdev: 0,
            blksize: 4096,
            flags: 0,
        })
    }
}

impl Filesystem for Passthrough {
    fn lookup(&mut self, _: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(dir) = self.paths.get(&parent).cloned() else { return reply.error(libc::ENOENT) };
        let p = dir.join(name);
        if std::fs::symlink_metadata(&p).is_err() {
            return reply.error(libc::ENOENT);
        }
        let ino = self.ino(p.clone());
        match Self::attr(ino, &p) {
            Some(a) => reply.entry(&TTL, &a, 0),
            None => reply.error(libc::ENOENT),
        }
    }

    fn getattr(&mut self, _: &Request, ino: u64, _: Option<u64>, reply: ReplyAttr) {
        match self.paths.get(&ino).and_then(|p| Self::attr(ino, p)) {
            Some(a) => reply.attr(&TTL, &a),
            None => reply.error(libc::ENOENT),
        }
    }

    fn readlink(&mut self, _: &Request, ino: u64, reply: ReplyData) {
        match self.paths.get(&ino).and_then(|p| std::fs::read_link(p).ok()) {
            Some(t) => reply.data(t.as_os_str().as_encoded_bytes()),
            None => reply.error(libc::ENOENT),
        }
    }

    fn open(&mut self, _: &Request, ino: u64, _flags: i32, reply: ReplyOpen) {
        match self.paths.get(&ino).and_then(|p| File::open(p).ok()) {
            Some(f) => {
                self.next_fh += 1;
                self.files.insert(self.next_fh, f);
                reply.opened(self.next_fh, fuser::consts::FOPEN_KEEP_CACHE)
            }
            None => reply.error(libc::EACCES),
        }
    }

    fn read(&mut self, _: &Request, _: u64, fh: u64, offset: i64, size: u32, _: i32, _: Option<u64>, reply: ReplyData) {
        let Some(f) = self.files.get(&fh) else { return reply.error(libc::EBADF) };
        let mut buf = vec![0u8; size as usize];
        match f.read_at(&mut buf, offset as u64) {
            Ok(n) => reply.data(&buf[..n]),
            Err(_) => reply.error(libc::EIO),
        }
    }

    fn release(&mut self, _: &Request, _: u64, fh: u64, _: i32, _: Option<u64>, _: bool, reply: fuser::ReplyEmpty) {
        self.files.remove(&fh);
        reply.ok();
    }

    fn readdir(&mut self, _: &Request, ino: u64, _: u64, offset: i64, mut reply: ReplyDirectory) {
        let Some(dir) = self.paths.get(&ino).cloned() else { return reply.error(libc::ENOENT) };
        let mut entries = vec![(ino, FileType::Directory, ".".into()), (ino, FileType::Directory, "..".into())];
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let ft = e.file_type().ok();
            let kind = match ft {
                Some(t) if t.is_dir() => FileType::Directory,
                Some(t) if t.is_symlink() => FileType::Symlink,
                _ => FileType::RegularFile,
            };
            let i = self.ino(e.path());
            entries.push((i, kind, e.file_name()));
        }
        for (i, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(ino, (i + 1) as i64, kind, name) {
                break;
            }
        }
        reply.ok();
    }
}

fn mount(src: &Path, at: &Path) {
    let src = std::fs::canonicalize(src).expect("源目录");
    let mut fs = Passthrough { paths: HashMap::new(), inos: HashMap::new(), next: 1, files: HashMap::new(), next_fh: 0 };
    fs.paths.insert(1, src.clone());
    fs.inos.insert(src, 1);
    let opts = [MountOption::RO, MountOption::FSName("drive-probe".into()), MountOption::DefaultPermissions];
    fuser::mount2(fs, at, &opts).expect("挂载失败");
}
