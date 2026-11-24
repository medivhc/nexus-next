//! 临时分享（M9.1，R73）：文件或目录、已发布的网页服务、终端的只读观看；
//! 带到期时间、次数限制、只读隔离，访客用链接在网页上直接使用，随时撤销并记录审计。
//! 功能点见 docs/feature-points/M9.1.md。

use nexus_testkit::nexus_web_core::WebHttpRequest;
use nexus_testkit::{local_tree, scenario, Service, World};
use nexus_wire::{ErrorCode, ShareTarget};
use std::time::Duration;

scenario! {
    name: share_drive_allows_scoped_read_and_blocks_escape,
    covers: "R73, R17",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let dir = local_tree(&[
            ("public/hello.txt", b"hello guest"),
            ("public/sub/nested.txt", b"nested content"),
            ("secret.txt", b"top secret"),
        ]);
        w.cli_ok("laptop", &["drive", "create", "work"]).await;
        w.cli_ok("laptop", &["drive", "import", dir.path().to_str().unwrap(), "work:/"]).await;

        // 1. 分享子目录 work:/public
        let created = w.cli_ok("laptop", &["share", "create", "--drive", "work:/public", "--ttl", "1h"]).await;
        let url = created["url"].as_str().expect("应当输出分享链接").to_string();

        let guest = w.browser().await;
        let info = guest.share_open(&url).await.expect("访客应当能打开分享");
        assert_eq!(info.target, ShareTarget::Drive { ws: "work".into(), path: "/public".into() });

        let entries = guest.share_drive_ls("").await.expect("能列分享根目录");
        let names: Vec<(&str, &str)> = entries.iter().map(|e| (e.name.as_str(), e.kind.as_str())).collect();
        assert_eq!(names, [("sub", "dir"), ("hello.txt", "file")]);

        let (size, bytes, truncated) = guest.share_drive_read("hello.txt", 1024).await.expect("能读分享目录下的文件");
        assert_eq!((size, bytes.as_slice(), truncated), (11, &b"hello guest"[..], false));

        let (_, nested, _) = guest.share_drive_read("sub/nested.txt", 1024).await.expect("能读嵌套子目录文件");
        assert_eq!(nested, b"nested content");

        // 试图通过 .. 越界读取上级 secret.txt 必须被拒绝
        let err = guest.share_drive_read("../secret.txt", 1024).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput, "{err:?}");

        // 2. 单文件分享 work:/public/hello.txt：不能读取同目录下的 sub/nested.txt 或上级 secret.txt
        let single = w.cli_ok("laptop", &["share", "create", "--drive", "work:/public/hello.txt", "--ttl", "1h"]).await;
        let single_url = single["url"].as_str().unwrap().to_string();
        let guest2 = w.browser().await;
        guest2.share_open(&single_url).await.unwrap();
        let (_, data, _) = guest2.share_drive_read("", 1024).await.unwrap();
        assert_eq!(data, b"hello guest");
        assert_eq!(guest2.share_drive_read("sub/nested.txt", 1024).await.unwrap_err().code, ErrorCode::Denied);
        assert_eq!(guest2.share_drive_ls("sub").await.unwrap_err().code, ErrorCode::Denied);
    },
}

scenario! {
    name: share_service_and_readonly_terminal_work,
    covers: "R73, R32",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;

        // 1. 临时分享已发布的网页服务
        let port = w.serve("home", Service::Http("shared web page".into())).await.to_string();
        w.cli_ok("home", &["publish", "web", "--port", &port, "--proto", "http"]).await;
        let svc_share = w.cli_ok("home", &["share", "create", "--service", "web", "--ttl", "1h"]).await;
        let svc_url = svc_share["url"].as_str().unwrap().to_string();

        let guest_svc = w.browser().await;
        let info = guest_svc.share_open(&svc_url).await.unwrap();
        assert_eq!(info.target, ShareTarget::Service { name: "web".into() });
        let resp = guest_svc
            .share_http_fetch(&WebHttpRequest {
                method: "GET".into(),
                path: "/index.html".into(),
                headers: vec![],
                body: vec![],
            })
            .await
            .expect("访客应当能访问分享的 HTTP 服务");
        assert_eq!(resp.status, 200);
        assert_eq!(String::from_utf8_lossy(&resp.body), "shared web page");

        // 2. 临时分享终端的只读观看：看得到输出，但访客按键不会输入到终端
        let owner = w.term("laptop", &["ssh", "home"]).await;
        owner.expect_prompt().await;
        owner.send("printf '%s%s\\n' SHARED_ TERM_READY\r").await;
        owner.expect_line("SHARED_TERM_READY").await;

        let term_share = w.cli_ok("home", &["share", "create", "--terminal", "main", "--ttl", "1h"]).await;
        let term_url = term_share["url"].as_str().unwrap().to_string();

        let guest_term = w.browser().await;
        guest_term.share_open(&term_url).await.unwrap();
        let watcher = guest_term.share_term().await.expect("访客应当能打开只读终端");
        watcher.expect_line("SHARED_TERM_READY").await;

        // 访客尝试发送按键输入：应当被静默忽略，不会出现在 owner 或 watcher 的屏幕上
        watcher.send("printf '%s%s\\n' GUEST_ IGNORED\r").await;
        w.run_for(Duration::from_millis(150)).await;
        owner.send("printf '%s%s\\n' OWNER_ AFTER_GUEST\r").await;
        watcher.expect_line("OWNER_AFTER_GUEST").await;
        assert!(!owner.screen().contains("GUEST_IGNORED"), "只读终端不能接受访客输入：{}", owner.screen());
        let _ = owner.detach().await;
    },
}

scenario! {
    name: share_enforces_expiry_max_uses_revocation_and_audits,
    covers: "R73, R17",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let port = w.serve("home", Service::Http("limited share".into())).await.to_string();
        w.cli_ok("home", &["publish", "web", "--port", &port, "--proto", "http"]).await;

        // 1. 次数上限（--max-uses 2）：前两次成功，第三次被拒绝
        let s1 = w.cli_ok("home", &["share", "create", "--service", "web", "--ttl", "1h", "--max-uses", "2"]).await;
        let url1 = s1["url"].as_str().unwrap().to_string();

        let g1 = w.browser().await;
        let info1 = g1.share_open(&url1).await.expect("第 1 次应当成功");
        assert_eq!(info1.remaining_uses, Some(1));

        let g2 = w.browser().await;
        let info2 = g2.share_open(&url1).await.expect("第 2 次应当成功");
        assert_eq!(info2.remaining_uses, Some(0));

        let g3 = w.browser().await;
        let err3 = g3.share_open(&url1).await.unwrap_err();
        assert_eq!(err3.code, ErrorCode::Denied, "第 3 次应当因次数耗尽被拒绝：{err3:?}");

        // 2. 到期时间（--ttl 1s）：到期后被拒绝
        let s2 = w.cli_ok("home", &["share", "create", "--service", "web", "--ttl", "1s"]).await;
        let url2 = s2["url"].as_str().unwrap().to_string();
        w.run_for(Duration::from_millis(1200)).await;
        let g_exp = w.browser().await;
        assert_eq!(g_exp.share_open(&url2).await.unwrap_err().code, ErrorCode::Denied);

        // 3. 随时撤销（nexus share revoke <id>）：撤销后连已打开的会话也不能再发请求
        let s3 = w.cli_ok("home", &["share", "create", "--service", "web", "--ttl", "1h"]).await;
        let id3 = s3["id"].as_str().unwrap().to_string();
        let url3 = s3["url"].as_str().unwrap().to_string();
        let g_rev = w.browser().await;
        g_rev.share_open(&url3).await.expect("撤销前可打开");

        w.cli_ok("home", &["share", "revoke", &id3]).await;
        let list = w.cli_ok("home", &["share", "ls"]).await;
        assert!(
            list["shares"].as_array().unwrap().iter().any(|x| x["id"] == id3 && x["revoked"] == true),
            "{list}"
        );
        assert_eq!(
            g_rev
                .share_http_fetch(&WebHttpRequest { method: "GET".into(), path: "/".into(), headers: vec![], body: vec![] })
                .await
                .unwrap_err()
                .code,
            ErrorCode::Denied
        );

        // 4. 审计记录：允许与拒绝均有记录，主体标明 share:<id>
        let audit = w.cli_ok("home", &["audit"]).await;
        let events = audit["events"].as_array().unwrap();
        let expected_tag = format!("share:{id3}");
        assert!(
            events.iter().any(|e| e["principal"]["passkey"] == expected_tag && e["capability"] == "share.open" && e["outcome"] == "allow"),
            "{audit}"
        );
        assert!(
            events.iter().any(|e| e["principal"]["passkey"] == expected_tag && e["capability"] == "share.service" && e["outcome"] == "deny"),
            "{audit}"
        );
    },
}
