//! Drive：Linux 挂载（M4.3）。功能点见 docs/feature-points/M4.3.md。
//!
//! 挂载要真实的内核（FUSE），所以这些场景只在真实后端上运行。

use nexus_testkit::{
    local_dir, local_tree, pseudo_random, scenario, Background, LocalTree, Output, World,
};
use std::time::{Duration, Instant};

/// 在节点的机器上运行一段 shell。
async fn sh(w: &World, node: &str, script: &str) -> Output {
    w.host(node, &["sh", "-c", script], b"").await
}

/// 在后台挂载，等到挂载点真正挂上。
async fn mount(w: &World, node: &str, args: &[&str], dir: &LocalTree) -> Background {
    let d = dir.path().to_str().unwrap();
    let mut all: Vec<&str> = args.to_vec();
    all.insert(all.iter().position(|a| *a == "drive").unwrap() + 1, "mount");
    all.push(d);
    let mut bg = w.spawn_cli(node, &all);
    let start = Instant::now();
    loop {
        if sh(w, node, &format!("mountpoint -q '{d}'")).await.code == 0 {
            return bg;
        }
        if let Some(code) = bg.exited() {
            panic!("挂载失败（{code}）：{}", bg.stderr());
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "10 秒内没有挂上：{}",
            bg.stderr()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn eventually(what: &str, mut f: impl AsyncFnMut() -> bool) {
    let start = Instant::now();
    while !f().await {
        assert!(start.elapsed() < Duration::from_secs(3), "3 秒内没有{what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn mtime(w: &World, m: &str) -> u64 {
    sh(w, "desktop", &format!("stat -c %Y '{m}/notes.txt'"))
        .await
        .stdout
        .trim()
        .parse()
        .unwrap()
}

fn toolchain_env() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let rustup = std::env::var("RUSTUP_HOME").unwrap_or(format!("{home}/.rustup"));
    let cargo = std::env::var("CARGO_HOME").unwrap_or(format!("{home}/.cargo"));
    format!("RUSTUP_HOME='{rustup}' CARGO_HOME='{cargo}' PATH=\"$PATH:{cargo}/bin\"")
}

scenario! {
    name: a_mounted_workspace_reads_like_a_directory,
    covers: "R53",
    nodes: ["laptop"],
    sim: (pending "挂载需要真实的内核"),
    real: yes,
    run: |w: World| async move {
        let big = pseudo_random(3 * 1024 * 1024, 7);
        let main_rs = b"fn main() { println!(\"hi from mount\"); }\n";
        let src = local_tree(&[
            ("Cargo.toml", b"[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"),
            ("src/main.rs", main_rs),
            ("run.sh", b"#!/bin/sh\necho run\n"),
            ("big.bin", &big),
        ]);
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(src.path().join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("src/main.rs", src.path().join("main-link")).unwrap();
        w.cli_ok("laptop", &["drive", "create", "work"]).await;
        w.cli_ok("laptop", &["drive", "import", src.path().to_str().unwrap(), "work:/"]).await;

        let dir = local_dir("d1");
        let m = dir.path().to_str().unwrap().to_string();
        let mut bg = mount(&w, "laptop", &["drive", "work:/"], &dir).await;

        assert_eq!(sh(&w, "laptop", &format!("ls -A '{m}' | LC_ALL=C sort")).await.stdout, "Cargo.toml\nbig.bin\nmain-link\nrun.sh\nsrc\n");
        assert_eq!(sh(&w, "laptop", &format!("cat '{m}/src/main.rs'")).await.stdout.as_bytes(), main_rs);
        assert_eq!(sh(&w, "laptop", &format!("'{m}/run.sh'")).await.stdout, "run\n", "可执行文件直接运行");
        assert_eq!(sh(&w, "laptop", &format!("readlink '{m}/main-link'")).await.stdout, "src/main.rs\n");
        assert_eq!(sh(&w, "laptop", &format!("cat '{m}/main-link'")).await.stdout.as_bytes(), main_rs);
        assert_eq!(sh(&w, "laptop", &format!("stat -c '%a %s' '{m}/run.sh' '{m}/Cargo.toml' '{m}/src'")).await.stdout.lines().map(|l| l.split(' ').next().unwrap().to_string()).collect::<Vec<_>>(), ["755", "644", "755"]);
        let part = sh(&w, "laptop", &format!("dd if='{m}/big.bin' bs=4096 skip=250 count=2 status=none")).await;
        assert!(part.stdout_raw == big[250 * 4096..252 * 4096], "范围读不对（{} 字节）", part.stdout_raw.len());
        let orig = src.path().join("big.bin");
        assert_eq!(sh(&w, "laptop", &format!("cmp '{m}/big.bin' '{}'", orig.display())).await.code, 0, "整个文件一致");

        // 编译器直接在挂载里工作。
        let target = local_dir("d2");
        let t = target.path().display();
        let build = sh(&w, "laptop", &format!("cd '{m}' && env {} CARGO_TARGET_DIR='{t}' cargo build --offline -q && '{t}/debug/demo'", toolchain_env())).await;
        assert_eq!((build.code, build.stdout.as_str()), (0, "hi from mount\n"), "{}", build.stderr);

        // fusermount -u 之后命令以 0 结束，目录恢复为空。
        let u = sh(&w, "laptop", &format!("fusermount3 -u '{m}' || fusermount -u '{m}'")).await;
        assert_eq!(u.code, 0, "{}", u.stderr);
        assert_eq!(bg.wait_within(Duration::from_secs(5)), Some(0), "{}", bg.stderr());
        assert_eq!(sh(&w, "laptop", &format!("ls -A '{m}'")).await.stdout, "");

        // 只挂一个子目录；SIGTERM 时卸载并以 0 结束。
        let sub = local_dir("d3");
        let s = sub.path().to_str().unwrap().to_string();
        let mut bg = mount(&w, "laptop", &["drive", "work:/src"], &sub).await;
        assert_eq!(sh(&w, "laptop", &format!("ls -A '{s}'")).await.stdout, "main.rs\n");
        assert_eq!(bg.terminate(), Some(0), "{}", bg.stderr());
        let left = sh(&w, "laptop", &format!("mountpoint '{s}'; grep ' {s} ' /proc/mounts")).await;
        assert_eq!(left.code, 1, "已卸载：{} {} / {}", left.stdout, left.stderr, bg.stderr());
    },
}

scenario! {
    name: writes_in_a_mount_stay_local_until_commit,
    covers: "R49, R53",
    nodes: ["laptop", "anchor"],
    sim: (pending "挂载需要真实的内核"),
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["anchor"]).await;
        w.cli_ok("anchor", &["drive", "serve", "on"]).await;
        w.cli_ok("laptop", &["drive", "create", "work"]).await;
        let src = local_tree(&[("a.txt", b"alpha"), ("keep.txt", b"keep"), ("old.txt", b"old"), ("tool.sh", b"echo t\n")]);
        w.cli_ok("laptop", &["drive", "import", src.path().to_str().unwrap(), "work:/"]).await;
        let first = w.cli_ok("laptop", &["drive", "commit", "work", "-m", "base"]).await["commit"].as_str().unwrap().to_string();

        let dir = local_dir("d4");
        let m = dir.path().to_str().unwrap().to_string();
        let _bg = mount(&w, "laptop", &["drive", "work:/"], &dir).await;
        let edit = sh(&w, "laptop", &format!(
            "cd '{m}' && printf new > new.txt && printf beta > a.txt && printf gamma > a.tmp && mv a.tmp a.txt \
             && mkdir -p d/e && printf x > d/e/f.txt && rm keep.txt && mv old.txt renamed.txt && chmod +x tool.sh"
        )).await;
        assert_eq!(edit.code, 0, "{}", edit.stderr);
        assert_eq!(sh(&w, "laptop", &format!("cat '{m}/a.txt'; stat -c %a '{m}/tool.sh'; ls -A '{m}' | LC_ALL=C sort")).await.stdout,
            "gamma755\na.txt\nd\nnew.txt\nrenamed.txt\ntool.sh\n");

        let st = w.cli_ok("laptop", &["drive", "status", "work"]).await;
        let mut changes: Vec<(String, String)> = st["changes"].as_array().unwrap().iter()
            .map(|c| (c["path"].as_str().unwrap().to_string(), c["kind"].as_str().unwrap().to_string())).collect();
        changes.sort();
        let expect = [("a.txt", "modified"), ("d/e/f.txt", "added"), ("keep.txt", "deleted"), ("new.txt", "added"), ("old.txt", "deleted"), ("renamed.txt", "added"), ("tool.sh", "modified")];
        assert_eq!(changes, expect.map(|(p, k)| (p.to_string(), k.to_string())), "{st}");
        assert_eq!(w.cli("laptop", &["drive", "cat", "work:/renamed.txt"]).await.stdout, "old");
        let ls = w.cli_ok("laptop", &["drive", "ls", "work:/tool.sh"]).await;
        assert_eq!(ls["entries"][0]["exec"], true);

        // 没提交：anchor 的头不变。
        let anchor_head = w.cli_ok("anchor", &["drive", "ls"]).await["workspaces"][0]["head"].as_str().map(str::to_string);
        assert_eq!(anchor_head.as_deref(), Some(first.as_str()));
        let c = w.cli_ok("laptop", &["drive", "commit", "work", "-m", "edits"]).await;
        assert_eq!(c["sync"]["state"], "pushed", "{c}");
        assert_eq!(w.cli("anchor", &["drive", "cat", "work:/d/e/f.txt"]).await.stdout, "x");
        assert_eq!(sh(&w, "laptop", &format!("cat '{m}/new.txt'")).await.stdout, "new", "提交后挂载照常");
    },
}

scenario! {
    name: synced_changes_show_up_in_a_mount,
    covers: "R50, R53",
    nodes: ["laptop", "anchor", "desktop"],
    sim: (pending "挂载需要真实的内核"),
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["anchor", "desktop"]).await;
        w.cli_ok("anchor", &["drive", "serve", "on"]).await;
        w.cli_ok("laptop", &["drive", "create", "work"]).await;
        w.cli_with_stdin("laptop", &["drive", "put", "work:/notes.txt", "--json"], b"v1").await;
        w.cli_ok("laptop", &["drive", "commit", "work", "-m", "v1"]).await;
        assert_eq!(w.cli_ok("desktop", &["drive", "sync", "work"]).await["state"], "pulled");

        let dir = local_dir("d5");
        let m = dir.path().to_str().unwrap().to_string();
        let _bg = mount(&w, "desktop", &["drive", "work:/"], &dir).await;
        assert_eq!(sh(&w, "desktop", &format!("cat '{m}/notes.txt'")).await.stdout, "v1");
        let before = mtime(&w, &m).await;
        tokio::time::sleep(Duration::from_millis(1200)).await;

        let longer = "version two, which is longer";
        w.cli_with_stdin("laptop", &["drive", "put", "work:/notes.txt", "--json"], longer.as_bytes()).await;
        w.cli_with_stdin("laptop", &["drive", "put", "work:/new.txt", "--json"], b"n").await;
        w.cli_ok("laptop", &["drive", "commit", "work", "-m", "v2"]).await;
        assert_eq!(w.cli_ok("desktop", &["drive", "sync", "work"]).await["state"], "pulled");

        eventually("看到新内容", async || sh(&w, "desktop", &format!("cat '{m}/notes.txt'")).await.stdout == longer).await;
        assert_eq!(sh(&w, "desktop", &format!("stat -c %s '{m}/notes.txt'; cat '{m}/new.txt'")).await.stdout, format!("{}\nn", longer.len()));
        assert!(mtime(&w, &m).await > before, "内容变了，修改时间也变");
    },
}

scenario! {
    name: mount_errors_and_agent_grants,
    covers: "R51, R53",
    nodes: ["laptop"],
    sim: (pending "挂载需要真实的内核"),
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &[]).await;
        w.cli_ok("laptop", &["drive", "create", "work"]).await;
        w.cli_with_stdin("laptop", &["drive", "put", "work:/file.txt", "--json"], b"f").await;
        w.cli_ok("laptop", &["drive", "commit", "work", "-m", "f"]).await;

        let dir = local_dir("d6");
        let m = dir.path().to_str().unwrap().to_string();
        assert_eq!(w.cli("laptop", &["drive", "mount", "nope:/", &m]).await.code, 109);
        assert_eq!(w.cli("laptop", &["drive", "mount", "work:/file.txt", &m]).await.code, 107, "不是目录");
        assert_eq!(w.cli("laptop", &["drive", "mount", "work:/", "/no/such/dir"]).await.code, 107);
        let full = local_tree(&[("x", b"x")]);
        assert_eq!(w.cli("laptop", &["drive", "mount", "work:/", full.path().to_str().unwrap()]).await.code, 107, "不是空目录");

        let issue = |label: &'static str, grant: &'static str| {
            let w = &w;
            async move { w.cli_ok("laptop", &["agent", "issue", "--label", label, "--grant", grant, "--ttl", "4h"]).await["credential"].as_str().unwrap().to_string() }
        };
        let other = issue("other", "drive.read@*/drive/other").await;
        assert_eq!(w.cli("laptop", &["--agent", &other, "drive", "mount", "work:/", &m]).await.code, 105);

        let reader = issue("reader", "drive.read@*/drive/work").await;
        let mut bg = mount(&w, "laptop", &["--agent", &reader, "drive", "work:/"], &dir).await;
        assert_eq!(sh(&w, "laptop", &format!("cat '{m}/file.txt'")).await.stdout, "f");
        assert_ne!(sh(&w, "laptop", &format!("printf x > '{m}/new.txt'")).await.code, 0, "只读的 agent 不能新建");
        assert_ne!(sh(&w, "laptop", &format!("printf y > '{m}/file.txt'")).await.code, 0, "也不能改：{}", bg.stderr());
        assert_eq!(w.cli_ok("laptop", &["drive", "status", "work"]).await["changes"], serde_json::json!([]));
        assert_eq!(bg.terminate(), Some(0), "{}", bg.stderr());

        // SIGTERM 时已关闭文件的写入都在工作树里。
        let mut bg = mount(&w, "laptop", &["drive", "work:/"], &dir).await;
        assert_eq!(sh(&w, "laptop", &format!("printf z > '{m}/z.txt'")).await.code, 0);
        assert_eq!(bg.terminate(), Some(0), "{}", bg.stderr());
        assert_eq!(w.cli("laptop", &["drive", "cat", "work:/z.txt"]).await.stdout, "z");
    },
}

scenario! {
    name: a_mount_can_record_a_prefetch_list,
    covers: "R53, R54",
    nodes: ["laptop", "anchor", "tablet"],
    sim: (pending "挂载需要真实的内核"),
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["anchor", "tablet"]).await;
        w.cli_ok("anchor", &["drive", "serve", "on"]).await;
        w.cli_ok("laptop", &["drive", "create", "work"]).await;
        let src = local_tree(&[("used.txt", b"used"), ("src/lib.rs", b"pub fn f() {}\n"), ("unused.bin", &pseudo_random(200_000, 9))]);
        w.cli_ok("laptop", &["drive", "import", src.path().to_str().unwrap(), "work:/"]).await;
        w.cli_ok("laptop", &["drive", "commit", "work", "-m", "base"]).await;

        let dir = local_dir("rec");
        let m = dir.path().to_str().unwrap().to_string();
        let mut bg = mount(&w, "laptop", &["drive", "work:/", "--record", "build"], &dir).await;
        assert_eq!(sh(&w, "laptop", &format!("cat '{m}/used.txt' '{m}/src/lib.rs' >/dev/null && ls -R '{m}' >/dev/null")).await.code, 0);
        assert_eq!(bg.terminate(), Some(0), "{}", bg.stderr());

        assert_eq!(w.cli_ok("tablet", &["drive", "sync", "work"]).await["state"], "pulled");
        let v = w.cli_ok("tablet", &["drive", "prefetch", "work", "--profile", "build"]).await;
        assert_eq!((v["files"].as_u64(), v["fetched_bytes"].as_u64()), (Some(2), Some(4 + 14)), "只取读过的文件：{v}");
    },
}
