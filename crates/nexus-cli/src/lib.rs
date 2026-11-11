//! 命令行的客户端部分：解析参数、向常驻程序发起控制调用、格式化输出。
//!
//! 这里的入口函数不接触进程、终端或网络，由调用方注入控制通道，
//! 因此仿真测试可以直接调用它，行为与真实的 `nexus` 二进制一致。
#![forbid(unsafe_code)]

use clap::{Parser, Subcommand};
use nexus_core_proto::roster::DeviceRole;
use nexus_core_proto::PathKind;
use nexus_wire::{CtlRequest, CtlResponse, ErrorCode, ListEntry, StatusReport};
use serde_json::{json, Value};
use std::future::Future;
use std::io;
use std::path::PathBuf;

/// `--json` 输出的结构版本。结构发生不兼容变化时递增。
pub const JSON_SCHEMA: u32 = 2;

pub const EXIT_DAEMON_UNREACHABLE: i32 = 101;
pub const EXIT_USAGE: i32 = 102;
pub const EXIT_UNKNOWN_NODE: i32 = 103;
pub const EXIT_INTERNAL: i32 = 104;
pub const EXIT_DENIED: i32 = 105;
pub const EXIT_INVALID_STATE: i32 = 106;
pub const EXIT_INVALID_INPUT: i32 = 107;
pub const EXIT_UNREACHABLE: i32 = 108;

#[derive(Parser, Debug)]
#[command(name = "nexus", version, about = "把你的所有设备组成一个整体")]
pub struct Cli {
    /// 本节点的数据目录（默认 $NEXUS_DATA_DIR 或 ~/.local/share/nexus）
    #[arg(long, global = true, value_name = "目录")]
    pub data_dir: Option<PathBuf>,
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// 运行常驻程序
    Daemon {
        /// 本节点的名字（默认取主机名）
        #[arg(long)]
        name: Option<String>,
        /// 只在本机回环地址上通信，不使用中继和地址发现（用于测试）
        #[arg(long)]
        local_only: bool,
        /// 中继：default（公共中继）、disabled，或自建中继的地址
        #[arg(long, default_value = "default")]
        relay: String,
        /// 不使用公共地址发现服务
        #[arg(long)]
        no_discovery: bool,
    },
    /// 在第一台设备上建立你的网络
    Init {
        /// 这台设备在网络中的名字
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 让这台设备准备加入网络：输出一段加入码，在管理设备上用 `nexus device approve` 批准
    Join {
        #[arg(long)]
        json: bool,
    },
    /// 管理名册里的设备（只能在管理设备上执行）
    Device {
        #[command(subcommand)]
        cmd: DeviceCmd,
    },
    /// 列出你的所有设备及其状态
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// 查看本机或另一个节点的状态
    Status {
        /// 节点的名字或标识；省略时为本机
        node: Option<String>,
        /// 以 JSON 输出
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum DeviceCmd {
    /// 批准一台设备加入
    Approve {
        /// 新设备上 `nexus join` 输出的加入码
        code: String,
        /// 给它起的名字（默认用它自己建议的名字）
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 把一台设备移出名册，它将无法再访问其他设备
    Revoke {
        name: String,
        #[arg(long)]
        json: bool,
    },
}

/// 到本机常驻程序的控制通道。
pub trait Ctl {
    fn call(&self, req: CtlRequest) -> impl Future<Output = io::Result<CtlResponse>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    fn ok(stdout: String) -> Self {
        Self { code: 0, stdout, stderr: String::new() }
    }
    fn err(code: i32, kind: &str, message: &str) -> Self {
        let line = json!({ "error": kind, "message": message });
        Self { code, stdout: String::new(), stderr: format!("{line}\n") }
    }
}

/// 解析参数。帮助和版本信息以 `Err(Output)` 返回，退出码为 0。
pub fn parse<I, A>(args: I) -> Result<Cli, Output>
where
    I: IntoIterator<Item = A>,
    A: Into<std::ffi::OsString> + Clone,
{
    let argv = std::iter::once("nexus".into()).chain(args.into_iter().map(Into::into));
    Cli::try_parse_from(argv).map_err(|e| {
        let text = e.render().to_string();
        if e.use_stderr() {
            Output { code: EXIT_USAGE, stdout: String::new(), stderr: text }
        } else {
            Output::ok(text)
        }
    })
}

/// 执行一条客户端命令。
pub async fn run_client<C: Ctl>(cmd: &Cmd, ctl: &C) -> Output {
    let (req, json) = match cmd {
        Cmd::Daemon { .. } => return Output::err(EXIT_USAGE, "usage", "daemon 不是客户端命令"),
        Cmd::Status { node: None, json } => (CtlRequest::Status, *json),
        Cmd::Status { node: Some(n), json } => (CtlRequest::RemoteStatus { node: n.clone() }, *json),
        Cmd::Init { name, json } => (CtlRequest::Init { name: name.clone() }, *json),
        Cmd::Join { json } => (CtlRequest::JoinStart, *json),
        Cmd::Device { cmd: DeviceCmd::Approve { code, name, json } } => {
            (CtlRequest::Approve { code: code.clone(), name: name.clone() }, *json)
        }
        Cmd::Device { cmd: DeviceCmd::Revoke { name, json } } => (CtlRequest::Revoke { name: name.clone() }, *json),
        Cmd::Ls { json } => (CtlRequest::List, *json),
    };
    let resp = match ctl.call(req).await {
        Err(e) => return Output::err(EXIT_DAEMON_UNREACHABLE, "daemon_unreachable", &format!("连不上本机的常驻程序（{e}），请先运行 nexus daemon")),
        Ok(r) => r,
    };
    match resp {
        CtlResponse::Error(e) => {
            let (code, kind) = match e.code {
                ErrorCode::UnknownNode => (EXIT_UNKNOWN_NODE, "unknown_node"),
                ErrorCode::Denied => (EXIT_DENIED, "denied"),
                ErrorCode::InvalidState => (EXIT_INVALID_STATE, "invalid_state"),
                ErrorCode::InvalidInput => (EXIT_INVALID_INPUT, "invalid_input"),
                ErrorCode::Unreachable => (EXIT_UNREACHABLE, "unreachable"),
                _ => (EXIT_INTERNAL, "internal"),
            };
            Output::err(code, kind, &e.message)
        }
        CtlResponse::Status(r) => Output::ok(if json { doc(status_json(&r)) } else { status_text(&r) }),
        CtlResponse::List(entries) => Output::ok(if json { list_json(&entries) } else { list_text(&entries) }),
        CtlResponse::Init(r) => {
            let v = json!({
                "schema": JSON_SCHEMA,
                "name": r.name,
                "node_id": r.node_id.to_string(),
                "root": nexus_core_proto::NodeId(r.root.0).to_string(),
                "recovery_code": r.recovery_code,
            });
            Output::ok(if json {
                doc(v)
            } else {
                format!(
                    "已建立网络，本机 {} 是管理设备。\n\n恢复码（请离线保存，所有设备都丢失时用它恢复；它只显示这一次）：\n  {}\n\n在另一台设备上运行 `nexus join`，再回到这里用 `nexus device approve <加入码>` 批准它。\n",
                    r.name, r.recovery_code
                )
            })
        }
        CtlResponse::JoinCode { code, expires_at } => Output::ok(if json {
            doc(json!({ "schema": JSON_SCHEMA, "code": code, "expires_at_ms": expires_at.0 }))
        } else {
            format!("加入码（10 分钟内有效）：\n\n{code}\n\n在管理设备上运行：nexus device approve <加入码>\n")
        }),
        CtlResponse::Membership(m) => {
            let v = json!({
                "schema": JSON_SCHEMA,
                "name": m.name,
                "node_id": m.node_id.to_string(),
                "roster_version": m.roster_version,
                "pushed": m.pushed,
                "unreachable": m.unreachable,
            });
            Output::ok(if json {
                doc(v)
            } else {
                let mut s = format!("完成：{}，名册版本 {}，已通知 {} 台设备。\n", m.name, m.roster_version, m.pushed);
                if !m.unreachable.is_empty() {
                    s.push_str(&format!("暂时没联系上：{}。它们会在下次与已更新的设备通信时得到新名册。\n", m.unreachable.join("、")));
                }
                s
            })
        }
    }
}

fn doc(v: Value) -> String {
    format!("{v}\n")
}

fn path_str(p: Option<PathKind>) -> Option<&'static str> {
    p.map(|p| match p {
        PathKind::Direct => "direct",
        PathKind::Relay => "relay",
    })
}

fn role_str(r: Option<DeviceRole>) -> Option<&'static str> {
    r.map(|r| match r {
        DeviceRole::Admin => "admin",
        DeviceRole::Member => "member",
    })
}

fn report_fields(r: &StatusReport) -> serde_json::Map<String, Value> {
    let v = json!({
        "name": r.name,
        "node_id": r.node_id.to_string(),
        "online": r.online,
        "role": role_str(r.role),
        "roster_version": r.info.as_ref().and_then(|i| i.roster_version),
        "version": r.info.as_ref().map(|i| i.version.clone()),
        "started_at_ms": r.info.as_ref().map(|i| i.started_at.0),
        "uptime_ms": r.info.as_ref().map(|i| i.now.millis_since(i.started_at)),
        "rtt_ms": r.rtt_ms,
        "path": path_str(r.path),
    });
    match v {
        Value::Object(m) => m,
        _ => unreachable!(),
    }
}

fn status_json(r: &StatusReport) -> Value {
    let mut m = report_fields(r);
    m.insert("schema".into(), json!(JSON_SCHEMA));
    Value::Object(m)
}

fn list_json(entries: &[ListEntry]) -> String {
    let devices: Vec<Value> = entries
        .iter()
        .map(|e| {
            let mut m = report_fields(&e.report);
            m.insert("self".into(), json!(e.is_self));
            Value::Object(m)
        })
        .collect();
    doc(json!({ "schema": JSON_SCHEMA, "devices": devices }))
}

fn status_text(r: &StatusReport) -> String {
    let mut out = format!("{}  {}", r.name, r.node_id.short());
    if let Some(role) = r.role {
        out.push_str(if role == DeviceRole::Admin { "  管理设备" } else { "  成员" });
    }
    out.push('\n');
    if !r.online {
        out.push_str("  离线\n");
        return out;
    }
    out.push_str("  在线");
    match path_str(r.path) {
        Some("direct") => out.push_str("，直连"),
        Some(_) => out.push_str("，中继"),
        None => {}
    }
    if let Some(ms) = r.rtt_ms {
        out.push_str(&format!("，往返 {ms} 毫秒"));
    }
    out.push('\n');
    if let Some(i) = &r.info {
        out.push_str(&format!("  版本 {}", i.version));
        if let Some(v) = i.roster_version {
            out.push_str(&format!("，名册版本 {v}"));
        } else {
            out.push_str("，尚未加入网络");
        }
        out.push('\n');
    }
    out
}

fn list_text(entries: &[ListEntry]) -> String {
    let mut out = String::new();
    for e in entries {
        let r = &e.report;
        let state = if !r.online {
            "离线".to_string()
        } else if e.is_self {
            "本机".to_string()
        } else {
            let path = if path_str(r.path) == Some("relay") { "中继" } else { "直连" };
            match r.rtt_ms {
                Some(ms) => format!("在线，{path}，{ms} 毫秒"),
                None => format!("在线，{path}"),
            }
        };
        let role = match r.role {
            Some(DeviceRole::Admin) => "管理设备",
            Some(DeviceRole::Member) => "成员",
            None => "未加入网络",
        };
        out.push_str(&format!("{:<16} {}  {:<8} {}\n", r.name, r.node_id.short(), role, state));
    }
    out
}
