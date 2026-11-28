//! Drive：预读（M4.4）。功能点见 docs/feature-points/M4.4.md。

use nexus_testkit::{local_tree, pseudo_random, scenario, World};

/// laptop 创建 work（a.txt、src/b.rs、big.bin）并复制到 anchor；`others` 同步过 work。
async fn setup(w: &World, others: &[&str]) -> Vec<u8> {
    let mut members = vec!["anchor"];
    members.extend_from_slice(others);
    w.network("laptop", &members).await;
    w.cli_ok("anchor", &["drive", "serve", "on"]).await;
    w.cli_ok("laptop", &["drive", "create", "work"]).await;
    let big = pseudo_random(1024 * 1024, 3);
    let b = pseudo_random(300 * 1024, 4);
    let dir = local_tree(&[("a.txt", b"alpha"), ("src/b.rs", &b), ("big.bin", &big)]);
    w.cli_ok(
        "laptop",
        &["drive", "import", dir.path().to_str().unwrap(), "work:/"],
    )
    .await;
    let c = w
        .cli_ok("laptop", &["drive", "commit", "work", "-m", "base"])
        .await;
    assert_eq!(c["sync"]["state"], "pushed", "{c}");
    for o in others {
        assert_eq!(
            w.cli_ok(o, &["drive", "sync", "work"]).await["state"],
            "pulled"
        );
    }
    b
}

async fn local(w: &World, node: &str, path: &str) -> (u64, u64) {
    let v = w.cli_ok(node, &["drive", "where", path]).await;
    (
        v["local_bytes"].as_u64().unwrap(),
        v["total_bytes"].as_u64().unwrap(),
    )
}

scenario! {
    name: reads_are_recorded_into_a_prefetch_list,
    covers: "R54",
    nodes: ["laptop", "anchor", "desktop"],
    real: yes,
    run: |w: World| async move {
        let b = setup(&w, &["desktop"]).await;
        assert_eq!(w.cli("desktop", &["drive", "record", "work", "--stop"]).await.code, 106, "没在记录");
        assert_eq!(w.cli("desktop", &["drive", "record", "nope", "--name", "x"]).await.code, 109);

        let v = w.cli_ok("desktop", &["drive", "record", "work", "--name", "build"]).await;
        assert_eq!(v["recording"], "build", "{v}");
        assert_eq!(w.cli("desktop", &["drive", "cat", "work:/a.txt"]).await.stdout, "alpha");
        let part = w.cli("desktop", &["drive", "cat", "work:/src/b.rs", "--offset", "100", "--length", "10"]).await;
        assert!(part.stdout_raw == b[100..110]);
        w.cli("desktop", &["drive", "cat", "work:/a.txt"]).await;
        let v = w.cli_ok("desktop", &["drive", "record", "work", "--stop"]).await;
        assert_eq!(v["profile"], "build", "{v}");
        assert_eq!(v["files"], serde_json::json!(["a.txt", "src/b.rs"]), "按第一次读到的顺序，不重复");
        assert_eq!(v["saved_to"], serde_json::json!(["anchor"]), "{v}");
        let text = w.cli("desktop", &["drive", "record", "work", "--name", "x"]).await;
        assert!(text.stdout.contains("开始记录"), "{}", text.stdout);
    },
}

scenario! {
    name: a_new_device_prefetches_what_was_recorded,
    covers: "R54",
    nodes: ["laptop", "anchor", "desktop", "tablet"],
    real: yes,
    run: |w: World| async move {
        let b = setup(&w, &["desktop", "tablet"]).await;
        w.cli_ok("desktop", &["drive", "record", "work", "--name", "build"]).await;
        w.cli("desktop", &["drive", "cat", "work:/a.txt"]).await;
        w.cli("desktop", &["drive", "cat", "work:/src/b.rs"]).await;
        w.cli_ok("desktop", &["drive", "record", "work", "--stop"]).await;

        // 只有 anchor 在线。
        w.take_offline("laptop");
        w.take_offline("desktop");
        assert_eq!(local(&w, "tablet", "work:/").await.0, 0);
        assert_eq!(w.cli("tablet", &["drive", "prefetch", "work", "--profile", "nope"]).await.code, 109);
        let v = w.cli_ok("tablet", &["drive", "prefetch", "work", "--profile", "build"]).await;
        assert_eq!((v["files"].as_u64(), v["fetched_bytes"].as_u64()), (Some(2), Some(5 + b.len() as u64)), "{v}");
        assert_eq!(local(&w, "tablet", "work:/a.txt").await, (5, 5));
        assert_eq!(local(&w, "tablet", "work:/src/b.rs").await, (b.len() as u64, b.len() as u64));
        assert_eq!(local(&w, "tablet", "work:/big.bin").await.0, 0, "清单里没有的不取");
        let again = w.cli_ok("tablet", &["drive", "prefetch", "work", "--profile", "build"]).await;
        assert_eq!(again["fetched_bytes"], 0, "{again}");
        let text = w.cli("tablet", &["drive", "prefetch", "work", "--profile", "build"]).await;
        assert!(text.stdout.contains("2 个文件"), "{}", text.stdout);

        let all = w.cli_ok("tablet", &["drive", "prefetch", "work", "--all"]).await;
        assert_eq!(all["fetched_bytes"], 1024 * 1024, "{all}");
        let (l, t) = local(&w, "tablet", "work:/").await;
        assert_eq!(l, t);
        w.take_offline("anchor");
        assert_eq!(w.cli("tablet", &["drive", "cat", "work:/big.bin", "--offset", "5", "--length", "3"]).await.code, 0, "离线也能读");
    },
}

scenario! {
    name: prefetch_and_record_need_grants,
    covers: "R51, R54",
    nodes: ["laptop", "anchor", "desktop"],
    real: yes,
    run: |w: World| async move {
        setup(&w, &["desktop"]).await;
        // agent 由管理设备 laptop 签发，在 laptop 上用。
        let reader = w.cli_ok("laptop", &["agent", "issue", "--label", "r", "--grant", "drive.read@*/drive/work", "--ttl", "4h"]).await["credential"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(w.cli("laptop", &["--agent", &reader, "drive", "record", "work", "--name", "x"]).await.code, 105);
        let v = w.cli("laptop", &["--agent", &reader, "drive", "prefetch", "work", "--all", "--json"]).await;
        assert_eq!(v.code, 0, "{}", v.stderr);
    },
}
