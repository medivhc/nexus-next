//! Forward datagrams captured by the TUN to a private service or a selected exit.

use super::{
    datagrams, drain, err, Node, OpenError, ProxyFail, ProxyStream, TunRouting,
    PROXY_HANDSHAKE_TIMEOUT,
};
use nexus_fabric::{write_response, Connection, Transport};
use nexus_net_core::route::{self, Decision, Outbound};
use nexus_net_core::{socks5, Host, Target};
use nexus_sys::{System, UdpConn};
use nexus_wire::{ErrorCode, Request, Response, WireError};
use std::io;
use std::net::SocketAddr;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

enum UdpOutbound<Tcp, Remote> {
    Direct(UdpConn),
    Nexus(Remote),
    Socks5 {
        control: Tcp,
        relay: UdpConn,
        header: Vec<u8>,
    },
}

impl<T: Transport, S: System> Node<T, S> {
    pub(super) async fn serve_tun_udp(
        &self,
        client: ProxyStream,
        dst: SocketAddr,
        routing: TunRouting,
    ) {
        let Some(target) = self.tun_target(dst, &routing) else {
            return;
        };
        let direct_allowed = matches!(routing, TunRouting::Application { .. });
        let decision = self.tun_decision(&target, routing);
        let result = match self
            .open_udp_outbound(&target, decision, direct_allowed)
            .await
        {
            Ok(outbound) => outbound
                .forward(client, &target)
                .await
                .map_err(|e| ProxyFail::Unreachable(e.to_string())),
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            eprintln!("nexus daemon: UDP {target}：{e}");
        }
    }

    async fn open_udp_outbound(
        &self,
        target: &Target,
        decision: Decision,
        direct_allowed: bool,
    ) -> Result<UdpOutbound<S::Tcp, <T::Conn as Connection>::Stream>, ProxyFail> {
        let outbound = match decision {
            Decision::Service { device, service } => {
                return self.open_udp_service(&device, service).await
            }
            Decision::Rule { outbound, .. } | Decision::Default(outbound) => outbound,
            Decision::BadInternalName => return Err(ProxyFail::Rejected),
        };
        match outbound {
            Outbound::Exit(device) => {
                let body = Request::ExitDatagram {
                    target: target.to_string(),
                };
                self.open_exit_stream(&device, body)
                    .await
                    .map(UdpOutbound::Nexus)
            }
            Outbound::Socks5(upstream) => tokio::time::timeout(
                PROXY_HANDSHAKE_TIMEOUT,
                self.open_udp_socks(&upstream, target),
            )
            .await
            .map_err(|_| ProxyFail::Unreachable(format!("上游代理 {upstream} 的 UDP 握手超时")))?
            .map_err(|e| {
                ProxyFail::Unreachable(format!("上游代理 {upstream} 的 UDP 转发失败：{e}"))
            }),
            Outbound::Direct if direct_allowed => {
                let ips = match &target.host {
                    Host::Ip(ip) => vec![*ip],
                    Host::Name(name) => self
                        .system
                        .resolve(name)
                        .await
                        .map_err(|e| ProxyFail::Unreachable(format!("解析不了 {name}：{e}")))?,
                };
                let mut last = format!("{target} 没有地址");
                for ip in ips {
                    match self
                        .system
                        .udp_connect(SocketAddr::new(ip, target.port))
                        .await
                    {
                        Ok(udp) => return Ok(UdpOutbound::Direct(udp)),
                        Err(e) => last = format!("打不开放往 {target} 的 UDP 套接字：{e}"),
                    }
                }
                Err(ProxyFail::Refused(last))
            }
            // A cached Fake IP outlives a rule change. Legacy capture does not
            // send that packet directly if its proxy rule was removed.
            _ => Err(ProxyFail::Rejected),
        }
    }

    async fn open_udp_service(
        &self,
        device: &str,
        name: String,
    ) -> Result<UdpOutbound<S::Tcp, <T::Conn as Connection>::Stream>, ProxyFail> {
        let device = self
            .resolve(device)
            .map_err(|e| ProxyFail::Unreachable(e.message))?;
        if device.node_id == self.me() {
            let addr = self
                .services
                .borrow()
                .iter()
                .find(|s| s.name == name && s.proto == "udp")
                .map(|s| s.addr)
                .ok_or_else(|| ProxyFail::Unreachable(format!("本机没有发布 UDP 服务 {name}")))?;
            return self
                .system
                .udp_connect(addr)
                .await
                .map(UdpOutbound::Direct)
                .map_err(|e| {
                    ProxyFail::Unreachable(format!("服务 {name} 的 UDP 套接字打不开：{e}"))
                });
        }
        let body = Request::SvcDatagram { name };
        for attempt in 0..2 {
            match self.open_stream(&device, body.clone(), None).await {
                Ok(stream) => return Ok(UdpOutbound::Nexus(stream)),
                Err(OpenError::StaleRoster) if attempt == 0 => {
                    let _ = self.push_roster(device.node_id).await;
                }
                Err(OpenError::StaleRoster) => break,
                Err(OpenError::Remote(e)) => return Err(ProxyFail::Unreachable(e.message)),
                Err(OpenError::Io(e)) => {
                    return Err(ProxyFail::Unreachable(format!(
                        "连不上 {}：{e}",
                        device.name
                    )))
                }
            }
        }
        Err(ProxyFail::Unreachable("对方的名册始终比本机旧".into()))
    }

    async fn open_udp_socks(
        &self,
        upstream: &str,
        target: &Target,
    ) -> io::Result<UdpOutbound<S::Tcp, <T::Conn as Connection>::Stream>> {
        let header = socks5::udp_header(target).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "SOCKS5 目标名长度不合法")
        })?;
        let (mut control, peer_ip) = self
            .connect_socks_upstream(upstream)
            .await
            .map_err(|e| io::Error::other(e.to_string()))?;
        let bound = socks_associate(&mut control).await?;
        if bound.port == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "SOCKS5 返回了无效的 UDP 中继端口",
            ));
        }
        let ips = match &bound.host {
            Host::Ip(ip) if ip.is_unspecified() => vec![peer_ip],
            Host::Ip(ip) => vec![*ip],
            Host::Name(name) => {
                if is_internal(&bound) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "SOCKS5 返回了 Nexus 私有中继域名",
                    ));
                }
                self.system.resolve(name).await?
            }
        };
        let mut last = io::Error::new(io::ErrorKind::NotFound, "SOCKS5 UDP 中继没有地址");
        for ip in ips {
            match self
                .system
                .udp_connect(SocketAddr::new(ip, bound.port))
                .await
            {
                Ok(relay) => {
                    return Ok(UdpOutbound::Socks5 {
                        control,
                        relay,
                        header,
                    })
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    pub(super) async fn serve_exit_datagram<St: AsyncRead + AsyncWrite + Unpin>(
        &self,
        mut stream: St,
        target: Target,
    ) {
        let udp = match self.connect_exit_udp(&target).await {
            Ok(udp) => udp,
            Err(e) => {
                if write_response(&mut stream, &Response::Error(e))
                    .await
                    .is_ok()
                {
                    let _ = stream.shutdown().await;
                    drain(&mut stream).await;
                }
                return;
            }
        };
        if write_response(&mut stream, &Response::Ok).await.is_ok() {
            let result = datagrams::relay(
                stream,
                async |b: &[u8]| udp.send(b).await,
                async |b: &mut [u8]| udp.recv(b).await.map(Some),
            )
            .await;
            if let Err(e) = result {
                eprintln!("nexus daemon: 出口 UDP {target}：{e}");
            }
        }
    }

    async fn connect_exit_udp(&self, target: &Target) -> Result<UdpConn, WireError> {
        if target.port == 0 || is_internal(target) {
            return Err(err(
                ErrorCode::InvalidInput,
                format!("无效的公网 UDP 目标：{target}"),
            ));
        }
        let mut last = String::new();
        for addr in self.exit_addresses(target).await? {
            match self.system.udp_connect(addr).await {
                Ok(udp) => return Ok(udp),
                Err(e) => last = format!("出口打不开 {target} 的 UDP 套接字：{e}"),
            }
        }
        Err(err(ErrorCode::Unreachable, last))
    }
}

fn is_internal(target: &Target) -> bool {
    matches!(
        route::decide(target, &[]),
        Decision::Service { .. } | Decision::BadInternalName
    )
}

/// The proxy advertises UDP support by successfully replying to UDP ASSOCIATE.
async fn socks_associate(
    control: &mut (impl AsyncRead + AsyncWrite + Unpin),
) -> io::Result<Target> {
    control.write_all(&[5, 1, 0]).await?;
    let mut greeting = [0; 2];
    control.read_exact(&mut greeting).await?;
    if greeting != socks5::NO_AUTH {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "上游不接受无认证 SOCKS5",
        ));
    }
    control.write_all(&socks5::UDP_ASSOCIATE).await?;
    let mut reply = vec![0; 5];
    control.read_exact(&mut reply).await?;
    if reply[0] != 5 || reply[2] != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "无法识别 UDP ASSOCIATE 回应",
        ));
    }
    if reply[1] != 0 {
        let kind = match reply[1] {
            2 => io::ErrorKind::PermissionDenied,
            7 => io::ErrorKind::Unsupported,
            _ => io::ErrorKind::ConnectionRefused,
        };
        return Err(io::Error::new(
            kind,
            format!("上游拒绝 UDP ASSOCIATE（回应码 {}）", reply[1]),
        ));
    }
    let size = match reply[3] {
        1 => 10,
        4 => 22,
        3 => 7 + usize::from(reply[4]),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "无法识别 UDP 中继地址类型",
            ))
        }
    };
    reply.resize(size, 0);
    control.read_exact(&mut reply[5..]).await?;
    socks5::bound_address(&reply)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "无法识别 UDP 中继地址"))
}

impl<Tcp: AsyncRead + Unpin, Remote: AsyncRead + AsyncWrite + Unpin> UdpOutbound<Tcp, Remote> {
    async fn forward(self, client: ProxyStream, target: &Target) -> io::Result<()> {
        // Each read from the TUN yields one packet. Preserve this boundary on every hop.
        let (mut rd, mut wr) = tokio::io::split(client);
        let send = async |b: &[u8]| {
            wr.write_all(b).await?;
            Ok(b.len())
        };
        let recv = async |b: &mut [u8]| read_tun_datagram(&mut rd, b).await;
        match self {
            Self::Direct(udp) => {
                datagrams::bridge(
                    recv,
                    send,
                    async |b: &[u8]| udp.send(b).await,
                    async |b: &mut [u8]| udp.recv(b).await,
                )
                .await
            }
            Self::Nexus(stream) => {
                datagrams::relay(stream, send, async |buf: &mut [u8]| {
                    read_tun_datagram(&mut rd, buf)
                        .await
                        .map(|n| (n != 0).then_some(n))
                })
                .await
            }
            Self::Socks5 {
                mut control,
                relay,
                header,
            } => {
                let mut buf = vec![0; 65535];
                let send_to_relay = async |data: &[u8]| {
                    let mut packet = Vec::with_capacity(header.len() + data.len());
                    packet.extend_from_slice(&header);
                    packet.extend_from_slice(data);
                    relay.send(&packet).await
                };
                let recv_from_relay = async |out: &mut [u8]| loop {
                    let size = relay.recv(&mut buf).await?;
                    if let Some((origin, data)) = socks5::parse_udp_packet(&buf[..size]) {
                        if reply_matches(target, &origin) && data.len() <= out.len() {
                            out[..data.len()].copy_from_slice(data);
                            return Ok(data.len());
                        }
                    }
                };
                let mut control_byte = [0; 1];
                tokio::select! {
                    result = datagrams::bridge(recv, send, send_to_relay, recv_from_relay) => result,
                    result = control.read(&mut control_byte) => {
                        result?;
                        Err(io::Error::new(io::ErrorKind::ConnectionAborted, "SOCKS5 UDP 控制连接已关闭或发送了意外数据"))
                    }
                }
            }
        }
    }
}

fn reply_matches(target: &Target, origin: &Target) -> bool {
    target.port == origin.port
        && match (&target.host, &origin.host) {
            (Host::Name(name), Host::Name(origin)) => name
                .trim_end_matches('.')
                .eq_ignore_ascii_case(origin.trim_end_matches('.')),
            (Host::Name(_), Host::Ip(ip)) => !ip.is_unspecified(),
            (Host::Ip(ip), Host::Ip(origin)) => ip == origin,
            (Host::Ip(_), Host::Name(_)) => false,
        }
}

/// The TUN stack signals normal UDP inactivity with a read timeout.
async fn read_tun_datagram(
    reader: &mut (impl AsyncRead + Unpin),
    buf: &mut [u8],
) -> io::Result<usize> {
    match reader.read(buf).await {
        Err(error) if error.kind() == io::ErrorKind::TimedOut => Ok(0),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::ReadBuf;

    struct ReadError(io::ErrorKind);

    impl AsyncRead for ReadError {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Err(self.0.into()))
        }
    }

    #[tokio::test]
    async fn tun_udp_idle_timeout_ends_the_flow_without_an_error() {
        let mut buf = [0; 8];
        let mut idle = ReadError(io::ErrorKind::TimedOut);
        let mut broken = ReadError(io::ErrorKind::BrokenPipe);
        assert_eq!(read_tun_datagram(&mut idle, &mut buf).await.unwrap(), 0);
        assert_eq!(
            read_tun_datagram(&mut broken, &mut buf)
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe,
        );
    }
}
