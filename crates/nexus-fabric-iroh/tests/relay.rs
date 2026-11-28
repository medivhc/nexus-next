//! 经本机中继通信（M2.0 技术验证）：网页节点在浏览器里只能经中继连接，
//! 这里用一个不开任何 UDP 套接字的节点扮演它。

use nexus_core_proto::PathKind;
use nexus_fabric::{Connection, Proto, Transport};
use nexus_fabric_iroh::{Config, IrohTransport, Relay};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn local_relay() -> (iroh_relay::server::Server, String) {
    let mut config = iroh_relay::server::ServerConfig::default();
    config.relay = Some(iroh_relay::server::RelayConfig::new(([127, 0, 0, 1], 0)));
    let server = iroh_relay::server::Server::spawn(config).await.expect("中继启动失败");
    let url = format!("http://{}", server.http_addr().expect("应当有 HTTP 地址"));
    (server, url)
}

#[tokio::test]
async fn relay_only_node_reaches_a_daemon_through_a_local_relay() {
    let (_relay, url) = local_relay().await;
    // 常驻程序：本机回环地址加中继。
    let daemon = IrohTransport::bind([1; 32], Config { relay: Relay::Custom(url.clone()), discovery: false, bind: Some(([127, 0, 0, 1], 0).into()), direct: true, scheduling: true, rendezvous: None })
        .await
        .unwrap();
    // 网页节点的替身：没有任何直连地址，只能经中继。
    let web = IrohTransport::bind([2; 32], Config { relay: Relay::Custom(url), discovery: false, bind: None, direct: false, scheduling: true, rendezvous: None })
        .await
        .unwrap();
    web.add_hint(daemon.node_id(), &daemon.local_hint());

    let server = async {
        let (conn, proto) = daemon.accept().await.unwrap();
        assert_eq!(proto, Proto::Rpc);
        let mut s = conn.accept_bi().await.unwrap().unwrap();
        let mut buf = [0u8; 5];
        s.read_exact(&mut buf).await.unwrap();
        s.write_all(&buf).await.unwrap();
        s.shutdown().await.unwrap();
        let mut rest = Vec::new();
        let _ = s.read_to_end(&mut rest).await;
    };
    let client = async {
        let started = std::time::Instant::now();
        let conn = tokio::time::timeout(std::time::Duration::from_secs(20), web.connect(daemon.node_id(), Proto::Rpc))
            .await
            .expect("20 秒内没有连上")
            .unwrap();
        let mut s = conn.open_bi().await.unwrap();
        s.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
        assert_eq!(conn.path_kind(), Some(PathKind::Relay), "只能经中继");
        // 刚启动的设备也要马上经自建中继可达（FP-2.5-01）：自建中继不提供 QUIC 地址探测，
        // 不能等探测超时（3 秒）之后才连上它。
        let took = started.elapsed();
        assert!(took < std::time::Duration::from_millis(1500), "经中继连上刚启动的设备用了 {took:?}");
        s.shutdown().await.unwrap();
    };
    tokio::join!(server, client);
}
