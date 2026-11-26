//! `nexus desktop`：远程桌面（M11.1，R77）。不自己编码画面：把本机现成的 VNC / RDP 服务端发布成服务，
//! 使用方在本机开端口、启动现成的查看器，每个连接经 Nexus 到对方。功能点见 docs/feature-points/M11.1.md。

use crate::{
    call, doc, fail, fail_wire, forward_forever, service_info, Ctl, Sink, EXIT_INTERNAL, EXIT_INVALID_INPUT, EXIT_NOT_FOUND,
    JSON_SCHEMA,
};
use clap::Subcommand;
use nexus_wire::{CtlRequest, CtlResponse, ServiceInfo};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Subcommand, Debug)]
pub enum DesktopCmd {
    /// 把本机的远程桌面（VNC 或 RDP 服务端）发布出去
    ///
    /// 不给 --port 时依次试本机的 5900、5901（VNC）和 3389（RDP）。
    /// 例：nexus desktop share；nexus desktop share --port 5900 --proto vnc
    Share {
        /// 服务的名字
        #[arg(long, default_value = "desktop")]
        name: String,
        /// 远程桌面服务端在本机监听的端口
        #[arg(long)]
        port: Option<u16>,
        /// vnc 或 rdp；不给时自动识别
        #[arg(long, value_parser = ["vnc", "rdp"])]
        proto: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 用本机的查看器打开另一台设备的桌面，直到按 Ctrl-C
    ///
    /// 在 127.0.0.1 上开一个端口接到那台设备的远程桌面，用 $NEXUS_DESKTOP_VIEWER（没有时按系统挑一个）打开。
    /// 例：nexus desktop open home；nexus desktop open home/screen
    Open {
        /// <设备>，或 <设备>/<服务>（那台设备有不止一个远程桌面时）
        target: String,
        /// 本机端口；默认挑一个空闲的
        #[arg(long)]
        port: Option<u16>,
        /// 不启动查看器，只显示地址
        #[arg(long)]
        no_viewer: bool,
    },
    /// 列出在线设备上能用的远程桌面
    Ls {
        #[arg(long)]
        json: bool,
    },
}

/// 服务的协议是不是远程桌面。
pub(crate) fn is_desktop(proto: &str) -> bool {
    matches!(proto, "vnc" | "rdp")
}

pub async fn run<C: Ctl, K: Sink>(cmd: &DesktopCmd, ctl: &C, sink: &mut K) -> i32 {
    match cmd {
        DesktopCmd::Share { name, port, proto, json } => share(ctl, name, *port, proto.as_deref(), *json, sink).await,
        DesktopCmd::Open { target, port, no_viewer } => open(ctl, target, *port, !*no_viewer, sink).await,
        DesktopCmd::Ls { json } => ls(ctl, *json, sink).await,
    }
}

// ---------- 被看的一方：识别并发布 ----------

/// 一个本机端口上是什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Probe {
    /// 连不上。
    Closed,
    /// 连得上，认不出（或没有识别）。
    Unknown,
    Vnc,
    Rdp,
}

/// 按服务端说的第一段话识别：VNC 先说 `RFB xxx.yyy\n`；RDP 对 X.224 连接请求回 TPKT（`0x03 0x00`）。
pub(crate) fn classify(first: &[u8]) -> Probe {
    if first.starts_with(b"RFB ") {
        Probe::Vnc
    } else if first.len() >= 2 && first[0] == 0x03 && first[1] == 0x00 {
        Probe::Rdp
    } else {
        Probe::Unknown
    }
}

/// X.224 连接请求，带 RDP 协商（请求 TLS 和 CredSSP）。RDP 服务端先等它，才说话。
const X224_CONNECTION_REQUEST: [u8; 19] =
    [0x03, 0x00, 0x00, 0x13, 0x0e, 0xe0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x08, 0x00, 0x03, 0x00, 0x00, 0x00];

/// 识别本机端口上的远程桌面服务端。`identify` 为假时只看连不连得上。
async fn probe(port: u16, identify: bool) -> Probe {
    const WAIT: Duration = Duration::from_secs(1);
    let Ok(Ok(mut s)) = tokio::time::timeout(WAIT, tokio::net::TcpStream::connect(("127.0.0.1", port))).await else {
        return Probe::Closed;
    };
    if !identify {
        return Probe::Unknown;
    }
    let mut buf = [0u8; 16];
    // VNC 的服务端先说话。
    match tokio::time::timeout(WAIT, s.read(&mut buf)).await {
        Ok(Ok(n)) if n > 0 => return classify(&buf[..n]),
        Ok(_) => return Probe::Unknown,
        Err(_) => {}
    }
    // 不说话：当作 RDP 问一句。
    if s.write_all(&X224_CONNECTION_REQUEST).await.is_err() {
        return Probe::Unknown;
    }
    match tokio::time::timeout(WAIT, s.read(&mut buf)).await {
        Ok(Ok(n)) if n > 0 => classify(&buf[..n]),
        _ => Probe::Unknown,
    }
}

/// 不给端口时依次试的端口。
const DEFAULT_PORTS: [u16; 3] = [5900, 5901, 3389];

fn how_to_enable() -> &'static str {
    if cfg!(target_os = "macos") {
        "在 系统设置 → 通用 → 共享 里打开「屏幕共享」（VNC，端口 5900）"
    } else if cfg!(windows) {
        "在 设置 → 系统 → 远程桌面 里打开远程桌面（RDP，端口 3389；Windows 家庭版没有，可以装一个 VNC 服务端）"
    } else {
        "装一个 VNC 服务端并只监听本机，例如 wayvnc 127.0.0.1（Wayland）、x11vnc -localhost（X11），\
         或者打开 GNOME 的远程桌面（RDP）；macOS 上是系统设置里的「屏幕共享」"
    }
}

async fn share<C: Ctl, K: Sink>(ctl: &C, name: &str, port: Option<u16>, proto: Option<&str>, json: bool, sink: &mut K) -> i32 {
    let candidates: Vec<u16> = match port {
        Some(p) => vec![p],
        None => DEFAULT_PORTS.to_vec(),
    };
    let mut found = None;
    let mut unknown = None;
    for p in candidates {
        match (probe(p, proto.is_none()).await, proto) {
            (Probe::Closed, _) => {}
            (_, Some(given)) => {
                found = Some((p, given));
                break;
            }
            (Probe::Vnc, None) => {
                found = Some((p, "vnc"));
                break;
            }
            (Probe::Rdp, None) => {
                found = Some((p, "rdp"));
                break;
            }
            (Probe::Unknown, None) => unknown = unknown.or(Some(p)),
        }
    }
    let Some((port, proto)) = found else {
        if let Some(p) = unknown {
            let msg = format!("本机端口 {p} 上的服务认不出是 VNC 还是 RDP：用 --proto vnc 或 --proto rdp 指定");
            return fail(sink, EXIT_INVALID_INPUT, "invalid_input", &msg);
        }
        let ports = match port {
            Some(p) => format!("端口 {p}"),
            None => "端口 5900、5901、3389".to_string(),
        };
        let msg = format!("本机{ports}上没有在运行的远程桌面服务。{}；服务端用了别的端口时加 --port", how_to_enable());
        return fail(sink, EXIT_NOT_FOUND, "not_found", &msg);
    };
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let req = CtlRequest::Publish { name: name.to_string(), addr, proto: proto.to_string(), env: Vec::new() };
    match call(ctl, req).await {
        Ok(CtlResponse::Published { path }) => {
            let device = path.split('/').next().unwrap_or_default().to_string();
            if json {
                sink.out(doc(json!({ "schema": JSON_SCHEMA, "path": path, "proto": proto, "port": port })).as_bytes());
            } else {
                let open = if name == "desktop" { device } else { path.clone() };
                sink.out(format!("已发布远程桌面 {path}（{proto}，本机端口 {port}）。其他设备：nexus desktop open {open}\n").as_bytes());
            }
            0
        }
        Ok(CtlResponse::Error(e)) => fail_wire(sink, &e),
        Ok(_) => fail(sink, EXIT_INTERNAL, "internal", "常驻程序的回应无法理解"),
        Err(e) => fail(sink, crate::EXIT_DAEMON_UNREACHABLE, "daemon_unreachable", &format!("连不上本机的常驻程序（{e}）")),
    }
}

// ---------- 看的一方：开端口、启动查看器 ----------

/// 一台设备上能用的远程桌面。
async fn desktops_on<C: Ctl>(ctl: &C, node: &str) -> Result<Vec<ServiceInfo>, nexus_wire::WireError> {
    match call(ctl, CtlRequest::Services { node: node.to_string() }).await {
        Ok(CtlResponse::Services { services, .. }) => Ok(services.into_iter().filter(|s| is_desktop(&s.proto)).collect()),
        Ok(CtlResponse::Error(e)) => Err(e),
        _ => Err(nexus_wire::WireError { code: nexus_wire::ErrorCode::Internal, message: "常驻程序的回应无法理解".into() }),
    }
}

async fn open<C: Ctl, K: Sink>(ctl: &C, target: &str, port: Option<u16>, viewer: bool, sink: &mut K) -> i32 {
    let parts: Vec<&str> = target.split('/').collect();
    let (node, name) = match parts.as_slice() {
        [node] if !node.is_empty() => (*node, None),
        [node, name] if !node.is_empty() && !name.is_empty() => (*node, Some(*name)),
        _ => return fail(sink, EXIT_INVALID_INPUT, "invalid_input", &format!("要打开的应当写成 <设备> 或 <设备>/<服务>：{target}")),
    };
    let name = match name {
        Some(n) => n.to_string(),
        None => {
            let found = match desktops_on(ctl, node).await {
                Ok(f) => f,
                Err(e) => return fail_wire(sink, &e),
            };
            match found.as_slice() {
                [] => {
                    let msg = format!("{node} 上没有你能用的远程桌面：在 {node} 上运行 nexus desktop share");
                    return fail(sink, EXIT_NOT_FOUND, "not_found", &msg);
                }
                [one] => one.name.clone(),
                many => {
                    let names: Vec<String> = many.iter().map(|s| format!("{node}/{}", s.name)).collect();
                    let msg = format!("{node} 上有不止一个远程桌面，写明要哪一个：{}", names.join("、"));
                    return fail(sink, EXIT_INVALID_INPUT, "invalid_input", &msg);
                }
            }
        }
    };
    let info = match service_info(ctl, node, &name).await {
        Ok(info) => info,
        Err((code, kind, msg)) => return fail(sink, code, kind, &msg),
    };
    if !is_desktop(&info.proto) {
        let msg = format!("{node}/{name} 不是远程桌面（协议 {}）：用 nexus open 或 nexus connect", info.proto);
        return fail(sink, EXIT_INVALID_INPUT, "invalid_input", &msg);
    }
    serve(ctl, node, &name, &info.proto, port, viewer, sink).await
}

/// 本机开端口接到远程桌面，启动查看器，直到被中断。`nexus open` 遇到远程桌面也走这里。
pub(crate) async fn serve<C: Ctl, K: Sink>(
    ctl: &C,
    node: &str,
    name: &str,
    proto: &str,
    port: Option<u16>,
    viewer: bool,
    sink: &mut K,
) -> i32 {
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port.unwrap_or(0))).await {
        Ok(l) => l,
        Err(e) => return fail(sink, EXIT_INVALID_INPUT, "invalid_input", &format!("无法在本机开端口 {}：{e}", port.unwrap_or(0))),
    };
    let local = match listener.local_addr() {
        Ok(a) => a.port(),
        Err(e) => return fail(sink, EXIT_INTERNAL, "internal", &format!("无法在本机开端口：{e}")),
    };
    let url = format!("{proto}://127.0.0.1:{local}");
    sink.out(format!("{node}/{name} → {url}（按 Ctrl-C 停止）\n").as_bytes());
    if viewer {
        if let Err(e) = launch_viewer(proto, local) {
            sink.err(format!("打不开查看器（{e}）：{}\n", manual_hint(proto, local)).as_bytes());
        }
    }
    forward_forever(ctl, &listener, node, name).await
}

/// 当前系统。
fn os() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else {
        "linux"
    }
}

/// 按系统和协议依次尝试的查看器命令行（第一个能启动的为准）。
pub(crate) fn viewer_commands(os: &str, proto: &str, port: u16) -> Vec<Vec<String>> {
    let hostport = format!("127.0.0.1:{port}");
    let url = format!("{proto}://{hostport}");
    let v = |args: &[&str]| args.iter().map(|a| a.to_string()).collect::<Vec<_>>();
    match (os, proto) {
        ("macos", "vnc") => vec![v(&["open", &url])],
        ("macos", _) => vec![v(&["open", &format!("rdp://full%20address=s:{hostport}")])],
        ("windows", "vnc") => vec![v(&["vncviewer", &format!("127.0.0.1::{port}")])],
        ("windows", _) => vec![v(&["mstsc", &format!("/v:{hostport}")])],
        (_, "vnc") => vec![
            v(&["vncviewer", &format!("127.0.0.1::{port}")]),
            v(&["remmina", "-c", &url]),
            v(&["xdg-open", &url]),
        ],
        (_, _) => vec![
            v(&["xfreerdp3", &format!("/v:{hostport}")]),
            v(&["xfreerdp", &format!("/v:{hostport}")]),
            v(&["remmina", "-c", &url]),
        ],
    }
}

/// 找不到查看器时怎么手动连。
fn manual_hint(proto: &str, port: u16) -> String {
    let install = match (os(), proto) {
        ("linux", "vnc") => "装一个 VNC 查看器（例如 tigervnc-viewer 或 remmina）",
        ("linux", _) => "装一个 RDP 客户端（例如 freerdp 或 remmina）",
        ("windows", "vnc") => "装一个 VNC 查看器（例如 TigerVNC 或 RealVNC Viewer）",
        ("macos", "rdp") => "装 Windows App（原 Microsoft Remote Desktop）",
        _ => "用你的远程桌面查看器",
    };
    format!("{install}，连 127.0.0.1:{port}；或者设置 NEXUS_DESKTOP_VIEWER")
}

fn spawn_detached(cmd: &mut std::process::Command) -> std::io::Result<()> {
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    let mut child = cmd.spawn()?;
    // 不等它：查看器一直开着。退出了就收尾，避免留下僵尸进程。
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// 启动查看器：`$NEXUS_DESKTOP_VIEWER`（地址作为最后一个参数），否则按系统挑第一个能启动的。
fn launch_viewer(proto: &str, port: u16) -> std::io::Result<()> {
    if let Some(v) = std::env::var("NEXUS_DESKTOP_VIEWER").ok().filter(|v| !v.trim().is_empty()) {
        return spawn_detached(std::process::Command::new(v).arg(format!("{proto}://127.0.0.1:{port}")));
    }
    let mut last = std::io::Error::new(std::io::ErrorKind::NotFound, "没有查看器");
    for argv in viewer_commands(os(), proto, port) {
        match spawn_detached(std::process::Command::new(&argv[0]).args(&argv[1..])) {
            Ok(()) => return Ok(()),
            Err(e) => last = std::io::Error::new(e.kind(), format!("{}：{e}", argv[0])),
        }
    }
    Err(last)
}

// ---------- 列出 ----------

async fn ls<C: Ctl, K: Sink>(ctl: &C, json: bool, sink: &mut K) -> i32 {
    let entries = match call(ctl, CtlRequest::List).await {
        Ok(CtlResponse::List(entries)) => entries,
        Ok(CtlResponse::Error(e)) => return fail_wire(sink, &e),
        Ok(_) => return fail(sink, EXIT_INTERNAL, "internal", "常驻程序的回应无法理解"),
        Err(e) => return fail(sink, crate::EXIT_DAEMON_UNREACHABLE, "daemon_unreachable", &format!("连不上本机的常驻程序（{e}）")),
    };
    let mut rows: Vec<(String, ServiceInfo)> = Vec::new();
    for e in entries.iter().filter(|e| e.report.online || e.is_self) {
        // 问不到的设备（例如版本太旧、刚断开）跳过。
        if let Ok(found) = desktops_on(ctl, &e.report.name).await {
            rows.extend(found.into_iter().map(|s| (e.report.name.clone(), s)));
        }
    }
    if json {
        let list: Vec<Value> = rows
            .iter()
            .map(|(d, s)| json!({ "path": format!("{d}/{}", s.name), "device": d, "name": s.name, "proto": s.proto }))
            .collect();
        sink.out(doc(json!({ "schema": JSON_SCHEMA, "desktops": list })).as_bytes());
    } else if rows.is_empty() {
        sink.out("没有你能用的远程桌面：在要被看的设备上运行 nexus desktop share\n".as_bytes());
    } else {
        for (d, s) in &rows {
            sink.out(format!("{:<24} {}\n", format!("{d}/{}", s.name), s.proto).as_bytes());
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    // FP-11.1-05 covers: R77
    #[test]
    fn classify_recognises_vnc_and_rdp() {
        assert_eq!(classify(b"RFB 003.008\n"), Probe::Vnc);
        assert_eq!(classify(&[0x03, 0x00, 0x00, 0x13, 0x0e, 0xd0]), Probe::Rdp);
        assert_eq!(classify(b"SSH-2.0-OpenSSH\r\n"), Probe::Unknown);
        assert_eq!(classify(b""), Probe::Unknown);
        assert_eq!(classify(&[0x03]), Probe::Unknown);
    }

    #[test]
    fn viewers_follow_the_platform_table() {
        let first = |os, proto| viewer_commands(os, proto, 5555).remove(0).join(" ");
        assert_eq!(first("macos", "vnc"), "open vnc://127.0.0.1:5555");
        assert_eq!(first("macos", "rdp"), "open rdp://full%20address=s:127.0.0.1:5555");
        assert_eq!(first("windows", "vnc"), "vncviewer 127.0.0.1::5555");
        assert_eq!(first("windows", "rdp"), "mstsc /v:127.0.0.1:5555");
        let linux_vnc: Vec<String> = viewer_commands("linux", "vnc", 5555).into_iter().map(|a| a[0].clone()).collect();
        assert_eq!(linux_vnc, ["vncviewer", "remmina", "xdg-open"]);
        let linux_rdp: Vec<String> = viewer_commands("linux", "rdp", 5555).into_iter().map(|a| a.join(" ")).collect();
        assert_eq!(linux_rdp, ["xfreerdp3 /v:127.0.0.1:5555", "xfreerdp /v:127.0.0.1:5555", "remmina -c rdp://127.0.0.1:5555"]);
    }

    #[test]
    fn only_vnc_and_rdp_are_desktops() {
        assert!(is_desktop("vnc") && is_desktop("rdp"));
        assert!(!is_desktop("http") && !is_desktop("tcp"));
    }

    #[tokio::test]
    async fn probe_tells_vnc_rdp_silent_and_closed_apart() {
        use tokio::net::TcpListener;
        // VNC：先说话
        let vnc = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let vnc_port = vnc.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = vnc.accept().await {
                let _ = s.write_all(b"RFB 003.008\n").await;
                tokio::spawn(async move { tokio::time::sleep(Duration::from_secs(5)).await; drop(s) });
            }
        });
        // RDP：收到连接请求才回 TPKT
        let rdp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let rdp_port = rdp.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = rdp.accept().await {
                tokio::spawn(async move {
                    let mut req = [0u8; 19];
                    if s.read_exact(&mut req).await.is_ok() && req[..2] == [0x03, 0x00] {
                        let _ = s.write_all(&[0x03, 0x00, 0x00, 0x13, 0x0e, 0xd0]).await;
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                });
            }
        });
        // 不说话
        let mute = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mute_port = mute.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((s, _)) = mute.accept().await {
                held.push(s);
            }
        });
        let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed_port = closed.local_addr().unwrap().port();
        drop(closed);

        assert_eq!(probe(vnc_port, true).await, Probe::Vnc);
        assert_eq!(probe(rdp_port, true).await, Probe::Rdp);
        assert_eq!(probe(mute_port, true).await, Probe::Unknown);
        assert_eq!(probe(mute_port, false).await, Probe::Unknown);
        assert_eq!(probe(closed_port, true).await, Probe::Closed);
    }
}
