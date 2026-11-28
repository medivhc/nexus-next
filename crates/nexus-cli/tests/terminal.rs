//! 常驻终端的验收场景（M1.4）。功能点见 docs/feature-points/M1.4.md。

use nexus_testkit::{scenario, World};
use std::time::Duration;

/// 一段让 shell 输出 `text` 的命令，但命令本身（回显在屏幕上）不包含完整的 `text`，
/// 这样在屏幕上找到整行 `text` 就说明命令真的执行了。
fn say(text: &str) -> String {
    let (a, b) = text.split_at(text.len() / 2);
    format!("printf '%s%s\\n' {a} {b}\r")
}

scenario! {
    name: terminal_needs_no_credentials,
    covers: "R04",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let t = w.term("laptop", &["ssh", "home"]).await;
        t.expect_prompt().await;
        t.send(&say("hello")).await;
        t.expect_line("hello").await;
        assert_eq!(t.detach().await, 0);

        let out = w.cli("laptop", &["ssh", "nowhere"]).await;
        assert_eq!(out.code, 103, "{}", out.stderr);
    },
}

scenario! {
    name: session_keeps_running_while_detached,
    covers: "R05",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let t = w.term("laptop", &["ssh", "home"]).await;
        t.expect_prompt().await;
        t.send(&format!("sleep 2; {}", say("during"))).await;
        // 命令开始执行后马上断开。
        t.expect_contains("sleep 2").await;
        assert_eq!(t.detach().await, 0);
        assert!(!t.screen().lines().any(|l| l.trim_end() == "during"));
        w.run_for(Duration::from_secs(3)).await;

        let t = w.term("laptop", &["ssh", "home"]).await;
        t.expect_prompt().await;
        t.expect_line("during").await;
        t.detach().await;
    },
}

scenario! {
    name: fullscreen_program_is_restored,
    covers: "R06",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let t = w.term("laptop", &["ssh", "home"]).await;
        t.expect_prompt().await;
        t.send(&say("underneath")).await;
        t.expect_line("underneath").await;
        t.send("printf '\\033[?1049h\\033[H\\033[2JFULL%s' SCREEN\r").await;
        t.wait_until("全屏画面", |s| s.alternate_screen() && s.contents().starts_with("FULLSCREEN")).await;
        t.detach().await;

        let t = w.term("laptop", &["ssh", "home"]).await;
        t.wait_until("重连后恢复全屏画面", |s| s.alternate_screen() && s.contents().starts_with("FULLSCREEN")).await;
        // 全屏程序退出后，下面原来的 shell 内容还在。
        t.send("printf '\\033[?1049l'\r").await;
        t.wait_until("回到主屏幕", |s| !s.alternate_screen()).await;
        t.expect_line("underneath").await;
        t.detach().await;
    },
}

scenario! {
    name: clients_share_a_session,
    covers: "R07",
    nodes: ["laptop", "phone", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["phone", "home"]).await;
        let a = w.term("laptop", &["ssh", "home", "--size", "80x24"]).await;
        a.expect_prompt().await;
        let b = w.term("phone", &["ssh", "home", "--size", "100x30"]).await;
        b.expect_prompt().await;

        a.send(&say("from-laptop")).await;
        a.expect_line("from-laptop").await;
        b.expect_line("from-laptop").await;
        b.send(&say("from-phone")).await;
        a.expect_line("from-phone").await;
        b.expect_line("from-phone").await;

        // 终端尺寸跟随最近输入的客户端。
        b.send("stty size\r").await;
        b.expect_line("30 100").await;
        a.send("stty size\r").await;
        a.expect_line("24 80").await;
        a.detach().await;
        b.detach().await;
    },
}

scenario! {
    name: slow_client_does_not_hold_back_others,
    covers: "R08",
    nodes: ["laptop", "phone", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["phone", "home"]).await;
        let slow = w.term("laptop", &["ssh", "home"]).await;
        slow.expect_prompt().await;
        let fast = w.term("phone", &["ssh", "home"]).await;
        fast.expect_prompt().await;
                slow.pause();

        fast.send(&format!("seq 1 300000; {}", say("DONE"))).await;
        fast.expect_line_within("DONE", Duration::from_secs(20)).await;

        slow.resume();
        slow.expect_line("DONE").await;
        assert_eq!(slow.screen(), fast.screen(), "追上之后两个客户端的屏幕一致");
        slow.detach().await;
        fast.detach().await;
    },
}

scenario! {
    name: session_survives_a_network_outage,
    covers: "R23",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let t = w.term("laptop", &["ssh", "home"]).await;
        t.expect_prompt().await;
        t.send(&format!("sleep 2; {}", say("after-outage"))).await;
        t.expect_contains("sleep 2").await;
        w.interrupt("laptop", "home", Duration::from_secs(4)).await;
        t.expect_line("after-outage").await;
        t.send(&say("still-here")).await;
        t.expect_line("still-here").await;
        t.detach().await;
    },
}

scenario! {
    name: sessions_can_be_listed_and_ended,
    covers: "R05",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let t = w.term("laptop", &["ssh", "home"]).await;
        t.expect_prompt().await;

        let ls = w.cli_ok("laptop", &["session", "ls", "home"]).await;
        let sessions = ls["sessions"].as_array().expect("应当有 sessions 数组");
        assert_eq!(sessions.len(), 1, "{ls}");
        assert_eq!(sessions[0]["name"], "main");
        assert_eq!(sessions[0]["clients"], 1);
        assert!(sessions[0]["created_at_ms"].is_u64());
        let text = w.cli("laptop", &["session", "ls", "home"]).await;
        assert_eq!(text.code, 0);
        assert!(text.stdout.contains("main"), "{}", text.stdout);

        w.cli_ok("laptop", &["session", "kill", "home", "main"]).await;
        assert_eq!(t.wait_exit().await, 0, "会话被结束后客户端退出");
        let ls = w.cli_ok("laptop", &["session", "ls", "home"]).await;
        assert_eq!(ls["sessions"].as_array().unwrap().len(), 0, "{ls}");

        let missing = w.cli("laptop", &["session", "kill", "home", "main"]).await;
        assert_eq!(missing.code, 109, "{}", missing.stderr);

        // shell 退出时会话结束，客户端带着它的退出码退出。
        let t = w.term("laptop", &["ssh", "home"]).await;
        t.expect_prompt().await;
                t.send("exit 7\r").await;
        assert_eq!(t.wait_exit().await, 7);
    },
}

scenario! {
    name: named_sessions_are_independent,
    covers: "R05",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let main = w.term("laptop", &["ssh", "home"]).await;
        main.expect_prompt().await;
        main.send(&say("in-main")).await;
        main.expect_line("in-main").await;
        let work = w.term("laptop", &["ssh", "home", "--session", "work"]).await;
        work.expect_prompt().await;
        work.send(&say("in-work")).await;
        work.expect_line("in-work").await;
        assert!(!work.screen().contains("in-main"));
        let ls = w.cli_ok("laptop", &["session", "ls", "home"]).await;
        let mut names: Vec<&str> = ls["sessions"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
        names.sort();
        assert_eq!(names, ["main", "work"]);
        main.detach().await;
        work.detach().await;
    },
}

scenario! {
    name: session_survives_a_long_outage,
    covers: "R23",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let t = w.term("laptop", &["ssh", "home"]).await;
        t.expect_prompt().await;
        t.send(&format!("sleep 2; {}", say("after-long-outage"))).await;
        t.expect_contains("sleep 2").await;
        // 断网时间长到两端都认定连接已死：客户端自己发现并重新连上，会话还在。
        w.cut("laptop", "home", Duration::from_secs(25)).await;
        t.expect_line_within("after-long-outage", Duration::from_secs(30)).await;
        t.send(&say("reconnected")).await;
        t.expect_line("reconnected").await;
        t.detach().await;
        // 失联的旧连接已被清理：重新连上的客户端断开后（断开在对方那里生效需要一点时间），会话里没有客户端。
        let mut ls = serde_json::Value::Null;
        for _ in 0..50 {
            ls = w.cli_ok("laptop", &["session", "ls", "home"]).await;
            if ls["sessions"][0]["clients"] == 0 {
                break;
            }
            w.run_for(Duration::from_millis(100)).await;
        }
        assert_eq!(ls["sessions"][0]["clients"], 0, "{ls}");
    },
}

scenario! {
    name: revoked_device_loses_its_existing_terminal,
    covers: "R18",
    nodes: ["laptop", "home", "office"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home", "office"]).await;
        let t = w.term("office", &["ssh", "home"]).await;
        t.expect_prompt().await;
        w.cli_ok("laptop", &["device", "revoke", "office"]).await;
        assert_ne!(t.wait_exit().await, 0);
    },
}
