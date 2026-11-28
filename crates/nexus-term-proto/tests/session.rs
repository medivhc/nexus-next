//! 会话状态机的性质测试（R05 恢复、R06 全屏程序、R07 多客户端、R08 慢客户端）。
//!
//! 每个客户端在测试里都有一个独立的内存终端：只喂它收到的消息，最后和会话的屏幕逐格比较。

use nexus_term_proto::{Action, ClientId, ClientMsg, Event, Session, Size};
use proptest::prelude::*;
use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroU16;

const SIZE: Size = Size { cols: 40, rows: 12 };

fn parser(size: Size) -> vt100::Parser {
    vt100::Parser::new(NonZeroU16::new(size.rows).unwrap(), NonZeroU16::new(size.cols).unwrap(), 0)
}

/// 屏幕上用户能看到的一切：每格的字符和样式、光标、模式。
#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    cells: Vec<String>,
    cursor: (u16, u16),
    alternate: bool,
    hide_cursor: bool,
    application_cursor: bool,
    bracketed_paste: bool,
}

fn snapshot(screen: &vt100::Screen) -> Snapshot {
    let (rows, cols) = screen.size();
    let mut cells = Vec::new();
    for r in 0..rows.get() {
        let mut line = String::new();
        for c in 0..cols.get() {
            let cell = screen.cell(r, c).unwrap();
            line.push_str(&format!(
                "[{}|{:?}|{:?}|{}{}{}]",
                cell.contents(),
                cell.fgcolor(),
                cell.bgcolor(),
                cell.bold() as u8,
                cell.inverse() as u8,
                cell.underline() as u8
            ));
        }
        cells.push(line);
    }
    Snapshot {
        cells,
        cursor: screen.cursor_position(),
        alternate: screen.alternate_screen(),
        hide_cursor: screen.hide_cursor(),
        application_cursor: screen.application_cursor(),
        bracketed_paste: screen.bracketed_paste(),
    }
}

/// 只报告不同的地方，失败信息才看得过来。
fn diff(a: &Snapshot, b: &Snapshot) -> String {
    let mut out = String::new();
    for (i, (x, y)) in a.cells.iter().zip(&b.cells).enumerate() {
        if x != y {
            out.push_str(&format!("row {i}:\n  {x}\n  {y}\n"));
        }
    }
    if (a.cursor, a.alternate, a.hide_cursor, a.application_cursor, a.bracketed_paste)
        != (b.cursor, b.alternate, b.hide_cursor, b.application_cursor, b.bracketed_paste)
    {
        out.push_str(&format!(
            "state: {:?} vs {:?}\n",
            (a.cursor, a.alternate, a.hide_cursor, a.application_cursor, a.bracketed_paste),
            (b.cursor, b.alternate, b.hide_cursor, b.application_cursor, b.bracketed_paste)
        ));
    }
    out
}

macro_rules! assert_same {
    ($a:expr, $b:expr) => {{
        let (a, b) = (snapshot($a), snapshot($b));
        prop_assert!(a == b, "{} 和 {} 的屏幕不同：\n{}", stringify!($a), stringify!($b), diff(&a, &b));
    }};
}

/// 测试里的一个客户端：收到的消息排队，确认后才喂给它的终端。
struct Peer {
    term: vt100::Parser,
    queue: VecDeque<ClientMsg>,
    in_flight: u64,
    exit: Option<Option<i32>>,
    restores: usize,
}

impl Peer {
    fn new(size: Size) -> Self {
        Self { term: parser(size), queue: VecDeque::new(), in_flight: 0, exit: None, restores: 0 }
    }

    /// 处理队首一条消息，返回要确认的字节数。
    fn consume_one(&mut self) -> Option<u64> {
        let msg = self.queue.pop_front()?;
        let cost = msg.cost();
        match msg {
            ClientMsg::Restore(b) => {
                // 恢复数据从一块空白终端开始画。
                let size = self.term.screen().size();
                self.term = vt100::Parser::new(size.0, size.1, 0);
                self.term.process(&b);
                self.restores += 1;
            }
            ClientMsg::Output(b) => self.term.process(&b),
            ClientMsg::Exit(code) => self.exit = Some(code),
        }
        self.in_flight -= cost;
        Some(cost)
    }
}

struct Harness {
    session: Session,
    peers: BTreeMap<ClientId, Peer>,
    pty_input: Vec<u8>,
    resizes: Vec<Size>,
    window: u64,
}

impl Harness {
    fn new(window: u64) -> Self {
        Self { session: Session::new(SIZE, window), peers: BTreeMap::new(), pty_input: Vec::new(), resizes: Vec::new(), window }
    }

    fn apply(&mut self, event: Event) {
        if let Event::Attach { client, size } = &event {
            self.peers.insert(*client, Peer::new(*size));
        }
        for action in self.session.handle(event) {
            match action {
                Action::Send { client, msg } => {
                    let peer = self.peers.get_mut(&client).expect("只给连着的客户端发消息");
                    peer.in_flight += msg.cost();
                    peer.queue.push_back(msg);
                    // 一条大于窗口的消息只能在没有其他未确认数据时发出。
                    assert!(
                        peer.in_flight <= self.window || peer.queue.iter().filter(|m| m.cost() > 0).count() == 1,
                        "客户端 {client} 未确认的数据 {} 超过窗口 {}",
                        peer.in_flight,
                        self.window
                    );
                }
                Action::WritePty(b) => self.pty_input.extend(b),
                Action::ResizePty(s) => self.resizes.push(s),
            }
        }
    }

    /// 客户端处理并确认它收到的全部消息，直到会话不再给它补发。
    fn settle(&mut self, client: ClientId) {
        loop {
            let mut acked = 0;
            while let Some(n) = self.peers.get_mut(&client).unwrap().consume_one() {
                acked += n;
            }
            if acked == 0 {
                break;
            }
            self.apply(Event::Ack { client, bytes: acked });
        }
    }

    fn peer(&self, client: ClientId) -> &Peer {
        &self.peers[&client]
    }
}

/// 终端输出的语料：普通文字、换行、光标移动、清屏、颜色、宽字符、模式切换、全屏程序。
fn token() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        "[a-zA-Z0-9 ]{1,30}".prop_map(String::into_bytes),
        Just(b"\r\n".to_vec()),
        Just(b"\n".to_vec()),
        Just(b"\r".to_vec()),
        Just(b"\x08".to_vec()),
        Just(b"\t".to_vec()),
        Just("中文宽字符".as_bytes().to_vec()),
        Just("é".as_bytes().to_vec()),
        (1u16..14, 1u16..42).prop_map(|(r, c)| format!("\x1b[{r};{c}H").into_bytes()),
        (1u16..5).prop_map(|n| format!("\x1b[{n}A").into_bytes()),
        (1u16..5).prop_map(|n| format!("\x1b[{n}C").into_bytes()),
        (0u16..3).prop_map(|n| format!("\x1b[{n}J").into_bytes()),
        (0u16..3).prop_map(|n| format!("\x1b[{n}K").into_bytes()),
        (30u16..38).prop_map(|n| format!("\x1b[{n}m").into_bytes()),
        (40u16..48).prop_map(|n| format!("\x1b[1;{n}m").into_bytes()),
        Just(b"\x1b[38;5;208m".to_vec()),
        Just(b"\x1b[7m".to_vec()),
        Just(b"\x1b[m".to_vec()),
        Just(b"\x1b[?25l".to_vec()),
        Just(b"\x1b[?25h".to_vec()),
        Just(b"\x1b[?1h".to_vec()),
        Just(b"\x1b[?1l".to_vec()),
        Just(b"\x1b[?2004h".to_vec()),
        Just(b"\x1b]0;window title\x07".to_vec()),
        Just(ENTER_ALT.to_vec()),
        Just(LEAVE_ALT.to_vec()),
    ]
}

const ENTER_ALT: &[u8] = b"\x1b[?1049h";
/// 全屏程序退出备用屏幕时都会重置样式；恢复数据不还原进入备用屏幕时保存的样式。
const LEAVE_ALT: &[u8] = b"\x1b[?1049l\x1b[m";

/// 像真实程序一样成对地进出备用屏幕。不在备用屏幕时“退出”会跳回保存的光标位置，
/// 而保存的光标位置不在恢复数据里（已知限制）。
fn well_nested(tokens: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut alt = false;
    tokens
        .into_iter()
        .filter(|t| {
            if t == ENTER_ALT {
                !std::mem::replace(&mut alt, true)
            } else if t == LEAVE_ALT {
                std::mem::replace(&mut alt, false)
            } else {
                true
            }
        })
        .collect()
}

/// 一段输出流，再按任意位置切成若干块（切口可能落在转义序列或多字节字符中间）。
fn stream() -> impl Strategy<Value = Vec<Vec<u8>>> {
    (prop::collection::vec(token(), 0..80), prop::collection::vec(1usize..40, 1..60)).prop_map(|(tokens, cuts)| {
        let bytes: Vec<u8> = well_nested(tokens).concat();
        let mut chunks = Vec::new();
        let mut at = 0;
        let mut i = 0;
        while at < bytes.len() {
            let n = cuts[i % cuts.len()].min(bytes.len() - at);
            chunks.push(bytes[at..at + n].to_vec());
            at += n;
            i += 1;
        }
        chunks
    })
}

proptest! {
    /// R05/R06：任意时刻连上来的客户端，先画恢复数据再接着显示输出，
    /// 看到的屏幕和一直在线看到的完全一样。
    #[test]
    fn late_client_sees_the_same_screen(chunks in stream(), attach_at in 0usize..60) {
        let mut h = Harness::new(1 << 20);
        let attach_at = attach_at.min(chunks.len());
        for (i, chunk) in chunks.iter().enumerate() {
            if i == attach_at {
                h.apply(Event::Attach { client: 1, size: SIZE });
                h.settle(1);
            }
            h.apply(Event::PtyOutput(chunk.clone()));
            if h.peers.contains_key(&1) {
                h.settle(1);
            }
        }
        if !h.peers.contains_key(&1) {
            h.apply(Event::Attach { client: 1, size: SIZE });
            h.settle(1);
        }
        // 参照终端一次性吃下全部输出（见 split_characters_lose_nothing）。
        let mut reference = parser(SIZE);
        reference.process(&chunks.concat());
        assert_same!(h.session.screen(), reference.screen());
        assert_same!(h.peer(1).term.screen(), reference.screen());
    }

    /// R08：慢客户端未确认的数据永远不超过窗口，快客户端不受它影响；
    /// 慢客户端最终追上时，屏幕和会话一致。
    #[test]
    fn slow_client_never_blocks_and_catches_up(
        chunks in stream(),
        slow_acks in prop::collection::vec(any::<bool>(), 1..20),
    ) {
        let window = 256;
        let mut h = Harness::new(window);
        h.apply(Event::Attach { client: 1, size: SIZE });
        h.apply(Event::Attach { client: 2, size: SIZE });
        h.settle(1);
        h.settle(2);
        for (i, chunk) in chunks.iter().enumerate() {
            h.apply(Event::PtyOutput(chunk.clone()));
            // 快客户端每块都马上处理确认。
            h.settle(2);
            // 慢客户端偶尔只处理一条。
            if slow_acks[i % slow_acks.len()] {
                if let Some(n) = h.peers.get_mut(&1).unwrap().consume_one() {
                    h.apply(Event::Ack { client: 1, bytes: n });
                }
            }
        }
        h.settle(1);
        assert_same!(h.peer(2).term.screen(), h.session.screen());
        assert_same!(h.peer(1).term.screen(), h.session.screen());
    }

    /// R07：所有客户端的输入按到达顺序原样写进终端程序。
    #[test]
    fn inputs_reach_the_program_in_order(inputs in prop::collection::vec((1u64..4, prop::collection::vec(any::<u8>(), 0..20)), 0..30)) {
        let mut h = Harness::new(1 << 20);
        for c in 1..4 {
            h.apply(Event::Attach { client: c, size: SIZE });
        }
        let mut expected = Vec::new();
        for (client, bytes) in inputs {
            expected.extend(&bytes);
            h.apply(Event::Input { client, bytes });
        }
        prop_assert_eq!(h.pty_input, expected);
    }
}

#[test]
fn attach_gets_a_restore_of_the_current_screen() {
    let mut h = Harness::new(1 << 20);
    h.apply(Event::PtyOutput(b"hello\r\nworld".to_vec()));
    h.apply(Event::Attach { client: 7, size: SIZE });
    assert!(matches!(h.peer(7).queue.front(), Some(ClientMsg::Restore(_))));
    h.settle(7);
    assert!(h.peer(7).term.screen().contents().starts_with("hello\nworld"));
}

#[test]
fn restore_of_a_fullscreen_program_keeps_the_shell_underneath() {
    let mut h = Harness::new(1 << 20);
    h.apply(Event::PtyOutput(b"$ vim\r\n".to_vec()));
    h.apply(Event::PtyOutput(b"\x1b[?1049h\x1b[H\x1b[2JFULLSCREEN".to_vec()));
    h.apply(Event::Attach { client: 1, size: SIZE });
    h.settle(1);
    assert!(h.peer(1).term.screen().alternate_screen());
    assert!(h.peer(1).term.screen().contents().contains("FULLSCREEN"));
    // 退出全屏程序后，下面的 shell 内容还在。
    h.apply(Event::PtyOutput(b"\x1b[?1049l".to_vec()));
    h.settle(1);
    assert!(h.peer(1).term.screen().contents().starts_with("$ vim"));
    assert_eq!(snapshot(h.peer(1).term.screen()), snapshot(h.session.screen()));
}

#[test]
fn size_follows_the_client_that_typed_last() {
    let mut h = Harness::new(1 << 20);
    let small = Size { cols: 30, rows: 10 };
    let big = Size { cols: 100, rows: 30 };
    h.apply(Event::Attach { client: 1, size: small });
    assert_eq!(h.resizes, vec![small]);
    h.apply(Event::Attach { client: 2, size: big });
    assert_eq!(h.resizes, vec![small, big]);
    h.apply(Event::Input { client: 1, bytes: b"x".to_vec() });
    assert_eq!(h.resizes, vec![small, big, small]);
    // 同一个客户端继续打字，尺寸不再变化。
    h.apply(Event::Input { client: 1, bytes: b"y".to_vec() });
    assert_eq!(h.resizes.len(), 3);
    // 不活跃的客户端改变窗口大小，不影响会话。
    h.apply(Event::Resize { client: 2, size: Size { cols: 120, rows: 40 } });
    assert_eq!(h.resizes.len(), 3);
    // 活跃的客户端改变窗口大小，会话跟着变。
    let mid = Size { cols: 60, rows: 20 };
    h.apply(Event::Resize { client: 1, size: mid });
    assert_eq!(h.resizes.last(), Some(&mid));
    assert_eq!(h.session.size(), mid);
}

#[test]
fn lagging_client_is_resynced_with_a_restore() {
    let mut h = Harness::new(64);
    h.apply(Event::Attach { client: 1, size: SIZE });
    h.settle(1);
    for i in 0..20 {
        h.apply(Event::PtyOutput(format!("line {i}\r\n").into_bytes()));
    }
    // 没确认过的客户端只收到了窗口内的输出。
    assert!(h.peer(1).in_flight <= 64);
    h.settle(1);
    assert_eq!(h.peer(1).restores, 2, "第一次连上一次，追上时又一次");
    assert!(h.peer(1).term.screen().contents().contains("line 19"));
}

#[test]
fn program_exit_reaches_every_client_even_a_lagging_one() {
    let mut h = Harness::new(64);
    h.apply(Event::Attach { client: 1, size: SIZE });
    h.apply(Event::Attach { client: 2, size: SIZE });
    h.settle(2);
    for i in 0..20 {
        h.apply(Event::PtyOutput(format!("line {i}\r\n").into_bytes()));
        h.settle(2);
    }
    h.apply(Event::PtyExited { code: Some(3) });
    h.settle(1);
    h.settle(2);
    assert_eq!(h.peer(1).exit, Some(Some(3)));
    assert_eq!(h.peer(2).exit, Some(Some(3)));
    // 落后的客户端退出前也拿到了最终屏幕。
    assert!(h.peer(1).term.screen().contents().contains("line 19"));
    assert!(h.session.has_exited());
    // 结束之后再连上来，立刻得到最终屏幕和退出码；输入被忽略。
    h.apply(Event::Attach { client: 3, size: SIZE });
    h.settle(3);
    assert_eq!(h.peer(3).exit, Some(Some(3)));
    h.apply(Event::Input { client: 3, bytes: b"ignored".to_vec() });
    assert!(h.pty_input.is_empty());
}

#[test]
fn detached_client_gets_nothing_more() {
    let mut h = Harness::new(1 << 20);
    h.apply(Event::Attach { client: 1, size: SIZE });
    h.apply(Event::Detach { client: 1 });
    assert_eq!(h.session.client_count(), 0);
    let actions = h.session.handle(Event::PtyOutput(b"x".to_vec()));
    assert!(actions.iter().all(|a| !matches!(a, Action::Send { .. })));
    // 未知客户端的确认和尺寸变化被忽略。
    assert!(h.session.handle(Event::Ack { client: 9, bytes: 10 }).is_empty());
    assert!(h.session.handle(Event::Resize { client: 9, size: SIZE }).is_empty());
}

/// 终端模拟库在一次读入以“上次的半个字符的后半截”开头、又以新的半个字符结尾时，
/// 会丢掉中间的控制字符（vte 0.15 的缺陷）。会话只在完整的字符和转义序列之间切分，
/// 自己的屏幕和发给客户端的输出都不受影响。
#[test]
fn split_characters_lose_nothing() {
    let whole = "é\n中".as_bytes();
    let mut naive = parser(SIZE);
    naive.process(&whole[..1]);
    naive.process(&whole[1..4]);
    naive.process(&whole[4..]);
    let mut reference = parser(SIZE);
    reference.process(whole);
    assert_ne!(naive.screen().cursor_position(), reference.screen().cursor_position(), "缺陷已修复时可以删掉这一行");

    let mut h = Harness::new(1 << 20);
    h.apply(Event::Attach { client: 1, size: SIZE });
    for part in [&whole[..1], &whole[1..4], &whole[4..]] {
        h.apply(Event::PtyOutput(part.to_vec()));
        h.settle(1);
    }
    assert_eq!(snapshot(h.session.screen()), snapshot(reference.screen()));
    assert_eq!(snapshot(h.peer(1).term.screen()), snapshot(reference.screen()));
}
