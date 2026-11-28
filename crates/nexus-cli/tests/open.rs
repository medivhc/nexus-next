//! 在浏览器里打开网页服务（M7.4，R69）。功能点见 docs/feature-points/M7.4.md。
//! 只在真实后端：要真的在本机开端口、执行浏览器程序。「浏览器」是一个把网址记到文件里的脚本。

use nexus_testkit::{scenario, Service, World};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 一个「浏览器」：把收到的网址写到 `<目录>/opened`。返回脚本路径和那个文件。
fn fake_browser(dir: &Path) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let log = dir.join("opened");
    let script = dir.join("browser");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nprintf '%s\\n' \"$1\" > {}\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (script, log)
}

/// 等 `f` 给出值，最多 10 秒。
async fn eventually<T>(w: &World, what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    for _ in 0..200 {
        if let Some(v) = f() {
            return v;
        }
        w.run_for(Duration::from_millis(50)).await;
    }
    panic!("等不到：{what}");
}

/// 网址 `http://127.0.0.1:<端口><路径>` 拆成端口和路径。
fn split(url: &str) -> (u16, String) {
    let rest = url
        .strip_prefix("http://127.0.0.1:")
        .unwrap_or_else(|| panic!("网址应当是本机的：{url}"));
    let (port, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    (
        port.parse()
            .unwrap_or_else(|_| panic!("网址里的端口不对：{url}")),
        if path.is_empty() {
            "/".into()
        } else {
            path.into()
        },
    )
}

/// 用 `BROWSER` 打开，返回浏览器收到的网址。
async fn open_with_browser(w: &World, args: &[&str]) -> (String, nexus_testkit::Background) {
    let dir = w.home_dir("laptop").join(format!(
        "browser-{}",
        args.join("_").replace(['/', '?', '='], "_")
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let (browser, log) = fake_browser(&dir);
    let mut full = vec!["open"];
    full.extend_from_slice(args);
    let bg = w.spawn_cli_env("laptop", &full, &[("BROWSER", browser.to_str().unwrap())]);
    let url = eventually(w, "浏览器收到网址", || {
        std::fs::read_to_string(&log)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    })
    .await;
    (url, bg)
}

scenario! {
    name: open_shows_a_web_service_in_the_browser,
    covers: "R69",
    nodes: ["laptop", "home"],
    sim: (pending "要真的在本机开端口、执行浏览器程序"),
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let port = w.serve("home", Service::Http("hello from home".into())).await.to_string();
        w.cli_ok("home", &["publish", "web", "--port", &port, "--proto", "http"]).await;
        w.cli_ok("home", &["publish", "api", "--port", &port, "--proto", "http", "--env", "BASE_URL=http://{addr}/v1"]).await;

        // FP-7.4-01
        let (url, bg) = open_with_browser(&w, &["home/web"]).await;
        let (local, path) = split(&url);
        assert_eq!(path, "/", "{url}");
        for _ in 0..3 {
            assert_eq!(nexus_testkit::http_get(local, &path).as_deref().ok(), Some("hello from home"), "{url}");
        }
        assert!(bg.stdout().contains(&url), "标准输出里应当有网址：{}", bg.stdout());
        bg.interrupt();

        // FP-7.4-02：带路径
        let (url, bg) = open_with_browser(&w, &["home/web/admin?tab=2"]).await;
        assert!(url.ends_with("/admin?tab=2"), "{url}");
        let (local, path) = split(&url);
        assert_eq!(nexus_testkit::http_get(local, &path).as_deref().ok(), Some("hello from home"));
        bg.interrupt();
        // 按服务的说明拼网址
        let (url, bg) = open_with_browser(&w, &["home/api"]).await;
        assert!(url.ends_with("/v1"), "{url}");
        bg.interrupt();
        // 不打开浏览器，指定端口
        let fixed = w.unused_port();
        let dir = w.home_dir("laptop").join("no-browser");
        std::fs::create_dir_all(&dir).unwrap();
        let (browser, log) = fake_browser(&dir);
        let p = fixed.to_string();
        let bg = w.spawn_cli_env("laptop", &["open", "home/web", "--no-browser", "--port", &p], &[("BROWSER", browser.to_str().unwrap())]);
        let want = format!("http://127.0.0.1:{fixed}/");
        eventually(&w, "标准输出里的网址", || bg.stdout().contains(&want).then_some(())).await;
        assert_eq!(nexus_testkit::http_get(fixed, "/").as_deref().ok(), Some("hello from home"));
        assert!(!log.exists(), "--no-browser 不应当打开浏览器");
        bg.interrupt();
    },
}

scenario! {
    name: open_refuses_what_it_cannot_show,
    covers: "R69",
    nodes: ["laptop", "home"],
    sim: (pending "要真的在本机开端口、执行浏览器程序"),
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let echo = w.serve("home", Service::Echo).await.to_string();
        w.cli_ok("home", &["publish", "echo", "--port", &echo]).await;
        let web = w.serve("home", Service::Http("hello from home".into())).await.to_string();
        w.cli_ok("home", &["publish", "web", "--port", &web, "--proto", "http"]).await;

        // FP-7.4-03
        assert_eq!(w.cli("laptop", &["open", "home/nope", "--no-browser"]).await.code, 109);
        assert_eq!(w.cli("laptop", &["open", "home", "--no-browser"]).await.code, 107);
        let out = w.cli("laptop", &["open", "home/echo", "--no-browser"]).await;
        assert_eq!(out.code, 107, "{}", out.stderr);
        assert!(out.stderr.contains("nexus connect"), "{}", out.stderr);
        assert!(out.stdout.is_empty(), "不应当开端口：{}", out.stdout);
        // 浏览器打不开：照样服务
        let bg = w.spawn_cli_env("laptop", &["open", "home/web"], &[("BROWSER", "/nonexistent/browser")]);
        let url = eventually(&w, "标准输出里的网址", || {
            bg.stdout().split_whitespace().find(|t| t.starts_with("http://")).map(|t| t.split('（').next().unwrap().to_string())
        })
        .await;
        let (local, path) = split(&url);
        assert_eq!(nexus_testkit::http_get(local, &path).as_deref().ok(), Some("hello from home"));
        eventually(&w, "说明打不开浏览器", || bg.stderr().contains("浏览器").then_some(())).await;
        bg.interrupt();
    },
}
