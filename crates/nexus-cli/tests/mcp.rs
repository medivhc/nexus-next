//! M10.2：完整能力 MCP 服务（R76）。
//!
//! 仿真和真实后端共用同一组场景。

use nexus_testkit::{scenario, World};
use serde_json::Value;

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("输出不是合法的 JSON（{e}）：{text:?}"))
}

/// 向 `nexus mcp` 发送一组 JSON-RPC 请求，返回按顺序收到的回应。
async fn mcp(w: &World, node: &str, extra: &[&str], requests: &[Value]) -> Vec<Value> {
    let input: String = requests.iter().map(|r| format!("{r}\n")).collect();
    let mut args = extra.to_vec();
    args.push("mcp");
    let out = w.cli_with_stdin(node, &args, input.as_bytes()).await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    out.stdout.lines().map(json).collect()
}

fn req_init(id: u64) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "mcp-test", "version": "1.0" }
        }
    })
}

fn req_call(id: u64, name: &str, arguments: Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments }
    })
}

scenario! {
    name: mcp_lists_all_full_tools,
    covers: "R76",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let replies = mcp(
            &w,
            "laptop",
            &[],
            &[
                req_init(1),
                serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
            ],
        )
        .await;
        assert_eq!(replies.len(), 2);
        let tools = replies[1]["result"]["tools"].as_array().expect("tools 应当是数组");
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for expected in [
            "nexus_ls",
            "nexus_status",
            "nexus_services",
            "nexus_exec",
            "nexus_session_ls",
            "nexus_session_kill",
            "nexus_drive_ls",
            "nexus_drive_read",
            "nexus_drive_write",
            "nexus_drive_rm",
            "nexus_drive_status",
            "nexus_drive_commit",
            "nexus_drive_where",
            "nexus_path",
            "nexus_route_explain",
            "nexus_audit",
        ] {
            assert!(names.contains(&expected), "缺少工具 {expected}，实际：{names:?}");
        }
        for t in tools {
            assert_eq!(t["inputSchema"]["type"], "object", "{t}");
            assert!(t["description"].as_str().is_some_and(|d| !d.is_empty()), "{t}");
        }
    },
}

scenario! {
    name: mcp_drive_read_write_commit_and_binary,
    covers: "R76",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        w.cli_ok("home", &["drive", "serve", "on"]).await;
        w.cli_ok("laptop", &["drive", "create", "work", "--replica", "home"]).await;

        let replies = mcp(
            &w,
            "laptop",
            &[],
            &[
                req_init(1),
                req_call(2, "nexus_drive_write", serde_json::json!({
                    "path": "work:/notes/hello.txt",
                    "content": "hello from mcp\n"
                })),
                // 二进制内容（包含 NUL 字节：0x00, 0x01, 0xfe, 0xff -> "AAH+/w=="）
                req_call(3, "nexus_drive_write", serde_json::json!({
                    "path": "work:/bin/blob.dat",
                    "content_base64": "AAH+/w=="
                })),
                req_call(4, "nexus_drive_ls", serde_json::json!({})),
                req_call(5, "nexus_drive_ls", serde_json::json!({ "path": "work:/notes" })),
                req_call(6, "nexus_drive_read", serde_json::json!({ "path": "work:/notes/hello.txt" })),
                req_call(7, "nexus_drive_read", serde_json::json!({
                    "path": "work:/notes/hello.txt",
                    "offset": 6,
                    "length": 4
                })),
                req_call(8, "nexus_drive_read", serde_json::json!({ "path": "work:/bin/blob.dat" })),
                req_call(9, "nexus_drive_status", serde_json::json!({ "ws": "work" })),
                req_call(10, "nexus_drive_commit", serde_json::json!({
                    "ws": "work",
                    "message": "mcp initial commit"
                })),
                req_call(11, "nexus_drive_where", serde_json::json!({ "path": "work:/notes/hello.txt" })),
                req_call(12, "nexus_drive_rm", serde_json::json!({ "path": "work:/bin/blob.dat" })),
            ],
        )
        .await;

        assert_eq!(replies.len(), 12);
        // 写文本
        assert_eq!(replies[1]["result"]["isError"], false, "{}", replies[1]);
        assert_eq!(replies[1]["result"]["structuredContent"]["size"], 15);
        // 写二进制
        assert_eq!(replies[2]["result"]["isError"], false, "{}", replies[2]);
        assert_eq!(replies[2]["result"]["structuredContent"]["size"], 4);
        // 列工作区
        let wss = replies[3]["result"]["structuredContent"]["workspaces"].as_array().unwrap();
        assert!(wss.iter().any(|x| x["name"] == "work"), "{}", replies[3]);
        // 列目录
        let entries = replies[4]["result"]["structuredContent"]["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["name"], "hello.txt");
        // 读全文文本
        let read_txt = &replies[5]["result"]["structuredContent"];
        assert_eq!(replies[5]["result"]["isError"], false, "{}", replies[5]);
        assert_eq!(read_txt["encoding"], "utf-8");
        assert_eq!(read_txt["content"], "hello from mcp\n");
        assert_eq!(read_txt["size"], 15);
        // 分段读
        let read_slice = &replies[6]["result"]["structuredContent"];
        assert_eq!(read_slice["encoding"], "utf-8");
        assert_eq!(read_slice["content"], "from");
        assert_eq!(read_slice["size"], 4);
        // 读二进制
        let read_bin = &replies[7]["result"]["structuredContent"];
        assert_eq!(replies[7]["result"]["isError"], false, "{}", replies[7]);
        assert_eq!(read_bin["encoding"], "base64");
        assert_eq!(read_bin["content_base64"], "AAH+/w==");
        assert_eq!(read_bin["size"], 4);
        // status
        let changes = replies[8]["result"]["structuredContent"]["changes"].as_array().unwrap();
        assert_eq!(changes.len(), 2, "{}", replies[8]);
        // commit
        assert_eq!(replies[9]["result"]["isError"], false, "{}", replies[9]);
        assert!(replies[9]["result"]["structuredContent"]["commit"].is_string());
        // where
        assert_eq!(replies[10]["result"]["isError"], false, "{}", replies[10]);
        // rm
        assert_eq!(replies[11]["result"]["isError"], false, "{}", replies[11]);
    },
}

scenario! {
    name: mcp_services_diagnostics_and_agent_least_privilege,
    covers: "R76",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        w.cli_ok("home", &["publish", "web", "--port", "8080", "--proto", "http"]).await;
        w.cli_ok("laptop", &["drive", "create", "work"]).await;
        let put = w.cli_with_stdin("laptop", &["drive", "put", "work:/readme.md"], b"docs").await;
        assert_eq!(put.code, 0, "{}", put.stderr);

        // 本人身份调用服务发现、路径、路由解释、审计
        let replies = mcp(
            &w,
            "laptop",
            &[],
            &[
                req_init(1),
                req_call(2, "nexus_services", serde_json::json!({ "node": "home" })),
                req_call(3, "nexus_path", serde_json::json!({ "device": "home" })),
                req_call(4, "nexus_route_explain", serde_json::json!({ "target": "web.home.internal:80" })),
                req_call(5, "nexus_audit", serde_json::json!({ "limit": 10 })),
            ],
        )
        .await;
        assert_eq!(replies[1]["result"]["isError"], false, "{}", replies[1]);
        let svcs = replies[1]["result"]["structuredContent"]["services"].as_array().unwrap();
        assert!(svcs.iter().any(|s| s["name"] == "web"), "{}", replies[1]);
        assert_eq!(replies[2]["result"]["isError"], false, "{}", replies[2]);
        assert_eq!(replies[3]["result"]["isError"], false, "{}", replies[3]);
        assert_eq!(replies[4]["result"]["isError"], false, "{}", replies[4]);

        // 只授权 drive.read@laptop/drive/work 的 agent：能读不能写、不能查审计
        let cred = w
            .cli_ok(
                "laptop",
                &[
                    "agent",
                    "issue",
                    "--label",
                    "doc-reader",
                    "--grant",
                    "drive.read@laptop/drive/work",
                    "--ttl",
                    "1h",
                ],
            )
            .await["credential"]
            .as_str()
            .unwrap()
            .to_string();

        let agent_replies = mcp(
            &w,
            "laptop",
            &["--agent", &cred],
            &[
                req_init(1),
                req_call(2, "nexus_drive_ls", serde_json::json!({ "path": "work:/" })),
                req_call(3, "nexus_drive_read", serde_json::json!({ "path": "work:/readme.md" })),
                req_call(4, "nexus_drive_write", serde_json::json!({
                    "path": "work:/hack.txt",
                    "content": "nope"
                })),
                req_call(5, "nexus_audit", serde_json::json!({})),
            ],
        )
        .await;

        assert_eq!(agent_replies[1]["result"]["isError"], false, "{}", agent_replies[1]);
        assert_eq!(agent_replies[2]["result"]["isError"], false, "{}", agent_replies[2]);
        assert_eq!(agent_replies[2]["result"]["structuredContent"]["content"], "docs");
        assert_eq!(agent_replies[3]["result"]["isError"], true, "{}", agent_replies[3]);
        assert_eq!(agent_replies[3]["result"]["structuredContent"]["error"], "denied");
        assert_eq!(agent_replies[4]["result"]["isError"], true, "{}", agent_replies[4]);
        assert_eq!(agent_replies[4]["result"]["structuredContent"]["error"], "denied");
    },
}
