//! 终端会话的持有进程（M7.2，R66）。
//!
//! 每个会话一个持有进程（`nexus session-hold`）：它打开伪终端、启动 shell、自己解析终端输出以保持当前屏幕，
//! 在 `<数据目录>/sessions/<会话名>.sock` 上等常驻程序连上来。常驻程序重启后重新连上，会话还在。
//!
//! 持有进程还定时把会话的屏幕和 shell 的工作目录记在 `<会话名>.state`（M11.5，R90）。设备重启或持有进程意外结束后，
//! 常驻程序下次启动时按它恢复会话：同名、同一个创建时间，在原来的工作目录里重新启动 shell，屏幕上是中断前的内容。
//! shell 正常退出或会话被结束时删掉它。
//!
//! 常驻程序和持有进程之间的协议（[`Up`]、[`Down`]）跨版本使用：升级后新的常驻程序要接管旧的持有进程。
//! 规则和节点间协议一样（docs/protocol.md）：只在末尾加新种类，已有的编码不变（`tests/holder_compat.rs`）。

use crate::RealPty;
use nexus_core_proto::Timestamp;
use nexus_sys::{AdoptedPty, ExitStatus, Pty, PtyEvent, Size};
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

/// 持有进程协议的主版本：不同时常驻程序不接管（让它照常运行，`session ls` 里看不到）。
pub const HOLDER_VERSION: u16 = 1;
/// 一帧最大 1 MiB。
const MAX_FRAME: usize = 1 << 20;
/// 常驻程序等持有进程就绪的时间。
const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// 持有进程发给常驻程序的。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Up {
    /// 连上后的第一条。紧接着是一条 `Output`：把终端重绘成当前屏幕的数据。
    Hello { version: u16, size: Size, created_at_ms: u64 },
    Output(Vec<u8>),
    /// shell 退出了，输出都已发完。之后持有进程退出。
    Exited { code: Option<i32>, signal: Option<i32> },
}

/// 常驻程序发给持有进程的。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Down {
    Write(Vec<u8>),
    Resize(Size),
    Kill,
}

/// 会话套接字所在的目录。
pub fn sessions_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("sessions")
}

fn socket_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.sock"))
}

fn pid_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.pid"))
}

fn state_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.state"))
}

/// 记下的会话状态的格式版本：不认识的版本不恢复。
const STATE_VERSION: u16 = 1;
/// 屏幕有变化时最多多久记一次。
const SAVE_EVERY: Duration = Duration::from_secs(1);

/// 为恢复会话记下的状态（`<会话名>.state`）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    pub version: u16,
    pub created_at_ms: u64,
    pub size: Size,
    /// shell 当时的工作目录（只在能查到的系统上有）。
    pub cwd: Option<PathBuf>,
    /// 把终端重绘成当时屏幕的数据。
    pub screen: Vec<u8>,
    /// 记下的时间（毫秒）。
    pub saved_at_ms: u64,
}

fn load_state(dir: &Path, name: &str) -> Option<Saved> {
    let bytes = std::fs::read(state_path(dir, name)).ok()?;
    let saved: Saved = postcard::from_bytes(&bytes).ok()?;
    (saved.version == STATE_VERSION).then_some(saved)
}

/// 先写临时文件再改名：中途断电也不会留下半个文件。
fn store_state(dir: &Path, name: &str, saved: &Saved) -> io::Result<()> {
    let path = state_path(dir, name);
    let tmp = dir.join(format!("{name}.state.tmp"));
    let bytes = postcard::to_allocvec(saved).map_err(bad)?;
    {
        let mut f = std::fs::OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(tmp, path)
}

/// shell 当前的工作目录。
#[allow(unused_variables)]
fn shell_cwd(pid: Option<u32>) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        return std::fs::read_link(format!("/proc/{}/cwd", pid?)).ok();
    }
    #[allow(unreachable_code)]
    None
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    let body = postcard::to_allocvec(msg).expect("内存中的值总能编码");
    let mut out = (body.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(&body);
    out
}

fn bad(what: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

async fn read_frame<T: for<'de> Deserialize<'de>, R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> io::Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(bad("帧太大"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    postcard::from_bytes(&body).map_err(|e| bad(format!("无法解码：{e}")))
}

fn read_frame_blocking<T: for<'de> Deserialize<'de>>(r: &mut impl Read) -> io::Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(bad("帧太大"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    postcard::from_bytes(&body).map_err(|e| bad(format!("无法解码：{e}")))
}

// ------------------------------------------------------------------ 持有进程

/// 持有进程的主体：`nexus session-hold` 调用，直到 shell 退出才返回。
/// `restore` 时按记下的状态恢复（见模块说明）。
pub fn run(data_dir: &Path, name: &str, size: Size, created_at: Timestamp, restore: bool) -> io::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(hold(data_dir, name, size, created_at, restore))
}

async fn hold(data_dir: &Path, name: &str, size: Size, created_at: Timestamp, restore: bool) -> io::Result<()> {
    let dir = sessions_dir(data_dir);
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let sock = socket_path(&dir, name);
    let pid = pid_path(&dir, name);
    let _ = std::fs::remove_file(&sock);
    let saved = if restore { load_state(&dir, name) } else { None };
    let cwd = saved.as_ref().and_then(|s| s.cwd.clone()).filter(|c| c.is_dir());
    let mut pty = RealPty::spawn_in(size, cwd.as_deref())?;
    let listener = tokio::net::UnixListener::bind(&sock)?;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
    std::fs::write(&pid, std::process::id().to_string())?;
    // 走到哪一步都要删掉这两个文件：常驻程序靠它们找会话。记下的状态只在会话真的结束时删（见下面 `ended`）。
    struct Cleanup(PathBuf, PathBuf, PathBuf, bool);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_file(&self.1);
            if self.3 {
                let _ = std::fs::remove_file(&self.2);
            }
        }
    }
    let mut _cleanup = Cleanup(sock, pid, state_path(&dir, name), true);

    // 只用来保持当前屏幕：它要发给客户端的东西（包括回答终端查询）由常驻程序那边的状态机做。
    let mut screen = nexus_term_proto::Session::new(size, u64::MAX);
    if let Some(saved) = &saved {
        // 先画中断前的屏幕（离开全屏程序的备用屏幕），再说明一句；新的 shell 的提示符接在下面。
        let _ = screen.handle(nexus_term_proto::Event::PtyOutput(saved.screen.clone()));
        let mut intro = Vec::new();
        if screen.screen().alternate_screen() {
            intro.extend_from_slice(b"\x1b[?1049l");
        }
        intro.extend_from_slice(b"\x1b[m\r\n");
        let place = cwd.as_ref().map(|c| format!("，工作目录 {}", c.display())).unwrap_or_default();
        intro.extend_from_slice(format!("[nexus] 会话已恢复：设备重启或会话的进程意外结束过，里面的程序没有保留，shell 重新启动{place}。\r\n").as_bytes());
        let _ = screen.handle(nexus_term_proto::Event::PtyOutput(intro));
    }
    let mut size = size.clamped();
    // 屏幕或尺寸变了，下次定时记下来。
    let mut dirty = true;
    let mut save = tokio::time::interval(SAVE_EVERY);
    save.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // 被要求结束（会话被 kill、数据目录没了）：shell 因此退出时会话真的结束。
    let mut killing = false;
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let (down_tx, mut down_rx) = mpsc::unbounded_channel::<(u64, Down)>();
    let mut conn: Option<(u64, tokio::net::unix::OwnedWriteHalf)> = None;
    let mut next_conn = 0u64;
    // 套接字文件被删掉了（数据目录被清掉了，比如测试结束）：没有常驻程序能再找到这个会话，结束它。
    let mut check = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            biased;
            _ = check.tick() => {
                if !_cleanup.0.exists() {
                    killing = true;
                    pty.kill();
                }
            }
            _ = save.tick() => {
                if dirty {
                    let state = Saved {
                        version: STATE_VERSION,
                        created_at_ms: created_at.0,
                        size,
                        cwd: shell_cwd(pty.pid()),
                        screen: screen.restore(),
                        saved_at_ms: now_ms(),
                    };
                    if store_state(&dir, name, &state).is_ok() {
                        dirty = false;
                    }
                }
            }
            // 关机时系统先发 SIGTERM：留着记下的状态，下次恢复。
            _ = term.recv() => {
                _cleanup.3 = false;
                return Ok(());
            }
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                // 新连上的常驻程序取代旧的（旧的多半已经不在了）。
                let (mut rd, mut wr) = stream.into_split();
                next_conn += 1;
                let id = next_conn;
                let hello = Up::Hello { version: HOLDER_VERSION, size, created_at_ms: created_at.0 };
                let restore = Up::Output(screen.restore());
                if wr.write_all(&encode(&hello)).await.is_err() || wr.write_all(&encode(&restore)).await.is_err() {
                    continue;
                }
                conn = Some((id, wr));
                let tx = down_tx.clone();
                tokio::spawn(async move {
                    while let Ok(msg) = read_frame::<Down, _>(&mut rd).await {
                        if tx.send((id, msg)).is_err() {
                            return;
                        }
                    }
                });
            }
            Some((id, msg)) = down_rx.recv() => {
                if conn.as_ref().map(|(c, _)| *c) != Some(id) {
                    continue;
                }
                match msg {
                    Down::Write(bytes) => pty.write(bytes),
                    Down::Resize(s) => {
                        size = s.clamped();
                        screen.set_size(size);
                        pty.resize(size);
                        dirty = true;
                    }
                    Down::Kill => {
                        killing = true;
                        pty.kill();
                    }
                }
            }
            event = pty.next_event() => match event {
                PtyEvent::Output(bytes) => {
                    let _ = screen.handle(nexus_term_proto::Event::PtyOutput(bytes.clone()));
                    dirty = true;
                    if let Some((_, wr)) = conn.as_mut() {
                        if wr.write_all(&encode(&Up::Output(bytes))).await.is_err() {
                            conn = None;
                        }
                    }
                }
                PtyEvent::Exited(status) => {
                    // shell 自己退出、或者是我们结束的：会话结束。被别人用信号结束的（关机时）：留着状态，下次恢复。
                    _cleanup.3 = killing || status.signal.is_none();
                    if let Some((_, mut wr)) = conn.take() {
                        let _ = wr.write_all(&encode(&Up::Exited { code: status.code, signal: status.signal })).await;
                        let _ = wr.shutdown().await;
                    }
                    return Ok(());
                }
            },
        }
    }
}

// ------------------------------------------------------------------ 常驻程序这一边

/// 由持有进程管理的终端。丢弃它只断开连接，会话照常运行。
pub struct HeldPty {
    events: mpsc::UnboundedReceiver<PtyEvent>,
    down: mpsc::UnboundedSender<Down>,
    exited: Option<ExitStatus>,
}

impl HeldPty {
    /// 连上持有进程，读出 `Hello`（阻塞，限时）。
    fn connect(sock: &Path) -> io::Result<(Self, Up)> {
        let mut s = std::os::unix::net::UnixStream::connect(sock)?;
        s.set_read_timeout(Some(READY_TIMEOUT))?;
        let hello: Up = read_frame_blocking(&mut s)?;
        match hello {
            Up::Hello { version, .. } if version == HOLDER_VERSION => {}
            Up::Hello { version, .. } => return Err(bad(format!("持有进程的协议版本 {version}，本机 {HOLDER_VERSION}"))),
            _ => return Err(bad("持有进程没有先发 Hello")),
        }
        s.set_read_timeout(None)?;
        s.set_nonblocking(true)?;
        let stream = tokio::net::UnixStream::from_std(s)?;
        let (mut rd, mut wr) = stream.into_split();
        let (ev_tx, events) = mpsc::unbounded_channel();
        let (down, mut down_rx) = mpsc::unbounded_channel::<Down>();
        tokio::spawn(async move {
            loop {
                match read_frame::<Up, _>(&mut rd).await {
                    Ok(Up::Output(b)) => {
                        if ev_tx.send(PtyEvent::Output(b)).is_err() {
                            return;
                        }
                    }
                    Ok(Up::Exited { code, signal }) => {
                        let _ = ev_tx.send(PtyEvent::Exited(ExitStatus { code, signal }));
                        return;
                    }
                    Ok(Up::Hello { .. }) => {}
                    // 持有进程不在了（被结束了）：当作程序退出。
                    Err(_) => {
                        let _ = ev_tx.send(PtyEvent::Exited(ExitStatus { code: None, signal: None }));
                        return;
                    }
                }
            }
        });
        tokio::spawn(async move {
            while let Some(msg) = down_rx.recv().await {
                if wr.write_all(&encode(&msg)).await.is_err() {
                    return;
                }
            }
        });
        Ok((Self { events, down, exited: None }, hello))
    }
}

impl Pty for HeldPty {
    async fn next_event(&mut self) -> PtyEvent {
        if let Some(status) = self.exited {
            return PtyEvent::Exited(status);
        }
        match self.events.recv().await {
            Some(PtyEvent::Exited(status)) => {
                self.exited = Some(status);
                PtyEvent::Exited(status)
            }
            Some(ev) => ev,
            None => {
                let status = ExitStatus { code: None, signal: None };
                self.exited = Some(status);
                PtyEvent::Exited(status)
            }
        }
    }

    fn write(&mut self, data: Vec<u8>) {
        let _ = self.down.send(Down::Write(data));
    }

    fn resize(&mut self, size: Size) {
        let _ = self.down.send(Down::Resize(size));
    }

    fn kill(&mut self) {
        let _ = self.down.send(Down::Kill);
    }
}

/// 启动一个持有进程并连上它。`exe` 是 `nexus` 二进制。
/// `restore` 时持有进程按记下的状态恢复会话。
pub fn spawn(exe: &Path, data_dir: &Path, name: &str, size: Size, created_at: Timestamp, restore: bool) -> io::Result<HeldPty> {
    use std::os::unix::process::CommandExt;
    let dir = sessions_dir(data_dir);
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let sock = socket_path(&dir, name);
    if sock.exists() {
        // 上一次留下的：还活着就是同名的会话已经在了。
        if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("会话 {name} 的持有进程已经在运行")));
        }
        let _ = std::fs::remove_file(&sock);
    }
    let log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(format!("{name}.log")))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--data-dir")
        .arg(data_dir)
        .arg("session-hold")
        .args(["--name", name, "--cols", &size.cols.to_string(), "--rows", &size.rows.to_string()])
        .args(["--created-at-ms", &created_at.0.to_string()])
        .args(if restore { &["--restore"][..] } else { &[] })
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // 自成一个进程组：常驻程序在前台被 Ctrl-C、或者被结束时，持有进程不受影响。
    cmd.process_group(0);
    let mut child = cmd.spawn()?;
    let deadline = std::time::Instant::now() + READY_TIMEOUT;
    loop {
        if sock.exists() {
            if let Ok((pty, _)) = HeldPty::connect(&sock) {
                // 持有进程由自己的进程会话管理；这里不等它（它退出时由系统回收：见下面的线程）。
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ok(pty);
            }
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(io::Error::other(format!("持有进程启动失败（{status}），日志在 {}", dir.join(format!("{name}.log")).display())));
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            return Err(io::Error::new(io::ErrorKind::TimedOut, "持有进程没有就绪"));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// 上一次运行留下的会话：还活着的接管；连不上的套接字（持有进程已经不在了）删掉。
/// 持有进程不在了、但记下了状态的（设备重启过，或持有进程意外结束），用 `exe` 启动新的持有进程恢复它。
pub fn adopt(exe: &Path, data_dir: &Path) -> Vec<AdoptedPty<HeldPty>> {
    let mut out = adopt_live(data_dir);
    let dir = sessions_dir(data_dir);
    let Ok(entries) = std::fs::read_dir(&dir) else { return out };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".state")).map(str::to_string) else {
            continue;
        };
        if out.iter().any(|a| a.name == name) || socket_path(&dir, &name).exists() {
            continue;
        }
        let Some(saved) = load_state(&dir, &name) else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        let created_at = Timestamp(saved.created_at_ms);
        match spawn(exe, data_dir, &name, saved.size, created_at, true) {
            Ok(pty) => out.push(AdoptedPty { name, size: saved.size, created_at, pty }),
            Err(e) => eprintln!("nexus daemon: 恢复会话 {name} 失败：{e}"),
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn adopt_live(data_dir: &Path) -> Vec<AdoptedPty<HeldPty>> {
    let dir = sessions_dir(data_dir);
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".sock")).map(str::to_string) else {
            continue;
        };
        match HeldPty::connect(&path) {
            Ok((pty, Up::Hello { size, created_at_ms, .. })) => {
                out.push(AdoptedPty { name, size, created_at: Timestamp(created_at_ms), pty });
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused || e.kind() == io::ErrorKind::NotFound => {
                let _ = std::fs::remove_file(&path);
                let _ = std::fs::remove_file(pid_path(&dir, &name));
            }
            // 别的错误（例如协议版本不同）：留着它，不接管。
            Err(_) => {}
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// 写一个帧，测试用。
#[doc(hidden)]
pub fn encode_frame<T: Serialize>(msg: &T) -> Vec<u8> {
    encode(msg)
}

#[allow(dead_code)]
fn _assert_write(_: &mut impl Write) {}
