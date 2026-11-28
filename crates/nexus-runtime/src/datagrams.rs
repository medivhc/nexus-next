//! UDP 转发（M11.5，R91）：数据报在节点间的流上按帧传，每帧是 2 字节大端长度 + 数据报。

use std::cell::Cell;
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::DATAGRAM_IDLE;

/// 一个数据报最长 65535 字节（IPv4 UDP 的上限），帧长度放得下。
const MAX_DATAGRAM: usize = 65535;

async fn read_frame<R: AsyncRead + Unpin>(r: &mut R, buf: &mut [u8]) -> io::Result<Option<usize>> {
    let mut len = [0u8; 2];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u16::from_be_bytes(len) as usize;
    r.read_exact(&mut buf[..n]).await?;
    Ok(Some(n))
}

async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, data: &[u8]) -> io::Result<()> {
    let n = data.len().min(MAX_DATAGRAM);
    let mut frame = Vec::with_capacity(n + 2);
    frame.extend_from_slice(&(n as u16).to_be_bytes());
    frame.extend_from_slice(&data[..n]);
    w.write_all(&frame).await?;
    w.flush().await
}

/// 空闲超时：`touch` 之后 [`DATAGRAM_IDLE`] 内没有再 `touch` 就返回。
async fn idle(last: &Cell<tokio::time::Instant>) {
    loop {
        let deadline = last.get() + DATAGRAM_IDLE;
        if tokio::time::Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep_until(deadline).await;
    }
}

/// 流（帧）和一个数据报端点互相转发：流上来的每帧交给 `send`，`recv` 收到的每个数据报写成一帧。
/// `recv` 的 `None` 表示端点结束，`Some(0)` 是空数据报。流结束、出错或两边都空闲时结束。
pub(crate) async fn relay<St, S, R>(stream: St, mut send: S, mut recv: R) -> io::Result<()>
where
    St: AsyncRead + AsyncWrite + Unpin,
    S: AsyncFnMut(&[u8]) -> io::Result<usize>,
    R: AsyncFnMut(&mut [u8]) -> io::Result<Option<usize>>,
{
    let (mut rd, mut wr) = tokio::io::split(stream);
    let last = Cell::new(tokio::time::Instant::now());
    let up = async {
        let mut buf = vec![0u8; MAX_DATAGRAM];
        while let Some(n) = read_frame(&mut rd, &mut buf).await? {
            last.set(tokio::time::Instant::now());
            send(&buf[..n]).await?;
        }
        Ok::<_, io::Error>(())
    };
    let down = async {
        let mut buf = vec![0u8; MAX_DATAGRAM];
        while let Some(n) = recv(&mut buf).await? {
            last.set(tokio::time::Instant::now());
            write_frame(&mut wr, &buf[..n]).await?;
        }
        Ok::<_, io::Error>(())
    };
    let r = tokio::select! {
        r = up => r,
        r = down => r,
        _ = idle(&last) => Ok(()),
    };
    let mut stream = rd.unsplit(wr);
    let _ = stream.shutdown().await;
    r
}

/// 两个数据报端点互相转发（完整节点上的程序发给本机自己的 UDP 服务）。读到 0 字节当作结束。
pub(crate) async fn bridge<AR, AS, BS, BR>(
    mut a_recv: AR,
    mut a_send: AS,
    mut b_send: BS,
    mut b_recv: BR,
) -> io::Result<()>
where
    AR: AsyncFnMut(&mut [u8]) -> io::Result<usize>,
    AS: AsyncFnMut(&[u8]) -> io::Result<usize>,
    BS: AsyncFnMut(&[u8]) -> io::Result<usize>,
    BR: AsyncFnMut(&mut [u8]) -> io::Result<usize>,
{
    let last = Cell::new(tokio::time::Instant::now());
    let ab = async {
        let mut buf = vec![0u8; MAX_DATAGRAM];
        loop {
            let n = a_recv(&mut buf).await?;
            if n == 0 {
                return Ok::<_, io::Error>(());
            }
            last.set(tokio::time::Instant::now());
            b_send(&buf[..n]).await?;
        }
    };
    let ba = async {
        let mut buf = vec![0u8; MAX_DATAGRAM];
        loop {
            let n = b_recv(&mut buf).await?;
            last.set(tokio::time::Instant::now());
            a_send(&buf[..n]).await?;
        }
        #[allow(unreachable_code)]
        Ok::<_, io::Error>(())
    };
    tokio::select! {
        r = ab => r,
        r = ba => r,
        _ = idle(&last) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip_and_end_with_the_stream() {
        let (mut a, b) = tokio::io::duplex(1 << 20);
        let sent = std::cell::RefCell::new(Vec::<Vec<u8>>::new());
        let replies = std::cell::RefCell::new(std::collections::VecDeque::from([b"pong".to_vec()]));
        let task = relay(
            b,
            async |d: &[u8]| {
                sent.borrow_mut().push(d.to_vec());
                Ok(d.len())
            },
            async |buf: &mut [u8]| {
                let next = replies.borrow_mut().pop_front();
                match next {
                    Some(r) => {
                        buf[..r.len()].copy_from_slice(&r);
                        Ok(Some(r.len()))
                    }
                    None => std::future::pending().await,
                }
            },
        );
        let client = async {
            write_frame(&mut a, b"ping").await.unwrap();
            write_frame(&mut a, &[7u8; 1400]).await.unwrap();
            write_frame(&mut a, b"").await.unwrap();
            let mut buf = vec![0u8; MAX_DATAGRAM];
            let n = read_frame(&mut a, &mut buf).await.unwrap().unwrap();
            assert_eq!(&buf[..n], b"pong");
            a.shutdown().await.unwrap();
        };
        let (r, ()) = tokio::join!(task, client);
        r.unwrap();
        assert_eq!(
            *sent.borrow(),
            vec![b"ping".to_vec(), vec![7u8; 1400], Vec::new()]
        );
    }
}
