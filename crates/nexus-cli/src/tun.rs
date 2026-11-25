//! 完整节点的虚拟网卡（M3.4）：在网卡上运行用户态协议栈，TCP 连接、UDP 数据报（M11.5）和发给名字服务的查询交给节点。
//! 网卡的地址只覆盖完整节点的网段：只有发往本机临时地址（以及名字服务）的包会进来。

use nexus_net_core::fakeip::FakePool;
use nexus_runtime::TunInbound;
use std::io;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::{mpsc, oneshot};

/// 建网卡、启动协议栈。网卡在常驻程序退出时随之消失。
#[cfg(target_os = "android")]
pub fn start(_: &str, _: &FakePool, _: mpsc::UnboundedSender<TunInbound>) -> io::Result<()> {
    Err(io::Error::other("Android 上由 Nexus 应用的 VPN 建网卡，不用 --tun"))
}

/// 建网卡、启动协议栈。网卡在常驻程序退出时随之消失。
#[cfg(not(target_os = "android"))]
pub fn start(name: &str, pool: &FakePool, entry: mpsc::UnboundedSender<TunInbound>) -> io::Result<()> {
    let (_, prefix) = pool.network();
    let dev = tun_rs::DeviceBuilder::new()
        .name(name)
        .ipv4(pool.gateway(), prefix, None)
        .mtu(1500)
        .build_async()?;
    run_stack(dev, pool, entry)?;
    configure_resolver(name, pool);
    Ok(())
}

/// 在别人建好的网卡上启动协议栈（Android：VpnService 建网卡、配地址和路由）。
pub fn run_stack(dev: tun_rs::AsyncDevice, pool: &FakePool, entry: mpsc::UnboundedSender<TunInbound>) -> io::Result<()> {
    let mut config = ipstack::IpStackConfig::default();
    config.mtu(1500).map_err(io::Error::other)?;
    config.udp_timeout(Duration::from_secs(30));
    let mut stack = ipstack::IpStack::new(config, PacketPipe::new(dev));
    let dns = std::net::IpAddr::V4(pool.dns());
    tokio::spawn(async move {
        loop {
            match stack.accept().await {
                Ok(ipstack::IpStackStream::Tcp(tcp)) => {
                    let dst = tcp.peer_addr();
                    if entry.send(TunInbound::Tcp { stream: Box::new(TunTcp::new(tcp)), dst }).is_err() {
                        return;
                    }
                }
                Ok(ipstack::IpStackStream::Udp(udp)) if udp.peer_addr().ip() == dns && udp.peer_addr().port() == 53 => {
                    tokio::spawn(serve_dns(udp, entry.clone()));
                }
                // 其余的 UDP（M11.5）：交给节点按目标转发（只转发到 UDP 服务）。
                Ok(ipstack::IpStackStream::Udp(udp)) => {
                    let dst = udp.peer_addr();
                    if entry.send(TunInbound::Udp { stream: Box::new(udp), dst }).is_err() {
                        return;
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("nexus daemon: 虚拟网卡的协议栈停止了：{e}");
                    return;
                }
            }
        }
    });
    Ok(())
}

/// 一个查询方（源端口）发来的查询：每个数据报是一个查询，逐个交给节点回答。
async fn serve_dns(mut udp: ipstack::IpStackUdpStream, entry: mpsc::UnboundedSender<TunInbound>) {
    let mut buf = vec![0u8; 1500];
    loop {
        let n = match udp.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        let (reply, answer) = oneshot::channel();
        if entry.send(TunInbound::Dns { query: buf[..n].to_vec(), reply }).is_err() {
            return;
        }
        let Ok(a) = answer.await else { return };
        if !a.is_empty() && udp.write_all(&a).await.is_err() {
            return;
        }
    }
}

/// 让系统把 .internal 交给名字服务。有 systemd-resolved 时自动配置；否则在日志里说明。
#[cfg(not(target_os = "android"))]
fn configure_resolver(name: &str, pool: &FakePool) {
    let dns = pool.dns().to_string();
    let ok = std::process::Command::new("resolvectl")
        .args(["dns", name, &dns])
        .status()
        .and_then(|s| if s.success() { std::process::Command::new("resolvectl").args(["domain", name, "~internal"]).status() } else { Ok(s) })
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        eprintln!("nexus daemon: 完整节点 {name}：.internal 的名字交给 {dns}（systemd-resolved）");
    } else {
        eprintln!(
            "nexus daemon: 完整节点 {name}：名字服务在 {dns}。系统没有 systemd-resolved，请把 .internal 的查询交给它\
             （例如 dnsmasq 的 server=/internal/{dns}）"
        );
    }
}

/// 协议栈上的一条 TCP 连接。关闭写方向时，协议栈要等发出的数据都被确认才发 FIN，
/// 但确认到达时不一定唤醒等待关闭的一方；这里定时再试，最多等 `SHUTDOWN_LIMIT`，之后当作已关闭。
struct TunTcp {
    inner: ipstack::IpStackTcpStream,
    tick: Pin<Box<tokio::time::Sleep>>,
    deadline: Option<tokio::time::Instant>,
}

const SHUTDOWN_RETRY: Duration = Duration::from_millis(20);
const SHUTDOWN_LIMIT: Duration = Duration::from_secs(5);

impl TunTcp {
    fn new(inner: ipstack::IpStackTcpStream) -> Self {
        Self { inner, tick: Box::pin(tokio::time::sleep(SHUTDOWN_RETRY)), deadline: None }
    }
}

impl AsyncRead for TunTcp {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for TunTcp {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        if let Poll::Ready(r) = Pin::new(&mut this.inner).poll_shutdown(cx) {
            return Poll::Ready(r);
        }
        let now = tokio::time::Instant::now();
        let deadline = *this.deadline.get_or_insert(now + SHUTDOWN_LIMIT);
        if now >= deadline {
            return Poll::Ready(Ok(()));
        }
        if this.tick.as_mut().poll(cx).is_ready() {
            this.tick.as_mut().reset(now + SHUTDOWN_RETRY);
            cx.waker().wake_by_ref();
        }
        Poll::Pending
    }
}

/// 一个包一次读写的适配：协议栈每次读到的就是一个 IP 包。
struct PacketPipe {
    rx: mpsc::Receiver<Vec<u8>>,
    tx: mpsc::UnboundedSender<Vec<u8>>,
}

impl PacketPipe {
    fn new(dev: tun_rs::AsyncDevice) -> Self {
        let dev = Arc::new(dev);
        let (in_tx, rx) = mpsc::channel(1024);
        let (tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let d = dev.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            while let Ok(n) = d.recv(&mut buf).await {
                if in_tx.send(buf[..n].to_vec()).await.is_err() {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            while let Some(p) = out_rx.recv().await {
                if dev.send(&p).await.is_err() {
                    break;
                }
            }
        });
        Self { rx, tx }
    }
}

impl AsyncRead for PacketPipe {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(p)) => {
                buf.put_slice(&p);
                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for PacketPipe {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let _ = self.tx.send(buf.to_vec());
        Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
