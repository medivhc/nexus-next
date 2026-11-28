// covers: R91, R111, R112
use super::*;
use nexus_sys::{Datagram, UdpConn};
use std::net::{IpAddr, SocketAddr};
use std::task::{Context, Poll};
use tokio::io::ReadBuf;

struct PacketEndpoint {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    rx: RefCell<mpsc::UnboundedReceiver<Vec<u8>>>,
}

impl Datagram for PacketEndpoint {
    fn poll_send(&self, _: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
        Poll::Ready(
            self.tx
                .send(data.to_vec())
                .map(|()| data.len())
                .map_err(|_| io::ErrorKind::BrokenPipe.into()),
        )
    }

    fn poll_recv(&self, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.rx.borrow_mut().poll_recv(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(data)) => {
                buf.put_slice(&data[..data.len().min(buf.remaining())]);
                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
        }
    }
}

fn datagram_pair() -> (UdpConn, UdpConn) {
    let (a_tx, a_rx) = mpsc::unbounded_channel();
    let (b_tx, b_rx) = mpsc::unbounded_channel();
    (
        UdpConn(Box::new(PacketEndpoint {
            tx: a_tx,
            rx: RefCell::new(b_rx),
        })),
        UdpConn(Box::new(PacketEndpoint {
            tx: b_tx,
            rx: RefCell::new(a_rx),
        })),
    )
}

#[tokio::test]
async fn public_udp_exit_coexists_with_tcp_and_private_udp() {
    let (tx, mut requests) = mpsc::unbounded_channel();
    let source = Node::new(
        TestTransport {
            id: 1,
            network: Some((NodeId([2; 32]), tx)),
        },
        TestSystem(Rc::default()),
        "phone",
    )
    .unwrap();
    let exit = Node::new(
        TestTransport {
            id: 2,
            network: None,
        },
        TestSystem(Rc::default()),
        "exit",
    )
    .unwrap();
    let key = SecretKey::from_seed([3; 32]);
    let roster = Roster {
        root: key.public(),
        version: 1,
        anchor: None,
        web_keys: Vec::new(),
        revoked_agents: Vec::new(),
        devices: [&source, &exit]
            .map(|node| Device {
                node_id: node.me(),
                name: node.configured_name.clone(),
                role: DeviceRole::Member,
                added_at: Timestamp(0),
            })
            .into(),
    }
    .sign(&key);
    source.trust.borrow_mut().roster = Some(roster.clone());
    exit.trust.borrow_mut().roster = Some(roster);
    exit.exit_enabled.set(true);
    source.routes.borrow_mut().rules.push(Rule {
        matcher: route::Matcher::Domain("public.test".into()),
        outbound: Outbound::Exit("exit".into()),
    });

    let ip = IpAddr::from([192, 0, 2, 10]);
    let (public, public_peer) = datagram_pair();
    let (private, private_peer) = datagram_pair();
    let public_addr = SocketAddr::new(ip, 443);
    let private_addr = SocketAddr::from(([127, 0, 0, 1], 9001));
    exit.system
        .0
        .dns
        .borrow_mut()
        .insert("public.test".into(), vec![ip]);
    exit.system
        .0
        .udp_targets
        .borrow_mut()
        .extend([(public_addr, public_peer), (private_addr, private_peer)]);
    exit.services.borrow_mut().push(PublishedService {
        name: "private".into(),
        addr: private_addr,
        proto: "udp".into(),
        env: Vec::new(),
    });
    let (mut tcp_target, tcp_endpoint) = tokio::io::duplex(8192);
    exit.system
        .0
        .tcp_targets
        .borrow_mut()
        .insert(public_addr, tcp_endpoint);

    let mut pool = FakePool::new([198, 18, 0, 0].into(), 16).unwrap();
    let public_fake = SocketAddr::new(pool.assign("public.test").into(), 443);
    let private_fake = SocketAddr::new(pool.assign("private.exit.internal").into(), 9000);
    source.enable_tun("test", pool);
    let (udp_client, udp_tun) = tokio::io::duplex(8192);
    let (private_client, private_tun) = tokio::io::duplex(8192);
    let (tcp_client, tcp_tun) = tokio::io::duplex(8192);

    async fn ask(mut client: DuplexStream, payload: &[u8]) {
        client.write_all(payload).await.unwrap();
        let mut reply = vec![0; payload.len()];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, payload);
    }
    async fn echo_udp(socket: &UdpConn, size: usize) {
        let mut packet = vec![0; 1500];
        let n = socket.recv(&mut packet).await.unwrap();
        assert_eq!(n, size);
        socket.send(&packet[..n]).await.unwrap();
    }

    let serve = async {
        let connection = TestConnection::default();
        let mut handlers = FuturesUnordered::new();
        for _ in 0..3 {
            let stream = requests.recv().await.unwrap();
            handlers.push(exit.serve_stream(&connection, source.me(), Proto::Rpc, stream));
        }
        while handlers.next().await.is_some() {}
    };
    let tcp_echo = async {
        let mut data = [0; 3];
        tcp_target.read_exact(&mut data).await.unwrap();
        tcp_target.write_all(&data).await.unwrap();
        tcp_target.shutdown().await.unwrap();
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            serve,
            tcp_echo,
            echo_udp(&public, 1200),
            echo_udp(&private, 7),
            source.serve_tun(Box::new(tcp_tun), public_fake, TunRouting::FakeIp),
            source.serve_tun_udp(Box::new(udp_tun), public_fake, TunRouting::FakeIp),
            source.serve_tun_udp(Box::new(private_tun), private_fake, TunRouting::FakeIp),
            ask(tcp_client, b"tcp"),
            ask(udp_client, &[42; 1200]),
            ask(private_client, b"private"),
        );
    })
    .await
    .unwrap();

    exit.exit_enabled.set(false);
    let request = Request::ExitDatagram {
        target: public_addr.to_string(),
    };
    let header = RequestHeader::new(1, Principal::Device, &request);
    assert!(
        matches!(exit.handle_rpc(source.me(), header, request), Dispatch::Reply(Response::Error(e)) if e.code == ErrorCode::Denied)
    );
}

#[tokio::test]
async fn public_udp_socks_associates_or_closes_without_fallback() {
    for supported in [true, false] {
        let node = Node::new(TestTransport::default(), TestSystem(Rc::default()), "phone").unwrap();
        let upstream = "127.0.0.1:1080".parse::<SocketAddr>().unwrap();
        let (mut proxy, control) = tokio::io::duplex(8192);
        node.system
            .0
            .tcp_targets
            .borrow_mut()
            .insert(upstream, control);
        let (relay, udp) = datagram_pair();
        let relay_addr = SocketAddr::from(([127, 0, 0, 1], 1081));
        node.system
            .0
            .udp_targets
            .borrow_mut()
            .insert(relay_addr, udp);
        let (_forbidden_direct, direct) = datagram_pair();
        let direct_addr = SocketAddr::from(([127, 0, 0, 1], 1082));
        node.system
            .0
            .dns
            .borrow_mut()
            .insert("public.test".into(), vec![IpAddr::from([127, 0, 0, 1])]);
        node.system
            .0
            .udp_targets
            .borrow_mut()
            .insert(direct_addr, direct);
        node.routes.borrow_mut().rules.push(Rule {
            matcher: route::Matcher::Domain("public.test".into()),
            outbound: Outbound::Socks5(upstream.to_string()),
        });
        let mut pool = FakePool::new([198, 18, 0, 0].into(), 16).unwrap();
        let target = SocketAddr::new(pool.assign("public.test").into(), direct_addr.port());
        node.enable_tun("test", pool);
        let (mut app, tun) = tokio::io::duplex(8192);
        let server = async {
            let mut greeting = [0; 3];
            proxy.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting, [5, 1, 0]);
            proxy.write_all(&socks5::NO_AUTH).await.unwrap();
            let mut request = [0; 10];
            proxy.read_exact(&mut request).await.unwrap();
            assert_eq!(request, socks5::UDP_ASSOCIATE);
            if !supported {
                proxy
                    .write_all(&socks5::reply(socks5::Reply::CommandNotSupported))
                    .await
                    .unwrap();
                return;
            }
            let mut reply = socks5::reply(socks5::Reply::Succeeded);
            reply[8..].copy_from_slice(&relay_addr.port().to_be_bytes());
            proxy.write_all(&reply).await.unwrap();
            let mut packet = [0; 1500];
            let n = relay.recv(&mut packet).await.unwrap();
            let (dst, data) = socks5::parse_udp_packet(&packet[..n]).unwrap();
            assert_eq!(
                dst,
                Target {
                    host: Host::Name("public.test".into()),
                    port: direct_addr.port()
                }
            );
            assert_eq!(data, b"hello");
            packet[2] = 1; // Fragmented responses must be ignored.
            packet[n - 5..n].copy_from_slice(b"wrong");
            relay.send(&packet[..n]).await.unwrap();
            packet[2] = 0;
            packet[n - 5..n].copy_from_slice(b"hello");
            relay.send(&packet[..n]).await.unwrap();
            assert_eq!(proxy.read(&mut [0]).await.unwrap(), 0);
        };
        let client = async move {
            app.write_all(b"hello").await.unwrap();
            if supported {
                let mut payload = [0; 5];
                app.read_exact(&mut payload).await.unwrap();
                assert_eq!(&payload, b"hello");
            } else {
                assert_eq!(app.read(&mut [0]).await.unwrap(), 0);
            }
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                node.serve_tun_udp(Box::new(tun), target, TunRouting::FakeIp),
                server,
                client
            );
        })
        .await
        .unwrap();
        assert!(node
            .system
            .0
            .udp_targets
            .borrow()
            .contains_key(&direct_addr));
    }
}

#[test]
fn application_capture_selects_literal_ip_and_preserves_private_routes() {
    let node = Node::new(TestTransport::default(), TestSystem(Rc::default()), "phone").unwrap();
    let mut pool = FakePool::new([198, 18, 0, 0].into(), 16).unwrap();
    let private = SocketAddr::new(pool.assign("web.home.internal").into(), 80);
    let blocked = SocketAddr::new(pool.assign("blocked.test").into(), 443);
    node.enable_tun("test", pool);
    node.routes.borrow_mut().rules.push(Rule {
        matcher: route::Matcher::parse_domain("blocked.test").unwrap(),
        outbound: Outbound::Reject,
    });
    let forced = || TunRouting::Application {
        outbound: Some(Outbound::Exit("vps".into())),
    };
    for literal in ["192.0.2.10:443", "[2001:db8::1]:443"] {
        let address = literal.parse().unwrap();
        assert!(node.tun_target(address, &TunRouting::FakeIp).is_none());
        let target = node.tun_target(address, &forced()).unwrap();
        assert_eq!(target.to_string(), literal);
        assert_eq!(
            node.tun_decision(&target, forced()),
            Decision::Default(Outbound::Exit("vps".into()))
        );
    }
    assert!(node
        .tun_target("198.18.0.50:443".parse().unwrap(), &forced())
        .is_none());
    let private = node.tun_target(private, &forced()).unwrap();
    assert!(matches!(
        node.tun_decision(&private, forced()),
        Decision::Service { .. }
    ));
    let blocked = node.tun_target(blocked, &forced()).unwrap();
    assert!(matches!(
        node.tun_decision(&blocked, forced()),
        Decision::Rule {
            outbound: Outbound::Reject,
            ..
        }
    ));
}

#[tokio::test]
async fn application_capture_dns_keeps_private_names_local() {
    fn query(name: &str, kind: u16) -> Vec<u8> {
        let mut packet = vec![0x12, 0x34, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            packet.push(label.len() as u8);
            packet.extend_from_slice(label.as_bytes());
        }
        packet.push(0);
        packet.extend_from_slice(&kind.to_be_bytes());
        packet.extend_from_slice(&[0, 1]);
        packet
    }

    let node = Node::new(TestTransport::default(), TestSystem(Rc::default()), "phone").unwrap();
    node.enable_tun("test", FakePool::new([198, 18, 0, 0].into(), 16).unwrap());
    let external = "public.test";
    let private = "web.unknown.internal";
    for name in [external, private] {
        node.system
            .0
            .dns
            .borrow_mut()
            .insert(name.into(), vec![IpAddr::from([192, 0, 2, 10])]);
    }
    let answer = node.answer_dns(&query(external, dns::A), true).await;
    assert_eq!(&answer[answer.len() - 4..], &[198, 18, 0, 3]);
    let answer = node.answer_dns(&query(external, dns::AAAA), true).await;
    assert_eq!(&answer[6..8], &[0, 0]);
    let answer = node.answer_dns(&query(private, dns::A), true).await;
    assert_eq!(answer[3] & 0xf, dns::NXDOMAIN);
    assert_eq!(&answer[6..8], &[0, 0]);
}
