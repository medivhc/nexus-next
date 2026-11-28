//! Drive：跨设备同步与授权（M4.2）。功能点见 docs/feature-points/M4.2.md。

use nexus_testkit::{local_tree, pseudo_random, scenario, World};

async fn setup(w: &World) {
    w.network("laptop", &["anchor", "desktop"]).await;
    let v = w.cli_ok("anchor", &["drive", "serve", "on"]).await;
    assert_eq!(v["serving"], true, "{v}");
}

async fn head(w: &World, node: &str, ws: &str) -> Option<String> {
    let v = w.cli_ok(node, &["drive", "ls"]).await;
    v["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["name"] == ws)
        .and_then(|x| x["head"].as_str().map(str::to_string))
}

scenario! {
    name: commits_are_replicated_to_the_storage_device,
    covers: "R50, R52",
    nodes: ["laptop", "anchor", "desktop"],
    real: yes,
    run: |w: World| async move {
        setup(&w).await;
        w.cli_ok("laptop", &["drive", "create", "work"]).await;
        assert!(head(&w, "anchor", "work").await.is_none());
        assert!(w.cli_ok("anchor", &["drive", "ls"]).await["workspaces"].as_array().unwrap().iter().any(|x| x["name"] == "work"), "anchor 知道 work");

        let dir = local_tree(&[("a.txt", b"alpha"), ("src/b.rs", b"fn b() {}")]);
        w.cli_ok("laptop", &["drive", "import", dir.path().to_str().unwrap(), "work:/"]).await;
        let wh = w.cli_ok("laptop", &["drive", "where", "work:/"]).await;
        assert_eq!((wh["synced"].as_bool(), wh["uncommitted"].as_u64()), (Some(false), Some(2)), "{wh}");

        let c = w.cli_ok("laptop", &["drive", "commit", "work", "-m", "first"]).await;
        assert_eq!(c["sync"]["state"], "pushed", "{c}");
        let commit = c["commit"].as_str().unwrap().to_string();
        assert_eq!(head(&w, "anchor", "work").await.as_deref(), Some(commit.as_str()));
        assert_eq!(w.cli("anchor", &["drive", "cat", "work:/src/b.rs"]).await.stdout, "fn b() {}");

        let wh = w.cli_ok("laptop", &["drive", "where", "work:/"]).await;
        assert_eq!(wh["replicas"], serde_json::json!(["anchor"]));
        assert_eq!((wh["synced"].as_bool(), wh["uncommitted"].as_u64()), (Some(true), Some(0)), "{wh}");
        assert_eq!((wh["total_bytes"].as_u64(), wh["local_bytes"].as_u64()), (Some(14), Some(14)));

        // 没有存储设备时：只在本机。
        w.cli_ok("anchor", &["drive", "serve", "off"]).await;
        w.cli_ok("laptop", &["drive", "create", "solo"]).await;
        w.cli_with_stdin("laptop", &["drive", "put", "solo:/x", "--json"], b"x").await;
        let c = w.cli_ok("laptop", &["drive", "commit", "solo", "-m", "x"]).await;
        assert_eq!(c["sync"]["state"], "local", "{c}");
        let wh = w.cli_ok("laptop", &["drive", "where", "solo:/x"]).await;
        assert_eq!((wh["replicas"].as_array().map(Vec::len), wh["synced"].as_bool()), (Some(0), Some(false)));
        let text = w.cli("laptop", &["drive", "where", "solo:/x"]).await;
        assert!(text.stdout.contains("只在本机"), "{}", text.stdout);
        // 指定的副本必须开着 serve。
        assert_eq!(w.cli("laptop", &["drive", "create", "other", "--replica", "anchor"]).await.code, 106);
        assert_eq!(w.cli("laptop", &["drive", "create", "other", "--replica", "nobody"]).await.code, 103);
    },
}

scenario! {
    name: other_devices_read_on_demand,
    covers: "R48, R50, R52",
    nodes: ["laptop", "anchor", "desktop"],
    real: yes,
    run: |w: World| async move {
        setup(&w).await;
        let big = pseudo_random(4 * 1024 * 1024, 11);
        let dir = local_tree(&[("big.bin", &big), ("readme.md", b"# hello\n")]);
        w.cli_ok("laptop", &["drive", "create", "data"]).await;
        w.cli_ok("laptop", &["drive", "import", dir.path().to_str().unwrap(), "data:/"]).await;
        let commit = w.cli_ok("laptop", &["drive", "commit", "data", "-m", "data"]).await["commit"].as_str().unwrap().to_string();

        // desktop 还没有 data：sync 时从存储设备找到它，取回历史和目录结构，不取内容。
        assert_eq!(w.cli("desktop", &["drive", "ls", "data:/"]).await.code, 109);
        let s = w.cli_ok("desktop", &["drive", "sync", "data"]).await;
        assert_eq!(s["state"], "pulled", "{s}");
        assert_eq!(head(&w, "desktop", "data").await.as_deref(), Some(commit.as_str()));
        assert_eq!(w.cli_ok("desktop", &["drive", "ls", "data:/"]).await["entries"].as_array().unwrap().len(), 2);
        let wh = w.cli_ok("desktop", &["drive", "where", "data:/"]).await;
        assert_eq!((wh["total_bytes"].as_u64(), wh["local_bytes"].as_u64()), (Some(big.len() as u64 + 8), Some(0)), "{wh}");
        assert_eq!(wh["synced"], true);

        // laptop 关机：desktop 从 anchor 取。只取覆盖这一段的块。
        w.take_offline("laptop");
        let out = w.cli("desktop", &["drive", "cat", "data:/big.bin", "--offset", "2000000", "--length", "1000"]).await;
        assert_eq!(out.code, 0, "{}", out.stderr);
        assert!(out.stdout_raw == big[2_000_000..2_001_000]);
        let local = w.cli_ok("desktop", &["drive", "where", "data:/big.bin"]).await["local_bytes"].as_u64().unwrap();
        assert!(local > 0 && local <= 2 * 256 * 1024, "只应取到一两块：{local}");
        assert_eq!(w.cli("desktop", &["drive", "cat", "data:/readme.md"]).await.stdout, "# hello\n");
        // 存储设备也停了：本机已有的照样能读，没有的报告取不到。
        w.take_offline("anchor");
        assert_eq!(w.cli("desktop", &["drive", "cat", "data:/readme.md"]).await.stdout, "# hello\n");
        assert_eq!(w.cli("desktop", &["drive", "cat", "data:/big.bin", "--offset", "0", "--length", "10"]).await.code, 108);
    },
}

scenario! {
    name: diverged_heads_are_reported_and_merged,
    covers: "R50",
    nodes: ["laptop", "anchor", "desktop"],
    real: yes,
    run: |w: World| async move {
        setup(&w).await;
        w.cli_ok("laptop", &["drive", "create", "work"]).await;
        let dir = local_tree(&[("shared.txt", b"base"), ("lap.txt", b"l0"), ("desk.txt", b"d0")]);
        w.cli_ok("laptop", &["drive", "import", dir.path().to_str().unwrap(), "work:/"]).await;
        w.cli_ok("laptop", &["drive", "commit", "work", "-m", "base"]).await;
        assert_eq!(w.cli_ok("desktop", &["drive", "sync", "work"]).await["state"], "pulled");

        // 两边各改各的，也都改了 shared.txt。
        w.cli_with_stdin("laptop", &["drive", "put", "work:/lap.txt", "--json"], b"l1").await;
        w.cli_with_stdin("laptop", &["drive", "put", "work:/shared.txt", "--json"], b"from laptop").await;
        let lap = w.cli_ok("laptop", &["drive", "commit", "work", "-m", "laptop edit"]).await;
        assert_eq!(lap["sync"]["state"], "pushed");
        let lap_commit = lap["commit"].as_str().unwrap().to_string();

        w.cli_with_stdin("desktop", &["drive", "put", "work:/desk.txt", "--json"], b"d1").await;
        w.cli_with_stdin("desktop", &["drive", "put", "work:/shared.txt", "--json"], b"from desktop").await;
        let desk = w.cli_ok("desktop", &["drive", "commit", "work", "-m", "desktop edit"]).await;
        assert_eq!(desk["sync"]["state"], "diverged", "{desk}");
        let desk_commit = desk["commit"].as_str().unwrap().to_string();
        assert_eq!(head(&w, "anchor", "work").await.as_deref(), Some(lap_commit.as_str()), "存储设备的头不被覆盖");
        let s = w.cli_ok("desktop", &["drive", "sync", "work"]).await;
        assert_eq!((s["state"].as_str(), s["head"].as_str(), s["remote"].as_str()), (Some("diverged"), Some(desk_commit.as_str()), Some(lap_commit.as_str())), "{s}");
        assert_eq!(head(&w, "desktop", "work").await.as_deref(), Some(desk_commit.as_str()), "本机的头不变");

        let m = w.cli_ok("desktop", &["drive", "merge", "work"]).await;
        assert_eq!(m["conflicts"], serde_json::json!(["shared.txt"]), "{m}");
        assert_eq!(m["sync"]["state"], "pushed", "{m}");
        let merged = m["commit"].as_str().unwrap().to_string();
        assert_eq!(head(&w, "anchor", "work").await.as_deref(), Some(merged.as_str()));
        let cat = |p: &'static str| {
            let w = &w;
            async move { w.cli("desktop", &["drive", "cat", p]).await.stdout }
        };
        assert_eq!(cat("work:/lap.txt").await, "l1");
        assert_eq!(cat("work:/desk.txt").await, "d1");
        assert_eq!(cat("work:/shared.txt").await, "from desktop");
        assert_eq!(cat("work:/shared.txt.conflict-laptop").await, "from laptop");
        let log = w.cli_ok("desktop", &["drive", "log", "work"]).await;
        assert!(log["commits"][0]["message"].as_str().unwrap().contains("合并"), "{log}");

        // laptop 同步后快进到合并提交。
        let s = w.cli_ok("laptop", &["drive", "sync", "work"]).await;
        assert_eq!((s["state"].as_str(), s["head"].as_str()), (Some("pulled"), Some(merged.as_str())), "{s}");
        assert_eq!(w.cli("laptop", &["drive", "cat", "work:/desk.txt"]).await.stdout, "d1");
        assert_eq!(w.cli("laptop", &["drive", "merge", "work"]).await.code, 106, "没有分叉");
    },
}

scenario! {
    name: agents_need_grants_for_workspaces,
    covers: "R51",
    nodes: ["laptop", "anchor", "desktop"],
    real: yes,
    run: |w: World| async move {
        setup(&w).await;
        w.cli_ok("laptop", &["drive", "create", "work"]).await;
        w.cli_with_stdin("laptop", &["drive", "put", "work:/secret.txt", "--json"], b"s3cr3t").await;
        w.cli_ok("laptop", &["drive", "commit", "work", "-m", "s"]).await;
        w.cli_ok("laptop", &["drive", "create", "other"]).await;

        let issue = |label: &'static str, grants: &'static [&'static str]| {
            let w = &w;
            async move {
                let mut args = vec!["agent", "issue", "--label", label, "--ttl", "4h"];
                for g in grants {
                    args.extend(["--grant", g]);
                }
                w.cli_ok("laptop", &args).await["credential"].as_str().unwrap().to_string()
            }
        };
        let none = issue("nobody", &["drive.read@*/drive/other"]).await;
        let reader = issue("reader", &["drive.read@*/drive/work"]).await;

        assert_eq!(w.cli("laptop", &["--agent", &none, "drive", "cat", "work:/secret.txt"]).await.code, 105);
        let out = w.cli("laptop", &["--agent", &reader, "drive", "cat", "work:/secret.txt"]).await;
        assert_eq!((out.code, out.stdout.as_str()), (0, "s3cr3t"), "{}", out.stderr);
        let ls = w.cli("laptop", &["--agent", &reader, "drive", "ls", "--json"]).await;
        let names: Vec<String> = serde_json::from_str::<serde_json::Value>(&ls.stdout).unwrap()["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["work".to_string()], "只看到能读的工作区");
        assert_eq!(w.cli_with_stdin("laptop", &["--agent", &reader, "drive", "put", "work:/x"], b"x").await.code, 105, "只读");
        assert_eq!(w.cli("laptop", &["--agent", &reader, "drive", "commit", "work", "-m", "x"]).await.code, 105);
        assert_eq!(w.cli("laptop", &["--agent", &reader, "drive", "create", "mine"]).await.code, 105);

        let audit = w.cli_ok("laptop", &["audit"]).await;
        let events = audit["events"].as_array().unwrap();
        assert!(events.iter().any(|e| e["capability"] == "drive.read" && e["outcome"] == "allow"), "{audit}");
        assert!(events.iter().any(|e| e["capability"] == "drive.write" && e["outcome"] != "allow"), "{audit}");
        // 存储设备上也有设备推送时的审计。
        let audit = w.cli_ok("anchor", &["audit"]).await;
        assert!(audit["events"].as_array().unwrap().iter().any(|e| e["capability"] == "drive.write" && e["resource"] == "anchor/drive/work"), "{audit}");
    },
}
