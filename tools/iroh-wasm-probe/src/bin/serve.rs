//! 本机中继 + 一个回声节点。输出：中继地址 和 节点标识。
#[cfg(not(target_arch = "wasm32"))]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use iroh::endpoint::presets;
    use iroh::{Endpoint, RelayMode, RelayUrl};
    let mut config = iroh_relay::server::ServerConfig::default();
    config.relay = Some(iroh_relay::server::RelayConfig::new(([127, 0, 0, 1], 3340)));
    let _relay = iroh_relay::server::Server::spawn(config).await?;
    let url: RelayUrl = "http://127.0.0.1:3340".parse()?;
    let ep = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::custom([url.clone()]))
        .alpns(vec![probe::ALPN.to_vec()])
        .bind()
        .await?;
    println!("{url} {}", ep.id());
    loop {
        let Some(incoming) = ep.accept().await else { break };
        tokio::spawn(async move {
            let Ok(conn) = incoming.await else { return };
            while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                let data = recv.read_to_end(1 << 20).await.unwrap_or_default();
                let mut reply = b"echo: ".to_vec();
                reply.extend(data);
                let _ = send.write_all(&reply).await;
                let _ = send.finish();
            }
        });
    }
    Ok(())
}
#[cfg(target_arch = "wasm32")]
fn main() {}
