//! 锚点（M2.4）：同一个端口上，中继路径交给内置的 iroh 中继，其余路径是网页节点的页面。
//!
//! 中继在本机回环地址上以 http 运行；前端（这里）负责 TLS、按请求路径分流，
//! 把中继的连接原样转给它（含 WebSocket 升级）。中继对所有连接开放（浏览器的临时身份不在名册里），
//! 每个连接限速。

use crate::web_serve;
use futures_util::StreamExt;
use iroh_relay::server::{ClientRateLimit, Limits, RelayConfig, Server, ServerConfig};
use nexus_cli::AnchorArgs;
use std::io;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// 每个中继连接的速率上限和突发上限。
const RELAY_RATE: u32 = 2 << 20;
const RELAY_BURST: u32 = 8 << 20;

/// 交给中继的路径。
fn is_relay_path(target: &str) -> bool {
    let path = target.split('?').next().unwrap_or("");
    path == "/relay" || path.starts_with("/relay/") || path == "/ping" || path == "/generate_204" || path == "/healthz"
}

pub struct Anchor {
    /// 锚点地址：其他设备的中继地址，也是页面的地址。
    pub url: String,
    _relay: Server,
}

/// 参数错误（启动前就能发现的问题）。
pub struct BadArgs(pub String);

enum Tls {
    Dev,
    Manual(Arc<rustls::ServerConfig>),
    Acme { domain: String, contact: Option<String>, staging: bool, cache: PathBuf },
}

/// 检查参数，起中继和前端。`data_dir` 用来缓存证书。
pub async fn start(args: &AnchorArgs, data_dir: &Path) -> Result<Result<Anchor, io::Error>, BadArgs> {
    // 依赖里可能同时编进了几种加密实现：明确使用 ring。
    let _ = rustls::crypto::ring::default_provider().install_default();
    let page = args.page_dir.as_ref().ok_or_else(|| BadArgs("作为锚点运行需要 --page-dir（网页节点的页面目录）".into()))?;
    let page = std::fs::canonicalize(page)
        .ok()
        .filter(|p| p.join("index.html").is_file())
        .ok_or_else(|| BadArgs(format!("{} 里没有 index.html", page.display())))?;
    let parse = |s: &str, what: &str| s.parse::<SocketAddr>().map_err(|_| BadArgs(format!("{what} 应当是 地址:端口：{s}")));
    let (listen, redirect, tls, url) = if let Some(dev) = &args.anchor_dev {
        let addr = parse(dev, "--anchor-dev")?;
        if !addr.ip().is_loopback() {
            return Err(BadArgs(format!("开发用的锚点只能监听本机地址：{dev}")));
        }
        (addr, None, Tls::Dev, format!("http://localhost:{}", addr.port()))
    } else {
        let domain = args.anchor.clone().ok_or_else(|| BadArgs("缺少 --anchor 或 --anchor-dev".into()))?;
        let url = format!("https://{domain}");
        if !nexus_core_proto::roster::valid_anchor_url(&url) {
            return Err(BadArgs(format!("域名不合法：{domain}")));
        }
        let tls = match (&args.tls_cert, &args.tls_key) {
            (Some(cert), Some(key)) => Tls::Manual(Arc::new(manual_tls(cert, key).map_err(BadArgs)?)),
            _ => Tls::Acme { domain, contact: args.acme_contact.clone(), staging: args.acme_staging, cache: data_dir.join("acme") },
        };
        (parse(&args.https_listen, "--https-listen")?, Some(parse(&args.http_listen, "--http-listen")?), tls, url)
    };
    Ok(run(page, listen, redirect, tls, url).await)
}

async fn run(page: PathBuf, listen: SocketAddr, redirect: Option<SocketAddr>, tls: Tls, url: String) -> io::Result<Anchor> {
    let mut rate = ClientRateLimit::new(NonZeroU32::new(RELAY_RATE).expect("非零"));
    rate.max_burst_bytes = NonZeroU32::new(RELAY_BURST);
    let mut limits = Limits::default();
    limits.client_rx = Some(rate);
    let mut relay_config = RelayConfig::new(SocketAddr::from(([127, 0, 0, 1], 0)));
    relay_config.limits = limits;
    let mut config = ServerConfig::default();
    config.relay = Some(relay_config);
    let relay = Server::spawn(config).await.map_err(|e| io::Error::other(format!("无法启动中继：{e}")))?;
    let relay_addr = relay.http_addr().ok_or_else(|| io::Error::other("中继没有监听地址"))?;

    let listener = TcpListener::bind(listen).await.map_err(|e| io::Error::new(e.kind(), format!("无法监听 {listen}：{e}")))?;
    if let Some(addr) = redirect {
        let http = TcpListener::bind(addr).await.map_err(|e| io::Error::new(e.kind(), format!("无法监听 {addr}：{e}")))?;
        tokio::spawn(redirect_to_https(http, url.clone()));
    }
    let page = Arc::new(page);
    match tls {
        Tls::Dev => {
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else { continue };
                    let page = page.clone();
                    tokio::spawn(async move {
                        let _ = front(stream, &page, relay_addr).await;
                    });
                }
            });
        }
        Tls::Manual(config) => {
            let acceptor = tokio_rustls::TlsAcceptor::from(config);
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else { continue };
                    let (page, acceptor) = (page.clone(), acceptor.clone());
                    tokio::spawn(async move {
                        if let Ok(Ok(tls)) = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                            let _ = front(tls, &page, relay_addr).await;
                        }
                    });
                }
            });
        }
        Tls::Acme { domain, contact, staging, cache } => {
            let mut acme = tokio_rustls_acme::AcmeConfig::new([domain])
                .cache(tokio_rustls_acme::caches::DirCache::new(cache))
                .directory_lets_encrypt(!staging);
            if let Some(c) = contact {
                acme = acme.contact_push(format!("mailto:{c}"));
            }
            let mut state = acme.state();
            let acceptor = state.acceptor();
            let config = Arc::new(rustls::ServerConfig::builder().with_no_client_auth().with_cert_resolver(state.resolver()));
            tokio::spawn(async move {
                while let Some(event) = state.next().await {
                    match event {
                        Ok(ok) => eprintln!("nexus anchor: 证书：{ok:?}"),
                        Err(e) => eprintln!("nexus anchor: 证书出错：{e}"),
                    }
                }
            });
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else { continue };
                    let (page, acceptor, config) = (page.clone(), acceptor.clone(), config.clone());
                    tokio::spawn(async move {
                        let accepted = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
                            match acceptor.accept(stream).await {
                                // 证书验证用的连接，已经处理完。
                                Ok(None) => None,
                                Ok(Some(start)) => start.into_stream(config).await.ok(),
                                Err(_) => None,
                            }
                        })
                        .await;
                        if let Ok(Some(tls)) = accepted {
                            let _ = front(tls, &page, relay_addr).await;
                        }
                    });
                }
            });
        }
    }
    Ok(Anchor { url, _relay: relay })
}

const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 读请求头，按路径交给中继或回应页面。
async fn front<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, page: &Path, relay: SocketAddr) -> io::Result<()> {
    let head = tokio::time::timeout(HANDSHAKE_TIMEOUT, web_serve::read_head(&mut stream))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "请求头超时"))??;
    let (_, target) = web_serve::request_line(&head);
    if is_relay_path(&target) {
        let mut upstream = TcpStream::connect(relay).await?;
        // 按连接上的第一个请求分流：不是协议升级的请求（探测）回应后必须关闭连接，
        // 否则浏览器复用这条连接请求页面时会落到中继那里。
        let head = if is_upgrade(&head) { head } else { force_close(&head) };
        upstream.write_all(&head).await?;
        tokio::io::copy_bidirectional(&mut stream, &mut upstream).await?;
        Ok(())
    } else {
        web_serve::respond(&mut stream, page, &head).await
    }
}

/// 请求头里是否要求协议升级（中继的 WebSocket 或 HTTP 升级）。
fn is_upgrade(head: &[u8]) -> bool {
    header_lines(head).any(|l| l.split_once(':').is_some_and(|(k, _)| k.trim().eq_ignore_ascii_case("upgrade")))
}

fn header_lines(head: &[u8]) -> impl Iterator<Item = &str> {
    let end = head.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(head.len());
    std::str::from_utf8(&head[..end]).unwrap_or("").split("\r\n").skip(1)
}

/// 把请求头改成回应后关闭连接（去掉原有的 Connection 头，加上 `Connection: close`）。
fn force_close(head: &[u8]) -> Vec<u8> {
    let Some(end) = head.windows(4).position(|w| w == b"\r\n\r\n") else { return head.to_vec() };
    let text = String::from_utf8_lossy(&head[..end]);
    let mut lines = text.split("\r\n");
    let mut out = String::from(lines.next().unwrap_or(""));
    for l in lines {
        if !l.split_once(':').is_some_and(|(k, _)| k.trim().eq_ignore_ascii_case("connection")) {
            out.push_str("\r\n");
            out.push_str(l);
        }
    }
    out.push_str("\r\nConnection: close\r\n\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(&head[end + 4..]);
    bytes
}

/// 80 端口：一律重定向到 https。
async fn redirect_to_https(listener: TcpListener, url: String) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else { continue };
        let url = url.clone();
        tokio::spawn(async move {
            let Ok(Ok(head)) = tokio::time::timeout(HANDSHAKE_TIMEOUT, web_serve::read_head(&mut stream)).await else { return };
            let (_, target) = web_serve::request_line(&head);
            let path = if target.starts_with('/') { target } else { "/".to_string() };
            let reply = format!("HTTP/1.1 301 Moved Permanently\r\nLocation: {url}{path}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = stream.write_all(reply.as_bytes()).await;
            let _ = stream.shutdown().await;
        });
    }
}

fn manual_tls(cert: &Path, key: &Path) -> Result<rustls::ServerConfig, String> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .and_then(|it| it.collect::<Result<_, _>>())
        .map_err(|e| format!("读不了证书 {}：{e}", cert.display()))?;
    if chain.is_empty() {
        return Err(format!("{} 里没有证书", cert.display()));
    }
    let key = PrivateKeyDer::from_pem_file(key).map_err(|e| format!("读不了私钥 {}：{e}", key.display()))?;
    rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(|e| format!("证书和私钥不匹配：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_paths_go_to_the_relay() {
        for p in ["/relay", "/relay?x=1", "/relay/x", "/ping", "/generate_204", "/healthz"] {
            assert!(is_relay_path(p), "{p}");
        }
        for p in ["/", "/index.html", "/relayx", "/pkg/relay", "/app.js"] {
            assert!(!is_relay_path(p), "{p}");
        }
    }

    #[test]
    fn probes_are_forwarded_with_connection_close() {
        let head = b"GET /ping HTTP/1.1\r\nHost: x\r\nconnection: keep-alive\r\n\r\n";
        assert!(!is_upgrade(head));
        assert_eq!(force_close(head), b"GET /ping HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n".to_vec());
        let ws = b"GET /relay HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n";
        assert!(is_upgrade(ws));
    }
}
