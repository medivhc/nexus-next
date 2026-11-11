//! 命令行的客户端部分：解析参数、向常驻程序发起控制调用、格式化输出。
//!
//! 这里的入口函数不接触进程、终端或网络，由调用方注入控制通道，
//! 因此仿真测试可以直接调用它，行为与真实的 `nexus` 二进制一致。
#![forbid(unsafe_code)]

use clap::{Parser, Subcommand};
use nexus_core_proto::roster::DeviceRole;
use nexus_core_proto::PathKind;
use nexus_runtime::CtlClient;
use nexus_wire::{CtlRequest, CtlResponse, ErrorCode, ExecEvent, ExecResult, ExecSpec, Input, ListEntry, StatusReport};
use serde_json::{json, Value};
use std::future::Future;
use std::io;
use std::path::PathBuf;
use tokio::io::{AsyncRead, AsyncReadExt};

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
/// `nexus exec` 的命令因超时被终止（与 timeout(1) 一致）。
pub const EXIT_TIMED_OUT: i32 = 124;
/// `nexus exec` 的命令因输出超过上限被终止。
pub const EXIT_TRUNCATED: i32 = 125;
/// `nexus exec` 的命令无法启动（与 shell 一致）。
pub const EXIT_SPAWN_FAILED: i32 = 127;

/// `nexus exec` 默认的输出上限。
pub const DEFAULT_MAX_OUTPUT: u64 = 64 * 1024 * 1024;

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
    /// 在一台设备上执行命令，返回它的输出和退出码
    ///
    /// 命令不经过 shell。本机的标准输入会转发给远端命令。
    /// 退出码与远端命令相同；超时为 124，输出超限为 125，无法启动为 127。
    Exec {
        /// 节点的名字或标识
        node: String,
        /// 超过这么多秒就终止命令
        #[arg(long, value_name = "秒")]
        timeout: Option<f64>,
        /// 输出（标准输出与标准错误合计）的字节数上限，超过就终止命令
        #[arg(long, value_name = "字节", default_value_t = DEFAULT_MAX_OUTPUT)]
        max_output: u64,
        /// 工作目录（默认是远端用户的主目录）
        #[arg(long, value_name = "目录")]
        cwd: Option<String>,
        /// 追加环境变量，可重复
        #[arg(long = "env", value_name = "名字=值")]
        env: Vec<String>,
        /// 结束后输出一份 JSON，包含完整的输出、退出码和是否超时、是否截断
        #[arg(long)]
        json: bool,
        /// 要执行的程序及其参数
        #[arg(last = true, required = true, value_name = "命令")]
        command: Vec<String>,
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

/// 到本机常驻程序的控制通道。每次调用打开一个会话。
pub trait Ctl {
    fn open(&self, req: CtlRequest) -> impl Future<Output = io::Result<CtlClient>>;
}

/// 命令的输出去处。真实的命令行写到终端，测试里收集起来。
pub trait Sink {
    fn out(&mut self, bytes: &[u8]);
    fn err(&mut self, bytes: &[u8]);
}

/// 把输出收集到内存里的 [`Sink`]。
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Sink for Captured {
    fn out(&mut self, bytes: &[u8]) {
        self.stdout.extend_from_slice(bytes);
    }
    fn err(&mut self, bytes: &[u8]) {
        self.stderr.extend_from_slice(bytes);
    }
}

/// 一条命令的完整结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub code: i32,
    /// 标准输出（不是合法 UTF-8 的部分被替换）。
    pub stdout: String,
    pub stderr: String,
    /// 标准输出的原始字节。
    pub stdout_raw: Vec<u8>,
}

impl Output {
    pub fn from_captured(code: i32, c: Captured) -> Self {
        Self {
            code,
            stdout: String::from_utf8_lossy(&c.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&c.stderr).into_owned(),
            stdout_raw: c.stdout,
        }
    }
}

fn fail<K: Sink>(sink: &mut K, code: i32, kind: &str, message: &str) -> i32 {
    let line = json!({ "error": kind, "message": message });
    sink.err(format!("{line}\n").as_bytes());
    code
}

fn fail_wire<K: Sink>(sink: &mut K, e: &nexus_wire::WireError) -> i32 {
    let (code, kind) = match e.code {
        ErrorCode::UnknownNode => (EXIT_UNKNOWN_NODE, "unknown_node"),
        ErrorCode::Denied => (EXIT_DENIED, "denied"),
        ErrorCode::InvalidState => (EXIT_INVALID_STATE, "invalid_state"),
        ErrorCode::InvalidInput => (EXIT_INVALID_INPUT, "invalid_input"),
        ErrorCode::Unreachable => (EXIT_UNREACHABLE, "unreachable"),
        ErrorCode::SpawnFailed => (EXIT_SPAWN_FAILED, "spawn_failed"),
        _ => (EXIT_INTERNAL, "internal"),
    };
    fail(sink, code, kind, &e.message)
}

/// 解析参数。帮助和版本信息写到 `sink` 后以 `Err(0)` 返回，用法错误以 `Err(EXIT_USAGE)` 返回。
pub fn parse<I, A, K>(args: I, sink: &mut K) -> Result<Cli, i32>
where
    I: IntoIterator<Item = A>,
    A: Into<std::ffi::OsString> + Clone,
    K: Sink,
{
    let argv = std::iter::once("nexus".into()).chain(args.into_iter().map(Into::into));
    Cli::try_parse_from(argv).map_err(|e| {
        let text = e.render().to_string();
        if e.use_stderr() {
            sink.err(text.as_bytes());
            EXIT_USAGE
        } else {
            sink.out(text.as_bytes());
            0
        }
    })
}

const DAEMON_HINT: &str = "，请先运行 nexus daemon";

async fn call<C: Ctl>(ctl: &C, req: CtlRequest) -> io::Result<CtlResponse> {
    let mut session = ctl.open(req).await?;
    session
        .rx
        .recv()
        .await
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "常驻程序没有回应"))
}

/// 执行一条客户端命令，返回退出码。`stdin` 只有 `exec` 会读取。
pub async fn run_client<C: Ctl, R: AsyncRead + Unpin, K: Sink>(cmd: &Cmd, ctl: &C, stdin: &mut R, sink: &mut K) -> i32 {
    let (req, json) = match cmd {
        Cmd::Daemon { .. } => return fail(sink, EXIT_USAGE, "usage", "daemon 不是客户端命令"),
        Cmd::Exec { node, timeout, max_output, cwd, env, json, command } => {
            let mut pairs = Vec::new();
            for kv in env {
                match kv.split_once('=') {
                    Some((k, v)) if !k.is_empty() => pairs.push((k.to_string(), v.to_string())),
                    _ => return fail(sink, EXIT_USAGE, "usage", &format!("--env 的格式应为 名字=值：{kv}")),
                }
            }
            if timeout.is_some_and(|t| !(t > 0.0 && t.is_finite())) {
                return fail(sink, EXIT_USAGE, "usage", "--timeout 必须是正数");
            }
            let spec = ExecSpec {
                program: command[0].clone(),
                args: command[1..].to_vec(),
                cwd: cwd.clone(),
                env: pairs,
                timeout_ms: timeout.map(|t| (t * 1000.0).ceil() as u64),
                max_output: *max_output,
            };
            return exec(ctl, CtlRequest::Exec { node: node.clone(), spec }, *json, stdin, sink).await;
        }
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
    let resp = match call(ctl, req).await {
        Err(e) => {
            return fail(sink, EXIT_DAEMON_UNREACHABLE, "daemon_unreachable", &format!("连不上本机的常驻程序（{e}）{DAEMON_HINT}"))
        }
        Ok(r) => r,
    };
    let text = match resp {
        CtlResponse::Error(e) => return fail_wire(sink, &e),
        CtlResponse::Exec(_) => return fail(sink, EXIT_INTERNAL, "internal", "常驻程序的回应无法理解"),
        CtlResponse::Status(r) => if json { doc(status_json(&r)) } else { status_text(&r) },
        CtlResponse::List(entries) => if json { list_json(&entries) } else { list_text(&entries) },
        CtlResponse::Init(r) => {
            let v = json!({
                "schema": JSON_SCHEMA,
                "name": r.name,
                "node_id": r.node_id.to_string(),
                "root": nexus_core_proto::NodeId(r.root.0).to_string(),
                "recovery_code": r.recovery_code,
            });
            if json {
                doc(v)
            } else {
                format!(
                    "已建立网络，本机 {} 是管理设备。\n\n恢复码（请离线保存，所有设备都丢失时用它恢复；它只显示这一次）：\n  {}\n\n在另一台设备上运行 `nexus join`，再回到这里用 `nexus device approve <加入码>` 批准它。\n",
                    r.name, r.recovery_code
                )
            }
        }
        CtlResponse::JoinCode { code, expires_at } => if json {
            doc(json!({ "schema": JSON_SCHEMA, "code": code, "expires_at_ms": expires_at.0 }))
        } else {
            format!("加入码（10 分钟内有效）：\n\n{code}\n\n在管理设备上运行：nexus device approve <加入码>\n")
        },
        CtlResponse::Membership(m) => {
            let v = json!({
                "schema": JSON_SCHEMA,
                "name": m.name,
                "node_id": m.node_id.to_string(),
                "roster_version": m.roster_version,
                "pushed": m.pushed,
                "unreachable": m.unreachable,
            });
            if json {
                doc(v)
            } else {
                let mut s = format!("完成：{}，名册版本 {}，已通知 {} 台设备。\n", m.name, m.roster_version, m.pushed);
                if !m.unreachable.is_empty() {
                    s.push_str(&format!("暂时没联系上：{}。它们会在下次与已更新的设备通信时得到新名册。\n", m.unreachable.join("、")));
                }
                s
            }
        }
    };
    sink.out(text.as_bytes());
    0
}

/// `nexus exec`：把本机标准输入送过去，把输出实时写出（或在 `--json` 时最后一次性输出）。
async fn exec<C: Ctl, R: AsyncRead + Unpin, K: Sink>(ctl: &C, req: CtlRequest, json: bool, stdin: &mut R, sink: &mut K) -> i32 {
    let mut session = match ctl.open(req).await {
        Ok(s) => s,
        Err(e) => {
            return fail(sink, EXIT_DAEMON_UNREACHABLE, "daemon_unreachable", &format!("连不上本机的常驻程序（{e}）{DAEMON_HINT}"))
        }
    };
    let mut collected = Captured::default();
    let mut buf = vec![0u8; 64 * 1024];
    let mut stdin_open = true;
    let result: ExecResult = loop {
        tokio::select! {
            biased;
            resp = session.rx.recv() => match resp {
                Some(CtlResponse::Exec(ExecEvent::Stdout(b))) => if json { collected.out(&b) } else { sink.out(&b) },
                Some(CtlResponse::Exec(ExecEvent::Stderr(b))) => if json { collected.err(&b) } else { sink.err(&b) },
                Some(CtlResponse::Exec(ExecEvent::Exit(r))) => break r,
                Some(CtlResponse::Error(e)) => return fail_wire(sink, &e),
                Some(_) => return fail(sink, EXIT_INTERNAL, "internal", "常驻程序的回应无法理解"),
                None => return fail(sink, EXIT_INTERNAL, "internal", "常驻程序提前结束了会话"),
            },
            n = stdin.read(&mut buf), if stdin_open => {
                let input = match n {
                    Ok(0) | Err(_) => {
                        stdin_open = false;
                        Input::CloseStdin
                    }
                    Ok(n) => Input::Stdin(buf[..n].to_vec()),
                };
                if session.tx.send(input).await.is_err() {
                    stdin_open = false;
                }
            }
        }
    };
    if json {
        let (stdout, stdout_b64) = text_or_base64(&collected.stdout);
        let (stderr, stderr_b64) = text_or_base64(&collected.stderr);
        let v = json!({
            "schema": JSON_SCHEMA,
            "exit_code": result.exit_code,
            "signal": result.signal,
            "timed_out": result.timed_out,
            "truncated": result.truncated,
            "duration_ms": result.duration_ms,
            "stdout": stdout,
            "stdout_base64": stdout_b64,
            "stderr": stderr,
            "stderr_base64": stderr_b64,
        });
        sink.out(doc(v).as_bytes());
    }
    exec_exit_code(&result, json, sink)
}

fn exec_exit_code<K: Sink>(r: &ExecResult, json: bool, sink: &mut K) -> i32 {
    let note = |sink: &mut K, kind: &str, msg: String| {
        if !json {
            fail(sink, 0, kind, &msg);
        }
    };
    if r.timed_out {
        note(sink, "timed_out", format!("命令在 {} 毫秒后因超时被终止", r.duration_ms));
        return EXIT_TIMED_OUT;
    }
    if r.truncated {
        note(sink, "truncated", "命令的输出超过上限，已被终止；上面的输出不完整".to_string());
        return EXIT_TRUNCATED;
    }
    match (r.exit_code, r.signal) {
        (Some(c), _) => c,
        (None, Some(s)) => 128 + s,
        (None, None) => EXIT_INTERNAL,
    }
}

/// 合法 UTF-8 时给出文本，否则给出 base64。
fn text_or_base64(bytes: &[u8]) -> (Option<String>, Option<String>) {
    match std::str::from_utf8(bytes) {
        Ok(s) => (Some(s.to_string()), None),
        Err(_) => (None, Some(base64(bytes))),
    }
}

/// 标准 base64（带填充）。
pub fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_examples() {
        for (input, want) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64(input.as_bytes()), want);
        }
        assert_eq!(base64(&[0xff, 0x00, b'x']), "/wB4");
    }
}
