//! 命令行的客户端部分：解析参数、向常驻程序发起控制调用、格式化输出。
//!
//! 这里的入口函数不接触进程、终端或网络，由调用方注入控制通道，
//! 因此仿真测试可以直接调用它，行为与真实的 `nexus` 二进制一致。
#![forbid(unsafe_code)]

use clap::{Parser, Subcommand};
use nexus_wire::{CtlRequest, CtlResponse, ErrorCode, StatusReport};
use std::future::Future;
use std::io;
use std::path::PathBuf;

/// `--json` 输出的结构版本。结构发生不兼容变化时递增。
pub const JSON_SCHEMA: u32 = 1;

pub const EXIT_DAEMON_UNREACHABLE: i32 = 101;
pub const EXIT_USAGE: i32 = 102;
pub const EXIT_UNKNOWN_NODE: i32 = 103;
pub const EXIT_INTERNAL: i32 = 104;

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
        let line = serde_json::json!({ "error": kind, "message": message });
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
    match cmd {
        Cmd::Daemon { .. } => Output::err(EXIT_USAGE, "usage", "daemon 不是客户端命令"),
        Cmd::Status { node, json } => {
            let req = match node {
                None => CtlRequest::Status,
                Some(n) => CtlRequest::RemoteStatus { node: n.clone() },
            };
            match ctl.call(req).await {
                Err(e) => Output::err(EXIT_DAEMON_UNREACHABLE, "daemon_unreachable", &e.to_string()),
                Ok(CtlResponse::Error(e)) => {
                    let (code, kind) = match e.code {
                        ErrorCode::UnknownNode => (EXIT_UNKNOWN_NODE, "unknown_node"),
                        _ => (EXIT_INTERNAL, "internal"),
                    };
                    Output::err(code, kind, &e.message)
                }
                Ok(CtlResponse::Status(report)) => Output::ok(render_status(&report, *json)),
            }
        }
    }
}

fn render_status(r: &StatusReport, json: bool) -> String {
    let path = r.path.map(|p| match p {
        nexus_core_proto::PathKind::Direct => "direct",
        nexus_core_proto::PathKind::Relay => "relay",
    });
    if json {
        let v = serde_json::json!({
            "schema": JSON_SCHEMA,
            "name": r.name,
            "node_id": r.node_id.to_string(),
            "online": r.online,
            "version": r.info.as_ref().map(|i| i.version.clone()),
            "started_at_ms": r.info.as_ref().map(|i| i.started_at.0),
            "uptime_ms": r.info.as_ref().map(|i| i.now.millis_since(i.started_at)),
            "rtt_ms": r.rtt_ms,
            "path": path,
        });
        return format!("{v}\n");
    }
    let mut out = format!("{}  {}\n", r.name, r.node_id.short());
    if !r.online {
        out.push_str("  离线\n");
        return out;
    }
    out.push_str("  在线");
    if let Some(p) = path {
        out.push_str(if p == "direct" { "，直连" } else { "，中继" });
    }
    if let Some(ms) = r.rtt_ms {
        out.push_str(&format!("，往返 {ms} 毫秒"));
    }
    out.push('\n');
    if let Some(i) = &r.info {
        out.push_str(&format!("  版本 {}\n", i.version));
    }
    out
}
