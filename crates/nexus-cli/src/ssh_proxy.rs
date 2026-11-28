//! `nexus ssh-proxy`：在标准输入输出上充当 SSH 服务端，供本机 ssh 的 ProxyCommand 使用。
//!
//! SSH 这一层只在本机的 ssh 和这个进程之间运行，不做认证（接受 none）；
//! 身份和授权由 Nexus 承担。各类 SSH 请求翻译成对目标设备的操作：
//!
//! | SSH 请求          | 翻译成                                   |
//! | ----------------- | ---------------------------------------- |
//! | shell（带终端）   | 连上常驻会话 main，和 `nexus ssh` 相同    |
//! | exec              | 在目标设备上 `sh -c <命令>`               |
//! | sftp 子系统       | 在目标设备上运行系统自带的 sftp-server    |
//! | 窗口变化          | 会话的尺寸变化                            |

use crate::UnixCtl;
use nexus_cli::{ssh, Ctl, SshArgs};
use nexus_wire::{CtlRequest, CtlResponse, ErrorCode, ExecEvent, ExecSpec, Input, Size, WireError};
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::PrivateKey;
use russh::server::{Auth, ChannelOpenHandle, Msg, Session};
use russh::{Channel, ChannelId, ChannelMsg, ChannelReadHalf, ChannelWriteHalf};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

/// 远端没有 sftp-server 时给出明确的提示，而不是让 scp 莫名失败。
const SFTP_SERVER: &str = "for p in /usr/lib/openssh/sftp-server /usr/libexec/openssh/sftp-server /usr/libexec/sftp-server \
     /usr/lib/ssh/sftp-server /usr/lib/sftp-server /opt/homebrew/libexec/sftp-server /usr/local/libexec/sftp-server; \
     do [ -x \"$p\" ] && exec \"$p\"; done; \
     echo 'nexus: 目标设备上没有找到 sftp-server（通常随 OpenSSH 服务端安装）；可以改用 scp -O' >&2; exit 127";

fn other(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

/// 运行代理，返回进程退出码。`host` 是 ssh 的 `%h`，形如 `home.nexus`。
pub async fn run(ctl: UnixCtl, host: &str) -> i32 {
    let node = host.strip_suffix(".nexus").unwrap_or(host).to_string();
    let seed = match host_key(&ctl, &node).await {
        Ok(seed) => seed,
        Err(message) => {
            eprintln!("nexus ssh-proxy: {message}");
            return 255;
        }
    };
    let config = russh::server::Config {
        keys: vec![PrivateKey::from(Ed25519Keypair::from_seed(&seed))],
        auth_rejection_time: std::time::Duration::ZERO,
        auth_rejection_time_initial: Some(std::time::Duration::ZERO),
        inactivity_timeout: None,
        ..Default::default()
    };
    let handler = Proxy {
        ctl: Arc::new(ctl),
        node,
        channels: HashMap::new(),
        ptys: HashMap::new(),
    };
    let stream = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
    let session = match russh::server::run_stream(Arc::new(config), stream, handler).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("nexus ssh-proxy: {e}");
            return 255;
        }
    };
    match session.await {
        Ok(()) => 0,
        // 本机 ssh 断开时会话以错误结束，这是正常的结束方式。
        Err(_) => 0,
    }
}

async fn host_key(ctl: &UnixCtl, node: &str) -> Result<[u8; 32], String> {
    let mut session = ctl
        .open(CtlRequest::SshHostKey {
            node: node.to_string(),
        })
        .await
        .map_err(|e| format!("连不上本机的常驻程序（{e}），先运行 nexus daemon"))?;
    match session.rx.recv().await {
        Some(CtlResponse::SshHostKey(seed)) => Ok(seed),
        Some(CtlResponse::Error(e)) => Err(e.message),
        _ => Err("常驻程序的回应无法理解".into()),
    }
}

struct Proxy {
    ctl: Arc<UnixCtl>,
    node: String,
    channels: HashMap<ChannelId, Channel<Msg>>,
    ptys: HashMap<ChannelId, Size>,
}

impl Proxy {
    /// 取出通道交给一个独立的任务处理之后的所有数据。
    fn start(
        &mut self,
        id: ChannelId,
        session: &mut Session,
        work: Work,
    ) -> Result<(), russh::Error> {
        let Some(channel) = self.channels.remove(&id) else {
            session.channel_failure(id)?;
            return Ok(());
        };
        session.channel_success(id)?;
        let ctl = self.ctl.clone();
        let node = self.node.clone();
        let size = self.ptys.get(&id).copied();
        tokio::spawn(async move {
            let (read, write) = channel.split();
            match work {
                Work::Shell => shell(&ctl, node, read, write, size.unwrap_or(Size::DEFAULT)).await,
                Work::Exec(spec) => exec(&ctl, node, read, write, spec).await,
            }
        });
        Ok(())
    }
}

/// ssh 在标准输入不是终端时（例如 `ssh -tt` 接管道）报告 0x0，这时用默认尺寸。
fn pty_size(cols: u32, rows: u32) -> Size {
    if cols == 0 || rows == 0 {
        return Size::DEFAULT;
    }
    Size {
        cols: cols.min(u16::MAX as u32) as u16,
        rows: rows.min(u16::MAX as u32) as u16,
    }
}

enum Work {
    Shell,
    Exec(ExecSpec),
}

fn shell_command(command: String) -> ExecSpec {
    ExecSpec {
        program: "sh".into(),
        args: vec!["-c".into(), command],
        cwd: None,
        env: vec![],
        timeout_ms: None,
        // 文件传输可能很大，不设上限。
        max_output: u64::MAX,
    }
}

impl russh::server::Handler for Proxy {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        cols: u32,
        rows: u32,
        _pw: u32,
        _ph: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.ptys.insert(channel, pty_size(cols, rows));
        session.channel_success(channel)
    }

    async fn env_request(
        &mut self,
        channel: ChannelId,
        _: &str,
        _: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // 环境变量不传递：远端程序的环境由目标设备决定。
        session.channel_failure(channel)
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.start(channel, session, Work::Shell)
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).into_owned();
        self.start(channel, session, Work::Exec(shell_command(command)))
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name != "sftp" {
            return session.channel_failure(channel);
        }
        self.start(
            channel,
            session,
            Work::Exec(shell_command(SFTP_SERVER.into())),
        )
    }
}

/// 把通道上收到的消息转到一个不限长的队列里。这样即使正在等待发送，
/// SSH 会话也不会因为通道的消息没人取而停下（那会让发送永远等不到窗口）。
fn pump_reads(mut read: ChannelReadHalf) -> mpsc::UnboundedReceiver<ChannelMsg> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(msg) = read.wait().await {
            if tx.send(msg).is_err() {
                break;
            }
        }
    });
    rx
}

/// SSH 的退出状态：Nexus 自身的错误用 255，与 ssh 的习惯一致。
async fn finish(write: &ChannelWriteHalf<Msg>, code: u32) {
    let _ = write.exit_status(code).await;
    let _ = write.eof().await;
    let _ = write.close().await;
}

async fn report(write: &ChannelWriteHalf<Msg>, e: &WireError) {
    let line = format!("nexus: {}\n", e.message);
    let _ = write.extended_data(1, line.as_bytes()).await;
    finish(write, 255).await;
}

async fn exec(
    ctl: &UnixCtl,
    node: String,
    read: ChannelReadHalf,
    write: ChannelWriteHalf<Msg>,
    spec: ExecSpec,
) {
    let mut incoming = pump_reads(read);
    let mut session = match ctl.open(CtlRequest::Exec { node, spec }).await {
        Ok(s) => s,
        Err(e) => {
            return report(
                &write,
                &WireError::new(ErrorCode::Internal, format!("连不上本机的常驻程序：{e}")),
            )
            .await
        }
    };
    // 发给常驻程序的消息先排队，有空位再发，与 `nexus exec` 相同的理由。
    let mut outbox: VecDeque<Input> = VecDeque::new();
    let mut input_open = true;
    loop {
        tokio::select! {
            biased;
            resp = session.rx.recv() => match resp {
                Some(CtlResponse::Exec(ExecEvent::Stdout(b))) => {
                    if write.data(&b[..]).await.is_err() {
                        outbox.push_back(Input::Cancel);
                    }
                }
                Some(CtlResponse::Exec(ExecEvent::Stderr(b))) => {
                    let _ = write.extended_data(1, &b[..]).await;
                }
                Some(CtlResponse::Exec(ExecEvent::Exit(r))) => {
                    let code = match (r.exit_code, r.signal) {
                        (Some(c), _) => c as u32,
                        (None, Some(sig)) => 128 + sig as u32,
                        (None, None) => 255,
                    };
                    return finish(&write, code).await;
                }
                Some(CtlResponse::Error(e)) => return report(&write, &e).await,
                Some(_) => return report(&write, &WireError::new(ErrorCode::Internal, "常驻程序的回应无法理解")).await,
                None => return finish(&write, 255).await,
            },
            permit = session.tx.reserve(), if !outbox.is_empty() => match permit {
                Ok(p) => p.send(outbox.pop_front().expect("队列非空")),
                Err(_) => return finish(&write, 255).await,
            },
            msg = incoming.recv(), if input_open && outbox.len() < 64 => match msg {
                Some(ChannelMsg::Data { data }) => outbox.push_back(Input::Stdin(data.to_vec())),
                Some(ChannelMsg::Eof) => outbox.push_back(Input::CloseStdin),
                Some(ChannelMsg::Close) | None => {
                    input_open = false;
                    outbox.push_back(Input::Cancel);
                }
                Some(_) => {}
            },
        }
    }
}

async fn shell(
    ctl: &UnixCtl,
    node: String,
    read: ChannelReadHalf,
    write: ChannelWriteHalf<Msg>,
    size: Size,
) {
    let mut incoming = pump_reads(read);
    // 用和 `nexus ssh` 完全相同的客户端代码：通道的数据当作键盘输入，通道当作终端。
    let (mut keys_in, mut keys_out) = tokio::io::duplex(64 * 1024);
    let (resize_tx, mut resizes) = mpsc::channel(4);
    let forward = async move {
        while let Some(msg) = incoming.recv().await {
            match msg {
                ChannelMsg::Data { data } => {
                    if keys_in.write_all(&data).await.is_err() {
                        break;
                    }
                }
                ChannelMsg::WindowChange {
                    col_width,
                    row_height,
                    ..
                } => {
                    let _ = resize_tx.send(pty_size(col_width, row_height)).await;
                }
                // 输入结束或通道关闭：断开会话（会话继续运行）。
                ChannelMsg::Eof | ChannelMsg::Close => break,
                _ => {}
            }
        }
        drop(keys_in);
    };
    let args = SshArgs {
        node,
        session: "main".into(),
        size,
        size_given: true,
    };
    let mut screen = write.make_writer();
    let client = async {
        let result = ssh(ctl, &args, &mut keys_out, &mut screen, &mut resizes).await;
        let _ = screen.flush().await;
        result
    };
    tokio::pin!(forward, client);
    // 会话结束（shell 退出）时不必等本机 ssh 的输入结束；输入先结束时，客户端随之断开。
    let result = tokio::select! {
        r = &mut client => r,
        _ = &mut forward => (&mut client).await,
    };
    match result {
        Ok(code) => finish(&write, code as u32).await,
        Err(nexus_cli::SshError::Remote(e)) => report(&write, &e).await,
        Err(nexus_cli::SshError::Daemon(e)) => {
            report(
                &write,
                &WireError::new(ErrorCode::Internal, other(e).to_string()),
            )
            .await
        }
    }
}
