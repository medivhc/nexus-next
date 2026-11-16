//! 在虚拟网卡上跑用户态协议栈：内核里的程序连 10.77.0.2:<端口>，协议栈接住连接并回声（或吞掉数据），
//! 测往返延迟和单连接吞吐。需要 CAP_NET_ADMIN。
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dev = tun_rs::DeviceBuilder::new()
        .name("nxprobe0")
        .ipv4("10.77.0.1", 24, None)
        .mtu(1500)
        .build_async()?;
    let mut config = ipstack::IpStackConfig::default();
    config.mtu(1500)?;
    let mut stack = ipstack::IpStack::new(config, PacketPipe::new(dev));

    tokio::spawn(async move {
        loop {
            match stack.accept().await {
                Ok(ipstack::IpStackStream::Tcp(mut tcp)) => {
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 64 * 1024];
                        loop {
                            let n = match tcp.read(&mut buf).await { Ok(0) | Err(_) => break, Ok(n) => n };
                            // 小包回声（测延迟），大块只计数（测吞吐）。
                            if n <= 64 && tcp.write_all(&buf[..n]).await.is_err() { break; }
                        }
                    });
                }
                Ok(other) => drop(other),
                Err(e) => { eprintln!("accept: {e}"); break; }
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    // 往返延迟
    let mut s = tokio::net::TcpStream::connect("10.77.0.2:7000").await?;
    s.set_nodelay(true)?;
    let mut rtts = Vec::new();
    for _ in 0..200 {
        let t = Instant::now();
        s.write_all(b"ping").await?;
        let mut b = [0u8; 4];
        s.read_exact(&mut b).await?;
        rtts.push(t.elapsed());
    }
    rtts.sort();
    println!("rtt p50={:?} p99={:?}", rtts[100], rtts[198]);

    // 吞吐：写 256 MiB
    let mut s = tokio::net::TcpStream::connect("10.77.0.2:7001").await?;
    let chunk = vec![7u8; 1 << 20];
    let t = Instant::now();
    for _ in 0..256 { s.write_all(&chunk).await?; }
    s.shutdown().await?;
    let secs = t.elapsed().as_secs_f64();
    println!("throughput {:.0} MiB/s", 256.0 / secs);
    Ok(())
}

/// 一个包一次读写的适配：协议栈读到的每一段就是一个 IP 包。
struct PacketPipe {
    rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
}

impl PacketPipe {
    fn new(dev: tun_rs::AsyncDevice) -> Self {
        let dev = std::sync::Arc::new(dev);
        let (in_tx, rx) = tokio::sync::mpsc::channel(1024);
        let (tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let d = dev.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            while let Ok(n) = d.recv(&mut buf).await {
                if in_tx.send(buf[..n].to_vec()).await.is_err() { break; }
            }
        });
        tokio::spawn(async move {
            while let Some(p) = out_rx.recv().await {
                if dev.send(&p).await.is_err() { break; }
            }
        });
        Self { rx, tx }
    }
}

impl tokio::io::AsyncRead for PacketPipe {
    fn poll_read(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> std::task::Poll<std::io::Result<()>> {
        match self.rx.poll_recv(cx) {
            std::task::Poll::Ready(Some(p)) => { buf.put_slice(&p); std::task::Poll::Ready(Ok(())) }
            std::task::Poll::Ready(None) => std::task::Poll::Ready(Ok(())),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

impl tokio::io::AsyncWrite for PacketPipe {
    fn poll_write(self: std::pin::Pin<&mut Self>, _cx: &mut std::task::Context<'_>, buf: &[u8]) -> std::task::Poll<std::io::Result<usize>> {
        let _ = self.tx.send(buf.to_vec());
        std::task::Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(self: std::pin::Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> { std::task::Poll::Ready(Ok(())) }
    fn poll_shutdown(self: std::pin::Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> { std::task::Poll::Ready(Ok(())) }
}
