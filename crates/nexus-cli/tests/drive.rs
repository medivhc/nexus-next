//! Drive：本地内容运行时（M4.1）。功能点见 docs/feature-points/M4.1.md。

use nexus_testkit::{local_tree, pseudo_random, scenario, World};
use serde_json::json;

fn names(v: &serde_json::Value) -> Vec<(String, String)> {
    v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["name"].as_str().unwrap().to_string(),
                e["kind"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

scenario! {
    name: workspaces_can_be_created_imported_and_listed,
    covers: "R47",
    nodes: ["laptop"],
    real: yes,
    run: |w: World| async move {
        let v = w.cli_ok("laptop", &["drive", "create", "work"]).await;
        assert_eq!(v["workspace"], "work");
        assert_eq!(w.cli("laptop", &["drive", "create", "work"]).await.code, 107, "同名");
        assert_eq!(w.cli("laptop", &["drive", "create", "Bad_Name"]).await.code, 107);

        let dir = local_tree(&[
            ("Cargo.toml", b"[package]\nname = \"demo\"\n"),
            ("src/main.rs", b"fn main() { println!(\"hi\"); }\n"),
            ("src/util/mod.rs", b"pub fn x() {}\n"),
            ("run.sh", b"#!/bin/sh\necho run\n"),
        ]);
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path().join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("src/main.rs", dir.path().join("main-link")).unwrap();
        let d = dir.path().to_str().unwrap();

        let v = w.cli_ok("laptop", &["drive", "import", d, "work:/"]).await;
        assert_eq!(v["files"], 5, "{v}");
        assert_eq!(w.cli("laptop", &["drive", "import", d, "nope:/"]).await.code, 109, "工作区不存在");
        assert_eq!(w.cli("laptop", &["drive", "import", "/no/such/dir", "work:/"]).await.code, 107);

        let root = w.cli_ok("laptop", &["drive", "ls", "work:/"]).await;
        assert_eq!(
            names(&root),
            vec![
                ("Cargo.toml".into(), "file".into()),
                ("main-link".into(), "symlink".into()),
                ("run.sh".into(), "file".into()),
                ("src".into(), "dir".into()),
            ]
        );
        let run = root["entries"].as_array().unwrap().iter().find(|e| e["name"] == "run.sh").unwrap().clone();
        assert_eq!((run["size"].as_u64(), run["exec"].as_bool()), (Some(19), Some(true)), "{run}");
        let link = root["entries"].as_array().unwrap().iter().find(|e| e["name"] == "main-link").unwrap().clone();
        assert_eq!(link["target"], "src/main.rs");
        assert_eq!(names(&w.cli_ok("laptop", &["drive", "ls", "work:/src"]).await), vec![("main.rs".into(), "file".into()), ("util".into(), "dir".into())]);
        assert_eq!(names(&w.cli_ok("laptop", &["drive", "ls", "work:/src/main.rs"]).await), vec![("main.rs".into(), "file".into())], "对文件 ls 列出它自己");
        assert_eq!(w.cli("laptop", &["drive", "ls", "work:/nope"]).await.code, 109);

        let out = w.cli("laptop", &["drive", "cat", "work:/src/main.rs"]).await;
        assert_eq!((out.code, out.stdout.as_str()), (0, "fn main() { println!(\"hi\"); }\n"));
        assert_eq!(w.cli("laptop", &["drive", "cat", "work:/src"]).await.code, 107, "是目录");
        assert_eq!(w.cli("laptop", &["drive", "cat", "work:/nope"]).await.code, 109);

        // 导入到子目录。
        let more = local_tree(&[("a.txt", b"a")]);
        w.cli_ok("laptop", &["drive", "import", more.path().to_str().unwrap(), "work:/vendor/x"]).await;
        assert_eq!(w.cli("laptop", &["drive", "cat", "work:/vendor/x/a.txt"]).await.stdout, "a");

        let list = w.cli_ok("laptop", &["drive", "ls"]).await;
        assert_eq!(list["workspaces"][0]["name"], "work", "{list}");
        assert_eq!(list["workspaces"][0]["head"], serde_json::Value::Null);
        assert_eq!(list["workspaces"][0]["changes"], 6);
    },
}

scenario! {
    name: reading_a_range_returns_just_that_part,
    covers: "R48",
    nodes: ["laptop"],
    real: yes,
    run: |w: World| async move {
        let big = pseudo_random(3 * 1024 * 1024 + 123, 5);
        let dir = local_tree(&[("big.bin", &big)]);
        w.cli_ok("laptop", &["drive", "create", "data"]).await;
        w.cli_ok("laptop", &["drive", "import", dir.path().to_str().unwrap(), "data:/"]).await;

        let whole = w.cli("laptop", &["drive", "cat", "data:/big.bin"]).await;
        assert!(whole.stdout_raw == big, "整个文件读回来不一样（{} 字节）", whole.stdout_raw.len());
        for (off, len) in [(0usize, 10usize), (1_000_000, 100), (65_536, 300_000), (big.len() - 50, 1000), (big.len() + 10, 5)] {
            let out = w.cli("laptop", &["drive", "cat", "data:/big.bin", "--offset", &off.to_string(), "--length", &len.to_string()]).await;
            let start = off.min(big.len());
            let end = (off + len).min(big.len());
            assert_eq!(out.code, 0);
            assert!(out.stdout_raw == big[start..end], "范围 {off}+{len} 不对");
        }
    },
}

scenario! {
    name: changes_are_committed_and_history_is_kept,
    covers: "R49",
    nodes: ["laptop"],
    real: yes,
    run: |w: World| async move {
        w.cli_ok("laptop", &["drive", "create", "work"]).await;
        assert_eq!(w.cli("laptop", &["drive", "commit", "work", "-m", "empty"]).await.code, 107, "没有可提交的");
        let put = w.cli_with_stdin("laptop", &["drive", "put", "work:/notes/a.txt", "--json"], b"version one").await;
        assert_eq!(put.code, 0, "{}", put.stderr);
        w.cli_with_stdin("laptop", &["drive", "put", "work:/b.txt", "--json"], b"bee").await;
        let st = w.cli_ok("laptop", &["drive", "status", "work"]).await;
        assert_eq!(st["changes"], json!([{ "kind": "added", "path": "b.txt" }, { "kind": "added", "path": "notes/a.txt" }]));

        let c1 = w.cli_ok("laptop", &["drive", "commit", "work", "-m", "first"]).await["commit"].as_str().unwrap().to_string();
        assert_eq!(c1.len(), 64);
        assert_eq!(w.cli_ok("laptop", &["drive", "status", "work"]).await["changes"], json!([]));

        w.cli_with_stdin("laptop", &["drive", "put", "work:/notes/a.txt", "--json"], b"version two").await;
        w.cli_ok("laptop", &["drive", "rm", "work:/b.txt"]).await;
        assert_eq!(w.cli("laptop", &["drive", "rm", "work:/b.txt"]).await.code, 109);
        assert_eq!(w.cli("laptop", &["drive", "rm", "work:/notes"]).await.code, 107, "目录要 -r");
        let st = w.cli_ok("laptop", &["drive", "status", "work"]).await;
        assert_eq!(st["changes"], json!([{ "kind": "deleted", "path": "b.txt" }, { "kind": "modified", "path": "notes/a.txt" }]));
        let c2 = w.cli_ok("laptop", &["drive", "commit", "work", "-m", "second"]).await["commit"].as_str().unwrap().to_string();

        let log = w.cli_ok("laptop", &["drive", "log", "work"]).await;
        let commits = log["commits"].as_array().unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!((commits[0]["commit"].as_str(), commits[0]["message"].as_str()), (Some(c2.as_str()), Some("second")));
        assert_eq!((commits[1]["commit"].as_str(), commits[1]["message"].as_str()), (Some(c1.as_str()), Some("first")));
        assert_eq!(commits[0]["device"], "laptop");

        // 读旧版本；提交号可以写前缀。
        assert_eq!(w.cli("laptop", &["drive", "cat", "work:/notes/a.txt", "--at", &c1[..8]]).await.stdout, "version one");
        assert_eq!(w.cli("laptop", &["drive", "cat", "work:/notes/a.txt"]).await.stdout, "version two");
        assert_eq!(w.cli("laptop", &["drive", "cat", "work:/b.txt", "--at", &c1]).await.stdout, "bee");
        assert_eq!(w.cli("laptop", &["drive", "cat", "work:/b.txt", "--at", "deadbeef"]).await.code, 109);

        let ws = w.cli_ok("laptop", &["drive", "ls"]).await;
        assert_eq!((ws["workspaces"][0]["head"].as_str(), ws["workspaces"][0]["changes"].as_u64()), (Some(c2.as_str()), Some(0)));
    },
}
