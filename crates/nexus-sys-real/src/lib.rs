//! 驱动：`System` 的真实实现。
#![forbid(unsafe_code)]

use nexus_core_proto::Timestamp;
use nexus_sys::{Cmd, ExitStatus, Proc, ProcEvent, System};
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
        std::fs::create_dir_all(dir.as_ref())?;
        Ok(Self { dir: dir.as_ref().to_path_buf() })
    }

    fn path(&self, key: &str) -> io::Result<PathBuf> {
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "非法的存储键"));
        }
        Ok(self.dir.join(format!("{key}.bin")))
    }
}

impl System for RealSystem {
    type Proc = RealProc;

    fn spawn(&self, cmd: Cmd) -> io::Result<RealProc> {
        let mut c = tokio::process::Command::new(&cmd.program);
        c.args(&cmd.args)
            .envs(cmd.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // 自成一个进程组，终止时连同它派生的进程一起终止。
            .process_group(0)
            .kill_on_drop(true);
        match cmd.cwd {
            Some(dir) => c.current_dir(dir),
            None => c.current_dir(std::env::var_os("HOME").unwrap_or_else(|| "/".into())),
        };
        let mut child = c.spawn()?;
        let pgid = child.id().map(|id| id as i32);
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        // 标准输入由单独的任务写入：命令不读输入时，不会卡住输出的转发。
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
            pgid,
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

    fn store_put(&self, key: &str, value: &[u8]) -> io::Result<()> {
        let path = self.path(key)?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, value)?;
        std::fs::rename(tmp, path)
    }
}

pub struct RealProc {
    child: Child,
    pgid: Option<i32>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    stdin: Option<mpsc::UnboundedSender<Vec<u8>>>,
    out_buf: Vec<u8>,
    err_buf: Vec<u8>,
}

/// 从一个可能已经关闭的管道读；已关闭时永远不返回。
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
                use std::os::unix::process::ExitStatusExt;
                return ProcEvent::Exited(match status {
                    Ok(s) => ExitStatus { code: s.code(), signal: s.signal() },
                    Err(_) => ExitStatus { code: None, signal: None },
                });
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
        // 发送端关闭后，写入任务写完已排队的数据就退出，管道随之关闭。
        self.stdin = None;
    }

    fn kill(&mut self) {
        if let Some(pgid) = self.pgid {
            let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), nix::sys::signal::Signal::SIGKILL);
        }
        let _ = self.child.start_kill();
    }
}
