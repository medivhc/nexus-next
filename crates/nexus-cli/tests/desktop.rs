//! 远程桌面：让现成的 VNC / RDP 经 Nexus 连起来（M11.1，R77）。功能点见 docs/feature-points/M11.1.md。
//! 只在真实后端：要真的在本机开端口、执行查看器程序。「查看器」是一个把参数记到文件里的脚本，
//! 「VNC 服务端」是测试里的一个 TCP 服务：先说 `RFB 003.008\n`，之后把收到的原样送回。

use nexus_testkit::{scenario, Service, World};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

const GREETING: &[u8] = b"RFB 003.008\n";

/// 一个假的 VNC 服务端，只监听本机。返回端口。
fn fake_vnc() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in listener.incoming() {
            let Ok(mut s) = s else { continue };
            std::thread::spawn(move || {
                if s.write_all(GREETING).is_err() {
                    return;
                }
                let mut buf = [0u8; 4096];
                while let Ok(n) = s.read(&mut buf) {
                    if n == 0 || s.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            });
        }
    });
    port
}

/// 一个只接受连接、从不说话的服务。
fn silent_service() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for s in listener.incoming().flatten() {
            held.push(s);
        }
    });
    port
}

/// 一个「查看器」：把收到的最后一个参数写到 `<目录>/opened`。
fn fake_viewer(dir: &Path) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).unwrap();
    let log = dir.join("opened");
    let script = dir.join("viewer");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nfor a; do last=\"$a\"; done\nprintf '%s\\n' \"$last\" > {}\n",
            log.display()
        ),
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

/// `vnc://127.0.0.1:<端口>` 里的端口。
fn port_of(url: &str) -> u16 {
    let rest = url
        .strip_prefix("vnc://127.0.0.1:")
        .unwrap_or_else(|| panic!("地址应当是本机的 vnc://：{url}"));
    rest.parse()
        .unwrap_or_else(|_| panic!("地址里的端口不对：{url}"))
}

/// 连本机端口：读到问候，写一句话再读回来。
fn talk(port: u16) -> (Vec<u8>, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut hello = vec![0u8; GREETING.len()];
    s.read_exact(&mut hello).unwrap();
    s.write_all(b"ping").unwrap();
    let mut back = vec![0u8; 4];
    s.read_exact(&mut back).unwrap();
    (hello, back)
}

/// 用 `NEXUS_DESKTOP_VIEWER` 运行，返回查看器收到的地址。
async fn open_with_viewer(
    w: &World,
    tag: &str,
    args: &[&str],
) -> (String, nexus_testkit::Background) {
    let (viewer, log) = fake_viewer(&w.home_dir("laptop").join(format!("viewer-{tag}")));
    let bg = w.spawn_cli_env(
        "laptop",
        args,
        &[("NEXUS_DESKTOP_VIEWER", viewer.to_str().unwrap())],
    );
    let url = eventually(w, "查看器收到地址", || {
        std::fs::read_to_string(&log)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    })
    .await;
    (url, bg)
}

scenario! {
    name: desktop_share_publishes_the_local_desktop,
    covers: "R77",
    nodes: ["laptop", "home"],
    sim: (pending "要真的在本机探测端口"),
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let vnc = fake_vnc().to_string();
        let silent = silent_service().to_string();

        // FP-11.1-01
        let v = w.cli_ok("home", &["desktop", "share", "--port", &vnc]).await;
        assert_eq!(v["path"], "home/desktop", "{v}");
        assert_eq!(v["proto"], "vnc", "{v}");
        let out = w.cli("home", &["desktop", "share", "--name", "screen", "--port", &vnc]).await;
        assert_eq!(out.code, 0, "{}", out.stderr);
        assert!(out.stdout.contains("nexus desktop open home"), "{}", out.stdout);
        let v = w.cli_ok("home", &["desktop", "share", "--name", "rdp", "--proto", "rdp", "--port", &silent]).await;
        assert_eq!(v["proto"], "rdp", "{v}");
        let ls = w.cli_ok("laptop", &["ls", "home"]).await;
        let protos: Vec<(String, String)> = ls["services"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| (s["name"].as_str().unwrap().to_string(), s["proto"].as_str().unwrap().to_string()))
            .collect();
        assert!(protos.contains(&("desktop".into(), "vnc".into())), "{ls}");
        assert!(protos.contains(&("rdp".into(), "rdp".into())), "{ls}");

        // 端口上什么都没有
        let none = w.unused_port().to_string();
        let out = w.cli("home", &["desktop", "share", "--port", &none]).await;
        assert_eq!(out.code, 109, "{}", out.stderr);
        assert!(out.stderr.contains("屏幕共享"), "应当说明怎么打开：{}", out.stderr);
        // 不说话、也没给协议
        let out = w.cli("home", &["desktop", "share", "--name", "mute", "--port", &silent]).await;
        assert_eq!(out.code, 107, "{}", out.stderr);
        assert!(out.stderr.contains("--proto"), "{}", out.stderr);
    },
}

scenario! {
    name: desktop_open_connects_a_viewer,
    covers: "R77",
    nodes: ["laptop", "home"],
    sim: (pending "要真的在本机开端口、执行查看器程序"),
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let vnc = fake_vnc().to_string();
        w.cli_ok("home", &["publish", "desktop", "--port", &vnc, "--proto", "vnc"]).await;
        let web = w.serve("home", Service::Http("hello".into())).await.to_string();
        w.cli_ok("home", &["publish", "web", "--port", &web, "--proto", "http"]).await;

        // FP-11.1-02
        let (url, bg) = open_with_viewer(&w, "a", &["desktop", "open", "home"]).await;
        let local = port_of(&url);
        for _ in 0..3 {
            assert_eq!(talk(local), (GREETING.to_vec(), b"ping".to_vec()), "{url}");
        }
        assert!(bg.stdout().contains(&url), "标准输出里应当有地址：{}", bg.stdout());
        bg.interrupt();

        // nexus open 遇到 vnc 服务也启动查看器
        let (url, bg) = open_with_viewer(&w, "b", &["open", "home/desktop"]).await;
        assert_eq!(talk(port_of(&url)).0, GREETING.to_vec());
        bg.interrupt();

        // 不启动查看器，指定端口
        let fixed = w.unused_port();
        let (viewer, log) = fake_viewer(&w.home_dir("laptop").join("viewer-none"));
        let p = fixed.to_string();
        let bg = w.spawn_cli_env("laptop", &["desktop", "open", "home/desktop", "--no-viewer", "--port", &p], &[("NEXUS_DESKTOP_VIEWER", viewer.to_str().unwrap())]);
        let want = format!("vnc://127.0.0.1:{fixed}");
        eventually(&w, "标准输出里的地址", || bg.stdout().contains(&want).then_some(())).await;
        assert_eq!(talk(fixed).0, GREETING.to_vec());
        assert!(!log.exists(), "--no-viewer 不应当启动查看器");
        bg.interrupt();

        // 查看器启动失败：照样服务
        let bg = w.spawn_cli_env("laptop", &["desktop", "open", "home"], &[("NEXUS_DESKTOP_VIEWER", "/nonexistent/viewer")]);
        let url = eventually(&w, "标准输出里的地址", || {
            bg.stdout().split_whitespace().find(|t| t.starts_with("vnc://")).map(|t| t.split('（').next().unwrap().to_string())
        })
        .await;
        assert_eq!(talk(port_of(&url)).0, GREETING.to_vec());
        eventually(&w, "说明打不开查看器", || bg.stderr().contains("查看器").then_some(())).await;
        bg.interrupt();
    },
}

scenario! {
    name: desktop_open_refuses_what_it_cannot_show,
    covers: "R77",
    nodes: ["laptop", "home"],
    sim: (pending "要真的在本机开端口"),
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let echo = w.serve("home", Service::Echo).await.to_string();
        w.cli_ok("home", &["publish", "echo", "--port", &echo]).await;

        // FP-11.1-03：没有远程桌面
        let out = w.cli("laptop", &["desktop", "open", "home", "--no-viewer"]).await;
        assert_eq!(out.code, 109, "{}", out.stderr);
        assert!(out.stderr.contains("nexus desktop share"), "{}", out.stderr);
        // 指定的服务不是远程桌面；带路径
        let out = w.cli("laptop", &["desktop", "open", "home/echo", "--no-viewer"]).await;
        assert_eq!(out.code, 107, "{}", out.stderr);
        assert!(out.stdout.is_empty(), "不应当开端口：{}", out.stdout);
        let vnc = fake_vnc().to_string();
        w.cli_ok("home", &["publish", "desktop", "--port", &vnc, "--proto", "vnc"]).await;
        let out = w.cli("laptop", &["desktop", "open", "home/desktop/x", "--no-viewer"]).await;
        assert_eq!(out.code, 107, "{}", out.stderr);
        // 两个远程桌面又没给名字
        w.cli_ok("home", &["publish", "screen", "--port", &vnc, "--proto", "rdp"]).await;
        let out = w.cli("laptop", &["desktop", "open", "home", "--no-viewer"]).await;
        assert_eq!(out.code, 107, "{}", out.stderr);
        assert!(out.stderr.contains("desktop") && out.stderr.contains("screen"), "{}", out.stderr);

        // FP-11.1-04
        let out = w.cli("laptop", &["desktop", "ls"]).await;
        assert_eq!(out.code, 0, "{}", out.stderr);
        assert!(out.stdout.contains("home/desktop") && out.stdout.contains("home/screen"), "{}", out.stdout);
        assert!(!out.stdout.contains("home/echo"), "{}", out.stdout);
        let v = w.cli_ok("laptop", &["desktop", "ls"]).await;
        let list = v["desktops"].as_array().unwrap();
        assert_eq!(list.len(), 2, "{v}");
        let d = list.iter().find(|d| d["name"] == "desktop").unwrap();
        assert_eq!((d["path"].as_str(), d["device"].as_str(), d["proto"].as_str()), (Some("home/desktop"), Some("home"), Some("vnc")), "{v}");
    },
}
