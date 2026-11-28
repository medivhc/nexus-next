// covers: R42, R43, R91, R102
//! N1 运行时仿真测试用例（TC-N1-07 / TC-N1-08 / TC-N1-09）：
//! 通用公网 UDP 经出口转发、严格拒绝出口本机回环地址、Reject/Socks5 绝不偷偷直连。

use nexus_core_proto::NodeId;
use nexus_fabric_sim::SimTransport;
use nexus_net_core::fakeip::FakePool;
use nexus_net_core::route::{Matcher, Outbound, Rule};
use nexus_runtime::{ctl_session, Node, TunInbound};
use nexus_sys_sim::{SimClock, SimDisk, SimServices, SimSystem};
use nexus_wire::{CtlRequest, CtlResponse};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::rc::Rc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

async fn ctl_call(tx: &mpsc::Sender<nexus_runtime::CtlCall>, req: CtlRequest) -> CtlResponse {
    let (call, mut client) = ctl_session(req, None);
    tx.send(call).await.expect("节点已停止");
    client.rx.recv().await.expect("控制通道关闭")
}

/// TC-N1-07 / TC-N1-08 / TC-N1-09：通用公网 UDP 经出口转发、拒绝本机回环、Reject/Socks5 不偷偷直连
#[test]
fn exit_datagram_forwards_udp_rejects_loopback_and_never_falls_back_to_direct() {
    let clock = SimClock::default();
    let home_services = SimServices::default();
    let phone_services = SimServices::default();

    let public_udp = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10)), 9000);
    let loopback_udp = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9001);
    let blocked_udp = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 20)), 9002);

    home_services.add_host("udp.far.test", public_udp.ip());
    home_services.add_udp_echo(public_udp);
    home_services.add_host("local.test", loopback_udp.ip());
    home_services.add_udp_echo(loopback_udp);
    // 即便 phone 本机也能解析/触达这些目标，也绝不能在 Reject/Socks5 下偷偷直连
    phone_services.add_host("blocked.test", blocked_udp.ip());
    phone_services.add_udp_echo(blocked_udp);
    phone_services.add_host("socks.test", blocked_udp.ip());

    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(30))
        .build();

    let (home_ctl_tx, home_ctl_rx) = mpsc::channel(32);
    let (phone_ctl_tx, phone_ctl_rx) = mpsc::channel(32);
    let (tun_ready_tx, tun_ready_rx) = tokio::sync::oneshot::channel();

    let home_rx = Rc::new(tokio::sync::Mutex::new(home_ctl_rx));
    let phone_rx = Rc::new(tokio::sync::Mutex::new(phone_ctl_rx));

    let clock_home = clock.clone();
    let svc_home = home_services.clone();
    sim.host("home", move || {
        let clock = clock_home.clone();
        let svc = svc_home.clone();
        let rx = home_rx.clone();
        async move {
            let sys = SimSystem::with_services(SimDisk::default(), 1, clock, svc);
            let key = nexus_runtime::load_or_create_device_key(&sys).unwrap();
            let fab = SimTransport::bind(NodeId(key.public().0), "home").await.unwrap();
            let node = Rc::new(Node::new(fab, sys, "home").unwrap());
            let mut rx = rx.lock().await;
            let _ = node.run(&mut rx).await;
            Ok(())
        }
    });

    let clock_phone = clock.clone();
    let svc_phone = phone_services.clone();
    let tun_tx_slot = Rc::new(std::cell::RefCell::new(Some(tun_ready_tx)));
    sim.host("phone", move || {
        let clock = clock_phone.clone();
        let svc = svc_phone.clone();
        let rx = phone_rx.clone();
        let tun_tx_slot = tun_tx_slot.clone();
        async move {
            let sys = SimSystem::with_services(SimDisk::default(), 2, clock, svc);
            let key = nexus_runtime::load_or_create_device_key(&sys).unwrap();
            let fab = SimTransport::bind(NodeId(key.public().0), "phone").await.unwrap();
            let node = Rc::new(Node::new(fab, sys, "phone").unwrap());
            let pool = FakePool::new(Ipv4Addr::new(198, 18, 0, 0), 16).unwrap();
            let tun_tx = node.enable_tun("sim-tun", pool);
            if let Some(tx) = tun_tx_slot.borrow_mut().take() {
                let _ = tx.send(tun_tx);
            }
            let mut rx = rx.lock().await;
            let _ = node.run(&mut rx).await;
            Ok(())
        }
    });

    let home_svc_check = home_services.clone();
    let phone_svc_check = phone_services.clone();
    sim.client("driver", async move {
        let tun_tx = tun_ready_rx.await.unwrap();

        // 1. 组网：home 初始化，phone 加入并由 home 批准
        let init_resp = ctl_call(&home_ctl_tx, CtlRequest::Init { name: Some("home".into()) }).await;
        assert!(!matches!(init_resp, CtlResponse::Error(_)), "home Init 失败：{init_resp:?}");
        let CtlResponse::JoinCode { code, .. } = ctl_call(&phone_ctl_tx, CtlRequest::JoinStart).await else {
            panic!("phone JoinStart 失败");
        };
        let approve_resp = ctl_call(&home_ctl_tx, CtlRequest::Approve { code, name: Some("phone".into()) }).await;
        assert!(
            !matches!(approve_resp, CtlResponse::Error(_)),
            "home Approve 失败：{approve_resp:?}"
        );
        assert!(matches!(ctl_call(&home_ctl_tx, CtlRequest::ExitSet { enabled: true }).await, CtlResponse::Exit { enabled: true }));

        // 2. 在 phone 上导入路由规则
        let rules = vec![
            Rule { matcher: Matcher::parse_domain("udp.far.test").unwrap(), outbound: Outbound::Exit("home".into()) },
            Rule { matcher: Matcher::parse_domain("local.test").unwrap(), outbound: Outbound::Exit("home".into()) },
            Rule { matcher: Matcher::parse_domain("blocked.test").unwrap(), outbound: Outbound::Reject },
            Rule { matcher: Matcher::parse_domain("socks.test").unwrap(), outbound: Outbound::Socks5("127.0.0.1:7890".into()) },
        ];
        assert!(matches!(
            ctl_call(&phone_ctl_tx, CtlRequest::RouteImport { rules, replace: true }).await,
            CtlResponse::RouteImported { total: 4, .. }
        ));

        // 辅助函数：经 TUN DNS 分配 FakeIP
        async fn fake_ip_for(tun_tx: &mpsc::UnboundedSender<TunInbound>, domain: &str) -> Ipv4Addr {
            let mut pkt = vec![0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
            for label in domain.split('.') {
                pkt.push(label.len() as u8);
                pkt.extend_from_slice(label.as_bytes());
            }
            pkt.extend_from_slice(&[0x00, 0x00, 0x01, 0x00, 0x01]);
            let (tx, rx) = tokio::sync::oneshot::channel();
            tun_tx.send(TunInbound::Dns { query: pkt, reply: tx }).unwrap();
            let resp = rx.await.unwrap();
            let n = resp.len();
            Ipv4Addr::new(resp[n - 4], resp[n - 3], resp[n - 2], resp[n - 1])
        }

        // 3. TC-N1-07：公网 UDP 经 home 出口成功往返
        let far_ip = fake_ip_for(&tun_tx, "udp.far.test").await;
        let (mut app, stack) = tokio::io::duplex(4096);
        tun_tx
            .send(TunInbound::Udp {
                stream: Box::new(stack),
                dst: SocketAddr::new(IpAddr::V4(far_ip), 9000),
            })
            .unwrap();
        app.write_all(b"ping-udp").await.unwrap();
        let mut buf = [0u8; 64];
        let n = tokio::time::timeout(Duration::from_secs(3), app.read(&mut buf))
            .await
            .expect("等待 UDP 回应超时")
            .unwrap();
        assert_eq!(&buf[..n], b"echo:ping-udp");
        assert_eq!(home_svc_check.udp_hits(public_udp), 1);

        // 验证 home 审计日志记录了 net.exit
        let CtlResponse::Audit(events) = ctl_call(&home_ctl_tx, CtlRequest::Audit { limit: Some(20) }).await else {
            panic!("读取 home 审计失败");
        };
        assert!(
            events.iter().any(|e| e.event.capability == "net.exit" && e.event.resource == "home/exit"),
            "未记录 net.exit 审计：{events:?}"
        );

        // 4. TC-N1-08：出口 UDP 严格拒绝本机回环地址 127.0.0.1
        let local_ip = fake_ip_for(&tun_tx, "local.test").await;
        let (mut app_local, stack_local) = tokio::io::duplex(4096);
        tun_tx
            .send(TunInbound::Udp {
                stream: Box::new(stack_local),
                dst: SocketAddr::new(IpAddr::V4(local_ip), 9001),
            })
            .unwrap();
        let _ = app_local.write_all(b"should-block").await;
        let mut b2 = [0u8; 32];
        let res = tokio::time::timeout(Duration::from_millis(500), app_local.read(&mut b2)).await;
        assert!(res.is_err() || matches!(res, Ok(Ok(0))), "本机回环地址绝不能收到回应：{res:?}");
        assert_eq!(home_svc_check.udp_hits(loopback_udp), 0, "出口绝不能访问自身回环地址");

        // 5. TC-N1-09：Reject 与 Socks5 规则下的 UDP 明确丢弃，绝不偷偷直连
        for domain in ["blocked.test", "socks.test"] {
            let ip = fake_ip_for(&tun_tx, domain).await;
            let (mut app_drop, stack_drop) = tokio::io::duplex(4096);
            tun_tx
                .send(TunInbound::Udp {
                    stream: Box::new(stack_drop),
                    dst: SocketAddr::new(IpAddr::V4(ip), 9002),
                })
                .unwrap();
            let _ = app_drop.write_all(b"no-leak").await;
            let mut b3 = [0u8; 32];
            let res = tokio::time::timeout(Duration::from_millis(300), app_drop.read(&mut b3)).await;
            assert!(res.is_err() || matches!(res, Ok(Ok(0))), "{domain} 不应有回应：{res:?}");
        }
        assert_eq!(phone_svc_check.udp_hits(blocked_udp), 0, "Reject/Socks5 绝不能偷偷降级直连");
        Ok(())
    });

    sim.run().unwrap();
}
