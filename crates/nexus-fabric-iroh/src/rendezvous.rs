//! 碰头服务（短加入码，M6.1）：pkarr 中继的接口，`PUT/GET <地址>/<公钥的 z-base-32>`。
//!
//! 客户端：数据放在从种子派生的签名密钥下，一条名为 `_nexus` 的 TXT 记录（base32，分段）。
//! 服务端：锚点和测试用；只接受签名有效、比已有记录新的记录，最多存 [`MAX_RECORDS`] 条，[`RECORD_TTL`] 后丢弃。

use iroh::address_lookup::pkarr::PkarrRelayClient;
use iroh::{PublicKey, SecretKey};
use iroh_dns::pkarr::SignedPacket;
use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// 默认的碰头服务：iroh 公共的地址发现服务。
pub const DEFAULT_URL: &str = "https://dns.iroh.link/pkarr";
const TXT_NAME: &str = "_nexus";
const CHUNK: usize = 200;
/// 服务端保留一条记录的时间（比加入码的有效期长一些）。
pub const RECORD_TTL: Duration = Duration::from_secs(15 * 60);
pub const MAX_RECORDS: usize = 10_000;

fn io_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

pub(crate) struct Client {
    http: PkarrRelayClient,
}

impl Client {
    pub(crate) fn new(url: &str, ep: &iroh::Endpoint) -> io::Result<Self> {
        let url = url.parse().map_err(|e| io_err(format!("碰头服务的地址无效：{e}")))?;
        let dns = ep.dns_resolver().map_err(io_err)?.clone();
        Ok(Self { http: PkarrRelayClient::new(url, ep.tls_config().clone(), dns) })
    }

    pub(crate) async fn put(&self, seed: [u8; 32], data: Vec<u8>) -> io::Result<()> {
        let text = nexus_core_proto::joincode::base32_encode(&data);
        let chunks: Vec<String> = text.as_bytes().chunks(CHUNK).map(|c| String::from_utf8_lossy(c).into_owned()).collect();
        let packet = SignedPacket::from_txt_strings(&SecretKey::from_bytes(&seed), TXT_NAME, chunks, 60).map_err(io_err)?;
        self.http.publish(&packet).await.map_err(|e| io_err(format!("{e:#}")))
    }

    pub(crate) async fn get(&self, seed: [u8; 32]) -> io::Result<Option<Vec<u8>>> {
        let public = SecretKey::from_bytes(&seed).public();
        match self.http.resolve(public).await {
            Ok(packet) => Ok(decode(&packet)),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(io_err(format!("{e:#}"))),
        }
    }
}

/// 碰头服务回应 404（这个位置上从来没有记录）。状态码在错误的来源链里。
fn is_not_found(e: &(dyn std::error::Error + 'static)) -> bool {
    use iroh::address_lookup::pkarr::PkarrError;
    let mut cur = Some(e);
    while let Some(err) = cur {
        if let Some(PkarrError::HttpRequest { status, .. }) = err.downcast_ref::<PkarrError>() {
            return status.as_u16() == 404;
        }
        cur = err.source();
    }
    false
}

fn decode(packet: &SignedPacket) -> Option<Vec<u8>> {
    let text: String = packet.txt_records(TXT_NAME).concat();
    if text.is_empty() {
        return None;
    }
    nexus_core_proto::joincode::base32_decode(&text)
}

/// 公钥 → 最新的记录和存进来的时间。
type Records = HashMap<[u8; 32], (SignedPacket, Instant)>;

const SWEEP_INTERVAL: Duration = Duration::from_secs(30);
const PUT_WINDOW: Duration = Duration::from_secs(60);
const MAX_PUTS_PER_IP_PER_WINDOW: u32 = 30;

#[derive(Default)]
struct StoreInner {
    records: Records,
    last_sweep: Option<Instant>,
    /// 按来源 IP 限制 PUT 频率，防止未鉴权客户端打满 `MAX_RECORDS`。
    put_rates: HashMap<std::net::IpAddr, (Instant, u32)>,
}

/// 服务端的存储，锚点和测试共用。
#[derive(Clone, Default)]
pub struct Store {
    inner: Arc<Mutex<StoreInner>>,
}

impl Store {
    /// 处理一个请求：`path` 是 `/pkarr/<公钥>` 之后的公钥部分。返回状态码和回应体。
    pub fn handle(&self, method: &str, key: &str, body: &[u8]) -> (u16, Vec<u8>) {
        self.handle_with_ip(None, method, key, body)
    }

    /// 带来源 IP 限流的请求处理（锚点公网入口调用）。
    pub fn handle_with_ip(&self, peer_ip: Option<std::net::IpAddr>, method: &str, key: &str, body: &[u8]) -> (u16, Vec<u8>) {
        let Ok(public) = PublicKey::from_z32(key) else { return (400, b"bad key".to_vec()) };
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let should_sweep = inner.last_sweep.is_none_or(|t| now.duration_since(t) >= SWEEP_INTERVAL);
        if should_sweep {
            inner.records.retain(|_, (_, at)| now.duration_since(*at) < RECORD_TTL);
            inner.put_rates.retain(|_, (start, _)| now.duration_since(*start) < PUT_WINDOW);
            inner.last_sweep = Some(now);
        }
        match method {
            "GET" => match inner.records.get(public.as_bytes()) {
                Some((packet, at)) if now.duration_since(*at) < RECORD_TTL => (200, packet.to_relay_payload()),
                _ => (404, Vec::new()),
            },
            "PUT" => {
                if let Some(ip) = peer_ip {
                    let entry = inner.put_rates.entry(ip).or_insert((now, 0));
                    if now.duration_since(entry.0) >= PUT_WINDOW {
                        *entry = (now, 0);
                    }
                    if entry.1 >= MAX_PUTS_PER_IP_PER_WINDOW {
                        return (429, b"rate limited".to_vec());
                    }
                    entry.1 += 1;
                }
                let Ok(packet) = SignedPacket::from_relay_payload(&public, body) else { return (400, b"bad signature".to_vec()) };
                if let Some((old, at)) = inner.records.get(public.as_bytes()) {
                    if now.duration_since(*at) < RECORD_TTL && !packet.more_recent_than(old) {
                        return (409, b"not newer".to_vec());
                    }
                } else if inner.records.len() >= MAX_RECORDS {
                    inner.records.retain(|_, (_, at)| now.duration_since(*at) < RECORD_TTL);
                    if inner.records.len() >= MAX_RECORDS {
                        return (429, b"full".to_vec());
                    }
                }
                inner.records.insert(*public.as_bytes(), (packet, now));
                (200, Vec::new())
            }
            _ => (405, Vec::new()),
        }
    }
}

/// 请求头和已经读到的部分之后，读完请求体、回应、关闭。`head` 是 [`read_head`] 的结果。
pub async fn respond<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S, head: &[u8], store: &Store) -> io::Result<()> {
    respond_with_ip(stream, head, store, None).await
}

/// 带来源 IP 限流的碰头服务响应。
pub async fn respond_with_ip<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    head: &[u8],
    store: &Store,
    peer_ip: Option<std::net::IpAddr>,
) -> io::Result<()> {
    let end = head.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| io_err("请求头不完整"))?;
    let text = String::from_utf8_lossy(&head[..end]);
    let mut lines = text.split("\r\n");
    let mut first = lines.next().unwrap_or("").split_whitespace();
    let (method, target) = (first.next().unwrap_or("").to_string(), first.next().unwrap_or("").to_string());
    let len: usize = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())
        .unwrap_or(0);
    let (status, reply) = if len > SignedPacket::MAX_BYTES {
        (413, Vec::new())
    } else {
        let mut body = head[end + 4..].to_vec();
        body.truncate(len);
        if body.len() < len {
            let mut rest = vec![0u8; len - body.len()];
            tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut rest)).await.map_err(io_err)??;
            body.extend_from_slice(&rest);
        }
        let key = target.split('?').next().unwrap_or("").trim_start_matches('/').strip_prefix("pkarr/").unwrap_or("");
        store.handle_with_ip(peer_ip, &method, key, &body)
    };
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/pkarr.org/relays#payload\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reply.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&reply).await?;
    stream.shutdown().await
}

/// 读到请求头结束（或 8 KiB）为止，可能包含请求体的开头。
pub async fn read_head<R: AsyncRead + Unpin>(stream: &mut R) -> io::Result<Vec<u8>> {
    let mut head = Vec::new();
    let mut buf = [0u8; 2048];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 8192 {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        head.extend_from_slice(&buf[..n]);
    }
    Ok(head)
}

/// 独立运行的碰头服务（测试用）：`http://<地址>/pkarr`。
pub async fn serve(listener: tokio::net::TcpListener, store: Store) -> io::Result<()> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let store = store.clone();
        tokio::spawn(async move {
            if let Ok(Ok(head)) = tokio::time::timeout(Duration::from_secs(10), read_head(&mut stream)).await {
                let _ = respond(&mut stream, &head, &store).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(seed: u8, values: &[&str]) -> SignedPacket {
        SignedPacket::from_txt_strings(&SecretKey::from_bytes(&[seed; 32]), TXT_NAME, values, 60).unwrap()
    }

    /// FP-6.1-07：有效的记录存进去再取回；签名不对的拒绝；旧的不覆盖新的。
    #[test]
    fn store_checks_signatures_and_order() {
        let store = Store::default();
        let key = SecretKey::from_bytes(&[1; 32]).public().to_z32();
        assert_eq!(store.handle("GET", &key, b"").0, 404);
        let older = packet(1, &["a"]);
        std::thread::sleep(Duration::from_millis(2));
        let newer = packet(1, &["b"]);
        assert_eq!(store.handle("PUT", &key, &newer.to_relay_payload()).0, 200);
        let (status, body) = store.handle("GET", &key, b"");
        assert_eq!(status, 200);
        assert_eq!(SignedPacket::from_relay_payload(&SecretKey::from_bytes(&[1; 32]).public(), &body).unwrap().txt_records(TXT_NAME), ["b"]);
        assert_eq!(store.handle("PUT", &key, &older.to_relay_payload()).0, 409);
        // 别人的签名放在这个位置上：拒绝。
        let other = packet(2, &["c"]);
        assert_eq!(store.handle("PUT", &key, &other.to_relay_payload()).0, 400);
        let mut bad = newer.to_relay_payload();
        bad[10] ^= 1;
        assert_eq!(store.handle("PUT", &key, &bad).0, 400);
        assert_eq!(store.handle("GET", "not-a-key", b"").0, 400);
        assert_eq!(store.handle("DELETE", &key, b"").0, 405);
    }

    /// 数据经 TXT 记录往返；空数据是失效的记录。
    #[test]
    fn data_round_trips_through_txt_records() {
        let data: Vec<u8> = (0..=255).collect();
        let text = nexus_core_proto::joincode::base32_encode(&data);
        let chunks: Vec<String> = text.as_bytes().chunks(CHUNK).map(|c| String::from_utf8_lossy(c).into_owned()).collect();
        assert!(chunks.len() > 1);
        let p = SignedPacket::from_txt_strings(&SecretKey::from_bytes(&[3; 32]), TXT_NAME, &chunks, 60).unwrap();
        assert_eq!(decode(&p), Some(data));
        let empty = SignedPacket::from_txt_strings(&SecretKey::from_bytes(&[3; 32]), TXT_NAME, Vec::<String>::new(), 60).unwrap();
        assert_eq!(decode(&empty), None);
    }
}
