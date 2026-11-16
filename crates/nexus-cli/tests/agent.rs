//! agent 身份、授权与审计的验收场景（M1.5）。功能点见 docs/feature-points/M1.5.md。

use nexus_testkit::{scenario, World};
use nexus_wire::AgentCredential;
use serde_json::Value;
use std::time::Duration;

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("输出不是合法的 JSON（{e}）：{text:?}"))
}

fn error_kind(stderr: &str) -> String {
    json(stderr.lines().last().unwrap_or_default())["error"].as_str().unwrap_or_default().to_string()
}

/// 在管理设备上签发一份凭据，返回凭据文本。
async fn issue(w: &World, admin: &str, label: &str, grants: &[&str], ttl: &str) -> String {
    let mut args = vec!["agent", "issue", "--label", label, "--ttl", ttl];
    for g in grants {
        args.extend(["--grant", g]);
    }
    let v = w.cli_ok(admin, &args).await;
    assert_eq!(v["label"], label, "{v}");
    assert!(v["agent_id"].as_str().is_some_and(|s| s.len() == 64), "{v}");
    assert!(v["not_after_ms"].is_u64(), "{v}");
    let cred = v["credential"].as_str().expect("应当输出凭据").to_string();
    assert!(cred.starts_with("nxa1-"), "{cred}");
    cred
}

/// 以凭据的身份在 `node` 上执行一条命令。
async fn as_agent(w: &World, node: &str, cred: &str, args: &[&str]) -> nexus_testkit::Output {
    let mut full = vec!["--agent", cred];
    full.extend_from_slice(args);
    w.cli(node, &full).await
}

scenario! {
    name: agent_can_only_do_what_it_was_granted,
    covers: "R11, R12",
    nodes: ["laptop", "home", "nas"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home", "nas"]).await;
        let cred = issue(&w, "laptop", "builder", &["term.exec@home"], "4h").await;

        let ok = as_agent(&w, "laptop", &cred, &["exec", "home", "--", "echo", "hi"]).await;
        assert_eq!((ok.code, ok.stdout.as_str()), (0, "hi\n"), "{}", ok.stderr);

        // 能看到 home，但没有会话的授权。
        let denied = as_agent(&w, "laptop", &cred, &["session", "ls", "home"]).await;
        assert_eq!(denied.code, 105, "{}", denied.stderr);
        assert_eq!(error_kind(&denied.stderr), "denied");

        // 管理操作一律不允许。
        for args in [&["device", "revoke", "nas"][..], &["agent", "issue", "--label", "x", "--grant", "term.exec@home", "--ttl", "1h"]] {
            let out = as_agent(&w, "laptop", &cred, args).await;
            assert_eq!(out.code, 105, "{args:?}: {}", out.stderr);
        }

        // 身份跟着凭据走：在 nas 上用同一份凭据。
        let elsewhere = as_agent(&w, "nas", &cred, &["exec", "home", "--", "echo", "hi"]).await;
        assert_eq!(elsewhere.code, 0, "{}", elsewhere.stderr);

        // 只有管理设备能签发。
        let not_admin = w.cli("home", &["agent", "issue", "--label", "x", "--grant", "term.exec@home", "--ttl", "1h"]).await;
        assert_eq!(not_admin.code, 106, "{}", not_admin.stderr);
    },
}

scenario! {
    name: agent_sees_only_what_it_was_granted,
    covers: "R15",
    nodes: ["laptop", "home", "nas"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home", "nas"]).await;
        // 用户在 home 上开两个会话。
        for name in ["main", "build"] {
            let t = w.term("laptop", &["ssh", "home", "--session", name]).await;
            t.expect_prompt().await;
            t.detach().await;
        }
        let cred = issue(&w, "laptop", "builder", &["term.session.*@home/session/build", "term.session.list@home"], "4h").await;

        let ls = as_agent(&w, "laptop", &cred, &["ls", "--json"]).await;
        assert_eq!(ls.code, 0, "{}", ls.stderr);
        let names: Vec<String> = json(&ls.stdout)["devices"].as_array().unwrap().iter().map(|d| d["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(names, ["home"]);

        // 没有授权的设备和不存在的设备看起来一样。
        for args in [&["status", "nas"][..], &["exec", "nas", "--", "true"], &["status", "nowhere"]] {
            let out = as_agent(&w, "laptop", &cred, args).await;
            assert_eq!(out.code, 103, "{args:?}: {}", out.stderr);
        }

        let sessions = as_agent(&w, "laptop", &cred, &["session", "ls", "home", "--json"]).await;
        assert_eq!(sessions.code, 0, "{}", sessions.stderr);
        let names: Vec<String> = json(&sessions.stdout)["sessions"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(names, ["build"]);
        let kill_main = as_agent(&w, "laptop", &cred, &["session", "kill", "home", "main"]).await;
        assert_eq!(kill_main.code, 105, "{}", kill_main.stderr);
    },
}

scenario! {
    name: requests_cannot_widen_a_grant,
    covers: "R16",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let cred = issue(&w, "laptop", "builder", &["term.exec@nas"], "4h").await;
        let other = issue(&w, "laptop", "other", &["term.exec@home"], "4h").await;

        // 把授权改大：证书签名不再成立。
        let mut forged = AgentCredential::decode(&cred).expect("凭据可以解码");
        forged.cert.cert.grants[0].resource = nexus_core_proto::path::ResourcePattern::parse("**").unwrap();
        let out = as_agent(&w, "laptop", &forged.encode(), &["exec", "home", "--", "true"]).await;
        assert_eq!(out.code, 105, "{}", out.stderr);

        // 用别人的私钥配自己的证书：请求签名不成立。
        let mut mixed = AgentCredential::decode(&other).unwrap();
        mixed.secret = AgentCredential::decode(&cred).unwrap().secret;
        let out = as_agent(&w, "laptop", &mixed.encode(), &["exec", "home", "--", "true"]).await;
        assert_eq!(out.code, 105, "{}", out.stderr);

        // 不是凭据的东西被明确拒绝。
        let out = as_agent(&w, "laptop", "nxa1-zz", &["ls"]).await;
        assert_eq!(out.code, 102, "{}", out.stderr);
    },
}

scenario! {
    name: grant_expires,
    covers: "R13",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let cred = issue(&w, "laptop", "builder", &["term.exec@home"], "2s").await;
        let ok = as_agent(&w, "laptop", &cred, &["exec", "home", "--", "true"]).await;
        assert_eq!(ok.code, 0, "{}", ok.stderr);
        w.run_for(Duration::from_secs(3)).await;
        let late = as_agent(&w, "laptop", &cred, &["exec", "home", "--", "true"]).await;
        assert_eq!(late.code, 105, "{}", late.stderr);
        assert!(late.stderr.contains("过期"), "{}", late.stderr);
    },
}

scenario! {
    name: revoked_agent_is_refused,
    covers: "R14",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let cred = issue(&w, "laptop", "builder", &["term.exec@home"], "4h").await;
        assert_eq!(as_agent(&w, "laptop", &cred, &["exec", "home", "--", "true"]).await.code, 0);

        let v = w.cli_ok("laptop", &["agent", "revoke", "builder"]).await;
        assert_eq!(v["pushed"], 1, "{v}");
        let out = as_agent(&w, "laptop", &cred, &["exec", "home", "--", "true"]).await;
        assert_eq!(out.code, 105, "{}", out.stderr);

        let agents = w.cli_ok("laptop", &["agent", "ls"]).await;
        let builder = agents["agents"].as_array().unwrap().iter().find(|a| a["label"] == "builder").expect("应当列出 builder");
        assert_eq!(builder["revoked"], true);
        let unknown = w.cli("laptop", &["agent", "revoke", "nobody"]).await;
        assert_eq!(unknown.code, 109, "{}", unknown.stderr);
    },
}

scenario! {
    name: every_access_is_audited,
    covers: "R17",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let cred = issue(&w, "laptop", "builder", &["term.exec@home"], "4h").await;
        assert_eq!(as_agent(&w, "laptop", &cred, &["exec", "home", "--", "true"]).await.code, 0);
        assert_eq!(as_agent(&w, "laptop", &cred, &["session", "ls", "home"]).await.code, 105);
        // 用户本人（以设备的身份）访问一次。
        w.cli_ok("laptop", &["status", "home"]).await;

        let audit = w.cli_ok("home", &["audit"]).await;
        let events = audit["events"].as_array().expect("应当有 events 数组");
        let by_agent: Vec<&Value> = events.iter().filter(|e| e["principal"]["label"] == "builder").collect();
        assert_eq!(by_agent.len(), 2, "{audit}");
        let allowed = by_agent.iter().find(|e| e["outcome"] == "allow").expect("应当有允许的记录");
        assert_eq!(allowed["capability"], "term.exec");
        assert_eq!(allowed["resource"], "home");
        assert_eq!(allowed["peer"], "laptop");
        assert!(allowed["at_ms"].is_u64());
        assert!(allowed["principal"]["agent_id"].as_str().is_some_and(|s| s.len() == 64));
        let denied = by_agent.iter().find(|e| e["outcome"] == "deny").expect("应当有拒绝的记录");
        assert_eq!(denied["capability"], "term.session.list");
        assert_eq!(denied["reason"], "no_matching_grant");
        // 设备本身的访问也有记录。
        assert!(events.iter().any(|e| e["principal"]["device"] == "laptop"), "{audit}");
    },
}

/// 向 `nexus mcp` 发送一组 JSON-RPC 请求，返回按 id 排列的回应。
async fn mcp(w: &World, node: &str, extra: &[&str], requests: &[Value]) -> Vec<Value> {
    let input: String = requests.iter().map(|r| format!("{r}\n")).collect();
    let mut args = extra.to_vec();
    args.push("mcp");
    let out = w.cli_with_stdin(node, &args, input.as_bytes()).await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    out.stdout.lines().map(json).collect()
}

scenario! {
    name: mcp_exposes_the_tools,
    covers: "R19, R21",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let init = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}});
        let inited = serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let list = serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        let call = serde_json::json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "nexus_exec", "arguments": {"node": "home", "command": ["echo", "from-mcp"]}}});
        let bad = serde_json::json!({"jsonrpc": "2.0", "id": 4, "method": "no/such"});
        let replies = mcp(&w, "laptop", &[], &[init.clone(), inited.clone(), list.clone(), call.clone(), bad]).await;
        assert_eq!(replies.len(), 4, "通知没有回应：{replies:?}");
        assert_eq!(replies[0]["id"], 1);
        assert!(replies[0]["result"]["serverInfo"]["name"].is_string(), "{}", replies[0]);
        assert!(replies[0]["result"]["capabilities"]["tools"].is_object());
        let tools: Vec<&str> = replies[1]["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(tools, ["nexus_ls", "nexus_status", "nexus_exec", "nexus_session_ls"]);
        for t in replies[1]["result"]["tools"].as_array().unwrap() {
            assert_eq!(t["inputSchema"]["type"], "object", "{t}");
            assert!(t["description"].as_str().is_some_and(|d| !d.is_empty()));
        }
        let result = &replies[2]["result"];
        assert_eq!(result["isError"], false, "{result}");
        let payload = &result["structuredContent"];
        assert_eq!(payload["exit_code"], 0, "{result}");
        assert_eq!(payload["stdout"], "from-mcp\n");
        assert_eq!(replies[3]["error"]["code"], -32601);

        // 带凭据时以 agent 的身份执行。
        let cred = issue(&w, "laptop", "reader", &["node.info@home"], "1h").await;
        let replies = mcp(&w, "laptop", &["--agent", &cred], &[init, inited, call]).await;
        assert_eq!(replies[1]["result"]["isError"], true, "{}", replies[1]);
    },
}

scenario! {
    name: every_command_has_structured_output,
    covers: "R19, R20",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let commands: &[&[&str]] = &[
            &["status"],
            &["status", "home"],
            &["ls"],
            &["exec", "home", "--json", "--", "true"],
            &["session", "ls", "home"],
            &["agent", "ls"],
            &["audit"],
        ];
        for args in commands {
            let mut full = args.to_vec();
            if !full.contains(&"--json") {
                full.push("--json");
            }
            let out = w.cli("laptop", &full).await;
            assert_eq!(out.code, 0, "{full:?}: {}", out.stderr);
            assert!(json(&out.stdout)["schema"].is_u64(), "{full:?}: {}", out.stdout);
        }
        let out = w.cli("laptop", &["exec", "nowhere", "--json", "--", "true"]).await;
        assert_eq!(error_kind(&out.stderr), "unknown_node");
    },
}

scenario! {
    name: help_is_enough,
    covers: "R21",
    nodes: ["laptop"],
    real: yes,
    run: |w: World| async move {
        let top = w.cli("laptop", &["--help"]).await;
        assert_eq!(top.code, 0);
        for sub in ["daemon", "init", "join", "device", "ls", "exec", "ssh", "session", "agent", "audit", "mcp", "status"] {
            assert!(top.stdout.lines().any(|l| l.trim_start().starts_with(sub)), "顶层帮助没有列出 {sub}：\n{}", top.stdout);
            let help = w.cli("laptop", &[sub, "--help"]).await;
            assert_eq!(help.code, 0, "{sub}: {}", help.stderr);
            assert!(help.stdout.len() > 40, "{sub} 的帮助太短：{}", help.stdout);
        }
        assert!(top.stdout.contains("--agent") && top.stdout.contains("NEXUS_AGENT"), "{}", top.stdout);
        let issue = w.cli("laptop", &["agent", "issue", "--help"]).await;
        assert!(issue.stdout.contains("term.exec@home"), "签发的帮助应当给出授权条目的例子：{}", issue.stdout);
    },
}

scenario! {
    name: agent_can_run_under_its_own_account,
    covers: "R16",
    nodes: ["laptop", "home"],
    sim: (pending "需要多个系统账号和本机套接字，只在真实后端运行"),
    real: yes,
    run: |w: World| async move {
        use std::io::{Read, Write};
        use std::os::unix::fs::PermissionsExt;
        w.network("laptop", &["home"]).await;
        let cred = issue(&w, "laptop", "builder", &["term.exec@home"], "1h").await;

        // agent 入口对本机所有账号开放，但只接受凭据：不带凭据的请求被拒绝。
        let socket = w.agent_socket("laptop");
        let mode = std::fs::metadata(&socket).expect("应当有 agent 入口").permissions().mode();
        assert_eq!(mode & 0o777, 0o666, "{mode:o}");
        let mut conn = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        let hello = nexus_wire::CtlHello { agent: None, request: nexus_wire::CtlRequest::Status };
        conn.write_all(&nexus_wire::encode(&hello).unwrap()).unwrap();
        let mut buf = Vec::new();
        conn.read_to_end(&mut buf).unwrap();
        let mut decoder = nexus_wire::FrameDecoder::default();
        decoder.push(&buf);
        match decoder.next_frame::<nexus_wire::CtlResponse>() {
            Ok(Some(nexus_wire::CtlResponse::Error(e))) => assert_eq!(e.code, nexus_wire::ErrorCode::Denied, "{e:?}"),
            other => panic!("不带凭据应当被拒绝：{other:?}"),
        }

        let is_root = std::os::unix::fs::MetadataExt::uid(&std::fs::metadata("/proc/self").unwrap()) == 0;
        if !is_root {
            eprintln!("不是 root，跳过以其他系统账号运行的部分");
            return;
        }
        let nobody = 65534;
        // 其他账号用不了用户自己的控制入口。
        let out = w.host_as("laptop", nobody, &["nexus", "status"], b"").await;
        assert_eq!(out.code, 101, "{}", out.stderr);
        // 带着凭据，经 agent 入口可以做被授权的事，越权的照样被拒绝。
        let out = w.host_as("laptop", nobody, &["nexus", "--agent", &cred, "exec", "home", "--", "echo", "hi"], b"").await;
        assert_eq!((out.code, out.stdout.as_str()), (0, "hi\n"), "{}", out.stderr);
        let out = w.host_as("laptop", nobody, &["nexus", "--agent", &cred, "session", "ls", "home"], b"").await;
        assert_eq!(out.code, 105, "{}", out.stderr);
        let out = w.host_as("laptop", nobody, &["nexus", "--agent", &cred, "device", "revoke", "home"], b"").await;
        assert_eq!(out.code, 105, "{}", out.stderr);
    },
}

scenario! {
    name: invalid_credentials_cannot_discover_devices,
    covers: "R13, R14, R15, R16",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let cred = issue(&w, "laptop", "builder", &["term.exec@home"], "4h").await;
        let mut forged = AgentCredential::decode(&cred).unwrap();
        forged.cert.cert.grants[0].resource = nexus_core_proto::path::ResourcePattern::parse("**").unwrap();
        for args in [&["ls"][..], &["status"], &["status", "home"]] {
            let out = as_agent(&w, "laptop", &forged.encode(), args).await;
            assert_eq!(out.code, 105, "{args:?}: {}", out.stderr);
            assert!(!out.stdout.contains("home"));
        }
        w.cli_ok("laptop", &["agent", "revoke", "builder"]).await;
        for args in [&["ls"][..], &["status"], &["status", "home"]] {
            assert_eq!(as_agent(&w, "laptop", &cred, args).await.code, 105);
        }
        let short = issue(&w, "laptop", "short", &["term.exec@home"], "2s").await;
        w.run_for(Duration::from_secs(3)).await;
        for args in [&["ls"][..], &["status"], &["status", "home"]] {
            assert_eq!(as_agent(&w, "laptop", &short, args).await.code, 105);
        }
    },
}

scenario! {
    name: revocation_disconnects_existing_agent_terminals,
    covers: "R14",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let cred = issue(&w, "laptop", "builder", &["term.session.*@**"], "4h").await;
        let local = w.term("laptop", &["--agent", &cred, "ssh", "laptop"]).await;
        local.expect_prompt().await;
        let remote = w.term("laptop", &["--agent", &cred, "ssh", "home"]).await;
        remote.expect_prompt().await;
        w.cli_ok("laptop", &["agent", "revoke", "builder"]).await;
        assert_ne!(local.wait_exit().await, 0);
        assert_ne!(remote.wait_exit().await, 0);
        // 只断开失权的客户端，常驻会话本身还在。
        let human = w.term("laptop", &["ssh", "home"]).await;
        human.expect_prompt().await;
        human.detach().await;
    },
}

scenario! {
    name: expiry_disconnects_existing_agent_terminals,
    covers: "R13",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let cred = issue(&w, "laptop", "builder", &["term.session.*@**"], "5s").await;
        let local = w.term("laptop", &["--agent", &cred, "ssh", "laptop"]).await;
        local.expect_prompt().await;
        let remote = w.term("laptop", &["--agent", &cred, "ssh", "home"]).await;
        remote.expect_prompt().await;
        w.run_for(Duration::from_secs(6)).await;
        assert_ne!(local.wait_exit().await, 0);
        assert_ne!(remote.wait_exit().await, 0);
    },
}

scenario! {
    name: approval_preserves_concurrent_agent_revocation,
    covers: "R02, R14",
    nodes: ["laptop", "home", "office"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let cred = issue(&w, "laptop", "builder", &["term.exec@**"], "4h").await;
        let joined = w.cli_ok("office", &["join"]).await;
        let code = joined["code"].as_str().expect("加入码");
        let approve_args = ["device", "approve", code, "--name", "office"];
        let revoke_args = ["agent", "revoke", "builder"];
        let (approve, revoke) = tokio::join!(w.cli("laptop", &approve_args), w.cli("laptop", &revoke_args));
        assert_eq!(approve.code, 0, "{}", approve.stderr);
        assert_eq!(revoke.code, 0, "{}", revoke.stderr);
        for target in ["laptop", "home", "office"] {
            assert_eq!(as_agent(&w, "home", &cred, &["exec", target, "--", "true"]).await.code, 105);
        }
    },
}
