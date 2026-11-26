//! 持有进程的连接在每个平台上都能用（M11.3，FP-11.3-04）：Unix 的域套接字，Windows 的回环 TCP 加口令。
//! 持有进程在本进程的一个线程里运行；常驻程序那一边用 `adopt` 连上、断开、再连上，最后结束会话。
// covers: R66, R84, R90

use nexus_core_proto::Timestamp;
use nexus_sys::{Pty, PtyEvent};
use nexus_sys_real::holder;
use nexus_sys_real::Size;
use std::path::Path;
use std::time::{Duration, Instant};

#[test]
fn holder_accepts_reconnects_and_exits() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    let sock = holder::sessions_dir(&data).join("main.sock");
    let size = Size { cols: 100, rows: 30 };
    let d = data.clone();
    let held = std::thread::spawn(move || holder::run(&d, "main", size, Timestamp(7), false));
    let deadline = Instant::now() + Duration::from_secs(20);
    while !sock.exists() {
        assert!(Instant::now() < deadline, "持有进程没有就绪");
        assert!(!held.is_finished(), "持有进程提前结束：{:?}", held.join());
        std::thread::sleep(Duration::from_millis(20));
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        eprintln!("持有进程就绪，第一次连上");
        // 第一次连上：会话名、尺寸、创建时间都对。
        let first = holder::adopt(Path::new("nexus-not-needed-for-live-sessions"), &data);
        assert_eq!(first.len(), 1, "应当找到一个会话");
        assert_eq!(first[0].name, "main");
        assert_eq!(first[0].size, size);
        assert_eq!(first[0].created_at, Timestamp(7));
        // 断开（常驻程序退出）再连上（新的常驻程序）：会话还在。
        drop(first);
        tokio::time::sleep(Duration::from_millis(200)).await;
        eprintln!("断开，再连上");
        let mut again = holder::adopt(Path::new("nexus-not-needed-for-live-sessions"), &data);
        assert_eq!(again.len(), 1, "断开之后应当还能连上");
        let mut pty = again.remove(0).pty;
        eprintln!("结束会话");
        pty.kill();
        let exited = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let PtyEvent::Exited(_) = pty.next_event().await {
                    return;
                }
            }
        })
        .await;
        assert!(exited.is_ok(), "结束会话之后应当收到退出");
    });
    // 持有进程在 shell 结束后要自己结束（限时：卡住时测试失败，而不是一直挂着）。
    let deadline = Instant::now() + Duration::from_secs(20);
    while !held.is_finished() {
        assert!(Instant::now() < deadline, "shell 结束 20 秒后持有进程还没有结束");
        std::thread::sleep(Duration::from_millis(50));
    }
    held.join().unwrap().unwrap();
    assert!(!sock.exists(), "持有进程退出时应当删掉端点");
    // 会话是被结束的（M11.5，R90）：为恢复记下的状态也要删掉，下次不会被当成要恢复的会话。
    assert!(!holder::sessions_dir(&data).join("main.state").exists(), "会话结束后应当删掉记下的状态");
}
