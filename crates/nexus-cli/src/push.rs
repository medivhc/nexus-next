//! 从一台设备升级其他设备（M7.3b，R68）。功能点见 docs/feature-points/M7.3b.md。
//!
//! 全在命令行这一侧：经 Nexus 的执行命令，在目标设备上运行它自己的 `nexus upgrade`（常驻程序执行的命令
//! 带着 `NEXUS_EXE` 和 `NEXUS_DATA_DIR`），新版本从标准输入送过去。

use super::{call, doc, exit_for, fail, Ctl, Sink, EXIT_INVALID_INPUT, EXIT_INVALID_STATE, EXIT_TIMED_OUT, EXIT_UNREACHABLE, JSON_SCHEMA};
use nexus_wire::{CtlRequest, CtlResponse, ExecEvent, ExecSpec, Input, WireError};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

pub struct Options {
    pub file: PathBuf,
    pub confirm_ms: u64,
    pub force: bool,
    pub to: Vec<String>,
    pub all: bool,
    pub json: bool,
}

const OLD: &str = "[ -n \"$NEXUS_EXE\" ] || { echo '{\"error\":\"version\",\"message\":\"这台设备的 nexus 版本较旧，还不能从别的设备升级：先在它上面运行一次 nexus upgrade --file\"}' >&2; exit 110; }";

fn status_script() -> String {
    format!("{OLD}\nexec \"$NEXUS_EXE\" upgrade --status --json")
}

/// `$1`：确认期限；`$2`：`--force` 或空。
fn push_script() -> String {
    format!(
        "{OLD}\nf=\"$NEXUS_DATA_DIR/upgrade-incoming\"\ncat > \"$f\" || exit 106\n\
         \"$NEXUS_EXE\" upgrade --file \"$f\" --confirm-within \"$1\" $2 --json\nc=$?\nrm -f \"$f\"\nexit $c"
    )
}

/// 一台设备的结果。
struct Outcome {
    device: String,
    state: &'static str,
    from: String,
    to: String,
    message: String,
    code: i32,
}

/// 在目标设备上执行的一条命令的结果。
struct Ran {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

enum RunErr {
    /// 本机或对方回答了错误（设备不存在、离线、没有权限……）。
    Wire(WireError),
    /// 命令还没结束连接就断了（例如对方的常驻程序正在重新启动）。
    Broken,
}

async fn run_remote<C: Ctl>(ctl: &C, node: &str, script: String, args: Vec<String>, input: Option<Vec<u8>>, timeout_ms: Option<u64>) -> Result<Ran, RunErr> {
    let mut all = vec!["-c".to_string(), script, "nexus-upgrade".to_string()];
    all.extend(args);
    let spec = ExecSpec { program: "sh".into(), args: all, cwd: None, env: Vec::new(), timeout_ms, max_output: 1 << 20 };
    let mut session = ctl.open(CtlRequest::Exec { node: node.to_string(), spec }).await.map_err(|_| RunErr::Broken)?;
    let tx = session.tx.clone();
    let sender = tokio::spawn(async move {
        if let Some(data) = input {
            for chunk in data.chunks(64 * 1024) {
                if tx.send(Input::Stdin(chunk.to_vec())).await.is_err() {
                    return;
                }
            }
        }
        let _ = tx.send(Input::CloseStdin).await;
    });
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let result = loop {
        match session.rx.recv().await {
            Some(CtlResponse::Exec(ExecEvent::Stdout(b))) => stdout.extend(b),
            Some(CtlResponse::Exec(ExecEvent::Stderr(b))) => stderr.extend(b),
            Some(CtlResponse::Exec(ExecEvent::Exit(r))) => break Ok(Ran { code: r.exit_code, stdout, stderr }),
            Some(CtlResponse::Error(e)) => break Err(RunErr::Wire(e)),
            Some(_) => {}
            None => break Err(RunErr::Broken),
        }
    };
    sender.abort();
    result
}

/// 对方 `nexus upgrade ... --json` 的回答。
#[derive(Default)]
struct Report {
    state: String,
    from: String,
    to: String,
    message: String,
    id: u64,
}

fn report(stdout: &[u8]) -> Option<Report> {
    let v: Value = serde_json::from_slice(stdout).ok()?;
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
    Some(Report { state: s("state"), from: s("from"), to: s("to"), message: s("message"), id: v["id"].as_u64().unwrap_or(0) })
}

/// 没有成功的命令：退出码和原因。
fn failure(ran: &Ran) -> (i32, String) {
    let text = String::from_utf8_lossy(&ran.stderr);
    let message = text
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str::<Value>(l).ok().and_then(|v| v["message"].as_str().map(String::from)))
        .unwrap_or_else(|| text.trim().to_string());
    (ran.code.unwrap_or(super::EXIT_INTERNAL), message)
}

fn run_failure(e: RunErr) -> (i32, String) {
    match e {
        RunErr::Wire(e) => (exit_for(e.code).0, e.message),
        RunErr::Broken => (EXIT_UNREACHABLE, "到这台设备的连接断了".into()),
    }
}

/// 目标设备上最近一次升级的状态。
async fn status<C: Ctl>(ctl: &C, node: &str) -> Result<Report, (i32, String)> {
    let ran = run_remote(ctl, node, status_script(), Vec::new(), None, Some(30_000)).await.map_err(run_failure)?;
    if ran.code != Some(0) {
        return Err(failure(&ran));
    }
    report(&ran.stdout).ok_or_else(|| (super::EXIT_INTERNAL, "这台设备的 nexus upgrade --status 回答无法理解".into()))
}

async fn upgrade_one<C: Ctl>(ctl: &C, node: &str, data: &[u8], opts: &Options) -> Outcome {
    let out = |state, code, from: String, to: String, message: String| Outcome { device: node.to_string(), state, from, to, message, code };
    let before = match status(ctl, node).await {
        Ok(r) => r,
        Err((code, message)) => return out("failed", code, String::new(), String::new(), message),
    };
    let confirm = format!("{}s", opts.confirm_ms.div_ceil(1000).max(1));
    let force = if opts.force { "--force" } else { "" }.to_string();
    match run_remote(ctl, node, push_script(), vec![confirm, force], Some(data.to_vec()), None).await {
        Ok(ran) if ran.code == Some(0) => {}
        Ok(ran) => {
            let (code, message) = failure(&ran);
            return out("failed", code, String::new(), String::new(), message);
        }
        // 执行过程中连接断了：对方回答之前就开始重新启动了，看状态（离线的设备在问状态时已经报告了）。
        Err(RunErr::Wire(e)) if e.code == nexus_wire::ErrorCode::Unreachable => {}
        Err(RunErr::Wire(e)) => {
            let (code, message) = run_failure(RunErr::Wire(e));
            return out("failed", code, String::new(), String::new(), message);
        }
        Err(RunErr::Broken) => {}
    }
    let wait = Duration::from_millis(opts.confirm_ms) + Duration::from_secs(30);
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if let Ok(r) = status(ctl, node).await {
            if r.id != before.id && (r.state == "confirmed" || r.state == "rolled_back") {
                let (state, code) = if r.state == "confirmed" { ("confirmed", 0) } else { ("rolled_back", EXIT_INVALID_STATE) };
                return out(state, code, r.from, r.to, r.message);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            let message = format!("等了 {} 秒，还不知道升级的结果：稍后用 nexus exec {node} -- nexus upgrade --status 查看", wait.as_secs());
            return out("timed_out", EXIT_TIMED_OUT, String::new(), String::new(), message);
        }
    }
}

/// 要升级的设备：`--to` 给的，或者其他所有在线的设备（按名字）。
async fn targets<C: Ctl>(ctl: &C, opts: &Options) -> Result<Vec<String>, (i32, String)> {
    if !opts.all {
        let mut list: Vec<String> = Vec::new();
        for d in &opts.to {
            if !d.is_empty() && !list.contains(d) {
                list.push(d.clone());
            }
        }
        return Ok(list);
    }
    match call(ctl, CtlRequest::List).await {
        Ok(CtlResponse::List(entries)) => {
            let mut names: Vec<String> = entries.into_iter().filter(|e| !e.is_self && e.report.online).map(|e| e.report.name).collect();
            names.sort();
            if names.is_empty() {
                return Err((EXIT_INVALID_STATE, "没有在线的其他设备".into()));
            }
            Ok(names)
        }
        Ok(CtlResponse::Error(e)) => Err((exit_for(e.code).0, e.message)),
        Ok(_) => Err((super::EXIT_INTERNAL, "常驻程序的回应无法理解".into())),
        Err(e) => Err((super::EXIT_DAEMON_UNREACHABLE, format!("连不上本机的常驻程序（{e}）{}", super::DAEMON_HINT))),
    }
}

pub async fn run<C: Ctl, K: Sink>(ctl: &C, opts: &Options, sink: &mut K) -> i32 {
    let data = match std::fs::read(&opts.file) {
        Ok(d) => d,
        Err(e) => return fail(sink, EXIT_INVALID_INPUT, "invalid_input", &format!("读不了 {}：{e}", opts.file.display())),
    };
    if let Err(e) = crate::upgrade::verify_checksum(&opts.file, &data) {
        return fail(sink, EXIT_INVALID_INPUT, "invalid_input", &e.message);
    }
    let devices = match targets(ctl, opts).await {
        Ok(d) => d,
        Err((code, message)) => return fail(sink, code, "invalid_state", &message),
    };
    let mut results: Vec<Outcome> = Vec::new();
    let mut stopped = false;
    for d in &devices {
        if stopped {
            let message = "前面的设备升级后换回了，没有继续".to_string();
            results.push(Outcome { device: d.clone(), state: "skipped", from: String::new(), to: String::new(), message, code: 0 });
            continue;
        }
        if !opts.json {
            sink.err(format!("升级 {d}……\n").as_bytes());
        }
        let r = upgrade_one(ctl, d, &data, opts).await;
        stopped = r.state == "rolled_back";
        if !opts.json {
            sink.out(line(&r).as_bytes());
        }
        results.push(r);
    }
    if opts.json {
        let list: Vec<Value> = results
            .iter()
            .map(|r| json!({ "device": r.device, "state": r.state, "from": r.from, "to": r.to, "message": r.message }))
            .collect();
        sink.out(doc(json!({ "schema": JSON_SCHEMA, "devices": list })).as_bytes());
    }
    results.iter().map(|r| r.code).find(|c| *c != 0).unwrap_or(0)
}

fn line(r: &Outcome) -> String {
    let d = &r.device;
    match r.state {
        "confirmed" => format!("{d}：{} → {}，已确认（{}）\n", r.from, r.to, r.message),
        "rolled_back" => format!("{d}：换回了 {}：{}\n", r.from, r.message),
        "skipped" => format!("{d}：没有升级（{}）\n", r.message),
        "failed" => format!("{d}：没有升级：{}\n", r.message),
        _ => format!("{d}：{}\n", r.message),
    }
}
