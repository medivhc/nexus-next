//! 锚点（M2.4）。功能点见 docs/feature-points/M2.4.md。

use nexus_core_proto::webauthn::soft::SoftAuthenticator;
use nexus_testkit::{scenario, World};
use std::time::Duration;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// 一个 HTTP GET，返回（状态码，Content-Type，正文）。
fn http_get(port: u16, path: &str) -> (u16, String, Vec<u8>) {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).expect("连不上锚点");
    write!(s, "GET {path} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n").unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("回应没有头部");
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).expect("没有状态码");
    let ctype = head
        .lines()
        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-type:").map(|v| v.trim().to_string()))
        .unwrap_or_default();
    (status, ctype, raw[split + 4..].to_vec())
}

/// 经 TLS（只信任 `ca`）发一个 HTTP GET，返回状态码和正文。
async fn https_get(port: u16, path: &str, ca: &rcgen::Certificate) -> (u16, Vec<u8>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.der().clone()).unwrap();
    let config = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.expect("连不上锚点");
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut tls = connector.connect(name, tcp).await.expect("TLS 握手失败");
    tls.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    let _ = tls.read_to_end(&mut raw).await;
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("回应没有头部");
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    (head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap(), raw[split + 4..].to_vec())
}

/// 一个输出 `text` 的命令，但命令本身（回显在屏幕上）不包含完整的 `text`。
fn say(text: &str) -> String {
    let (a, b) = text.split_at(text.len() / 2);
    format!("printf '%s%s\\n' {a} {b}\r")
}

scenario! {
    name: anchor_serves_the_page_and_relays_browsers,
    covers: "R35, R28",
    nodes: ["laptop", "home", "anchor"],
    sim: (pending "锚点是真实的 HTTP 服务和中继，只在真实后端运行"),
    real: yes,
    run: |w: World| async move {
        let page = w.home_dir("anchor").join("page");
        std::fs::create_dir_all(page.join("pkg")).unwrap();
        std::fs::write(page.join("index.html"), "<!doctype html><title>nexus</title>").unwrap();
        std::fs::write(page.join("pkg/nexus_web_bg.wasm"), b"\0asm\x01\0\0\0").unwrap();
        let page = page.to_str().unwrap().to_string();
        let port = free_port();
        let listen = format!("127.0.0.1:{port}");
        let url = format!("http://localhost:{port}");

        // 参数不对时立即失败。
        let out = w.cli("anchor", &["daemon", "--anchor-dev", &listen]).await;
        assert_eq!(out.code, 107, "{}", out.stderr);
        let empty = w.home_dir("anchor").join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let out = w.cli("anchor", &["daemon", "--anchor-dev", &listen, "--page-dir", empty.to_str().unwrap()]).await;
        assert_eq!(out.code, 107, "{}", out.stderr);
        let out = w.cli("anchor", &["daemon", "--anchor-dev", "0.0.0.0:0", "--page-dir", &page]).await;
        assert_eq!(out.code, 107, "{}", out.stderr);

        // 一条命令起锚点：同一个端口上是页面和中继。
        w.restart("anchor", &["--anchor-dev", &listen, "--page-dir", &page, "--no-discovery"]).await;
        let (code, ctype, body) = http_get(port, "/");
        assert_eq!((code, body.as_slice()), (200, &b"<!doctype html><title>nexus</title>"[..]));
        assert!(ctype.starts_with("text/html"), "{ctype}");
        assert_eq!(http_get(port, "/pkg/nexus_web_bg.wasm").1, "application/wasm");
        assert_eq!(http_get(port, "/../secret").0, 404);

        // 其他设备只知道锚点这一个中继。
        for node in ["laptop", "home"] {
            w.restart(node, &["--relay", &url, "--no-discovery"]).await;
        }
        w.network("laptop", &["home", "anchor"]).await;
        let v = w.cli_ok("laptop", &["anchor", "set", &url]).await;
        assert_eq!(v["pushed"], 2, "{v}");

        // 只经中继的浏览器：经锚点登记、登录、进入 home 的会话。
        let mut phone = SoftAuthenticator::new([42; 32], "localhost", &url);
        let link = w.cli_ok("laptop", &["web", "enroll", "--label", "phone"]).await["url"].as_str().unwrap().to_string();
        assert!(link.starts_with(&format!("{url}/#enroll=")), "{link}");
        w.browser_via(&url).await.enroll(&link, &phone).await.expect("经锚点登记");
        let browser = w.browser_via(&url).await;
        browser.login(&w, &link, &mut phone).await.expect("经锚点登录");
        assert_eq!(browser.devices(), ["anchor", "home", "laptop"]);
        let t = browser.term("home", "main").await.expect("经锚点连上 home 的会话");
        t.expect_prompt().await;
        t.send(&say("through-the-anchor")).await;
        t.expect_line_within("through-the-anchor", Duration::from_secs(10)).await;
        assert_eq!(t.detach().await, 0);
    },
}

/// 测试里的锚点地址：本机一个没人监听的端口，设备把它加为中继也不会连到任何外部服务。
const ANCHOR: &str = "http://localhost:9";

scenario! {
    name: the_anchor_is_shared_through_the_roster,
    covers: "R35",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        assert_eq!(w.cli_ok("home", &["anchor", "show"]).await["anchor"], serde_json::Value::Null);

        // 只有管理设备能设置；地址必须是 https（本机开发可以用 http://localhost）。
        assert_eq!(w.cli("home", &["anchor", "set", ANCHOR]).await.code, 106);
        for bad in ["http://example.com", "http://127.0.0.1:9", "https://a/b", "nonsense"] {
            let out = w.cli("laptop", &["anchor", "set", bad]).await;
            assert_eq!(out.code, 107, "{bad}: {}", out.stderr);
        }

        let v = w.cli_ok("laptop", &["anchor", "set", ANCHOR]).await;
        assert_eq!(v["anchor"], ANCHOR);
        assert_eq!(v["pushed"], 1, "{v}");
        assert_eq!(w.cli_ok("home", &["anchor", "show"]).await["anchor"], ANCHOR);
        let text = w.cli("home", &["anchor", "show"]).await;
        assert!(text.stdout.contains(ANCHOR), "{}", text.stdout);

        // 登记链接默认指向锚点上的页面；明确给出 --page 时用给出的。
        let link = w.cli_ok("laptop", &["web", "enroll", "--label", "phone"]).await;
        assert!(link["url"].as_str().unwrap().starts_with(&format!("{ANCHOR}/#enroll=")), "{link}");
        let link = w.cli_ok("laptop", &["web", "enroll", "--label", "tablet", "--page", "https://bewith.link"]).await;
        assert!(link["url"].as_str().unwrap().starts_with("https://bewith.link/#enroll="), "{link}");

        // 清除之后回到默认页面。
        let v = w.cli_ok("laptop", &["anchor", "set", "--clear"]).await;
        assert_eq!(v["anchor"], serde_json::Value::Null);
        assert_eq!(w.cli_ok("home", &["anchor", "show"]).await["anchor"], serde_json::Value::Null);
        let link = w.cli_ok("laptop", &["web", "enroll", "--label", "pad"]).await;
        assert!(link["url"].as_str().unwrap().starts_with("https://bewith.link/#enroll="), "{link}");
    },
}

scenario! {
    name: anchor_serves_https_with_a_given_certificate,
    covers: "R35",
    nodes: ["anchor"],
    sim: (pending "锚点是真实的 HTTPS 服务，只在真实后端运行"),
    real: yes,
    run: |w: World| async move {
        let dir = w.home_dir("anchor");
        let page = dir.join("page");
        std::fs::create_dir_all(&page).unwrap();
        std::fs::write(page.join("index.html"), "hello over tls").unwrap();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let (cert_path, key_path) = (dir.join("cert.pem"), dir.join("key.pem"));
        std::fs::write(&cert_path, cert.cert.pem()).unwrap();
        std::fs::write(&key_path, cert.signing_key.serialize_pem()).unwrap();
        let (https, http) = (free_port(), free_port());
        let (https_listen, http_listen) = (format!("127.0.0.1:{https}"), format!("127.0.0.1:{http}"));
        let p = |x: &std::path::Path| x.to_str().unwrap().to_string();
        let args = |key: &str| {
            vec![
                "--anchor".to_string(), "localhost".into(), "--page-dir".into(), p(&page),
                "--tls-cert".into(), p(&cert_path), "--tls-key".into(), key.to_string(),
                "--https-listen".into(), https_listen.clone(), "--http-listen".into(), http_listen.clone(),
                "--no-discovery".into(),
            ]
        };

        // 私钥读不了：立即失败。
        let bad: Vec<String> = ["daemon".to_string()].into_iter().chain(args("/nonexistent/key.pem")).collect();
        let bad: Vec<&str> = bad.iter().map(String::as_str).collect();
        let out = w.cli("anchor", &bad).await;
        assert_eq!(out.code, 107, "{}", out.stderr);

        let good = args(&p(&key_path));
        let good: Vec<&str> = good.iter().map(String::as_str).collect();
        w.restart("anchor", &good).await;
        let (code, body) = https_get(https, "/", &cert.cert).await;
        assert_eq!((code, body.as_slice()), (200, &b"hello over tls"[..]), "{}", w.daemon_log("anchor"));
        assert_eq!(https_get(https, "/nope", &cert.cert).await.0, 404);

        // http 一律重定向到 https。
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(("127.0.0.1", http)).unwrap();
        write!(s, "GET /x?y=1 HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut raw = String::new();
        s.read_to_string(&mut raw).unwrap();
        assert!(raw.starts_with("HTTP/1.1 301"), "{raw}");
        assert!(raw.contains("Location: https://localhost/x?y=1\r\n"), "{raw}");
    },
}
