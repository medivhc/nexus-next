//! 网页节点的技术验证：浏览器里的 iroh 经中继连上原生节点，发一句话收回来。
pub const ALPN: &[u8] = b"nexus/probe/1";

#[cfg(target_arch = "wasm32")]
mod web {
    use super::ALPN;
    use iroh::endpoint::presets;
    use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl};
    use wasm_bindgen::prelude::*;

    fn js(e: impl std::fmt::Display) -> JsError {
        JsError::new(&e.to_string())
    }

    #[wasm_bindgen]
    pub async fn ping(relay: String, id: String, text: String) -> Result<String, JsError> {
        console_error_panic_hook::set_once();
        let relay: RelayUrl = relay.parse().map_err(js)?;
        let id: EndpointId = id.parse().map_err(js)?;
        let ep = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::custom([relay.clone()]))
            .bind()
            .await
            .map_err(js)?;
        let conn = ep.connect(EndpointAddr::new(id).with_relay_url(relay), ALPN).await.map_err(js)?;
        let (mut send, mut recv) = conn.open_bi().await.map_err(js)?;
        send.write_all(text.as_bytes()).await.map_err(js)?;
        send.finish().map_err(js)?;
        let reply = recv.read_to_end(1 << 20).await.map_err(js)?;
        conn.close(0u32.into(), b"bye");
        ep.close().await;
        Ok(String::from_utf8_lossy(&reply).into_owned())
    }
}
