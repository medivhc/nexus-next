//! 测试用的中继（M11.4）：在给定地址上以 http 提供 iroh 中继，不限速，按 Ctrl-C 或被杀时停止。
//!
//! `tools/multipath_check.py --only relay` 在 home 的网络命名空间里运行它，验证经中继的路径（R86）。
//! 锚点（`--anchor-dev`）只能监听本机回环地址，别的命名空间连不到，所以单独起一个。
//!
//! 用法：cargo run --release -p nexus-fabric-iroh --example local_relay -- 0.0.0.0:3340

#[tokio::main]
async fn main() {
    let addr: std::net::SocketAddr = std::env::args().nth(1).as_deref().unwrap_or("0.0.0.0:3340").parse().expect("地址:端口");
    let mut config = iroh_relay::server::ServerConfig::<(), ()>::default();
    config.relay = Some(iroh_relay::server::RelayConfig::new(addr));
    let server = iroh_relay::server::Server::spawn(config).await.expect("中继启动失败");
    println!("http://{}", server.http_addr().expect("应当有 HTTP 地址"));
    let _ = tokio::signal::ctrl_c().await;
}
