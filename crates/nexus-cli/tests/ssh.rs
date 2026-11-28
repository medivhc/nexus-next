//! 对本机程序表现为 SSH（M1.6）。功能点见 docs/feature-points/M1.6.md。
//!
//! 这些场景用系统自带的 ssh、scp、git，只在真实后端运行；
//! 它们背后的执行和终端会话在仿真里已经由其他场景覆盖。

use nexus_testkit::{scenario, World};
use std::time::{Duration, Instant};

/// 按 `nexus ssh-config` 的输出配置 laptop 上的 ssh。除此之外不做任何 SSH 配置，也没有密钥。
async fn configure_ssh(w: &World, node: &str) {
    let out = w
        .host(
            node,
            &[
                "sh",
                "-c",
                "mkdir -p ~/.ssh && nexus ssh-config >> ~/.ssh/config",
            ],
            b"",
        )
        .await;
    assert_eq!(out.code, 0, "{}", out.stderr);
}

/// ssh 按系统账号的主目录找配置，不看 `HOME`；测试里每个节点有自己的 `HOME`，所以用 -F 明确指定。
fn config(w: &World, node: &str) -> String {
    w.home_dir(node).join(".ssh/config").display().to_string()
}

/// 用 ssh 时都加 BatchMode：任何需要交互的地方（密码、确认主机密钥）都会直接失败，而不是卡住。
async fn ssh(w: &World, node: &str, args: &[&str], stdin: &[u8]) -> nexus_testkit::Output {
    let cfg = config(w, node);
    let mut argv = vec!["ssh", "-F", &cfg, "-o", "BatchMode=yes"];
    argv.extend_from_slice(args);
    w.host(node, &argv, stdin).await
}

scenario! {
    name: ssh_runs_commands,
    covers: "R24",
    nodes: ["laptop", "home"],
    sim: (pending "需要系统自带的 ssh、scp、git，只在真实后端运行"),
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        configure_ssh(&w, "laptop").await;

        let out = ssh(&w, "laptop", &["home.nexus", "echo $((40+2)); echo oops >&2; exit 3"], b"").await;
        assert_eq!(out.code, 3, "{}", out.stderr);
        assert_eq!(out.stdout, "42\n");
        assert!(out.stderr.contains("oops"), "{}", out.stderr);

        // 标准输入原样送到远端命令。
        let out = ssh(&w, "laptop", &["home.nexus", "wc -c"], b"abcde").await;
        assert_eq!((out.code, out.stdout.trim()), (0, "5"), "{}", out.stderr);

        // 第二次连接不需要任何确认（主机密钥稳定）。
        let again = ssh(&w, "laptop", &["home.nexus", "true"], b"").await;
        assert_eq!(again.code, 0, "{}", again.stderr);

        // 不认识的设备：ssh 失败，并说明原因。
        let unknown = ssh(&w, "laptop", &["nowhere.nexus", "true"], b"").await;
        assert_ne!(unknown.code, 0);
        assert!(unknown.stderr.contains("nowhere"), "{}", unknown.stderr);
    },
}

scenario! {
    name: scp_copies_files_both_ways,
    covers: "R24",
    nodes: ["laptop", "home"],
    sim: (pending "需要系统自带的 ssh、scp、git，只在真实后端运行"),
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        configure_ssh(&w, "laptop").await;
        let remote_dir = w.home_dir("home").join("inbox");
        let remote = format!("home.nexus:{}/f.txt", remote_dir.display());
        assert_eq!(ssh(&w, "laptop", &["home.nexus", &format!("mkdir -p {}", remote_dir.display())], b"").await.code, 0);

        let payload: String = (0..20_000).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        std::fs::write(w.home_dir("laptop").join("f.txt"), &payload).unwrap();
        let cfg = config(&w, "laptop");
        let up = w.host("laptop", &["scp", "-q", "-F", &cfg, "-o", "BatchMode=yes", "f.txt", &remote], b"").await;
        assert_eq!(up.code, 0, "{}", up.stderr);
        assert_eq!(std::fs::read_to_string(remote_dir.join("f.txt")).unwrap(), payload);

        let down = w.host("laptop", &["scp", "-q", "-F", &cfg, "-o", "BatchMode=yes", &remote, "back.txt"], b"").await;
        assert_eq!(down.code, 0, "{}", down.stderr);
        assert_eq!(std::fs::read_to_string(w.home_dir("laptop").join("back.txt")).unwrap(), payload);
    },
}

scenario! {
    name: git_works_over_ssh,
    covers: "R24",
    nodes: ["laptop", "home"],
    sim: (pending "需要系统自带的 ssh、scp、git，只在真实后端运行"),
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        configure_ssh(&w, "laptop").await;
        let repo = w.home_dir("home").join("repo.git");
        let init = ssh(&w, "laptop", &["home.nexus", &format!("git init -q --bare {}", repo.display())], b"").await;
        assert_eq!(init.code, 0, "{}", init.stderr);

        let url = format!("home.nexus:{}", repo.display());
        let cfg = config(&w, "laptop");
        let script = format!(
            "set -e
             export GIT_SSH_COMMAND='ssh -F {cfg} -o BatchMode=yes'
             git init -q work && cd work
             echo hello-from-git > README
             git add README
             git -c user.name=t -c user.email=t@example.com commit -q -m first
             git push -q {url} HEAD:refs/heads/main
             cd .. && git clone -q -b main {url} clone
             cat clone/README"
        );
        let out = w.host("laptop", &["sh", "-c", &script], b"").await;
        assert_eq!(out.code, 0, "{}", out.stderr);
        assert_eq!(out.stdout, "hello-from-git\n");
    },
}

scenario! {
    name: interactive_ssh_uses_the_persistent_session,
    covers: "R24, R05",
    nodes: ["laptop", "home"],
    sim: (pending "需要系统自带的 ssh、scp、git，只在真实后端运行"),
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        configure_ssh(&w, "laptop").await;
        // 带终端登录，输入一条命令后输入结束：ssh 断开，会话继续运行。
        let out = ssh(&w, "laptop", &["-tt", "home.nexus"], b"sleep 1; printf '%s%s\\n' dur ing\r").await;
        assert_eq!(out.code, 0, "{}", out.stderr);
        w.run_for(Duration::from_secs(2)).await;

        let t = w.term("laptop", &["ssh", "home"]).await;
        // 输入可能先于提示符到达，输出会和提示符在同一行；命令行本身写的是 "dur ing"，不会误匹配。
        t.expect_contains("during").await;
        t.detach().await;

        // 在会话里退出 shell，ssh 带着它的退出码结束。
        let out = ssh(&w, "laptop", &["-tt", "home.nexus"], b"exit 4\r").await;
        assert_eq!(out.code, 4, "{}", out.stderr);
    },
}

scenario! {
    name: onboarding_cost_is_recorded,
    covers: "R27",
    nodes: ["laptop", "home"],
    sim: (pending "上手成本按真实的进程和时间计量"),
    real: yes,
    run: |w: World| async move {
        // 两台设备的常驻程序已经在运行（安装后的第一步）。从这里开始计步数和时间。
        let start = Instant::now();
        let mut steps = vec!["nexus daemon（两台设备各一次）".to_string()];
        let mut run = |s: &str| steps.push(s.to_string());

        run("laptop: nexus init");
        w.cli_ok("laptop", &["init", "--name", "laptop"]).await;
        run("home: nexus join");
        let code = w.cli_ok("home", &["join"]).await["code"].as_str().unwrap().to_string();
        run("laptop: nexus device approve <加入码> --name home");
        w.cli_ok("laptop", &["device", "approve", &code, "--name", "home"]).await;
        run("laptop: nexus ssh home");
        let t = w.term("laptop", &["ssh", "home"]).await;
        t.expect_prompt().await;
        let elapsed = start.elapsed();
        t.detach().await;

        let record = serde_json::json!({
            "steps": steps,
            "step_count": steps.len(),
            "elapsed_ms": elapsed.as_millis() as u64,
            "network": "同一台机器上的两个进程，只用回环地址",
        });
        let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("onboarding.json");
        std::fs::write(&path, serde_json::to_string_pretty(&record).unwrap()).unwrap();
        eprintln!("上手成本已记录到 {}：{record}", path.display());
        assert_eq!(steps.len(), 5, "步骤数变了，请更新 FEATURES.md 里的记录：{steps:?}");
        assert!(elapsed < Duration::from_secs(10), "从初始化到进入终端用了 {elapsed:?}");
    },
}
