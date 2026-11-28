//! 网页里打开已发布的网页服务（M8.2，R71）。功能点见 docs/feature-points/M8.2.md。
//! 测试工具里的浏览器跑的是页面用的同一份网页节点核心；页面本身（转发程序、cookie、跳转）在 tools/web-e2e。

use nexus_core_proto::webauthn::soft::SoftAuthenticator;
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
    w.browser()
        .await
        .enroll(&link, auth)
        .await
        .expect("登记应当成功");
    link
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

async fn setup(w: &World) {
    w.network("laptop", &["home"]).await;
    let web = w.serve("home", Service::HttpEcho).await.to_string();
    w.cli_ok(
        "home",
        &["publish", "web", "--port", &web, "--proto", "http"],
    )
    .await;
    let echo = w.serve("home", Service::Echo).await.to_string();
    w.cli_ok("home", &["publish", "echo", "--port", &echo])
        .await;
    let dead = w.unused_port().to_string();
    w.cli_ok(
        "home",
        &["publish", "dead", "--port", &dead, "--proto", "http"],
    )
    .await;
}

scenario! {
    name: web_reaches_a_published_web_service,
    covers: "R71",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        setup(&w).await;
        let mut phone = passkey(52);
        let entry = enrolled(&w, "phone", &["node.info@**", "svc.connect@**"], &phone).await;
        let browser = w.browser().await;
        browser.login(&w, &entry, &mut phone).await.expect("登录应当成功");

        // FP-8.2-01
        let services = browser.services("home").await.expect("能列出服务");
        let mut list: Vec<(&str, &str)> = services.iter().map(|s| (s.name.as_str(), s.proto.as_str())).collect();
        list.sort();
        assert_eq!(list, [("dead", "http"), ("echo", "tcp"), ("web", "http")]);

        let r = browser.http("home", "web", "GET", "/hello?x=1", &[], b"").await.expect("GET 应当成功");
        assert_eq!(r.status, 200);
        assert!(String::from_utf8_lossy(&r.body).starts_with("GET /hello?x=1\n"), "{:?}", String::from_utf8_lossy(&r.body));
        assert_eq!(header(&r.headers, "set-cookie"), Some("visited=yes; Path=/"), "{:?}", r.headers);
        assert_eq!(header(&r.headers, "transfer-encoding"), None, "{:?}", r.headers);

        let cookie = [("Cookie".to_string(), "visited=yes".to_string()), ("Content-Type".to_string(), "application/x-www-form-urlencoded".to_string())];
        let r = browser.http("home", "web", "POST", "/form", &cookie, b"a=1").await.unwrap();
        let text = String::from_utf8_lossy(&r.body).to_string();
        assert!(text.contains("POST /form") && text.contains("cookie: visited=yes") && text.contains("body: a=1"), "{text}");

        let r = browser.http("home", "web", "GET", "/go", &[], b"").await.unwrap();
        assert_eq!((r.status, header(&r.headers, "location")), (302, Some("/landed")));

        // FP-8.2-02
        assert_eq!(browser.http("home", "echo", "GET", "/", &[], b"").await.unwrap_err().code, ErrorCode::InvalidInput);
        assert_eq!(browser.http("home", "nope", "GET", "/", &[], b"").await.unwrap_err().code, ErrorCode::NotFound);
        assert_eq!(browser.http("home", "dead", "GET", "/", &[], b"").await.unwrap_err().code, ErrorCode::Unreachable);
    },
}

scenario! {
    name: web_service_needs_a_grant,
    covers: "R71",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        setup(&w).await;
        let mut plain = passkey(53);
        let entry = enrolled(&w, "plain", &[], &plain).await;
        let browser = w.browser().await;
        browser.login(&w, &entry, &mut plain).await.unwrap();
        // FP-8.2-02：默认授权
        assert!(browser.services("home").await.unwrap().is_empty());
        assert_eq!(browser.http("home", "web", "GET", "/", &[], b"").await.unwrap_err().code, ErrorCode::Denied);
        let audit = w.cli_ok("home", &["audit"]).await;
        assert!(
            audit["events"].as_array().unwrap().iter().any(|e| e["principal"]["passkey"] == "plain" && e["capability"] == "svc.connect"),
            "{audit}"
        );
    },
}
