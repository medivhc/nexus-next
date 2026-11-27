// covers: R43
//! N3 运行时仿真测试（TC-N3-07 / TC-N3-08）：
//! 验证 answer_dns 执行自定义 DNS Override、BlockNxdomain、BlockNodata、Allowlist，
//! 并将脱敏事件写入本地 FlowRing 供 CtlRequest::FlowRecent 查询。

use nexus_core_proto::NodeId;
use nexus_fabric_sim::SimTransport;
use nexus_net_core::dns_policy::{DnsAction, DnsRule};
use nexus_net_core::fakeip::FakePool;
use nexus_net_core::route::Matcher;
use nexus_runtime::{ctl_session, Node, TunInbound};
use nexus_sys_sim::{SimClock, SimDisk, SimServices, SimSystem};
use nexus_wire::{CtlRequest, CtlResponse};
use std::net::{IpAddr, Ipv4Addr};
use std::rc::Rc;
use std::time::Duration;
use tokio::sync::mpsc;

async fn ctl_call(tx: &mpsc::Sender<nexus_runtime::CtlCall>, req: CtlRequest) -> CtlResponse {
    let (call, mut client) = ctl_session(req, None);
    tx.send(call).await.expect("节点已停止");
    client.rx.recv().await.expect("控制通道关闭")
}

fn build_a_query(domain: &str) -> Vec<u8> {
    let mut pkt = vec![0xab, 0xcd, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    for label in domain.split('.') {
        pkt.push(label.len() as u8);
        pkt.extend_from_slice(label.as_bytes());
    }
    pkt.extend_from_slice(&[0x00, 0x00, 0x01, 0x00, 0x01]);
    pkt
}

#[test]
fn tun_dns_applies_override_block_nodata_allowlist_and_records_flows() {
    let clock = SimClock::default();
    let services = SimServices::default();
    services.add_host("ok.ads.test", IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)));

    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(20))
        .build();

    let (ctl_tx, ctl_rx) = mpsc::channel(32);
    let (tun_tx_send, tun_tx_recv) = tokio::sync::oneshot::channel();
    let rx_rc = Rc::new(tokio::sync::Mutex::new(ctl_rx));
    let tun_slot = Rc::new(std::cell::RefCell::new(Some(tun_tx_send)));

    sim.host("node", move || {
        let clock = clock.clone();
        let services = services.clone();
        let rx_rc = rx_rc.clone();
        let tun_slot = tun_slot.clone();
        async move {
            let sys = SimSystem::with_services(SimDisk::default(), 7, clock, services);
            let key = nexus_runtime::load_or_create_device_key(&sys).unwrap();
            let fab = SimTransport::bind(NodeId(key.public().0), "node").await.unwrap();
            let node = Rc::new(Node::new(fab, sys, "node").unwrap());
            let pool = FakePool::new(Ipv4Addr::new(198, 18, 0, 0), 16).unwrap();
            let tun_tx = node.enable_tun("sim-tun", pool);
            if let Some(tx) = tun_slot.borrow_mut().take() {
                let _ = tx.send(tun_tx);
            }
            let mut rx = rx_rc.lock().await;
            let _ = node.run(&mut rx).await;
            Ok(())
        }
    });

    sim.client("driver", async move {
        let tun_tx = tun_tx_recv.await.unwrap();

        let rules = vec![
            DnsRule {
                id: "dev-override".into(),
                matcher: Matcher::parse_domain_exact("api.dev.test").unwrap(),
                action: DnsAction::Override {
                    ips: vec![IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5))],
                    ttl: 30,
                },
            },
            DnsRule {
                id: "block-ads".into(),
                matcher: Matcher::parse_domain("ads.test").unwrap(),
                action: DnsAction::BlockNxdomain,
            },
            DnsRule {
                id: "allow-ok-ads".into(),
                matcher: Matcher::parse_domain_exact("ok.ads.test").unwrap(),
                action: DnsAction::Allow,
            },
            DnsRule {
                id: "nodata-track".into(),
                matcher: Matcher::parse_domain("track.test").unwrap(),
                action: DnsAction::BlockNodata,
            },
        ];
        for rule in rules {
            assert!(matches!(ctl_call(&ctl_tx, CtlRequest::DnsRuleAdd { rule }).await, CtlResponse::Ok));
        }

        async fn query_dns(tun_tx: &mpsc::UnboundedSender<TunInbound>, domain: &str) -> Vec<u8> {
            let (tx, rx) = tokio::sync::oneshot::channel();
            tun_tx.send(TunInbound::Dns { query: build_a_query(domain), reply: tx }).unwrap();
            rx.await.unwrap()
        }

        // 1. Override 返回自定义 10.0.0.5
        let r_override = query_dns(&tun_tx, "api.dev.test").await;
        assert_eq!(r_override[3] & 0x0f, 0, "Override 应返回 NOERROR");
        assert_eq!(u16::from_be_bytes([r_override[6], r_override[7]]), 1, "应含 1 条 A 记录");
        assert_eq!(&r_override[r_override.len() - 4..], &[10, 0, 0, 5]);

        // 2. BlockNxdomain 返回 NXDOMAIN (rcode = 3)
        let r_block = query_dns(&tun_tx, "bad.ads.test").await;
        assert_eq!(r_block[3] & 0x0f, 3, "BlockNxdomain 应返回 NXDOMAIN");

        // 3. BlockNodata 返回空 NOERROR (rcode = 0, ancount = 0)
        let r_nodata = query_dns(&tun_tx, "pixel.track.test").await;
        assert_eq!(r_nodata[3] & 0x0f, 0, "BlockNodata 应返回 NOERROR");
        assert_eq!(u16::from_be_bytes([r_nodata[6], r_nodata[7]]), 0, "BlockNodata 应含 0 条回答");

        // 4. Allowlist 局部放行 ok.ads.test，正常解析到 93.184.216.34
        let r_allow = query_dns(&tun_tx, "ok.ads.test").await;
        assert_eq!(r_allow[3] & 0x0f, 0, "Allow 放行后应解析成功");
        assert_eq!(&r_allow[r_allow.len() - 4..], &[93, 184, 216, 34]);

        // 5. TC-N3-08：本地脱敏 FlowRing 已记录上述 4 条 DNS 事件
        let CtlResponse::Flows(events) = ctl_call(&ctl_tx, CtlRequest::FlowRecent { limit: 10 }).await else {
            panic!("FlowRecent 返回异常");
        };
        assert_eq!(events.len(), 4, "应记录 4 条 DNS 事件：{events:?}");
        assert_eq!(events[0].target, "ok.ads.test");
        assert!(events.iter().any(|e| e.target == "bad.ads.test" && e.result == "nxdomain"));
        assert!(events.iter().any(|e| e.target == "api.dev.test" && e.decision.contains("dev-override")));
        Ok(())
    });

    sim.run().unwrap();
}
