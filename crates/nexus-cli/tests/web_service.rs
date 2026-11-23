//! 网页端打开已发布的网页服务（M8.2，R71）。功能点见 docs/feature-points/M8.2.md。

use nexus_core_proto::webauthn::soft::SoftAuthenticator;
use nexus_testkit::nexus_web_core::{rewrite_response_headers, WebHttpRequest};
use nexus_testkit::{scenario, Service, World};
use nexus_wire::ErrorCode;

fn passkey(seed: u8) -> SoftAuthenticator {
    SoftAuthenticator::new([seed; 32], "bewith.link", "https://bewith.link")
}

async fn enrolled(w: &World, label: &str, grants: &[&str], auth: &SoftAuthenticator) -> String {
    let mut args = vec!["web", "enroll", "--label", label];
    for g in grants {
        args.extend(["--grant", g]);
    }
    let v = w.cli_ok("laptop", &args).await;
    let link = v["url"].as_str().expect("应当输出登记链接").to_string();
    w.browser().await.enroll(&link, auth).await.expect("登记应当成功");
    link
}

fn get(path: &str) -> WebHttpRequest {
    WebHttpRequest { method: "GET".into(), path: path.into(), headers: vec![], body: vec![] }
}

scenario! {
    name: web_service_respects_minimal_grants_and_audits,
    covers: "R71, R33, R37",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let web_port = w.serve("home", Service::Http("hello from web".into())).await.to_string();
        let admin_port = w.serve("home", Service::Http("secret admin".into())).await.to_string();
        let echo_port = w.serve("home", Service::Echo).await.to_string();
        w.cli_ok("home", &["publish", "web", "--port", &web_port, "--proto", "http"]).await;
        w.cli_ok("home", &["publish", "admin", "--port", &admin_port, "--proto", "http"]).await;
        w.cli_ok("home", &["publish", "echo", "--port", &echo_port, "--proto", "tcp"]).await;

        // FP-8.2-01：默认通行密钥不含 svc.connect，看不到任何服务，直接访问被拒。
        let mut default_key = passkey(61);
        let entry = enrolled(&w, "default-svc", &[], &default_key).await;
        let b_default = w.browser().await;
        b_default.login(&w, &entry, &mut default_key).await.unwrap();
        assert!(b_default.services("home").await.unwrap().is_empty());
        assert_eq!(b_default.http_fetch("home", "web", &get("/")).await.unwrap_err().code, ErrorCode::Denied);

        // 仅授予 home/svc/web 权限 + node.info。
        let mut web_key = passkey(62);
        let entry = enrolled(&w, "web-viewer", &["node.info@**", "svc.connect@home/svc/web"], &web_key).await;
        let b = w.browser().await;
        b.login(&w, &entry, &mut web_key).await.unwrap();
        let svcs = b.services("home").await.unwrap();
        assert_eq!(svcs.len(), 1);
        assert_eq!((svcs[0].name.as_str(), svcs[0].proto.as_str()), ("web", "http"));

        let resp = b.http_fetch("home", "web", &get("/")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(String::from_utf8_lossy(&resp.body), "hello from web");

        assert_eq!(b.http_fetch("home", "admin", &get("/")).await.unwrap_err().code, ErrorCode::Denied);

        // 目标设备审计中记录了允许与拒绝。
        let audit = w.cli_ok("home", &["audit"]).await;
        let events = audit["events"].as_array().unwrap();
        assert!(
            events.iter().any(|e| e["principal"]["passkey"] == "web-viewer"
                && e["capability"] == "svc.connect"
                && e["resource"] == "home/svc/web"
                && e["outcome"] == "allow"),
            "{audit}"
        );
        assert!(
            events.iter().any(|e| e["principal"]["passkey"] == "web-viewer"
                && e["capability"] == "svc.connect"
                && e["resource"] == "home/svc/admin"
                && e["outcome"] == "deny"),
            "{audit}"
        );
    },
}

scenario! {
    name: web_service_forwards_http_and_rewrites_headers,
    covers: "R71",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let port = w.serve("home", Service::Http("dashboard ok".into())).await.to_string();
        w.cli_ok("home", &["publish", "app", "--port", &port, "--proto", "http"]).await;

        let mut key = passkey(63);
        let entry = enrolled(&w, "phone", &["node.info@**", "svc.connect@**"], &key).await;
        let browser = w.browser().await;
        browser.login(&w, &entry, &mut key).await.unwrap();

        // FP-8.2-02：HTTP/1.1 请求与响应转发。
        let req = WebHttpRequest {
            method: "GET".into(),
            path: "/index.html?x=1".into(),
            headers: vec![("X-From".into(), "nexus-web".into())],
            body: vec![],
        };
        let mut resp = browser.http_fetch("home", "app", &req).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"dashboard ok");

        // FP-8.2-03：响应头重定向与 Cookie 路径隔离。
        resp.headers.push(("Location".into(), "/login".into()));
        resp.headers.push(("Set-Cookie".into(), "sid=42; Path=/; HttpOnly".into()));
        rewrite_response_headers(&mut resp, "home", "app");
        assert!(resp.headers.iter().any(|(k, v)| k == "Location" && v == "/_svc/home/app/login"));
        assert!(resp.headers.iter().any(|(k, v)| k == "Set-Cookie" && v == "sid=42; Path=/_svc/home/app/; HttpOnly"));
    },
}

scenario! {
    name: web_service_reports_unreachable_when_backend_down,
    covers: "R71",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let dead = w.unused_port().to_string();
        w.cli_ok("home", &["publish", "dead", "--port", &dead, "--proto", "http"]).await;

        let mut key = passkey(64);
        let entry = enrolled(&w, "phone", &["node.info@**", "svc.connect@**"], &key).await;
        let browser = w.browser().await;
        browser.login(&w, &entry, &mut key).await.unwrap();

        // FP-8.2-04：目标服务未运行时明确返回 Unreachable，不存在的服务返回 NotFound。
        assert_eq!(browser.http_fetch("home", "dead", &get("/")).await.unwrap_err().code, ErrorCode::Unreachable);
        assert_eq!(browser.http_fetch("home", "nope", &get("/")).await.unwrap_err().code, ErrorCode::NotFound);
    },
}

scenario! {
    name: web_service_stops_after_passkey_revocation,
    covers: "R71, R31",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let port = w.serve("home", Service::Http("live".into())).await.to_string();
        w.cli_ok("home", &["publish", "web", "--port", &port, "--proto", "http"]).await;

        let mut key = passkey(65);
        let entry = enrolled(&w, "phone", &["node.info@**", "svc.connect@**"], &key).await;
        let browser = w.browser().await;
        browser.login(&w, &entry, &mut key).await.unwrap();
        assert_eq!(browser.http_fetch("home", "web", &get("/")).await.unwrap().body, b"live");

        // FP-8.2-05：撤销通行密钥后立即拒绝后续请求。
        w.cli_ok("laptop", &["web", "revoke", "phone"]).await;
        assert_eq!(browser.services("home").await.unwrap_err().code, ErrorCode::Denied);
        assert_eq!(browser.http_fetch("home", "web", &get("/")).await.unwrap_err().code, ErrorCode::Denied);
    },
}
