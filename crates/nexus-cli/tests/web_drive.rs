//! 网页里浏览工作区（M8.1，R70）。功能点见 docs/feature-points/M8.1.md。
//! 测试工具里的浏览器跑的是页面用的同一份网页节点核心。

use nexus_core_proto::webauthn::soft::SoftAuthenticator;
use nexus_testkit::{local_tree, pseudo_random, scenario, World};
use nexus_wire::ErrorCode;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n not really a png";

fn passkey(seed: u8) -> SoftAuthenticator {
    SoftAuthenticator::new([seed; 32], "bewith.link", "https://bewith.link")
}

/// 在 laptop 上登记通行密钥，返回入口链接。
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

/// laptop、anchor（存储设备）；work 有几个文件并已提交。
async fn setup(w: &World) {
    w.network("laptop", &["anchor"]).await;
    w.cli_ok("anchor", &["drive", "serve", "on"]).await;
    let big = pseudo_random(3 * 1024 * 1024, 7);
    let dir = local_tree(&[("a.txt", b"alpha"), ("src/b.rs", b"fn b() {}\n"), ("logo.png", PNG), ("big.bin", &big)]);
    w.cli_ok("laptop", &["drive", "create", "work"]).await;
    w.cli_ok("laptop", &["drive", "import", dir.path().to_str().unwrap(), "work:/"]).await;
    let c = w.cli_ok("laptop", &["drive", "commit", "work", "-m", "first"]).await;
    assert_eq!(c["sync"]["state"], "pushed", "{c}");
}

scenario! {
    name: web_lists_workspaces_and_reads_files,
    covers: "R70",
    nodes: ["laptop", "anchor"],
    real: yes,
    run: |w: World| async move {
        setup(&w).await;
        let mut phone = passkey(42);
        let entry = enrolled(&w, "phone", &["node.info@**", "drive.read@**"], &phone).await;
        let browser = w.browser().await;
        browser.login(&w, &entry, &mut phone).await.expect("登录应当成功");

        // FP-8.1-01
        let ws = browser.drive_workspaces().await.expect("能列出工作区");
        assert_eq!(ws, [("work".to_string(), "anchor".to_string())]);
        let root = browser.drive_ls("work", "").await.expect("能列根目录");
        let names: Vec<(&str, &str, u64)> = root.iter().map(|e| (e.name.as_str(), e.kind.as_str(), e.size)).collect();
        assert_eq!(names, [("src", "dir", 0), ("a.txt", "file", 5), ("big.bin", "file", 3 * 1024 * 1024), ("logo.png", "file", PNG.len() as u64)]);
        let src = browser.drive_ls("work", "src").await.unwrap();
        assert_eq!(src.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["b.rs"]);
        assert_eq!(browser.drive_ls("work", "/src/").await.unwrap(), src, "路径前后的 / 不影响");

        // FP-8.1-02
        let f = browser.drive_read("work", "src/b.rs", 1 << 20).await.unwrap();
        assert_eq!((f.bytes.as_slice(), f.size, f.truncated), (&b"fn b() {}\n"[..], 10, false));
        assert_eq!(browser.drive_read("work", "logo.png", 1 << 20).await.unwrap().bytes, PNG);
        let big = pseudo_random(3 * 1024 * 1024, 7);
        let head = browser.drive_read("work", "big.bin", 1 << 20).await.unwrap();
        assert_eq!((head.size, head.truncated, head.bytes.len()), (big.len() as u64, true, 1 << 20));
        assert_eq!(head.bytes, big[..1 << 20]);
        let all = browser.drive_read("work", "big.bin", u64::MAX).await.unwrap();
        assert!(!all.truncated && all.bytes == big, "整个文件应当逐字节相同");
        let audit = w.cli_ok("anchor", &["audit"]).await;
        assert!(
            audit["events"].as_array().unwrap().iter().any(|e| e["principal"]["passkey"] == "phone" && e["capability"] == "drive.read"),
            "{audit}"
        );

        // FP-8.1-03：不存在
        assert_eq!(browser.drive_ls("nope", "").await.unwrap_err().code, ErrorCode::NotFound);
        assert_eq!(browser.drive_ls("work", "nothing").await.unwrap_err().code, ErrorCode::NotFound);
        assert_eq!(browser.drive_read("work", "src/none.rs", 10).await.unwrap_err().code, ErrorCode::NotFound);
        assert_eq!(browser.drive_ls("work", "a.txt").await.unwrap_err().code, ErrorCode::InvalidInput);
        assert_eq!(browser.drive_read("work", "src", 10).await.unwrap_err().code, ErrorCode::InvalidInput);

        // FP-8.1-04：只看到同步了的提交
        w.cli_with_stdin("laptop", &["drive", "put", "work:/a.txt", "--json"], b"beta!").await;
        assert_eq!(browser.drive_read("work", "a.txt", 100).await.unwrap().bytes, b"alpha", "没提交的改动看不到");
        w.cli_ok("laptop", &["drive", "commit", "work", "-m", "second"]).await;
        assert_eq!(browser.drive_read("work", "a.txt", 100).await.unwrap().bytes, b"beta!");
    },
}

scenario! {
    name: web_drive_needs_a_grant,
    covers: "R70",
    nodes: ["laptop", "anchor"],
    real: yes,
    run: |w: World| async move {
        setup(&w).await;
        w.cli_ok("laptop", &["drive", "create", "docs"]).await;
        w.cli_with_stdin("laptop", &["drive", "put", "docs:/readme.md", "--json"], b"# docs\n").await;
        w.cli_ok("laptop", &["drive", "commit", "docs", "-m", "docs"]).await;

        // FP-8.1-03：默认授权读不了工作区
        let mut plain = passkey(43);
        let entry = enrolled(&w, "plain", &[], &plain).await;
        let browser = w.browser().await;
        browser.login(&w, &entry, &mut plain).await.unwrap();
        assert!(browser.drive_workspaces().await.unwrap().is_empty());
        assert_eq!(browser.drive_read("work", "a.txt", 10).await.unwrap_err().code, ErrorCode::Denied);

        // 只授权 docs
        let mut reader = passkey(44);
        let entry = enrolled(&w, "reader", &["node.info@**", "drive.read@anchor/drive/docs"], &reader).await;
        let browser = w.browser().await;
        browser.login(&w, &entry, &mut reader).await.unwrap();
        assert_eq!(browser.drive_workspaces().await.unwrap(), [("docs".to_string(), "anchor".to_string())]);
        assert_eq!(browser.drive_read("docs", "readme.md", 100).await.unwrap().bytes, b"# docs\n");
        assert_eq!(browser.drive_ls("work", "").await.unwrap_err().code, ErrorCode::Denied);
    },
}
