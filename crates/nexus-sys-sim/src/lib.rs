//! 驱动：`System` 的仿真实现。时间来自 turmoil 的模拟时钟，随机数来自种子，
//! 存储在内存里并且可以跨「重启」保留。
#![forbid(unsafe_code)]

use nexus_core_proto::Timestamp;
use nexus_sys::{Cmd, ExitStatus, Proc, ProcEvent, System};
use std::collections::VecDeque;
use std::time::Duration;
use tokio::sync::Notify;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::io;
use std::rc::Rc;

/// 仿真世界的起始时间：2026-01-01T00:00:00Z。
pub const SIM_EPOCH_MS: u64 = 1_767_225_600_000;

/// 一个节点的「磁盘」。克隆后共享同一份内容，用来模拟重启后数据仍在。
#[derive(Clone, Default)]
pub struct SimDisk(Rc<RefCell<BTreeMap<String, Vec<u8>>>>);

/// 仿真世界共用的时钟偏移。拨动它相当于所有机器的时钟一起向前跳了一段，
/// 用来验证「过期」这类只取决于当前时间的行为，而不必真的模拟这段时间里的每一毫秒。
#[derive(Clone, Default)]
pub struct SimClock(Rc<Cell<u64>>);

impl SimClock {
    pub fn jump(&self, d: std::time::Duration) {
        self.0.set(self.0.get() + d.as_millis() as u64);
    }
}

pub struct SimSystem {
    disk: SimDisk,
    rng: Cell<u64>,
    clock: SimClock,
}

impl SimSystem {
    pub fn new(disk: SimDisk, seed: u64, clock: SimClock) -> Self {
        Self { disk, rng: Cell::new(seed ^ 0x9e37_79b9_7f4a_7c15), clock }
    }

    fn next_u64(&self) -> u64 {
        // splitmix64：足够用于可复现的测试数据，不用于真实密钥。
        let mut z = self.rng.get().wrapping_add(0x9e37_79b9_7f4a_7c15);
        self.rng.set(z);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

impl System for SimSystem {
    type Proc = SimProc;

    fn spawn(&self, cmd: Cmd) -> io::Result<SimProc> {
        let ctx = ShellCtx { cwd: cmd.cwd.clone().unwrap_or_else(|| "/home/sim".into()), env: cmd.env.clone() };
        let mut argv = vec![cmd.program.clone()];
        argv.extend(cmd.args.iter().cloned());
        let steps = if cmd.program == "sh" {
            match cmd.args.as_slice() {
                [flag, script] if flag == "-c" => run_script(script, &ctx),
                _ => return Err(io::Error::new(io::ErrorKind::Unsupported, "仿真的 sh 只支持 sh -c")),
            }
        } else {
            match run_command(&argv, &ctx) {
                Some((mut steps, status)) => {
                    steps.push(Step::Exit(status));
                    steps
                }
                None => return Err(io::Error::new(io::ErrorKind::NotFound, format!("没有这个程序：{}", cmd.program))),
            }
        };
        Ok(SimProc {
            steps: steps.into(),
            stdin: VecDeque::new(),
            stdin_closed: false,
            killed: false,
            sleep_until: None,
            notify: Notify::new(),
        })
    }

    fn now(&self) -> Timestamp {
        let elapsed = turmoil::sim_elapsed().unwrap_or_default();
        Timestamp(SIM_EPOCH_MS + self.clock.0.get() + elapsed.as_millis() as u64)
    }

    fn random(&self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let v = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }

    fn store_get(&self, key: &str) -> io::Result<Option<Vec<u8>>> {
        Ok(self.disk.0.borrow().get(key).cloned())
    }

    fn store_put(&self, key: &str, value: &[u8]) -> io::Result<()> {
        self.disk.0.borrow_mut().insert(key.to_string(), value.to_vec());
        Ok(())
    }
}

// ---------------------------------------------------------------- 仿真的进程

/// 仿真进程按顺序执行的步骤。
#[derive(Debug)]
enum Step {
    Out(Vec<u8>),
    Err(Vec<u8>),
    Sleep(Duration),
    /// 把标准输入原样复制到标准输出，直到输入关闭。
    Cat,
    Exit(i32),
}

/// 一个脚本化的进程。只认识验收场景用到的少数几个命令，行为与真实的同名命令一致。
pub struct SimProc {
    steps: VecDeque<Step>,
    stdin: VecDeque<Vec<u8>>,
    stdin_closed: bool,
    killed: bool,
    sleep_until: Option<tokio::time::Instant>,
    notify: Notify,
}

/// 单个输出事件的最大字节数，和真实管道一次读到的量相当。
const CHUNK: usize = 64 * 1024;

impl Proc for SimProc {
    async fn next_event(&mut self) -> ProcEvent {
        loop {
            if self.killed {
                return ProcEvent::Exited(ExitStatus { code: None, signal: Some(9) });
            }
            match self.steps.front_mut() {
                None => return ProcEvent::Exited(ExitStatus { code: Some(0), signal: None }),
                Some(Step::Exit(code)) => return ProcEvent::Exited(ExitStatus { code: Some(*code), signal: None }),
                Some(Step::Out(bytes)) | Some(Step::Err(bytes)) if bytes.is_empty() => {
                    self.steps.pop_front();
                }
                Some(Step::Out(bytes)) => {
                    let rest = bytes.split_off(bytes.len().min(CHUNK));
                    let chunk = std::mem::replace(bytes, rest);
                    return ProcEvent::Stdout(chunk);
                }
                Some(Step::Err(bytes)) => {
                    let rest = bytes.split_off(bytes.len().min(CHUNK));
                    let chunk = std::mem::replace(bytes, rest);
                    return ProcEvent::Stderr(chunk);
                }
                Some(Step::Sleep(d)) => {
                    let d = *d;
                    let until = *self.sleep_until.get_or_insert_with(|| tokio::time::Instant::now() + d);
                    tokio::select! {
                        biased;
                        _ = tokio::time::sleep_until(until) => {
                            self.sleep_until = None;
                            self.steps.pop_front();
                        }
                        _ = self.notify.notified() => {}
                    }
                }
                Some(Step::Cat) => {
                    if let Some(chunk) = self.stdin.pop_front() {
                        return ProcEvent::Stdout(chunk);
                    }
                    if self.stdin_closed {
                        self.steps.pop_front();
                    } else {
                        self.notify.notified().await;
                    }
                }
            }
        }
    }

    fn write_stdin(&mut self, data: Vec<u8>) {
        if !self.stdin_closed {
            self.stdin.push_back(data);
            self.notify.notify_one();
        }
    }

    fn close_stdin(&mut self) {
        self.stdin_closed = true;
        self.notify.notify_one();
    }

    fn kill(&mut self) {
        self.killed = true;
        self.notify.notify_one();
    }
}

struct ShellCtx {
    cwd: String,
    env: Vec<(String, String)>,
}

/// 执行 `sh -c` 的脚本：以 `;` 分隔的命令依次执行，退出码是最后一条命令的。
fn run_script(script: &str, ctx: &ShellCtx) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut status = 0;
    for stmt in script.split(';') {
        let words = tokenize(stmt);
        if words.is_empty() {
            continue;
        }
        if words[0] == "exit" {
            status = words.get(1).and_then(|w| w.parse().ok()).unwrap_or(status);
            break;
        }
        match run_command(&words, ctx) {
            Some((mut s, st)) => {
                steps.append(&mut s);
                status = st;
            }
            None => {
                steps.push(Step::Err(format!("sh: 1: {}: not found\n", words[0]).into_bytes()));
                status = 127;
            }
        }
    }
    steps.push(Step::Exit(status));
    steps
}

/// 执行一条命令，返回它的步骤和退出码；不认识的命令返回 `None`。
fn run_command(argv: &[String], ctx: &ShellCtx) -> Option<(Vec<Step>, i32)> {
    let (argv, to_stderr) = match argv.split_last() {
        Some((last, rest)) if last == ">&2" => (rest, true),
        _ => (argv, false),
    };
    let emit = |bytes: Vec<u8>| if to_stderr { Step::Err(bytes) } else { Step::Out(bytes) };
    let args = &argv[1..];
    let result = match argv[0].as_str() {
        "true" => (vec![], 0),
        "false" => (vec![], 1),
        "echo" => (vec![emit(format!("{}\n", args.join(" ")).into_bytes())], 0),
        "printf" => (vec![emit(unescape(args.first().map(String::as_str).unwrap_or("")))], 0),
        "pwd" => (vec![emit(format!("{}\n", ctx.cwd).into_bytes())], 0),
        "printenv" => match args.first().and_then(|name| ctx.env.iter().find(|(k, _)| k == name)) {
            Some((_, v)) => (vec![emit(format!("{v}\n").into_bytes())], 0),
            None => (vec![], 1),
        },
        "cat" if args.is_empty() => (vec![Step::Cat], 0),
        "sleep" => {
            let secs: f64 = args.first()?.parse().ok()?;
            (vec![Step::Sleep(Duration::from_secs_f64(secs))], 0)
        }
        "head" => match args {
            [c, n, file] if c == "-c" && file == "/dev/zero" => (vec![emit(vec![0; n.parse().ok()?])], 0),
            _ => return None,
        },
        _ => return None,
    };
    Some(result)
}

/// 按 shell 的规则切分单词：空白分隔，单引号和双引号内的内容原样保留。
fn tokenize(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    for ch in s.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(ch);
                in_word = true;
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            (None, c) => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    words
}

/// printf 格式串里的转义：\n、\t、\\ 和最多三位的八进制 \NNN。
fn unescape(fmt: &str) -> Vec<u8> {
    let b = fmt.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' || i + 1 >= b.len() {
            out.push(b[i]);
            i += 1;
            continue;
        }
        i += 1;
        match b[i] {
            b'n' => { out.push(b'\n'); i += 1; }
            b't' => { out.push(b'\t'); i += 1; }
            b'\\' => { out.push(b'\\'); i += 1; }
            b'0'..=b'7' => {
                let mut v: u32 = 0;
                let mut n = 0;
                while n < 3 && i < b.len() && (b'0'..=b'7').contains(&b[i]) {
                    v = v * 8 + (b[i] - b'0') as u32;
                    i += 1;
                    n += 1;
                }
                out.push(v as u8);
            }
            other => { out.push(b'\\'); out.push(other); i += 1; }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_and_unescape_behave_like_sh_and_printf() {
        assert_eq!(tokenize(" echo  'a b' \"c\" d "), ["echo", "a b", "c", "d"]);
        assert_eq!(tokenize("echo err >&2"), ["echo", "err", ">&2"]);
        assert_eq!(unescape("\\377\\000x"), [0xff, 0x00, b'x']);
        assert_eq!(unescape("a\\nb\\\\"), b"a\nb\\");
    }
}
