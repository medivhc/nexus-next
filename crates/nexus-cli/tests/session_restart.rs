//! 终端会话在常驻程序重启后保留（M7.2，R66）。功能点见 docs/feature-points/M7.2.md。
//! 只在真实后端：要真的结束和启动常驻程序。

use nexus_testkit::{scenario, World};
use std::time::Duration;

/// 让 shell 输出 `text`，但回显的命令本身不含完整的 `text`。
fn say(text: &str) -> String {
    let (a, b) = text.split_at(text.len() / 2);
    format!("printf '%s%s\\n' {a} {b}\r")
}

fn sessions_dir_entries(w: &World, node: &str) -> Vec<String> {
    let dir = w.data_dir(node).join("sessions");
    let mut v: Vec<String> = std::fs::read_dir(&dir).map(|d| d.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    v.sort();
    v
}

scenario! {
    name: session_survives_daemon_restart,
    covers: "R66",
    nodes: ["laptop", "home"],
    sim: (pending "要真的结束和启动常驻程序"),
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let t = w.term("laptop", &["ssh", "home"]).await;
        t.expect_prompt().await;
        t.send(&say("before")).await;
        t.expect_line("before").await;
        let created = w.cli_ok("laptop", &["session", "ls", "home"]).await["sessions"][0]["created_at_ms"].clone();
        // FP-7.2-02：常驻程序不在时的输出
        t.send(&format!("sleep 2; {}", say("during"))).await;
        t.expect_contains("sleep 2").await;
        // FP-7.2-01
        w.restart("home", &["--local-only"]).await;
        w.run_for(Duration::from_secs(3)).await;
        t.expect_line("during").await;
        assert!(t.screen().lines().any(|l| l.trim_end() == "before"), "{}", t.screen());
        t.send(&say("after")).await;
        t.expect_line("after").await;
        let ls = w.cli_ok("laptop", &["session", "ls", "home"]).await;
        assert_eq!(ls["sessions"][0]["name"], "main", "{ls}");
        assert_eq!(ls["sessions"][0]["created_at_ms"], created, "{ls}");
        // FP-7.2-03：重启后 exit 照常
        t.send("exit 3\r").await;
        assert_eq!(t.wait_exit().await, 3);
        w.run_for(Duration::from_secs(1)).await;
        assert_eq!(sessions_dir_entries(&w, "home"), Vec::<String>::new(), "持有进程应当退出并删掉套接字");
    },
}

scenario! {
    name: session_kill_and_exit_while_daemon_is_down,
    covers: "R66",
    nodes: ["laptop", "home"],
    sim: (pending "要真的结束和启动常驻程序"),
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        // 两个会话：build 之后被 kill，quick 在常驻程序不在时自己退出
        let build = w.term("laptop", &["ssh", "home", "--session", "build"]).await;
        build.expect_prompt().await;
        let quick = w.term("laptop", &["ssh", "home", "--session", "quick"]).await;
        quick.expect_prompt().await;
        quick.send("sleep 1; exit 5\r").await;
        quick.expect_contains("sleep 1").await;
        w.restart("home", &["--local-only"]).await;
        w.run_for(Duration::from_secs(2)).await;
        // FP-7.2-03：quick 在常驻程序不在时退出了：不在列表里
        let names: Vec<String> = w.cli_ok("laptop", &["session", "ls", "home"]).await["sessions"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(names, ["build"]);
        w.cli_ok("laptop", &["session", "kill", "home", "build"]).await;
        build.wait_exit().await;
        w.run_for(Duration::from_secs(1)).await;
        assert_eq!(sessions_dir_entries(&w, "home"), Vec::<String>::new());
        drop(quick);
    },
}
