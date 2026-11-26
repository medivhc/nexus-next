//! 常驻程序和终端持有进程之间的协议（M7.2，FP-7.2-04）：升级后新的常驻程序要接管旧的持有进程，
//! 所以已有消息的编码永远不变。加新种类只能加在末尾，并在这里加一行。

use nexus_sys_real::holder::{encode_frame, Down, Up, HOLDER_VERSION};
use nexus_sys_real::Size;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn holder_messages_keep_their_encoding() {
    assert_eq!(HOLDER_VERSION, 1, "改了持有进程协议的主版本：旧的持有进程不会被接管，要在发布说明里写明");
    let size = Size { cols: 80, rows: 24 };
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("up.hello", encode_frame(&Up::Hello { version: 1, size, created_at_ms: 1_000 }), "0000000600015018e807"),
        ("up.output", encode_frame(&Up::Output(b"hi".to_vec())), "0000000401026869"),
        ("up.exited", encode_frame(&Up::Exited { code: Some(3), signal: None }), "0000000402010600"),
        ("down.write", encode_frame(&Down::Write(b"ls\r".to_vec())), "0000000500036c730d"),
        ("down.resize", encode_frame(&Down::Resize(Size { cols: 120, rows: 40 })), "00000003017828"),
        ("down.kill", encode_frame(&Down::Kill), "0000000102"),
    ];
    let mut wrong = Vec::new();
    for (name, bytes, want) in &cases {
        if hex(bytes) != *want {
            wrong.push(format!("{name}: 现在 {}，应当 {want}", hex(bytes)));
        }
    }
    assert!(wrong.is_empty(), "持有进程协议的编码变了（不兼容）：\n{}", wrong.join("\n"));
}
