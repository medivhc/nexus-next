//! 纯逻辑层：一个终端会话的状态机。
//!
//! 会话持有一个内存终端，所有输出都先经过它，因此任何时刻都知道屏幕上是什么。
//! 状态机不做 IO：输入是事件，输出是动作，由运行时去执行。
//!
//! 流控：每个客户端有一个额度窗口。发给它的字节在被确认之前占用额度；额度不够时
//! 不再给它发实时输出，把它标记为落后。它把积压的内容全部确认后，补发一份恢复数据，
//! 从当前屏幕重新开始。这样慢的客户端既不会拖慢会话，也不会让内存无限增长。
#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type ClientId = u64;

/// 终端尺寸。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

impl Size {
    pub const DEFAULT: Size = Size { cols: 80, rows: 24 };

    /// 两个维度都至少为 1。
    pub fn clamped(self) -> Size {
        Size { cols: self.cols.max(1), rows: self.rows.max(1) }
    }
}

/// 默认的流控窗口：每个客户端最多 1 MiB 未确认的数据。
pub const DEFAULT_WINDOW: u64 = 1 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// 终端程序产生的输出。
    PtyOutput(Vec<u8>),
    /// 一个客户端连上来，报告它的终端尺寸。
    Attach { client: ClientId, size: Size },
    /// 客户端的键盘输入。
    Input { client: ClientId, bytes: Vec<u8> },
    /// 客户端的终端尺寸变了。
    Resize { client: ClientId, size: Size },
    /// 客户端确认已经处理完这么多字节。
    Ack { client: ClientId, bytes: u64 },
    Detach { client: ClientId },
    /// 终端程序退出了。
    PtyExited { code: Option<i32> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Send { client: ClientId, msg: ClientMsg },
    WritePty(Vec<u8>),
    ResizePty(Size),
}

/// 发给客户端的消息。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg {
    /// 把客户端的终端重绘成当前屏幕。之后的 `Output` 都接在它后面。
    Restore(Vec<u8>),
    Output(Vec<u8>),
    /// 会话结束了。
    Exit(Option<i32>),
}

impl ClientMsg {
    /// 占用流控额度的字节数。
    pub fn cost(&self) -> u64 {
        match self {
            ClientMsg::Restore(b) | ClientMsg::Output(b) => b.len() as u64,
            ClientMsg::Exit(_) => 0,
        }
    }
}

struct Client {
    size: Size,
    credit: u64,
    lagging: bool,
}

pub struct Session {
    parser: vt100::Parser,
    size: Size,
    window: u64,
    clients: BTreeMap<ClientId, Client>,
    /// 最后一个连上来或打过字的客户端，会话尺寸跟随它。
    active: Option<ClientId>,
    /// 还没凑成完整转义序列或字符的输出尾巴。
    pending: Vec<u8>,
    exited: Option<Option<i32>>,
}

/// 未结束的转义序列最多缓存这么多字节，超过就照常放行（比如没有结束符的窗口标题）。
const MAX_PENDING: usize = 4096;

impl Session {
    pub fn new(size: Size, window: u64) -> Self {
        let size = size.clamped();
        Self {
            parser: vt100::Parser::new(nz(size.rows), nz(size.cols), 0),
            size,
            window,
            clients: BTreeMap::new(),
            active: None,
            pending: Vec::new(),
            exited: None,
        }
    }

    pub fn size(&self) -> Size {
        self.size
    }

    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    pub fn has_exited(&self) -> bool {
        self.exited.is_some()
    }

    /// 直接改变屏幕尺寸（终端的持有进程用，M7.2：尺寸由常驻程序决定，这里只跟着改）。
    pub fn set_size(&mut self, size: Size) {
        let mut ignored = Vec::new();
        self.resize(size.clamped(), &mut ignored);
    }

    /// 当前屏幕，供测试和诊断使用。
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// 把任意状态的终端重绘成当前屏幕所需的字节。
    pub fn restore(&mut self) -> Vec<u8> {
        // 先离开备用屏幕并清掉样式，客户端终端原来是什么状态都没关系。
        let mut out = String::from("\x1b[?1049l\x1b[m");
        if self.parser.screen().alternate_screen() {
            // 全屏程序下面还有 shell 的内容：先画主屏幕，再进入备用屏幕画全屏程序。
            // `?47` 只切换显示的屏幕，不清屏也不动光标，切回来后状态不变。
            self.parser.process(b"\x1b[?47l");
            out.push_str(&self.parser.screen().contents_formatted());
            self.parser.process(b"\x1b[?47h");
            out.push_str("\x1b[?1049h");
        }
        let screen = self.parser.screen();
        out.push_str(&screen.contents_formatted());
        out.push_str(&screen.input_mode_formatted());
        out.into_bytes()
    }

    /// 处理一个事件，返回要执行的动作。
    pub fn handle(&mut self, event: Event) -> Vec<Action> {
        let mut actions = Vec::new();
        match event {
            Event::PtyOutput(bytes) => self.output(bytes, &mut actions),
            Event::Attach { client, size } => {
                self.clients.insert(client, Client { size: size.clamped(), credit: self.window, lagging: false });
                if self.exited.is_none() {
                    self.activate(client, &mut actions);
                }
                let restore = ClientMsg::Restore(self.restore());
                self.send(client, restore, &mut actions);
                if let Some(code) = self.exited {
                    self.send(client, ClientMsg::Exit(code), &mut actions);
                }
            }
            Event::Input { client, bytes } => {
                if self.exited.is_some() || !self.clients.contains_key(&client) {
                    return actions;
                }
                self.activate(client, &mut actions);
                if !bytes.is_empty() {
                    actions.push(Action::WritePty(bytes));
                }
            }
            Event::Resize { client, size } => {
                let Some(c) = self.clients.get_mut(&client) else { return actions };
                c.size = size.clamped();
                let size = c.size;
                if self.active == Some(client) && self.exited.is_none() {
                    self.resize(size, &mut actions);
                }
            }
            Event::Ack { client, bytes } => {
                let window = self.window;
                let Some(c) = self.clients.get_mut(&client) else { return actions };
                c.credit = c.credit.saturating_add(bytes).min(window);
                if c.lagging && c.credit == window {
                    // 积压的都处理完了：从当前屏幕重新开始。
                    c.lagging = false;
                    let restore = ClientMsg::Restore(self.restore());
                    self.send(client, restore, &mut actions);
                    if let Some(code) = self.exited {
                        self.send(client, ClientMsg::Exit(code), &mut actions);
                    }
                }
            }
            Event::Detach { client } => {
                self.clients.remove(&client);
                if self.active == Some(client) {
                    self.active = None;
                }
            }
            Event::PtyExited { code } => {
                if self.exited.is_some() {
                    return actions;
                }
                // 剩下的半截序列也画出来。
                let rest = std::mem::take(&mut self.pending);
                if !rest.is_empty() {
                    self.broadcast(rest, &mut actions);
                }
                self.exited = Some(code);
                let ids: Vec<ClientId> = self.clients.keys().copied().collect();
                for id in ids {
                    // 落后的客户端追上时会先拿到最终屏幕，再拿到退出码。
                    if !self.clients[&id].lagging {
                        self.send(id, ClientMsg::Exit(code), &mut actions);
                    }
                }
            }
        }
        actions
    }

    fn output(&mut self, bytes: Vec<u8>, actions: &mut Vec<Action>) {
        self.pending.extend(bytes);
        let cut = safe_prefix(&self.pending);
        let cut = if cut == 0 && self.pending.len() > MAX_PENDING { self.pending.len() } else { cut };
        if cut == 0 {
            return;
        }
        let rest = self.pending.split_off(cut);
        let ready = std::mem::replace(&mut self.pending, rest);
        self.broadcast(ready, actions);
    }

    fn broadcast(&mut self, bytes: Vec<u8>, actions: &mut Vec<Action>) {
        self.parser.process(&bytes);
        let ids: Vec<ClientId> = self.clients.keys().copied().collect();
        for id in ids {
            self.send(id, ClientMsg::Output(bytes.clone()), actions);
        }
    }

    /// 在额度允许时发送；不允许就把客户端标记为落后，之后的实时输出都不再发给它。
    /// 没有任何未确认数据时总可以发一条，否则大于窗口的消息永远发不出去。
    fn send(&mut self, client: ClientId, msg: ClientMsg, actions: &mut Vec<Action>) {
        let window = self.window;
        let Some(c) = self.clients.get_mut(&client) else { return };
        if c.lagging {
            return;
        }
        let cost = msg.cost();
        if c.credit >= cost || c.credit == window {
            c.credit = c.credit.saturating_sub(cost);
            actions.push(Action::Send { client, msg });
        } else {
            c.lagging = true;
        }
    }

    fn activate(&mut self, client: ClientId, actions: &mut Vec<Action>) {
        if self.active == Some(client) {
            return;
        }
        self.active = Some(client);
        let size = self.clients[&client].size;
        self.resize(size, actions);
    }

    fn resize(&mut self, size: Size, actions: &mut Vec<Action>) {
        if size == self.size {
            return;
        }
        self.size = size;
        self.parser.screen_mut().set_size(nz(size.rows), nz(size.cols));
        actions.push(Action::ResizePty(size));
    }
}

/// 输出里最长的、不在转义序列或多字节字符中间结束的前缀长度。
pub fn safe_prefix(bytes: &[u8]) -> usize {
    #[derive(Clone, Copy, PartialEq)]
    enum St {
        Ground,
        Esc,
        EscInter,
        Csi,
        Str,
        StrEsc,
        Utf8(u8),
    }
    let mut st = St::Ground;
    let mut safe = 0;
    for (i, &b) in bytes.iter().enumerate() {
        st = loop {
            let next = match st {
                St::Ground => match b {
                    0x1b => St::Esc,
                    0xc0..=0xdf => St::Utf8(1),
                    0xe0..=0xef => St::Utf8(2),
                    0xf0..=0xf7 => St::Utf8(3),
                    _ => St::Ground,
                },
                St::Utf8(n) => match b {
                    0x80..=0xbf if n > 1 => St::Utf8(n - 1),
                    0x80..=0xbf => St::Ground,
                    // 残缺的字符：从这个字节重新开始判断。
                    _ => {
                        st = St::Ground;
                        continue;
                    }
                },
                St::Esc => match b {
                    b'[' => St::Csi,
                    b']' | b'P' | b'X' | b'^' | b'_' => St::Str,
                    0x20..=0x2f => St::EscInter,
                    0x1b => St::Esc,
                    _ => St::Ground,
                },
                St::EscInter => match b {
                    0x20..=0x2f => St::EscInter,
                    _ => St::Ground,
                },
                St::Csi => match b {
                    0x40..=0x7e | 0x18 | 0x1a => St::Ground,
                    0x1b => St::Esc,
                    _ => St::Csi,
                },
                St::Str => match b {
                    0x07 | 0x18 | 0x1a => St::Ground,
                    0x1b => St::StrEsc,
                    _ => St::Str,
                },
                St::StrEsc => match b {
                    b'\\' => St::Ground,
                    _ => {
                        st = St::Esc;
                        continue;
                    }
                },
            };
            break next;
        };
        if st == St::Ground {
            safe = i + 1;
        }
    }
    safe
}

fn nz(n: u16) -> core::num::NonZeroU16 {
    core::num::NonZeroU16::new(n.max(1)).expect("至少为 1")
}

#[cfg(test)]
mod tests {
    use super::safe_prefix;

    #[test]
    fn safe_prefix_stops_before_unfinished_sequences() {
        assert_eq!(safe_prefix(b"abc"), 3);
        assert_eq!(safe_prefix(b"ab\x1b"), 2);
        assert_eq!(safe_prefix(b"ab\x1b[3"), 2);
        assert_eq!(safe_prefix(b"ab\x1b[31m"), 7);
        assert_eq!(safe_prefix(b"\x1b]0;title"), 0);
        assert_eq!(safe_prefix(b"\x1b]0;t\x07x"), 7);
        assert_eq!(safe_prefix(b"\x1b]0;t\x1b\\"), 7);
        assert_eq!(safe_prefix("中".as_bytes()), 3);
        assert_eq!(safe_prefix(&"中".as_bytes()[..2]), 0);
        assert_eq!(safe_prefix(b"\x1b(B"), 3);
    }
}
