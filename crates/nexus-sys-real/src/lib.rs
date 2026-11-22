//! 驱动：`System` 的真实实现。
//!
//! Windows 与 Unix 版本通过 `cfg` 分叉：
//! - 进程树终止：Unix 用 `killpg`，Windows 用 `taskkill /T`。
//! - 目录与文件权限：Unix 显式 `mode(0o700)/0o600`，Windows 靠默认 ACL。
//! - Shell 与环境变量：Unix 用 `$SHELL`、`$HOME`；Windows 用 `ComSpec`/`powershell`、`USERPROFILE`。
//! - 伪终端：Unix 用 `/bin/sh`，Windows 用 `powershell.exe`/`cmd.exe`（`portable-pty` 选对实现）。
//!
//! 以上分叉均不引入 `unsafe`，保持 `#![forbid(unsafe_code)]`。
#![forbid(unsafe_code)]

use nexus_core_proto::Timestamp;
use nexus_sys::{Cmd, ExitStatus, Proc, ProcEvent, Pty, PtyEvent, Size, System};
use std::process::Stdio;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStderr, ChildStdout};
use tokio::sync::mpsc;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct RealSystem {
    dir: PathBuf,
}

impl RealSystem {
    /// `dir` 是本节点的数据目录，不存在时创建。
    pub fn new(dir: impl AsRef<Path>) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir.as_ref())?;
            std::fs::set_permissions(dir.as_ref(), std::fs::Permissions::from_mode(0o700))?;
        }
        #[cfg(windows)]
        {
            std::fs::create_dir_all(dir.as_ref())?;
        }
        Ok(Self { dir: dir.as_ref().to_path_buf() })
    }

    fn path(&self, key: &str) -> io::Result<PathBuf> {
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "非法的存储键"));
        }
        Ok(self.dir.join(format!("{key}.bin")))
    }

    /// 对象放在 cas/<前两个十六进制字符>/<其余>。
    fn cas_path(&self, hash: &[u8; 32]) -> PathBuf {
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        self.dir.join("cas").join(&hex[..2]).join(&hex[2..])
    }
}

fn default_cwd() -> PathBuf {
    #[cfg(unix)]
    {
        std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into())
    }
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(|| ".".into())
    }
}

impl System for RealSystem {
    type Proc = RealProc;
    type Pty = RealPty;
    type Tcp = tokio::net::TcpStream;

    async fn tcp_connect(&self, addr: std::net::SocketAddr) -> io::Result<tokio::net::TcpStream> {
        let s = tokio::net::TcpStream::connect(addr).await?;
        let _ = s.set_nodelay(true);
        Ok(s)
    }

    async fn resolve(&self, host: &str) -> io::Result<Vec<std::net::IpAddr>> {
        Ok(tokio::net::lookup_host((host, 0)).await?.map(|a| a.ip()).collect())
    }

    fn spawn_pty(&self, size: Size) -> io::Result<RealPty> {
        RealPty::spawn(size)
    }

    fn spawn(&self, cmd: Cmd) -> io::Result<RealProc> {
        let mut c = tokio::process::Command::new(&cmd.program);
        c.args(&cmd.args)
            .envs(cmd.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        c.process_group(0);
        c.kill_on_drop(true);
        match cmd.cwd {
            Some(dir) => c.current_dir(dir),
            None => c.current_dir(default_cwd()),
        };
        let mut child = c.spawn()?;
        #[cfg(unix)]
        let pgid = child.id().map(|id| id as i32);
        #[cfg(windows)]
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        if let Some(mut stdin) = child.stdin.take() {
            tokio::spawn(async move {
                while let Some(chunk) = stdin_rx.recv().await {
                    if stdin.write_all(&chunk).await.is_err() {
                        return;
                    }
                }
            });
        }
        Ok(RealProc {
            child,
            #[cfg(unix)]
            pgid,
            #[cfg(windows)]
            pid,
            stdout,
            stderr,
            stdin: Some(stdin_tx),
            out_buf: vec![0; 64 * 1024],
            err_buf: vec![0; 64 * 1024],
        })
    }

    fn now(&self) -> Timestamp {
        let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        Timestamp(d.as_millis() as u64)
    }

    fn random(&self, buf: &mut [u8]) {
        getrandom::fill(buf).expect("操作系统随机数源不可用");
    }

    fn store_get(&self, key: &str) -> io::Result<Option<Vec<u8>>> {
        match std::fs::read(self.path(key)?) {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn append_log(&self, name: &str, record: &[u8], max_bytes: u64) -> io::Result<()> {
        use std::io::Write;
        let path = self.path(name)?.with_extension("log");
        let current = std::fs::metadata(&path).map_or(0, |m| m.len());
        if current > 0 && current + record.len() as u64 > max_bytes {
            std::fs::rename(&path, path.with_extension("log.1"))?;
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        f.write_all(record)
    }

    fn read_log(&self, name: &str) -> io::Result<Vec<u8>> {
        let path = self.path(name)?.with_extension("log");
        let read = |p: &std::path::Path| match std::fs::read(p) {
            Ok(v) => Ok(v),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        };
        let mut out = read(&path.with_extension("log.1"))?;
        out.extend(read(&path)?);
        Ok(out)
    }

    fn cas_get(&self, hash: &[u8; 32]) -> io::Result<Option<Vec<u8>>> {
        match std::fs::read(self.cas_path(hash)) {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn cas_put(&self, hash: &[u8; 32], bytes: &[u8]) -> io::Result<()> {
        let path = self.cas_path(hash);
        if path.exists() {
            return Ok(());
        }
        std::fs::create_dir_all(path.parent().expect("有上级目录"))?;
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(tmp, path)
    }

    fn cas_has(&self, hash: &[u8; 32]) -> io::Result<bool> {
        Ok(self.cas_path(hash).exists())
    }

    fn store_put(&self, key: &str, value: &[u8]) -> io::Result<()> {
        let path = self.path(key)?;
        let tmp = path.with_extension("tmp");
        use std::io::Write;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            file.write_all(value)?;
        }
        #[cfg(windows)]
        {
            let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?;
            file.write_all(value)?;
        }
        std::fs::rename(tmp, path)
    }
}

pub struct RealProc {
    child: Child,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    stdin: Option<mpsc::UnboundedSender<Vec<u8>>>,
    out_buf: Vec<u8>,
    err_buf: Vec<u8>,
    #[cfg(unix)]
    pgid: Option<i32>,
    #[cfg(windows)]
    pid: Option<u32>,
}

async fn read_some<R: AsyncRead + Unpin>(pipe: &mut Option<R>, buf: &mut [u8]) -> io::Result<usize> {
    match pipe {
        Some(p) => p.read(buf).await,
        None => std::future::pending().await,
    }
}

impl Proc for RealProc {
    async fn next_event(&mut self) -> ProcEvent {
        loop {
            if self.stdout.is_none() && self.stderr.is_none() {
                let status = self.child.wait().await;
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    return ProcEvent::Exited(match status {
                        Ok(s) => ExitStatus { code: s.code(), signal: s.signal() },
                        Err(_) => ExitStatus { code: None, signal: None },
                    });
                }
                #[cfg(windows)]
                {
                    return ProcEvent::Exited(match status {
                        Ok(s) => ExitStatus { code: s.code(), signal: None },
                        Err(_) => ExitStatus { code: None, signal: None },
                    });
                }
            }
            tokio::select! {
                r = read_some(&mut self.stdout, &mut self.out_buf) => match r {
                    Ok(0) | Err(_) => self.stdout = None,
                    Ok(n) => return ProcEvent::Stdout(self.out_buf[..n].to_vec()),
                },
                r = read_some(&mut self.stderr, &mut self.err_buf) => match r {
                    Ok(0) | Err(_) => self.stderr = None,
                    Ok(n) => return ProcEvent::Stderr(self.err_buf[..n].to_vec()),
                },
            }
        }
    }

    fn write_stdin(&mut self, data: Vec<u8>) {
        if let Some(tx) = &self.stdin {
            let _ = tx.send(data);
        }
    }

    fn close_stdin(&mut self) {
        self.stdin = None;
    }

    fn kill(&mut self) {
        #[cfg(unix)]
        {
            if let Some(pgid) = self.pgid {
                let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), nix::sys::signal::Signal::SIGKILL);
            }
        }
        #[cfg(windows)]
        {
            if let Some(pid) = self.pid {
                let _ = std::process::Command::new("taskkill")
                    .args(["/F", "/T", "/PID"])
                    .arg(pid.to_string())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
        }
        let _ = self.child.start_kill();
    }
}

// ---------------------------------------------------------------- 伪终端

fn pty_size(size: Size) -> portable_pty::PtySize {
    portable_pty::PtySize { rows: size.rows.max(1), cols: size.cols.max(1), pixel_width: 0, pixel_height: 0 }
}

enum PtyMsg {
    Output(Vec<u8>),
    Eof,
    Exited(ExitStatus),
}

const DRAIN_AFTER_EXIT: std::time::Duration = std::time::Duration::from_millis(300);

pub struct RealPty {
    master: Box<dyn portable_pty::MasterPty + Send>,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    #[cfg(unix)]
    pid: Option<u32>,
    #[cfg(windows)]
    pid: Option<u32>,
    events: mpsc::UnboundedReceiver<PtyMsg>,
    input: std::sync::mpsc::Sender<Vec<u8>>,
    eof: bool,
    status: Option<ExitStatus>,
}

impl RealPty {
    fn spawn(size: Size) -> io::Result<Self> {
        fn other(e: impl std::fmt::Display) -> io::Error {
            io::Error::other(e.to_string())
        }
        let pair = portable_pty::native_pty_system().openpty(pty_size(size)).map_err(other)?;

        #[cfg(unix)]
        let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
        #[cfg(windows)]
        let shell = std::env::var("ComSpec")
            .ok()
            .and_then(|p| {
                if std::path::Path::new(&p).file_name().map_or(false, |n| n.to_string_lossy().eq_ignore_ascii_case("powershell.exe")) {
                    Some(p)
                } else if std::path::Path::new("C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe").exists() {
                    Some("C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe".into())
                } else {
                    Some(p)
                }
            })
            .unwrap_or_else(|| "cmd.exe".into());

        let mut cmd = portable_pty::CommandBuilder::new(shell);
        #[cfg(unix)]
        cmd.env("TERM", "xterm-256color");
        #[cfg(unix)]
        cmd.cwd(std::env::var_os("HOME").unwrap_or_else(|| "/".into()));
        #[cfg(windows)]
        cmd.cwd(std::env::var_os("USERPROFILE").unwrap_or_else(|| ".".into()));

        let mut child = pair.slave.spawn_command(cmd).map_err(other)?;
        drop(pair.slave);
        let pid = child.process_id();
        let killer = child.clone_killer();
        let mut reader = pair.master.try_clone_reader().map_err(other)?;
        let mut writer = pair.master.take_writer().map_err(other)?;

        let (tx, events) = mpsc::unbounded_channel();
        let out = tx.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if out.send(PtyMsg::Output(buf[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                }
            }
            let _ = out.send(PtyMsg::Eof);
        });
        std::thread::spawn(move || {
            let status = match child.wait() {
                #[cfg(unix)]
                Ok(s) => {
                    let signal = s.signal().map(|_| 0);
                    ExitStatus { code: if signal.is_some() { None } else { Some(s.exit_code() as i32) }, signal }
                }
                #[cfg(windows)]
                Ok(s) => {
                    ExitStatus { code: Some(s.exit_code() as i32), signal: None }
                }
                Err(_) => ExitStatus { code: None, signal: None },
            };
            let _ = tx.send(PtyMsg::Exited(status));
        });
        let (input, input_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            use std::io::Write;
            while let Ok(chunk) = input_rx.recv() {
                if writer.write_all(&chunk).and_then(|_| writer.flush()).is_err() {
                    return;
                }
            }
        });
        Ok(Self { master: pair.master, killer, pid, events, input, eof: false, status: None })
    }
}

impl Pty for RealPty {
    async fn next_event(&mut self) -> PtyEvent {
        loop {
            if let (true, Some(status)) = (self.eof, self.status) {
                return PtyEvent::Exited(status);
            }
            let msg = match self.status {
                Some(status) => match tokio::time::timeout(DRAIN_AFTER_EXIT, self.events.recv()).await {
                    Ok(m) => m,
                    Err(_) => return PtyEvent::Exited(status),
                },
                None => self.events.recv().await,
            };
            match msg {
                Some(PtyMsg::Output(b)) => return PtyEvent::Output(b),
                Some(PtyMsg::Eof) => self.eof = true,
                Some(PtyMsg::Exited(s)) => self.status = Some(s),
                None => return PtyEvent::Exited(self.status.unwrap_or(ExitStatus { code: None, signal: None })),
            }
        }
    }

    fn write(&mut self, data: Vec<u8>) {
        let _ = self.input.send(data);
    }

    fn resize(&mut self, size: Size) {
        let _ = self.master.resize(pty_size(size));
    }

    fn kill(&mut self) {
        #[cfg(unix)]
        {
            if let Some(pid) = self.pid {
                let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGKILL);
            }
        }
        #[cfg(windows)]
        {
            if let Some(pid) = self.pid {
                let _ = std::process::Command::new("taskkill")
                    .args(["/F", "/T", "/PID"])
                    .arg(pid.to_string())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
        }
        let _ = self.killer.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_and_key_files_are_private_even_with_existing_permissive_paths() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("data");
            std::fs::create_dir(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            let tmp = path.join("device_key.tmp");
            std::fs::write(&tmp, b"old").unwrap();
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
            let sys = RealSystem::new(&path).unwrap();
            sys.store_put("device_key", &[42; 32]).unwrap();
            sys.store_put("root_key", &[43; 32]).unwrap();
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o700);
            for key in ["device_key", "root_key"] {
                assert_eq!(std::fs::metadata(sys.path(key).unwrap()).unwrap().permissions().mode() & 0o777, 0o600);
            }
            assert_eq!(sys.store_get("device_key").unwrap().unwrap(), [42; 32]);
        }
    }

    #[test]
    fn logs_rotate_at_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let sys = RealSystem::new(dir.path()).unwrap();
        nexus_sys::check_log_rotation(&sys);
    }
}
